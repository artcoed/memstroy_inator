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

/// Drag state for clips already on the timeline. Tracks only whether a
/// timeline clip is currently being dragged, so we can take a single undo
/// snapshot at the start of the gesture.
#[derive(Default, Clone)]
pub struct TimelineDrag {
    pub dragging_clip: Option<usize>,
}

/// Drag-and-drop state for an item being dragged out of the clip library.
#[derive(Default, Clone)]
pub struct AssetDrag {
    /// Path of the clip being dragged.
    pub dragging: Option<PathBuf>,
    /// Kind of asset being dragged.
    pub kind: AssetDragKind,
    /// Current pointer position during the drag (used to anchor the ghost).
    pub pos: [f32; 2],
    /// Human-readable label rendered next to the drag ghost.
    pub label: String,
    /// Optional thumbnail used by the drag ghost preview.
    pub thumbnail: Option<PathBuf>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum AssetDragKind {
    #[default]
    None,
    Clip,
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
    pub render_progress: Option<RenderProgress>,
    pub refreshing: bool,
    pub undo: UndoStack,
    /// Playback state
    pub playing: bool,
    /// Playback speed multiplier (1.0 = normal, 2.0 = 2x, 0.5 = half)
    pub playback_speed: f32,
    /// Timeline zoom level (pixels per second)
    pub timeline_zoom: f32,
    /// Timeline horizontal scroll offset in seconds
    pub timeline_scroll: f32,
    /// Timeline vertical zoom multiplier (multiplies each track's height for display)
    pub timeline_v_zoom: f32,
    /// Timeline vertical scroll offset in pixels (top of viewport in scaled-track-space)
    pub timeline_v_scroll: f32,
    /// Split tool active: when true, clicking on a clip cuts it at the click position.
    pub split_tool_active: bool,
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

    // ─── Premiere Pro-style timeline ───
    /// Fixed tracks (lanes). Video tracks are kept above audio tracks at all
    /// times — see `enforce_track_order`.
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
    /// Explicit track assignment for actors. Key = actor index, value = track index.
    pub actor_track_assignments: std::collections::HashMap<usize, usize>,
    /// Explicit track assignment for audio rows. Key = audio index, value = track index.
    pub audio_track_assignments: std::collections::HashMap<usize, usize>,
    /// Explicit track assignment for overlay rows (text/image/video overlays).
    /// Overlays without an entry default to the second video track.
    pub overlay_track_assignments: std::collections::HashMap<usize, usize>,

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

impl EditorState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.assets_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        s.scene = Scene::default();
        s.status = "Ready".into();
        s.playback_speed = 1.0;
        s.timeline_zoom = 80.0; // 80 pixels per second
        s.timeline_scroll = 0.0;
        s.timeline_v_zoom = 1.0;
        s.timeline_v_scroll = 0.0;
        s.split_tool_active = false;
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

    /// Indices of all video tracks in screen order (top → bottom).
    pub fn video_track_indices(&self) -> Vec<usize> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Video)
            .map(|(i, _)| i)
            .collect()
    }

    /// Indices of all audio tracks in screen order (top → bottom).
    pub fn audio_track_indices(&self) -> Vec<usize> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Audio)
            .map(|(i, _)| i)
            .collect()
    }

    /// Re-sort `tracks` so all `Video` lanes come before all `Audio` lanes
    /// (video on top, audio at the bottom of the layers panel). The order
    /// inside each kind is preserved. All track-assignment maps are
    /// remapped through the same permutation so existing actors/overlays
    /// keep referring to the same physical row.
    pub fn enforce_track_order(&mut self) {
        let n = self.tracks.len();
        let mut perm: Vec<usize> = (0..n).collect();
        perm.sort_by_key(|&i| match self.tracks[i].kind {
            TrackKind::Video => 0u8,
            TrackKind::Audio => 1u8,
        });
        if perm.iter().enumerate().all(|(new, &old)| new == old) {
            return;
        }
        let new_tracks: Vec<Track> = perm.iter().map(|&i| self.tracks[i].clone()).collect();
        let mut old_to_new = vec![0usize; n];
        for (new, &old) in perm.iter().enumerate() {
            old_to_new[old] = new;
        }
        self.tracks = new_tracks;
        let remap = |m: &mut std::collections::HashMap<usize, usize>| {
            for v in m.values_mut() {
                if *v < old_to_new.len() {
                    *v = old_to_new[*v];
                }
            }
        };
        remap(&mut self.actor_track_assignments);
        remap(&mut self.audio_track_assignments);
        remap(&mut self.overlay_track_assignments);
    }

    /// Insert a new video track immediately before the first audio track
    /// (or at the end if there are no audio tracks). Returns the new
    /// track's index. Existing assignments are bumped by 1 wherever they
    /// referenced a row at or after the insertion point.
    pub fn insert_video_track_at_bottom(&mut self) -> usize {
        let n = self.tracks.iter().filter(|t| t.kind == TrackKind::Video).count() + 1;
        let pos = self.tracks.iter()
            .position(|t| t.kind == TrackKind::Audio)
            .unwrap_or(self.tracks.len());
        self.tracks.insert(pos, Track::video(format!("V{}", n)));
        let bump = |m: &mut std::collections::HashMap<usize, usize>, pivot: usize| {
            for v in m.values_mut() {
                if *v >= pivot { *v += 1; }
            }
        };
        bump(&mut self.actor_track_assignments, pos);
        bump(&mut self.audio_track_assignments, pos);
        bump(&mut self.overlay_track_assignments, pos);
        pos
    }

    /// Insert a new video track at index 0 (top). Returns 0. Every existing
    /// assignment is shifted up by one so it keeps referring to the same row.
    pub fn insert_video_track_at_top(&mut self) -> usize {
        let n = self.tracks.iter().filter(|t| t.kind == TrackKind::Video).count() + 1;
        self.tracks.insert(0, Track::video(format!("V{}", n)));
        let bump = |m: &mut std::collections::HashMap<usize, usize>| {
            for v in m.values_mut() { *v += 1; }
        };
        bump(&mut self.actor_track_assignments);
        bump(&mut self.audio_track_assignments);
        bump(&mut self.overlay_track_assignments);
        0
    }

    /// Insert a new audio track immediately after the last video track
    /// (i.e. at the top of the audio block). Returns the new track's index.
    pub fn insert_audio_track_at_top(&mut self) -> usize {
        let n = self.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count() + 1;
        let pos = self.tracks.iter()
            .rposition(|t| t.kind == TrackKind::Video)
            .map(|p| p + 1)
            .unwrap_or(0);
        self.tracks.insert(pos, Track::audio(format!("A{}", n)));
        let bump = |m: &mut std::collections::HashMap<usize, usize>, pivot: usize| {
            for v in m.values_mut() {
                if *v >= pivot { *v += 1; }
            }
        };
        bump(&mut self.actor_track_assignments, pos);
        bump(&mut self.audio_track_assignments, pos);
        bump(&mut self.overlay_track_assignments, pos);
        pos
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

    /// Add a new audio track at the bottom of the layers panel.
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
    }
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
    /// Snapshot of legacy actor WORLD positions at drag start (for actors
    /// without a canvas_layouts entry). Used to keep them visually fixed
    /// while the render frame moves/resizes.
    pub actor_legacy_snapshot: Vec<(usize, [f32; 2])>,
    /// Snapshot of overlay WORLD positions at drag start. Used to keep
    /// overlays visually fixed while the render frame moves/resizes.
    pub overlay_world_snapshot: Vec<(usize, [f32; 2])>,
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum CanvasDragMode {
    /// No drag in progress.
    #[default]
    None,
    /// Move the selected actor in canvas world-pixel space.
    /// `initial_pos` is the actor's `canvas_layouts` position at drag start.
    MoveActorWorld { actor_idx: usize, initial_pos: [f32; 2] },
    /// Move the selected actor using the legacy normalised layout.
    MoveActorLegacy { actor_idx: usize, initial_pos: [f32; 2] },
    /// Move the selected overlay (normalised relative to render frame).
    MoveOverlay { overlay_idx: usize, initial_pos: [f32; 2] },
    /// Resize the selected element using a specific handle. The element is
    /// anchored at the opposite handle so it stretches in the direction of
    /// the drag. `handle` is 0..3 = corners (TL,TR,BR,BL), 4..7 = edges
    /// (Top,Right,Bottom,Left). Hold Shift while dragging for uniform
    /// (proportional) scaling.
    ResizeSelection {
        handle: u8,
        initial_scale: f32,
        initial_scale_y: f32,
        initial_pos_world: [f32; 2],
        anchor_world: [f32; 2],
        base_w: f32,
        base_h: f32,
    },
    /// Move the render frame in world space.
    MoveRenderFrame { initial_pos: [f32; 2] },
    /// Resize (zoom) the render frame.
    ResizeRenderFrame { initial_zoom: f32, anchor_distance: f32 },
}
