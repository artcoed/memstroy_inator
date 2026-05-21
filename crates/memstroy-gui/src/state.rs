use std::path::PathBuf;

use memstroy_core::Scene;
use memstroy_tg::model::DownloadState;

use crate::undo::UndoStack;

/// Fixed track in the timeline. Tracks are numbered lanes; clips sit on them.
#[derive(Debug, Clone)]
pub struct Track {
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub locked: bool,
    pub visible: bool,
    /// Height in pixels (can be resized).
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

impl Track {
    pub fn video(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: TrackKind::Video,
            muted: false,
            locked: false,
            visible: true,
            height: 40.0,
        }
    }
    pub fn audio(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: TrackKind::Audio,
            muted: false,
            locked: false,
            visible: true,
            height: 48.0,
        }
    }
}

/// A clip placed on a track at a specific time.
#[derive(Debug, Clone)]
pub struct TimelineClip {
    /// Which track index this clip is on.
    pub track_index: usize,
    /// What scene element this clip represents.
    pub element: ClipElement,
    /// Start time on the timeline (seconds).
    pub start: f32,
    /// Duration on the timeline (seconds).
    pub duration: f32,
    /// Offset into the source media (for trimmed clips).
    pub source_offset: f32,
    /// Color for the clip bar.
    pub color: [u8; 3],
}

/// What scene element a timeline clip corresponds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipElement {
    Actor(usize),
    Overlay(usize),
    Background(usize),
    Audio(usize),
}

/// Drag-and-drop state for timeline clips.
#[derive(Default, Clone)]
pub struct TimelineDrag {
    /// Which clip is being dragged (by its index in timeline_clips).
    pub dragging_clip: Option<usize>,
    /// Original track when drag started.
    pub original_track: usize,
    /// Original start time when drag started.
    pub original_start: f32,
    /// Accumulated drag delta in pixels.
    pub drag_delta_x: f32,
    pub drag_delta_y: f32,
}

/// Drag-and-drop state for asset library items.
#[derive(Default, Clone)]
pub struct AssetDrag {
    /// Path of the asset being dragged from library.
    pub dragging: Option<PathBuf>,
    /// Kind of asset being dragged.
    pub kind: AssetDragKind,
    /// Current mouse position during drag.
    pub pos: [f32; 2],
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum AssetDragKind {
    #[default]
    None,
    Clip,
    Background,
    Prop,
    Audio,
}

/// Audio waveform data for visualization.
#[derive(Debug, Clone)]
pub struct AudioWaveform {
    /// Peak amplitudes (0.0..1.0) sampled at regular intervals.
    pub peaks: Vec<f32>,
    /// Duration of the audio file in seconds.
    pub duration: f32,
    /// Whether waveform extraction is complete.
    pub ready: bool,
    /// Whether extraction is currently running.
    pub extracting: bool,
}

impl Default for AudioWaveform {
    fn default() -> Self {
        Self { peaks: Vec::new(), duration: 0.0, ready: false, extracting: false }
    }
}

impl AudioWaveform {
    /// Extract audio peaks from a file using ffmpeg. Returns peaks vector.
    /// This runs synchronously and should be called from a background thread.
    pub fn extract_peaks(audio_path: &std::path::Path, num_samples: usize) -> Option<(Vec<f32>, f32)> {
        let ffprobe = {
            let mut p = memstroy_render::ffmpeg_binary();
            p.set_file_name("ffprobe");
            if !p.exists() { std::path::PathBuf::from("ffprobe") } else { p }
        };

        // Get duration
        let duration = match std::process::Command::new(&ffprobe)
            .args(["-v", "error", "-show_entries", "format=duration",
                   "-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(audio_path)
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().unwrap_or(0.0),
            Err(_) => return None,
        };

        if duration <= 0.0 { return None; }

        // Extract raw PCM samples via ffmpeg, downsample to mono 8kHz
        let ffmpeg = memstroy_render::ffmpeg_binary();
        let output = std::process::Command::new(&ffmpeg)
            .args(["-y", "-hide_banner", "-loglevel", "error",
                   "-i"])
            .arg(audio_path)
            .args(["-ac", "1", "-ar", "8000", "-f", "s16le", "-"])
            .output();

        let raw = match output {
            Ok(o) if o.status.success() => o.stdout,
            _ => return None,
        };

        // Convert i16 PCM to peaks
        let samples: Vec<i16> = raw.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        if samples.is_empty() { return None; }

        let chunk_size = (samples.len() / num_samples).max(1);
        let peaks: Vec<f32> = samples.chunks(chunk_size)
            .map(|chunk| {
                let max_val = chunk.iter().map(|s| s.unsigned_abs() as f32).fold(0.0_f32, f32::max);
                (max_val / 32768.0).clamp(0.0, 1.0)
            })
            .collect();

        Some((peaks, duration))
    }
}

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
    /// Playback state
    pub playing: bool,
    /// Playback speed multiplier (1.0 = normal, 2.0 = 2x, 0.5 = half)
    pub playback_speed: f32,
    /// Last playhead value that was rendered as preview (for auto-preview debounce)
    pub last_rendered_playhead: f32,
    /// Whether a preview render is currently in-flight
    pub preview_rendering: bool,
    /// Timeline zoom level (pixels per second)
    pub timeline_zoom: f32,
    /// Timeline horizontal scroll offset in seconds
    pub timeline_scroll: f32,
    /// Whether the (scaffold) node editor window is open.
    pub node_editor_open: bool,
    /// Library search filter text.
    pub library_search: String,
    /// Whether ffmpeg is available (checked once at startup).
    pub ffmpeg_available: bool,
    /// Razor tool mode: when active, clicking a track bar splits at click position.
    pub razor_mode: bool,
    /// Per-actor frame caches for real-time video preview. Key = actor index.
    pub frame_caches: Vec<crate::video_cache::FrameCache>,
    /// Eyedropper mode: when true, clicking on preview picks the pixel color for chroma-key.
    pub eyedropper_active: bool,
    /// Whether the Assets tab is active (vs Clips tab) in the left panel.
    pub assets_tab_active: bool,

    // ─── NEW: Premiere Pro-style timeline ───
    /// Fixed tracks (lanes). Clips are placed on tracks.
    pub tracks: Vec<Track>,
    /// Timeline drag state for clip movement between tracks.
    pub timeline_drag: TimelineDrag,
    /// Asset drag from library to timeline.
    pub asset_drag: AssetDrag,
    /// Audio waveforms keyed by audio track index.
    pub audio_waveforms: Vec<AudioWaveform>,
    /// Whether snapping is enabled (clips snap to playhead, other clip edges).
    pub snap_enabled: bool,
    /// Inspector tab: 0=Transform, 1=Timing, 2=Effects
    pub inspector_tab: usize,
    /// Multi-selection: additional actor indices selected via Ctrl+click.
    /// The primary `selection` field still tracks the "focused" element for the inspector.
    pub multi_select: Vec<usize>,

    /// Whether curve editor panel is open
    pub curve_editor_open: bool,
    /// Which property is selected in curve editor (0=scale, 1=pos_x, 2=pos_y, 3=opacity, 4=rotation)
    pub curve_editor_property: usize,
    /// Whether clip editor window is open
    pub clip_editor_open: bool,
    /// Detected pose points from motion tracking (normalised [0,1] coordinates)
    pub detected_points: Vec<[f32; 2]>,

    /// Index of text overlay currently being inline-edited on the preview
    pub editing_text_overlay: Option<usize>,

    // ─── Auto-save / recovery ──────────────────────────────────────
    /// Timestamp of last autosave (None until first autosave fires).
    pub last_autosave: Option<std::time::Instant>,
    /// Autosave interval in seconds.
    pub autosave_interval: f32,
    /// Path to a recovery scene that was found at startup, awaiting user decision.
    pub recovery_pending: Option<std::path::PathBuf>,
    /// Whether the recovery dialog is currently visible.
    pub recovery_dialog_open: bool,
    /// Time when the "Auto-saved" toast started (for fading the message after 2s).
    pub autosave_toast_until: Option<std::time::Instant>,

    // ─── Loop preview ──────────────────────────────────────────────
    /// Whether loop-preview mode is active.
    pub loop_mode: bool,
    /// Optional (start, end) loop region in seconds.
    pub loop_region: Option<(f32, f32)>,
    /// Pending loop edit: holds the first Shift+click time until the second click.
    pub loop_pending_start: Option<f32>,

    // ─── Title templates popup ─────────────────────────────────────
    /// Whether the "Add Title" template picker popup is open.
    pub title_picker_open: bool,

    // ─── Free Canvas viewport ──────────────────────────────────────
    /// Editor viewport camera for the free canvas (pan/zoom).
    pub canvas_viewport: memstroy_core::EditorViewport,
    /// Whether the canvas is in pan mode (middle mouse or Space+drag).
    pub canvas_panning: bool,
    /// Active drag interaction on the canvas. Persists across frames so the
    /// drag origin stays stable.
    pub canvas_drag: CanvasDrag,

    // ─── Skeleton Editor ───────────────────────────────────────────
    /// State for the skeleton constructor editor window.
    pub skeleton_editor: crate::skeleton_editor::SkeletonEditorState,

    // ─── Track assignment overrides ────────────────────────────────
    /// Explicit track assignment for actors. Key = actor index, Value = track index.
    /// When an actor is not in this map, the default round-robin assignment is used.
    pub actor_track_assignments: std::collections::HashMap<usize, usize>,

    // ─── Multi-tab scenes ──────────────────────────────────────────
    /// All open scene tabs. Index 0 is always the active tab's scene (synced with `self.scene`).
    pub scene_tabs: Vec<SceneTab>,
    /// Index of the currently active tab.
    pub active_tab: usize,
}

/// A single scene tab with its own file path and name.
#[derive(Clone)]
pub struct SceneTab {
    pub name: String,
    pub path: Option<PathBuf>,
    pub scene: Scene,
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
    Audio(usize),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RenderProgress {
    pub started: std::time::Instant,
    pub last_log: String,
    pub done: bool,
    pub error: Option<String>,
    /// Render progress as a float (0.0 - 1.0), parsed from ffmpeg output.
    pub progress: f32,
}

/// Legacy DragState kept for compatibility but mostly unused now.
#[derive(Default, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum DragState {
    #[default]
    None,
    BackgroundStart(usize),
    BackgroundEnd(usize),
    BackgroundMove(usize, f32),
    ActorMove(usize),
    OverlayMove(usize),
}

impl EditorState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.assets_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        s.scene = Scene::default();
        s.status = "Ready".into();
        s.playback_speed = 1.0;
        s.timeline_zoom = 80.0; // 80 pixels per second
        s.timeline_scroll = 0.0;
        s.last_rendered_playhead = -1.0;
        s.ffmpeg_available = check_ffmpeg();
        s.razor_mode = false;
        s.snap_enabled = true;
        s.inspector_tab = 0;

        // Default tracks: 3 video + 2 audio
        s.tracks = vec![
            Track::video("V1"),
            Track::video("V2"),
            Track::video("V3"),
            Track::audio("A1"),
            Track::audio("A2"),
        ];

        // New window states
        s.curve_editor_open = false;
        s.curve_editor_property = 0;
        s.clip_editor_open = false;

        s.detected_points = Vec::new();

        s.editing_text_overlay = None;

        // Auto-save defaults
        s.last_autosave = None;
        s.autosave_interval = 30.0;
        s.recovery_pending = None;
        s.recovery_dialog_open = false;
        s.autosave_toast_until = None;

        // Loop preview defaults
        s.loop_mode = false;
        s.loop_region = None;
        s.loop_pending_start = None;

        // Title templates popup
        s.title_picker_open = false;

        // Free canvas viewport
        s.canvas_viewport = memstroy_core::EditorViewport::default();
        s.canvas_panning = false;

        // Multi-tab: start with one untitled tab
        s.scene_tabs = vec![SceneTab {
            name: "Untitled".into(),
            path: None,
            scene: Scene::default(),
        }];
        s.active_tab = 0;

        s
    }

    pub fn clips_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("mellstroy")
    }

    pub fn state_path(&self) -> PathBuf {
        self.clips_dir().join("state.json")
    }

    /// Directory used for the autosave file. Falls back to the OS temp dir
    /// when `$HOME` is not set.
    pub fn autosave_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join(".memstroy");
            return p;
        }
        std::env::temp_dir().join("memstroy")
    }

    /// Path of the autosave scene file.
    pub fn autosave_path() -> PathBuf {
        Self::autosave_dir().join("autosave.scene.yaml")
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

    /// Get the track index for a given clip element based on the current scene layout.
    /// This is a heuristic — actors go on V1-V3, overlays on V2-V3, bgs on V1, audio on A1-A2.
    pub fn default_track_for_element(&self, elem: &ClipElement) -> usize {
        match elem {
            ClipElement::Actor(i) => {
                // Spread actors across video tracks
                let video_tracks: Vec<usize> = self.tracks.iter().enumerate()
                    .filter(|(_, t)| t.kind == TrackKind::Video)
                    .map(|(i, _)| i)
                    .collect();
                if video_tracks.is_empty() { 0 } else { video_tracks[*i % video_tracks.len()] }
            }
            ClipElement::Background(_) => 0, // Always bottom video track
            ClipElement::Overlay(i) => {
                let video_tracks: Vec<usize> = self.tracks.iter().enumerate()
                    .filter(|(_, t)| t.kind == TrackKind::Video)
                    .map(|(i, _)| i)
                    .collect();
                if video_tracks.len() >= 2 { video_tracks[1.min(video_tracks.len() - 1)] }
                else if !video_tracks.is_empty() { video_tracks[0] }
                else { *i }
            }
            ClipElement::Audio(i) => {
                let audio_tracks: Vec<usize> = self.tracks.iter().enumerate()
                    .filter(|(_, t)| t.kind == TrackKind::Audio)
                    .map(|(i, _)| i)
                    .collect();
                if audio_tracks.is_empty() { self.tracks.len().saturating_sub(1) }
                else { audio_tracks[*i % audio_tracks.len()] }
            }
        }
    }

    // ─── Tab management ──────────────────────────────────────────────

    /// Create a new empty tab and switch to it.
    pub fn new_tab(&mut self) {
        // Save current scene into its tab before switching
        self.sync_scene_to_tab();
        let name = format!("Scene {}", self.scene_tabs.len() + 1);
        self.scene_tabs.push(SceneTab {
            name,
            path: None,
            scene: Scene::default(),
        });
        self.active_tab = self.scene_tabs.len() - 1;
        self.sync_tab_to_scene();
    }

    /// Switch to tab at index.
    pub fn switch_tab(&mut self, idx: usize) {
        if idx >= self.scene_tabs.len() || idx == self.active_tab { return; }
        self.sync_scene_to_tab();
        self.active_tab = idx;
        self.sync_tab_to_scene();
    }

    /// Close tab at index. If it's the last tab, create a new empty one.
    pub fn close_tab(&mut self, idx: usize) {
        if self.scene_tabs.len() <= 1 {
            // Can't close last tab — just reset it
            self.scene = Scene::default();
            self.scene_path = None;
            self.scene_tabs[0] = SceneTab { name: "Untitled".into(), path: None, scene: Scene::default() };
            return;
        }
        self.scene_tabs.remove(idx);
        if self.active_tab >= self.scene_tabs.len() {
            self.active_tab = self.scene_tabs.len() - 1;
        }
        self.sync_tab_to_scene();
    }

    /// Sync `self.scene` into the active tab's stored scene.
    pub fn sync_scene_to_tab(&mut self) {
        if self.active_tab < self.scene_tabs.len() {
            self.scene_tabs[self.active_tab].scene = self.scene.clone();
            self.scene_tabs[self.active_tab].path = self.scene_path.clone();
        }
    }

    /// Load the active tab's scene into `self.scene`.
    pub fn sync_tab_to_scene(&mut self) {
        if self.active_tab < self.scene_tabs.len() {
            let tab = &self.scene_tabs[self.active_tab];
            self.scene = tab.scene.clone();
            self.scene_path = tab.path.clone();
            // Clear caches when switching
            self.frame_caches.clear();
            self.selection = Selection::None;
            self.playhead = 0.0;
        }
    }

    /// Add a new video track.
    pub fn add_video_track(&mut self) {
        let n = self.tracks.iter().filter(|t| t.kind == TrackKind::Video).count() + 1;
        self.tracks.push(Track::video(format!("V{}", n)));
    }

    /// Add a new audio track.
    pub fn add_audio_track(&mut self) {
        let n = self.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count() + 1;
        self.tracks.push(Track::audio(format!("A{}", n)));
    }

    /// Save timeline layout state (zoom, scroll, track heights) to a JSON file.
    pub fn save_layout(&self, path: &std::path::Path) {
        let track_heights: Vec<f32> = self.tracks.iter().map(|t| t.height).collect();
        let data = serde_json::json!({
            "timeline_zoom": self.timeline_zoom,
            "timeline_scroll": self.timeline_scroll,
            "track_heights": track_heights,
            "curve_editor_open": self.curve_editor_open,
            "curve_editor_property": self.curve_editor_property,
            "clip_editor_open": self.clip_editor_open,
        });
        if let Ok(json_str) = serde_json::to_string_pretty(&data) {
            let _ = std::fs::write(path, json_str);
        }
    }

    /// Load timeline layout state (zoom, scroll, track heights) from a JSON file.
    pub fn load_layout(&mut self, path: &std::path::Path) {
        let Ok(contents) = std::fs::read_to_string(path) else { return };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&contents) else { return };

        if let Some(zoom) = data.get("timeline_zoom").and_then(|v| v.as_f64()) {
            self.timeline_zoom = zoom as f32;
        }
        if let Some(scroll) = data.get("timeline_scroll").and_then(|v| v.as_f64()) {
            self.timeline_scroll = scroll as f32;
        }
        if let Some(heights) = data.get("track_heights").and_then(|v| v.as_array()) {
            for (i, h) in heights.iter().enumerate() {
                if i < self.tracks.len() {
                    if let Some(hf) = h.as_f64() {
                        self.tracks[i].height = hf as f32;
                    }
                }
            }
        }
        if let Some(ce_open) = data.get("curve_editor_open").and_then(|v| v.as_bool()) {
            self.curve_editor_open = ce_open;
        }
        if let Some(ce_prop) = data.get("curve_editor_property").and_then(|v| v.as_u64()) {
            self.curve_editor_property = ce_prop as usize;
        }
        if let Some(clip_open) = data.get("clip_editor_open").and_then(|v| v.as_bool()) {
            self.clip_editor_open = clip_open;
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


// ─── CANVAS INTERACTION STATE ────────────────────────────────────────

/// Active interaction with the free canvas. Captured once at the start of a
/// drag and persisted until the pointer is released, so the origin doesn't
/// drift between frames.
#[derive(Default, Clone)]
pub struct CanvasDrag {
    pub mode: CanvasDragMode,
    /// Pointer position (screen px, relative to the canvas rect) at drag start.
    pub start_screen: [f32; 2],
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum CanvasDragMode {
    /// No drag in progress.
    #[default]
    None,
    /// Panning the viewport.
    Pan,
    /// Move the selected actor in canvas world-pixel space.
    /// `initial_pos` is the actor's `canvas_layouts` position at drag start.
    MoveActorWorld { actor_idx: usize, initial_pos: [f32; 2] },
    /// Move the selected actor using the legacy normalised layout.
    MoveActorLegacy { actor_idx: usize, initial_pos: [f32; 2] },
    /// Move the selected overlay (normalised relative to render frame).
    MoveOverlay { overlay_idx: usize, initial_pos: [f32; 2] },
    /// Uniformly scale the selected element from its original anchor.
    ResizeSelection { initial_scale: f32, anchor_distance: f32 },
    /// Move the render frame in world space.
    MoveRenderFrame { initial_pos: [f32; 2] },
    /// Resize (zoom) the render frame.
    ResizeRenderFrame { initial_zoom: f32, anchor_distance: f32 },
}
