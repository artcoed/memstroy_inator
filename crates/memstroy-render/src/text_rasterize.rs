//! Rasterise a [`TextOverlay`] (text + plate + outline + wrap mode) to
//! a transparent RGBA PNG that the filter graph can overlay through the
//! same machinery as a static image.
//!
//! ## Why not `drawtext`?
//!
//! ffmpeg's built-in `drawtext` filter is fast, but it can only draw a
//! single rectangular box around the text and ignores corner radius,
//! gradients, asymmetric padding, per-line plate widths
//! (`TextBoxKind::Wrap`), rotation and flip — all of which the canvas
//! preview supports. The user's complaint
//! ("результат рендера должен точно соответствовать тому, что было в
//! предпросмотре … много багов, например текст в непонятном месте")
//! traces directly to that gap: every visible difference between the
//! editor's preview and the rendered MP4 originated in `drawtext`'s
//! limitations.
//!
//! ## What this does
//!
//! 1. Parses the chosen TTF/OTF (via [`crate::fonts::find_font`]).
//! 2. Lays out each text line in the chosen font + size.
//! 3. Computes the same plate geometry the canvas preview uses
//!    (`box_padding`, `box_extra_left/right`, `box_corner_radius`,
//!    `TextBoxKind::Wrap` per-line plates, …).
//! 4. Paints plate(s) and glyphs into an [`image::RgbaImage`].
//! 5. Saves the result to a temp PNG and returns its path together
//!    with the on-canvas anchor offset the overlay path needs.
//!
//! Rotation and flip are deliberately **not** baked into the PNG:
//! they're applied at compose-time via ffmpeg `rotate=`/`hflip`/
//! `vflip` filters so animation keyframes still work.

use std::path::PathBuf;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use image::{ImageBuffer, Rgba, RgbaImage};
use memstroy_core::{TextAlign, TextBoxKind, TextOverlay};

/// Output of [`rasterize_text_overlay`] — everything the filter graph
/// needs to overlay the rasterised text on the composite canvas.
pub struct RasterizedText {
    /// Absolute path to a transparent RGBA PNG containing the plate
    /// and glyphs, sized to the natural box plus padding.
    pub png_path: PathBuf,
    /// PNG width in output-resolution pixels.
    pub width: u32,
    /// PNG height in output-resolution pixels.
    pub height: u32,
    /// Distance from the PNG's left edge to the on-canvas anchor
    /// (i.e. the point that `pos.x * output_w` maps to). Subtract
    /// from the overlay X expression so the rasterised image's
    /// anchor lines up with the same logical "centre" the canvas
    /// preview uses.
    pub anchor_dx_from_left: f32,
    /// Same, vertically.
    pub anchor_dy_from_top: f32,
}

/// Rasterise `txt` against an `output_w × output_h` canvas. Returns
/// `Ok(None)` when the text body is empty — callers should skip the
/// overlay entirely in that case.
pub fn rasterize_text_overlay(
    txt: &TextOverlay,
    _output_w: u32,
    _output_h: u32,
) -> Result<Option<RasterizedText>> {
    let style = &txt.style;
    let raw_lines: Vec<&str> = if txt.text.is_empty() {
        return Ok(None);
    } else {
        txt.text.lines().collect()
    };
    if raw_lines.iter().all(|l| l.is_empty()) {
        return Ok(None);
    }

    // ── Font selection ────────────────────────────────────────────
    let prefer_bold = style.bold;
    let font_path = crate::fonts::find_font(&style.font, prefer_bold)
        .or_else(|| crate::fonts::find_font("DejaVuSans", prefer_bold))
        .or_else(|| crate::fonts::find_font("Arial", prefer_bold))
        .ok_or_else(|| {
            anyhow!(
                "no font found for family '{}' (and no DejaVuSans / Arial fallback either) — \
                 install a TTF or set MEMSTROY_FONT_DIR",
                style.font,
            )
        })?;
    let font_bytes = std::fs::read(&font_path)
        .with_context(|| format!("read font {}", font_path.display()))?;
    let font = FontVec::try_from_vec(font_bytes)
        .with_context(|| format!("parse font {}", font_path.display()))?;

    // ── Layout maths ───────────────────────────────────────────────
    // Render text in OUTPUT resolution pixels. The canvas preview
    // multiplies font_size by `canvas_zoom` for screen display, but
    // that's a viewport zoom; the model's font_size is already in
    // output-pixel units (matches what `drawtext fontsize=` consumed
    // before this rewrite).
    // ── Hardening: bound everything that feeds the canvas allocation ──
    // `style.*` come verbatim from an untrusted scene file. Without these
    // caps a value like `box_padding = 1e9` makes the saturating `f32 as u32`
    // cast below produce a ~u32::MAX canvas dimension and the final
    // `ImageBuffer::from_pixel` attempts a multi-terabyte allocation (panic /
    // OOM = render+preview DoS). Clamp the inputs and hard-cap the canvas.
    const MAX_FONT_SIZE: f32 = 2048.0;
    const MAX_TEXT_PAD: f32 = 4096.0;
    const MAX_TEXT_CANVAS_DIM: u32 = 8192;
    let font_size = style.font_size.max(1.0).min(MAX_FONT_SIZE);
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_h = font_size * 1.2;

    // Per-line widths via ab_glyph (advance-only, no kerning beyond
    // what ab_glyph already exposes via h_advance — close enough to
    // egui's measurement that the plate stays glued to the text).
    let line_widths: Vec<f32> = raw_lines
        .iter()
        .map(|line| measure_line_width(&font, scale, line))
        .collect();
    let max_line_w = line_widths.iter().cloned().fold(0.0_f32, f32::max);
    let total_h = raw_lines.len() as f32 * line_h;

    // Padding & extras (in output-resolution pixels, as authored).
    // The preview multiplies these by zoom * scale; the render-side
    // PNG is rasterised at output resolution at scale = 1.0 (we let
    // the per-frame scale_expr resize the final PNG via the overlay
    // path, so animation continues to drive size).
    let pad = style.box_padding.max(0.0).min(MAX_TEXT_PAD);
    let pad_l = style.box_extra_left.max(0.0).min(MAX_TEXT_PAD);
    let pad_r = style.box_extra_right.max(0.0).min(MAX_TEXT_PAD);
    let radius = style.box_corner_radius.max(0.0).min(MAX_TEXT_PAD);
    let plate_w = max_line_w + pad * 2.0 + pad_l + pad_r;
    let plate_h = total_h + pad * 2.0;

    // Stroke bleed — the glyph outline can extend beyond the plate by
    // up to `outline_width` px. Pad the canvas so we don't clip it.
    let stroke_w = if style.outline.is_some() {
        style.outline_width.max(0.0).min(MAX_TEXT_PAD)
    } else {
        0.0
    };
    // Saturating arithmetic + a hard cap: `plate_w`/`plate_h` can still be huge
    // when the text string itself is pathologically long, so the final `as u32`
    // casts saturate and we clamp the result to MAX_TEXT_CANVAS_DIM. This bounds
    // the allocation to at most 8192*8192*4 ≈ 256 MiB instead of crashing.
    let canvas_extra = (stroke_w.ceil() as u32).saturating_add(2);
    let canvas_w = (plate_w.ceil() as u32)
        .saturating_add(canvas_extra.saturating_mul(2))
        .clamp(2, MAX_TEXT_CANVAS_DIM);
    let canvas_h = (plate_h.ceil() as u32)
        .saturating_add(canvas_extra.saturating_mul(2))
        .clamp(2, MAX_TEXT_CANVAS_DIM);

    let plate_origin_x = canvas_extra as f32;
    let plate_origin_y = canvas_extra as f32;
    let text_block_left = plate_origin_x + pad + pad_l;
    let text_block_top = plate_origin_y + pad;

    // ── Allocate canvas ───────────────────────────────────────────
    let mut img: RgbaImage = ImageBuffer::from_pixel(canvas_w, canvas_h, Rgba([0, 0, 0, 0]));

    // ── Plate background ──────────────────────────────────────────
    if let Some(box_color) = style.box_color {
        let box_alpha = (style.box_opacity.clamp(0.0, 1.0) * 255.0) as u8;
        let primary = Rgba([box_color[0], box_color[1], box_color[2], box_alpha]);

        match style.box_kind {
            TextBoxKind::None => {}
            TextBoxKind::Solid => {
                draw_rounded_rect(
                    &mut img,
                    plate_origin_x,
                    plate_origin_y,
                    plate_w,
                    plate_h,
                    radius,
                    primary,
                );
            }
            TextBoxKind::Gradient => {
                let end_rgb = style.box_gradient_end.unwrap_or(box_color);
                let end = Rgba([end_rgb[0], end_rgb[1], end_rgb[2], box_alpha]);
                draw_vertical_gradient_rect(
                    &mut img,
                    plate_origin_x,
                    plate_origin_y,
                    plate_w,
                    plate_h,
                    radius,
                    primary,
                    end,
                );
            }
            TextBoxKind::OutlineOnly => {
                let border_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
                let border_color = Rgba([border_rgb[0], border_rgb[1], border_rgb[2], box_alpha]);
                let bw = if style.box_outline_width > 0.0 {
                    style.box_outline_width
                } else {
                    2.0
                };
                draw_rounded_rect_outline(
                    &mut img,
                    plate_origin_x,
                    plate_origin_y,
                    plate_w,
                    plate_h,
                    radius,
                    bw,
                    border_color,
                );
            }
            TextBoxKind::Wrap => {
                // Per-line plates: each hugs its own line's width.
                // The block's outer top/bottom padding extends the
                // first / last plate so the block looks padded as a
                // whole.
                let n = raw_lines.len();
                for (li, lw) in line_widths.iter().enumerate() {
                    let line_total_w = lw + pad * 2.0;
                    let line_center_x = match style.align {
                        TextAlign::Left => plate_origin_x + pad + lw * 0.5,
                        TextAlign::Right => plate_origin_x + plate_w - pad - lw * 0.5,
                        TextAlign::Center => plate_origin_x + plate_w * 0.5,
                    };
                    let pl_x = line_center_x - line_total_w * 0.5 - pad_l;
                    let pl_w = line_total_w + pad_l + pad_r;
                    let mut pl_y = text_block_top + li as f32 * line_h;
                    let mut pl_h = line_h;
                    if li == 0 {
                        pl_y -= pad;
                        pl_h += pad;
                    }
                    if li + 1 == n {
                        pl_h += pad;
                    }
                    draw_rounded_rect(&mut img, pl_x, pl_y, pl_w, pl_h, radius, primary);
                }
            }
            TextBoxKind::FitText => {
                // Plate hugs the text block as tightly as possible —
                // exactly `max_line_w` × `total_h`, ignoring
                // box_padding / box_extra_left/right entirely. The
                // glyph block already starts at `text_block_left`,
                // `text_block_top`; we re-anchor on those so the
                // plate sits right under the text regardless of
                // alignment-driven positioning.
                draw_rounded_rect(
                    &mut img,
                    text_block_left,
                    text_block_top,
                    max_line_w,
                    total_h,
                    radius,
                    primary,
                );
            }
        }

        // Plate border (any non-Wrap, non-OutlineOnly variant — Wrap
        // and OutlineOnly already handle borders themselves).
        if style.box_outline_width > 0.0
            && !matches!(style.box_kind, TextBoxKind::OutlineOnly | TextBoxKind::Wrap | TextBoxKind::FitText)
        {
            let border_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
            let border_color = Rgba([border_rgb[0], border_rgb[1], border_rgb[2], box_alpha]);
            draw_rounded_rect_outline(
                &mut img,
                plate_origin_x,
                plate_origin_y,
                plate_w,
                plate_h,
                radius,
                style.box_outline_width,
                border_color,
            );
        }
    }

    // ── Glyph stroke ──────────────────────────────────────────────
    let glyph_alpha = 255u8;
    let glyph_color = Rgba([style.color[0], style.color[1], style.color[2], glyph_alpha]);

    if stroke_w > 0.5 {
        if let Some(stroke_rgb) = style.outline {
            let stroke_color = Rgba([stroke_rgb[0], stroke_rgb[1], stroke_rgb[2], glyph_alpha]);
            // Repeat-and-offset stroke approximation, matching the
            // canvas preview's 8-step circular tap pattern.
            let n_steps = 8;
            for step in 0..n_steps {
                let theta = (step as f32) / (n_steps as f32) * std::f32::consts::TAU;
                let dx = theta.cos() * stroke_w;
                let dy = theta.sin() * stroke_w;
                draw_text_block(
                    &mut img,
                    &font,
                    scale,
                    ascent,
                    descent,
                    line_h,
                    &raw_lines,
                    &line_widths,
                    text_block_left,
                    text_block_top,
                    plate_origin_x,
                    plate_w,
                    pad,
                    style.align,
                    dx,
                    dy,
                    stroke_color,
                );
            }
        }
    }

    // ── Glyph fill ────────────────────────────────────────────────
    let bold_offsets: &[f32] = if style.bold && !font_face_is_bold(&font_path) {
        // Synthesise weight by drawing each line a couple of sub-pixel
        // offsets to the right (matches the canvas preview's bold
        // synthesis when the bundled font has no real bold variant).
        &[0.0, 0.7, 1.4]
    } else {
        &[0.0]
    };
    for &dx in bold_offsets {
        draw_text_block(
            &mut img,
            &font,
            scale,
            ascent,
            descent,
            line_h,
            &raw_lines,
            &line_widths,
            text_block_left,
            text_block_top,
            plate_origin_x,
            plate_w,
            pad,
            style.align,
            dx,
            0.0,
            glyph_color,
        );
    }

    // ── Save to temp file ────────────────────────────────────────
    let temp_dir = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let safe_id: String = txt
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(48)
        .collect();
    let png_path = temp_dir.join(format!(
        "memstroy_text_{}_{}.png",
        if safe_id.is_empty() { "anon" } else { safe_id.as_str() },
        stamp,
    ));
    img.save(&png_path)
        .with_context(|| format!("write rasterised text to {}", png_path.display()))?;

    // The on-canvas anchor mirrors the canvas preview's `center_pos`
    // — it's the text-block centre, NOT the plate centre. Asymmetric
    // L/R extras shift the plate's centre but the text/anchor stay
    // put. Without this, an "extra-left=80" plate with pos=[0.5,…]
    // would end up rendered with its plate centre on the canvas
    // centre instead of its text centre, and the visible text would
    // shift 40 px to the right of where the user placed it in the
    // preview.
    let text_block_center_x = text_block_left + max_line_w * 0.5;
    let text_block_center_y = text_block_top + total_h * 0.5;
    Ok(Some(RasterizedText {
        png_path,
        width: canvas_w,
        height: canvas_h,
        anchor_dx_from_left: text_block_center_x,
        anchor_dy_from_top: text_block_center_y,
    }))
}

// ─── Drawing helpers ────────────────────────────────────────────────

fn font_face_is_bold(path: &std::path::Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    stem.contains("bold") || stem.contains("black") || stem.contains("heavy")
}

/// Measure the horizontal advance of `line` at the given font scale,
/// in pixels. Mirrors what egui's text layout produces well enough for
/// the plate to hug the glyphs without visible gap.
fn measure_line_width(font: &FontVec, scale: PxScale, line: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut w = 0.0_f32;
    let mut prev_glyph: Option<ab_glyph::GlyphId> = None;
    for ch in line.chars() {
        let glyph_id = font.glyph_id(ch);
        if let Some(prev) = prev_glyph {
            w += scaled.kern(prev, glyph_id);
        }
        w += scaled.h_advance(glyph_id);
        prev_glyph = Some(glyph_id);
    }
    w
}

#[allow(clippy::too_many_arguments)]
fn draw_text_block(
    img: &mut RgbaImage,
    font: &FontVec,
    scale: PxScale,
    ascent: f32,
    _descent: f32,
    line_h: f32,
    lines: &[&str],
    line_widths: &[f32],
    text_block_left: f32,
    text_block_top: f32,
    plate_origin_x: f32,
    plate_w: f32,
    pad: f32,
    align: TextAlign,
    dx: f32,
    dy: f32,
    color: Rgba<u8>,
) {
    let scaled = font.as_scaled(scale);
    for (li, line) in lines.iter().enumerate() {
        let line_w = line_widths[li];
        let line_x_left = match align {
            TextAlign::Left => text_block_left,
            TextAlign::Right => plate_origin_x + plate_w - pad - line_w,
            TextAlign::Center => plate_origin_x + plate_w * 0.5 - line_w * 0.5,
        };
        // Baseline is `text_block_top + li*line_h + ascent` — same
        // convention egui's galley uses (rendered glyphs sit above the
        // baseline by their bearing).
        let baseline_y = text_block_top + li as f32 * line_h + ascent;
        let mut pen_x = line_x_left;
        let mut prev_glyph: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            let glyph_id = font.glyph_id(ch);
            if let Some(prev) = prev_glyph {
                pen_x += scaled.kern(prev, glyph_id);
            }
            let glyph = glyph_id.with_scale_and_position(
                scale,
                ab_glyph::point(pen_x + dx, baseline_y + dy),
            );
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bb = outlined.px_bounds();
                outlined.draw(|x, y, c| {
                    let px = bb.min.x + x as f32;
                    let py = bb.min.y + y as f32;
                    if px < 0.0 || py < 0.0 {
                        return;
                    }
                    let pxu = px as u32;
                    let pyu = py as u32;
                    if pxu >= img.width() || pyu >= img.height() {
                        return;
                    }
                    let alpha = (c * color[3] as f32).clamp(0.0, 255.0) as u8;
                    if alpha == 0 {
                        return;
                    }
                    let dst = img.get_pixel_mut(pxu, pyu);
                    blend_over(dst, color, alpha);
                });
            }
            pen_x += scaled.h_advance(glyph_id);
            prev_glyph = Some(glyph_id);
        }
    }
}

/// Standard `over` alpha compositing (premultiplied output).
fn blend_over(dst: &mut Rgba<u8>, src_color: Rgba<u8>, src_alpha: u8) {
    let sa = src_alpha as f32 / 255.0;
    let inv = 1.0 - sa;
    let dr = dst[0] as f32 * inv + src_color[0] as f32 * sa;
    let dg = dst[1] as f32 * inv + src_color[1] as f32 * sa;
    let db = dst[2] as f32 * inv + src_color[2] as f32 * sa;
    let da = dst[3] as f32 * inv + src_alpha as f32;
    dst[0] = dr.clamp(0.0, 255.0) as u8;
    dst[1] = dg.clamp(0.0, 255.0) as u8;
    dst[2] = db.clamp(0.0, 255.0) as u8;
    dst[3] = da.clamp(0.0, 255.0) as u8;
}

fn draw_rounded_rect(
    img: &mut RgbaImage,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: Rgba<u8>,
) {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let x0 = x.max(0.0) as i32;
    let y0 = y.max(0.0) as i32;
    let x1 = ((x + w).min(img.width() as f32)) as i32;
    let y1 = ((y + h).min(img.height() as f32)) as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let coverage = rounded_rect_coverage(fx, fy, x, y, w, h, r);
            if coverage <= 0.0 {
                continue;
            }
            let alpha = (color[3] as f32 * coverage).clamp(0.0, 255.0) as u8;
            if alpha == 0 {
                continue;
            }
            let dst = img.get_pixel_mut(px as u32, py as u32);
            blend_over(dst, color, alpha);
        }
    }
}

fn draw_rounded_rect_outline(
    img: &mut RgbaImage,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    border_w: f32,
    color: Rgba<u8>,
) {
    if border_w <= 0.0 {
        return;
    }
    let r_outer = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let inner_w = (w - border_w * 2.0).max(0.0);
    let inner_h = (h - border_w * 2.0).max(0.0);
    let r_inner = (r_outer - border_w).max(0.0);
    let x0 = x.max(0.0) as i32;
    let y0 = y.max(0.0) as i32;
    let x1 = ((x + w).min(img.width() as f32)) as i32;
    let y1 = ((y + h).min(img.height() as f32)) as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let outer = rounded_rect_coverage(fx, fy, x, y, w, h, r_outer);
            let inner = rounded_rect_coverage(
                fx,
                fy,
                x + border_w,
                y + border_w,
                inner_w,
                inner_h,
                r_inner,
            );
            let cov = (outer - inner).max(0.0);
            if cov <= 0.0 {
                continue;
            }
            let alpha = (color[3] as f32 * cov).clamp(0.0, 255.0) as u8;
            if alpha == 0 {
                continue;
            }
            let dst = img.get_pixel_mut(px as u32, py as u32);
            blend_over(dst, color, alpha);
        }
    }
}

fn draw_vertical_gradient_rect(
    img: &mut RgbaImage,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    top: Rgba<u8>,
    bottom: Rgba<u8>,
) {
    let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
    let x0 = x.max(0.0) as i32;
    let y0 = y.max(0.0) as i32;
    let x1 = ((x + w).min(img.width() as f32)) as i32;
    let y1 = ((y + h).min(img.height() as f32)) as i32;
    for py in y0..y1 {
        for px in x0..x1 {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let cov = rounded_rect_coverage(fx, fy, x, y, w, h, r);
            if cov <= 0.0 {
                continue;
            }
            let t = ((fy - y) / h.max(0.001)).clamp(0.0, 1.0);
            let col = Rgba([
                lerp_u8(top[0], bottom[0], t),
                lerp_u8(top[1], bottom[1], t),
                lerp_u8(top[2], bottom[2], t),
                lerp_u8(top[3], bottom[3], t),
            ]);
            let alpha = (col[3] as f32 * cov).clamp(0.0, 255.0) as u8;
            if alpha == 0 {
                continue;
            }
            let dst = img.get_pixel_mut(px as u32, py as u32);
            blend_over(dst, col, alpha);
        }
    }
}

/// Anti-aliased coverage of point `(fx, fy)` against the rounded
/// rectangle defined by top-left `(rx, ry)`, size `(rw, rh)` and
/// uniform corner radius `r`. Returns 0..=1.
fn rounded_rect_coverage(fx: f32, fy: f32, rx: f32, ry: f32, rw: f32, rh: f32, r: f32) -> f32 {
    if fx < rx || fy < ry || fx > rx + rw || fy > ry + rh {
        return 0.0;
    }
    if r <= 0.5 {
        return 1.0;
    }
    // Distance to the nearest "corner-arc center". When the point is
    // outside any corner's bounding circle we use SDF-style
    // anti-aliased coverage (1 px transition).
    let cx = if fx < rx + r {
        rx + r
    } else if fx > rx + rw - r {
        rx + rw - r
    } else {
        fx
    };
    let cy = if fy < ry + r {
        ry + r
    } else if fy > ry + rh - r {
        ry + rh - r
    } else {
        fy
    };
    let dx = fx - cx;
    let dy = fy - cy;
    let dist = (dx * dx + dy * dy).sqrt();
    let edge = r;
    if dist <= edge - 0.5 {
        1.0
    } else if dist >= edge + 0.5 {
        0.0
    } else {
        edge + 0.5 - dist
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t).clamp(0.0, 255.0) as u8
}
