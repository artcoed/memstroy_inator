//! Background removal (rembg-style) via the **U²-Netp** salient-object
//! ONNX model.
//!
//! The pipeline mirrors the upstream `u2net_test.py` recipe used by the
//! popular `rembg` Python tool:
//!
//!   1. Decode the source image and resize to 320×320 RGB.
//!   2. Normalize to NCHW with ImageNet mean / std and run the ONNX
//!      session — the first output (`d0`, the deepest fused saliency
//!      map) is the only one we need.
//!   3. Min-max normalize the predicted map to `[0, 1]`.
//!   4. Resize the mask back to the original resolution with bilinear
//!      sampling.
//!   5. Compose an `RgbaImage` whose alpha is the mask (multiplied by
//!      255), so background pixels become transparent and the subject
//!      is kept as-is. An optional binary threshold is available for a
//!      hard-cutout look.
//!
//! Typical usage:
//!
//! ```no_run
//! use std::path::PathBuf;
//! use memstroy_vision::bgremove::{BackgroundRemover, U2NetpBgRemover};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let remover = U2NetpBgRemover::new(PathBuf::from("assets/models/u2netp.onnx"));
//! let rgba = remover.remove(std::path::Path::new("photo.jpg")).await?;
//! rgba.save("photo.cutout.png")?;
//! # Ok(())
//! # }
//! ```
//!
//! Inference runs inside [`tokio::task::spawn_blocking`] so the async
//! caller does not stall the runtime. The model is ~4.7 MB and takes
//! roughly 0.2-0.5 s per image on a modern CPU.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use image::{Rgba, RgbaImage};
use tracing::info;

/// Square edge length of the U²-Net input tensor.
const INPUT_SIZE: usize = 320;
/// ImageNet RGB mean used by the upstream U²-Net `ToTensorLab` transform.
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
/// ImageNet RGB standard deviation.
const STD: [f32; 3] = [0.229, 0.224, 0.225];

// ─── TRAIT ───────────────────────────────────────────────────────────

/// Async-friendly background-removal abstraction. Lets callers swap a
/// no-op stub during tests for a real ONNX implementation in
/// production, paralleling [`crate::pose::PoseEstimator`].
#[async_trait]
pub trait BackgroundRemover: Send + Sync {
    /// Decode `input`, predict the foreground mask, and return an RGBA
    /// image where background pixels have alpha = 0.
    async fn remove(&self, input: &Path) -> Result<RgbaImage>;

    /// Short identifier for logs / status messages.
    fn id(&self) -> &'static str;
}

/// Pass-through stub used in tests / when the ONNX model is missing.
/// Decodes the input image and returns it as opaque RGBA — no actual
/// matting is performed.
pub struct StubBackgroundRemover;

#[async_trait]
impl BackgroundRemover for StubBackgroundRemover {
    async fn remove(&self, input: &Path) -> Result<RgbaImage> {
        let img = image::open(input)
            .with_context(|| format!("open {}", input.display()))?
            .to_rgba8();
        Ok(img)
    }
    fn id(&self) -> &'static str {
        "stub"
    }
}

// ─── ONNX IMPLEMENTATION ────────────────────────────────────────────

/// ONNX-backed background remover using the U²-Netp salient-object
/// model. The same struct works with both the official rembg-exported
/// model and the upstream PyTorch export, since both emit `d0` at
/// output index 0 with shape `[1, 1, 320, 320]`.
pub struct U2NetpBgRemover {
    model_path: PathBuf,
    /// Saliency cutoff in `[0, 1]`. When `apply_threshold` is true,
    /// pixels with predicted alpha below this value are forced fully
    /// transparent and the rest fully opaque (binary cutout).
    alpha_threshold: f32,
    apply_threshold: bool,
}

impl U2NetpBgRemover {
    /// Build a remover that will load `model_path` on first call.
    /// Existence is *not* checked here so the constructor stays cheap.
    pub fn new(model_path: PathBuf) -> Self {
        Self {
            model_path,
            alpha_threshold: 0.0,
            apply_threshold: false,
        }
    }

    /// Switch to a hard binary cutout above the given saliency value.
    /// Pass `0.0` (or omit the call) to keep the soft alpha matte
    /// produced directly by the model.
    pub fn with_threshold(mut self, t: f32) -> Self {
        self.alpha_threshold = t.clamp(0.0, 1.0);
        self.apply_threshold = t > 0.0;
        self
    }
}

#[async_trait]
impl BackgroundRemover for U2NetpBgRemover {
    async fn remove(&self, input: &Path) -> Result<RgbaImage> {
        if !self.model_path.exists() {
            return Err(anyhow!(
                "Model not found: {}. Download u2netp.onnx into assets/models/",
                self.model_path.display()
            ));
        }

        // Decode synchronously — `image` is fast enough for a single
        // file and keeps the blocking section minimal.
        let img = image::open(input)
            .with_context(|| format!("open {}", input.display()))?
            .to_rgb8();
        let (w, h) = img.dimensions();
        info!(w, h, "running u2netp background removal");

        let model_path = self.model_path.clone();
        let raw = img.into_raw();
        let alpha_threshold = self.alpha_threshold;
        let apply_threshold = self.apply_threshold;

        let rgba = tokio::task::spawn_blocking(move || -> Result<RgbaImage> {
            run_u2netp(&model_path, &raw, w, h, alpha_threshold, apply_threshold)
        })
        .await??;

        info!(
            w = rgba.width(),
            h = rgba.height(),
            "background removal complete"
        );
        Ok(rgba)
    }

    fn id(&self) -> &'static str {
        "u2netp"
    }
}

/// Convenience: run `remover` on `input`, encode the resulting RGBA
/// image to PNG (or whatever the extension implies), and write it to
/// `output`.
pub async fn remove_background_to_file(
    remover: &dyn BackgroundRemover,
    input: &Path,
    output: &Path,
) -> Result<()> {
    let rgba = remover.remove(input).await?;
    rgba.save(output)
        .with_context(|| format!("save {}", output.display()))?;
    Ok(())
}

// ─── INFERENCE CORE ─────────────────────────────────────────────────

/// Blocking ONNX inference + alpha compositing. `rgb` is the raw
/// `width * height * 3` buffer of the source image.
fn run_u2netp(
    model_path: &Path,
    rgb: &[u8],
    w: u32,
    h: u32,
    alpha_threshold: f32,
    apply_threshold: bool,
) -> Result<RgbaImage> {
    use ort::session::builder::GraphOptimizationLevel;

    // Same session-builder dance as `pose.rs` — `with_optimization_level`
    // returns a `Result` whose error type carries the partially-built
    // builder via `.recover()` so we can never lose it.
    let builder =
        ort::session::Session::builder().map_err(|e| anyhow!("ort session builder: {}", e))?;
    let mut builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .unwrap_or_else(|e| e.recover());
    let mut session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("ort load model: {}", e))?;

    // 1. Resize source RGB to the model input.
    let resized = resize_rgb_bilinear(rgb, w as usize, h as usize, INPUT_SIZE, INPUT_SIZE);

    // 2. NCHW float32 with ImageNet normalization.
    let plane = INPUT_SIZE * INPUT_SIZE;
    let mut input_data = vec![0.0f32; 3 * plane];
    for y in 0..INPUT_SIZE {
        for x in 0..INPUT_SIZE {
            let src = (y * INPUT_SIZE + x) * 3;
            let pix = y * INPUT_SIZE + x;
            input_data[pix] = (resized[src] as f32 / 255.0 - MEAN[0]) / STD[0];
            input_data[plane + pix] = (resized[src + 1] as f32 / 255.0 - MEAN[1]) / STD[1];
            input_data[2 * plane + pix] = (resized[src + 2] as f32 / 255.0 - MEAN[2]) / STD[2];
        }
    }

    // 3. Run inference.
    let input_tensor =
        ort::value::Tensor::from_array(([1usize, 3, INPUT_SIZE, INPUT_SIZE], input_data))
            .map_err(|e| anyhow!("create tensor: {}", e))?;

    let outputs = session
        .run(ort::inputs![input_tensor])
        .map_err(|e| anyhow!("inference: {}", e))?;

    // 4. Pull the first (deepest, most refined) saliency map. Both the
    //    rembg export and the original repo place `d0` at index 0
    //    with shape `[1, 1, 320, 320]`, but we tolerate any shape that
    //    ends in (..., 320, 320) and just take the first 320×320 plane.
    let output_value = &outputs[0];
    let (shape, data) = output_value
        .try_extract_tensor::<f32>()
        .map_err(|e| anyhow!("extract tensor: {}", e))?;
    let total: usize = shape.iter().map(|d| *d as usize).product();
    if total < plane {
        return Err(anyhow!(
            "u2netp output has {} elements, expected at least {} (shape {:?})",
            total,
            plane,
            shape
        ));
    }
    let mask_raw: Vec<f32> = data.iter().take(plane).copied().collect();

    // 5. Min-max normalize per the U²-Net paper post-processing recipe.
    let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
    for &v in &mask_raw {
        if v < mn {
            mn = v;
        }
        if v > mx {
            mx = v;
        }
    }
    let range = (mx - mn).max(1e-6);
    let mask_norm: Vec<f32> = mask_raw
        .iter()
        .map(|v| ((*v - mn) / range).clamp(0.0, 1.0))
        .collect();

    // 6. Resize mask back to the source resolution.
    let mask_full =
        resize_mask_bilinear(&mask_norm, INPUT_SIZE, INPUT_SIZE, w as usize, h as usize);

    // 7. Compose RGBA. We write into the raw buffer and build the
    //    `RgbaImage` once at the end — much faster than `put_pixel`.
    let pixels = (w as usize) * (h as usize);
    let mut out_buf = Vec::<u8>::with_capacity(pixels * 4);
    for i in 0..pixels {
        let src = i * 3;
        let mut a = mask_full[i];
        if apply_threshold {
            a = if a >= alpha_threshold { 1.0 } else { 0.0 };
        }
        let alpha = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        out_buf.extend_from_slice(&[rgb[src], rgb[src + 1], rgb[src + 2], alpha]);
    }
    let out = RgbaImage::from_raw(w, h, out_buf)
        .ok_or_else(|| anyhow!("failed to build RgbaImage from raw buffer"))?;

    // Suppress "unused" warning when neither path uses the variant.
    let _ = Rgba::<u8>([0, 0, 0, 0]);
    Ok(out)
}

// ─── RESIZE HELPERS ─────────────────────────────────────────────────

/// Bilinear RGB resize. Avoids pulling a heavier dep just for one call
/// and matches the style of `resize_rgb` in `pose.rs` (but with proper
/// bilinear instead of nearest-neighbour, since matting quality
/// suffers a lot from blocky upscales).
fn resize_rgb_bilinear(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dw * dh * 3];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return dst;
    }
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;
    let max_x = (sw - 1) as f32;
    let max_y = (sh - 1) as f32;
    for dy in 0..dh {
        let fy = ((dy as f32 + 0.5) * sy - 0.5).clamp(0.0, max_y);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for dx in 0..dw {
            let fx = ((dx as f32 + 0.5) * sx - 0.5).clamp(0.0, max_x);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            let di = (dy * dw + dx) * 3;
            for c in 0..3 {
                let p00 = src[(y0 * sw + x0) * 3 + c] as f32;
                let p10 = src[(y0 * sw + x1) * 3 + c] as f32;
                let p01 = src[(y1 * sw + x0) * 3 + c] as f32;
                let p11 = src[(y1 * sw + x1) * 3 + c] as f32;
                let v = p00 * (1.0 - wx) * (1.0 - wy)
                    + p10 * wx * (1.0 - wy)
                    + p01 * (1.0 - wx) * wy
                    + p11 * wx * wy;
                dst[di + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    dst
}

/// Bilinear single-channel resize for the predicted mask.
fn resize_mask_bilinear(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let mut dst = vec![0.0f32; dw * dh];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return dst;
    }
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;
    let max_x = (sw - 1) as f32;
    let max_y = (sh - 1) as f32;
    for dy in 0..dh {
        let fy = ((dy as f32 + 0.5) * sy - 0.5).clamp(0.0, max_y);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for dx in 0..dw {
            let fx = ((dx as f32 + 0.5) * sx - 0.5).clamp(0.0, max_x);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            let p00 = src[y0 * sw + x0];
            let p10 = src[y0 * sw + x1];
            let p01 = src[y1 * sw + x0];
            let p11 = src[y1 * sw + x1];
            dst[dy * dw + dx] = p00 * (1.0 - wx) * (1.0 - wy)
                + p10 * wx * (1.0 - wy)
                + p01 * (1.0 - wx) * wy
                + p11 * wx * wy;
        }
    }
    dst
}

// ─── TESTS (resize helpers only — model not exercised here) ─────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_resize_preserves_constant_image() {
        let src = vec![17u8; 4 * 4 * 3];
        let out = resize_rgb_bilinear(&src, 4, 4, 8, 8);
        for &b in &out {
            assert_eq!(b, 17);
        }
    }

    #[test]
    fn mask_resize_preserves_constant_field() {
        let src = vec![0.42f32; 8 * 8];
        let out = resize_mask_bilinear(&src, 8, 8, 16, 16);
        for v in out {
            assert!((v - 0.42).abs() < 1e-5);
        }
    }

    #[tokio::test]
    async fn stub_remover_returns_rgba() {
        let dir = std::env::temp_dir();
        let p = dir.join("memstroy_bgremove_stub_input.png");
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
        img.put_pixel(1, 0, Rgba([40, 50, 60, 255]));
        img.put_pixel(0, 1, Rgba([70, 80, 90, 255]));
        img.put_pixel(1, 1, Rgba([100, 110, 120, 255]));
        img.save(&p).unwrap();
        let stub = StubBackgroundRemover;
        let out = stub.remove(&p).await.unwrap();
        assert_eq!(out.dimensions(), (2, 2));
        let _ = std::fs::remove_file(&p);
    }
}
