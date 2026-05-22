use std::path::PathBuf;

use memstroy_core::Scene;

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
    /// Latest "new lane requested" intent emitted by the per-frame drag
    /// classifier. We accumulate it during the gesture and only commit
    /// the actual `state.tracks.insert` on drag END, so passing through a
    /// gap on the way to a real lane doesn't create spurious empty
    /// layers. Cleared on every drag start and on drag end.
    pub pending_new_lane: Option<NewLaneIntent>,
    /// Pointer Y at drag start. Used to gate vertical lane changes:
    /// while the pointer is within `lane_lock_threshold_px` of this Y,
    /// the dragged clip stays on its original row even if the pointer
    /// briefly enters a neighbouring row. This kills the "wobble" feel
    /// where a horizontal drag accidentally pops the clip onto another
    /// lane mid-motion.
    pub start_pointer_y: Option<f32>,
}

/// What the layer panel wants to do when the current drag finally ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewLaneIntent {
    VideoTopForActor(usize),
    VideoBottomForActor(usize),
    VideoTopForOverlay(usize),
    VideoBottomForOverlay(usize),
    AudioTopForAudio(usize),
    AudioBottomForAudio(usize),
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
    /// An audio file — drops onto the canvas/timeline as an AudioTrack.
    Sound,
    /// A PNG / image asset — drops as an `Overlay::Image`.
    Image,
    /// A particle sprite — same as `Image` but the spawned overlay
    /// gets a Spin + Pulse modifier preset so it feels alive on drop.
    Particle,
    /// A user-imported video file from the project library.
    /// Behaves identically to a `Clip` drop (creates an actor + audio
    /// track) but is sourced from `assets/videos/` instead of the
    /// downloaded mellstroy clip pool.
    Video,
}

/// Drag state for cross-panel "element-to-skeleton-point" attachment. A
/// chip in the inspector is the drag source; the per-skeleton-point rows
/// are the drop targets. Both source and drop logic live in the inspector
/// today, so this is just shared state for the duration of the gesture.
#[derive(Default, Clone)]
pub struct ElementDrag {
    /// What's being dragged — either an overlay or an actor that should
    /// follow a skeleton point.
    pub source: Option<AttachableElement>,
    /// Latest pointer position (screen px) for the drag-ghost preview.
    pub pos: [f32; 2],
    /// Human-readable label for the drag-ghost.
    pub label: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttachableElement {
    /// An overlay (text/image/video) being attached to a skeleton point.
    Overlay(usize),
    /// Another actor being attached to a skeleton point of *this* actor.
    Actor(usize),
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
    /// Token of the in-flight drag-undo group, if any. See
    /// `EditorState::mutate_drag` for the full contract — the short
    /// version is "one undo snapshot per drag gesture, automatically".
    pub last_drag_group: Option<u64>,
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
    /// Currently visible sub-library tab (Clips / Sounds / Images / Particles).
    pub library_tab: LibraryTab,
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
    /// Element drag-and-drop: while a chip in the inspector is being
    /// dragged, this holds the source selection (overlay or actor)
    /// being attached. Drop zones (skeleton point rows) check this on
    /// pointer release to commit the binding.
    pub element_drag: ElementDrag,
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
    /// Index of the tab whose name is currently being edited inline (via
    /// double-click on the tab title). Cleared on Enter / focus loss.
    pub editing_tab_idx: Option<usize>,
    /// Buffer used by the inline tab-name editor.
    pub editing_tab_buf: String,

    // ─── Per-param keyframe selection (timeline → inspector) ───────
    /// Currently selected keyframes inside the focused layer. Cleared
    /// when the layer changes; mutated by clicking diamonds in the
    /// per-param keyframe rows. Shift / Ctrl+click extends the selection.
    /// Pressing Delete removes every selected keyframe.
    pub selected_keyframes: Vec<crate::kf_anim::SelectedKeyframe>,
    /// Brief "this param row was just clicked from a kf" highlight, used
    /// by the inspector to flash the matching control so the user can
    /// follow the connection visually.
    pub kf_highlight: crate::kf_anim::KfHighlight,
    /// Library panel screen rect, captured during library() so the
    /// app's external file-drop handler can route OS drops to the right
    /// asset directory based on which tab is visible.
    pub library_panel_rect: Option<egui::Rect>,
    /// Vertical split ratio inside the library panel between the
    /// "Local" (user-imported, drop-zone) section on top and the
    /// "Global" (auto-fetched / built-in) section on the bottom.
    /// Range 0.05..=0.95; persisted in the layout file. The same ratio
    /// is reused across every tab for predictable feel.
    pub library_split: f32,
    /// Tracks which selection most recently caused us to auto-bump the
    /// timeline's vertical zoom. Comparing per-frame ensures we don't
    /// keep re-applying the bump every paint while a layer is selected.
    pub last_v_zoom_selection: Option<Selection>,

    // ─── Clipboard & multi-selection ──────────────────────────────
    /// Items copied via Ctrl+C, pasted via Ctrl+V. Each paste creates a
    /// fresh element with a derived id and (for actors / audio) lands
    /// on a brand-new track ABOVE the current ones so the user can
    /// freely tweak the duplicate without touching the source.
    pub clipboard: Vec<ClipboardItem>,
    /// Multi-selection on the canvas. Mirrors `selection` for the
    /// inspector but holds the FULL set the user has painted with
    /// Ctrl/Shift+click or a marquee. Ctrl+C copies every entry; Ctrl+V
    /// pastes one duplicate per entry on its own new layer. Empty when
    /// only the primary `selection` is active.
    pub canvas_selection: Vec<Selection>,
    /// Active marquee (rubber-band) selection on the canvas. `Some` while
    /// the user is dragging an empty area to lasso multiple elements at
    /// once. World-pixel coords for both corners.
    pub canvas_marquee: Option<CanvasMarquee>,

    /// State for the "Shared" library tab — talks to a separate
    /// `memstroy-assets-server` instance over HTTP and lazily streams
    /// previews / files into the editor.
    // ─── Server-driven Telegram refresh (replaces the old "Shared" tab) ────
    /// Base URL of the local `memstroy-assets-server` instance that
    /// performs Telegram scraping on the GUI's behalf. The Refresh
    /// button on the Clips library tab POSTs to `{server_url}/api/ingest/tg`
    /// with `{tg_channel, tg_limit}`. Default points at loopback so a
    /// developer running `cargo run -p memstroy-assets-server` next to
    /// the GUI gets the right URL out of the box.
    pub server_url: String,
    /// Telegram channel name (without the leading `@`) the GUI asks the
    /// server to refresh. Surfaced on the Clips tab so the user can
    /// edit it without leaving the editor.
    pub tg_channel: String,
    /// How many of the most recent matching posts to ingest per refresh.
    pub tg_limit: u32,

    /// Last value of `library_search` we acted on. The library panel
    /// compares against this every frame and triggers a server refresh
    /// when the user types into the search box, so the inspector stays
    /// auto-fresh without an explicit Refresh button.
    pub prev_library_search: String,
    /// Tab whose `prev_library_search` snapshot was taken on, so
    /// switching tabs alone does not fire a refresh.
    pub prev_library_search_tab: LibraryTab,
    /// Wall-clock time of the last auto-refresh kick (any source). Used
    /// to debounce the "scroll near bottom" trigger so we don't refire
    /// the network call every frame the user holds the mouse wheel.
    pub last_auto_refresh: Option<std::time::Instant>,

    /// Tokio runtime handle injected by the App on startup so panels
    /// that talk to network services (the asset server, Telegram
    /// ingest, etc.) can spawn async tasks without rebuilding their
    /// own runtime. `None` only inside unit tests that don't bring
    /// up an `App`.
    pub tokio_handle: Option<tokio::runtime::Handle>,

    /// Persistent editor settings (language, master volume, etc.).
    /// Loaded from disk at startup; the settings dialog edits these
    /// fields directly and writes them back on close.
    pub settings: crate::settings::EditorSettings,
    /// Whether the File > Settings modal window is currently visible.
    pub settings_open: bool,

    /// Lazy texture cache for `Overlay::Image` PNG/JPEG sources. The
    /// canvas-side draw code calls `image_textures.lock()` and either
    /// retrieves an existing handle or kicks off a synchronous decode
    /// of the file with `image::open`. Mutex (rather than RefCell) so
    /// EditorState stays `Send` for any future background users; the
    /// lock is only held for the few microseconds spent on the lookup
    /// or the one-time decode.
    pub image_textures: std::sync::Mutex<
        std::collections::HashMap<PathBuf, ImageTextureSlot>,
    >,
}

/// Cached state for one image-overlay source. `Loading` is held only
/// briefly while the synchronous decode runs (we keep it as a state
/// rather than `Option<Result<...>>` so future async loaders can fit
/// without changing call-sites).
#[derive(Clone)]
pub enum ImageTextureSlot {
    Loaded {
        texture: egui::TextureHandle,
        size: [u32; 2],
    },
    /// Decode failed (missing file, unsupported format, etc.). Cached
    /// so we don't keep retrying on every frame.
    Failed,
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
    /// User-curated sound library — drag a row onto the timeline (or
    /// canvas) to insert a new AudioTrack at the drop position.
    pub sounds: Vec<LibraryAsset>,
    /// PNG / image stickers — dropped overlays use the file as-is.
    pub images: Vec<LibraryAsset>,
    /// Particle presets — actually image overlays bundled with a few
    /// modifier presets that give a "particle"-style motion (spin +
    /// pulse + slight wobble). The image is the particle sprite.
    pub particles: Vec<LibraryAsset>,
    /// User-imported videos. Same drag-to-canvas semantics as the
    /// downloaded mellstroy `Clip` tab — these come from the project's
    /// `assets/videos/` directory instead.
    pub videos: Vec<LibraryAsset>,
}

/// Generic library entry that's not a Mellstroy clip — a sound, an
/// image sticker, or a particle preset. The schema is intentionally
/// minimal so adding a new category later only needs another `Vec`
/// in `AssetLibrary` and a tab in the panel UI.
#[derive(Debug, Clone)]
pub struct LibraryAsset {
    /// Stable identifier (typically the file stem) used for the
    /// corresponding scene element's id.
    pub id: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Free-form label / description shown next to the entry.
    pub label: String,
    /// Optional thumbnail (`.png` / `.webp`) used by the drag ghost
    /// and the row card. Falls back to a generic icon when absent.
    pub thumbnail: Option<PathBuf>,
}

/// Which sub-library is currently visible in the panel. Persisted on
/// the editor state so the UI keeps the user's last tab choice.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum LibraryTab {
    #[default]
    Clips,
    Sounds,
    Images,
    Particles,
    Videos,
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
    /// The render frame (output region rectangle) is selected and edited
    /// like a normal element from the inspector.
    RenderFrame,
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

        // Default library local/global split — equal halves.
        s.library_split = 0.5;

        // ── Server-driven TG refresh defaults ──
        // The user clicks Refresh on the Clips tab; the GUI POSTs to
        // `{server_url}/api/ingest/tg` with `{tg_channel, tg_limit}`.
        // The server (memstroy-assets-server) does the actual scraping
        // and download. Defaults assume the server is running locally
        // on its standard port.
        s.server_url = "http://127.0.0.1:8765".to_string();
        s.tg_channel = "MELLSTROYfonz".to_string();
        s.tg_limit = 80;
        s.prev_library_search = String::new();
        s.prev_library_search_tab = LibraryTab::Clips;
        s.last_auto_refresh = None;

        // ── Load persistent editor preferences (language, master
        // volume, autosave interval, snap toggle) and apply the bits
        // that mirror onto runtime fields. The audio engine's master
        // volume is applied later in App::new (after the engine is
        // constructed), so we only stash the value here.
        s.settings = crate::settings::EditorSettings::load();
        s.autosave_interval = s.settings.autosave_interval;
        s.snap_enabled = s.settings.snap_enabled;
        s.settings_open = false;

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
        // Any explicit `mutate` call ends the previous drag-group, so the
        // next drag starts a fresh undo entry.
        self.last_drag_group = None;
        self.undo.push(&self.scene);
        f(&mut self.scene);
    }

    /// Drag-aware mutation. Pushes ONE undo snapshot for the very first
    /// mutation in a contiguous gesture identified by `token`, then
    /// mutates without snapshotting on every subsequent call. The token
    /// stays "live" until either:
    ///  - a different token is passed to `mutate_drag`,
    ///  - `mutate(...)` is called (which ends the group), or
    ///  - the application explicitly calls `end_drag_group()` (typically
    ///    when no pointer button is held).
    ///
    /// The result: dragging a clip across 60 frames produces a single
    /// undoable history entry instead of 60. Ctrl+Z reverts the entire
    /// gesture, Ctrl+Shift+Z restores it.
    pub fn mutate_drag<F>(&mut self, token: u64, f: F)
    where
        F: FnOnce(&mut Scene),
    {
        if self.last_drag_group != Some(token) {
            self.undo.push(&self.scene);
            self.last_drag_group = Some(token);
        }
        f(&mut self.scene);
    }

    /// Clear the active drag-undo group. The next `mutate_drag` call
    /// (regardless of token) will push a fresh undo snapshot.
    pub fn end_drag_group(&mut self) {
        self.last_drag_group = None;
    }

    /// Stable, namespace-aware token suitable for `mutate_drag`. Combine
    /// a fixed category string (`"drag_actor"`, `"trim_audio_left"`,
    /// etc.) with a small payload hash to keep different elements'
    /// drags isolated from each other.
    pub fn drag_token(category: &'static str, id: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        category.hash(&mut h);
        id.hash(&mut h);
        h.finish()
    }

    /// Undo the last action.
    pub fn undo(&mut self) {
        // Pressing Ctrl+Z must finalise any in-flight drag group so the
        // next drag pushes a fresh snapshot afterwards.
        self.last_drag_group = None;
        if let Some(prev) = self.undo.undo(&self.scene) {
            self.scene = prev;
            self.status = "\u{21A9} Undo".into();
        }
    }

    /// Redo the last undone action.
    pub fn redo(&mut self) {
        self.last_drag_group = None;
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

    /// Close tab at index. If it's the last tab, reset it to a fresh
    /// "Untitled" scene in place. Otherwise removes the tab and shifts
    /// `active_tab` so the focused selection lines up with the new
    /// indices (left-shift when closing a tab to the left of active,
    /// clamp to last when closing the rightmost active tab).
    pub fn close_tab(&mut self, idx: usize) {
        if idx >= self.scene_tabs.len() { return; }

        // Always sync the active tab's working scene back to its slot
        // first, so closing a different tab doesn't lose unsaved edits.
        self.sync_scene_to_tab();

        if self.scene_tabs.len() <= 1 {
            // Last tab: reset to fresh untitled state. Clear the loaded
            // scene buffer too so the canvas/inspector pick up a blank
            // slate without requiring a tab switch.
            self.scene_tabs[0] = SceneTab {
                name: "Untitled".into(),
                path: None,
                scene: Scene::default(),
            };
            self.active_tab = 0;
            self.scene = Scene::default();
            self.scene_path = None;
            self.frame_caches.clear();
            self.selection = Selection::None;
            self.playhead = 0.0;
            self.status = "Closed last tab — created fresh Untitled.".into();
            return;
        }

        let was_active = idx == self.active_tab;
        self.scene_tabs.remove(idx);

        // Adjust active_tab so it still refers to the same logical tab.
        if was_active {
            // Closed the focused tab: prefer the one that took its slot,
            // falling back to the last one.
            if self.active_tab >= self.scene_tabs.len() {
                self.active_tab = self.scene_tabs.len() - 1;
            }
        } else if idx < self.active_tab {
            // Closed a tab to the LEFT of the focused one: indices
            // shift left by one — keep the same logical tab focused.
            self.active_tab -= 1;
        }
        // (idx > self.active_tab → no change needed.)

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
        self.apply_layout_json(&data);
    }

    /// Apply a layout JSON value to the editor state. Extracted so the
    /// `.memstroy` bundle loader can reuse the same field-by-field
    /// extraction without going through the filesystem.
    fn apply_layout_json(&mut self, data: &serde_json::Value) {
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
        if let Some(split) = data.get("library_split").and_then(|v| v.as_f64()) {
            self.library_split = (split as f32).clamp(0.05, 0.95);
        }
    }

    /// Build a JSON value summarising editor layout (zoom, track
    /// heights, etc.). Used by both `save_layout` and `save_memstroy`.
    fn build_layout_json(&self) -> serde_json::Value {
        let track_heights: Vec<f32> = self.tracks.iter().map(|t| t.height).collect();
        serde_json::json!({
            "timeline_zoom": self.timeline_zoom,
            "timeline_scroll": self.timeline_scroll,
            "track_heights": track_heights,
            "curve_editor_open": self.curve_editor_open,
            "curve_editor_property": self.curve_editor_property,
            "clip_editor_open": self.clip_editor_open,
            "library_split": self.library_split,
        })
    }

    /// Save the active scene + editor layout to a `.memstroy` project
    /// file. The format is JSON with the shape:
    ///
    /// ```json
    /// {
    ///   "format": "memstroy",
    ///   "format_version": 1,
    ///   "scene": { ... },
    ///   "layout": { ... }
    /// }
    /// ```
    ///
    /// Both keys are required; `scene` carries the full scene tree
    /// (round-trippable through `Scene::load`), `layout` carries the
    /// editor's view state (timeline zoom, track heights, ...).
    pub fn save_memstroy(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bundle = serde_json::json!({
            "format": "memstroy",
            "format_version": 1,
            "scene": &self.scene,
            "layout": self.build_layout_json(),
        });
        let json = serde_json::to_string_pretty(&bundle)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, json)
    }

    /// Load a `.memstroy` bundle: returns the parsed Scene and applies
    /// the embedded layout to `self`. The caller is responsible for
    /// installing the scene into the active tab.
    pub fn load_memstroy(&mut self, path: &std::path::Path) -> Result<Scene, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let bundle: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid .memstroy json: {e}"))?;
        let scene_value = bundle
            .get("scene")
            .cloned()
            .ok_or_else(|| ".memstroy file missing \"scene\" key".to_string())?;
        let mut scene: Scene = serde_json::from_value(scene_value)
            .map_err(|e| format!("invalid scene in .memstroy: {e}"))?;
        scene.backfill_animated_params();
        // Apply layout if present (it's optional).
        if let Some(layout) = bundle.get("layout") {
            self.apply_layout_json(layout);
        }
        Ok(scene)
    }

    pub fn reload_library(&mut self) {
        // Local clip pool — we no longer carry a Telegram-side
        // `DownloadState` sidecar in the GUI (TG is the server's job).
        // Just enumerate `*.mp4` files in the clips dir and pair them
        // with thumbnails, when present, in the `thumbs/` subfolder.
        let clips_dir = self.clips_dir();
        let thumbs_dir = clips_dir.join("thumbs");
        let mut clips: Vec<LibraryClip> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&clips_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() { continue; }
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                if ext != "mp4" { continue; }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("clip")
                    .to_string();
                // Preserve the existing `id: u64` shape so the rest of
                // the GUI keeps treating clips as numeric ids; non-numeric
                // filenames hash to a derived id so the row still
                // displays.
                let id: u64 = stem
                    .parse::<u64>()
                    .unwrap_or_else(|_| {
                        use std::collections::hash_map::DefaultHasher;
                        use std::hash::{Hash, Hasher};
                        let mut h = DefaultHasher::new();
                        stem.hash(&mut h);
                        h.finish()
                    });
                let thumb_jpg = thumbs_dir.join(format!("{}.jpg", stem));
                let thumb_png = thumbs_dir.join(format!("{}.png", stem));
                let thumbnail = if thumb_jpg.exists() {
                    Some(thumb_jpg)
                } else if thumb_png.exists() {
                    Some(thumb_png)
                } else {
                    None
                };
                clips.push(LibraryClip {
                    id,
                    path: path.clone(),
                    description: stem,
                    downloaded: true,
                    thumbnail,
                });
            }
        }
        clips.sort_by_key(|c| c.id);
        self.library.mellstroy_clips = clips;

        // Also rescan the user's sound / image / particle bundles so the
        // sub-libraries pick up any new files dropped into their dirs.
        self.library.sounds = scan_asset_dir(&self.sounds_dir(), AssetCategory::Sound);
        self.library.images = scan_asset_dir(&self.images_dir(), AssetCategory::Image);
        self.library.particles = scan_asset_dir(&self.particles_dir(), AssetCategory::Particle);
        self.library.videos = scan_asset_dir(&self.videos_dir(), AssetCategory::Video);
    }

    /// Directory holding sound effects available in the library. Files
    /// dropped in here get picked up by the next `reload_library` call.
    pub fn sounds_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("sounds")
    }

    /// Directory holding PNG / image stickers.
    pub fn images_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("images")
    }

    /// Directory holding particle sprites.
    pub fn particles_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("particles")
    }

    /// Directory holding user-imported videos. Drops onto the Videos tab
    /// from the OS file manager copy here; drags from the tab spawn
    /// actor clips on the canvas / timeline.
    pub fn videos_dir(&self) -> PathBuf {
        self.assets_root.join("assets").join("videos")
    }

    // ─── Clipboard / copy-paste ─────────────────────────────────────

    /// Snapshot the currently-active selection set into the clipboard.
    /// When [`Self::canvas_selection`] is non-empty, every entry in it
    /// is copied; otherwise the primary [`Self::selection`] is the
    /// only item snapshotted.
    ///
    /// Each item is a deep clone of the live scene element so the
    /// clipboard survives subsequent edits / deletions of the source.
    pub fn copy_selection_to_clipboard(&mut self) -> usize {
        let mut targets: Vec<Selection> = if !self.canvas_selection.is_empty() {
            self.canvas_selection.clone()
        } else if self.selection != Selection::None {
            vec![self.selection]
        } else {
            return 0;
        };
        // Stable order: actors before overlays before backgrounds before
        // audio, ascending by index, so paste deterministic.
        targets.sort_by_key(|s| match s {
            Selection::Actor(i) => (0_u8, *i),
            Selection::Overlay(i) => (1, *i),
            Selection::Background(i) => (2, *i),
            Selection::Audio(i) => (3, *i),
            _ => (255, 0),
        });
        targets.dedup();

        let mut buf: Vec<ClipboardItem> = Vec::with_capacity(targets.len());
        for sel in targets {
            match sel {
                Selection::Actor(i) if i < self.scene.actors.len() => {
                    buf.push(ClipboardItem::Actor(self.scene.actors[i].clone()));
                }
                Selection::Overlay(i) if i < self.scene.overlays.len() => {
                    buf.push(ClipboardItem::Overlay(self.scene.overlays[i].clone()));
                }
                Selection::Background(i) if i < self.scene.backgrounds.len() => {
                    buf.push(ClipboardItem::Background(self.scene.backgrounds[i].clone()));
                }
                Selection::Audio(i) if i < self.scene.audio.len() => {
                    buf.push(ClipboardItem::Audio(self.scene.audio[i].clone()));
                }
                _ => {}
            }
        }
        let n = buf.len();
        if n > 0 {
            self.clipboard = buf;
        }
        n
    }

    /// Paste every item in [`Self::clipboard`] into the scene. Each
    /// pasted item lands on a brand-new layer at the TOP of the layer
    /// stack so it doesn't overwrite existing content. Returns the
    /// number of items pasted.
    pub fn paste_clipboard(&mut self) -> usize {
        if self.clipboard.is_empty() {
            return 0;
        }
        let buf = self.clipboard.clone();
        let mut new_selections: Vec<Selection> = Vec::new();
        // Take a single undo snapshot for the whole paste batch.
        self.last_drag_group = None;
        self.undo.push(&self.scene);

        for item in buf {
            match item {
                ClipboardItem::Actor(mut a) => {
                    a.id = unique_actor_id(&self.scene.actors, &a.id);
                    let new_idx = self.scene.actors.len();
                    self.scene.actors.push(a);
                    // Each pasted actor goes onto a brand-new video
                    // track inserted at the very TOP of the panel so
                    // the duplicate stacks above the source.
                    let new_track = self.insert_video_track_at_top();
                    self.actor_track_assignments.insert(new_idx, new_track);
                    new_selections.push(Selection::Actor(new_idx));
                }
                ClipboardItem::Overlay(mut o) => {
                    match &mut o {
                        memstroy_core::Overlay::Text(t) => {
                            t.id = unique_overlay_id(&self.scene.overlays, &t.id);
                        }
                        memstroy_core::Overlay::Image(im) => {
                            im.id = unique_overlay_id(&self.scene.overlays, &im.id);
                        }
                        memstroy_core::Overlay::Video(v) => {
                            v.id = unique_overlay_id(&self.scene.overlays, &v.id);
                        }
                    }
                    let new_idx = self.scene.overlays.len();
                    self.scene.overlays.push(o);
                    let new_track = self.insert_video_track_at_top();
                    self.overlay_track_assignments.insert(new_idx, new_track);
                    new_selections.push(Selection::Overlay(new_idx));
                }
                ClipboardItem::Background(mut bg) => {
                    bg.id = unique_background_id(&self.scene.backgrounds, &bg.id);
                    let new_idx = self.scene.backgrounds.len();
                    self.scene.backgrounds.push(bg);
                    new_selections.push(Selection::Background(new_idx));
                }
                ClipboardItem::Audio(mut au) => {
                    au.id = unique_audio_id(&self.scene.audio, &au.id);
                    // Standalone copy — never inherit a parent_actor
                    // binding, otherwise the duplicate would shadow the
                    // source's actor sync logic.
                    au.parent_actor = None;
                    let new_idx = self.scene.audio.len();
                    self.scene.audio.push(au);
                    let new_track = self.insert_audio_track_at_top();
                    self.audio_track_assignments.insert(new_idx, new_track);
                    new_selections.push(Selection::Audio(new_idx));
                }
            }
        }

        // Switch the primary selection to the LAST pasted item; keep
        // the full list as the multi-selection so the user can keep
        // working with the duplicates as a group.
        if let Some(&last) = new_selections.last() {
            self.selection = last;
        }
        self.canvas_selection = new_selections.clone();

        new_selections.len()
    }
}

fn unique_actor_id(actors: &[memstroy_core::Actor], base: &str) -> String {
    let stem = strip_copy_suffix(base);
    let mut candidate = format!("{}_copy", stem);
    let mut n = 2;
    while actors.iter().any(|a| a.id == candidate) {
        candidate = format!("{}_copy{}", stem, n);
        n += 1;
    }
    candidate
}

fn unique_overlay_id(overlays: &[memstroy_core::Overlay], base: &str) -> String {
    let stem = strip_copy_suffix(base);
    let mut candidate = format!("{}_copy", stem);
    let mut n = 2;
    while overlays.iter().any(|o| {
        let id = match o {
            memstroy_core::Overlay::Text(t) => &t.id,
            memstroy_core::Overlay::Image(im) => &im.id,
            memstroy_core::Overlay::Video(v) => &v.id,
        };
        id == &candidate
    }) {
        candidate = format!("{}_copy{}", stem, n);
        n += 1;
    }
    candidate
}

fn unique_background_id(bgs: &[memstroy_core::Background], base: &str) -> String {
    let stem = strip_copy_suffix(base);
    let mut candidate = format!("{}_copy", stem);
    let mut n = 2;
    while bgs.iter().any(|b| b.id == candidate) {
        candidate = format!("{}_copy{}", stem, n);
        n += 1;
    }
    candidate
}

fn unique_audio_id(audios: &[memstroy_core::AudioTrack], base: &str) -> String {
    let stem = strip_copy_suffix(base);
    let mut candidate = format!("{}_copy", stem);
    let mut n = 2;
    while audios.iter().any(|a| a.id == candidate) {
        candidate = format!("{}_copy{}", stem, n);
        n += 1;
    }
    candidate
}

/// Strip a trailing `_copy[<digits>]` so repeated copy-paste cycles
/// don't grow ids like `foo_copy_copy_copy`.
fn strip_copy_suffix(id: &str) -> &str {
    if let Some(idx) = id.rfind("_copy") {
        let tail = &id[idx + "_copy".len()..];
        if tail.is_empty() || tail.chars().all(|c| c.is_ascii_digit()) {
            return &id[..idx];
        }
    }
    id
}

/// Categories used by `scan_asset_dir` to filter the file extensions
/// it accepts. Each maps to a fixed list of recognised suffixes —
/// anything else is ignored, so dropping non-media files into the
/// library directory is harmless.
enum AssetCategory {
    Sound,
    Image,
    Particle,
    Video,
}

impl AssetCategory {
    fn is_supported(&self, ext: &str) -> bool {
        let lower = ext.to_ascii_lowercase();
        match self {
            AssetCategory::Sound => matches!(
                lower.as_str(),
                "mp3" | "wav" | "ogg" | "flac" | "aac" | "m4a" | "opus"
            ),
            AssetCategory::Image | AssetCategory::Particle => matches!(
                lower.as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            ),
            AssetCategory::Video => matches!(
                lower.as_str(),
                "mp4" | "mov" | "webm" | "avi" | "mkv" | "m4v"
            ),
        }
    }
}

/// Scan a directory for assets of the given category and turn each
/// supported file into a `LibraryAsset` row. Used by `reload_library`
/// so the user can drop files in the directory and refresh the panel.
fn scan_asset_dir(dir: &std::path::Path, category: AssetCategory) -> Vec<LibraryAsset> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<LibraryAsset> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() { continue; }
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if ext.is_empty() || !category.is_supported(&ext) { continue; }
        let id = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("asset")
            .to_string();
        // Look for a sibling thumbnail (`<stem>.png` or `<stem>.thumb.png`).
        let thumbnail = match category {
            // For images / particles, the asset itself is its thumbnail.
            AssetCategory::Image | AssetCategory::Particle => Some(path.clone()),
            AssetCategory::Sound | AssetCategory::Video => {
                let candidates = [
                    path.with_extension("thumb.png"),
                    path.with_extension("thumb.jpg"),
                ];
                candidates.into_iter().find(|p| p.exists())
            }
        };
        out.push(LibraryAsset {
            id: id.clone(),
            path: path.clone(),
            label: id,
            thumbnail,
        });
    }
    out.sort_by(|a, b| a.label.to_ascii_lowercase().cmp(&b.label.to_ascii_lowercase()));
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
    /// Snapshot of legacy actor WORLD positions at drag start (for actors
    /// without a canvas_layouts entry). Used to keep them visually fixed
    /// while the render frame moves/resizes.
    pub actor_legacy_snapshot: Vec<(usize, [f32; 2])>,
    /// Snapshot of overlay WORLD positions at drag start. Used to keep
    /// overlays visually fixed while the render frame moves/resizes.
    pub overlay_world_snapshot: Vec<(usize, [f32; 2])>,
    /// Currently active snap guidelines in world space — drawn on top of
    /// the canvas while a move/resize drag is in flight to give the user
    /// visual feedback about which edge/center the element snapped to.
    /// Each entry is (axis, world_coordinate). Reset to empty whenever no
    /// snap is active.
    pub snap_guides: Vec<SnapGuide>,
    /// Playhead time captured the moment the drag started (seconds, in
    /// scene time). Every keyframe write performed during the gesture is
    /// re-anchored to THIS time instead of the live `state.playhead`, so
    /// dragging while playback is running cannot spawn a fresh keyframe
    /// per frame — every upsert lands on the same kf and the result is
    /// exactly one keyframe at the drag-start time. `None` outside an
    /// active drag.
    pub drag_start_playhead: Option<f32>,
    /// Whether playback was running at drag start. Used so the canvas
    /// can auto-pause for the duration of the gesture (and optionally
    /// resume on release — currently we leave it paused so the user
    /// can review the keyframe they just authored).
    pub was_playing_at_drag_start: bool,
}

/// One active snap guideline.
#[derive(Clone, Copy, PartialEq)]
pub struct SnapGuide {
    pub axis: SnapAxis,
    /// World coord (X for Vertical guides, Y for Horizontal guides).
    pub world: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SnapAxis {
    /// Vertical line at world X = `world` — used to snap horizontal positions.
    Vertical,
    /// Horizontal line at world Y = `world` — used to snap vertical positions.
    Horizontal,
}

// ─── CLIPBOARD / MULTI-SELECTION TYPES ──────────────────────────────

/// One entry in the editor's in-memory clipboard. Each variant owns a
/// fully-cloned snapshot of the source scene element so paste does not
/// depend on the source still existing.
#[derive(Clone)]
pub enum ClipboardItem {
    Actor(memstroy_core::Actor),
    Overlay(memstroy_core::Overlay),
    Background(memstroy_core::Background),
    Audio(memstroy_core::AudioTrack),
}

/// Active marquee (rubber-band) selection on the canvas. Both corners
/// live in world-pixel coordinates so the same rectangle stays anchored
/// regardless of pan / zoom while the user drags.
#[derive(Clone, Copy, Debug)]
pub struct CanvasMarquee {
    pub start: [f32; 2],
    pub end: [f32; 2],
}

impl CanvasMarquee {
    pub fn rect_world(&self) -> ([f32; 2], [f32; 2]) {
        let xa = self.start[0].min(self.end[0]);
        let xb = self.start[0].max(self.end[0]);
        let ya = self.start[1].min(self.end[1]);
        let yb = self.start[1].max(self.end[1]);
        ([xa, ya], [xb, yb])
    }
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
    /// Rotate the selected element around its centre. `start_angle_rad`
    /// is the angle from element-centre to pointer at drag start (radians).
    /// `initial_rot_deg` is the element's rotation at drag start.
    RotateSelection {
        initial_rot_deg: f32,
        center_screen: [f32; 2],
        start_angle_rad: f32,
    },
}
