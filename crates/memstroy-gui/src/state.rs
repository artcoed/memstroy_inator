use std::path::PathBuf;

use memstroy_core::Scene;
use memstroy_tg::model::{ClipEntry, DownloadState};

/// Editor-side selection state. The GUI mutates the scene through
/// these handles.
#[derive(Default)]
pub struct EditorState {
    pub scene: Scene,
    pub scene_path: Option<PathBuf>,
    pub assets_root: PathBuf,
    pub library: AssetLibrary,
    pub selection: Selection,
    /// Current playhead time (seconds).
    pub playhead: f32,
    pub status: String,
    pub last_preview: Option<PathBuf>,
    pub render_progress: Option<RenderProgress>,
    /// Whether a download/refresh job is currently running.
    pub refreshing: bool,
}

#[derive(Default)]
pub struct AssetLibrary {
    /// Mellstroy clips with metadata from state.json
    pub mellstroy_clips: Vec<LibraryClip>,
    pub backgrounds: Vec<PathBuf>,
    pub props: Vec<PathBuf>,
}

/// A clip in the library with its metadata.
#[derive(Debug, Clone)]
pub struct LibraryClip {
    pub id: u64,
    pub path: PathBuf,
    pub description: String,
    pub downloaded: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    #[default]
    None,
    Actor(usize),
    Overlay(usize),
    Background(usize),
    Camera(usize),
}

#[derive(Debug, Clone)]
pub struct RenderProgress {
    pub started: std::time::Instant,
    pub last_log: String,
    pub done: bool,
    pub error: Option<String>,
}

impl EditorState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.assets_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        s.scene = Scene::default();
        s.status = "Ready. Click \u{1F504} Refresh Clips to download mellstroy footage!".into();
        s
    }

    /// Path to the clips directory.
    pub fn clips_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("mellstroy")
    }

    /// Path to the download state file.
    pub fn state_path(&self) -> PathBuf {
        self.clips_dir().join("state.json")
    }

    /// Reload library from download state on disk.
    pub fn reload_library(&mut self) {
        let state = DownloadState::load(&self.state_path());
        let clips_dir = self.clips_dir();

        self.library.mellstroy_clips = state
            .all_clips_sorted()
            .into_iter()
            .filter(|c| c.downloaded)
            .map(|c| LibraryClip {
                id: c.id,
                path: clips_dir.join(&c.filename),
                description: c.description.clone(),
                downloaded: c.downloaded,
            })
            .collect();

        // Sort by id ascending (oldest first)
        self.library.mellstroy_clips.sort_by_key(|c| c.id);

        // Scan backgrounds and props
        self.library.backgrounds =
            scan_dir(&self.assets_root.join("assets/backgrounds"), &["mp4", "mov", "webm", "jpg", "jpeg", "png", "webp"]);
        self.library.props =
            scan_dir(&self.assets_root.join("assets/props"), &["png", "webp", "svg"]);
    }
}

fn scan_dir(dir: &std::path::Path, exts: &[&str]) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if exts.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
