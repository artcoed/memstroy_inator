use std::path::PathBuf;

use memstroy_core::Scene;

use crate::undo::UndoStack;

/// Rewrite a server URL so it can actually be used as the *target* of an
/// HTTP request from this GUI process.
///
/// The asset-server typically *binds* to the wildcard address
/// `0.0.0.0:8765` (or `[::]:8765` for IPv6) so it accepts traffic on
/// every local interface. Both addresses are valid for `bind(2)` but
/// invalid for `connect(2)` on Windows (and on macOS for IPv6 — only
/// Linux is forgiving here). When the user runs
/// `memstroy-assets-server --addr 0.0.0.0:8765` and the GUI then tries
/// to POST to `http://0.0.0.0:8765/api/ingest/tg`, Windows fails the
/// connect with `WSAEADDRNOTAVAIL` and the user sees
/// "Refresh failed: Server unreachable (http://0.0.0.0:8765…)".
///
/// Replace the unspecified host with the appropriate loopback so
/// client requests reach the server reliably regardless of how the
/// operator configured the bind address.
///
/// The function preserves the URL's scheme, port, and path intact, and
/// only touches the host component when it matches `0.0.0.0` /
/// `[::]` / `0:0:0:0:0:0:0:0`. Anything else (including real IPs and
/// hostnames) is returned untouched so a remote
/// `https://assets.example.com` keeps working.
pub fn rewrite_server_url_for_client(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    // Split optional scheme so we never strip an `http(s)://` prefix.
    let (scheme, host_and_rest) = match trimmed.find("://") {
        Some(idx) => (&trimmed[..idx + 3], &trimmed[idx + 3..]),
        None => ("", trimmed),
    };

    // Split the host[:port][/path...] section: everything up to the
    // first `/` is host(:port).
    let (host_port, tail) = match host_and_rest.find('/') {
        Some(idx) => (&host_and_rest[..idx], &host_and_rest[idx..]),
        None => (host_and_rest, ""),
    };

    // Bracketed IPv6 form: `[::]:port` or `[::]`.
    if let Some(end) = host_port.find(']') {
        if host_port.starts_with('[') {
            let host = &host_port[1..end];
            let port = &host_port[end + 1..]; // includes the `:port` if any
            if is_unspecified_ipv6(host) {
                return format!("{}[::1]{}{}", scheme, port, tail);
            }
            return format!("{}{}{}", scheme, host_port, tail);
        }
    }

    // IPv4 host[:port] form, or bare hostname.
    let (host, port) = match host_port.rfind(':') {
        Some(i) => (&host_port[..i], &host_port[i..]),
        None => (host_port, ""),
    };
    if host == "0.0.0.0" {
        return format!("{}127.0.0.1{}{}", scheme, port, tail);
    }
    if is_unspecified_ipv6(host) {
        return format!("{}[::1]{}{}", scheme, port, tail);
    }
    trimmed.to_string()
}

/// IPv6 unspecified-address detector. Accepts both the canonical `::`
/// short-form and the `0:0:…:0` long-form (eight zero groups). Pure
/// string compare — no `Ipv6Addr::parse`, so we don't accidentally
/// reject malformed input the rest of the URL parser would also flag.
fn is_unspecified_ipv6(host: &str) -> bool {
    if host == "::" {
        return true;
    }
    let groups: Vec<&str> = host.split(':').collect();
    if groups.len() == 8 && groups.iter().all(|g| !g.is_empty() && g.chars().all(|c| c == '0')) {
        return true;
    }
    false
}

#[cfg(test)]
mod url_rewrite_tests {
    use super::rewrite_server_url_for_client as r;

    #[test]
    fn ipv4_unspecified_with_scheme() {
        assert_eq!(r("http://0.0.0.0:8765"), "http://127.0.0.1:8765");
        assert_eq!(
            r("http://0.0.0.0:8765/api/assets"),
            "http://127.0.0.1:8765/api/assets"
        );
    }

    #[test]
    fn ipv4_unspecified_no_scheme() {
        assert_eq!(r("0.0.0.0:8765"), "127.0.0.1:8765");
    }

    #[test]
    fn ipv6_unspecified_short_form() {
        assert_eq!(r("http://[::]:8765"), "http://[::1]:8765");
    }

    #[test]
    fn ipv6_unspecified_long_form_in_brackets() {
        assert_eq!(
            r("http://[0:0:0:0:0:0:0:0]:8765"),
            "http://[::1]:8765"
        );
    }

    #[test]
    fn loopback_untouched() {
        assert_eq!(r("http://127.0.0.1:8765"), "http://127.0.0.1:8765");
    }

    #[test]
    fn remote_hostname_untouched() {
        assert_eq!(
            r("https://assets.example.com"),
            "https://assets.example.com"
        );
    }
}

/// Fixed track in the timeline. Tracks are numbered lanes; clips sit on them.
#[derive(Debug, Clone)]
pub struct Track {
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub locked: bool,
    /// Reserved for an upcoming "hide track from preview" toggle that
    /// the timeline UI will surface next to `locked`. Kept on the
    /// struct so existing scene-state files round-trip without losing
    /// the field.
    #[allow(dead_code)]
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

/// Active rectangle (rubber-band) selection on the timeline. Both
/// corners live in screen-pixel coordinates because the timeline
/// already operates in screen space (the world-pixel anchor used by
/// the canvas marquee doesn't apply here).
#[derive(Clone, Copy, Debug)]
pub struct TimelineMarquee {
    pub start: egui::Pos2,
    pub end: egui::Pos2,
}

impl TimelineMarquee {
    pub fn rect(&self) -> egui::Rect {
        egui::Rect::from_two_pos(self.start, self.end)
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
    /// Movers that need an overlap-trim pass when the drag ends. We
    /// queue them up during the gesture instead of trimming on every
    /// frame (which made neighbouring clips disappear the instant the
    /// dragged clip even momentarily overlapped them). On pointer-up
    /// the queue is drained and `enforce_no_overlap_on_layer` runs
    /// once per unique mover.
    pub pending_overlap: Vec<PendingOverlapMover>,
}

/// Lightweight mirror of `panels::MovedClipKind` used to persist
/// "deferred overlap-trim" requests on the EditorState across frames
/// without exposing that internal type from the panels module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PendingOverlapMover {
    Actor(usize),
    Overlay(usize),
    Audio(usize),
    Background(usize),
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
        let duration = {
            let mut cmd = std::process::Command::new(&ffprobe);
            cmd.args(["-v", "error", "-show_entries", "format=duration",
                      "-of", "default=noprint_wrappers=1:nokey=1"])
                .arg(audio_path);
            match memstroy_render::hide_console_std(&mut cmd).output() {
                Ok(out) => String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().unwrap_or(0.0),
                Err(_) => return None,
            }
        };

        if duration <= 0.0 { return None; }

        // Extract raw PCM samples via ffmpeg, downsample to mono 8kHz
        let ffmpeg = memstroy_render::ffmpeg_binary();
        let output = {
            let mut cmd = std::process::Command::new(&ffmpeg);
            cmd.args(["-y", "-hide_banner", "-loglevel", "error",
                      "-i"])
                .arg(audio_path)
                .args(["-ac", "1", "-ar", "8000", "-f", "s16le", "-"]);
            memstroy_render::hide_console_std(&mut cmd).output()
        };

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
    /// Snapshot of the scene captured the last time a pointer button
    /// was pressed. Used by `app.rs::update` as a frame-level catch-all
    /// for inspector edits (slider drags, button clicks, …) that don't
    /// route through `mutate_drag`. On pointer-release, if the scene
    /// has diverged from this snapshot AND no `mutate_drag` token
    /// fired during the gesture, the snapshot is pushed to the undo
    /// stack — yielding "one Ctrl+Z = one user gesture" everywhere,
    /// not just on the canvas / timeline.
    pub pre_press_scene: Option<memstroy_core::Scene>,
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
    /// Index into the curve editor's candidate-target list — used when
    /// the user has multiple compatible elements multi-selected and
    /// picks one in the in-window dropdown.
    pub curve_editor_active_idx: usize,
    /// Whether image editor window is open (image-only filters / crop).
    pub image_editor_open: bool,
    /// Interactive-brush state for the image editor's preview area —
    /// holds the currently armed tool, in-progress polygon points
    /// (sampled in source-image UV 0..1), and the parameters the
    /// commit step bakes into a new `EffectKind::Mask` / `Crop`
    /// entry on the selected image overlay. Only consulted while the
    /// image editor window is open.
    pub image_brush: ImageEditorBrush,

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

    /// Active marquee on the timeline panel. `Some` while the user is
    /// dragging an empty area between clips to lasso a group of clips.
    /// Coordinates are in screen pixels — the timeline panel handler
    /// commits the selection on drag-end against every clip whose
    /// screen-rect intersects the marquee.
    pub timeline_marquee: Option<TimelineMarquee>,

    /// Press position recorded while we wait to see whether a fresh
    /// timeline pointer-press will grow into a real marquee or stay a
    /// click. The marquee handler only promotes this into
    /// `timeline_marquee` once the pointer has travelled past the
    /// drag-threshold, and consumes it on pointer release (where it
    /// becomes the "click on empty space → clear selection" gesture).
    /// Cleared on every release.
    pub timeline_marquee_pending: Option<egui::Pos2>,

    /// True while the user is actively scrubbing the timeline by
    /// dragging the playhead vertical line in the tracks area
    /// (separate from the ruler scrubbing). The flag is set on the
    /// initial press if the press lands within a few pixels of the
    /// playhead's screen-X, and cleared on release. While set, the
    /// per-clip drag handlers and the marquee both bow out so the
    /// playhead drag stays the ONLY active gesture — even when the
    /// playhead line visually overlaps a clip bar.
    pub timeline_scrubbing_playhead: bool,

    // ─── Mask / crop drawing tools ──────────────────────────────────
    /// Currently armed mask / crop drawing tool. `None` = transform mode
    /// (default — clicks on the canvas select / move / resize the
    /// element). When set, click-drag inside the selected element's
    /// bounding box paints a mask shape into the element's effects
    /// stack on release. The toolbar above the canvas toggles this
    /// field; pressing Escape clears it.
    pub mask_tool: MaskTool,
    /// Polyline accumulated while the freehand mask tool is in flight.
    /// UV coords (0..1) inside the selected element's bounding box.
    /// Cleared on commit / cancel. Lives on the editor state because
    /// `CanvasDragMode` derives `Copy` and can't store a Vec.
    pub mask_draft_points: Vec<[f32; 2]>,

    /// Live cursor UV (in element-local coords, 0..1) updated every
    /// frame while `MaskTool::SegmentMask` is armed and the user is
    /// hovering over the selected element. The mask-draft renderer
    /// reads this to draw a "rubber-band" line from the last
    /// committed vertex to the live cursor — and a dashed
    /// closure-preview from the cursor back to the first vertex.
    /// `None` when the cursor is off-element or the tool is idle.
    /// Reset alongside `mask_draft_points` on commit / cancel.
    pub mask_segment_cursor_uv: Option<[f32; 2]>,

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

    /// Wall-clock time of the most recent local-asset-directory poll
    /// (sounds / images / particles / videos / clips). Used by
    /// [`Self::auto_rescan_local_library_if_due`] to debounce a
    /// cheap mtime-fingerprint check that catches files dropped into
    /// the asset directories by an external tool — file manager,
    /// download manager, sync service — without the user needing to
    /// trigger a paste/drag first. Replaces the old "library is
    /// frozen until I add the first picture myself" behaviour.
    pub last_library_rescan: Option<std::time::Instant>,
    /// Fingerprint of the asset directories from the most recent
    /// rescan, in the same order as [`Self::library_dirs`]. Each
    /// entry is `(file_count, latest_mtime_unix_secs)` — picking up
    /// a single new file changes either the count or the mtime, so
    /// even a "rename only" external edit forces a rescan. Used
    /// purely as an inexpensive change detector; the actual library
    /// rebuild still goes through [`Self::reload_library`].
    pub library_dir_fingerprint: Vec<(u64, u64)>,

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
    /// Cache of effects-baked image textures keyed by
    /// `(source_path, effects_signature)`. The actual cache lives in
    /// `crate::image_fx_cache::ImageFxCache` — a two-layer LRU with a
    /// Pending/Ready/Failed state machine that lets the canvas draw
    /// the unprocessed picture while the effect stack bakes on a
    /// worker thread (see `crate::image_fx_worker`). Wrapped in `Arc`
    /// so the worker can share ownership without cloning the cache
    /// state itself.
    pub image_fx_cache: std::sync::Arc<crate::image_fx_cache::ImageFxCache>,

    /// Sender used by the canvas paint loop to dispatch background
    /// image-effects bake jobs back to the App's `pump_events` drain.
    /// Wired up by `App::new`; `None` only inside unit tests that
    /// don't bring up an App.
    pub image_fx_tx: Option<std::sync::mpsc::Sender<crate::jobs::JobEvent>>,

    // ─── Web image search ──────────────────────────────────────────
    /// Whether the floating "Web Image Search" window is visible.
    /// Persisted in the layout file alongside the other floating-
    /// window toggles.
    pub web_image_search_open: bool,
    /// Per-panel state for the web image search (current query,
    /// in-flight flag, the last batch of results). Kept on the editor
    /// state so the user's search persists across show/hide cycles.
    pub web_image_search: crate::web_image_search::WebImageSearchState,
}

/// Brush / interactive-tool state owned by the image editor floating
/// window. Lives on `EditorState` (instead of being module-local) so
/// it persists across show/hide cycles of the window and so the
/// (potentially many) painted polygon points round-trip with the
/// surrounding mutable state without per-frame `egui` memory
/// shuffling.
///
/// All in-progress points are sampled in **source-image UV (0..1)**.
/// On commit they are baked into the selected overlay's effect stack
/// as either an `EffectKind::Mask { Polygon, .. }` or an
/// `EffectKind::Crop { .. }` entry depending on the active tool.
#[derive(Clone, Default)]
pub struct ImageEditorBrush {
    pub tool: ImageBrushTool,
    /// Soft edge applied to a freshly committed polygon mask
    /// (UV-space fraction 0..0.5).
    pub feather: f32,
    /// When set, the freshly committed mask uses `invert: true`. The
    /// "Cutout" tool flips this on automatically; the "Brush" tool
    /// leaves it off.
    pub invert: bool,
    /// Live polygon being painted by the user. Cleared on every
    /// pointer release (after commit) and on tool changes.
    pub draft: Vec<[f32; 2]>,
    /// Anchor point for rectangle-style drags (Crop). Stored in
    /// source-image UV. `None` outside an active drag.
    pub crop_drag_start: Option<[f32; 2]>,
}

/// Image-editor brush mode. Mirrors the tool buttons in the
/// floating window's toolbar; selecting `None` is the "no
/// interactive tool" fallback that lets the preview behave as a
/// passive thumbnail.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ImageBrushTool {
    /// Default — preview is non-interactive.
    #[default]
    None,
    /// Freehand brush: the painted polygon becomes the visible
    /// region (`invert: false`). Pixels outside the polygon are
    /// masked away.
    Brush,
    /// Cutout brush: the painted polygon becomes the *masked* region
    /// (`invert: true`). Useful for erasing watermarks / unwanted
    /// objects without leaving the editor.
    Cutout,
    /// Rectangle drag — the rectangle painted by the user is baked
    /// into the overlay's `EffectKind::Crop` entry.
    Crop,
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
    /// Decode failed (missing file, unsupported format, partial write,
    /// race with the downloader, …). Holds the wall-clock instant of
    /// the last attempt so the canvas can re-try after a short cool-
    /// down — without that, an image that was briefly missing on the
    /// first paint (very common for files dropped from the web image
    /// search before the download finished) was permanently disabled
    /// because the cached `Failed` slot was never invalidated.
    Failed {
        last_attempt: std::time::Instant,
    },
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
        s.assets_root = Self::default_assets_root();
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
        s.curve_editor_active_idx = 0;
        s.image_editor_open = false;
        s.image_brush = ImageEditorBrush::default();

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
        // and download. The default URL is baked at build time:
        // developer builds get the loopback (`http://127.0.0.1:8765`,
        // matching the in-process server `app.rs` boots up), while
        // packaged client builds get whatever
        // `MEMSTROY_DEFAULT_SERVER_URL` was set to when the bundle was
        // produced (e.g. `https://assets.your-domain.example`). Either
        // way the literal goes through `obfstr` so it is not visible
        // verbatim in `strings(1)` over the binary.
        s.server_url = crate::build_info::default_server_url();
        s.tg_channel = "MELLSTROYfonz".to_string();
        // Default catalogue depth for the "Refresh from Telegram"
        // button. Set to 500 (was 80) so the first refresh on a
        // fresh install pulls a real backlog of clips instead of
        // the most recent handful — the user reported that the
        // library showed only the latest few dozen even on
        // channels with hundreds of posts. The server caps this on
        // its side as well, so very large values are safe.
        s.tg_limit = 500;
        s.prev_library_search = String::new();
        s.prev_library_search_tab = LibraryTab::Clips;
        s.last_auto_refresh = None;
        s.last_library_rescan = None;
        s.library_dir_fingerprint = Vec::new();

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

    /// Path to a sidecar state file for the clips directory. Reserved
    /// for an upcoming per-folder cache of last-used filters / scroll
    /// position that other parts of the GUI haven't started writing
    /// yet, but a couple of exporter scripts already read.
    #[allow(dead_code)]
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

    /// Default value for `assets_root` based on the build flavour.
    ///
    /// * **Developer builds** (`MEMSTROY_CLIENT_BUILD` unset at compile
    ///   time): rooted at the current working directory, matching the
    ///   long-standing dev workflow where `cargo run -p memstroy-gui`
    ///   from the workspace root surfaces the in-tree `assets/` dir.
    /// * **Client-distribution builds**: rooted at a per-user cache
    ///   directory (`~/.memstroy/cache/` on Unix,
    ///   `%USERPROFILE%\.memstroy\cache\` on Windows). Client bundles
    ///   ship without any bundled assets — every clip / image / sound
    ///   is fetched from the operator's `memstroy-assets-server` over
    ///   HTTP and lands here on demand. Putting the cache outside the
    ///   bundle directory means re-installing the editor never wipes
    ///   downloaded media, and the user does not need write access to
    ///   the install location.
    pub fn default_assets_root() -> PathBuf {
        if crate::build_info::IS_CLIENT_BUILD {
            Self::user_cache_dir()
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    }

    /// Per-user cache directory used as `assets_root` in client builds.
    ///
    /// Mirrors the placement convention of [`Self::autosave_dir`] so
    /// "wipe `~/.memstroy/`" cleans both autosaves and downloaded
    /// assets at once. Falls back to `$TEMP/memstroy/cache` only when
    /// neither `HOME` nor `USERPROFILE` is set, which in practice only
    /// happens on heavily restricted CI runners.
    pub fn user_cache_dir() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".memstroy").join("cache");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(home).join(".memstroy").join("cache");
        }
        std::env::temp_dir().join("memstroy").join("cache")
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

    /// Find a video lane that has no actor / overlay clip currently
    /// occupying time `t`. Returns the first such lane (smallest index)
    /// or `None` when every lane is currently busy. Used by canvas-drop
    /// handlers so a freshly dropped clip lands on its own row instead
    /// of stacking on top of whatever is already on V1.
    pub fn find_empty_video_lane_at(&self, t: f32) -> Option<usize> {
        let scene_dur = self.scene.output.duration.max(0.0);
        for &lane in self.video_track_indices().iter() {
            let mut busy = false;
            // Actors assigned to this lane.
            for (ai, _) in self.scene.actors.iter().enumerate() {
                let assigned = self
                    .actor_track_assignments
                    .get(&ai)
                    .copied()
                    .unwrap_or_else(|| {
                        self.video_track_indices()
                            .first()
                            .copied()
                            .unwrap_or(0)
                    });
                if assigned != lane { continue; }
                let a = &self.scene.actors[ai];
                let t_in = a.t_in.unwrap_or(0.0);
                let t_out = a.t_out.unwrap_or(scene_dur);
                if t >= t_in && t <= t_out {
                    busy = true;
                    break;
                }
            }
            if busy { continue; }
            // Overlays assigned to this lane.
            let default_overlay_lane = {
                let v = self.video_track_indices();
                if v.len() >= 2 { v[1] } else { v.first().copied().unwrap_or(0) }
            };
            for (oi, ov) in self.scene.overlays.iter().enumerate() {
                let assigned = self
                    .overlay_track_assignments
                    .get(&oi)
                    .copied()
                    .unwrap_or(default_overlay_lane);
                if assigned != lane { continue; }
                let (t_in, t_out) = match ov {
                    memstroy_core::Overlay::Text(o) => (o.t_in, o.t_out),
                    memstroy_core::Overlay::Image(o) => (o.t_in, o.t_out),
                    memstroy_core::Overlay::Video(o) => (o.t_in, o.t_out),
                };
                if t >= t_in && t <= t_out {
                    busy = true;
                    break;
                }
            }
            if !busy {
                return Some(lane);
            }
        }
        None
    }

    /// Pick the lane a fresh canvas-dropped clip should land on:
    /// the first empty video lane at time `t`, falling back to a
    /// freshly-inserted lane at the top of the video stack when every
    /// existing lane is busy.
    pub fn pick_or_create_empty_video_lane_at(&mut self, t: f32) -> usize {
        if let Some(lane) = self.find_empty_video_lane_at(t) {
            lane
        } else {
            self.insert_video_track_at_top()
        }
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

    /// Find an audio lane on which no existing audio clip's time range
    /// overlaps `[t_in, t_out]`. Returns the first such lane (smallest
    /// index) or `None` when every existing lane is currently busy or
    /// when there are no audio lanes at all. Used by the canvas /
    /// library / timeline drop handlers so a freshly added sound always
    /// lands on its own row instead of stacking on top of whatever is
    /// already there.
    ///
    /// Two ranges that merely touch at a single instant (e.g. one ends
    /// at 5.0 and the next starts at 5.0) are intentionally treated as
    /// non-overlapping so back-to-back placement on the same lane
    /// stays legal.
    pub fn find_empty_audio_lane_for_range(&self, t_in: f32, t_out: f32) -> Option<usize> {
        let scene_dur = self.scene.output.duration.max(0.0);
        let audio_lanes = self.audio_track_indices();
        if audio_lanes.is_empty() {
            return None;
        }
        for &lane in audio_lanes.iter() {
            let mut busy = false;
            for (aui, au) in self.scene.audio.iter().enumerate() {
                let assigned = self
                    .audio_track_assignments
                    .get(&aui)
                    .copied()
                    .unwrap_or_else(|| audio_lanes[aui % audio_lanes.len()]);
                if assigned != lane { continue; }
                let other_in = au.t_in;
                let other_out = au.t_out.unwrap_or(scene_dur);
                // Half-open overlap test: ranges [a_in, a_out] and
                // [b_in, b_out] overlap iff a_in < b_out AND b_in < a_out.
                if t_in < other_out && other_in < t_out {
                    busy = true;
                    break;
                }
            }
            if !busy {
                return Some(lane);
            }
        }
        None
    }

    /// Pick the lane a fresh audio clip with the given time range
    /// should land on: the first empty audio lane that fits, falling
    /// back to a freshly-inserted lane right after the video stack
    /// when every existing lane is busy (or when no audio lanes exist
    /// yet). Mirrors `pick_or_create_empty_video_lane_at` for sound.
    pub fn pick_or_create_empty_audio_lane_for_range(
        &mut self,
        t_in: f32,
        t_out: f32,
    ) -> usize {
        if let Some(lane) = self.find_empty_audio_lane_for_range(t_in, t_out) {
            lane
        } else {
            self.insert_audio_track_at_top()
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
            "image_editor_open": self.image_editor_open,
            "web_image_search_open": self.web_image_search_open,
            "web_image_search_query": self.web_image_search.query,
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
        if let Some(clip_open) = data.get("image_editor_open").and_then(|v| v.as_bool()) {
            self.image_editor_open = clip_open;
        }
        if let Some(open) = data.get("web_image_search_open").and_then(|v| v.as_bool()) {
            self.web_image_search_open = open;
        }
        if let Some(q) = data.get("web_image_search_query").and_then(|v| v.as_str()) {
            self.web_image_search.query = q.to_string();
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
            "image_editor_open": self.image_editor_open,
            "web_image_search_open": self.web_image_search_open,
            "web_image_search_query": self.web_image_search.query,
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
        scene.upgrade_legacy();
        // Apply layout if present (it's optional).
        if let Some(layout) = bundle.get("layout") {
            self.apply_layout_json(layout);
        }
        Ok(scene)
    }

    pub fn reload_library(&mut self) {
        // Make sure the asset subdirectories exist BEFORE we walk
        // them. On a fresh install (or in a client build whose
        // `assets_root` is `~/.memstroy/cache/`) none of these
        // directories ship with the binary — and `scan_asset_dir`
        // returns an empty Vec when the path is missing. Without this
        // upfront `create_dir_all` the editor would show an empty
        // Images / Sounds / Videos / Particles tab on first run, then
        // suddenly populate the moment the user dropped or pasted the
        // first file (which created the directory as a side-effect).
        // The user reported exactly that: "картинки подгружаются из
        // локального кеша в проект только после добавления первой
        // картинки". Creating the dirs eagerly + the periodic
        // mtime-fingerprint poll in `auto_rescan_local_library_if_due`
        // means existing files become visible right away, and any
        // file dropped in by an external tool is picked up within a
        // couple of seconds without a manual refresh.
        for dir in self.library_dirs() {
            let _ = std::fs::create_dir_all(&dir);
        }

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
                // Pick up the description sidecar (`<stem>.txt`) if it
                // exists. It's written by the assets-server during TG
                // ingest and mirrored locally by `jobs::spawn_refresh`,
                // so each clip card can show the original Telegram
                // caption rather than the bare numeric id.
                let txt_path = clips_dir.join(format!("{}.txt", stem));
                let description = match std::fs::read_to_string(&txt_path) {
                    Ok(s) => {
                        let trimmed = s.trim().to_string();
                        if trimmed.is_empty() { stem.clone() } else { trimmed }
                    }
                    Err(_) => stem.clone(),
                };
                clips.push(LibraryClip {
                    id,
                    path: path.clone(),
                    description,
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

        // Refresh the directory fingerprint so the next
        // `auto_rescan_local_library_if_due` only triggers an actual
        // reload when the on-disk state really did change.
        self.library_dir_fingerprint = self.compute_library_dir_fingerprint();
        self.last_library_rescan = Some(std::time::Instant::now());
    }

    /// Every asset directory the editor's library panel surfaces, in a
    /// fixed order so [`Self::library_dir_fingerprint`] stays
    /// comparable across rescans. Used to (a) eagerly create them on
    /// startup so external file drops show up without first having to
    /// paste an image inside the editor, and (b) walk for an mtime
    /// fingerprint in [`Self::compute_library_dir_fingerprint`].
    pub fn library_dirs(&self) -> [PathBuf; 5] {
        [
            self.clips_dir(),
            self.sounds_dir(),
            self.images_dir(),
            self.particles_dir(),
            self.videos_dir(),
        ]
    }

    /// Cheap "did the asset dirs change?" detector. For each library
    /// directory we walk the *direct* contents (no subdirectories
    /// beyond `clips_dir/thumbs/`, which only the clips tab cares
    /// about and is already tracked under the parent's mtime) and
    /// produce `(file_count, max_mtime_unix_secs)`. Only the
    /// aggregates leave this function — we never store the per-file
    /// list, so the cost is one `read_dir` + a tiny stat per direct
    /// entry, well within "run on every UI frame".
    ///
    /// Returns one tuple per entry in [`Self::library_dirs`].
    fn compute_library_dir_fingerprint(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::with_capacity(5);
        for dir in self.library_dirs() {
            out.push(dir_fingerprint(&dir));
        }
        out
    }

    /// Periodic auto-rescan of the asset directories, called once per
    /// UI frame. Cheap: at most a `read_dir` + per-entry `metadata`
    /// per asset directory every `MIN_INTERVAL`, with an early-out
    /// if nothing changed.
    ///
    /// Triggers a full [`Self::reload_library`] iff:
    ///
    ///   1. We have not rescanned for at least `MIN_INTERVAL`, AND
    ///   2. The fingerprint differs from the one captured at the
    ///      last rebuild.
    ///
    /// This is what keeps externally-dropped files (file manager,
    /// download manager, screenshot tool, OneDrive sync, …) flowing
    /// into the library panel without the user needing to paste or
    /// drag something inside the editor first.
    pub fn auto_rescan_local_library_if_due(&mut self) {
        // Don't fight an ongoing TG refresh — that worker sends its
        // own `RefreshLibraryReloaded` events when new clips land,
        // and we'd just be doing redundant work polling alongside it.
        if self.refreshing {
            return;
        }
        const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2_000);
        if let Some(last) = self.last_library_rescan {
            if last.elapsed() < MIN_INTERVAL {
                return;
            }
        }

        let now = self.compute_library_dir_fingerprint();
        if now != self.library_dir_fingerprint {
            // On-disk state changed (file added / removed / replaced
            // / renamed externally). Run the heavy rebuild — it
            // re-stamps `library_dir_fingerprint` and bumps
            // `last_library_rescan` itself.
            self.reload_library();
        } else {
            // Nothing to do; just bump the timer so we don't busy-poll.
            self.last_library_rescan = Some(std::time::Instant::now());
        }
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
        // Snapshot the playhead once so every pasted item is anchored
        // to the same time, even if a side-effect (track insertion,
        // selection update) shifts editor state mid-loop.
        let playhead = self.playhead;
        // Take a single undo snapshot for the whole paste batch.
        self.last_drag_group = None;
        self.undo.push(&self.scene);

        for item in buf {
            match item {
                ClipboardItem::Actor(mut a) => {
                    a.id = unique_actor_id(&self.scene.actors, &a.id);
                    // Re-anchor the duplicate to the current playhead so
                    // it lands where the user expects to see it. The
                    // original clip-local duration is preserved.
                    let dur = match (a.t_in, a.t_out) {
                        (Some(ti), Some(to)) => (to - ti).max(0.1),
                        _ => self.scene.output.duration.max(0.1),
                    };
                    a.t_in = Some(playhead);
                    a.t_out = Some(playhead + dur);
                    let new_idx = self.scene.actors.len();
                    self.scene.actors.push(a);
                    // Each pasted actor lands on the first empty video
                    // lane at the playhead — only when none is free do
                    // we spawn a fresh track at the top. Mirrors the
                    // canvas-drop behaviour and matches the user's
                    // mental model of "paste shows up on whichever
                    // empty layer is closest right now".
                    let new_track = self.pick_or_create_empty_video_lane_at(playhead);
                    self.actor_track_assignments.insert(new_idx, new_track);
                    new_selections.push(Selection::Actor(new_idx));
                }
                ClipboardItem::Overlay(mut o) => {
                    let (orig_t_in, orig_t_out) = match &o {
                        memstroy_core::Overlay::Text(t) => (t.t_in, t.t_out),
                        memstroy_core::Overlay::Image(im) => (im.t_in, im.t_out),
                        memstroy_core::Overlay::Video(v) => (v.t_in, v.t_out),
                    };
                    let dur = (orig_t_out - orig_t_in).max(0.1);
                    let new_t_in = playhead;
                    let new_t_out = playhead + dur;
                    match &mut o {
                        memstroy_core::Overlay::Text(t) => {
                            t.id = unique_overlay_id(&self.scene.overlays, &t.id);
                            t.t_in = new_t_in;
                            t.t_out = new_t_out;
                        }
                        memstroy_core::Overlay::Image(im) => {
                            im.id = unique_overlay_id(&self.scene.overlays, &im.id);
                            im.t_in = new_t_in;
                            im.t_out = new_t_out;
                        }
                        memstroy_core::Overlay::Video(v) => {
                            v.id = unique_overlay_id(&self.scene.overlays, &v.id);
                            v.t_in = new_t_in;
                            v.t_out = new_t_out;
                        }
                    }
                    let new_idx = self.scene.overlays.len();
                    self.scene.overlays.push(o);
                    let new_track = self.pick_or_create_empty_video_lane_at(playhead);
                    self.overlay_track_assignments.insert(new_idx, new_track);
                    new_selections.push(Selection::Overlay(new_idx));
                }
                ClipboardItem::Background(mut bg) => {
                    bg.id = unique_background_id(&self.scene.backgrounds, &bg.id);
                    bg.start = playhead;
                    new_selections.push(Selection::Background(self.scene.backgrounds.len()));
                    self.scene.backgrounds.push(bg);
                }
                ClipboardItem::Audio(mut au) => {
                    au.id = unique_audio_id(&self.scene.audio, &au.id);
                    // Standalone copy — never inherit a parent_actor
                    // binding, otherwise the duplicate would shadow the
                    // source's actor sync logic.
                    au.parent_actor = None;
                    au.t_in = playhead;
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

    // ─── System-clipboard image paste ───────────────────────────────

    /// Save raw RGBA pixels from the system clipboard into the project's
    /// image library and return the resulting [`LibraryAsset`]. Used by
    /// the Ctrl+V handler when the OS clipboard contains a bitmap (e.g.
    /// a PrintScreen capture or a "copy image" from a browser).
    ///
    /// The PNG ends up in `assets/images/clipboard_<timestamp>.png` so
    /// repeated pastes don't fight over a single filename. The library
    /// listing is refreshed before the function returns so the new
    /// asset shows up on the panel immediately.
    pub fn save_clipboard_image_to_library(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<LibraryAsset, String> {
        if width == 0 || height == 0 {
            return Err("clipboard image is empty".into());
        }
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected {
            return Err(format!(
                "clipboard image buffer too small: {} < {}",
                rgba.len(),
                expected
            ));
        }
        let dir = self.images_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("create {}: {}", dir.display(), e));
        }
        // Filename: `clipboard_<unix-millis>.png` — millisecond
        // resolution avoids collisions across rapid pastes while still
        // being trivially sortable in a file manager.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut filename = format!("clipboard_{}.png", stamp);
        let mut path = dir.join(&filename);
        let mut suffix = 1u32;
        while path.exists() {
            filename = format!("clipboard_{}_{}.png", stamp, suffix);
            path = dir.join(&filename);
            suffix += 1;
            if suffix > 1000 { break; }
        }
        let buf = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| "failed to wrap clipboard image".to_string())?;
        buf.save(&path)
            .map_err(|e| format!("save {}: {}", path.display(), e))?;
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clipboard")
            .to_string();
        let asset = LibraryAsset {
            id: id.clone(),
            path: path.clone(),
            label: id,
            thumbnail: Some(path),
        };
        // Refresh the library listing so the new file shows up on the
        // Images tab with a thumbnail.
        self.reload_library();
        Ok(asset)
    }

    /// Save a baked frame snapshot (e.g. produced by the "📸 Extract
    /// frame" toolbar button) into the project's image library and
    /// return the resulting [`LibraryAsset`].
    ///
    /// Mirrors [`Self::save_clipboard_image_to_library`] but writes
    /// to `assets/images/frame_<unix-millis>.png` so the file manager
    /// listing stays sorted and snapshots don't collide with images
    /// pasted from the OS clipboard.
    pub fn save_snapshot_image_to_library(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<LibraryAsset, String> {
        if width == 0 || height == 0 {
            return Err("snapshot image is empty".into());
        }
        let expected = (width as usize) * (height as usize) * 4;
        if rgba.len() < expected {
            return Err(format!(
                "snapshot image buffer too small: {} < {}",
                rgba.len(),
                expected
            ));
        }
        let dir = self.images_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(format!("create {}: {}", dir.display(), e));
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut filename = format!("frame_{}.png", stamp);
        let mut path = dir.join(&filename);
        let mut suffix = 1u32;
        while path.exists() {
            filename = format!("frame_{}_{}.png", stamp, suffix);
            path = dir.join(&filename);
            suffix += 1;
            if suffix > 1000 {
                break;
            }
        }
        let buf = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| "failed to wrap snapshot image".to_string())?;
        buf.save(&path)
            .map_err(|e| format!("save {}: {}", path.display(), e))?;
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("frame")
            .to_string();
        let asset = LibraryAsset {
            id: id.clone(),
            path: path.clone(),
            label: id,
            thumbnail: Some(path),
        };
        self.reload_library();
        Ok(asset)
    }

    /// Spawn an `Overlay::Image` for the supplied library asset at the
    /// current playhead, anchored to the first empty video lane (or a
    /// freshly-spawned one). Returns the overlay index of the new
    /// element so the caller can push it into the selection. Used by
    /// the Ctrl+V system-clipboard handler — pasting an image from the
    /// OS turns into "image now lives in the library AND on the canvas
    /// at the current time" in a single gesture.
    pub fn add_image_overlay_at_playhead(&mut self, asset: &LibraryAsset) -> usize {
        let t = self.playhead;
        let dur = self.scene.output.duration.max(0.1);
        let t_in = t;
        let t_out = (t + 3.0).min(dur).max(t + 0.1);
        let overlay = memstroy_core::Overlay::Image(memstroy_core::ImageOverlay {
            id: unique_overlay_id(&self.scene.overlays, &asset.id),
            source: asset.path.clone(),
            t_in,
            t_out,
            layout: vec![memstroy_core::Keyframe::new(
                0.0,
                memstroy_core::OverlayState::default(),
            )],
            modifiers: Vec::new(),
            skeleton_attachment: None,
            effects: Vec::new(),
            animated_params: Default::default(),
            chroma_key: None,
        });
        self.last_drag_group = None;
        self.undo.push(&self.scene);
        let new_idx = self.scene.overlays.len();
        self.scene.overlays.push(overlay);
        let new_track = self.pick_or_create_empty_video_lane_at(t);
        self.overlay_track_assignments.insert(new_idx, new_track);
        self.selection = Selection::Overlay(new_idx);
        self.canvas_selection.clear();
        new_idx
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
        // Optional sidecar caption: `<stem>.txt` next to the asset.
        // The assets-server writes these for Telegram-ingested clips,
        // and the GUI mirrors them locally on refresh — picking them
        // up here means sounds / videos / particles get a real label
        // instead of a bare numeric id once a sidecar is present.
        let txt_path = path.with_extension("txt");
        let label = match std::fs::read_to_string(&txt_path) {
            Ok(s) => {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() { id.clone() } else { trimmed }
            }
            Err(_) => id.clone(),
        };
        out.push(LibraryAsset {
            id: id.clone(),
            path: path.clone(),
            label,
            thumbnail,
        });
    }
    out.sort_by(|a, b| a.label.to_ascii_lowercase().cmp(&b.label.to_ascii_lowercase()));
    out
}

/// Inexpensive `(file_count, latest_mtime_unix_secs)` summary of a
/// single directory. Used by [`EditorState::auto_rescan_local_library_if_due`]
/// to detect external changes without re-scanning every byte every
/// frame. Subdirectories are ignored — the editor never recurses
/// past one level for library assets, so anything below them
/// (e.g. `clips_dir/thumbs/`) doesn't affect the asset listing.
///
/// Returns `(0, 0)` when the directory is missing, unreadable, or
/// empty — those all map to "nothing visible in the library tab",
/// so the comparison degrades gracefully without panicking on a
/// recently-deleted folder.
fn dir_fingerprint(dir: &std::path::Path) -> (u64, u64) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };
    let mut count: u64 = 0;
    let mut latest_mtime: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Only count files; subdirs are walked separately.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        count += 1;
        if let Ok(modified) = meta.modified() {
            if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                let secs = d.as_secs();
                if secs > latest_mtime {
                    latest_mtime = secs;
                }
            }
        }
        // Suppress an unused-warning on the path binding when no
        // future field needs it; debug builds optimise this out.
        let _ = path;
    }
    (count, latest_mtime)
}

/// Check if ffmpeg binary is accessible.
fn check_ffmpeg() -> bool {
    let mut cmd = std::process::Command::new(memstroy_render::ffmpeg_binary());
    cmd.arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    memstroy_render::hide_console_std(&mut cmd).status().is_ok()
}


// ─── CANVAS INTERACTION STATE ────────────────────────────────────────

/// Per-element snapshot used by canvas multi-selection drags. The
/// editor stores one of these per `state.canvas_selection` entry at
/// drag start, then re-uses them every frame to compute "where this
/// element should be RIGHT NOW" from the primary's accumulated delta —
/// so the entire group translates / scales / rotates as one piece
/// without drifting between frames.
#[derive(Clone, Copy)]
pub struct MultiDragEntry {
    pub selection: Selection,
    /// World-pixel centre of the element at drag start.
    pub initial_pos: [f32; 2],
    /// Scale at drag start. 1.0 for elements that don't expose a scale.
    pub initial_scale: f32,
    /// Y-axis stretch factor at drag start.
    pub initial_scale_y: f32,
    /// Rotation in degrees at drag start.
    pub initial_rotation: f32,
}

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
    /// Per-element snapshot for canvas_selection at the start of a
    /// Move/Resize/Rotate gesture. Each entry stores the element's
    /// world-centre, scale, scale_y, and rotation — applied on every
    /// frame as `entry + (delta vs primary)` so every selected element
    /// transforms together without per-frame drift.
    /// Only filled when `canvas_selection.len() > 1` and the drag mode
    /// is a transform; cleared on drag end.
    pub multi_drag_snapshot: Vec<MultiDragEntry>,
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
    /// Unused when `axis == SnapAxis::Line`; see `line_origin` /
    /// `line_angle_rad` instead.
    pub world: f32,
    /// World-space anchor for a free-orientation `Line` guide. The
    /// guide passes through this point at angle `line_angle_rad`.
    /// Defaults to the origin so `SnapGuide { axis, world }`-style
    /// constructors keep working without explicit values.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub line_origin: [f32; 2],
    /// Direction of the `Line` guide in radians, measured from the
    /// world +X axis. Snapping projects onto the line perpendicularly.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub line_angle_rad: f32,
}

impl SnapGuide {
    /// Convenience constructor for the common axis-aligned cases so
    /// existing `SnapGuide { axis, world }` literals can migrate
    /// incrementally without having to touch every callsite at once.
    #[inline]
    pub fn axis_aligned(axis: SnapAxis, world: f32) -> Self {
        Self {
            axis,
            world,
            line_origin: [0.0, 0.0],
            line_angle_rad: 0.0,
        }
    }

    /// Free-orientation guide: the line passes through `origin` at
    /// `angle_rad`. Used when the render frame is rotated and we want
    /// to snap to one of its rotated edges.
    #[inline]
    pub fn line(origin: [f32; 2], angle_rad: f32) -> Self {
        Self {
            axis: SnapAxis::Line,
            world: 0.0,
            line_origin: origin,
            line_angle_rad: angle_rad,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SnapAxis {
    /// Vertical line at world X = `world` — used to snap horizontal positions.
    Vertical,
    /// Horizontal line at world Y = `world` — used to snap vertical positions.
    Horizontal,
    /// Free-orientation line. The line passes through
    /// `SnapGuide::line_origin` at `SnapGuide::line_angle_rad`. Used
    /// for rotated render-frame edges so the user sees the actual
    /// rotated guide rather than an axis-aligned approximation.
    Line,
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

/// Currently armed mask / crop tool. Used by the canvas to dispatch
/// pointer-drag input to the mask painter instead of the regular
/// transform handlers. Stored on [`EditorState`] so the toolbar above
/// the canvas, the inspector, and the input pipeline can all read /
/// flip the same bit.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaskTool {
    /// Default: transform mode — clicks select / move / resize.
    #[default]
    None,
    /// Rectangle mask. Drag-defines a rectangle; on release pushes
    /// `EffectKind::Mask { shape: Rect }` onto the selected element.
    /// Replaces the legacy "Crop" tool — both used to live side by
    /// side and produced visually-equivalent results, so they were
    /// merged into a single rectangle gesture. Old scenes that still
    /// carry `EffectKind::Crop` continue to render unchanged.
    RectMask,
    /// Ellipse mask. Drag from one corner of the bounding box to the
    /// opposite corner; the inscribed ellipse becomes the mask.
    EllipseMask,
    /// Freehand polygon mask. Each pointer move during the drag
    /// appends a new vertex; on release the polyline is closed back
    /// to its first point.
    FreehandMask,
    /// Eyedropper colour-key mask. A single click on the canvas
    /// samples the underlying pixel colour and pushes a fresh
    /// `EffectKind::ColorKey` entry onto the layer's effect stack.
    /// Works on actors (sampled from the decoded frame cache) and
    /// image overlays (sampled from the source PNG) alike.
    ///
    /// **Activation lives in the inspector "Masks" panel** — the
    /// floating canvas toolbar no longer exposes a button for this
    /// tool because the per-effect controls (similarity / blend /
    /// spill / invert) need to be visible while the user is picking,
    /// and the inspector is the only place those sliders live. The
    /// canvas-side click handler is unchanged: once the inspector
    /// arms the tool, the next click on the picture samples the
    /// pixel and writes it to the colour-key entry.
    Eyedropper,
    /// **Segment selection mask** — click-by-click polygon
    /// construction with an eyedropper-style crosshair cursor. Each
    /// click plants a vertex; segments are drawn between consecutive
    /// vertices. Closure happens in three equivalent ways so the
    /// user can pick whichever feels most natural for the current
    /// gesture:
    ///   * click near the first vertex (with ≥ 3 vertices placed),
    ///   * double-click anywhere,
    ///   * press Enter / Return.
    /// Right-click pops the last vertex (handy for backing out of a
    /// misplaced corner without restarting the polygon). Esc cancels
    /// the entire draft. The committed shape is identical to a
    /// freehand polygon (`MaskShape::Polygon`) so downstream sampling
    /// (`apply_mask_alpha`, FFmpeg export) works unchanged — what
    /// differs is *how* the user lays the points down.
    SegmentMask,
}

impl MaskTool {
    #[allow(dead_code)]
    pub fn is_active(self) -> bool { self != MaskTool::None }
    pub fn label(self) -> &'static str {
        match self {
            MaskTool::None => "Select",
            MaskTool::RectMask => "Rect mask",
            MaskTool::EllipseMask => "Ellipse mask",
            MaskTool::FreehandMask => "Freehand mask",
            MaskTool::Eyedropper => "Eyedropper mask",
            MaskTool::SegmentMask => "Segment mask",
        }
    }
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
    /// Active rubber-band (marquee) selection. The user pressed the
    /// primary button on an empty area of the canvas and is dragging to
    /// lasso multiple elements at once. World-pixel coordinates of the
    /// drag origin are kept here so the live `CanvasMarquee.start`
    /// stays anchored regardless of pan / zoom while the gesture is
    /// in flight.
    ///
    /// `extend` is set when the user held Ctrl/Shift/Cmd at drag-start
    /// — on commit, the lasso ADDS to the existing `canvas_selection`
    /// instead of replacing it.
    Marquee {
        start_world: [f32; 2],
        extend: bool,
    },
    /// Rotate the selected element around its centre. `start_angle_rad`
    /// is the angle from element-centre to pointer at drag start (radians).
    /// `initial_rot_deg` is the element's rotation at drag start.
    RotateSelection {
        initial_rot_deg: f32,
        center_screen: [f32; 2],
        start_angle_rad: f32,
    },
    /// Drawing a mask shape on top of the selected element. The
    /// pointer's start position (in element-local UV space, 0..1) is
    /// captured in `start_uv`; freehand mode also accumulates the
    /// per-frame pointer trail in `EditorState.mask_draft_points`. On
    /// release the active drag commits the resulting shape to the
    /// element's `effects` stack as the matching `EffectKind::Mask`
    /// or `EffectKind::ColorKey` entry.
    DrawMask {
        tool: MaskTool,
        start_uv: [f32; 2],
        /// Element selection the drag was started against. Captured on
        /// pointer-down so the commit lands on the same element even if
        /// the user clicks elsewhere mid-drag.
        target: Selection,
    },
}
