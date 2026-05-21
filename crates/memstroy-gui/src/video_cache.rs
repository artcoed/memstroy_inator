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
