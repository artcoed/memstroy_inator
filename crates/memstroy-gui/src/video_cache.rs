//! Frame-cache for real-time video preview.
//! Extracts frames via ffmpeg CLI at reduced quality for speed, then pre-loads
//! a ring buffer of frames into memory for smooth 60fps playback with a single
//! reusable texture handle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use egui::{ColorImage, TextureHandle, TextureOptions};
use tokio::runtime::Handle;

/// Number of frames to keep pre-loaded in the ring buffer.
const BUFFER_SIZE: usize = 30;

/// Frame-cache: extracts video frames to disk via ffmpeg, then pre-loads
/// frames into a memory ring buffer and uploads them to a single reusable
/// texture handle for smooth playback.
pub struct FrameCache {
    /// Which actor this cache belongs to.
    pub actor_index: usize,
    /// Temp directory holding extracted JPEG frames.
    pub cache_dir: PathBuf,
    /// Source video file path.
    pub source: PathBuf,
    /// Extraction fps (typically 30.0).
    pub fps: f32,
    /// Total number of frames extracted.
    pub frame_count: usize,
    /// Video duration in seconds.
    pub duration: f32,
    /// Whether extraction is complete and frames are available.
    pub ready: bool,
    /// Whether extraction is currently running.
    pub extracting: bool,
    /// Source frame dimensions (width, height) — read from first extracted frame.
    pub source_width: u32,
    pub source_height: u32,
    /// Single reusable texture handle — updated each frame.
    texture: Option<TextureHandle>,
    /// Frame index that was last uploaded to the `texture` handle. Used to
    /// skip GPU re-uploads when the playhead is on the same frame.
    texture_uploaded_frame: Option<usize>,
    /// Cached processed (chromakey/color-corrected) texture handle.
    /// Reused across frames when the source frame index AND the effect
    /// parameters are unchanged — avoids running the per-pixel CPU loop
    /// on every repaint.
    fx_texture: Option<TextureHandle>,
    /// Identity of the last processed frame (frame index + parameter hash).
    fx_key: Option<(usize, u64)>,
    /// Pre-loaded frame images in memory (ring buffer).
    buffer: Vec<Option<ColorImage>>,
    /// First frame index represented in the buffer.
    buffer_start: usize,
    /// How many slots in the buffer.
    buffer_size: usize,
    /// Last frame index that was displayed (to detect movement).
    last_displayed_frame: usize,
    /// Shared slot for background pre-load thread results.
    preload_slot: Arc<Mutex<Option<PreloadResult>>>,
    /// Whether a background pre-load is currently running.
    preloading: bool,
}

/// Result from the background pre-load thread.
struct PreloadResult {
    start: usize,
    frames: Vec<Option<ColorImage>>,
}

impl FrameCache {
    /// Create a new empty frame cache (not yet ready).
    pub fn new(source: PathBuf, actor_index: usize) -> Self {
        Self {
            actor_index,
            cache_dir: PathBuf::new(),
            source,
            fps: 30.0,
            frame_count: 0,
            duration: 0.0,
            ready: false,
            extracting: false,
            source_width: 480,
            source_height: 270,
            texture: None,
            texture_uploaded_frame: None,
            fx_texture: None,
            fx_key: None,
            buffer: Vec::new(),
            buffer_start: 0,
            buffer_size: BUFFER_SIZE,
            last_displayed_frame: usize::MAX,
            preload_slot: Arc::new(Mutex::new(None)),
            preloading: false,
        }
    }

    /// Start frame extraction in the background via tokio.
    ///
    /// Spawns ffprobe to get duration, then ffmpeg to extract frames at 30fps/480px
    /// with quality level 8 (fast, smaller files).
    /// Calls `on_done` with (duration, frame_count, cache_dir) when finished.
    pub fn start_extraction(
        source: PathBuf,
        rt: &Handle,
        on_done: impl FnOnce(f32, usize, PathBuf) + Send + 'static,
    ) {
        let ffmpeg = memstroy_render::ffmpeg_binary();
        // Derive ffprobe path from ffmpeg path (sibling binary)
        let ffprobe = {
            let mut p = ffmpeg.clone();
            p.set_file_name("ffprobe");
            if !p.exists() {
                // Fallback: just use "ffprobe" from PATH
                PathBuf::from("ffprobe")
            } else {
                p
            }
        };

        rt.spawn(async move {
            // Create temp directory for frames
            let cache_dir = std::env::temp_dir().join(format!(
                "memstroy_frames_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            ));
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                tracing::error!("Failed to create frame cache dir: {e}");
                return;
            }

            // Probe duration
            let duration = match std::process::Command::new(&ffprobe)
                .args([
                    "-v", "error",
                    "-show_entries", "format=duration",
                    "-of", "default=noprint_wrappers=1:nokey=1",
                ])
                .arg(&source)
                .output()
            {
                Ok(out) => {
                    let s = String::from_utf8_lossy(&out.stdout);
                    s.trim().parse::<f32>().unwrap_or(10.0)
                }
                Err(e) => {
                    tracing::error!("ffprobe failed: {e}");
                    10.0
                }
            };

            // Extract frames at lower quality/smaller size for speed
            let output_pattern = cache_dir.join("%06d.jpg");
            let status = std::process::Command::new(&ffmpeg)
                .args([
                    "-y",
                    "-hide_banner",
                    "-loglevel", "error",
                    "-i",
                ])
                .arg(&source)
                .args([
                    "-vf", "fps=30,scale=480:-1",
                    "-q:v", "8",
                ])
                .arg(&output_pattern)
                .status();

            match status {
                Ok(s) if s.success() => {
                    // Count extracted frames
                    let frame_count = std::fs::read_dir(&cache_dir)
                        .map(|rd| rd.filter_map(|e| e.ok()).filter(|e| {
                            e.path().extension().and_then(|s| s.to_str()) == Some("jpg")
                        }).count())
                        .unwrap_or(0);

                    tracing::info!(
                        "Frame extraction complete: {} frames, {:.1}s duration",
                        frame_count, duration
                    );
                    on_done(duration, frame_count, cache_dir);
                }
                Ok(s) => {
                    tracing::error!("ffmpeg frame extraction exited with: {}", s);
                }
                Err(e) => {
                    tracing::error!("ffmpeg frame extraction failed: {e}");
                }
            }
        });
    }

    /// Mark the cache as ready with extraction results and pre-load initial frames.
    pub fn set_ready(&mut self, duration: f32, frame_count: usize, cache_dir: PathBuf) {
        self.duration = duration;
        self.frame_count = frame_count;
        self.cache_dir = cache_dir;
        self.ready = true;
        self.extracting = false;

        // Detect source dimensions from the first extracted frame
        let first_frame_path = self.cache_dir.join("000001.jpg");
        if let Ok(img) = image::open(&first_frame_path) {
            self.source_width = img.width();
            self.source_height = img.height();
        }

        // Initialize ring buffer
        self.buffer = vec![None; self.buffer_size];
        self.buffer_start = 0;

        // Synchronously pre-load initial frames
        self.load_buffer_range(0);
    }

    /// Whether the cache has extracted frames and is ready for use.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Get the raw ColorImage for a given time (for post-processing like chroma-key).
    /// Same logic as frame_at_time but returns the image, not the texture.
    pub fn raw_frame_at_time(&mut self, t: f32) -> Option<ColorImage> {
        if !self.ready || self.frame_count == 0 {
            return None;
        }

        // Check for completed background pre-load
        self.poll_preload();

        // Compute frame index (0-based)
        let frame_index = ((t * self.fps).floor() as usize)
            .clamp(0, self.frame_count.saturating_sub(1));

        // Try buffer first
        if self.is_in_buffer(frame_index) {
            let buf_idx = frame_index - self.buffer_start;
            if let Some(img) = self.buffer.get(buf_idx).and_then(|s| s.clone()) {
                // Trigger read-ahead
                if frame_index != self.last_displayed_frame {
                    self.last_displayed_frame = frame_index;
                    let buffer_end = self.buffer_start + self.buffer_size;
                    let remaining = buffer_end.saturating_sub(frame_index);
                    if remaining < self.buffer_size / 3 && !self.preloading {
                        self.trigger_preload(frame_index);
                    }
                }
                return Some(img);
            }
        }

        // Fallback: load from disk (seek case)
        let img = self.load_frame_from_disk(frame_index);
        if img.is_some() {
            self.buffer_start = frame_index;
            self.buffer = vec![None; self.buffer_size];
            self.buffer[0] = img.clone();
            self.trigger_preload(frame_index);
            self.last_displayed_frame = frame_index;
        }
        img
    }

    /// Get the texture for a given time `t` in seconds.
    /// Uses a ring buffer for O(1) access and reuses a single TextureHandle.
    /// Returns `None` if the cache is not ready or frame cannot be loaded.
    pub fn frame_at_time(&mut self, t: f32, ctx: &egui::Context) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 {
            return None;
        }

        // Check for completed background pre-load
        self.poll_preload();

        // Compute frame index (0-based)
        let frame_index = ((t * self.fps).floor() as usize)
            .clamp(0, self.frame_count.saturating_sub(1));

        // Check if frame is in the ring buffer
        let image = if self.is_in_buffer(frame_index) {
            let buf_idx = frame_index - self.buffer_start;
            self.buffer.get(buf_idx).and_then(|slot| slot.clone())
        } else {
            // Frame not in buffer — load synchronously (happens on seeks)
            let img = self.load_frame_from_disk(frame_index);
            // Reposition buffer around this frame and trigger background pre-load
            self.buffer_start = frame_index;
            self.buffer = vec![None; self.buffer_size];
            if let Some(ref image) = img {
                self.buffer[0] = Some(image.clone());
            }
            // Fill rest in background
            self.trigger_preload(frame_index);
            img
        };

        let image = image?;

        // Skip the GPU re-upload when we're still on the same frame as last
        // time — this is the dominant cost when the playhead isn't advancing
        // (e.g., paused, or before the next video frame is due) and avoids
        // playback lag with multiple actors / overlays on screen.
        if self.texture.is_some()
            && self.texture_uploaded_frame == Some(frame_index)
        {
            // Update read-ahead bookkeeping below as before but no upload.
            if frame_index != self.last_displayed_frame {
                self.last_displayed_frame = frame_index;
                let buffer_end = self.buffer_start + self.buffer_size;
                let frames_remaining = buffer_end.saturating_sub(frame_index);
                if frames_remaining < self.buffer_size / 3 && !self.preloading {
                    self.trigger_preload(frame_index);
                }
            }
            return self.texture.as_ref();
        }

        // Update the single TextureHandle with the new image data
        let options = TextureOptions::LINEAR;
        match self.texture.as_mut() {
            Some(tex) => {
                tex.set(image, options);
            }
            None => {
                let tex = ctx.load_texture("frame_preview", image, options);
                self.texture = Some(tex);
            }
        }
        self.texture_uploaded_frame = Some(frame_index);

        // Trigger read-ahead if playhead advanced
        if frame_index != self.last_displayed_frame {
            self.last_displayed_frame = frame_index;
            // If we're approaching the end of the buffer, trigger pre-load ahead
            let buffer_end = self.buffer_start + self.buffer_size;
            let frames_remaining = buffer_end.saturating_sub(frame_index);
            if frames_remaining < self.buffer_size / 3 && !self.preloading {
                // Pre-load next chunk starting from current position
                self.trigger_preload(frame_index);
            }
        }

        self.texture.as_ref()
    }

    /// Check if a frame index is within the current ring buffer range.
    fn is_in_buffer(&self, frame_index: usize) -> bool {
        frame_index >= self.buffer_start
            && frame_index < self.buffer_start + self.buffer_size
            && frame_index < self.frame_count
    }

    /// Return a cached texture for the given time AFTER applying chroma-key
    /// and colour-correction. The processed texture is cached and only
    /// recomputed when the frame index or effect parameters change.
    pub fn processed_frame_at_time(
        &mut self,
        t: f32,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        ctx: &egui::Context,
    ) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 { return None; }
        let frame_index = ((t * self.fps).floor() as usize)
            .clamp(0, self.frame_count.saturating_sub(1));
        let new_key = (frame_index, hash_effect_params(ck, cc));

        // Cache hit — reuse the existing processed texture.
        if let (Some(prev_key), Some(_)) = (self.fx_key, self.fx_texture.as_ref()) {
            if prev_key == new_key {
                return self.fx_texture.as_ref();
            }
        }

        // Miss: get the raw frame and apply effects on a downscaled copy
        // for fast preview. Chromakey/CC quality at preview-resolution is
        // sufficient and avoids per-pixel CPU work on full HD frames.
        let raw = self.raw_frame_at_time(t)?;
        let scaled = downscale_for_preview(&raw, 360);
        let processed = apply_effects_cpu(&scaled, ck, cc);
        let options = TextureOptions::LINEAR;
        match self.fx_texture.as_mut() {
            Some(tex) => tex.set(processed, options),
            None => {
                let tex = ctx.load_texture(
                    format!("frame_fx_{}", self.actor_index),
                    processed,
                    options,
                );
                self.fx_texture = Some(tex);
            }
        }
        self.fx_key = Some(new_key);
        self.fx_texture.as_ref()
    }

    /// Load a single frame from disk as a ColorImage.
    fn load_frame_from_disk(&self, frame_index: usize) -> Option<ColorImage> {
        let file_name = format!("{:06}.jpg", frame_index + 1); // 1-based file naming
        let frame_path = self.cache_dir.join(&file_name);

        let img = match image::open(&frame_path) {
            Ok(img) => img.to_rgba8(),
            Err(_) => return None,
        };

        let size = [img.width() as usize, img.height() as usize];
        let pixels = img.into_raw();
        Some(ColorImage::from_rgba_unmultiplied(size, &pixels))
    }

    /// Synchronously load frames into the buffer starting at `start_frame`.
    fn load_buffer_range(&mut self, start_frame: usize) {
        self.buffer_start = start_frame;
        self.buffer = vec![None; self.buffer_size];

        for i in 0..self.buffer_size {
            let idx = start_frame + i;
            if idx >= self.frame_count {
                break;
            }
            self.buffer[i] = self.load_frame_from_disk(idx);
        }
    }

    /// Trigger a background thread to pre-load frames starting at `start_frame`.
    fn trigger_preload(&mut self, start_frame: usize) {
        if self.preloading {
            return;
        }
        self.preloading = true;

        let cache_dir = self.cache_dir.clone();
        let frame_count = self.frame_count;
        let buffer_size = self.buffer_size;
        let slot = self.preload_slot.clone();

        thread::spawn(move || {
            let mut frames = Vec::with_capacity(buffer_size);
            for i in 0..buffer_size {
                let idx = start_frame + i;
                if idx >= frame_count {
                    frames.push(None);
                    continue;
                }
                let file_name = format!("{:06}.jpg", idx + 1);
                let frame_path = cache_dir.join(&file_name);
                let img = match image::open(&frame_path) {
                    Ok(img) => {
                        let rgba = img.to_rgba8();
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        let pixels = rgba.into_raw();
                        Some(ColorImage::from_rgba_unmultiplied(size, &pixels))
                    }
                    Err(_) => None,
                };
                frames.push(img);
            }

            if let Ok(mut guard) = slot.lock() {
                *guard = Some(PreloadResult {
                    start: start_frame,
                    frames,
                });
            }
        });
    }

    /// Poll for completed background pre-load and apply results.
    fn poll_preload(&mut self) {
        if !self.preloading {
            return;
        }
        let result = if let Ok(mut guard) = self.preload_slot.lock() {
            guard.take()
        } else {
            None
        };

        if let Some(result) = result {
            self.buffer_start = result.start;
            self.buffer = result.frames;
            self.preloading = false;
        }
    }
}


// ─── EFFECT CACHING HELPERS ──────────────────────────────────────────

/// Hash the chroma-key + color-correction parameters with sub-millisecond
/// precision. Two equal parameter sets yield the same hash so the processed
/// texture cache can be reused.
fn hash_effect_params(
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ck.key_color.hash(&mut h);
    bits(ck.similarity).hash(&mut h);
    bits(ck.blend).hash(&mut h);
    bits(ck.spill).hash(&mut h);
    bits(cc.brightness).hash(&mut h);
    bits(cc.contrast).hash(&mut h);
    bits(cc.saturation).hash(&mut h);
    bits(cc.temperature).hash(&mut h);
    // Pro grade controls.
    for v in cc.lift.iter().chain(cc.gamma.iter()).chain(cc.gain.iter()) {
        bits(*v).hash(&mut h);
    }
    for curve in [&cc.curves.master, &cc.curves.red, &cc.curves.green, &cc.curves.blue] {
        (curve.len() as u32).hash(&mut h);
        for p in curve {
            bits(p[0]).hash(&mut h);
            bits(p[1]).hash(&mut h);
        }
    }
    h.finish()
}

#[inline]
fn bits(f: f32) -> u32 { (f * 10_000.0).round() as i32 as u32 }

/// Downscale a ColorImage to a target maximum dimension while preserving
/// aspect ratio. Used to keep the CPU chromakey/CC fast during playback.
/// Returns the input unchanged if it's already small enough.
pub fn downscale_for_preview(img: &ColorImage, max_dim: usize) -> ColorImage {
    let (sw, sh) = (img.size[0], img.size[1]);
    if sw <= max_dim && sh <= max_dim {
        return img.clone();
    }
    let aspect = sw as f32 / sh as f32;
    let (dw, dh) = if sw >= sh {
        let dw = max_dim;
        let dh = ((dw as f32) / aspect).round().max(1.0) as usize;
        (dw, dh)
    } else {
        let dh = max_dim;
        let dw = ((dh as f32) * aspect).round().max(1.0) as usize;
        (dw, dh)
    };
    // Nearest-neighbour is enough for chromakey input; the result is uploaded
    // with linear filtering so the final on-screen texture stays smooth.
    let mut out = ColorImage::new([dw, dh], egui::Color32::TRANSPARENT);
    for y in 0..dh {
        let sy = (y * sh) / dh;
        for x in 0..dw {
            let sx = (x * sw) / dw;
            out.pixels[y * dw + x] = img.pixels[sy * sw + sx];
        }
    }
    out
}

/// CPU implementation of the chroma-key + colour-correction pipeline.
/// Mirrors what the live `apply_preview_effects` did but lives on the cache so
/// the result can be reused across repaints.
///
/// Pipeline order (per pixel):
///   1. chromakey + spill suppression
///   2. legacy brightness / contrast / saturation / temperature
///   3. lift → gain → gamma per RGB channel (DaVinci-style)
///   4. master curve, then per-channel curves (R / G / B)
///
/// To keep this fast on full-HD frames the four tone curves are pre-baked
/// into 256-entry LUTs once per call instead of re-sampled per pixel.
pub fn apply_effects_cpu(
    img: &ColorImage,
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> ColorImage {
    let mut out = ColorImage::new(img.size, egui::Color32::TRANSPARENT);
    let key_r = ck.key_color[0] as f32;
    let key_g = ck.key_color[1] as f32;
    let key_b = ck.key_color[2] as f32;
    let similarity = ck.similarity.clamp(0.0, 1.0);
    let blend = ck.blend.clamp(0.0, 1.0);
    let spill = ck.spill.clamp(0.0, 1.0);
    let threshold = similarity * 441.0;
    let blend_range = (blend * 200.0).max(0.01);

    // Pre-bake tone curves into LUTs for cache-friendly per-pixel lookup.
    let lut_master = build_curve_lut(&cc.curves.master);
    let lut_r = build_curve_lut(&cc.curves.red);
    let lut_g = build_curve_lut(&cc.curves.green);
    let lut_b = build_curve_lut(&cc.curves.blue);
    let curves_active = !cc.curves.is_identity();

    // Pre-clamp the LGG parameters so degenerate values (e.g. gamma = 0)
    // can't blow up the per-pixel pow().
    let gain = [
        cc.gain[0].max(0.0),
        cc.gain[1].max(0.0),
        cc.gain[2].max(0.0),
    ];
    let inv_gamma = [
        1.0 / cc.gamma[0].max(0.05),
        1.0 / cc.gamma[1].max(0.05),
        1.0 / cc.gamma[2].max(0.05),
    ];
    let lift = cc.lift;
    let lgg_active = lift.iter().any(|v| v.abs() > 1e-4)
        || gain.iter().any(|v| (v - 1.0).abs() > 1e-4)
        || inv_gamma.iter().any(|v| (v - 1.0).abs() > 1e-4);

    for (i, pixel) in img.pixels.iter().enumerate() {
        let r = pixel.r() as f32;
        let g = pixel.g() as f32;
        let b = pixel.b() as f32;

        let dist = ((r - key_r).powi(2) + (g - key_g).powi(2) + (b - key_b).powi(2)).sqrt();
        let alpha = if dist < threshold {
            0.0
        } else if dist < threshold + blend_range {
            (dist - threshold) / blend_range
        } else {
            1.0
        };

        let (mut or_, mut og, mut ob) = (r, g, b);
        if alpha > 0.0 && spill > 0.0 && g > (r + b) * 0.5 {
            let avg_rb = (r + b) * 0.5;
            og = g - (g - avg_rb) * spill;
        }

        // Brightness / contrast / saturation / temperature
        or_ = (or_ + cc.brightness * 255.0).clamp(0.0, 255.0);
        og  = (og  + cc.brightness * 255.0).clamp(0.0, 255.0);
        ob  = (ob  + cc.brightness * 255.0).clamp(0.0, 255.0);
        or_ = ((or_ - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        og  = ((og  - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        ob  = ((ob  - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        let gray = 0.299 * or_ + 0.587 * og + 0.114 * ob;
        or_ = (gray + (or_ - gray) * cc.saturation).clamp(0.0, 255.0);
        og  = (gray + (og  - gray) * cc.saturation).clamp(0.0, 255.0);
        ob  = (gray + (ob  - gray) * cc.saturation).clamp(0.0, 255.0);
        if cc.temperature != 0.0 {
            or_ = (or_ + cc.temperature * 30.0).clamp(0.0, 255.0);
            ob  = (ob  - cc.temperature * 30.0).clamp(0.0, 255.0);
        }

        // ── DaVinci-style lift / gain / gamma per channel ──
        // Work in normalised 0..1 space and apply:
        //   out = pow((in + lift*(1-in)) * gain, 1/gamma)
        // which means lift pushes shadows up, gain scales highlights, and
        // gamma reshapes midtones — each independently per RGB channel.
        if lgg_active {
            let mut nr = or_ / 255.0;
            let mut ng = og / 255.0;
            let mut nb = ob / 255.0;
            nr = nr + lift[0] * (1.0 - nr);
            ng = ng + lift[1] * (1.0 - ng);
            nb = nb + lift[2] * (1.0 - nb);
            nr = (nr * gain[0]).max(0.0);
            ng = (ng * gain[1]).max(0.0);
            nb = (nb * gain[2]).max(0.0);
            nr = nr.powf(inv_gamma[0]);
            ng = ng.powf(inv_gamma[1]);
            nb = nb.powf(inv_gamma[2]);
            or_ = (nr * 255.0).clamp(0.0, 255.0);
            og  = (ng * 255.0).clamp(0.0, 255.0);
            ob  = (nb * 255.0).clamp(0.0, 255.0);
        }

        // ── Tone curves: master first, then per-channel ──
        if curves_active {
            or_ = lut_master[or_ as usize] as f32;
            og  = lut_master[og  as usize] as f32;
            ob  = lut_master[ob  as usize] as f32;
            or_ = lut_r[or_ as usize] as f32;
            og  = lut_g[og  as usize] as f32;
            ob  = lut_b[ob  as usize] as f32;
        }

        let a = (alpha * 255.0).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(or_ as u8, og as u8, ob as u8, a);
    }
    out
}

/// Build a 256-entry LUT from a piecewise-linear tone curve. Each LUT entry
/// is the curve's output for the corresponding 8-bit input clamped to 0..255.
/// Identity curves are detected up-front and produce an identity table.
fn build_curve_lut(curve: &[[f32; 2]]) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let x = i as f32 / 255.0;
        let y = memstroy_core::ToneCurves::sample(curve, x).clamp(0.0, 1.0);
        *slot = (y * 255.0).round() as u8;
    }
    lut
}
