use std::path::PathBuf;

use memstroy_core::Scene;
use memstroy_tg::model::DownloadState;

use crate::undo::UndoStack;

/// Editor-side selection state.
#[derive(Default)]
pub struct EditorState {
    pub scene: Scene,
    pub scene_path: Option<PathBuf>,
    pub assets_root: PathBuf,
    pub library: AssetLibrary,
    pub selection: Selection,
    pub playhead: f32,
    pub status: String,
    pub last_preview: Option<PathBuf>,
    pub render_progress: Option<RenderProgress>,
    pub refreshing: bool,
    pub undo: UndoStack,
    /// Drag state for timeline interactions.
    pub _drag: DragState,
    /// Playback state
    pub playing: bool,
    /// Playback speed multiplier (1.0 = normal, 2.0 = 2x, 0.5 = half)
    pub playback_speed: f32,
    /// Last playhead value that was rendered as preview (for auto-preview debounce)
    pub last_rendered_playhead: f32,
    /// Whether a preview render is currently in-flight
    pub preview_rendering: bool,
    /// Timeline zoom level (1.0 = full duration visible, 2.0 = zoomed 2x)
    pub timeline_zoom: f32,
    /// Timeline scroll offset (normalised 0..1)
    pub timeline_scroll: f32,
    /// Whether the (scaffold) node editor window is open.
    pub node_editor_open: bool,
    /// Library search filter text.
    pub library_search: String,
    /// Whether ffmpeg is available (checked once at startup).
    pub ffmpeg_available: bool,
    /// Razor tool mode: when active, clicking a track bar splits at click position.
    pub razor_mode: bool,
}

#[derive(Default)]
pub struct AssetLibrary {
    pub mellstroy_clips: Vec<LibraryClip>,
    pub backgrounds: Vec<PathBuf>,
    pub props: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LibraryClip {
    pub id: u64,
    pub path: PathBuf,
    pub description: String,
    pub downloaded: bool,
    pub thumbnail: Option<PathBuf>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Selection {
    #[default]
    None,
    Actor(usize),
    Overlay(usize),
    Background(usize),
    Camera(usize),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RenderProgress {
    pub started: std::time::Instant,
    pub last_log: String,
    pub done: bool,
    pub error: Option<String>,
}

/// What's currently being dragged in the timeline.
#[derive(Default, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum DragState {
    #[default]
    None,
    /// Dragging a background segment's start time.
    BackgroundStart(usize),
    /// Dragging a background segment's end (duration).
    BackgroundEnd(usize),
    /// Moving a background segment (both start & end).
    BackgroundMove(usize, f32), // index, original_start
    /// Moving an actor's t_in/t_out window.
    ActorMove(usize),
    /// Moving an overlay's time window.
    OverlayMove(usize),
}

impl EditorState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.assets_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        s.scene = Scene::default();
        s.status = "Ready".into();
        s.playback_speed = 1.0;
        s.timeline_zoom = 1.0;
        s.last_rendered_playhead = -1.0; // force first render
        s.ffmpeg_available = check_ffmpeg();
        s.razor_mode = false;
        s
    }

    pub fn clips_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("mellstroy")
    }

    pub fn state_path(&self) -> PathBuf {
        self.clips_dir().join("state.json")
    }

    /// Save undo snapshot, then apply a mutation via the closure.
    pub fn mutate(&mut self, f: impl FnOnce(&mut Scene)) {
        self.undo.push(&self.scene);
        f(&mut self.scene);
    }

    /// Undo the last action.
    pub fn undo(&mut self) {
        if let Some(prev) = self.undo.undo(&self.scene) {
            self.scene = prev;
            self.status = "\u{21A9} Undo".into();
        }
    }

    /// Redo the last undone action.
    pub fn redo(&mut self) {
        if let Some(next) = self.undo.redo(&self.scene) {
            self.scene = next;
            self.status = "\u{21AA} Redo".into();
        }
    }

    pub fn reload_library(&mut self) {
        let state = DownloadState::load(&self.state_path());
        let clips_dir = self.clips_dir();

        self.library.mellstroy_clips = state
            .all_clips_sorted()
            .into_iter()
            .filter(|c| c.downloaded)
            .map(|c| {
                let thumb_path = clips_dir.join("thumbs").join(format!("{}.jpg", c.id));
                let thumbnail = if thumb_path.exists() { Some(thumb_path) } else { None };
                LibraryClip {
                    id: c.id,
                    path: clips_dir.join(&c.filename),
                    description: c.description.clone(),
                    downloaded: c.downloaded,
                    thumbnail,
                }
            })
            .collect();

        self.library.mellstroy_clips.sort_by_key(|c| c.id);

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

/// Check if ffmpeg binary is accessible.
fn check_ffmpeg() -> bool {
    std::process::Command::new(memstroy_render::ffmpeg_binary())
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
