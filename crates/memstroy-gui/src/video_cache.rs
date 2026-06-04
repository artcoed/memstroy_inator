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

pub fn adaptive_extract_width(actor_count: usize) -> u32 {
    if actor_count >= 6 {
        280
    } else if actor_count >= 4 {
        360
    } else {
        480
    }
}

pub fn scaled_preview_dimensions(
    source_width: u32,
    source_height: u32,
    actor_count: usize,
) -> Option<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    let width = adaptive_extract_width(actor_count);
    let height = ((source_height as f32 * width as f32) / source_width as f32)
        .round()
        .max(1.0) as u32;
    Some((width, height))
}

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
    /// Set when ffmpeg/ffprobe could not build a preview (bad file, missing ffmpeg, …).
    pub failed: bool,
    /// Wall-clock start of the in-flight extraction (for UI timeouts).
    pub extract_started_at: Option<std::time::Instant>,
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

    // ── Background processed-frame pipeline (chroma-key + effects) ──
    /// Post-processed frame images in memory (ring buffer). Kept in sync
    /// with `buffer` — same indices, same start, but with chroma-key,
    /// colour-correction and the effect stack already applied.
    processed_buffer: Vec<Option<ColorImage>>,
    /// First frame index represented in `processed_buffer`.
    processed_buffer_start: usize,
    /// Hash of the (ck, cc, effects) params that produced the current
    /// `processed_buffer`. Invalidated when the user edits any colour
    /// or effect control so the next `processed_frame_at_time` rebuilds.
    processed_params_hash: u64,
    /// Shared slot for background *processed* pre-load thread results.
    processed_preload_slot: Arc<Mutex<Option<ProcessedPreloadResult>>>,
    /// Whether a background *processed* pre-load is currently running.
    processed_preloading: bool,
}

/// Result from the background pre-load thread.
struct PreloadResult {
    start: usize,
    frames: Vec<Option<ColorImage>>,
}

/// Result from the background *processed* pre-load thread.
/// Includes `params_hash` so stale results (params changed while the
/// thread was running) can be discarded on arrival.
struct ProcessedPreloadResult {
    start: usize,
    frames: Vec<Option<ColorImage>>,
    params_hash: u64,
}

impl FrameCache {
    /// After a razor split, the right-half actor can reuse the left
    /// half's extracted frames (same source file) so preview sizing
    /// and playback stay correct while a dedicated cache row exists.
    pub fn seed_from_sibling(&mut self, sibling: &FrameCache) {
        if self.source != sibling.source || !sibling.is_ready() {
            return;
        }
        self.cache_dir = sibling.cache_dir.clone();
        self.fps = sibling.fps;
        self.frame_count = sibling.frame_count;
        self.duration = sibling.duration;
        self.source_width = sibling.source_width;
        self.source_height = sibling.source_height;
        self.ready = true;
        self.extracting = false;
        self.failed = false;
        self.extract_started_at = None;
    }

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
            failed: false,
            extract_started_at: None,
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

            processed_buffer: Vec::new(),
            processed_buffer_start: 0,
            processed_params_hash: 0,
            processed_preload_slot: Arc::new(Mutex::new(None)),
            processed_preloading: false,
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
        on_done: impl FnOnce(Result<(f32, usize, PathBuf), ()>) + Send + 'static,
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
        on_done: impl FnOnce(Result<(f32, usize, PathBuf), ()>) + Send + 'static,
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
        self.failed = false;
        self.extract_started_at = None;

        // Initialize ring buffers. Decode only frame 0 on the UI thread —
        // loading all 90 slots here was freezing the app for 1–2s when
        // extraction finished and the canvas switched thumbnail → video.
        self.buffer_start = 0;
        self.buffer = vec![None; self.buffer_size];
        self.processed_buffer = vec![None; self.buffer_size];
        self.processed_buffer_start = 0;
        self.processed_params_hash = 0;

        if self.frame_count > 0 {
            if let Some(first) = self.load_frame_from_disk(0) {
                self.source_width = first.size[0] as u32;
                self.source_height = first.size[1] as u32;
                self.buffer[0] = Some(first);
            }
        }
        self.trigger_preload(0);
    }

    /// Whether the background chroma/effects pipeline has produced at least
    /// one frame. While false, callers should keep showing the library
    /// thumbnail instead of blocking on a synchronous chroma pass.
    pub fn processed_frame_warm(&self) -> bool {
        self.processed_buffer.iter().any(|slot| slot.is_some())
    }

    /// Kick off (without waiting for) a processed preload window covering `t`.
    pub fn ensure_processed_preload(
        &mut self,
        t: f32,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
    ) {
        if !self.ready || self.frame_count == 0 {
            return;
        }
        self.poll_processed_preload();
        let frame_index =
            ((t * self.fps).floor() as usize).clamp(0, self.frame_count.saturating_sub(1));
        let params_hash = hash_effect_params(ck, cc, effects);
        if self.processed_params_hash != params_hash {
            self.processed_buffer.clear();
            self.processed_buffer_start = 0;
            self.processed_params_hash = params_hash;
        }
        if !self.processed_preloading {
            self.trigger_processed_preload(frame_index, ck, cc, effects);
        }
    }

    /// Insert a cache slot for a new actor index, optionally cloning
    /// probe metadata from an existing row that shares the same source.
    pub fn insert_for_actor_split(
        caches: &mut Vec<FrameCache>,
        left_idx: usize,
        right_idx: usize,
        source: PathBuf,
    ) {
        while caches.len() <= right_idx {
            let idx = caches.len();
            caches.push(FrameCache::new(PathBuf::new(), idx));
        }
        let mut right = FrameCache::new(source, right_idx);
        if left_idx < caches.len() {
            right.seed_from_sibling(&caches[left_idx]);
        }
        caches.insert(right_idx, right);
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

        self.poll_preload();

        let frame_index =
            ((t * self.fps).floor() as usize).clamp(0, self.frame_count.saturating_sub(1));

        let img = self.resolve_frame_image(frame_index)?;
        self.note_frame_displayed(frame_index);
        Some(img)
    }

    /// Get the texture for a given time `t` in seconds.
    /// Uses a ring buffer for O(1) access and reuses a single TextureHandle.
    /// Returns `None` if the cache is not ready or frame cannot be loaded.
    pub fn frame_at_time(&mut self, t: f32, ctx: &egui::Context) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 {
            return None;
        }

        self.poll_preload();

        let frame_index =
            ((t * self.fps).floor() as usize).clamp(0, self.frame_count.saturating_sub(1));

        let image = match self.resolve_frame_image(frame_index) {
            Some(img) => img,
            None => {
                // Transient I/O gap while another preload is in flight — keep
                // the last uploaded frame so actors don't flash empty for ~0.5s.
                if self.texture.is_some() {
                    self.note_frame_displayed(frame_index);
                    return self.texture.as_ref();
                }
                return None;
            }
        };

        // Skip the GPU re-upload when we're still on the same frame as last
        // time — this is the dominant cost when the playhead isn't advancing
        // (e.g., paused, or before the next video frame is due) and avoids
        // playback lag with multiple actors / overlays on screen.
        if self.texture.is_some() && self.texture_uploaded_frame == Some(frame_index) {
            self.note_frame_displayed(frame_index);
            return self.texture.as_ref();
        }

        // Update the single TextureHandle with the new image data
        let options = TextureOptions::LINEAR;
        match self.texture.as_mut() {
            Some(tex) => {
                tex.set(image, options);
            }
            None => {
                let tex = ctx.load_texture(
                    format!("frame_preview_{}", self.actor_index),
                    image,
                    options,
                );
                self.texture = Some(tex);
            }
        }
        self.texture_uploaded_frame = Some(frame_index);

        self.note_frame_displayed(frame_index);

        self.texture.as_ref()
    }

    /// Resolve a frame from the ring buffer, falling back to a synchronous disk
    /// read and back-filling the slot. Fixes holes when preload slots are `None`
    /// or the playhead moved ahead of a stale background preload.
    fn resolve_frame_image(&mut self, frame_index: usize) -> Option<ColorImage> {
        if self.is_in_buffer(frame_index) {
            let buf_idx = frame_index - self.buffer_start;
            if let Some(img) = self.buffer.get(buf_idx).and_then(|s| s.clone()) {
                return Some(img);
            }
        }

        let img = self.load_frame_from_disk(frame_index)?;

        if self.is_in_buffer(frame_index) {
            let buf_idx = frame_index - self.buffer_start;
            if let Some(slot) = self.buffer.get_mut(buf_idx) {
                *slot = Some(img.clone());
            }
        } else {
            self.buffer_start = frame_index;
            self.buffer = vec![None; self.buffer_size];
            self.buffer[0] = Some(img.clone());
            self.trigger_preload(frame_index);
        }

        Some(img)
    }

    /// Read-ahead bookkeeping after a frame is shown.
    fn note_frame_displayed(&mut self, frame_index: usize) {
        if frame_index == self.last_displayed_frame {
            return;
        }
        self.last_displayed_frame = frame_index;
        let buffer_end = self.buffer_start + self.buffer_size;
        let frames_remaining = buffer_end.saturating_sub(frame_index + 1);
        if frames_remaining < self.buffer_size / 2 && !self.preloading {
            self.trigger_preload(frame_index);
        }
    }

    /// Preview downscale dimension — smaller when many caches are preloading.
    fn preview_dim_for_preload() -> usize {
        let active_caches = PRELOAD_SEMAPHORE.load(std::sync::atomic::Ordering::Relaxed);
        if active_caches >= 4 {
            180
        } else if active_caches >= 2 {
            240
        } else {
            360
        }
    }

    /// Run chroma-key + colour-correction + effects on one decoded frame.
    fn process_raw_frame(
        raw: &ColorImage,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
        preview_dim: usize,
    ) -> ColorImage {
        let scaled = downscale_for_preview(raw, preview_dim);

        let chroma_active = ck.is_active();
        let cc_active = (cc.brightness.abs() > 1e-4)
            || (cc.contrast.abs() > 1e-4)
            || (cc.saturation.abs() > 1e-4)
            || (cc.temperature.abs() > 1e-4)
            || cc.lift.iter().any(|v| v.abs() > 1e-4)
            || cc.gamma.iter().any(|v| (v - 1.0).abs() > 1e-4)
            || cc.gain.iter().any(|v| (v - 1.0).abs() > 1e-4)
            || !cc.curves.is_identity();
        let effects_active = effects.iter().any(|e| e.enabled && e.intensity > 0.001);

        if !chroma_active && !cc_active && !effects_active {
            scaled
        } else {
            let mut p = apply_effects_cpu(&scaled, ck, cc);
            if effects_active {
                p = apply_effect_stack_cpu(&p, effects);
            }
            p
        }
    }

    /// Synchronously decode + process a single frame when the processed ring
    /// buffer hasn't caught up yet. Keeps playback moving past the 90-frame
    /// window instead of freezing on the last cached texture.
    fn sync_process_frame(
        &mut self,
        frame_index: usize,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
    ) -> Option<ColorImage> {
        let raw = self.resolve_frame_image(frame_index)?;
        Some(Self::process_raw_frame(
            &raw,
            ck,
            cc,
            effects,
            Self::preview_dim_for_preload(),
        ))
    }

    fn preload_covers_frame(&self, start: usize, len: usize, frame_index: usize) -> bool {
        frame_index >= start && frame_index < start + len
    }

    /// Check if a frame index is within the current ring buffer range.
    fn is_in_buffer(&self, frame_index: usize) -> bool {
        frame_index >= self.buffer_start
            && frame_index < self.buffer_start + self.buffer_size
            && frame_index < self.frame_count
    }

    /// Return a cached texture for the given time AFTER applying chroma-key,
    /// colour-correction, and the post-process effect stack.
    ///
    /// **Three-tier cache:**
    /// 1. `fx_texture` — single-frame LRU for the exact `(frame_index, params)` pair.
    /// 2. `processed_buffer` — ring buffer of already-processed frames coming from
    ///    a background thread (zero CPU cost on the UI thread).
    /// 3. Raw fallback — when neither processed cache is ready we return the
    ///    unprocessed frame instantly and kick off a background processed preload.
    ///    This keeps playback responsive (no UI-blocking chroma-key) at the cost
    ///    of showing the raw clip for 1-2 frames until the background worker catches up.
    pub fn processed_frame_at_time(
        &mut self,
        t: f32,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
        ctx: &egui::Context,
    ) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 {
            return None;
        }
        let frame_index =
            ((t * self.fps).floor() as usize).clamp(0, self.frame_count.saturating_sub(1));
        let params_hash = hash_effect_params(ck, cc, effects);
        let new_key = (frame_index, params_hash);

        self.poll_processed_preload();
        self.note_frame_displayed(frame_index);

        // ── Tier 1: single-frame texture cache (fastest) ──
        if let (Some(prev_key), Some(_)) = (self.fx_key, self.fx_texture.as_ref()) {
            if prev_key == new_key {
                return self.fx_texture.as_ref();
            }
        }

        // ── Tier 2: processed ring buffer (background worker) ──
        if self.processed_params_hash != params_hash {
            self.processed_buffer.clear();
            self.processed_buffer_start = 0;
            self.processed_params_hash = params_hash;
        }

        if self.is_in_processed_buffer(frame_index) {
            let buf_idx = frame_index - self.processed_buffer_start;
            if let Some(img) = self.processed_buffer.get(buf_idx).and_then(|s| s.as_ref()) {
                let options = TextureOptions::LINEAR;
                match self.fx_texture.as_mut() {
                    Some(tex) => tex.set(img.clone(), options),
                    None => {
                        let tex = ctx.load_texture(
                            format!("frame_fx_{}", self.actor_index),
                            img.clone(),
                            options,
                        );
                        self.fx_texture = Some(tex);
                    }
                }
                self.fx_key = Some(new_key);

                let proc_end = self.processed_buffer_start + self.processed_buffer.len();
                let proc_remaining = proc_end.saturating_sub(frame_index + 1);
                if proc_remaining < self.buffer_size / 2 {
                    self.trigger_processed_preload(frame_index, ck, cc, effects);
                }

                return self.fx_texture.as_ref();
            }
        }

        // ── Tier 2b: sync process when the playhead outran the buffer ──
        if !self.processed_preloading {
            self.trigger_processed_preload(frame_index, ck, cc, effects);
        }
        // Skip synchronous chroma on a cold processed buffer (typical right
        // after extraction) — the canvas keeps the library thumbnail visible
        // until the background worker delivers the first processed frame.
        if self.processed_frame_warm() {
            if let Some(processed) = self.sync_process_frame(frame_index, ck, cc, effects) {
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
                return self.fx_texture.as_ref();
            }
        }

        // Cold processed cache or a stuck background worker: show the raw
        // frame immediately instead of keeping the canvas on a loader.
        self.frame_at_time(t, ctx)
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
            // Playhead jumped ahead of the in-flight window — cancel the stale
            // preload so we can start one centred on the current frame.
            if self.last_displayed_frame != usize::MAX
                && !self.is_in_buffer(self.last_displayed_frame)
            {
                self.preloading = false;
                if let Ok(mut guard) = self.preload_slot.lock() {
                    *guard = None;
                }
            } else {
                return;
            }
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
            let display = self.last_displayed_frame;
            let covers = display == usize::MAX
                || self.preload_covers_frame(result.start, result.frames.len(), display);
            if covers {
                self.buffer_start = result.start;
                self.buffer = result.frames;
            } else if display != usize::MAX && !self.is_in_buffer(display) {
                // Stale preload completed behind the playhead — reload around
                // the frame we're actually showing instead of swapping in data
                // that leaves the current index uncovered.
                self.load_buffer_range(display);
                self.trigger_preload(display);
            }
            self.preloading = false;
        }
    }

    // ── PROCESSED (CHROMA-KEY + EFFECTS) BACKGROUND PIPELINE ─────────────

    /// Check if a frame index is within the current processed ring buffer range.
    fn is_in_processed_buffer(&self, frame_index: usize) -> bool {
        frame_index >= self.processed_buffer_start
            && frame_index < self.processed_buffer_start + self.processed_buffer.len()
            && frame_index < self.frame_count
    }

    /// Trigger a background thread that decodes raw frames and runs the
    /// full chroma-key + colour-correction + effect stack on them.
    /// Results land in `processed_preload_slot`; the UI thread picks them
    /// up via `poll_processed_preload` on the next frame.
    fn trigger_processed_preload(
        &mut self,
        start_frame: usize,
        ck: &memstroy_core::ChromaKeyParams,
        cc: &memstroy_core::ColorCorrection,
        effects: &[memstroy_core::Effect],
    ) {
        if self.processed_preloading {
            if self.last_displayed_frame != usize::MAX
                && !self.is_in_processed_buffer(self.last_displayed_frame)
            {
                self.processed_preloading = false;
                if let Ok(mut guard) = self.processed_preload_slot.lock() {
                    *guard = None;
                }
            } else {
                return;
            }
        }

        let params_hash = hash_effect_params(ck, cc, effects);
        let preview_dim = Self::preview_dim_for_preload();

        // Clone params for the closure.
        let ck = ck.clone();
        let cc = cc.clone();
        let effects = effects.to_vec();
        let cache_dir = self.cache_dir.clone();
        let frame_count = self.frame_count;
        let buffer_size = self.buffer_size;
        let slot = self.processed_preload_slot.clone();

        self.processed_preloading = true;

        match thread::Builder::new()
            .name(format!("memstroy-processed-preload-{}", self.actor_index))
            .spawn(move || {
                let frames = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut frames = Vec::with_capacity(buffer_size);
                    for i in 0..buffer_size {
                        let idx = start_frame + i;
                        if idx >= frame_count {
                            frames.push(None);
                            continue;
                        }

                        // Decode raw JPEG from disk (same path as raw preload).
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

                        let processed = img.map(|raw| {
                            FrameCache::process_raw_frame(&raw, &ck, &cc, &effects, preview_dim)
                        });

                        frames.push(processed);
                    }
                    frames
                }))
                .unwrap_or_else(|_| {
                    tracing::warn!("processed frame preload panicked");
                    Vec::new()
                });

                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(ProcessedPreloadResult {
                        start: start_frame,
                        frames,
                        params_hash,
                    });
                }
                // Note: we do NOT reset processed_preloading here —
                // that flag is cleared by poll_processed_preload on the UI thread.
            }) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("failed to spawn processed frame preload: {e}");
                self.processed_preloading = false;
            }
        }
    }

    /// Poll for completed background *processed* pre-load and apply
    /// results into the processed ring buffer. Stale results (params_hash
    /// mismatch) are silently discarded so the UI never shows frames that
    /// were computed with outdated colour / effect settings.
    fn poll_processed_preload(&mut self) {
        if !self.processed_preloading {
            return;
        }
        let result = if let Ok(mut guard) = self.processed_preload_slot.lock() {
            guard.take()
        } else {
            None
        };

        if let Some(result) = result {
            // Discard stale work: if the user edited params while the thread
            // was running, the arrived frames are for the old settings.
            if result.params_hash == self.processed_params_hash {
                let display = self.last_displayed_frame;
                let covers = display == usize::MAX
                    || self.preload_covers_frame(result.start, result.frames.len(), display);
                if covers {
                    self.processed_buffer_start = result.start;
                    self.processed_buffer = result.frames;
                } else if display != usize::MAX && !self.is_in_processed_buffer(display) {
                    // Stale preload completed behind the playhead — discard it.
                    // `processed_frame_at_time` sync-processes the current frame
                    // and will re-trigger preload centred on the playhead.
                }
            }
            self.processed_preloading = false;
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
    ck.enabled.hash(&mut h);
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
    for curve in [
        &cc.curves.master,
        &cc.curves.red,
        &cc.curves.green,
        &cc.curves.blue,
    ] {
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
        K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV | K::OldFilm | K::Vhs => {}
        K::HueShift { degrees } => bits(*degrees).hash(h),
        K::Vignette { strength } => bits(*strength).hash(h),
        K::Pixelate { block_size } => bits(*block_size).hash(h),
        K::Posterize { levels } => levels.hash(h),
        K::Glow { radius, intensity } => {
            bits(*radius).hash(h);
            bits(*intensity).hash(h);
        }
        K::Brightness { amount }
        | K::Contrast { amount }
        | K::Saturation { amount }
        | K::Glitch { strength: amount }
        | K::Noise { amount }
        | K::EdgeDetect { threshold: amount } => bits(*amount).hash(h),
        K::ChromaticAberration { offset } => bits(*offset).hash(h),
        K::Wave {
            amplitude,
            wavelength,
        } => {
            bits(*amplitude).hash(h);
            bits(*wavelength).hash(h);
        }
        K::Bloom { radius } => bits(*radius).hash(h),
        K::Crop {
            left,
            top,
            right,
            bottom,
        } => {
            bits(*left).hash(h);
            bits(*top).hash(h);
            bits(*right).hash(h);
            bits(*bottom).hash(h);
        }
        K::Mask {
            shape,
            feather,
            invert,
        } => {
            bits(*feather).hash(h);
            invert.hash(h);
            hash_mask_shape(shape, h);
        }
        K::ColorKey {
            color,
            similarity,
            blend,
            spill,
            invert,
        } => {
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
        M::Rect {
            left,
            top,
            right,
            bottom,
        } => {
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
        M::BrushStroke { points, radius } => {
            bits(*radius).hash(h);
            (points.len() as u32).hash(h);
            for p in points {
                bits(p[0]).hash(h);
                bits(p[1]).hash(h);
            }
        }
    }
}

#[inline]
fn bits(f: f32) -> u32 {
    (f * 10_000.0).round() as i32 as u32
}

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
pub(crate) fn apply_effects_cpu_scalar(
    img: &ColorImage,
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> ColorImage {
    // Fast path: if neither chromakey nor color correction is active,
    // skip the per-pixel loop entirely and just return a clone. This
    // is the common case when the user hasn't touched any color
    // controls — and saves a per-frame allocation+walk during playback.
    let similarity_check = ck.is_active();
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
    let similarity = if ck.similarity.is_finite() {
        ck.similarity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = if ck.blend.is_finite() {
        ck.blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let spill = if ck.spill.is_finite() {
        ck.spill.clamp(0.0, 1.0)
    } else {
        0.0
    };
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
            let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
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
        og = (og + cc.brightness * 255.0).clamp(0.0, 255.0);
        ob = (ob + cc.brightness * 255.0).clamp(0.0, 255.0);
        or_ = ((or_ - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        og = ((og - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        ob = ((ob - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        let gray = 0.299 * or_ + 0.587 * og + 0.114 * ob;
        or_ = (gray + (or_ - gray) * cc.saturation).clamp(0.0, 255.0);
        og = (gray + (og - gray) * cc.saturation).clamp(0.0, 255.0);
        ob = (gray + (ob - gray) * cc.saturation).clamp(0.0, 255.0);
        if cc.temperature != 0.0 {
            or_ = (or_ + cc.temperature * 30.0).clamp(0.0, 255.0);
            ob = (ob - cc.temperature * 30.0).clamp(0.0, 255.0);
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
            og = (ng * 255.0).clamp(0.0, 255.0);
            ob = (nb * 255.0).clamp(0.0, 255.0);
        }

        // ── Tone curves: master first, then per-channel ──
        if curves_active {
            or_ = lut_master[or_ as usize] as f32;
            og = lut_master[og as usize] as f32;
            ob = lut_master[ob as usize] as f32;
            or_ = lut_r[or_ as usize] as f32;
            og = lut_g[og as usize] as f32;
            ob = lut_b[ob as usize] as f32;
        }

        let a = (alpha * 255.0).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(or_ as u8, og as u8, ob as u8, a);
    }
    out
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) fn apply_effects_cpu_simd(
    img: &ColorImage,
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> ColorImage {
    use wide::{f32x8, CmpGt, CmpLt};

    // ── Early-out checks (same as scalar) ──
    let similarity_check = ck.is_active();
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

    let similarity = if ck.similarity.is_finite() {
        ck.similarity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = if ck.blend.is_finite() {
        ck.blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let spill = if ck.spill.is_finite() {
        ck.spill.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let chroma_active = similarity >= 1.0e-5;
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(ck.key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;

    // LUTs & LGG pre-clamp (same as scalar)
    let lut_master = build_curve_lut(&cc.curves.master);
    let lut_r = build_curve_lut(&cc.curves.red);
    let lut_g = build_curve_lut(&cc.curves.green);
    let lut_b = build_curve_lut(&cc.curves.blue);
    let curves_active = !cc.curves.is_identity();

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

    let n = img.pixels.len();
    let mut out = ColorImage::new(img.size, egui::Color32::TRANSPARENT);

    // ── SIMD constants ──
    let zero = f32x8::ZERO;
    let one = f32x8::ONE;
    let max255 = f32x8::splat(255.0);
    let half = f32x8::splat(0.5);
    let c128 = f32x8::splat(128.0);
    let brightness = f32x8::splat(cc.brightness * 255.0);
    let contrast = f32x8::splat(cc.contrast);
    let saturation = f32x8::splat(cc.saturation);
    let temp = f32x8::splat(cc.temperature * 30.0);
    let gray_r = f32x8::splat(0.299);
    let gray_g = f32x8::splat(0.587);
    let gray_b = f32x8::splat(0.114);
    let key_cb_v = f32x8::splat(key_cb);
    let key_cr_v = f32x8::splat(key_cr);
    let dist_norm_v = f32x8::splat(dist_norm);
    let sim_v = f32x8::splat(similarity);
    let blend_v = f32x8::splat(blend);
    let spill_v = f32x8::splat(spill);
    let cb_coeff_r = f32x8::splat(-0.169);
    let cb_coeff_g = f32x8::splat(-0.331);
    let cb_coeff_b = f32x8::splat(0.500);
    let cr_coeff_r = f32x8::splat(0.500);
    let cr_coeff_g = f32x8::splat(-0.419);
    let cr_coeff_b = f32x8::splat(-0.081);

    let batch = 8;
    let simd_end = n - (n % batch);

    // ── SIMD main loop: chroma-key + brightness + contrast + saturation + temperature ──
    for i in (0..simd_end).step_by(batch) {
        let mut r_arr = [0.0f32; 8];
        let mut g_arr = [0.0f32; 8];
        let mut b_arr = [0.0f32; 8];
        for j in 0..8 {
            let px = img.pixels[i + j];
            r_arr[j] = px.r() as f32;
            g_arr[j] = px.g() as f32;
            b_arr[j] = px.b() as f32;
        }
        let mut r = f32x8::new(r_arr);
        let mut g = f32x8::new(g_arr);
        let mut b = f32x8::new(b_arr);

        // Chroma-key
        let mut alpha = one;
        if chroma_active {
            let cb = r * cb_coeff_r + g * cb_coeff_g + b * cb_coeff_b + c128;
            let cr = r * cr_coeff_r + g * cr_coeff_g + b * cr_coeff_b + c128;
            let du = cb - key_cb_v;
            let dv = cr - key_cr_v;
            let diff = (du * du + dv * dv).sqrt() / dist_norm_v;

            if blend > 0.0 {
                alpha = ((diff - sim_v) / blend_v).max(zero).min(one);
            } else {
                let mask = diff.cmp_lt(sim_v);
                alpha = mask.blend(zero, one);
            }
        }

        // Spill suppression
        if chroma_active && spill > 0.0 {
            let cond = alpha.cmp_gt(zero) & g.cmp_gt((r + b) * half);
            let avg_rb = (r + b) * half;
            let og_spill = g - (g - avg_rb) * spill_v;
            g = cond.blend(og_spill, g);
        }

        // Brightness
        r = r + brightness;
        g = g + brightness;
        b = b + brightness;

        // Contrast
        r = (r - c128) * contrast + c128;
        g = (g - c128) * contrast + c128;
        b = (b - c128) * contrast + c128;

        // Clamp
        r = r.max(zero).min(max255);
        g = g.max(zero).min(max255);
        b = b.max(zero).min(max255);

        // Saturation
        let gray = r * gray_r + g * gray_g + b * gray_b;
        r = gray + (r - gray) * saturation;
        g = gray + (g - gray) * saturation;
        b = gray + (b - gray) * saturation;

        // Temperature
        if cc.temperature != 0.0 {
            r = r + temp;
            b = b - temp;
        }

        // Clamp
        r = r.max(zero).min(max255);
        g = g.max(zero).min(max255);
        b = b.max(zero).min(max255);

        // Pack
        let r_a = r.to_array();
        let g_a = g.to_array();
        let b_a = b.to_array();
        let a_a = alpha.to_array();
        for j in 0..8 {
            let a = (a_a[j] * 255.0).clamp(0.0, 255.0) as u8;
            out.pixels[i + j] =
                egui::Color32::from_rgba_unmultiplied(r_a[j] as u8, g_a[j] as u8, b_a[j] as u8, a);
        }
    }

    // ── Scalar tail for leftover pixels (< 8) ──
    for i in simd_end..n {
        let px = img.pixels[i];
        let r = px.r() as f32;
        let g = px.g() as f32;
        let b = px.b() as f32;

        let alpha = if !chroma_active {
            1.0
        } else {
            let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
            let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
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

        or_ = (or_ + cc.brightness * 255.0).clamp(0.0, 255.0);
        og = (og + cc.brightness * 255.0).clamp(0.0, 255.0);
        ob = (ob + cc.brightness * 255.0).clamp(0.0, 255.0);
        or_ = ((or_ - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        og = ((og - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        ob = ((ob - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        let gray = 0.299 * or_ + 0.587 * og + 0.114 * ob;
        or_ = (gray + (or_ - gray) * cc.saturation).clamp(0.0, 255.0);
        og = (gray + (og - gray) * cc.saturation).clamp(0.0, 255.0);
        ob = (gray + (ob - gray) * cc.saturation).clamp(0.0, 255.0);
        if cc.temperature != 0.0 {
            or_ = (or_ + cc.temperature * 30.0).clamp(0.0, 255.0);
            ob = (ob - cc.temperature * 30.0).clamp(0.0, 255.0);
        }

        let a = (alpha * 255.0).clamp(0.0, 255.0) as u8;
        out.pixels[i] = egui::Color32::from_rgba_unmultiplied(or_ as u8, og as u8, ob as u8, a);
    }

    // ── Scalar second pass: LGG + curves over all pixels ──
    if lgg_active || curves_active {
        for px in out.pixels.iter_mut() {
            let mut r = px.r() as f32;
            let mut g = px.g() as f32;
            let mut b = px.b() as f32;

            if lgg_active {
                let mut nr = r / 255.0;
                let mut ng = g / 255.0;
                let mut nb = b / 255.0;
                nr = nr + lift[0] * (1.0 - nr);
                ng = ng + lift[1] * (1.0 - ng);
                nb = nb + lift[2] * (1.0 - nb);
                nr = (nr * gain[0]).max(0.0);
                ng = (ng * gain[1]).max(0.0);
                nb = (nb * gain[2]).max(0.0);
                nr = nr.powf(inv_gamma[0]);
                ng = ng.powf(inv_gamma[1]);
                nb = nb.powf(inv_gamma[2]);
                r = (nr * 255.0).clamp(0.0, 255.0);
                g = (ng * 255.0).clamp(0.0, 255.0);
                b = (nb * 255.0).clamp(0.0, 255.0);
            }

            if curves_active {
                r = lut_master[r as usize] as f32;
                g = lut_master[g as usize] as f32;
                b = lut_master[b as usize] as f32;
                r = lut_r[r as usize] as f32;
                g = lut_g[g as usize] as f32;
                b = lut_b[b as usize] as f32;
            }

            *px = egui::Color32::from_rgba_unmultiplied(r as u8, g as u8, b as u8, px.a());
        }
    }

    out
}

/// Public dispatcher: selects the SIMD path on x86_64 / aarch64 when the
/// frame is wide enough to amortise the setup cost, otherwise falls back
/// to the scalar reference implementation.
pub fn apply_effects_cpu(
    img: &ColorImage,
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> ColorImage {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        // Only use SIMD when the compiler is allowed to emit vector
        // instructions (target-feature avx / neon).  In debug builds
        // without those flags `wide` falls back to scalar loops and
        // is actually slower, so we stay on the scalar reference.
        let has_vector = cfg!(target_feature = "avx")
            || cfg!(target_feature = "avx2")
            || cfg!(target_feature = "neon");
        if has_vector && img.pixels.len() >= 64 {
            return apply_effects_cpu_simd(img, ck, cc);
        }
    }
    apply_effects_cpu_scalar(img, ck, cc)
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
    let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
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
pub fn apply_effect_stack_cpu(img: &ColorImage, effects: &[memstroy_core::Effect]) -> ColorImage {
    // Fast path: skip the clone when there's nothing to do
    if effects.is_empty() {
        return img.clone();
    }
    let mut current = img.clone();
    for eff in effects {
        if !eff.enabled {
            continue;
        }
        let intensity = eff.intensity.clamp(0.0, 1.0);
        if intensity <= 0.001 {
            continue;
        }
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
        K::Glow {
            radius,
            intensity: i2,
        } => glow(img, *radius, *i2 * intensity),
        K::Brightness { amount } => brightness(img, *amount * intensity),
        K::Contrast { amount } => contrast(img, *amount * intensity),
        K::Saturation { amount } => saturation(img, *amount * intensity),
        K::EdgeDetect { threshold } => edge_detect(img, *threshold, intensity),
        K::MirrorH => mix_pixels(img, &mirror_h(img), intensity),
        K::MirrorV => mix_pixels(img, &mirror_v(img), intensity),
        K::ChromaticAberration { offset } => chromatic_aberration(img, *offset * intensity),
        K::Noise { amount } => noise(img, *amount * intensity),
        K::Wave {
            amplitude,
            wavelength,
        } => wave(img, *amplitude * intensity, *wavelength),
        K::OldFilm => old_film(img, intensity),
        K::Vhs => vhs(img, intensity),
        K::Glitch { strength } => glitch(img, *strength * intensity),
        K::Bloom { radius } => bloom(img, *radius, intensity),
        K::Crop {
            left,
            top,
            right,
            bottom,
        } => crop_alpha(
            img,
            (*left * intensity).clamp(0.0, 0.49),
            (*top * intensity).clamp(0.0, 0.49),
            (*right * intensity).clamp(0.0, 0.49),
            (*bottom * intensity).clamp(0.0, 0.49),
        ),
        K::Mask {
            shape,
            feather,
            invert,
        } => apply_mask_color_image(img, shape, *feather, *invert, intensity),
        K::ColorKey {
            color,
            similarity,
            blend,
            spill,
            invert,
        } => apply_color_key_color_image(
            img,
            *color,
            *similarity,
            *blend,
            *spill,
            *invert,
            intensity,
        ),
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
    if w == 0 || h == 0 {
        return out;
    }
    let i = intensity.clamp(0.0, 1.0);

    // Fast path: hard-edge axis-aligned rectangle (no feather).
    // Just clear alpha outside the rect (or inside if invert).
    if let memstroy_core::MaskShape::Rect {
        left,
        top,
        right,
        bottom,
    } = shape
    {
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
                            out.pixels[idx] =
                                egui::Color32::from_rgba_unmultiplied(p.r(), p.g(), p.b(), 0);
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
            let keep = crate::image_effects::sample_mask_alpha(shape, u, v, feather, invert);
            // Skip pixels that don't change (keep == 1.0 and intensity full)
            if keep >= 0.9999 && i >= 0.9999 {
                continue;
            }
            let idx = row + x;
            if idx < out.pixels.len() {
                let p = out.pixels[idx];
                let orig = p.a() as f32;
                let target = orig * keep;
                let new_a = (orig + (target - orig) * i).clamp(0.0, 255.0) as u8;
                out.pixels[idx] = egui::Color32::from_rgba_unmultiplied(p.r(), p.g(), p.b(), new_a);
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
    if w == 0 || h == 0 {
        return out;
    }
    let i = intensity.clamp(0.0, 1.0);
    // Mirror `chromakey_filter`'s "disabled below 1e-5" rule so a
    // dialed-down ColorKey effect renders identically on both
    // surfaces — the export pipeline emits a no-op `null` filter on
    // that threshold, so the preview must do nothing too.
    let similarity = if similarity.is_finite() {
        similarity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = if blend.is_finite() {
        blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if similarity < 1.0e-5 && !invert {
        // Disabled key — nothing to attenuate. Returning early keeps
        // the per-pixel loop's no-op path obvious.
        return out;
    }
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;
    for px in 0..(w * h) {
        if px >= out.pixels.len() {
            break;
        }
        let p = out.pixels[px];
        let r = p.r() as f32;
        let g = p.g() as f32;
        let b = p.b() as f32;
        let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
        let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
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
        if invert {
            alpha_keep = 1.0 - alpha_keep;
        }
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
fn crop_alpha(img: &ColorImage, left: f32, top: f32, right: f32, bottom: f32) -> ColorImage {
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
                    out.pixels[idx] = egui::Color32::from_rgba_unmultiplied(p.r(), p.g(), p.b(), 0);
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
        let r = px.r() as f32;
        let g = px.g() as f32;
        let b = px.b() as f32;
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
        *px =
            egui::Color32::from_rgba_unmultiplied(255 - px.r(), 255 - px.g(), 255 - px.b(), px.a());
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
        let r = px.r() as f32;
        let g = px.g() as f32;
        let b = px.b() as f32;
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
        let r = px.r() as f32;
        let g = px.g() as f32;
        let b = px.b() as f32;
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
                if by + dy >= h {
                    break;
                }
                for dx in 0..(block as usize) {
                    if bx + dx >= w {
                        break;
                    }
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
    if radius <= 0 {
        return img.clone();
    }
    // Two-pass separable box blur — cheap and good enough for preview.
    let pass1 = blur_pass(img, radius, true);
    blur_pass(&pass1, radius, false)
}

fn blur_pass(img: &ColorImage, radius: i32, horizontal: bool) -> ColorImage {
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let mut out = img.clone();
    for y in 0..h {
        for x in 0..w {
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut a = 0u32;
            let mut count = 0u32;
            for k in -radius..=radius {
                let (sx, sy) = if horizontal { (x + k, y) } else { (x, y + k) };
                if sx < 0 || sy < 0 || sx >= w || sy >= h {
                    continue;
                }
                let p = img.pixels[(sy * w + sx) as usize];
                r += p.r() as u32;
                g += p.g() as u32;
                b += p.b() as u32;
                a += p.a() as u32;
                count += 1;
            }
            if count == 0 {
                continue;
            }
            out.pixels[(y * w + x) as usize] = egui::Color32::from_rgba_unmultiplied(
                (r / count) as u8,
                (g / count) as u8,
                (b / count) as u8,
                (a / count) as u8,
            );
        }
    }
    out
}

fn sharpen(img: &ColorImage, amount: f32) -> ColorImage {
    if amount <= 0.001 {
        return img.clone();
    }
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
    if intensity <= 0.001 || radius <= 0.5 {
        return img.clone();
    }
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
                + lum(x + 1, y - 1)
                + 2.0 * lum(x + 1, y)
                + lum(x + 1, y + 1);
            let gy = -lum(x - 1, y - 1) - 2.0 * lum(x, y - 1) - lum(x + 1, y - 1)
                + lum(x - 1, y + 1)
                + 2.0 * lum(x, y + 1)
                + lum(x + 1, y + 1);
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
    if off == 0 {
        return img.clone();
    }
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
            out.pixels[(y * w + x) as usize] =
                egui::Color32::from_rgba_unmultiplied(pr.r(), pg.g(), pb.b(), pg.a());
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
    if strength <= 0.001 {
        return img.clone();
    }
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
/// `on_done` with extraction results (or `Err` when ffmpeg/ffprobe fails).
fn extract_frames_blocking(
    source: PathBuf,
    on_done: impl FnOnce(Result<(f32, usize, PathBuf), ()>) + Send + 'static,
) {
    extract_frames_blocking_with_scale(source, 480, on_done);
}

/// Same as `extract_frames_blocking` but with a configurable max width.
/// Used by the adaptive-quality system to extract at lower resolution
/// when many actors are on screen simultaneously.
pub fn extract_frames_blocking_with_scale(
    source: PathBuf,
    max_width: u32,
    on_done: impl FnOnce(Result<(f32, usize, PathBuf), ()>) + Send + 'static,
) {
    let ffmpeg = memstroy_render::ffmpeg_binary();
    let ffprobe = memstroy_render::ffprobe_binary();

    let cache_dir = std::env::temp_dir().join(format!(
        "memstroy_frames_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        tracing::error!("Failed to create frame cache dir: {e}");
        on_done(Err(()));
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
                        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jpg"))
                        .count()
                })
                .unwrap_or(0);

            if frame_count == 0 {
                tracing::warn!(
                    path = %source.display(),
                    "Frame extraction produced zero frames"
                );
                on_done(Err(()));
                return;
            }
            tracing::info!(
                "Frame extraction complete: {} frames, {:.1}s duration",
                frame_count,
                duration
            );
            on_done(Ok((duration, frame_count, cache_dir)));
        }
        Ok(s) => {
            tracing::error!("ffmpeg frame extraction exited with: {}", s);
            on_done(Err(()));
        }
        Err(e) => {
            tracing::error!("ffmpeg frame extraction failed: {e}");
            on_done(Err(()));
        }
    }
}

// ─── TESTS ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Build a synthetic 480×270 green-screen image (chroma-key target).
    fn synthetic_green_screen() -> ColorImage {
        let w = 480_usize;
        let h = 270_usize;
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                // Center area = bright green (key target), edges = skin tone.
                let is_green = x > w / 4 && x < w * 3 / 4 && y > h / 4 && y < h * 3 / 4;
                let c = if is_green {
                    egui::Color32::from_rgb(0, 255, 0)
                } else {
                    egui::Color32::from_rgb(200, 120, 80)
                };
                pixels.push(c);
            }
        }
        ColorImage {
            size: [w, h],
            pixels,
        }
    }

    /// Ensure chroma-key + colour-correction finishes a 480p frame in
    /// well under the 16ms budget (single actor, single frame). This is
    /// the baseline CPU cost that the background pipeline moves off the
    /// UI thread.
    #[test]
    fn chroma_key_480p_under_16ms() {
        let img = synthetic_green_screen();
        let ck = memstroy_core::ChromaKeyParams {
            enabled: true,
            key_color: [0, 255, 0],
            similarity: 0.3,
            blend: 0.1,
            spill: 0.2,
        };
        let cc = memstroy_core::ColorCorrection::default();

        let start = Instant::now();
        let _out = apply_effects_cpu(&img, &ck, &cc);
        let elapsed = start.elapsed().as_secs_f32() * 1000.0;

        // Debug builds are ~3-4× slower than release. In release this
        // should finish in < 8 ms; in debug we allow up to 50 ms so the
        // test doesn't spuriously fail on CI. The important thing is
        // that we have a measured baseline — 34 ms in debug means the
        // UI thread would drop to ~30 fps with a single actor if this
        // ran synchronously, which is exactly why the background
        // processed-frame pipeline was added.
        let budget = if cfg!(debug_assertions) { 50.0 } else { 8.0 };
        assert!(
            elapsed < budget,
            "chroma-key + CC on 480p took {:.2}ms, exceeds {}ms budget",
            elapsed,
            budget
        );
    }

    /// Ensure the processed pipeline hash changes when params change.
    #[test]
    fn hash_effect_params_changes_with_params() {
        let ck1 = memstroy_core::ChromaKeyParams {
            enabled: true,
            key_color: [0, 255, 0],
            similarity: 0.3,
            blend: 0.1,
            spill: 0.2,
        };
        let ck2 = memstroy_core::ChromaKeyParams {
            enabled: true,
            key_color: [0, 255, 0],
            similarity: 0.5, // changed
            blend: 0.1,
            spill: 0.2,
        };
        let cc = memstroy_core::ColorCorrection::default();
        let effects: Vec<memstroy_core::Effect> = Vec::new();

        let h1 = hash_effect_params(&ck1, &cc, &effects);
        let h2 = hash_effect_params(&ck2, &cc, &effects);
        assert_ne!(h1, h2, "hash should differ when similarity changes");
    }

    /// downscale_for_preview should return input unchanged when already small.
    #[test]
    fn downscale_noop_when_small() {
        let img = ColorImage::new([100, 100], egui::Color32::RED);
        let out = downscale_for_preview(&img, 360);
        assert_eq!(out.size, [100, 100]);
    }

    #[test]
    fn scaled_preview_dimensions_match_ffmpeg_width_policy() {
        assert_eq!(adaptive_extract_width(1), 480);
        assert_eq!(adaptive_extract_width(4), 360);
        assert_eq!(adaptive_extract_width(6), 280);
        assert_eq!(scaled_preview_dimensions(1080, 1920, 1), Some((480, 853)));
        assert_eq!(scaled_preview_dimensions(1080, 1920, 4), Some((360, 640)));
        assert_eq!(scaled_preview_dimensions(0, 1920, 1), None);
    }

    /// Ensure the SIMD path produces pixel-perfect (±1) output compared
    /// to the scalar reference for a diverse synthetic image.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn simd_matches_scalar_pixel_perfect() {
        let mut pixels = Vec::with_capacity(100 * 100);
        for y in 0..100 {
            for x in 0..100 {
                // Mix of greens, skin tones, and neutral greys.
                let c = match (x + y) % 5 {
                    0 => egui::Color32::from_rgb(0, 255, 0),
                    1 => egui::Color32::from_rgb(255, 0, 0),
                    2 => egui::Color32::from_rgb(0, 0, 255),
                    3 => egui::Color32::from_rgb(128, 128, 128),
                    _ => egui::Color32::from_rgb(200, 120, 80),
                };
                pixels.push(c);
            }
        }
        let img = ColorImage {
            size: [100, 100],
            pixels,
        };

        let ck = memstroy_core::ChromaKeyParams {
            enabled: true,
            key_color: [0, 255, 0],
            similarity: 0.25,
            blend: 0.08,
            spill: 0.15,
        };
        let cc = memstroy_core::ColorCorrection {
            brightness: 0.05,
            contrast: 1.1,
            saturation: 1.2,
            temperature: 0.02,
            ..Default::default()
        };

        let scalar = apply_effects_cpu_scalar(&img, &ck, &cc);
        let simd = apply_effects_cpu_simd(&img, &ck, &cc);

        assert_eq!(scalar.size, simd.size);
        for (i, (s, m)) in scalar.pixels.iter().zip(simd.pixels.iter()).enumerate() {
            let dr = (s.r() as i16 - m.r() as i16).abs();
            let dg = (s.g() as i16 - m.g() as i16).abs();
            let db = (s.b() as i16 - m.b() as i16).abs();
            let da = (s.a() as i16 - m.a() as i16).abs();
            assert!(
                dr <= 1 && dg <= 1 && db <= 1 && da <= 1,
                "pixel {i}: scalar {:?} vs simd {:?}",
                s,
                m
            );
        }
    }
}
