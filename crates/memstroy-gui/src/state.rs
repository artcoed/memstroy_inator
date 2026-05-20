use std::path::PathBuf;

use memstroy_core::Scene;

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
}

#[derive(Default)]
pub struct AssetLibrary {
    pub mellstroy_clips: Vec<PathBuf>,
    pub backgrounds: Vec<PathBuf>,
    pub props: Vec<PathBuf>,
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
        s.status = "Ready.".into();
        s
    }
}
