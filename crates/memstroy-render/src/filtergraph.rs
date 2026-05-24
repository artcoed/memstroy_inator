use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use memstroy_core::*;
use memstroy_vision::pose::load_anchor_track;

use crate::plan::{FfmpegInput, InputKind};

/// Translates a [`Scene`] into an FFmpeg `filter_complex` graph.
///
/// Strategy:
///
/// 1. A blank canvas of the output resolution is the base layer.
///    Implemented with the `color=` source and `format=yuva420p`.
/// 2. Each [`Background`] is added as an `-i` input, scaled/cropped
///    with `Fit` semantics and `enable='between(t, start, end)'`
///    overlays. Backgrounds are stacked sequentially.
/// 3. Each [`Actor`] is added as an `-i` input, optionally piped
///    through `chromakey` and `despill`, then animated via
///    `overlay=x=expr:y=expr` with piecewise-linear position and
///    `scale=` driven by the layout keyframes.
/// 4. Each [`Overlay::Text`] becomes a `drawtext` filter with a
///    `box=1:boxcolor=...` for the meme-style white plate.
/// 5. Each [`Overlay::Image`] / [`Overlay::Video`] is added as an
///    input and overlaid like an actor (chromakey is optional).
///
/// Some advanced features (camera moves, complex easings, pose-driven
/// attachments) are reserved for the next iteration and currently
/// emit warnings instead of altering the graph.
pub struct FilterGraphBuilder<'a> {
    scene: &'a Scene,
    assets_root: PathBuf,
    inputs: Vec<FfmpegInput>,
    /// Filter graph chunks joined with ";\n".
    chunks: Vec<String>,
    /// Label of the current top-of-stack composite stream.
    cursor: String,
    label_counter: u32,
    map_audio: Option<String>,
    /// Side-output: temp PNG files this builder generated for
    /// `EffectKind::Mask` exports. The render runner deletes these
    /// after FFmpeg finishes so we don't leak under `std::env::temp_dir()`.
    mask_assets: Vec<PathBuf>,
}

impl<'a> FilterGraphBuilder<'a> {
    pub fn new(scene: &'a Scene, assets_root: &Path) -> Self {
        Self {
            scene,
            assets_root: assets_root.to_path_buf(),
            inputs: Vec::new(),
            chunks: Vec::new(),
            cursor: "[base]".into(),
            label_counter: 0,
            map_audio: None,
            mask_assets: Vec::new(),
        }
    }

    pub fn finish(self) -> (String, Vec<FfmpegInput>, String, Option<String>, Vec<PathBuf>) {
        let map_video = self.cursor.clone();
        // Join with a bare `;` (no trailing `\n`) so the
        // filter_complex string we hand to `Command::args` is a
        // single physical line. Newlines inside the argument are
        // legal but they confuse some Windows console tooling and
        // make the diagnostic log less greppable; the renderer's
        // tail-line buffer in `runner.rs` only keeps the last few
        // lines of stderr, so a graph that spans 200 newlines used
        // to push the actual error message off the visible window.
        (self.chunks.join(";"), self.inputs, map_video, self.map_audio, self.mask_assets)
    }

    pub fn build(&mut self) -> Result<()> {
        self.emit_base_canvas();
        self.emit_backgrounds()?;
        // Single z-ordered pass — actors and overlays are interleaved
        // by their per-element `z_order` so the export matches the
        // preview canvas's timeline-track stacking. The GUI populates
        // `z_order` on each actor / overlay right before invoking the
        // renderer (see `populate_render_z_order` in
        // `memstroy-gui/src/jobs.rs`); legacy scenes / scripts that
        // never set it fall back to the historical
        // text-behind-actors → actors → everything-else-on-top order.
        self.emit_z_ordered_elements()?;
        self.emit_camera()?;
        self.emit_audio()?;
        // Output normalisation pass — must be the LAST video filter so
        // every code path above feeds the encoder a stream with a
        // known-good shape (yuv420p, square pixels, even dims, CFR at
        // the requested fps). See `finalize_video` for the rationale.
        self.finalize_video();
        Ok(())
    }

    /// Emit every actor and overlay in a single pass, sorted by
    /// `z_order`. Lower z = drawn first (visually behind), higher z
    /// = drawn later (visually on top). Within ties, the previous
    /// emit ordering is preserved (text overlays sorted by `z_index`,
    /// scene index for tiebreak).
    ///
    /// Falls back to the legacy 3-phase ordering (text-behind-actors
    /// → actors → image/video/text-on-top) when ALL elements have
    /// `z_order == 0`, which is the case for scenes saved before the
    /// `z_order` field was introduced or scripts that don't populate
    /// it.
    fn emit_z_ordered_elements(&mut self) -> Result<()> {
        let [w, h] = self.scene.output.resolution;

        // Detect whether anything overrides the legacy ordering.
        let any_explicit = self.scene.actors.iter().any(|a| a.z_order != 0)
            || self.scene.overlays.iter().any(|ov| match ov {
                Overlay::Text(t) => t.z_order != 0,
                Overlay::Image(i) => i.z_order != 0,
                Overlay::Video(v) => v.z_order != 0,
            });

        // (sort_key_primary, secondary, tertiary, kind)
        // - kind 0 = actor, 1 = overlay
        let mut items: Vec<(i32, i32, usize, u8, usize)> = Vec::new();

        for (idx, actor) in self.scene.actors.iter().enumerate() {
            let primary = if any_explicit { actor.z_order } else { 100 };
            // Actors don't have a per-element z_index secondary, so
            // use 0; index acts as the stable tiebreak.
            items.push((primary, 0, idx, 0, idx));
        }
        for (idx, ov) in self.scene.overlays.iter().enumerate() {
            let (primary, secondary) = if any_explicit {
                let z = match ov {
                    Overlay::Text(t) => t.z_order,
                    Overlay::Image(i) => i.z_order,
                    Overlay::Video(v) => v.z_order,
                };
                let z2 = match ov {
                    Overlay::Text(t) => t.z_index,
                    _ => 100,
                };
                (z, z2)
            } else {
                // Legacy fallback: text with `behind_actors=true` goes
                // BELOW actors; everything else (image / video / text
                // with `behind_actors=false`) goes ABOVE actors. Within
                // each band, sort by `z_index` (text only) ascending.
                match ov {
                    Overlay::Text(t) if t.behind_actors => (0, t.z_index),
                    Overlay::Text(t) => (200, t.z_index),
                    _ => (200, 100),
                }
            };
            items.push((primary, secondary, idx, 1, idx));
        }

        // Sort: primary asc (lower drawn first → behind), then secondary
        // (text z_index), then stable scene-order tiebreak. Ties between
        // actors and overlays at the same z_order resolve by scene index
        // — same shape the preview's `frame_snapshot` uses.
        items.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.4.cmp(&b.4))
        });

        for (_p, _s, _i, kind, idx) in items {
            if kind == 0 {
                self.emit_actor_at(idx)?;
            } else {
                match &self.scene.overlays[idx] {
                    Overlay::Text(t) => self.emit_text(t, w, h)?,
                    Overlay::Image(im) => self.emit_image_overlay(im, w, h)?,
                    Overlay::Video(v) => self.emit_video_overlay(v, w, h)?,
                }
            }
        }

        Ok(())
    }

    /// Append a single normalisation chunk to the cursor that locks
    /// the output stream shape to libx264's happy path:
    ///
    /// * `format=yuv420p` — drop the alpha channel BEFORE the encoder
    ///   sees the stream, instead of relying on ffmpeg's auto-inserted
    ///   yuva→yuv converter which historically tripped on the more
    ///   elaborate filter-graph topologies (rotate + per-frame scale +
    ///   alphamerge masks) and surfaced as
    ///   `vost#0:0/libx264 Task finished with error code: -22`.
    /// * `scale=trunc(iw/2)*2:trunc(ih/2)*2` — defence in depth: when
    ///   `output.resolution` comes from a script (`scripting.rs::set
    ///   _output_resolution`) the user can pick odd dimensions, which
    ///   yuv420p's 4:2:0 chroma subsampling rejects with the same
    ///   `-22` error from inside libx264's `x264_picture_alloc`.
    /// * `setsar=1` — force square pixels, matching what every player
    ///   assumes for vertical 1080×1920 output and avoiding a stretch
    ///   mismatch between input clips with non-1 SAR and the canvas.
    /// * `fps={out_fps}` — re-time the composite to a strict CFR with
    ///   the requested frame rate. The command-level `-r` and
    ///   `-fps_mode cfr` flags tell the encoder what to expect, but
    ///   the filter graph can still emit frames with non-monotonic
    ///   timestamps when overlay chains drop / repeat depending on
    ///   `enable=` clauses; resampling here gives the encoder a clean
    ///   monotonic stream regardless of upstream noise.
    /// * `setpts=PTS-STARTPTS` — start at PTS=0 even when the input
    ///   sources had non-zero baseline timestamps (background videos
    ///   with `-ss` seek, etc.).
    fn finalize_video(&mut self) {
        let final_label = self.alloc_label("vout");
        let fps = self.scene.output.fps;
        self.chunks.push(format!(
            "{cur}format=yuv420p,scale=trunc(iw/2)*2:trunc(ih/2)*2:flags=lanczos,setsar=1,fps={fps},setpts=PTS-STARTPTS{out}",
            cur = self.cursor,
            fps = fps,
            out = final_label,
        ));
        self.cursor = final_label;
    }

    fn alloc_label(&mut self, hint: &str) -> String {
        self.label_counter += 1;
        format!("[{}_{}]", hint, self.label_counter)
    }

    fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() { p.to_path_buf() } else { self.assets_root.join(p) }
    }

    fn add_input(&mut self, inp: FfmpegInput) -> usize {
        self.inputs.push(inp);
        self.inputs.len() - 1
    }

    fn emit_base_canvas(&mut self) {
        let [w, h] = self.scene.output.resolution;
        // When the scene has no backgrounds the user gets a full-frame
        // chromakey green base layer (so the export can be keyed in
        // post). When at least one background is present we honour
        // the configured `output.background_color` because a partial
        // background still needs a fallback colour for the gaps.
        let [r, g, b] = if self.scene.backgrounds.is_empty() {
            [0u8, 255u8, 0u8]
        } else {
            self.scene.output.background_color
        };
        let bg_hex = format!("0x{:02X}{:02X}{:02X}", r, g, b);
        // The base canvas is generated entirely from a filter source so
        // no `-i` slot is consumed.
        self.chunks.push(format!(
            "color=c={hex}:s={w}x{h}:r={fps}:d={dur}[base]",
            hex = bg_hex,
            w = w,
            h = h,
            fps = self.scene.output.fps,
            dur = self.scene.output.duration,
        ));
        self.cursor = "[base]".into();
    }

    fn emit_backgrounds(&mut self) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        for bg in &self.scene.backgrounds {
            let path = self.resolve(match &bg.source {
                MediaSource::Image { path } => path,
                MediaSource::Video { path, .. } => path,
                MediaSource::SolidColor { color } => {
                    // For solid colours emit a color filter and overlay
                    // it directly; no input slot needed.
                    let bg_hex = format!("0x{:02X}{:02X}{:02X}", color[0], color[1], color[2]);
                    let solid = self.alloc_label("solid");
                    self.chunks.push(format!(
                        "color=c={hex}:s={w}x{h}:r={fps}:d={dur}{solid}",
                        hex = bg_hex,
                        w = w,
                        h = h,
                        fps = self.scene.output.fps,
                        dur = bg.duration,
                    ));
                    let next = self.alloc_label("bgstack");
                    self.chunks.push(format!(
                        "{cur}{solid}overlay=enable='between(t,{a},{b})'{next}",
                        cur = self.cursor,
                        solid = solid,
                        a = bg.start,
                        b = bg.start + bg.duration,
                        next = next,
                    ));
                    self.cursor = next;
                    continue;
                }
            });

            let kind = match &bg.source {
                MediaSource::Image { .. } => InputKind::Image,
                MediaSource::Video { .. } => InputKind::Video,
                MediaSource::SolidColor { .. } => unreachable!(),
            };
            let r#loop = matches!(&bg.source, MediaSource::Video { r#loop: true, .. });
            let seek = match &bg.source {
                MediaSource::Video { start_at, .. } => Some(*start_at),
                _ => None,
            };
            let idx = self.add_input(FfmpegInput {
                path,
                kind,
                r#loop,
                seek,
                t: None,
            });

            // Fit the background to the output canvas.
            let scaled = self.alloc_label("bg");
            let fit = match bg.fit {
                Fit::Cover => format!(
                    "scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}",
                    w = w, h = h
                ),
                Fit::Contain => format!(
                    "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
                    w = w, h = h
                ),
                Fit::Stretch => format!("scale={w}:{h}", w = w, h = h),
                Fit::Original => format!(
                    "pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black",
                    w = w, h = h
                ),
            };
            self.chunks.push(format!("[{idx}:v]{fit},setsar=1,format=yuva420p{out}", idx = idx, fit = fit, out = scaled));

            // Transitions: Cut, Snap, Fade, Slide*
            let composed = self.alloc_label("bgstack");
            let alpha_expr = match bg.transition {
                Transition::Fade => format!(
                    "fade=t=in:st={a}:d=0.25:alpha=1,fade=t=out:st={fade_out}:d=0.25:alpha=1",
                    a = bg.start,
                    fade_out = (bg.start + bg.duration - 0.25).max(bg.start),
                ),
                Transition::Snap => {
                    // Snap: 2-frame white flash at the start of this segment.
                    // We fade in extremely fast (1 frame ≈ 0.017s @ 60fps).
                    format!(
                        "fade=t=in:st={a}:d=0.033:alpha=1",
                        a = bg.start
                    )
                }
                _ => String::new(),
            };

            // Slide transitions: offset the overlay position over time
            let slide_overlay = match bg.transition {
                Transition::SlideLeft => {
                    let dur = 0.3;
                    format!(
                        "overlay=x='if(lt(t,{end}),W*(1-((t-{start})/{dur})),0)':y=0:enable='between(t,{start},{seg_end})':eof_action=pass",
                        start = bg.start, end = bg.start + dur, dur = dur, seg_end = bg.start + bg.duration
                    )
                }
                Transition::SlideRight => {
                    let dur = 0.3;
                    format!(
                        "overlay=x='if(lt(t,{end}),-W*(1-((t-{start})/{dur})),0)':y=0:enable='between(t,{start},{seg_end})':eof_action=pass",
                        start = bg.start, end = bg.start + dur, dur = dur, seg_end = bg.start + bg.duration
                    )
                }
                Transition::SlideUp => {
                    let dur = 0.3;
                    format!(
                        "overlay=x=0:y='if(lt(t,{end}),H*(1-((t-{start})/{dur})),0)':enable='between(t,{start},{seg_end})':eof_action=pass",
                        start = bg.start, end = bg.start + dur, dur = dur, seg_end = bg.start + bg.duration
                    )
                }
                Transition::SlideDown => {
                    let dur = 0.3;
                    format!(
                        "overlay=x=0:y='if(lt(t,{end}),-H*(1-((t-{start})/{dur})),0)':enable='between(t,{start},{seg_end})':eof_action=pass",
                        start = bg.start, end = bg.start + dur, dur = dur, seg_end = bg.start + bg.duration
                    )
                }
                _ => String::new(),
            };

            let staged = if alpha_expr.is_empty() {
                scaled.clone()
            } else {
                let tagged = self.alloc_label("bgfade");
                self.chunks.push(format!(
                    "{scaled}{filter}{out}",
                    scaled = scaled,
                    filter = alpha_expr,
                    out = tagged
                ));
                tagged
            };

            if !slide_overlay.is_empty() {
                // Slide: use custom overlay expression
                self.chunks.push(format!(
                    "{cur}{staged}{slide}{out}",
                    cur = self.cursor,
                    staged = staged,
                    slide = slide_overlay,
                    out = composed
                ));
            } else {
                self.chunks.push(format!(
                    "{cur}{staged}overlay=enable='between(t,{a},{b})':eof_action=pass{out}",
                    cur = self.cursor,
                    staged = staged,
                    a = bg.start,
                    b = bg.start + bg.duration,
                    out = composed
                ));
            }

            // Snap flash: overlay a white frame for 2 frames at transition start
            if matches!(bg.transition, Transition::Snap) {
                let flash = self.alloc_label("flash");
                let flash_dur = 0.033; // ~2 frames at 60fps
                self.chunks.push(format!(
                    "color=c=0xFFFFFF:s={w}x{h}:r={fps}:d={fd}[{fl}raw]",
                    w = w, h = h, fps = self.scene.output.fps,
                    fd = flash_dur, fl = &flash[1..flash.len()-1]
                ));
                let raw_label = format!("[{}raw]", &flash[1..flash.len()-1]);
                let after_flash = self.alloc_label("postflash");
                self.chunks.push(format!(
                    "{composed}{raw}overlay=enable='between(t,{a},{b})':eof_action=pass{out}",
                    composed = composed,
                    raw = raw_label,
                    a = bg.start,
                    b = bg.start + flash_dur,
                    out = after_flash,
                ));
                self.cursor = after_flash;
            } else {
                self.cursor = composed;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn emit_actors(&mut self) -> Result<()> {
        for idx in 0..self.scene.actors.len() {
            self.emit_actor_at(idx)?;
        }
        Ok(())
    }

    /// Emit a single actor's filter chunks (chromakey → effect stack →
    /// scale/rotate/opacity → overlay onto cursor → attachments).
    /// Lifted out of the original `emit_actors` loop so the new
    /// `emit_z_ordered_elements` pass can interleave actors with
    /// overlays based on the GUI-supplied per-element `z_order` —
    /// matching the preview's timeline-track stacking.
    fn emit_actor_at(&mut self, actor_idx: usize) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        let actor = &self.scene.actors[actor_idx];
        {
            let path = self.resolve(&actor.source);
            let idx = self.add_input(FfmpegInput {
                path: path.clone(),
                kind: InputKind::Video,
                r#loop: actor.loop_source,
                seek: if actor.source_start > 0.0 { Some(actor.source_start) } else { None },
                t: None,
            });

            let key = actor.chroma_key.key_color;
            let key_hex = format!("0x{:02X}{:02X}{:02X}", key[0], key[1], key[2]);
            // Build the full preview-faithful transform: skeleton
            // attachments (when bound) override world position; then
            // canvas_layouts, then legacy normalised; render_frame
            // motion shifts the composite; modifiers / easing / per-
            // frame rotation / opacity / flip are all honoured. See
            // `expr::build_element_transform` for the contract.
            let t_in_clip = actor.t_in.unwrap_or(0.0);
            let t_out_clip = actor.t_out.unwrap_or(self.scene.output.duration);
            let xform = crate::expr::build_element_transform(
                self.scene,
                &actor.id,
                &actor.layout,
                &actor.modifiers,
                t_in_clip,
                actor.skeleton_attachments.first(),
            );
            if xform.opacity_animated {
                tracing::debug!(
                    actor_id = %actor.id,
                    "actor opacity is keyframed; export uses static midpoint sample",
                );
            }
            let speed = actor.speed.max(0.0001);
            let speed_part = if (speed - 1.0).abs() > 1.0e-4 {
                Some(format!("setpts=PTS/{:.6}", speed))
            } else {
                None
            };
            let scale_part = format!(
                "scale=w='max(2,iw*({sx}))':h='max(2,ih*({sy}))':eval=frame",
                sx = xform.sx_expr,
                sy = xform.sy_expr,
            );
            // Combined static flip = `flip_horizontal` XOR midpoint
            // `flip_x_anim < 0`. Same XOR rule the preview's mesh
            // uses (`combined_x = if static_hflip { -anim } else { anim }`).
            let want_hflip = actor.flip_horizontal ^ xform.hflip;
            let want_vflip = xform.vflip;
            // Pre-scale CC sits with the source's native pixel data —
            // matching the preview where `apply_effects_cpu` also runs
            // before the on-canvas resize.
            let cc_filters =
                crate::expr::color_correction_filters(&actor.color_correction, t_in_clip, actor.t_out);
            let actor_label = self.alloc_label("actor");

            if effects_have_mask(&actor.effects) {
                // Mask effects need extra inputs and a multi-stream
                // sub-graph (alphamerge), so we have to break the
                // single-chunk chain. Lay it out in three pieces:
                //   1) chromakey/format/hflip + CC up to a labelled stage,
                //   2) the user effect stack (handles mask boundaries),
                //   3) speed + layout scale + per-frame rotate/alpha.
                let mut prefix = format!(
                    "[{idx}:v]chromakey={hex}:{sim}:{blend},format=yuva420p",
                    idx = idx,
                    hex = key_hex,
                    sim = actor.chroma_key.similarity,
                    blend = actor.chroma_key.blend,
                );
                if want_hflip {
                    prefix.push_str(",hflip");
                }
                if want_vflip {
                    prefix.push_str(",vflip");
                }
                for cc in &cc_filters {
                    prefix.push(',');
                    prefix.push_str(cc);
                }
                let pre_label = self.alloc_label("actorPre");
                self.chunks.push(format!("{prefix}{pre_label}", prefix = prefix, pre_label = pre_label));
                let after_fx = self.apply_effect_stack(pre_label, &actor.effects, t_in_clip, t_out_clip)?;
                let mut tail_filters: Vec<String> = Vec::new();
                if let Some(s) = speed_part { tail_filters.push(s); }
                tail_filters.push(scale_part);
                if let Some(rot) = &xform.rot_expr {
                    // `c=none` keeps the corners transparent so the
                    // rotated frame composites cleanly over the canvas.
                    tail_filters.push(format!(
                        "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                        r = rot,
                    ));
                }
                if xform.opacity_static < 0.999 {
                    tail_filters.push(format!(
                        "colorchannelmixer=aa={:.4}",
                        xform.opacity_static.clamp(0.0, 1.0),
                    ));
                }
                self.chunks.push(format!(
                    "{src}{filters}{out}",
                    src = after_fx,
                    filters = tail_filters.join(","),
                    out = actor_label,
                ));
            } else {
                // Fast path — keep the historical single-chunk shape so
                // the export trace stays compact for scenes without
                // any masks.
                let mut chain = format!(
                    "[{idx}:v]chromakey={hex}:{sim}:{blend},format=yuva420p",
                    idx = idx,
                    hex = key_hex,
                    sim = actor.chroma_key.similarity,
                    blend = actor.chroma_key.blend,
                );
                if want_hflip {
                    chain.push_str(",hflip");
                }
                if want_vflip {
                    chain.push_str(",vflip");
                }
                for cc in &cc_filters {
                    chain.push(',');
                    chain.push_str(cc);
                }
                for snippet in effect_stack_filters(&actor.effects, t_in_clip, t_out_clip) {
                    chain.push(',');
                    chain.push_str(&snippet);
                }
                if let Some(s) = &speed_part {
                    chain.push(',');
                    chain.push_str(s);
                }
                chain.push(',');
                chain.push_str(&scale_part);
                if let Some(rot) = &xform.rot_expr {
                    chain.push(',');
                    chain.push_str(&format!(
                        "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                        r = rot,
                    ));
                }
                if xform.opacity_static < 0.999 {
                    chain.push(',');
                    chain.push_str(&format!(
                        "colorchannelmixer=aa={:.4}",
                        xform.opacity_static.clamp(0.0, 1.0),
                    ));
                }
                self.chunks.push(format!("{chain}{out}", chain = chain, out = actor_label));
            }

            let composed = self.alloc_label("stack");
            let enable = match (actor.t_in, actor.t_out) {
                (Some(a), Some(b)) => format!(":enable='between(t,{},{})'", a, b),
                (Some(a), None) => format!(":enable='gte(t,{})'", a),
                (None, Some(b)) => format!(":enable='lte(t,{})'", b),
                (None, None) => String::new(),
            };
            self.chunks.push(format!(
                "{cur}{actor}overlay=x='{x}':y='{y}'{enable}:eof_action=pass{out}",
                cur = self.cursor,
                actor = actor_label,
                x = xform.x_expr,
                y = xform.y_expr,
                enable = enable,
                out = composed,
            ));
            self.cursor = composed;

            // ─── ATTACHMENTS ─────────────────────────────────────
            if !actor.attachments.is_empty() {
                self.emit_attachments(actor, w, h)?;
            }
        }
        Ok(())
    }

    /// Emit overlay filters for each attachment on an actor.
    /// Props are positioned based on the actor's AnchorTrack (if available)
    /// or fall back to the actor's center position.
    fn emit_attachments(&mut self, actor: &Actor, w: u32, h: u32) -> Result<()> {
        // Try to load anchor track
        let track = actor.anchors.as_ref().and_then(|p| {
            let resolved = self.resolve(p);
            load_anchor_track(&resolved)
                .or_else(|| {
                    // Also try loading from actor source path
                    load_anchor_track(&self.resolve(&actor.source))
                })
        }).or_else(|| {
            // Fallback: try to load from actor source .anchors.json
            load_anchor_track(&self.resolve(&actor.source))
        });

        for attachment in &actor.attachments {
            let prop_path = self.resolve(&attachment.asset);
            let idx = self.add_input(FfmpegInput {
                path: prop_path,
                kind: InputKind::Image,
                r#loop: false,
                seek: None,
                t: None,
            });

            // Compute position expression for this prop
            let (prop_x, prop_y) = if let Some(ref track) = track {
                // Build piecewise expression from anchor samples
                let anchor_name = anchor_point_to_name(attachment.anchor);
                build_anchor_position_expr(
                    track,
                    &anchor_name,
                    attachment.offset,
                    attachment.scale,
                    &actor.layout,
                    w,
                    h,
                )
            } else {
                // Fallback: position relative to actor's center
                let (ax, ay, _, _) = position_and_scale_expr(&actor.layout, w, h);
                (
                    format!("{}+{}", ax, attachment.offset[0]),
                    format!("{}+{}", ay, attachment.offset[1]),
                )
            };

            // Scale expression: actor_scale * attachment.scale
            let (_, _, actor_scale, _) = position_and_scale_expr(&actor.layout, w, h);
            let prop_scale = format!("{}*{}", actor_scale, attachment.scale);

            // Build filter chain for the prop
            let chain = format!(
                "[{idx}:v]format=yuva420p,scale=w='max(2,iw*({s}))':h='max(2,ih*({s}))':eval=frame",
                idx = idx,
                s = prop_scale,
            );
            let prop_label = self.alloc_label("prop");
            self.chunks.push(format!("{chain}{out}", chain = chain, out = prop_label));

            // Overlay prop on top of current composite
            let enable = match (actor.t_in, actor.t_out) {
                (Some(a), Some(b)) => format!(":enable='between(t,{},{})'", a, b),
                (Some(a), None) => format!(":enable='gte(t,{})'", a),
                (None, Some(b)) => format!(":enable='lte(t,{})'", b),
                (None, None) => String::new(),
            };
            let composed = self.alloc_label("propstack");
            self.chunks.push(format!(
                "{cur}{prop}overlay=x='{x}':y='{y}'{enable}:eof_action=pass{out}",
                cur = self.cursor,
                prop = prop_label,
                x = prop_x,
                y = prop_y,
                enable = enable,
                out = composed,
            ));
            self.cursor = composed;
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn emit_overlays(&mut self) -> Result<()> {
        // Legacy single-pass emitter, kept for any callers; emits all overlays
        // in scene order regardless of `behind_actors`.
        let [w, h] = self.scene.output.resolution;
        for ov in &self.scene.overlays {
            match ov {
                Overlay::Text(t) => self.emit_text(t, w, h)?,
                Overlay::Image(i) => self.emit_image_overlay(i, w, h)?,
                Overlay::Video(v) => self.emit_video_overlay(v, w, h)?,
            }
        }
        Ok(())
    }

    /// Emit overlays in z-order. `under_actors=true` selects only text overlays
    /// flagged with `behind_actors`; otherwise emits everything else.
    #[allow(dead_code)]
    fn emit_overlays_filtered(&mut self, under_actors: bool) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        // Collect indices with their effective z and a stable scene-order tie.
        let mut indexed: Vec<(usize, i32)> = self.scene.overlays.iter().enumerate()
            .filter(|(_, ov)| {
                match ov {
                    Overlay::Text(t) => t.behind_actors == under_actors,
                    // Image/video overlays are always above actors.
                    _ => !under_actors,
                }
            })
            .map(|(i, ov)| {
                let z = match ov {
                    Overlay::Text(t) => t.z_index,
                    _ => 100,
                };
                (i, z)
            })
            .collect();
        // Stable sort by z asc; FFmpeg overlays stack later=on-top so this
        // matches the canvas rendering order.
        indexed.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        for (idx, _) in indexed {
            match &self.scene.overlays[idx] {
                Overlay::Text(t) => self.emit_text(t, w, h)?,
                Overlay::Image(i) => self.emit_image_overlay(i, w, h)?,
                Overlay::Video(v) => self.emit_video_overlay(v, w, h)?,
            }
        }
        Ok(())
    }

    fn emit_text(&mut self, t: &TextOverlay, w: u32, h: u32) -> Result<()> {
        // ── Render text via the rasterise → overlay path ──
        //
        // ffmpeg's `drawtext` filter is too limited to express the
        // full preview style (corner radius, gradient plates,
        // asymmetric padding, per-line `Wrap` plates, glyph stroke
        // tuning, rotation, flip). The previous code papered over
        // those gaps with approximations and the on-canvas position
        // ended up shifted by `box_padding` because drawtext's
        // built-in box doesn't participate in the `(w, h)` used by
        // the overlay-style centring expression.
        //
        // We instead rasterise the text + plate into a transparent
        // PNG at output resolution (see `text_rasterize.rs`) and feed
        // that PNG through the existing image-overlay machinery —
        // which is the same code path the canvas preview's image
        // overlay rendering shares with the renderer. The result is
        // byte-for-byte identical to the preview for all the
        // properties drawtext used to silently drop.
        let raster = match crate::text_rasterize::rasterize_text_overlay(t, w, h) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // empty text — nothing to overlay
            Err(e) => {
                tracing::warn!(
                    text_id = %t.id,
                    error = %e,
                    "text rasterisation failed; falling back to drawtext"
                );
                return self.emit_text_drawtext_fallback(t, w, h);
            }
        };

        // Track the temp PNG so the runner can clean it up after the
        // ffmpeg subprocess finishes. Reusing `mask_assets` keeps the
        // cleanup surface minimal (it's already wired into the
        // builder's `finish` return tuple).
        self.mask_assets.push(raster.png_path.clone());

        let idx = self.add_input(FfmpegInput {
            path: raster.png_path.clone(),
            kind: InputKind::Image,
            r#loop: false,
            seek: None,
            t: None,
        });

        // The PNG was rasterised in OUTPUT resolution at scale=1, so
        // the overlay needs to use the layout's pos/scale exactly the
        // way an ImageOverlay does. The canvas-preview anchor is the
        // *plate centre*; our PNG's plate centre lives at
        // (anchor_dx_from_left, anchor_dy_from_top). Subtract that
        // from the centred pos expression so the PNG slides into
        // place such that its plate centre lands on the layout's
        // world-space centre — including any canvas_layouts override,
        // render_frame motion or modifier-driven offset.
        let xform = crate::expr::build_element_transform(
            self.scene,
            &t.id,
            &t.layout,
            &t.modifiers,
            t.t_in,
            t.skeleton_attachment.as_ref(),
        );
        let scale_part = format!(
            "scale=w='max(2,iw*({sx}))':h='max(2,ih*({sy}))':eval=frame",
            sx = xform.sx_expr,
            sy = xform.sy_expr,
        );
        // The PNG centre lands on `(anchor_dx_from_left,
        // anchor_dy_from_top)`. To put the plate centre on the
        // layout's centre we subtract that anchor offset (relative to
        // the centre of the PNG) from the centre expression.
        let dx = raster.anchor_dx_from_left - raster.width as f32 * 0.5;
        let dy = raster.anchor_dy_from_top - raster.height as f32 * 0.5;
        let x_expr = format!("({cx})-w/2-({dx:.4})", cx = xform.centre_x_expr, dx = dx);
        let y_expr = format!("({cy})-h/2-({dy:.4})", cy = xform.centre_y_expr, dy = dy);

        // Build the pre-overlay chain. We support layout-driven
        // rotation via a per-frame `rotate=` filter (animation is now
        // honoured — previously the renderer sampled rotation only
        // at the layout midpoint). Static flips fall through as
        // `hflip` / `vflip` for the dominant midpoint sign.
        let hflip = xform.hflip;
        let vflip = xform.vflip;
        let rot_part = xform.rot_expr.as_ref().map(|r| {
            format!(
                "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                r = r,
            )
        });

        let txt_label = self.alloc_label("txt");
        let mut chain = format!("[{idx}:v]format=yuva420p", idx = idx);
        chain.push(',');
        chain.push_str(&scale_part);
        if hflip {
            chain.push_str(",hflip");
        }
        if vflip {
            chain.push_str(",vflip");
        }
        if let Some(r) = rot_part {
            chain.push(',');
            chain.push_str(&r);
        }
        if xform.opacity_static < 0.999 {
            chain.push(',');
            chain.push_str(&format!(
                "colorchannelmixer=aa={:.4}",
                xform.opacity_static.clamp(0.0, 1.0),
            ));
        }
        self.chunks
            .push(format!("{chain}{out}", chain = chain, out = txt_label));

        let next = self.alloc_label("textstack");
        self.chunks.push(format!(
            "{cur}{txt}overlay=x='{x}':y='{y}':enable='between(t,{a},{b})':eof_action=pass{out}",
            cur = self.cursor,
            txt = txt_label,
            x = x_expr,
            y = y_expr,
            a = t.t_in,
            b = t.t_out,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    /// Defensive fallback for the rare case where text rasterisation
    /// fails (e.g. no font found AND DejaVuSans not installed) so an
    /// otherwise-good render doesn't produce a blank video. Mirrors
    /// the legacy `drawtext` path verbatim.
    fn emit_text_drawtext_fallback(&mut self, t: &TextOverlay, w: u32, h: u32) -> Result<()> {
        let style = &t.style;
        let (px, py, _, _) = position_and_scale_expr(&t.layout, w, h);
        let escaped = escape_drawtext(&t.text);
        let color = format!(
            "0x{:02X}{:02X}{:02X}",
            style.color[0], style.color[1], style.color[2]
        );
        let mut params = format!(
            "drawtext=text='{txt}':x='{x}':y='{y}':fontsize={size}:fontcolor={c}:enable='between(t,{a},{b})'",
            txt = escaped,
            x = px,
            y = py,
            size = style.font_size,
            c = color,
            a = t.t_in,
            b = t.t_out,
        );
        if let Some(font_path) = crate::fonts::find_font(&style.font, style.bold) {
            let p = font_path.to_string_lossy().replace(':', "\\:");
            params.push_str(&format!(":fontfile='{}'", p));
        }
        if let Some(box_color) = style.box_color {
            let opacity = style.box_opacity.clamp(0.0, 1.0);
            let bc = format!(
                "0x{:02X}{:02X}{:02X}@{:.3}",
                box_color[0], box_color[1], box_color[2], opacity,
            );
            params.push_str(&format!(":box=1:boxcolor={}:boxborderw={}", bc, style.box_padding));
        }
        if let Some(o) = style.outline {
            let oc = format!("0x{:02X}{:02X}{:02X}", o[0], o[1], o[2]);
            params.push_str(&format!(":bordercolor={}:borderw={}", oc, style.outline_width));
        }
        match style.align {
            TextAlign::Center => params.push_str(":text_align=center"),
            TextAlign::Left => params.push_str(":text_align=left"),
            TextAlign::Right => params.push_str(":text_align=right"),
        }
        let next = self.alloc_label("textstack");
        self.chunks.push(format!(
            "{cur}{filter}{out}",
            cur = self.cursor,
            filter = params,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    fn emit_image_overlay(&mut self, ov: &ImageOverlay, _w: u32, _h: u32) -> Result<()> {
        let path = self.resolve(&ov.source);
        let idx = self.add_input(FfmpegInput {
            path,
            kind: InputKind::Image,
            r#loop: false,
            seek: None,
            t: None,
        });
        // Same canvas_layouts / render_frame / easing / modifier
        // pipeline the actor uses — the preview treats image overlays
        // identically to actor clips for positioning and animation.
        let xform = crate::expr::build_element_transform(
            self.scene,
            &ov.id,
            &ov.layout,
            &ov.modifiers,
            ov.t_in,
            ov.skeleton_attachment.as_ref(),
        );
        if xform.opacity_animated {
            tracing::debug!(
                overlay_id = %ov.id,
                "image overlay opacity is keyframed; export uses static midpoint sample",
            );
        }
        let scale_part = format!(
            "scale=w='max(2,iw*({sx}))':h='max(2,ih*({sy}))':eval=frame",
            sx = xform.sx_expr,
            sy = xform.sy_expr,
        );
        // Optional chromakey filter — added when the user picked a
        // colour with the eyedropper. Mirrors the `VideoOverlay` /
        // `Actor` pipelines so the same ChromaKeyParams produces the
        // same visual result regardless of which overlay flavour the
        // user picked.
        let chroma_part = ov.chroma_key.as_ref().map(|ck| {
            let key_hex = format!(
                "0x{:02X}{:02X}{:02X}",
                ck.key_color[0], ck.key_color[1], ck.key_color[2]
            );
            format!("chromakey={}:{}:{}", key_hex, ck.similarity, ck.blend)
        });
        let img_label = self.alloc_label("img");

        if effects_have_mask(&ov.effects) {
            // See `emit_actors` — masks force a multi-stream layout
            // because they need a second input plus alphamerge.
            let pre_label = self.alloc_label("imgPre");
            let mut prefix = format!("[{idx}:v]format=yuva420p", idx = idx);
            if let Some(c) = &chroma_part {
                prefix.push(',');
                prefix.push_str(c);
            }
            if xform.hflip { prefix.push_str(",hflip"); }
            if xform.vflip { prefix.push_str(",vflip"); }
            self.chunks.push(format!(
                "{prefix}{pre_label}",
                prefix = prefix,
                pre_label = pre_label,
            ));
            let after_fx = self.apply_effect_stack(pre_label, &ov.effects, ov.t_in, ov.t_out)?;
            let mut tail: Vec<String> = vec![scale_part];
            if let Some(rot) = &xform.rot_expr {
                tail.push(format!(
                    "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                    r = rot,
                ));
            }
            if xform.opacity_static < 0.999 {
                tail.push(format!(
                    "colorchannelmixer=aa={:.4}",
                    xform.opacity_static.clamp(0.0, 1.0),
                ));
            }
            self.chunks.push(format!(
                "{src}{filters}{out}",
                src = after_fx,
                filters = tail.join(","),
                out = img_label,
            ));
        } else {
            let mut chain = format!("[{idx}:v]format=yuva420p", idx = idx);
            if let Some(ref c) = chroma_part {
                chain.push(',');
                chain.push_str(c);
            }
            if xform.hflip { chain.push_str(",hflip"); }
            if xform.vflip { chain.push_str(",vflip"); }
            // Apply the user-defined effect stack before the layout scale,
            // matching the actor pipeline so e.g. blur / hue shift work in
            // the image's native pixel space (before the on-canvas resize).
            for snippet in effect_stack_filters(&ov.effects, ov.t_in, ov.t_out) {
                chain.push(',');
                chain.push_str(&snippet);
            }
            chain.push(',');
            chain.push_str(&scale_part);
            if let Some(rot) = &xform.rot_expr {
                chain.push(',');
                chain.push_str(&format!(
                    "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                    r = rot,
                ));
            }
            if xform.opacity_static < 0.999 {
                chain.push(',');
                chain.push_str(&format!(
                    "colorchannelmixer=aa={:.4}",
                    xform.opacity_static.clamp(0.0, 1.0),
                ));
            }
            self.chunks.push(format!("{chain}{out}", chain = chain, out = img_label));
        }
        let next = self.alloc_label("imgstack");
        self.chunks.push(format!(
            "{cur}{img}overlay=x='{x}':y='{y}':enable='between(t,{a},{b})':eof_action=pass{out}",
            cur = self.cursor,
            img = img_label,
            x = xform.x_expr,
            y = xform.y_expr,
            a = ov.t_in,
            b = ov.t_out,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    fn emit_video_overlay(&mut self, ov: &VideoOverlay, _w: u32, _h: u32) -> Result<()> {
        let path = self.resolve(&ov.source);
        let idx = self.add_input(FfmpegInput {
            path,
            kind: InputKind::Video,
            r#loop: ov.loop_source,
            seek: if ov.source_start > 0.0 { Some(ov.source_start) } else { None },
            t: None,
        });
        let xform = crate::expr::build_element_transform(
            self.scene,
            &ov.id,
            &ov.layout,
            &ov.modifiers,
            ov.t_in,
            ov.skeleton_attachment.as_ref(),
        );
        if xform.opacity_animated {
            tracing::debug!(
                overlay_id = %ov.id,
                "video overlay opacity is keyframed; export uses static midpoint sample",
            );
        }
        let chroma_part = ov.chroma_key.as_ref().map(|ck| {
            let key_hex = format!(
                "0x{:02X}{:02X}{:02X}",
                ck.key_color[0], ck.key_color[1], ck.key_color[2]
            );
            format!("chromakey={}:{}:{}", key_hex, ck.similarity, ck.blend)
        });
        let speed = ov.speed.max(0.0001);
        let speed_part = if (speed - 1.0).abs() > 1.0e-4 {
            Some(format!("setpts=PTS/{:.6}", speed))
        } else {
            None
        };
        let scale_part = format!(
            "scale=w='max(2,iw*({sx}))':h='max(2,ih*({sy}))':eval=frame",
            sx = xform.sx_expr,
            sy = xform.sy_expr,
        );
        let v_label = self.alloc_label("vid");

        if effects_have_mask(&ov.effects) {
            // Multi-stream layout for masks — see `emit_actors`.
            let mut prefix = format!("[{idx}:v]format=yuva420p", idx = idx);
            if let Some(c) = &chroma_part { prefix.push(','); prefix.push_str(c); }
            if xform.hflip { prefix.push_str(",hflip"); }
            if xform.vflip { prefix.push_str(",vflip"); }
            let pre_label = self.alloc_label("vidPre");
            self.chunks.push(format!("{prefix}{pre_label}", prefix = prefix, pre_label = pre_label));
            let after_fx = self.apply_effect_stack(pre_label, &ov.effects, ov.t_in, ov.t_out)?;
            let mut tail: Vec<String> = Vec::new();
            if let Some(s) = speed_part { tail.push(s); }
            tail.push(scale_part);
            if let Some(rot) = &xform.rot_expr {
                tail.push(format!(
                    "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                    r = rot,
                ));
            }
            if xform.opacity_static < 0.999 {
                tail.push(format!(
                    "colorchannelmixer=aa={:.4}",
                    xform.opacity_static.clamp(0.0, 1.0),
                ));
            }
            self.chunks.push(format!(
                "{src}{filters}{out}",
                src = after_fx,
                filters = tail.join(","),
                out = v_label,
            ));
        } else {
            let mut chain = format!("[{idx}:v]format=yuva420p", idx = idx);
            if let Some(c) = &chroma_part { chain.push(','); chain.push_str(c); }
            if xform.hflip { chain.push_str(",hflip"); }
            if xform.vflip { chain.push_str(",vflip"); }
            // Apply the user-defined effect stack — same convention as
            // images / actors so the export matches what the canvas previews.
            for snippet in effect_stack_filters(&ov.effects, ov.t_in, ov.t_out) {
                chain.push(',');
                chain.push_str(&snippet);
            }
            // Speed multiplier — see `emit_actors` for the math.
            if let Some(s) = &speed_part {
                chain.push(',');
                chain.push_str(s);
            }
            chain.push(',');
            chain.push_str(&scale_part);
            if let Some(rot) = &xform.rot_expr {
                chain.push(',');
                chain.push_str(&format!(
                    "rotate={r}:c=none:ow=hypot(iw\\,ih):oh=hypot(iw\\,ih)",
                    r = rot,
                ));
            }
            if xform.opacity_static < 0.999 {
                chain.push(',');
                chain.push_str(&format!(
                    "colorchannelmixer=aa={:.4}",
                    xform.opacity_static.clamp(0.0, 1.0),
                ));
            }
            self.chunks.push(format!("{chain}{out}", chain = chain, out = v_label));
        }
        let next = self.alloc_label("vidstack");
        self.chunks.push(format!(
            "{cur}{v}overlay=x='{x}':y='{y}':enable='between(t,{a},{b})':eof_action=pass{out}",
            cur = self.cursor,
            v = v_label,
            x = xform.x_expr,
            y = xform.y_expr,
            a = ov.t_in,
            b = ov.t_out,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    /// Apply camera moves (zoom/pan/rotate) to the final composite.
    /// Uses FFmpeg's `crop` + `scale` filters with piecewise expressions
    /// driven by `CameraState` keyframes OR the new `RenderFrame` layout.
    fn emit_camera(&mut self) -> Result<()> {
        // Prefer new render_frame if it has non-trivial keyframes
        let use_render_frame = self.scene.render_frame.layout.len() > 1
            || self.scene.render_frame.layout.first()
                .map(|kf| kf.value.zoom != 1.0 || kf.value.rotation_deg != 0.0)
                .unwrap_or(false);

        if use_render_frame {
            return self.emit_render_frame_camera();
        }

        if self.scene.camera.is_empty() {
            return Ok(());
        }

        let [w, h] = self.scene.output.resolution;
        let camera = &self.scene.camera;

        // Build piecewise expressions for zoom, center_x, center_y, rotation
        let make_expr = |getter: fn(&CameraState) -> f32| -> String {
            if camera.len() == 1 {
                return format!("{}", getter(&camera[0].value));
            }
            let mut expr = format!("{}", getter(&camera.last().unwrap().value));
            for win in camera.windows(2).rev() {
                let (a, b) = (&win[0], &win[1]);
                let v0 = getter(&a.value);
                let v1 = getter(&b.value);
                let span = (b.t - a.t).max(1e-6);
                expr = format!(
                    "if(lt(t,{tb}),{v0}+({v1}-{v0})*((t-{ta})/{span}),{else_})",
                    tb = b.t, v0 = v0, v1 = v1, ta = a.t, span = span, else_ = expr
                );
            }
            expr
        };

        let zoom_expr = make_expr(|c| c.zoom);
        let cx_expr = make_expr(|c| c.center[0]);
        let cy_expr = make_expr(|c| c.center[1]);
        let rot_expr = make_expr(|c| c.rotation_deg);

        // Strategy: same camera viewport semantics as
        // `emit_render_frame_camera` — pad → un-rotate → centre crop
        // → scale. See that method for the full rationale; the only
        // difference here is that the centre is `(cx, cy)` (in
        // normalised coords) instead of always `(0.5, 0.5)`.
        //
        // crop_w = W / zoom, crop_h = H / zoom, clamped to source dims
        // crop_x = cx*W - crop_w/2, crop_y = cy*H - crop_h/2

        let crop_w = format!("min(iw,max(2,floor({w}/({zoom})/2)*2))", w = w, zoom = zoom_expr);
        let crop_h = format!("min(ih,max(2,floor({h}/({zoom})/2)*2))", h = h, zoom = zoom_expr);
        let crop_x = format!(
            "max(0,min(iw-({cw}), floor(({cx})*iw-({cw})/2)))",
            cw = crop_w, cx = cx_expr,
        );
        let crop_y = format!(
            "max(0,min(ih-({ch}), floor(({cy})*ih-({ch})/2)))",
            ch = crop_h, cy = cy_expr,
        );

        let cam_label = self.alloc_label("cam");

        // Check if there's any rotation
        let has_rotation = camera.iter().any(|kf| kf.value.rotation_deg.abs() > 0.01);

        if has_rotation {
            // Match the v2 render-frame camera: pad to hypot before
            // rotating so corners survive, and use **negative**
            // rotation so the camera's "tilt" un-rotates the world
            // into an axis-aligned output. Wrap the angle in single
            // quotes — when keyframes produce piecewise
            // `if(lt(t,…),…,…)` expressions the commas inside would
            // otherwise be parsed as filter-chain separators by
            // FFmpeg, breaking the entire graph.
            let rot_rad = format!("(-({}))*PI/180", rot_expr);
            let pad_label = self.alloc_label("campad");
            self.chunks.push(format!(
                "{cur}pad=w='hypot(iw\\,ih)':h='hypot(iw\\,ih)':x='(ow-iw)/2':y='(oh-ih)/2':color=#00000000{out}",
                cur = self.cursor,
                out = pad_label,
            ));
            let rot_label = self.alloc_label("rot");
            self.chunks.push(format!(
                "{pad}rotate='{rad}':ow=iw:oh=ih:c=none{out}",
                pad = pad_label,
                rad = rot_rad,
                out = rot_label,
            ));
            self.chunks.push(format!(
                "{rot}crop=w='{cw}':h='{ch}':x='{cx}':y='{cy}',scale={w}:{h}{out}",
                rot = rot_label,
                cw = crop_w, ch = crop_h, cx = crop_x, cy = crop_y,
                w = w, h = h, out = cam_label,
            ));
        } else {
            // No rotation: just crop + scale
            self.chunks.push(format!(
                "{cur}crop=w='{cw}':h='{ch}':x='{cx}':y='{cy}',scale={w}:{h}{out}",
                cur = self.cursor,
                cw = crop_w, ch = crop_h, cx = crop_x, cy = crop_y,
                w = w, h = h, out = cam_label,
            ));
        }

        self.cursor = cam_label;
        Ok(())
    }

    /// Emit camera based on the new RenderFrame layout (world-pixel coords).
    /// The render frame defines a region on the canvas; we crop that region
    /// from the composite and scale it to the output resolution.
    fn emit_render_frame_camera(&mut self) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        let rf = &self.scene.render_frame;

        // Build piecewise expressions for render frame zoom
        let make_rf_expr = |getter: fn(&memstroy_core::RenderFrameState) -> f32| -> String {
            if rf.layout.len() == 1 {
                return format!("{}", getter(&rf.layout[0].value));
            }
            let mut expr = format!("{}", getter(&rf.layout.last().unwrap().value));
            for win in rf.layout.windows(2).rev() {
                let (a, b) = (&win[0], &win[1]);
                let v0 = getter(&a.value);
                let v1 = getter(&b.value);
                let span = (b.t - a.t).max(1e-6);
                expr = format!(
                    "if(lt(t,{tb}),{v0}+({v1}-{v0})*((t-{ta})/{span}),{else_})",
                    tb = b.t, v0 = v0, v1 = v1, ta = a.t, span = span, else_ = expr
                );
            }
            expr
        };

        let zoom_expr = make_rf_expr(|s| s.zoom);
        let rot_expr = make_rf_expr(|s| s.rotation_deg);

        // ── Camera viewport semantics ──
        //
        // The composite at this point is a `W×H` canvas, axis-aligned
        // with world axes, with `rf.pos` translated to the canvas
        // centre `(W/2, H/2)` by every overlay's `centre_x_expr` /
        // `centre_y_expr`. The render frame is a tilted / scaled
        // rectangle in world space; the export must show the world
        // content captured by that rectangle, with the rectangle
        // straightened to the output's axis-aligned `W×H`.
        //
        // The pipeline is therefore:
        //
        //   1. Pad the `W×H` composite to `hypot(W,H) × hypot(W,H)`.
        //      Rotating in place with `ow=iw:oh=ih` would chop ~14%
        //      off every corner at a 30° tilt — which the user
        //      reported as "при повёрнутой области рендера клип не
        //      рендерится". The hypot pad guarantees that any rotation
        //      of the original `W×H` content fits in the output frame
        //      without clipping.
        //
        //   2. Rotate the padded composite by **`-rf.rotation_deg`**.
        //      A positive `rotation_deg` tilts the rf rectangle CW in
        //      world space, so producing an axis-aligned output of
        //      "what's inside the rectangle" requires UN-rotating the
        //      world by the same angle. The previous build applied
        //      `+rf.rotation_deg` here, which produced the mirror
        //      image of the camera view — text, images and clips ended
        //      up tilted in the OPPOSITE direction from what the canvas
        //      showed.
        //
        //   3. Crop a centred `W/zoom × H/zoom` region. With `zoom > 1`
        //      this captures less canvas (zoom-in); with `zoom < 1`
        //      we'd want to capture more, but the composite never
        //      extends past `hypot(W,H)`, so we clamp via `min(iw,…)`.
        //
        //   4. Scale the cropped region back to `W×H`.
        let crop_w = format!(
            "min(iw,max(2,floor({w}/({zoom})/2)*2))",
            w = w, zoom = zoom_expr,
        );
        let crop_h = format!(
            "min(ih,max(2,floor({h}/({zoom})/2)*2))",
            h = h, zoom = zoom_expr,
        );
        let crop_x = format!("max(0,(iw-({cw}))/2)", cw = crop_w);
        let crop_y = format!("max(0,(ih-({ch}))/2)", ch = crop_h);

        let cam_label = self.alloc_label("rfcam");

        let has_rotation = rf.layout.iter().any(|kf| kf.value.rotation_deg.abs() > 0.01);
        if has_rotation {
            // Pad → rotate → crop → scale. Sign flip: `-rotation_deg`
            // un-rotates the world so the rf rectangle becomes axis-
            // aligned in the output. `c=#00000000` keeps the corners
            // transparent so backgrounds composited later (or the
            // default chroma-key base) bleed through cleanly.
            //
            // Wrap the angle in single quotes — when the user
            // keyframes the rotation the piecewise expression contains
            // `if(lt(t,…),…,…)` whose commas would otherwise be
            // parsed as filter-chain separators by ffmpeg, breaking
            // the entire graph with "No option name near
            // 'none:ow=iw:oh=ih'".
            let rot_rad = format!("(-({}))*PI/180", rot_expr);
            let pad_label = self.alloc_label("rfpad");
            self.chunks.push(format!(
                "{cur}pad=w='hypot(iw\\,ih)':h='hypot(iw\\,ih)':x='(ow-iw)/2':y='(oh-ih)/2':color=#00000000{out}",
                cur = self.cursor,
                out = pad_label,
            ));
            let rot_label = self.alloc_label("rfrot");
            self.chunks.push(format!(
                "{pad}rotate='{rad}':ow=iw:oh=ih:c=none{out}",
                pad = pad_label, rad = rot_rad, out = rot_label,
            ));
            self.chunks.push(format!(
                "{rot}crop=w='{cw}':h='{ch}':x='{cx}':y='{cy}',scale={w}:{h}{out}",
                rot = rot_label,
                cw = crop_w, ch = crop_h, cx = crop_x, cy = crop_y,
                w = w, h = h, out = cam_label,
            ));
        } else {
            // No rotation → no need to pad; cropping a centred region
            // of the `W×H` composite + scaling to `W×H` is enough.
            self.chunks.push(format!(
                "{cur}crop=w='{cw}':h='{ch}':x='{cx}':y='{cy}',scale={w}:{h}{out}",
                cur = self.cursor,
                cw = crop_w, ch = crop_h, cx = crop_x, cy = crop_y,
                w = w, h = h, out = cam_label,
            ));
        }

        self.cursor = cam_label;
        Ok(())
    }

    fn emit_audio(&mut self) -> Result<()> {
        if self.scene.audio.is_empty() {
            return Ok(());
        }
        // ── Audio pipeline ─────────────────────────────────────────
        //
        // The previous implementation wired every track straight into
        // `amix=inputs=N:normalize=0` with nothing but a `volume` +
        // `adelay`. ffmpeg's `amix` filter REQUIRES uniform sample
        // rate, channel layout and sample format across all inputs —
        // anything else aborts the *entire* filter graph at init time
        // with "Invalid argument" (-22). When that happens both
        // streams fail in lockstep:
        //
        //   * libx264 can't accept frames because its filtergraph
        //     dependency died → `vost#0/libx264 Task finished with
        //     error code: -22`,
        //   * AAC never sees a sample → `aost#0/aac Could not open
        //     encoder before EOF`,
        //
        // and ffmpeg writes the file with `Lsize=0KiB` and
        // `frame=0` — the exact failure mode the user hit when
        // mixing `416_2_audio_R` (extracted at 48 kHz) with
        // `453_3_audio_R` (44.1 kHz). The fix is to normalise every
        // track BEFORE amix and pin the post-mix bus once more so
        // the AAC encoder sees a stable PCM stream regardless of how
        // each source happens to have been encoded.
        //
        // The chain per track (in order) is:
        //   1. `aresample` to a fixed sample rate so amix's mixers
        //      align channel for channel.
        //   2. `aformat` to lock sample format + layout (stereo).
        //   3. `atrim=duration={clip_dur}` to cap the source at its
        //      intended on-timeline length BEFORE we shift it into
        //      place — otherwise a long source bleeds past `t_out`
        //      and we lose control of the mix.
        //   4. `asetpts=PTS-STARTPTS` to reset timestamps so adelay
        //      treats clip-local time as zero.
        //   5. `volume` (forced to 0 when `mute` is set).
        //   6. `afade` for fade_in / fade_out (curves match the GUI
        //      preview).
        //   7. `adelay={t_in_ms}|...` to position on the scene
        //      timeline.
        //   8. `apad=whole_dur={scene_dur}` so the per-track chain
        //      always reaches the end of the scene with silence;
        //      this guarantees the AAC encoder receives at least one
        //      frame to flush even when all sources end early.
        //   9. `atrim=duration={scene_dur}` + `asetpts=PTS-STARTPTS`
        //      caps the chain at the scene length so no track over-
        //      shoots `-t` and trips the muxer.
        //
        // After the mix we apply one more `aformat` to nail the bus
        // to the AAC encoder's expected layout (the `-ac 2 -ar 44100`
        // output flags are belt-and-braces but only work if the
        // filter actually delivers that format).
        const SR: u32 = 44_100;
        const FMT: &str = "fltp";
        const LAYOUT: &str = "stereo";

        let scene_dur = self.scene.output.duration.max(1.0 / 60.0);

        let mut audio_labels = Vec::new();
        for tr in &self.scene.audio {
            let path = self.resolve(&tr.source);
            // Defence in depth: if the audio file went missing (user
            // moved/deleted it after dropping it on the timeline) the
            // filter `[idx:a]` reference would fail at graph init.
            // Skip with a warning so the rest of the export still
            // runs.
            if !path.exists() {
                tracing::warn!(
                    track_id = %tr.id,
                    path = %path.display(),
                    "audio track source not found on disk; skipping",
                );
                continue;
            }
            // Defence in depth #2 — by far the most common cause of
            // the "Could not open encoder before EOF" / "libx264 Task
            // finished with error code: -22" double-failure the user
            // hit on render: `add_actor_from_clip_at_time` in the GUI
            // auto-pushes an `AudioTrack` referencing the same source
            // file as the dropped actor so the embedded soundtrack
            // gets its own row on the audio lanes. When the source
            // clip happens to be silent (gif-to-mp4 conversions,
            // screen captures without a mic, the editor's own muted
            // exports), the auto-track still gets created and the
            // renderer's `[idx:a]aresample=…` reference resolves to
            // nothing. ffmpeg then aborts filter-graph configuration
            // with "Stream specifier ':a' matches no streams" — and
            // because that abort kills the *whole* graph BOTH encoder
            // threads die together, with the symptom pair the user
            // reported. ffprobing the source up front lets us drop
            // the track gracefully instead of taking down the whole
            // export.
            if !crate::proc::probe_has_audio_stream(&path) {
                tracing::warn!(
                    track_id = %tr.id,
                    path = %path.display(),
                    "audio track source has no audio stream; skipping (silent source clip?)",
                );
                continue;
            }

            let idx = self.add_input(FfmpegInput {
                path,
                kind: InputKind::Audio,
                r#loop: false,
                seek: if tr.source_start > 0.0 { Some(tr.source_start) } else { None },
                t: None,
            });

            let t_in = tr.t_in.max(0.0).min(scene_dur);
            let t_out = tr
                .t_out
                .unwrap_or(scene_dur)
                .clamp(t_in, scene_dur);
            let clip_dur = (t_out - t_in).max(1.0 / 60.0);

            // Mute overrides the static volume completely — the GUI's
            // M button is a hard mute, not a "remember the volume"
            // switch.
            let volume = if tr.mute { 0.0 } else { tr.volume.max(0.0) };

            let mut filters: Vec<String> = Vec::with_capacity(10);
            filters.push(format!("aresample={sr}", sr = SR));
            filters.push(format!(
                "aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}",
                fmt = FMT,
                sr = SR,
                ch = LAYOUT,
            ));
            filters.push(format!("atrim=duration={:.6}", clip_dur));
            filters.push("asetpts=PTS-STARTPTS".into());
            if tr.fade_in > 0.0 {
                let fi = tr.fade_in.min(clip_dur);
                if fi > 0.0 {
                    filters.push(format!(
                        "afade=t=in:st=0:d={:.6}:curve=tri",
                        fi
                    ));
                }
            }
            if tr.fade_out > 0.0 {
                let fo = tr.fade_out.min(clip_dur);
                if fo > 0.0 {
                    let st = (clip_dur - fo).max(0.0);
                    filters.push(format!(
                        "afade=t=out:st={:.6}:d={:.6}:curve=tri",
                        st, fo
                    ));
                }
            }
            // `volume=1` is a no-op — skip emitting it to keep the
            // graph string short for the common unmuted-default case.
            if (volume - 1.0).abs() > 1e-4 {
                filters.push(format!("volume={:.6}", volume));
            }
            let delay_ms = (t_in * 1000.0).round().max(0.0) as u64;
            if delay_ms > 0 {
                // `all=1` propagates the same delay to every channel
                // even when the input layout disagrees with the per-
                // channel list — robust against odd source layouts.
                filters.push(format!("adelay={d}:all=1", d = delay_ms));
            }
            filters.push(format!("apad=whole_dur={:.6}", scene_dur));
            filters.push(format!("atrim=duration={:.6}", scene_dur));
            filters.push("asetpts=PTS-STARTPTS".into());

            let lbl = self.alloc_label("a");
            self.chunks.push(format!(
                "[{idx}:a]{filters}{out}",
                idx = idx,
                filters = filters.join(","),
                out = lbl,
            ));
            audio_labels.push(lbl);
        }

        if audio_labels.is_empty() {
            // All tracks were skipped (sources missing). Render
            // proceeds without an audio stream — better than failing
            // the whole encode for a single bad reference.
            return Ok(());
        }

        let mix = self.alloc_label("amix");
        if audio_labels.len() == 1 {
            // Single-track fast path: skip amix entirely and just
            // re-pin the format. amix=inputs=1 also works but adds a
            // pointless mix node and a normalize coefficient.
            self.chunks.push(format!(
                "{src}aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}{out}",
                src = audio_labels[0],
                fmt = FMT,
                sr = SR,
                ch = LAYOUT,
                out = mix,
            ));
        } else {
            let inputs = audio_labels.join("");
            let mixed = self.alloc_label("amixraw");
            // `dropout_transition=0` keeps each input at full gain
            // even after another input EOFs (with our apad above no
            // input EOFs early anyway, but this stays safe in case a
            // future change removes the pad). `normalize=0` matches
            // the GUI preview's mixer where each track's static
            // volume is preserved verbatim.
            self.chunks.push(format!(
                "{inputs}amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0{out}",
                inputs = inputs,
                n = audio_labels.len(),
                out = mixed,
            ));
            self.chunks.push(format!(
                "{src}aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}{out}",
                src = mixed,
                fmt = FMT,
                sr = SR,
                ch = LAYOUT,
                out = mix,
            ));
        }
        self.map_audio = Some(mix);
        Ok(())
    }

    // ─── EFFECT STACK + MASKS ──────────────────────────────────────
    //
    // The bulk of the per-element effect pipeline is single-pass: each
    // entry compiles to one or more comma-joined filters appended to
    // the element's chain. `EffectKind::Mask` is the exception — its
    // alpha shape is in element-local UV space, which FFmpeg can't
    // express in a single-pass filter. So we render a grayscale alpha
    // PNG once per Mask instance, add it as a synthetic image input,
    // and stitch it into the chain with `alphamerge`. The mask is
    // multiplied with the element's existing alpha (rather than
    // replacing it) so chromakey results survive the masking step,
    // matching the live-preview semantics in `image_effects.rs`.

    /// Walk `effects` and apply them on top of `current` (a labelled
    /// stream). Non-mask effects are buffered into a single comma
    /// chunk for compactness; each `Mask` flushes the buffer to its
    /// own labelled stage, then emits an alphamerge sub-graph and
    /// returns the new label so subsequent effects continue from
    /// there. Returns the label of the final stream.
    ///
    /// `t_in` / `t_out` define the host clip's visible window —
    /// passed through to `effect_animated_filter_chain` so per-
    /// parameter keyframes are honoured for non-mask effects on the
    /// stack. Mask geometry is not animated by the model so it falls
    /// through to the static path either way.
    fn apply_effect_stack(
        &mut self,
        mut current: String,
        effects: &[Effect],
        t_in: f32,
        t_out: f32,
    ) -> Result<String> {
        let mut buffer: Vec<String> = Vec::new();
        for eff in effects {
            if !eff.enabled { continue; }
            // Mask carries no per-parameter keyframes the renderer
            // honours, so it stays on the static intensity path.
            // Animated mask intensity is not modelled either; the
            // `:001` clamp matches the historical fast path.
            let i_static = eff.intensity.clamp(0.0, 1.0);
            if let EffectKind::Mask { shape, feather, invert } = &eff.kind {
                if i_static <= 0.001 { continue; }
                current = self.flush_effect_buffer(current, &mut buffer);
                current = self.emit_mask_alphamerge(current, shape, *feather, *invert, i_static)?;
            } else if let Some(snippet) = crate::expr::effect_animated_filter_chain(
                eff,
                t_in,
                t_out,
                |kind, intensity| effect_to_filter(kind, intensity),
            ) {
                buffer.push(snippet);
            }
        }
        current = self.flush_effect_buffer(current, &mut buffer);
        Ok(current)
    }

    /// Drain `buffer` into a single comma-joined filter chunk attached
    /// to `current`. Returns either the unchanged `current` (when the
    /// buffer is empty) or the label of the new output stream.
    fn flush_effect_buffer(&mut self, current: String, buffer: &mut Vec<String>) -> String {
        if buffer.is_empty() { return current; }
        let next = self.alloc_label("fxchain");
        self.chunks.push(format!(
            "{cur}{joined}{out}",
            cur = current,
            joined = buffer.join(","),
            out = next,
        ));
        buffer.clear();
        next
    }

    /// Emit an alphamerge sub-graph that masks the existing alpha of
    /// `current` against a generated grayscale PNG. The returned label
    /// is the new output stream (with composite alpha already
    /// applied). The PNG is registered for cleanup so it's removed
    /// after FFmpeg finishes regardless of success / failure.
    fn emit_mask_alphamerge(
        &mut self,
        current: String,
        shape: &MaskShape,
        feather: f32,
        invert: bool,
        intensity: f32,
    ) -> Result<String> {
        let png_path = self.generate_mask_png(shape, feather, invert, intensity)?;
        let mask_idx = self.add_input(FfmpegInput {
            path: png_path,
            kind: InputKind::Image,
            r#loop: false,
            seek: None,
            t: None,
        });

        // Sub-graph layout (labels are fresh per call):
        //
        //   [current]format=yuva420p,fps={fps},setpts=PTS-STARTPTS,setsar=1,split=2[mainA][mainB];
        //   [mainA]alphaextract,format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1[mainAlpha];
        //   [mask_idx:v]format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1[maskRaw];
        //   [maskRaw][mainB]scale2ref=w=main_w:h=main_h[maskScaledRaw][mainBp];
        //   [maskScaledRaw]format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1[maskScaled];
        //   [mainAlpha][maskScaled]blend=all_mode=multiply:all_opacity=1:shortest=0:repeatlast=1[combined];
        //   [mainBp][combined]alphamerge[masked]
        //
        // - `split` duplicates the main stream so we can both extract
        //   its alpha (for the multiply) and use the colour data on
        //   the alphamerge side.
        // - `scale2ref` resizes the mask PNG to the source's pixel
        //   dimensions; the mask is authored in UV space so any
        //   reference resolution works, and stretching gives a soft
        //   anti-aliased edge that matches the GPU preview. The
        //   filter is deprecated in ffmpeg 7.0+ ("use scale=rw:rh
        //   instead") but the replacement isn't available in older
        //   builds we still support, so we keep it AND patch around
        //   the framesync-init regression that came with the same
        //   release. See below.
        // - `blend=multiply` combines the two alpha planes so the
        //   element's existing alpha (chromakey, prior masks) is
        //   preserved instead of being overwritten.
        //
        // Why the explicit `format/fps/setpts/setsar` belt around
        // every link going into `blend` and `alphamerge`:
        //
        //   ffmpeg 7.0 tightened `framesync_configure()` (the helper
        //   `blend` uses to pair its two inputs frame-by-frame). It
        //   now refuses to initialise when the two inputs disagree on
        //   timebase, sample-aspect-ratio, or frame-rate metadata —
        //   conditions that fall out trivially when one input is a
        //   looped still PNG (`-loop 1 -framerate fps -i mask.png`)
        //   and the other is video-derived through `split` +
        //   `alphaextract`. The user-facing symptom is a four-line
        //   tombstone in stderr:
        //
        //       [Parsed_scale2ref_33] scale2ref is deprecated, use scale=rw:rh instead
        //       [Parsed_blend_34] Failed to configure output pad on Parsed_blend_34
        //       Error reinitializing filters!
        //       Failed to inject frame into filter network: Invalid argument
        //
        //   …followed by the usual chain reaction (libx264 task -22,
        //   AAC "Could not open encoder before EOF", `Lsize=0KiB`,
        //   "Conversion failed!"). Pinning every blend/alphamerge
        //   input to `gray @ {fps} fps, sar 1:1, PTS-STARTPTS` makes
        //   `framesync_configure()` agree on a common axis again and
        //   the graph initialises cleanly on every ffmpeg ≥ 4.x.
        let fps = self.scene.output.fps;
        let main_a = self.alloc_label("maskMainA");
        let main_b = self.alloc_label("maskMainB");
        self.chunks.push(format!(
            "{cur}format=yuva420p,fps={fps},setpts=PTS-STARTPTS,setsar=1,split=2{a}{b}",
            cur = current, fps = fps, a = main_a, b = main_b,
        ));
        let main_alpha = self.alloc_label("maskMainAlpha");
        self.chunks.push(format!(
            "{a}alphaextract,format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1{aout}",
            a = main_a, fps = fps, aout = main_alpha,
        ));
        let mask_raw = self.alloc_label("maskRaw");
        self.chunks.push(format!(
            "[{idx}:v]format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1{m}",
            idx = mask_idx, fps = fps, m = mask_raw,
        ));
        // scale2ref pre-stage: produces a resized (but timing-naive)
        // mask plus a passthrough of the reference. We immediately
        // wrap the resized output in a fresh format/fps/setpts/setsar
        // chain so it carries the same axis as `mainAlpha` going into
        // `blend`. The passthrough mainBp is also re-stamped before
        // it reaches alphamerge — alphamerge uses framesync too.
        let mask_scaled_pre = self.alloc_label("maskScaledRaw");
        let main_bp_pre = self.alloc_label("maskMainBpRaw");
        self.chunks.push(format!(
            "{m}{b}scale2ref=w=main_w:h=main_h{ms_pre}{bp_pre}",
            m = mask_raw, b = main_b, ms_pre = mask_scaled_pre, bp_pre = main_bp_pre,
        ));
        let mask_scaled = self.alloc_label("maskScaled");
        self.chunks.push(format!(
            "{ms_pre}format=gray,fps={fps},setpts=PTS-STARTPTS,setsar=1{ms}",
            ms_pre = mask_scaled_pre, fps = fps, ms = mask_scaled,
        ));
        let main_bp = self.alloc_label("maskMainBp");
        self.chunks.push(format!(
            "{bp_pre}fps={fps},setpts=PTS-STARTPTS,setsar=1{bp}",
            bp_pre = main_bp_pre, fps = fps, bp = main_bp,
        ));
        let combined = self.alloc_label("maskCombined");
        // `shortest=0:repeatlast=1` tells framesync to keep the still
        // mask alive for the full duration of the video stream
        // instead of bailing at the first "EOF" from the looped PNG
        // input. Without this, on some ffmpeg builds the blend would
        // emit a single frame and then silently shut down the chain.
        self.chunks.push(format!(
            "{ma}{ms}blend=all_mode=multiply:all_opacity=1:shortest=0:repeatlast=1{c}",
            ma = main_alpha, ms = mask_scaled, c = combined,
        ));
        let masked = self.alloc_label("masked");
        self.chunks.push(format!(
            "{bp}{c}alphamerge{out}",
            bp = main_bp, c = combined, out = masked,
        ));
        Ok(masked)
    }

    /// Generate a grayscale PNG for the given mask parameters and
    /// return its path. The image is 2048×2048 (UV-space; the
    /// filtergraph stretches it to source resolution at render time
    /// via `scale2ref`). Each pixel encodes the per-pixel alpha keep
    /// factor blended with `intensity` exactly like
    /// `image_effects::sample_mask_alpha`, so the FFmpeg result lines
    /// up with the live preview pixel-for-pixel.
    fn generate_mask_png(
        &mut self,
        shape: &MaskShape,
        feather: f32,
        invert: bool,
        intensity: f32,
    ) -> Result<PathBuf> {
        const SIZE: u32 = 2048;
        let mut buf = vec![0u8; (SIZE * SIZE) as usize];
        let inv_dim = 1.0 / SIZE as f32;
        let f = feather.clamp(0.0, 0.5).max(1e-6);
        let hard_edge = feather <= 1e-6;
        let i = intensity.clamp(0.0, 1.0);
        for y in 0..SIZE {
            let v = (y as f32 + 0.5) * inv_dim;
            let row = (y as usize) * SIZE as usize;
            for x in 0..SIZE {
                let u = (x as f32 + 0.5) * inv_dim;
                let margin = shape.signed_margin_uv(u, v);
                let mut keep = if hard_edge {
                    if margin >= 0.0 { 1.0 } else { 0.0 }
                } else {
                    (margin / f + 0.5).clamp(0.0, 1.0)
                };
                if invert { keep = 1.0 - keep; }
                // Same intensity blend as `apply_mask_alpha`:
                // i = 0  → keep_eff = 1.0 (mask is a no-op),
                // i = 1  → keep_eff = keep.
                let keep_eff = 1.0 - i * (1.0 - keep);
                buf[row + x as usize] = (keep_eff * 255.0).clamp(0.0, 255.0) as u8;
            }
        }

        // Filename includes pid + time + counter so concurrent renders
        // (e.g. GUI scrubber + CLI export) don't collide.
        let counter = self.mask_assets.len() as u32 + self.label_counter;
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let filename = format!("memstroy-mask-{}-{}-{}.png", pid, nanos, counter);
        let path = std::env::temp_dir().join(filename);

        let img: image::GrayImage = image::ImageBuffer::from_raw(SIZE, SIZE, buf)
            .ok_or_else(|| anyhow::anyhow!("failed to wrap mask buffer as GrayImage"))?;
        img.save_with_format(&path, image::ImageFormat::Png)
            .with_context(|| format!("write mask PNG to {}", path.display()))?;
        self.mask_assets.push(path.clone());
        Ok(path)
    }
}

/// Build piecewise-linear FFmpeg expressions for x/y position (in pixels)
/// and scale (multiplier). Returns `(x_expr, y_expr, scale_x_expr,
/// scale_y_expr)`. The Y axis scale is `scale * scale_y` (where `scale_y`
/// defaults to 1.0 for proportional scaling).
fn position_and_scale_expr<S>(layout: &[Keyframe<S>], w: u32, h: u32) -> (String, String, String, String)
where
    S: PositionedState,
{
    if layout.is_empty() {
        return (
            format!("(W-w)/2"),
            format!("(H-h)/2"),
            format!("1.0"),
            format!("1.0"),
        );
    }
    if layout.len() == 1 {
        let s = &layout[0].value;
        let pos = s.pos();
        let sx = s.scale();
        let sy = s.scale() * s.scale_y();
        return (
            format!("({}*W-w/2)", pos[0]),
            format!("({}*H-h/2)", pos[1]),
            format!("{}", sx),
            format!("{}", sy),
        );
    }
    // Multi-keyframe linear: build an `if(lt(t,t1), v0+(v1-v0)*((t-t0)/(t1-t0)), if(lt(t,t2), ...))`
    let _ = (w, h);
    let make = |getter: fn(&S) -> f32| -> String {
        let mut expr = format!("{}", getter(&layout.last().unwrap().value));
        for w in layout.windows(2).rev() {
            let (a, b) = (&w[0], &w[1]);
            let v0 = getter(&a.value);
            let v1 = getter(&b.value);
            let span = (b.t - a.t).max(1e-6);
            expr = format!(
                "if(lt(t,{tb}), {v0}+({v1}-{v0})*((t-{ta})/{span}), {else_})",
                tb = b.t,
                v0 = v0,
                v1 = v1,
                ta = a.t,
                span = span,
                else_ = expr,
            );
        }
        expr
    };
    let nx = make(|s| s.pos()[0]);
    let ny = make(|s| s.pos()[1]);
    let ns = make(|s| s.scale());
    let nsy_factor = make(|s| s.scale_y());
    (
        format!("({}*W-w/2)", nx),
        format!("({}*H-h/2)", ny),
        ns.clone(),
        format!("({})*({})", ns, nsy_factor),
    )
}

/// Internal shim so `position_and_scale_expr` can work for both
/// `ActorState` and `OverlayState` without exposing private fields.
trait PositionedState {
    fn pos(&self) -> [f32; 2];
    fn scale(&self) -> f32;
    fn scale_y(&self) -> f32;
}
impl PositionedState for ActorState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
    fn scale_y(&self) -> f32 { self.scale_y }
}
impl PositionedState for OverlayState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
    fn scale_y(&self) -> f32 { self.scale_y }
}

/// Escape a user string for use inside a `drawtext=text='...'` arg.
/// FFmpeg's filtergraph requires escaping `:` `\` `'` `%` and commas.
fn escape_drawtext(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\u2019"), // approximate, drawtext doesn't accept escaped quotes
            ':' => out.push_str("\\:"),
            '%' => out.push_str("\\%"),
            ',' => out.push_str("\\,"),
            _ => out.push(ch),
        }
    }
    out
}

/// Convert an `AnchorPoint` enum to the string key used in AnchorTrack JSON.
fn anchor_point_to_name(ap: AnchorPoint) -> String {
    // serde serializes with snake_case, which matches our track keys
    serde_json::to_string(&ap)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

/// Build FFmpeg piecewise-linear position expressions for a prop that
/// tracks an anchor point from an AnchorTrack.
///
/// The anchor track provides normalised [0,1] positions per sample time.
/// We combine these with the actor's own layout (position/scale) to get
/// final scene-space pixel coordinates for the prop overlay.
fn build_anchor_position_expr(
    track: &AnchorTrack,
    anchor_name: &str,
    offset: [f32; 2],
    _prop_scale: f32,
    actor_layout: &[Keyframe<ActorState>],
    w: u32,
    h: u32,
) -> (String, String) {
    // Collect samples where this anchor has data
    let points: Vec<(f32, f32, f32)> = track
        .samples
        .iter()
        .filter_map(|s| {
            s.points.get(anchor_name).map(|kp| (s.t, kp.x, kp.y))
        })
        .collect();

    if points.is_empty() {
        // No anchor data — fall back to actor center + offset
        let (ax, ay, _, _) = position_and_scale_expr(actor_layout, w, h);
        return (
            format!("{}+{}", ax, offset[0]),
            format!("{}+{}", ay, offset[1]),
        );
    }

    // Build piecewise if(lt(t,...)) expressions for x and y.
    // The anchor coords are in video-normalised space [0,1].
    // To convert to scene pixel position:
    //   scene_x = actor_pos_x_pixels + (anchor_x - 0.5) * actor_scaled_width + offset_x
    //
    // Simplified approach: since the actor is already overlaid at its position,
    // and the prop overlay is placed on the full scene canvas, we compute:
    //   prop_scene_x = actor_scene_x + (anchor_norm_x - 0.5) * actor_width * actor_scale + offset_x
    //
    // For FFmpeg expressions: the overlay x/y are relative to the main canvas.
    // We'll use the actor's position expression as base and offset by the anchor delta.

    let (actor_x_expr, actor_y_expr, actor_scale_expr, _actor_scale_y_expr) =
        position_and_scale_expr(actor_layout, w, h);

    // For simplicity with many samples, we limit to at most 20 keypoints
    // (FFmpeg expressions have practical length limits)
    let max_samples = 20;
    let step = (points.len() / max_samples).max(1);
    let sampled: Vec<&(f32, f32, f32)> = points.iter().step_by(step).collect();

    if sampled.len() <= 1 {
        // Single point — static offset
        let (_, ax, ay) = sampled.first().map(|&&p| p).unwrap_or((0.0, 0.5, 0.5));
        let dx = (ax - 0.5) * w as f32;
        let dy = (ay - 0.5) * h as f32;
        return (
            format!("({}+{}*{}+{})-w/2", actor_x_expr, dx, actor_scale_expr, offset[0]),
            format!("({}+{}*{}+{})-h/2", actor_y_expr, dy, actor_scale_expr, offset[1]),
        );
    }

    // Build piecewise expression for anchor offset from center
    let build_piecewise = |sampled: &[&(f32, f32, f32)], getter: &dyn Fn(&(f32, f32, f32)) -> f32| -> String {
        let last_val = getter(sampled.last().unwrap());
        let mut expr = format!("{}", last_val);
        for window in sampled.windows(2).rev() {
            let (t0, ..) = window[0];
            let (t1, ..) = window[1];
            let v0 = getter(window[0]);
            let v1 = getter(window[1]);
            let span = (t1 - t0).max(1e-6);
            expr = format!(
                "if(lt(t,{}),{}+({})*((t-{})/{})  ,{})",
                t1, v0, v1 - v0, t0, span, expr
            );
        }
        expr
    };

    // anchor_x values (normalised 0..1, we want delta from 0.5 * scene_width)
    let ax_expr = build_piecewise(&sampled, &|p| (p.1 - 0.5) * w as f32);
    let ay_expr = build_piecewise(&sampled, &|p| (p.2 - 0.5) * h as f32);

    // Final position: actor_base + anchor_delta * actor_scale + offset - prop_center
    let prop_x = format!(
        "({}+({})*{}+{})-w/2",
        actor_x_expr, ax_expr, actor_scale_expr, offset[0]
    );
    let prop_y = format!(
        "({}+({})*{}+{})-h/2",
        actor_y_expr, ay_expr, actor_scale_expr, offset[1]
    );

    (prop_x, prop_y)
}



// ─── EFFECT STACK FILTERS ────────────────────────────────────────────
//
// Translate the per-element effect stack (`Vec<Effect>`) into a list of
// filtergraph snippets that can be appended to an actor / overlay's
// existing chain. Each entry returns one or more comma-separated
// filters; the caller joins them with the rest of the chain. Effects
// that we cannot reasonably express in ffmpeg are emitted as a no-op
// (`null`) with a comment-style snippet so the chain remains valid.
//
// The intensity slider is folded into each snippet so the user gets a
// continuous "fade in / out" of every effect — a 0.0 intensity always
// renders as a no-op.

fn effect_stack_filters(effects: &[Effect], t_in: f32, t_out: f32) -> Vec<String> {
    let mut out = Vec::with_capacity(effects.len());
    for eff in effects {
        if !eff.enabled { continue; }
        if let Some(s) = crate::expr::effect_animated_filter_chain(
            eff,
            t_in,
            t_out,
            |kind, intensity| effect_to_filter(kind, intensity),
        ) {
            out.push(s);
        }
    }
    out
}

/// Quick predicate used by emitters to decide whether the element's
/// effect stack contains any [`EffectKind::Mask`] entries that are
/// actually live (enabled and with non-zero intensity). When the
/// answer is `false` we keep the historical single-chunk emission
/// path; when it's `true` we switch to the multi-chunk layout that
/// can host the alphamerge sub-graph for each mask.
fn effects_have_mask(effects: &[Effect]) -> bool {
    effects.iter().any(|e| {
        e.enabled
            && e.intensity.clamp(0.0, 1.0) > 0.001
            && matches!(e.kind, EffectKind::Mask { .. })
    })
}

fn effect_to_filter(kind: &EffectKind, i: f32) -> Option<String> {
    use EffectKind as K;
    Some(match kind {
        K::Blur { radius } => format!("boxblur=luma_radius={r}:luma_power=1", r = (radius * i).max(0.5) as i32),
        K::Sharpen { amount } => format!(
            "unsharp=lx=5:ly=5:la={la:.4}:cx=5:cy=5:ca=0",
            la = (amount * i).clamp(0.0, 3.0),
        ),
        K::Grayscale => format!(
            // Keyed `colorchannelmixer` form so the per-segment
            // animation path can append `:enable=...` without colliding
            // with the legacy positional argument list.
            "colorchannelmixer=rr=.299*{i}+1-{i}:rg=.587*{i}:rb=.114*{i}:ra=0:gr=.299*{i}:gg=.587*{i}+1-{i}:gb=.114*{i}:ga=0:br=.299*{i}:bg=.587*{i}:bb=.114*{i}+1-{i}:ba=0",
            i = i,
        ),
        K::Sepia => format!(
            "colorchannelmixer=rr={a}:rg={b}:rb={c}:ra=0:gr={d}:gg={e}:gb={f}:ga=0:br={g}:bg={h}:bb={j}:ba=0",
            a = 0.393 * i + (1.0 - i), b = 0.769 * i, c = 0.189 * i,
            d = 0.349 * i, e = 0.686 * i + (1.0 - i), f = 0.168 * i,
            g = 0.272 * i, h = 0.534 * i, j = 0.131 * i + (1.0 - i),
        ),
        K::Invert => {
            // Mix between source and negate via a per-pixel subtraction.
            // ffmpeg has `negate` which is full-strength only; use lutrgb
            // for an intensity-aware version.
            format!(
                "lutrgb=r='val+(255-2*val)*{i}':g='val+(255-2*val)*{i}':b='val+(255-2*val)*{i}'",
                i = i,
            )
        }
        K::HueShift { degrees } => format!("hue=h={}", degrees * i),
        K::Vignette { strength } => format!(
            "vignette=angle=PI/3*{s:.4}:mode=forward",
            s = (strength * i).clamp(0.0, 1.0),
        ),
        K::Pixelate { block_size } => {
            // Down/up scale via neighbour sampling for the pixelation look.
            let bs = (block_size * i).max(1.0) as i32;
            format!(
                "scale=w=iw/{bs}:h=ih/{bs}:flags=neighbor,scale=w=iw*{bs}:h=ih*{bs}:flags=neighbor",
                bs = bs.max(1)
            )
        }
        K::Posterize { levels } => format!(
            "lutrgb=r='floor(val/(255/{l}))*255/({l}-1)':g='floor(val/(255/{l}))*255/({l}-1)':b='floor(val/(255/{l}))*255/({l}-1)'",
            l = (*levels).max(2),
        ),
        K::Glow { radius, intensity } => {
            let r = (radius * i).max(1.0) as i32;
            // Approximate via gblur + blend:add at a reduced intensity.
            // We can't easily express both sides of a blend in a chain
            // without splitting; the simpler `eq=brightness` bumps light
            // pixels enough for a reasonable approximation in single-pass.
            format!(
                "gblur=sigma={r},eq=brightness={b}",
                r = r,
                b = (intensity * i * 0.15).clamp(0.0, 0.5),
            )
        }
        K::Brightness { amount } => format!("eq=brightness={}", amount * i),
        K::Contrast { amount } => format!("eq=contrast={}", 1.0 + amount * i),
        K::Saturation { amount } => format!("eq=saturation={}", 1.0 + amount * i),
        K::EdgeDetect { threshold: _ } => "edgedetect=mode=colormix".to_string(),
        K::MirrorH => "hflip".to_string(),
        K::MirrorV => "vflip".to_string(),
        K::ChromaticAberration { offset } => {
            // `rgbashift` separates the per-channel offsets: keep G centered,
            // shift R left and B right by `offset` pixels.
            let o = (offset * i).round() as i32;
            format!("rgbashift=rh={l}:bh={r}", l = -o, r = o)
        }
        K::Noise { amount } => {
            let strength = (amount * i * 80.0).clamp(0.0, 100.0) as i32;
            format!("noise=alls={}:allf=t", strength)
        }
        K::Wave { amplitude: _, wavelength: _ } => {
            // Time/space-varying displacement is awkward in ffmpeg without
            // GLSL — skip cleanly to keep the export stable.
            return None;
        }
        K::OldFilm => format!(
            "colorchannelmixer=rr=.393:rg=.769:rb=.189:ra=0:gr=.349:gg=.686:gb=.168:ga=0:br=.272:bg=.534:bb=.131:ba=0,vignette=angle=PI/3*0.7,noise=alls={n}:allf=t",
            n = (i * 12.0) as i32,
        ),
        K::Vhs => format!(
            "rgbashift=rh=-{o}:bh={o},noise=alls={n}:allf=t",
            o = (4.0 * i).round() as i32,
            n = (i * 8.0) as i32,
        ),
        K::Glitch { strength: _ } => {
            // Real per-frame glitching needs an enable expression and is
            // out of scope for the static stack — emit a low-frequency
            // chromatic shift as a stand-in so the user sees something.
            format!("rgbashift=rh=-{o}:bh={o}", o = (i * 6.0).round() as i32)
        }
        K::Bloom { radius } => format!("gblur=sigma={}", (radius * i).max(1.0)),
        K::Mask { shape: _, feather: _, invert: _ } => {
            // Mask is handled by the multi-stream alphamerge sub-graph
            // emitted from `FilterGraphBuilder::emit_mask_alphamerge`,
            // not via the single-pass comma chain. Returning `None`
            // here lets the legacy `effect_stack_filters` path skip
            // the entry safely; the new `apply_effect_stack` method
            // never calls this arm because it special-cases `Mask`
            // before reaching `effect_to_filter`.
            return None;
        }
        K::Crop { left, top, right, bottom } => {
            // Express the visible window as a `crop=w:h:x:y` filter using
            // ffmpeg's `iw` / `ih` source dimensions. The sub-image is then
            // padded back to the source size so the surrounding pipeline
            // (scale/overlay) keeps working with the same dimensions —
            // padded pixels are transparent so the crop reads as a mask
            // when the element is composited over a background.
            let l = (left * i).clamp(0.0, 0.49);
            let t = (top * i).clamp(0.0, 0.49);
            let r = (right * i).clamp(0.0, 0.49);
            let b = (bottom * i).clamp(0.0, 0.49);
            let w = (1.0 - l - r).max(0.02);
            let h = (1.0 - t - b).max(0.02);
            format!(
                "crop=w=iw*{w:.4}:h=ih*{h:.4}:x=iw*{l:.4}:y=ih*{t:.4},pad=w=iw/{w:.4}:h=ih/{h:.4}:x=iw*{lp:.4}:y=ih*{tp:.4}:color=0x00000000",
                w = w, h = h, l = l, t = t,
                lp = l / (1.0 - l - r).max(0.001),
                tp = t / (1.0 - t - b).max(0.001),
            )
        }
        K::ColorKey { color, similarity, blend, spill: _, invert } => {
            // FFmpeg's `chromakey` filter does the same HSV-distance
            // alpha keying we run on the CPU preview side, so the
            // exported video matches the live editor frame exactly.
            // `intensity` (the master envelope) scales the similarity
            // distance: a faded effect keys a smaller core. `invert`
            // is achieved by swapping `chromakey` for `chromahold`,
            // which keeps the keyed region instead of cutting it.
            // De-spill is intentionally omitted — the existing per-
            // element `chromakey` filter at the actor / overlay level
            // already runs spill suppression for the source colour;
            // the effect-stack ColorKey is meant for compositing
            // touch-ups where spill rarely matters.
            //
            // Named-argument form (`color=` / `similarity=` / `blend=`)
            // is required so the per-segment animation path can append
            // `:enable='between(t,a,b)'` without the parser
            // mis-reading it as a fourth positional arg.
            let key_hex = format!(
                "0x{:02X}{:02X}{:02X}",
                color[0], color[1], color[2],
            );
            let sim = (similarity * i).clamp(0.0, 1.0);
            let blend = blend.clamp(0.0, 1.0);
            if *invert {
                format!(
                    "chromahold=color={hex}:similarity={sim:.4}:blend={blend:.4}",
                    hex = key_hex,
                    sim = sim,
                    blend = blend,
                )
            } else {
                format!(
                    "chromakey=color={hex}:similarity={sim:.4}:blend={blend:.4}",
                    hex = key_hex,
                    sim = sim,
                    blend = blend,
                )
            }
        }
    })
}
