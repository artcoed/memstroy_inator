//! Lightweight CPU image-effects pipeline used for the live canvas
//! preview of overlays / actors.
//!
//! The FFmpeg export path (in `memstroy-render/src/filtergraph.rs`)
//! handles the same effect stack but only fires at render time. For
//! interactive preview we run the stack on the CPU once per
//! (image-source, effects-signature) pair, cache the resulting
//! `egui::TextureHandle`, and reuse it across frames. The pipeline
//! intentionally covers the colour / pixel transforms that are cheap
//! enough to run synchronously on a UI thread:
//!
//!   - Blur (box-blur with adjustable radius)
//!   - Sharpen (3×3 unsharp-mask kernel)
//!   - Grayscale / Sepia / Invert / HueShift / Saturation
//!   - Brightness / Contrast
//!   - Pixelate / Posterize
//!   - Vignette / Glow / Bloom (cheap approximation)
//!   - Mirror H / V
//!   - Noise / OldFilm / VHS / Glitch (preset blends)
//!   - EdgeDetect (Sobel)
//!   - ChromaticAberration / Wave (small geometric jiggle)
//!   - Crop (returned as UV inset so the renderer adjusts the mesh)
//!
//! Each entry's `enabled` flag and `intensity` are honoured exactly
//! like the FFmpeg path. Effects whose intensity is below ~0.001 are
//! skipped so the preview is always equivalent to "no entry" when
//! the user temporarily fades an effect down.
//!
//! Performance: small overlay PNGs (≤ 1024×1024) finish the full stack
//! in low single-digit milliseconds on a typical laptop. Larger
//! images get a single decode pass, so the cost is paid once and the
//! result is cached until the effect parameters change.

use memstroy_core::effects::{Effect, EffectKind};

/// Apply the effect stack in `effects` to an RGBA8 buffer in place.
/// `(w, h)` are the image dimensions in pixels.
///
/// Returns the resulting Crop inset (`left, top, right, bottom`) in
/// normalised 0..1 — the mesh renderer uses this to clip the visible
/// rectangle / shrink the UVs. When no Crop is in the stack the
/// returned tuple is `(0.0, 0.0, 0.0, 0.0)`.
pub fn apply_effect_stack(
    rgba: &mut Vec<u8>,
    w: u32,
    h: u32,
    effects: &[Effect],
    _t: f32,
) -> (f32, f32, f32, f32) {
    let mut crop = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
    for eff in effects {
        if !eff.enabled { continue; }
        let i = eff.intensity.clamp(0.0, 1.0);
        if i <= 0.001 { continue; }
        match &eff.kind {
            EffectKind::Blur { radius } => {
                let r = ((radius * i) as i32).max(1);
                box_blur_rgba(rgba, w, h, r);
            }
            EffectKind::Sharpen { amount } => {
                sharpen(rgba, w, h, (amount * i).clamp(0.0, 3.0));
            }
            EffectKind::Grayscale => {
                blend_each_pixel(rgba, i, |r, g, b| {
                    let y = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
                    [y, y, y]
                });
            }
            EffectKind::Sepia => {
                blend_each_pixel(rgba, i, |r, g, b| {
                    let rf = r as f32; let gf = g as f32; let bf = b as f32;
                    let nr = (0.393*rf + 0.769*gf + 0.189*bf).clamp(0.0, 255.0) as u8;
                    let ng = (0.349*rf + 0.686*gf + 0.168*bf).clamp(0.0, 255.0) as u8;
                    let nb = (0.272*rf + 0.534*gf + 0.131*bf).clamp(0.0, 255.0) as u8;
                    [nr, ng, nb]
                });
            }
            EffectKind::Invert => {
                blend_each_pixel(rgba, i, |r, g, b| [255 - r, 255 - g, 255 - b]);
            }
            EffectKind::HueShift { degrees } => {
                hue_shift(rgba, degrees * i);
            }
            EffectKind::Vignette { strength } => {
                vignette(rgba, w, h, (strength * i).clamp(0.0, 1.0));
            }
            EffectKind::Pixelate { block_size } => {
                let block = ((block_size * i) as u32).max(1);
                pixelate(rgba, w, h, block);
            }
            EffectKind::Posterize { levels } => {
                let lv = (*levels).clamp(2, 32);
                blend_each_pixel(rgba, i, |r, g, b| {
                    let q = |c: u8| -> u8 {
                        let n = lv as f32;
                        let v = (c as f32 / 255.0 * (n - 1.0)).round() / (n - 1.0);
                        (v * 255.0).clamp(0.0, 255.0) as u8
                    };
                    [q(r), q(g), q(b)]
                });
            }
            EffectKind::Glow { radius, intensity } => {
                glow_or_bloom(rgba, w, h, ((radius * i) as i32).max(1), (intensity * i).clamp(0.0, 2.0));
            }
            EffectKind::Bloom { radius } => {
                glow_or_bloom(rgba, w, h, ((radius * i) as i32).max(1), 0.6 * i);
            }
            EffectKind::Brightness { amount } => {
                let factor = 1.0 + (amount * i);
                blend_each_pixel(rgba, i, |r, g, b| {
                    let s = |c: u8| -> u8 {
                        ((c as f32 * factor).clamp(0.0, 255.0)) as u8
                    };
                    [s(r), s(g), s(b)]
                });
            }
            EffectKind::Contrast { amount } => {
                let f = 1.0 + amount * i;
                blend_each_pixel(rgba, i, |r, g, b| {
                    let s = |c: u8| -> u8 {
                        ((((c as f32 - 128.0) * f) + 128.0).clamp(0.0, 255.0)) as u8
                    };
                    [s(r), s(g), s(b)]
                });
            }
            EffectKind::Saturation { amount } => {
                let factor = 1.0 + amount * i;
                blend_each_pixel(rgba, i, |r, g, b| {
                    let y = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
                    let nr = (y + (r as f32 - y) * factor).clamp(0.0, 255.0) as u8;
                    let ng = (y + (g as f32 - y) * factor).clamp(0.0, 255.0) as u8;
                    let nb = (y + (b as f32 - y) * factor).clamp(0.0, 255.0) as u8;
                    [nr, ng, nb]
                });
            }
            EffectKind::EdgeDetect { threshold } => {
                edge_detect(rgba, w, h, (threshold * i).clamp(0.0, 1.0));
            }
            EffectKind::MirrorH => mirror_horizontal(rgba, w, h),
            EffectKind::MirrorV => mirror_vertical(rgba, w, h),
            EffectKind::ChromaticAberration { offset } => {
                chromatic_aberration(rgba, w, h, (offset * i) as i32);
            }
            EffectKind::Noise { amount } => {
                noise(rgba, w, h, (amount * i).clamp(0.0, 1.0));
            }
            EffectKind::Wave { amplitude, wavelength } => {
                wave(rgba, w, h, amplitude * i, wavelength.max(1.0));
            }
            EffectKind::OldFilm => {
                // Sepia + grain + gentle vignette as a single preset.
                blend_each_pixel(rgba, i, |r, g, b| {
                    let rf = r as f32; let gf = g as f32; let bf = b as f32;
                    let nr = (0.393*rf + 0.769*gf + 0.189*bf).clamp(0.0, 255.0) as u8;
                    let ng = (0.349*rf + 0.686*gf + 0.168*bf).clamp(0.0, 255.0) as u8;
                    let nb = (0.272*rf + 0.534*gf + 0.131*bf).clamp(0.0, 255.0) as u8;
                    [nr, ng, nb]
                });
                noise(rgba, w, h, 0.15 * i);
                vignette(rgba, w, h, 0.4 * i);
            }
            EffectKind::Vhs => {
                chromatic_aberration(rgba, w, h, (4.0 * i) as i32);
                scanlines(rgba, w, h, 0.4 * i);
            }
            EffectKind::Glitch { strength } => {
                glitch(rgba, w, h, (strength * i).clamp(0.0, 1.0));
            }
            EffectKind::Crop { left, top, right, bottom } => {
                let l = (left * i).clamp(0.0, 0.49);
                let t = (top * i).clamp(0.0, 0.49);
                let r = (right * i).clamp(0.0, 0.49);
                let b = (bottom * i).clamp(0.0, 0.49);
                crop.0 = (crop.0 + l).min(0.49);
                crop.1 = (crop.1 + t).min(0.49);
                crop.2 = (crop.2 + r).min(0.49);
                crop.3 = (crop.3 + b).min(0.49);
            }
        }
    }
    crop
}

/// Hash signature for an effect stack at a given time. Used as the
/// cache key for the per-overlay processed-texture cache so we only
/// re-run the pipeline when the parameters change.
pub fn signature(effects: &[Effect]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for eff in effects {
        if !eff.enabled { continue; }
        let i = (eff.intensity * 1000.0) as i32;
        i.hash(&mut h);
        match &eff.kind {
            EffectKind::Blur { radius } => { 1u8.hash(&mut h); ((radius * 100.0) as i32).hash(&mut h); }
            EffectKind::Sharpen { amount } => { 2u8.hash(&mut h); ((amount * 100.0) as i32).hash(&mut h); }
            EffectKind::Grayscale => { 3u8.hash(&mut h); }
            EffectKind::Sepia => { 4u8.hash(&mut h); }
            EffectKind::Invert => { 5u8.hash(&mut h); }
            EffectKind::HueShift { degrees } => { 6u8.hash(&mut h); ((degrees * 10.0) as i32).hash(&mut h); }
            EffectKind::Vignette { strength } => { 7u8.hash(&mut h); ((strength * 100.0) as i32).hash(&mut h); }
            EffectKind::Pixelate { block_size } => { 8u8.hash(&mut h); ((block_size * 10.0) as i32).hash(&mut h); }
            EffectKind::Posterize { levels } => { 9u8.hash(&mut h); levels.hash(&mut h); }
            EffectKind::Glow { radius, intensity } => { 10u8.hash(&mut h); ((radius * 10.0) as i32).hash(&mut h); ((intensity * 100.0) as i32).hash(&mut h); }
            EffectKind::Brightness { amount } => { 11u8.hash(&mut h); ((amount * 100.0) as i32).hash(&mut h); }
            EffectKind::Contrast { amount } => { 12u8.hash(&mut h); ((amount * 100.0) as i32).hash(&mut h); }
            EffectKind::Saturation { amount } => { 13u8.hash(&mut h); ((amount * 100.0) as i32).hash(&mut h); }
            EffectKind::EdgeDetect { threshold } => { 14u8.hash(&mut h); ((threshold * 100.0) as i32).hash(&mut h); }
            EffectKind::MirrorH => { 15u8.hash(&mut h); }
            EffectKind::MirrorV => { 16u8.hash(&mut h); }
            EffectKind::ChromaticAberration { offset } => { 17u8.hash(&mut h); ((offset * 10.0) as i32).hash(&mut h); }
            EffectKind::Noise { amount } => { 18u8.hash(&mut h); ((amount * 100.0) as i32).hash(&mut h); }
            EffectKind::Wave { amplitude, wavelength } => { 19u8.hash(&mut h); ((amplitude * 10.0) as i32).hash(&mut h); ((wavelength * 10.0) as i32).hash(&mut h); }
            EffectKind::OldFilm => { 20u8.hash(&mut h); }
            EffectKind::Vhs => { 21u8.hash(&mut h); }
            EffectKind::Glitch { strength } => { 22u8.hash(&mut h); ((strength * 100.0) as i32).hash(&mut h); }
            EffectKind::Bloom { radius } => { 23u8.hash(&mut h); ((radius * 10.0) as i32).hash(&mut h); }
            EffectKind::Crop { left, top, right, bottom } => {
                24u8.hash(&mut h);
                ((left * 1000.0) as i32).hash(&mut h);
                ((top * 1000.0) as i32).hash(&mut h);
                ((right * 1000.0) as i32).hash(&mut h);
                ((bottom * 1000.0) as i32).hash(&mut h);
            }
        }
    }
    h.finish()
}

// ─── PIXEL OPS ──────────────────────────────────────────────────────

/// Apply `f(r,g,b) -> [r,g,b]` to every opaque pixel and blend the
/// result back with `intensity`. Alpha is left untouched. Pixels that
/// are fully transparent are skipped — they typically carry uninit
/// colour data that would corrupt the output.
fn blend_each_pixel<F: Fn(u8, u8, u8) -> [u8; 3]>(
    rgba: &mut Vec<u8>,
    intensity: f32,
    f: F,
) {
    let i = intensity.clamp(0.0, 1.0);
    let inv = 1.0 - i;
    for px in rgba.chunks_exact_mut(4) {
        if px[3] == 0 { continue; }
        let [r, g, b] = f(px[0], px[1], px[2]);
        px[0] = ((r as f32 * i) + (px[0] as f32 * inv)) as u8;
        px[1] = ((g as f32 * i) + (px[1] as f32 * inv)) as u8;
        px[2] = ((b as f32 * i) + (px[2] as f32 * inv)) as u8;
    }
}

/// In-place separable box blur with radius `r` (pixels). Skipped when
/// `r <= 0`. Alpha is blurred along with the colour so the drop-shadow
/// edges of the picture stay smooth.
fn box_blur_rgba(rgba: &mut Vec<u8>, w: u32, h: u32, r: i32) {
    if r <= 0 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let mut tmp = vec![0u8; rgba.len()];
    // Horizontal pass.
    for y in 0..h_i {
        let row_start = (y * w_i * 4) as usize;
        for x in 0..w_i {
            let mut sr = 0u32;
            let mut sg = 0u32;
            let mut sb = 0u32;
            let mut sa = 0u32;
            let mut count = 0u32;
            let xa = (x - r).max(0);
            let xb = (x + r).min(w_i - 1);
            for xi in xa..=xb {
                let idx = row_start + (xi as usize) * 4;
                sr += rgba[idx] as u32;
                sg += rgba[idx + 1] as u32;
                sb += rgba[idx + 2] as u32;
                sa += rgba[idx + 3] as u32;
                count += 1;
            }
            let didx = row_start + (x as usize) * 4;
            tmp[didx] = (sr / count) as u8;
            tmp[didx + 1] = (sg / count) as u8;
            tmp[didx + 2] = (sb / count) as u8;
            tmp[didx + 3] = (sa / count) as u8;
        }
    }
    // Vertical pass.
    for x in 0..w_i {
        for y in 0..h_i {
            let mut sr = 0u32;
            let mut sg = 0u32;
            let mut sb = 0u32;
            let mut sa = 0u32;
            let mut count = 0u32;
            let ya = (y - r).max(0);
            let yb = (y + r).min(h_i - 1);
            for yi in ya..=yb {
                let idx = ((yi * w_i + x) as usize) * 4;
                sr += tmp[idx] as u32;
                sg += tmp[idx + 1] as u32;
                sb += tmp[idx + 2] as u32;
                sa += tmp[idx + 3] as u32;
                count += 1;
            }
            let didx = ((y * w_i + x) as usize) * 4;
            rgba[didx] = (sr / count) as u8;
            rgba[didx + 1] = (sg / count) as u8;
            rgba[didx + 2] = (sb / count) as u8;
            rgba[didx + 3] = (sa / count) as u8;
        }
    }
}

/// 3×3 unsharp-mask sharpen. `amount` is a master scale 0..3.
fn sharpen(rgba: &mut Vec<u8>, w: u32, h: u32, amount: f32) {
    if amount <= 0.001 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let src = rgba.clone();
    let kern = [
        0.0, -amount, 0.0,
        -amount, 1.0 + 4.0 * amount, -amount,
        0.0, -amount, 0.0,
    ];
    for y in 0..h_i {
        for x in 0..w_i {
            let mut sr = 0.0_f32;
            let mut sg = 0.0_f32;
            let mut sb = 0.0_f32;
            for ky in -1i32..=1 {
                for kx in -1i32..=1 {
                    let nx = (x + kx).clamp(0, w_i - 1);
                    let ny = (y + ky).clamp(0, h_i - 1);
                    let idx = ((ny * w_i + nx) as usize) * 4;
                    let k = kern[((ky + 1) * 3 + (kx + 1)) as usize];
                    sr += src[idx] as f32 * k;
                    sg += src[idx + 1] as f32 * k;
                    sb += src[idx + 2] as f32 * k;
                }
            }
            let didx = ((y * w_i + x) as usize) * 4;
            rgba[didx] = sr.clamp(0.0, 255.0) as u8;
            rgba[didx + 1] = sg.clamp(0.0, 255.0) as u8;
            rgba[didx + 2] = sb.clamp(0.0, 255.0) as u8;
        }
    }
}

fn hue_shift(rgba: &mut Vec<u8>, degrees: f32) {
    let rad = degrees.to_radians();
    let cos_a = rad.cos();
    let sin_a = rad.sin();
    // Rotation matrix in YIQ-ish space (close enough for preview).
    let mr = [
        0.213 + 0.787*cos_a - 0.213*sin_a,
        0.715 - 0.715*cos_a - 0.715*sin_a,
        0.072 - 0.072*cos_a + 0.928*sin_a,
    ];
    let mg = [
        0.213 - 0.213*cos_a + 0.143*sin_a,
        0.715 + 0.285*cos_a + 0.140*sin_a,
        0.072 - 0.072*cos_a - 0.283*sin_a,
    ];
    let mb = [
        0.213 - 0.213*cos_a - 0.787*sin_a,
        0.715 - 0.715*cos_a + 0.715*sin_a,
        0.072 + 0.928*cos_a + 0.072*sin_a,
    ];
    for px in rgba.chunks_exact_mut(4) {
        if px[3] == 0 { continue; }
        let r = px[0] as f32; let g = px[1] as f32; let b = px[2] as f32;
        let nr = (r*mr[0] + g*mr[1] + b*mr[2]).clamp(0.0, 255.0) as u8;
        let ng = (r*mg[0] + g*mg[1] + b*mg[2]).clamp(0.0, 255.0) as u8;
        let nb = (r*mb[0] + g*mb[1] + b*mb[2]).clamp(0.0, 255.0) as u8;
        px[0] = nr; px[1] = ng; px[2] = nb;
    }
}

fn vignette(rgba: &mut Vec<u8>, w: u32, h: u32, strength: f32) {
    let cx = w as f32 * 0.5;
    let cy = h as f32 * 0.5;
    let max_d = (cx * cx + cy * cy).sqrt();
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx*dx + dy*dy).sqrt() / max_d;
            // Tail off near the centre, dim toward the edges.
            let dim = (1.0 - strength * d.powi(2)).clamp(0.0, 1.0);
            let idx = ((y * w + x) as usize) * 4;
            rgba[idx] = (rgba[idx] as f32 * dim) as u8;
            rgba[idx + 1] = (rgba[idx + 1] as f32 * dim) as u8;
            rgba[idx + 2] = (rgba[idx + 2] as f32 * dim) as u8;
        }
    }
}

fn pixelate(rgba: &mut Vec<u8>, w: u32, h: u32, block: u32) {
    if block <= 1 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let block_i = block as i32;
    for by in (0..h_i).step_by(block as usize) {
        for bx in (0..w_i).step_by(block as usize) {
            // Sample the top-left pixel of the block and broadcast it.
            let src_idx = ((by * w_i + bx) as usize) * 4;
            let r = rgba[src_idx];
            let g = rgba[src_idx + 1];
            let b = rgba[src_idx + 2];
            let a = rgba[src_idx + 3];
            for y in by..(by + block_i).min(h_i) {
                for x in bx..(bx + block_i).min(w_i) {
                    let idx = ((y * w_i + x) as usize) * 4;
                    rgba[idx] = r;
                    rgba[idx + 1] = g;
                    rgba[idx + 2] = b;
                    rgba[idx + 3] = a;
                }
            }
        }
    }
}

fn glow_or_bloom(rgba: &mut Vec<u8>, w: u32, h: u32, radius: i32, intensity: f32) {
    if radius <= 0 || intensity <= 0.001 { return; }
    // Bright-pass extract.
    let mut bright = rgba.clone();
    for px in bright.chunks_exact_mut(4) {
        let lum = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
        if lum < 180.0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        }
    }
    box_blur_rgba(&mut bright, w, h, radius);
    // Additive blend back into rgba.
    for (dst, src) in rgba.chunks_exact_mut(4).zip(bright.chunks_exact(4)) {
        dst[0] = ((dst[0] as f32 + src[0] as f32 * intensity).clamp(0.0, 255.0)) as u8;
        dst[1] = ((dst[1] as f32 + src[1] as f32 * intensity).clamp(0.0, 255.0)) as u8;
        dst[2] = ((dst[2] as f32 + src[2] as f32 * intensity).clamp(0.0, 255.0)) as u8;
    }
}

fn edge_detect(rgba: &mut Vec<u8>, w: u32, h: u32, threshold: f32) {
    let w_i = w as i32;
    let h_i = h as i32;
    let src = rgba.clone();
    let lum = |idx: usize| -> f32 {
        0.299 * src[idx] as f32 + 0.587 * src[idx + 1] as f32 + 0.114 * src[idx + 2] as f32
    };
    let thr = threshold * 255.0;
    for y in 1..h_i - 1 {
        for x in 1..w_i - 1 {
            let i00 = ((y * w_i + x) as usize) * 4;
            let i_l = (((y * w_i) + (x - 1)) as usize) * 4;
            let i_r = (((y * w_i) + (x + 1)) as usize) * 4;
            let i_t = ((((y - 1) * w_i) + x) as usize) * 4;
            let i_b = ((((y + 1) * w_i) + x) as usize) * 4;
            let gx = lum(i_r) - lum(i_l);
            let gy = lum(i_b) - lum(i_t);
            let g = (gx * gx + gy * gy).sqrt();
            let v = if g > thr { 255 } else { 0 };
            rgba[i00] = v;
            rgba[i00 + 1] = v;
            rgba[i00 + 2] = v;
        }
    }
}

fn mirror_horizontal(rgba: &mut Vec<u8>, w: u32, h: u32) {
    let w_i = w as usize;
    let h_i = h as usize;
    for y in 0..h_i {
        for x in 0..(w_i / 2) {
            let li = (y * w_i + x) * 4;
            let ri = (y * w_i + (w_i - 1 - x)) * 4;
            for c in 0..4 {
                rgba.swap(li + c, ri + c);
            }
        }
    }
}

fn mirror_vertical(rgba: &mut Vec<u8>, w: u32, h: u32) {
    let w_i = w as usize;
    let h_i = h as usize;
    for y in 0..(h_i / 2) {
        for x in 0..w_i {
            let ti = (y * w_i + x) * 4;
            let bi = ((h_i - 1 - y) * w_i + x) * 4;
            for c in 0..4 {
                rgba.swap(ti + c, bi + c);
            }
        }
    }
}

fn chromatic_aberration(rgba: &mut Vec<u8>, w: u32, h: u32, offset: i32) {
    if offset == 0 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let src = rgba.clone();
    for y in 0..h_i {
        for x in 0..w_i {
            let dst_idx = ((y * w_i + x) as usize) * 4;
            // R channel sampled offset px to the right, B offset px to the left.
            let rx = (x + offset).clamp(0, w_i - 1);
            let bx = (x - offset).clamp(0, w_i - 1);
            let r_idx = ((y * w_i + rx) as usize) * 4;
            let b_idx = ((y * w_i + bx) as usize) * 4;
            rgba[dst_idx] = src[r_idx];
            rgba[dst_idx + 2] = src[b_idx + 2];
        }
    }
}

fn noise(rgba: &mut Vec<u8>, w: u32, h: u32, amount: f32) {
    if amount <= 0.001 { return; }
    // Simple LCG for determinism without pulling in a rand crate.
    let mut seed: u32 = (w.wrapping_mul(73856093) ^ h.wrapping_mul(19349663)).wrapping_add(1);
    let amp = amount * 60.0;
    for px in rgba.chunks_exact_mut(4) {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let n = ((seed >> 16) as f32 / 32768.0 - 1.0) * amp;
        px[0] = (px[0] as f32 + n).clamp(0.0, 255.0) as u8;
        px[1] = (px[1] as f32 + n).clamp(0.0, 255.0) as u8;
        px[2] = (px[2] as f32 + n).clamp(0.0, 255.0) as u8;
    }
}

fn wave(rgba: &mut Vec<u8>, w: u32, h: u32, amplitude: f32, wavelength: f32) {
    if amplitude.abs() < 0.5 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let src = rgba.clone();
    for y in 0..h_i {
        let dx = (((y as f32) / wavelength) * std::f32::consts::TAU).sin() * amplitude;
        let shift = dx.round() as i32;
        for x in 0..w_i {
            let sx = (x - shift).clamp(0, w_i - 1);
            let dst = ((y * w_i + x) as usize) * 4;
            let s = ((y * w_i + sx) as usize) * 4;
            rgba[dst..dst + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
}

fn scanlines(rgba: &mut Vec<u8>, w: u32, h: u32, strength: f32) {
    let w_i = w as usize;
    for y in 0..h as usize {
        if y % 2 == 0 { continue; }
        for x in 0..w_i {
            let idx = (y * w_i + x) * 4;
            let dim = 1.0 - strength;
            rgba[idx] = (rgba[idx] as f32 * dim) as u8;
            rgba[idx + 1] = (rgba[idx + 1] as f32 * dim) as u8;
            rgba[idx + 2] = (rgba[idx + 2] as f32 * dim) as u8;
        }
    }
}

fn glitch(rgba: &mut Vec<u8>, w: u32, h: u32, strength: f32) {
    if strength <= 0.001 { return; }
    let w_i = w as i32;
    let h_i = h as i32;
    let src = rgba.clone();
    let mut seed: u32 = (w.wrapping_mul(2654435761) ^ h.wrapping_mul(40503)).wrapping_add(7);
    let band = (strength * 30.0).max(2.0) as i32;
    let max_off = (strength * w as f32 * 0.15).max(2.0) as i32;
    let mut y = 0i32;
    while y < h_i {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let off = ((seed >> 16) as i32 % (max_off * 2)) - max_off;
        let band_h = (band + (seed >> 8) as i32 % band).max(1);
        for yy in y..(y + band_h).min(h_i) {
            for x in 0..w_i {
                let sx = (x + off).clamp(0, w_i - 1);
                let dst = ((yy * w_i + x) as usize) * 4;
                let s = ((yy * w_i + sx) as usize) * 4;
                rgba[dst..dst + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
        y += band_h;
    }
}
