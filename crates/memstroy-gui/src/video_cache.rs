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
/// With 7+ actors on screen, a larger buffer reduces the frequency of
/// disk-seek stalls during playback. 90 frames = 3 seconds at 30fps,
/// which gives the preload thread enough runway to stay ahead of the
/// playhead even when multiple actors are competing for I/O bandwidth.
const BUFFER_SIZE: usize = 90;

/// Maximum number of concurrent preload threads across ALL frame caches.
/// When many actors are playing simultaneously, unlimited preload threads
/// compete for disk I/O bandwidth and cause stalls. This semaphore limits
/// the concurrency so at most N caches are reading from disk at once.
/// The remaining caches wait their turn, which paradoxically improves
/// throughput because the disk's sequential read pattern isn't broken
/// by random seeks across 6+ temp directories.
static PRELOAD_SEMAPHORE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CONCURRENT_PRELOADS: usize = 3;

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
    #[allow(dead_code)]
    pub fn start_extraction(
        source: PathBuf,
        rt: &Handle,
        on_done: impl FnOnce(f32, usize, PathBuf) + Send + 'static,
    ) {
        rt.spawn(async move {
            extract_frames_blocking(source, on_done);
        });
    }

    /// Same as `start_extraction` but uses a plain OS thread instead of a
    /// tokio runtime handle. Useful for callers that don't have access to
    /// the App's runtime. Originally added for the floating Skeleton
    /// Constructor side panel; the constructor was retired in favour of
    /// the inspector skeleton tab, but the helper is kept on the API so
    /// future panels can extract previews without dragging the runtime
    /// around with them.
    #[allow(dead_code)]
    pub fn start_extraction_thread(
        source: PathBuf,
        on_done: impl FnOnce(f32, usize, PathBuf) + Send + 'static,
    ) {
        thread::spawn(move || {
            extract_frames_blocking(source, on_done);
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
                    if remaining < self.buffer_size / 2 && !self.preloading {
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
                if frames_remaining < self.buffer_size / 2 && !self.preloading {
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
            if frames_remaining < self.buffer_size / 2 && !self.preloading {
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

    /// Return a cached texture for the given time AFTER applying chroma-key,
    /// colour-correction, and the post-process effect stack. The processed
    /// texture is cached and only recomputed when the frame index or any
    /// effect parameter changes.
    pub fn processed_frame_at_time(
        &mut self,
        t: f32,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
        ctx: &egui::Context,
    ) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 { return None; }
        let frame_index = ((t * self.fps).floor() as usize)
            .clamp(0, self.frame_count.saturating_sub(1));
        let new_key = (frame_index, hash_effect_params(ck, cc, effects));

        // Cache hit — reuse the existing processed texture.
        if let (Some(prev_key), Some(_)) = (self.fx_key, self.fx_texture.as_ref()) {
            if prev_key == new_key {
                return self.fx_texture.as_ref();
            }
        }

        // Fast path: when there are no user effects AND no active
        // chroma/CC, skip the entire processing pipeline and just
        // upload the downscaled raw frame. This saves the allocation
        // + per-pixel loop on every frame during playback for the
        // common case of "no color tweaks applied".
        let raw = self.raw_frame_at_time(t)?;
        let active_caches = PRELOAD_SEMAPHORE.load(std::sync::atomic::Ordering::Relaxed);
        let preview_dim = if active_caches >= 4 { 180 } else if active_caches >= 2 { 240 } else { 360 };
        let scaled = downscale_for_preview(&raw, preview_dim);

        let chroma_active = ck.similarity.is_finite() && ck.similarity >= 1.0e-5;
        let cc_active = (cc.brightness.abs() > 1e-4)
            || (cc.contrast.abs() > 1e-4)
            || (cc.saturation.abs() > 1e-4)
            || (cc.temperature.abs() > 1e-4)
            || cc.lift.iter().any(|v| v.abs() > 1e-4)
            || cc.gamma.iter().any(|v| (v - 1.0).abs() > 1e-4)
            || cc.gain.iter().any(|v| (v - 1.0).abs() > 1e-4)
            || !cc.curves.is_identity();
        let effects_active = effects.iter().any(|e| e.enabled && e.intensity > 0.001);

        let processed = if !chroma_active && !cc_active && !effects_active {
            // No-op: just upload the scaled frame as-is.
            scaled
        } else {
            let mut p = apply_effects_cpu(&scaled, ck, cc);
            if effects_active {
                p = apply_effect_stack_cpu(&p, effects);
            }
            p
        };
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
        // Check the global preload semaphore — if too many caches are
        // already preloading, skip this request. The next frame's
        // poll_preload / read-ahead check will retry, and by then one
        // of the other preloads will have finished and released its
        // slot. This prevents 6+ threads hammering the disk in parallel.
        let current = PRELOAD_SEMAPHORE.load(std::sync::atomic::Ordering::Relaxed);
        if current >= MAX_CONCURRENT_PRELOADS {
            return;
        }
        PRELOAD_SEMAPHORE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.preloading = true;

        let cache_dir = self.cache_dir.clone();
        let frame_count = self.frame_count;
        let buffer_size = self.buffer_size;
        let slot = self.preload_slot.clone();

        thread::Builder::new()
            .name("memstroy-frame-preload".into())
            .spawn(move || {
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
                // Release the semaphore slot so other caches can preload.
                PRELOAD_SEMAPHORE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            })
            .ok(); // Ignore spawn failure — worst case we fall back to sync load.
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
    effects: &[memstroy_core::Effect],
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
    // Effect stack.
    (effects.len() as u32).hash(&mut h);
    for e in effects {
        e.enabled.hash(&mut h);
        bits(e.intensity).hash(&mut h);
        hash_effect_kind(&e.kind, &mut h);
    }
    h.finish()
}

fn hash_effect_kind<H: std::hash::Hasher>(kind: &memstroy_core::EffectKind, h: &mut H) {
    use memstroy_core::EffectKind as K;
    use std::hash::Hash;
    // Discriminant plus relevant numeric bits.
    std::mem::discriminant(kind).hash(h);
    match kind {
        K::Blur { radius } => bits(*radius).hash(h),
        K::Sharpen { amount } => bits(*amount).hash(h),
        K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV
            | K::OldFilm | K::Vhs => {}
        K::HueShift { degrees } => bits(*degrees).hash(h),
        K::Vignette { strength } => bits(*strength).hash(h),
        K::Pixelate { block_size } => bits(*block_size).hash(h),
        K::Posterize { levels } => levels.hash(h),
        K::Glow { radius, intensity } => {
            bits(*radius).hash(h); bits(*intensity).hash(h);
        }
        K::Brightness { amount } | K::Contrast { amount }
            | K::Saturation { amount } | K::Glitch { strength: amount }
            | K::Noise { amount } | K::EdgeDetect { threshold: amount } => bits(*amount).hash(h),
        K::ChromaticAberration { offset } => bits(*offset).hash(h),
        K::Wave { amplitude, wavelength } => {
            bits(*amplitude).hash(h); bits(*wavelength).hash(h);
        }
        K::Bloom { radius } => bits(*radius).hash(h),
        K::Crop { left, top, right, bottom } => {
            bits(*left).hash(h);
            bits(*top).hash(h);
            bits(*right).hash(h);
            bits(*bottom).hash(h);
        }
        K::Mask { shape, feather, invert } => {
            bits(*feather).hash(h);
            invert.hash(h);
            hash_mask_shape(shape, h);
        }
        K::ColorKey { color, similarity, blend, spill, invert } => {
            color.hash(h);
            bits(*similarity).hash(h);
            bits(*blend).hash(h);
            bits(*spill).hash(h);
            invert.hash(h);
        }
    }
}

fn hash_mask_shape<H: std::hash::Hasher>(shape: &memstroy_core::MaskShape, h: &mut H) {
    use memstroy_core::MaskShape as M;
    use std::hash::Hash;
    std::mem::discriminant(shape).hash(h);
    match shape {
        M::Rect { left, top, right, bottom } => {
            bits(*left).hash(h);
            bits(*top).hash(h);
            bits(*right).hash(h);
            bits(*bottom).hash(h);
        }
        M::Ellipse { cx, cy, rx, ry } => {
            bits(*cx).hash(h);
            bits(*cy).hash(h);
            bits(*rx).hash(h);
            bits(*ry).hash(h);
        }
        M::Polygon { points } => {
            (points.len() as u32).hash(h);
            for p in points {
                bits(p[0]).hash(h);
                bits(p[1]).hash(h);
            }
        }
    }
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
    // Fast path: if neither chromakey nor color correction is active,
    // skip the per-pixel loop entirely and just return a clone. This
    // is the common case when the user hasn't touched any color
    // controls — and saves a per-frame allocation+walk during playback.
    let similarity_check = ck.similarity.is_finite() && ck.similarity >= 1.0e-5;
    let cc_check = (cc.brightness.abs() > 1e-4)
        || (cc.contrast.abs() > 1e-4)
        || (cc.saturation.abs() > 1e-4)
        || (cc.temperature.abs() > 1e-4)
        || cc.lift.iter().any(|v| v.abs() > 1e-4)
        || cc.gamma.iter().any(|v| (v - 1.0).abs() > 1e-4)
        || cc.gain.iter().any(|v| (v - 1.0).abs() > 1e-4)
        || !cc.curves.is_identity();
    if !similarity_check && !cc_check {
        return img.clone();
    }
    let mut out = ColorImage::new(img.size, egui::Color32::TRANSPARENT);
    // ── Chromakey: FFmpeg-faithful YCbCr (BT.601) distance ──
    //
    // The export pipeline (memstroy-render/filtergraph.rs) emits
    // FFmpeg's `chromakey` filter, which keys on the Cb/Cr distance
    // between each pixel and the chosen key colour, normalised to
    // `[0, 1]` by `255*sqrt(2)`. Mirroring that maths here keeps the
    // canvas preview pixel-aligned with the rendered video — the
    // legacy RGB-Euclidean approximation (`*441 / *200`) drifted
    // visibly from the export, especially at the soft edges, which
    // is the "preview shows one thing, render shows another" bug
    // the user reported.
    //
    // `similarity < 1e-5` (or non-finite) is treated as "chromakey
    // disabled" — the same threshold the export-side `chromakey_filter`
    // helper uses, so dialing the slider all the way down disables the
    // key on both surfaces consistently instead of crashing the
    // export with `Result too large`.
    let similarity = if ck.similarity.is_finite() { ck.similarity.clamp(0.0, 1.0) } else { 0.0 };
    let blend = if ck.blend.is_finite() { ck.blend.clamp(0.0, 1.0) } else { 0.0 };
    let spill = if ck.spill.is_finite() { ck.spill.clamp(0.0, 1.0) } else { 0.0 };
    let chroma_active = similarity >= 1.0e-5;
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(ck.key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;

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

        // FFmpeg-equivalent YCbCr keying: identical formula to the
        // chromakey filter the export pipeline uses, so the alpha
        // map the user sees in the canvas matches the rendered MP4.
        // When `similarity < 1e-5` the chromakey is disabled (the
        // export skips the filter entirely on the same threshold),
        // keeping the source pixel fully opaque.
        let alpha = if !chroma_active {
            1.0
        } else {
            let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
            let cr =  0.500 * r - 0.419 * g - 0.081 * b + 128.0;
            let du = cb - key_cb;
            let dv = cr - key_cr;
            let diff = (du * du + dv * dv).sqrt() / dist_norm;
            if diff < similarity {
                0.0
            } else if blend > 0.0 && diff < similarity + blend {
                ((diff - similarity) / blend).clamp(0.0, 1.0)
            } else {
                1.0
            }
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

/// Convert an RGB triple (0..=255) to BT.601 chroma components
/// `(Cb, Cr)` in the same 0..255 axis used by FFmpeg's `chromakey`
/// filter. Shared between `apply_effects_cpu` and the colour-key
/// effect-stack preview so both paths produce the alpha map FFmpeg
/// would emit on export.
#[inline]
pub(crate) fn rgb_to_cbcr_bt601(rgb: [u8; 3]) -> (f32, f32) {
    let r = rgb[0] as f32;
    let g = rgb[1] as f32;
    let b = rgb[2] as f32;
    let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
    let cr =  0.500 * r - 0.419 * g - 0.081 * b + 128.0;
    (cb, cr)
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


// ─── EFFECT STACK (CPU PREVIEW) ──────────────────────────────────────
//
// Generic per-element post-process stack applied AFTER chroma-key and
// colour correction. Each effect is implemented as a small in-place
// pixel transform — the simplest approach that keeps the preview snappy
// at 360p without any GPU support, matching the rest of the preview
// pipeline. Heavier effects (blur, bloom, glow) use a tiny separable
// box-kernel approximation; the export pipeline (ffmpeg) gets the real
// thing via filtergraph.

/// Apply the user's effect stack to a preview frame in declared order.
/// Disabled / zero-intensity entries are skipped.
pub fn apply_effect_stack_cpu(
    img: &ColorImage,
    effects: &[memstroy_core::Effect],
) -> ColorImage {
    // Fast path: skip the clone when there's nothing to do
    if effects.is_empty() {
        return img.clone();
    }
    let mut current = img.clone();
    for eff in effects {
        if !eff.enabled { continue; }
        let intensity = eff.intensity.clamp(0.0, 1.0);
        if intensity <= 0.001 { continue; }
        current = apply_single_effect(&current, &eff.kind, intensity);
    }
    current
}

fn apply_single_effect(
    img: &ColorImage,
    kind: &memstroy_core::EffectKind,
    intensity: f32,
) -> ColorImage {
    use memstroy_core::EffectKind as K;
    match kind {
        K::Blur { radius } => box_blur(img, ((*radius) * intensity).round() as i32),
        K::Sharpen { amount } => sharpen(img, *amount * intensity),
        K::Grayscale => mix_pixels(img, &grayscale(img), intensity),
        K::Sepia => mix_pixels(img, &sepia(img), intensity),
        K::Invert => mix_pixels(img, &invert(img), intensity),
        K::HueShift { degrees } => hue_shift(img, *degrees * intensity),
        K::Vignette { strength } => vignette(img, (*strength * intensity).clamp(0.0, 1.0)),
        K::Pixelate { block_size } => pixelate(img, ((*block_size).max(1.0)) as i32, intensity),
        K::Posterize { levels } => posterize(img, *levels, intensity),
        K::Glow { radius, intensity: i2 } => glow(img, *radius, *i2 * intensity),
        K::Brightness { amount } => brightness(img, *amount * intensity),
        K::Contrast { amount } => contrast(img, *amount * intensity),
        K::Saturation { amount } => saturation(img, *amount * intensity),
        K::EdgeDetect { threshold } => edge_detect(img, *threshold, intensity),
        K::MirrorH => mix_pixels(img, &mirror_h(img), intensity),
        K::MirrorV => mix_pixels(img, &mirror_v(img), intensity),
        K::ChromaticAberration { offset } => chromatic_aberration(img, *offset * intensity),
        K::Noise { amount } => noise(img, *amount * intensity),
        K::Wave { amplitude, wavelength } => wave(img, *amplitude * intensity, *wavelength),
        K::OldFilm => old_film(img, intensity),
        K::Vhs => vhs(img, intensity),
        K::Glitch { strength } => glitch(img, *strength * intensity),
        K::Bloom { radius } => bloom(img, *radius, intensity),
        K::Crop { left, top, right, bottom } => crop_alpha(
            img,
            (*left * intensity).clamp(0.0, 0.49),
            (*top * intensity).clamp(0.0, 0.49),
            (*right * intensity).clamp(0.0, 0.49),
            (*bottom * intensity).clamp(0.0, 0.49),
        ),
        K::Mask { shape, feather, invert } => {
            apply_mask_color_image(img, shape, *feather, *invert, intensity)
        }
        K::ColorKey { color, similarity, blend, spill, invert } => {
            apply_color_key_color_image(
                img,
                *color,
                *similarity,
                *blend,
                *spill,
                *invert,
                intensity,
            )
        }
    }
}

/// Apply a [`memstroy_core::MaskShape`] to a `ColorImage`'s alpha
/// channel. Mirrors `image_effects::apply_mask_alpha` but operates on
/// the live frame buffer used by the video preview pipeline.
///
/// Optimized: takes ownership and mutates the alpha channel in place
/// without allocating a fresh ColorImage. For simple shapes (Rect
/// without feather), uses fast-path bounds checks instead of calling
/// `sample_mask_alpha` per pixel.
fn apply_mask_color_image(
    img: &ColorImage,
    shape: &memstroy_core::MaskShape,
    feather: f32,
    invert: bool,
    intensity: f32,
) -> ColorImage {
    let mut out = img.clone();
    let w = out.size[0];
    let h = out.size[1];
    if w == 0 || h == 0 { return out; }
    let i = intensity.clamp(0.0, 1.0);

    // Fast path: hard-edge axis-aligned rectangle (no feather).
    // Just clear alpha outside the rect (or inside if invert).
    if let memstroy_core::MaskShape::Rect { left, top, right, bottom } = shape {
        if feather <= 1e-6 && i >= 0.999 {
            let lx = (left * w as f32) as i32;
            let ty = (top * h as f32) as i32;
            let rx = (right * w as f32) as i32;
            let by = (bottom * h as f32) as i32;
            for y in 0..h as i32 {
                let inside_y = y >= ty && y <= by;
                let row = (y as usize) * w;
                for x in 0..w as i32 {
                    let inside_x = x >= lx && x <= rx;
                    let inside = inside_x && inside_y;
                    let keep = if invert { !inside } else { inside };
                    if !keep {
                        let idx = row + x as usize;
                        if idx < out.pixels.len() {
                            let p = out.pixels[idx];
                            out.pixels[idx] = egui::Color32::from_rgba_unmultiplied(
                                p.r(), p.g(), p.b(), 0,
                            );
                        }
                    }
                }
            }
            return out;
        }
    }

    // General path: per-pixel sample.
    let inv_w = 1.0 / (w as f32);
    let inv_h = 1.0 / (h as f32);
    for y in 0..h {
        let v = (y as f32 + 0.5) * inv_h;
        let row = y * w;
        for x in 0..w {
            let u = (x as f32 + 0.5) * inv_w;
            let keep = crate::image_effects::sample_mask_alpha(
                shape, u, v, feather, invert,
            );
            // Skip pixels that don't change (keep == 1.0 and intensity full)
            if keep >= 0.9999 && i >= 0.9999 { continue; }
            let idx = row + x;
            if idx < out.pixels.len() {
                let p = out.pixels[idx];
                let orig = p.a() as f32;
                let target = orig * keep;
                let new_a = (orig + (target - orig) * i).clamp(0.0, 255.0) as u8;
                out.pixels[idx] = egui::Color32::from_rgba_unmultiplied(
                    p.r(), p.g(), p.b(), new_a,
                );
            }
        }
    }
    out
}

/// Apply a colour-key mask by attenuating alpha for pixels close to
/// `key_color` in YUV (Cb/Cr) space — the same maths FFmpeg's
/// `chromakey` filter uses. Mirrors `image_effects::apply_color_key_alpha`
/// so the canvas preview's alpha map is pixel-aligned with the
/// rendered video. The legacy HSV-distance approximation visibly
/// drifted from the export — the user's "what I see in the preview is
/// not what gets rendered" complaint — and is replaced here.
fn apply_color_key_color_image(
    img: &ColorImage,
    key_color: [u8; 3],
    similarity: f32,
    blend: f32,
    _spill: f32,
    invert: bool,
    intensity: f32,
) -> ColorImage {
    let mut out = img.clone();
    let w = img.size[0];
    let h = img.size[1];
    if w == 0 || h == 0 { return out; }
    let i = intensity.clamp(0.0, 1.0);
    // Mirror `chromakey_filter`'s "disabled below 1e-5" rule so a
    // dialed-down ColorKey effect renders identically on both
    // surfaces — the export pipeline emits a no-op `null` filter on
    // that threshold, so the preview must do nothing too.
    let similarity = if similarity.is_finite() { similarity.clamp(0.0, 1.0) } else { 0.0 };
    let blend = if blend.is_finite() { blend.clamp(0.0, 1.0) } else { 0.0 };
    if similarity < 1.0e-5 && !invert {
        // Disabled key — nothing to attenuate. Returning early keeps
        // the per-pixel loop's no-op path obvious.
        return out;
    }
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;
    for px in 0..(w * h) {
        if px >= out.pixels.len() { break; }
        let p = out.pixels[px];
        let r = p.r() as f32;
        let g = p.g() as f32;
        let b = p.b() as f32;
        let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
        let cr =  0.500 * r - 0.419 * g - 0.081 * b + 128.0;
        let du = cb - key_cb;
        let dv = cr - key_cr;
        let diff = (du * du + dv * dv).sqrt() / dist_norm;
        let mut alpha_keep = if diff < similarity {
            0.0
        } else if blend > 0.0 && diff < similarity + blend {
            ((diff - similarity) / blend).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if invert { alpha_keep = 1.0 - alpha_keep; }
        let orig = p.a() as f32;
        let target = orig * alpha_keep;
        let new_a = (orig + (target - orig) * i).clamp(0.0, 255.0) as u8;
        out.pixels[px] = egui::Color32::from_rgba_unmultiplied(p.r(), p.g(), p.b(), new_a);
    }
    out
}

/// Apply a Crop effect by zeroing the alpha channel outside the visible
/// rectangle. Cheap and faithful enough for the preview path; the
/// ffmpeg export uses a real `crop` filter for full fidelity.
fn crop_alpha(
    img: &ColorImage,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
) -> ColorImage {
    let mut out = img.clone();
    let w = img.size[0];
    let h = img.size[1];
    if w == 0 || h == 0 {
        return out;
    }
    let lx = (left * w as f32).round() as usize;
    let ty = (top * h as f32).round() as usize;
    let rx = w.saturating_sub((right * w as f32).round() as usize);
    let by = h.saturating_sub((bottom * h as f32).round() as usize);
    for y in 0..h {
        for x in 0..w {
            if x < lx || x >= rx || y < ty || y >= by {
                let idx = y * w + x;
                if idx < out.pixels.len() {
                    let p = out.pixels[idx];
                    out.pixels[idx] = egui::Color32::from_rgba_unmultiplied(
                        p.r(), p.g(), p.b(), 0,
                    );
                }
            }
        }
    }
    out
}

fn mix_pixels(a: &ColorImage, b: &ColorImage, t: f32) -> ColorImage {
    let t = t.clamp(0.0, 1.0);
    let mut out = a.clone();
    let n = a.pixels.len().min(b.pixels.len());
    for i in 0..n {
        let pa = a.pixels[i];
        let pb = b.pixels[i];
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(
            lerp_u8(pa.r(), pb.r(), t),
            lerp_u8(pa.g(), pb.g(), t),
            lerp_u8(pa.b(), pb.b(), t),
            pa.a(),
        );
    }
    out
}

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let af = a as f32;
    let bf = b as f32;
    (af + (bf - af) * t).clamp(0.0, 255.0) as u8
}

fn grayscale(img: &ColorImage) -> ColorImage {
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        let g = (0.299 * px.r() as f32 + 0.587 * px.g() as f32 + 0.114 * px.b() as f32)
            .clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(g, g, g, px.a());
    }
    out
}

fn sepia(img: &ColorImage) -> ColorImage {
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        let r = px.r() as f32; let g = px.g() as f32; let b = px.b() as f32;
        let nr = (0.393 * r + 0.769 * g + 0.189 * b).clamp(0.0, 255.0) as u8;
        let ng = (0.349 * r + 0.686 * g + 0.168 * b).clamp(0.0, 255.0) as u8;
        let nb = (0.272 * r + 0.534 * g + 0.131 * b).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(nr, ng, nb, px.a());
    }
    out
}

fn invert(img: &ColorImage) -> ColorImage {
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        *px = egui::Color32::from_rgba_unmultiplied(255 - px.r(), 255 - px.g(), 255 - px.b(), px.a());
    }
    out
}

fn brightness(img: &ColorImage, amount: f32) -> ColorImage {
    let mut out = img.clone();
    let add = amount * 255.0;
    for px in out.pixels.iter_mut() {
        *px = egui::Color32::from_rgba_unmultiplied(
            (px.r() as f32 + add).clamp(0.0, 255.0) as u8,
            (px.g() as f32 + add).clamp(0.0, 255.0) as u8,
            (px.b() as f32 + add).clamp(0.0, 255.0) as u8,
            px.a(),
        );
    }
    out
}

fn contrast(img: &ColorImage, amount: f32) -> ColorImage {
    // amount in [-1, 1]: -1 = grey, 0 = neutral, 1 = strong contrast.
    let factor = (1.0 + amount).max(0.0);
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        let r = ((px.r() as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        let g = ((px.g() as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        let b = ((px.b() as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(r, g, b, px.a());
    }
    out
}

fn saturation(img: &ColorImage, amount: f32) -> ColorImage {
    let factor = (1.0 + amount).max(0.0);
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        let r = px.r() as f32; let g = px.g() as f32; let b = px.b() as f32;
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        let nr = (gray + (r - gray) * factor).clamp(0.0, 255.0) as u8;
        let ng = (gray + (g - gray) * factor).clamp(0.0, 255.0) as u8;
        let nb = (gray + (b - gray) * factor).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(nr, ng, nb, px.a());
    }
    out
}

fn hue_shift(img: &ColorImage, degrees: f32) -> ColorImage {
    let mut out = img.clone();
    let theta = degrees.to_radians();
    let c = theta.cos();
    let s = theta.sin();
    // Approximate hue rotation matrix in RGB space (Foley & van Dam).
    let m00 = 0.213 + 0.787 * c - 0.213 * s;
    let m01 = 0.213 - 0.213 * c + 0.413 * s;
    let m02 = 0.213 - 0.213 * c - 0.787 * s;
    let m10 = 0.715 - 0.715 * c - 0.715 * s;
    let m11 = 0.715 + 0.285 * c + 0.140 * s;
    let m12 = 0.715 - 0.715 * c + 0.715 * s;
    let m20 = 0.072 - 0.072 * c + 0.928 * s;
    let m21 = 0.072 - 0.072 * c - 0.283 * s;
    let m22 = 0.072 + 0.928 * c + 0.072 * s;
    for px in out.pixels.iter_mut() {
        let r = px.r() as f32; let g = px.g() as f32; let b = px.b() as f32;
        let nr = (m00 * r + m10 * g + m20 * b).clamp(0.0, 255.0) as u8;
        let ng = (m01 * r + m11 * g + m21 * b).clamp(0.0, 255.0) as u8;
        let nb = (m02 * r + m12 * g + m22 * b).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(nr, ng, nb, px.a());
    }
    out
}

fn vignette(img: &ColorImage, strength: f32) -> ColorImage {
    let mut out = img.clone();
    let (w, h) = (img.size[0] as f32, img.size[1] as f32);
    let cx = w * 0.5;
    let cy = h * 0.5;
    let max_d = (cx * cx + cy * cy).sqrt();
    for y in 0..img.size[1] {
        for x in 0..img.size[0] {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt() / max_d;
            // Smooth falloff toward the corners.
            let factor = (1.0 - strength * (d * d)).clamp(0.0, 1.0);
            let i = y * img.size[0] + x;
            let p = out.pixels[i];
            out.pixels[i] = egui::Color32::from_rgba_unmultiplied(
                (p.r() as f32 * factor) as u8,
                (p.g() as f32 * factor) as u8,
                (p.b() as f32 * factor) as u8,
                p.a(),
            );
        }
    }
    out
}

fn pixelate(img: &ColorImage, block: i32, intensity: f32) -> ColorImage {
    let block = block.max(1);
    let (w, h) = (img.size[0], img.size[1]);
    let mut pixelated = img.clone();
    for by in (0..h).step_by(block as usize) {
        for bx in (0..w).step_by(block as usize) {
            // Sample one pixel per block (top-left).
            let p = img.pixels[by * w + bx];
            for dy in 0..(block as usize) {
                if by + dy >= h { break; }
                for dx in 0..(block as usize) {
                    if bx + dx >= w { break; }
                    pixelated.pixels[(by + dy) * w + (bx + dx)] = p;
                }
            }
        }
    }
    mix_pixels(img, &pixelated, intensity)
}

fn posterize(img: &ColorImage, levels: u32, intensity: f32) -> ColorImage {
    let levels = levels.clamp(2, 64) as f32;
    let step = 255.0 / (levels - 1.0);
    let mut out = img.clone();
    for px in out.pixels.iter_mut() {
        let r = ((px.r() as f32 / step).round() * step).clamp(0.0, 255.0) as u8;
        let g = ((px.g() as f32 / step).round() * step).clamp(0.0, 255.0) as u8;
        let b = ((px.b() as f32 / step).round() * step).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(r, g, b, px.a());
    }
    mix_pixels(img, &out, intensity)
}

fn box_blur(img: &ColorImage, radius: i32) -> ColorImage {
    if radius <= 0 { return img.clone(); }
    // Two-pass separable box blur — cheap and good enough for preview.
    let pass1 = blur_pass(img, radius, true);
    blur_pass(&pass1, radius, false)
}

fn blur_pass(img: &ColorImage, radius: i32, horizontal: bool) -> ColorImage {
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let mut out = img.clone();
    for y in 0..h {
        for x in 0..w {
            let mut r = 0u32; let mut g = 0u32; let mut b = 0u32; let mut a = 0u32;
            let mut count = 0u32;
            for k in -radius..=radius {
                let (sx, sy) = if horizontal { (x + k, y) } else { (x, y + k) };
                if sx < 0 || sy < 0 || sx >= w || sy >= h { continue; }
                let p = img.pixels[(sy * w + sx) as usize];
                r += p.r() as u32; g += p.g() as u32; b += p.b() as u32; a += p.a() as u32;
                count += 1;
            }
            if count == 0 { continue; }
            out.pixels[(y * w + x) as usize] = egui::Color32::from_rgba_unmultiplied(
                (r / count) as u8, (g / count) as u8, (b / count) as u8, (a / count) as u8,
            );
        }
    }
    out
}

fn sharpen(img: &ColorImage, amount: f32) -> ColorImage {
    if amount <= 0.001 { return img.clone(); }
    let blurred = box_blur(img, 2);
    let mut out = img.clone();
    for i in 0..out.pixels.len() {
        let p = img.pixels[i];
        let bl = blurred.pixels[i];
        let r = (p.r() as f32 + amount * (p.r() as f32 - bl.r() as f32)).clamp(0.0, 255.0) as u8;
        let g = (p.g() as f32 + amount * (p.g() as f32 - bl.g() as f32)).clamp(0.0, 255.0) as u8;
        let b = (p.b() as f32 + amount * (p.b() as f32 - bl.b() as f32)).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(r, g, b, p.a());
    }
    out
}

fn glow(img: &ColorImage, radius: f32, intensity: f32) -> ColorImage {
    if intensity <= 0.001 || radius <= 0.5 { return img.clone(); }
    let blurred = box_blur(img, radius.round() as i32);
    // Additive blend of the blurred copy on top of the original.
    let mut out = img.clone();
    for i in 0..out.pixels.len() {
        let p = img.pixels[i];
        let bl = blurred.pixels[i];
        let r = (p.r() as f32 + bl.r() as f32 * intensity).clamp(0.0, 255.0) as u8;
        let g = (p.g() as f32 + bl.g() as f32 * intensity).clamp(0.0, 255.0) as u8;
        let b = (p.b() as f32 + bl.b() as f32 * intensity).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(r, g, b, p.a());
    }
    out
}

fn bloom(img: &ColorImage, radius: f32, intensity: f32) -> ColorImage {
    // Threshold the bright pixels, blur them, add back.
    let mut bright = img.clone();
    for px in bright.pixels.iter_mut() {
        let lum = 0.299 * px.r() as f32 + 0.587 * px.g() as f32 + 0.114 * px.b() as f32;
        if lum < 200.0 {
            *px = egui::Color32::from_rgba_unmultiplied(0, 0, 0, px.a());
        }
    }
    let blurred = box_blur(&bright, radius.round() as i32);
    let mut out = img.clone();
    for i in 0..out.pixels.len() {
        let p = img.pixels[i];
        let bl = blurred.pixels[i];
        let r = (p.r() as f32 + bl.r() as f32 * intensity).clamp(0.0, 255.0) as u8;
        let g = (p.g() as f32 + bl.g() as f32 * intensity).clamp(0.0, 255.0) as u8;
        let b = (p.b() as f32 + bl.b() as f32 * intensity).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(r, g, b, p.a());
    }
    out
}

fn edge_detect(img: &ColorImage, threshold: f32, intensity: f32) -> ColorImage {
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let mut edges = img.clone();
    let lum = |x: i32, y: i32| -> f32 {
        let cx = x.clamp(0, w - 1);
        let cy = y.clamp(0, h - 1);
        let p = img.pixels[(cy * w + cx) as usize];
        0.299 * p.r() as f32 + 0.587 * p.g() as f32 + 0.114 * p.b() as f32
    };
    let cutoff = threshold * 255.0;
    for y in 0..h {
        for x in 0..w {
            // Sobel approximation.
            let gx = -lum(x - 1, y - 1) - 2.0 * lum(x - 1, y) - lum(x - 1, y + 1)
                   +  lum(x + 1, y - 1) + 2.0 * lum(x + 1, y) + lum(x + 1, y + 1);
            let gy = -lum(x - 1, y - 1) - 2.0 * lum(x, y - 1) - lum(x + 1, y - 1)
                   +  lum(x - 1, y + 1) + 2.0 * lum(x, y + 1) + lum(x + 1, y + 1);
            let mag = (gx * gx + gy * gy).sqrt();
            let v = if mag > cutoff { 255 } else { 0 };
            let alpha = img.pixels[(y * w + x) as usize].a();
            edges.pixels[(y * w + x) as usize] =
                egui::Color32::from_rgba_unmultiplied(v, v, v, alpha);
        }
    }
    mix_pixels(img, &edges, intensity)
}

fn mirror_h(img: &ColorImage) -> ColorImage {
    let mut out = img.clone();
    let (w, h) = (img.size[0], img.size[1]);
    for y in 0..h {
        for x in 0..w {
            out.pixels[y * w + x] = img.pixels[y * w + (w - 1 - x)];
        }
    }
    out
}

fn mirror_v(img: &ColorImage) -> ColorImage {
    let mut out = img.clone();
    let (w, h) = (img.size[0], img.size[1]);
    for y in 0..h {
        for x in 0..w {
            out.pixels[y * w + x] = img.pixels[(h - 1 - y) * w + x];
        }
    }
    out
}

fn chromatic_aberration(img: &ColorImage, offset: f32) -> ColorImage {
    let off = offset.round() as i32;
    if off == 0 { return img.clone(); }
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let mut out = img.clone();
    let sample = |x: i32, y: i32| -> egui::Color32 {
        let cx = x.clamp(0, w - 1);
        let cy = y.clamp(0, h - 1);
        img.pixels[(cy * w + cx) as usize]
    };
    for y in 0..h {
        for x in 0..w {
            // Shift R channel left, B channel right; G stays put.
            let pr = sample(x - off, y);
            let pg = sample(x, y);
            let pb = sample(x + off, y);
            out.pixels[(y * w + x) as usize] = egui::Color32::from_rgba_unmultiplied(
                pr.r(), pg.g(), pb.b(), pg.a(),
            );
        }
    }
    out
}

fn noise(img: &ColorImage, amount: f32) -> ColorImage {
    let mut out = img.clone();
    let amp = (amount * 255.0).clamp(0.0, 255.0);
    // Cheap deterministic hash-based noise (no rng dependency).
    for (i, px) in out.pixels.iter_mut().enumerate() {
        let n = ((i.wrapping_mul(2654435761) ^ 0x9E3779B9) as u32) as f32;
        let nf = ((n / u32::MAX as f32) * 2.0 - 1.0) * amp;
        let r = (px.r() as f32 + nf).clamp(0.0, 255.0) as u8;
        let g = (px.g() as f32 + nf).clamp(0.0, 255.0) as u8;
        let b = (px.b() as f32 + nf).clamp(0.0, 255.0) as u8;
        *px = egui::Color32::from_rgba_unmultiplied(r, g, b, px.a());
    }
    out
}

fn wave(img: &ColorImage, amplitude: f32, wavelength: f32) -> ColorImage {
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let mut out = img.clone();
    let lambda = wavelength.max(1.0);
    for y in 0..h {
        let dx = (((y as f32) / lambda) * std::f32::consts::TAU).sin() * amplitude;
        let off = dx.round() as i32;
        for x in 0..w {
            let sx = (x + off).clamp(0, w - 1);
            out.pixels[(y * w + x) as usize] = img.pixels[(y * w + sx) as usize];
        }
    }
    out
}

fn old_film(img: &ColorImage, intensity: f32) -> ColorImage {
    let s = sepia(img);
    let v = vignette(&s, 0.7 * intensity);
    let n = noise(&v, 0.10 * intensity);
    mix_pixels(img, &n, intensity)
}

fn vhs(img: &ColorImage, intensity: f32) -> ColorImage {
    let ca = chromatic_aberration(img, 4.0 * intensity);
    let n = noise(&ca, 0.06 * intensity);
    // Simple scanlines: dim every other row.
    let (w, h) = (n.size[0], n.size[1]);
    let mut out = n.clone();
    for y in 0..h {
        if y % 2 == 0 {
            for x in 0..w {
                let p = out.pixels[y * w + x];
                let dim = (1.0 - 0.25 * intensity).clamp(0.0, 1.0);
                out.pixels[y * w + x] = egui::Color32::from_rgba_unmultiplied(
                    (p.r() as f32 * dim) as u8,
                    (p.g() as f32 * dim) as u8,
                    (p.b() as f32 * dim) as u8,
                    p.a(),
                );
            }
        }
    }
    out
}

fn glitch(img: &ColorImage, strength: f32) -> ColorImage {
    if strength <= 0.001 { return img.clone(); }
    let (w, h) = (img.size[0], img.size[1]);
    let mut out = img.clone();
    // Slice the image into ~12 horizontal bands and shift each by a
    // pseudo-random offset proportional to strength.
    let band_count = 12usize;
    let band_h = (h / band_count).max(1);
    for bi in 0..band_count {
        let y0 = bi * band_h;
        let y1 = ((bi + 1) * band_h).min(h);
        // Hash-based "random" shift, deterministic across frames.
        let raw = ((bi as u32).wrapping_mul(2246822519) ^ 0x9E3779B9) as f32;
        let r = (raw / u32::MAX as f32) * 2.0 - 1.0;
        let off = (r * strength * w as f32 * 0.15) as i32;
        for y in y0..y1 {
            for x in 0..w as i32 {
                let sx = (x + off).rem_euclid(w as i32);
                out.pixels[y * w + x as usize] = img.pixels[y * w + sx as usize];
            }
        }
    }
    out
}


// ─── EXTRACTION HELPER ───────────────────────────────────────────────

/// Synchronously extract frames for a clip via `ffmpeg` and `ffprobe`.
/// Invoked from background workers (tokio task or std::thread). Calls
/// `on_done` with the resulting (duration_secs, frame_count, cache_dir)
/// only on success; errors are logged and the callback is skipped.
fn extract_frames_blocking(
    source: PathBuf,
    on_done: impl FnOnce(f32, usize, PathBuf) + Send + 'static,
) {
    extract_frames_blocking_with_scale(source, 480, on_done);
}

/// Same as `extract_frames_blocking` but with a configurable max width.
/// Used by the adaptive-quality system to extract at lower resolution
/// when many actors are on screen simultaneously.
pub fn extract_frames_blocking_with_scale(
    source: PathBuf,
    max_width: u32,
    on_done: impl FnOnce(f32, usize, PathBuf) + Send + 'static,
) {
    let ffmpeg = memstroy_render::ffmpeg_binary();
    let ffprobe = {
        let mut p = ffmpeg.clone();
        p.set_file_name("ffprobe");
        if !p.exists() {
            PathBuf::from("ffprobe")
        } else {
            p
        }
    };

    let cache_dir = std::env::temp_dir().join(format!(
        "memstroy_frames_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        tracing::error!("Failed to create frame cache dir: {e}");
        return;
    }

    let duration = {
        let mut cmd = std::process::Command::new(&ffprobe);
        cmd.args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(&source);
        match memstroy_render::hide_console_std(&mut cmd).output() {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout);
                s.trim().parse::<f32>().unwrap_or(10.0)
            }
            Err(e) => {
                tracing::error!("ffprobe failed: {e}");
                10.0
            }
        }
    };

    let output_pattern = cache_dir.join("%06d.jpg");
    let scale_filter = format!("fps=30,scale={}:-1", max_width);
    let status = {
        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&source)
            .args(["-vf", &scale_filter, "-q:v", "8"])
            .arg(&output_pattern);
        memstroy_render::hide_console_std(&mut cmd).status()
    };

    match status {
        Ok(s) if s.success() => {
            let frame_count = std::fs::read_dir(&cache_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path().extension().and_then(|s| s.to_str()) == Some("jpg")
                        })
                        .count()
                })
                .unwrap_or(0);

            tracing::info!(
                "Frame extraction complete: {} frames, {:.1}s duration",
                frame_count,
                duration
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
}
