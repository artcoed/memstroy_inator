use std::path::{Path, PathBuf};

use anyhow::Result;
use memstroy_core::*;

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

            // Apply a transition by altering the overlay alpha at the
            // segment start. For the first iteration we support only
            // Cut / Fade. Snap is identical to Cut visually (with a
            // 1-frame flash), Slide* are TODO.
            let composed = self.alloc_label("bgstack");
            let alpha_expr = match bg.transition {
                Transition::Fade => format!(
                    "fade=t=in:st={a}:d=0.25:alpha=1,fade=t=out:st={fade_out}:d=0.25:alpha=1",
                    a = bg.start,
                    fade_out = (bg.start + bg.duration - 0.25).max(bg.start),
                ),
                _ => String::new(),
            };
            let staged = if alpha_expr.is_empty() {
                scaled
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

            self.chunks.push(format!(
                "{cur}{staged}overlay=enable='between(t,{a},{b})':eof_action=pass{out}",
                cur = self.cursor,
                staged = staged,
                a = bg.start,
                b = bg.start + bg.duration,
                out = composed
            ));
            self.cursor = composed;
        }
        Ok(())
    }

    fn emit_actors(&mut self) -> Result<()> {
        let [w, h] = self.scene.output.resolution;
        for actor in &self.scene.actors {
            let path = self.resolve(&actor.source);
            let idx = self.add_input(FfmpegInput {
                path,
                kind: InputKind::Video,
                r#loop: actor.loop_source,
                seek: if actor.source_start > 0.0 { Some(actor.source_start) } else { None },
                t: None,
            });

            // chromakey + despill (despill is a no-op approximation
            // using `colorchannelmixer`; full despill comes later).
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

            // Resolve the actor's animation: position + scale at the
            // start keyframe (full piecewise expressions are added in
            // a follow-up). For now we emit the value at t=0 and a
            // linear ramp toward the next keyframe if present.
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
