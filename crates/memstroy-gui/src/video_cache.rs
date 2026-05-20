//! Frame-cache for real-time video preview.
//! Extracts frames via ffmpeg CLI, loads them as textures on demand.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use egui::TextureHandle;
use tokio::runtime::Handle;

/// Maximum number of decoded textures kept in memory.
const MAX_TEXTURES: usize = 10;

/// Frame-cache: extracts video frames to disk via ffmpeg, then loads
/// individual JPEG frames on demand and caches them as egui textures.
pub struct FrameCache {
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
    /// LRU cache of loaded textures keyed by frame index.
    textures: HashMap<usize, TextureHandle>,
    /// Access order for LRU eviction (front = oldest).
    last_accessed: VecDeque<usize>,
}

impl FrameCache {
    /// Create a new empty frame cache (not yet ready).
    pub fn new(source: PathBuf) -> Self {
        Self {
            cache_dir: PathBuf::new(),
            source,
            fps: 30.0,
            frame_count: 0,
            duration: 0.0,
            ready: false,
            extracting: false,
            textures: HashMap::new(),
            last_accessed: VecDeque::new(),
        }
    }

    /// Start frame extraction in the background via tokio.
    ///
    /// Spawns ffprobe to get duration, then ffmpeg to extract frames at 30fps/640px.
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

            // Extract frames
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
                    "-vf", "fps=30,scale=640:-1",
                    "-q:v", "4",
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

    /// Mark the cache as ready with extraction results.
    pub fn set_ready(&mut self, duration: f32, frame_count: usize, cache_dir: PathBuf) {
        self.duration = duration;
        self.frame_count = frame_count;
        self.cache_dir = cache_dir;
        self.ready = true;
        self.extracting = false;
    }

    /// Whether the cache has extracted frames and is ready for use.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Get the texture for a given time `t` in seconds.
    /// Loads from disk and caches if not already in memory.
    /// Returns `None` if the cache is not ready or frame cannot be loaded.
    pub fn frame_at_time(&mut self, t: f32, ctx: &egui::Context) -> Option<&TextureHandle> {
        if !self.ready || self.frame_count == 0 {
            return None;
        }

        // Compute frame index (1-based file naming)
        let frame_index = ((t * self.fps).floor() as usize).clamp(0, self.frame_count.saturating_sub(1));

        // Check if already cached
        if self.textures.contains_key(&frame_index) {
            // Move to back of LRU
            self.last_accessed.retain(|&x| x != frame_index);
            self.last_accessed.push_back(frame_index);
            return self.textures.get(&frame_index);
        }

        // Load from disk
        let file_name = format!("{:06}.jpg", frame_index + 1);
        let frame_path = self.cache_dir.join(&file_name);

        let img = match image::open(&frame_path) {
            Ok(img) => img.to_rgba8(),
            Err(_) => return None,
        };

        let size = [img.width() as usize, img.height() as usize];
        let pixels = img.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);

        let texture = ctx.load_texture(
            format!("frame_{}", frame_index),
            color_image,
            egui::TextureOptions::LINEAR,
        );

        // Evict oldest if at capacity
        while self.textures.len() >= MAX_TEXTURES {
            if let Some(oldest) = self.last_accessed.pop_front() {
                self.textures.remove(&oldest);
            } else {
                break;
            }
        }

        self.textures.insert(frame_index, texture);
        self.last_accessed.push_back(frame_index);

        self.textures.get(&frame_index)
    }
}
