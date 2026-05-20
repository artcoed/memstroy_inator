use std::path::{Path, PathBuf};

use anyhow::Result;
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
        }
    }

    pub fn finish(self) -> (String, Vec<FfmpegInput>, String, Option<String>) {
        let map_video = self.cursor.clone();
        (self.chunks.join(";\n"), self.inputs, map_video, self.map_audio)
    }

    pub fn build(&mut self) -> Result<()> {
        self.emit_base_canvas();
        self.emit_backgrounds()?;
        self.emit_actors()?;
        self.emit_overlays()?;
        self.emit_camera()?;
        self.emit_audio()?;
        Ok(())
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
        let [r, g, b] = self.scene.output.background_color;
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

    fn emit_actors(&mut self) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        for actor in &self.scene.actors {
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
            let mut chain = format!(
                "[{idx}:v]chromakey={hex}:{sim}:{blend},format=yuva420p",
                idx = idx,
                hex = key_hex,
                sim = actor.chroma_key.similarity,
                blend = actor.chroma_key.blend,
            );
            if actor.flip_horizontal {
                chain.push_str(",hflip");
            }

            let (pos_x, pos_y, scale_expr) = position_and_scale_expr(&actor.layout, w, h);
            chain.push_str(&format!(",scale=w='iw*{scale_expr}':h='ih*{scale_expr}':eval=frame"));

            let actor_label = self.alloc_label("actor");
            self.chunks.push(format!("{chain}{out}", chain = chain, out = actor_label));

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
                x = pos_x,
                y = pos_y,
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
                let (ax, ay, _) = position_and_scale_expr(&actor.layout, w, h);
                (
                    format!("{}+{}", ax, attachment.offset[0]),
                    format!("{}+{}", ay, attachment.offset[1]),
                )
            };

            // Scale expression: actor_scale * attachment.scale
            let (_, _, actor_scale) = position_and_scale_expr(&actor.layout, w, h);
            let prop_scale = format!("{}*{}", actor_scale, attachment.scale);

            // Build filter chain for the prop
            let chain = format!(
                "[{idx}:v]format=yuva420p,scale=w='iw*{s}':h='ih*{s}':eval=frame",
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

    fn emit_overlays(&mut self) -> Result<()> {
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

    fn emit_text(&mut self, t: &TextOverlay, w: u32, h: u32) -> Result<()> {
        let style = &t.style;
        let (px, py, _) = position_and_scale_expr(&t.layout, w, h);
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
            // drawtext requires colons inside the path to be escaped.
            let p = font_path.to_string_lossy().replace(':', "\\:");
            params.push_str(&format!(":fontfile='{}'", p));
        }
        if let Some(box_color) = style.box_color {
            let bc = format!(
                "0x{:02X}{:02X}{:02X}@1",
                box_color[0], box_color[1], box_color[2]
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

    fn emit_image_overlay(&mut self, ov: &ImageOverlay, w: u32, h: u32) -> Result<()> {
        let path = self.resolve(&ov.source);
        let idx = self.add_input(FfmpegInput {
            path,
            kind: InputKind::Image,
            r#loop: false,
            seek: None,
            t: None,
        });
        let (x, y, scale_expr) = position_and_scale_expr(&ov.layout, w, h);
        let chain = format!(
            "[{idx}:v]format=yuva420p,scale=w='iw*{s}':h='ih*{s}':eval=frame",
            idx = idx,
            s = scale_expr,
        );
        let img_label = self.alloc_label("img");
        self.chunks.push(format!("{chain}{out}", chain = chain, out = img_label));
        let next = self.alloc_label("imgstack");
        self.chunks.push(format!(
            "{cur}{img}overlay=x='{x}':y='{y}':enable='between(t,{a},{b})':eof_action=pass{out}",
            cur = self.cursor,
            img = img_label,
            x = x,
            y = y,
            a = ov.t_in,
            b = ov.t_out,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    fn emit_video_overlay(&mut self, ov: &VideoOverlay, w: u32, h: u32) -> Result<()> {
        let path = self.resolve(&ov.source);
        let idx = self.add_input(FfmpegInput {
            path,
            kind: InputKind::Video,
            r#loop: ov.loop_source,
            seek: if ov.source_start > 0.0 { Some(ov.source_start) } else { None },
            t: None,
        });
        let (x, y, scale_expr) = position_and_scale_expr(&ov.layout, w, h);
        let mut chain = format!("[{idx}:v]format=yuva420p", idx = idx);
        if let Some(ck) = &ov.chroma_key {
            let key_hex = format!(
                "0x{:02X}{:02X}{:02X}",
                ck.key_color[0], ck.key_color[1], ck.key_color[2]
            );
            chain.push_str(&format!(",chromakey={}:{}:{}", key_hex, ck.similarity, ck.blend));
        }
        chain.push_str(&format!(",scale=w='iw*{s}':h='ih*{s}':eval=frame", s = scale_expr));
        let v_label = self.alloc_label("vid");
        self.chunks.push(format!("{chain}{out}", chain = chain, out = v_label));
        let next = self.alloc_label("vidstack");
        self.chunks.push(format!(
            "{cur}{v}overlay=x='{x}':y='{y}':enable='between(t,{a},{b})':eof_action=pass{out}",
            cur = self.cursor,
            v = v_label,
            x = x,
            y = y,
            a = ov.t_in,
            b = ov.t_out,
            out = next,
        ));
        self.cursor = next;
        Ok(())
    }

    /// Apply camera moves (zoom/pan/rotate) to the final composite.
    /// Uses FFmpeg's `crop` + `scale` filters with piecewise expressions
    /// driven by `CameraState` keyframes.
    fn emit_camera(&mut self) -> Result<()> {
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

        // Strategy: use crop to select a sub-region of the composite,
        // then scale back up to output resolution. This simulates zoom + pan.
        //
        // crop_w = W / zoom, crop_h = H / zoom
        // crop_x = center_x * W - crop_w / 2
        // crop_y = center_y * H - crop_h / 2
        //
        // Then scale the cropped region back to WxH.

        let crop_w = format!("floor({w}/({zoom})/2)*2", w = w, zoom = zoom_expr);
        let crop_h = format!("floor({h}/({zoom})/2)*2", h = h, zoom = zoom_expr);
        let crop_x = format!(
            "max(0,min({w}-floor({w}/({zoom})/2)*2, floor(({cx})*{w}-floor({w}/({zoom})/2)*2/2)))",
            w = w, zoom = zoom_expr, cx = cx_expr
        );
        let crop_y = format!(
            "max(0,min({h}-floor({h}/({zoom})/2)*2, floor(({cy})*{h}-floor({h}/({zoom})/2)*2/2)))",
            h = h, zoom = zoom_expr, cy = cy_expr
        );

        let cam_label = self.alloc_label("cam");

        // Check if there's any rotation
        let has_rotation = camera.iter().any(|kf| kf.value.rotation_deg.abs() > 0.01);

        if has_rotation {
            // With rotation: use rotate filter before crop
            let rot_rad = format!("({})*PI/180", rot_expr);
            let rot_label = self.alloc_label("rot");
            self.chunks.push(format!(
                "{cur}rotate={rad}:c=none:ow=iw:oh=ih{out}",
                cur = self.cursor,
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

    fn emit_audio(&mut self) -> Result<()> {
        if self.scene.audio.is_empty() {
            return Ok(());
        }
        // For the first iteration we mix all audio inputs with amix.
        let mut audio_labels = Vec::new();
        for tr in &self.scene.audio {
            let path = self.resolve(&tr.source);
            let idx = self.add_input(FfmpegInput {
                path,
                kind: InputKind::Audio,
                r#loop: false,
                seek: if tr.source_start > 0.0 { Some(tr.source_start) } else { None },
                t: None,
            });
            let lbl = self.alloc_label("a");
            self.chunks.push(format!(
                "[{idx}:a]volume={v},adelay={d}|{d}{out}",
                idx = idx,
                v = tr.volume,
                d = (tr.t_in * 1000.0) as u64,
                out = lbl,
            ));
            audio_labels.push(lbl);
        }
        if audio_labels.is_empty() {
            return Ok(());
        }
        let inputs = audio_labels.join("");
        let mix = self.alloc_label("amix");
        self.chunks.push(format!(
            "{inputs}amix=inputs={n}:normalize=0{out}",
            inputs = inputs,
            n = audio_labels.len(),
            out = mix
        ));
        self.map_audio = Some(mix);
        Ok(())
    }
}

/// Build piecewise-linear FFmpeg expressions for x/y position (in pixels)
/// and scale (multiplier). Falls back to safe defaults for empty tracks.
fn position_and_scale_expr<S>(layout: &[Keyframe<S>], w: u32, h: u32) -> (String, String, String)
where
    S: PositionedState,
{
    if layout.is_empty() {
        return (
            format!("(W-w)/2"),
            format!("(H-h)/2"),
            format!("1.0"),
        );
    }
    if layout.len() == 1 {
        let s = &layout[0].value;
        let pos = s.pos();
        return (
            format!("({}*W-w/2)", pos[0]),
            format!("({}*H-h/2)", pos[1]),
            format!("{}", s.scale()),
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
    (
        format!("({}*W-w/2)", nx),
        format!("({}*H-h/2)", ny),
        ns,
    )
}

/// Internal shim so `position_and_scale_expr` can work for both
/// `ActorState` and `OverlayState` without exposing private fields.
trait PositionedState {
    fn pos(&self) -> [f32; 2];
    fn scale(&self) -> f32;
}
impl PositionedState for ActorState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
}
impl PositionedState for OverlayState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
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
    prop_scale: f32,
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
        let (ax, ay, _) = position_and_scale_expr(actor_layout, w, h);
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

    let (actor_x_expr, actor_y_expr, actor_scale_expr) =
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
