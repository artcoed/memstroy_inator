//! Main eframe application: wires panels together and dispatches jobs.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, Rounding, Stroke, Vec2, ViewportCommand};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::audio_engine::AudioEngine;
use crate::jobs::{spawn_render, JobEvent};
use crate::panels;
use crate::state::{EditorState, SceneExitAction, Selection};

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out = text.chars().take(keep).collect::<String>();
    out.push('…');
    out
}

pub struct App {
    rt: Option<Runtime>,
    state: EditorState,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
    /// Per-actor extraction results. Key = actor index.
    frame_extract_results: Vec<Arc<Mutex<Option<Result<(f32, usize, std::path::PathBuf), ()>>>>>,
    /// Per-audio-track waveform extraction results (`None` in slot = pending).
    waveform_extract_results: Vec<Arc<Mutex<Option<Option<(Vec<f32>, f32)>>>>>,
    /// Debounce rapid `reload_library` calls from background jobs.
    library_reload_debounce_until: Option<std::time::Instant>,
    /// True while a background library scan is running.
    library_reload_in_progress: bool,
    /// Audio playback engine
    audio_engine: AudioEngine,
    /// Previous playing state (for detecting transitions)
    was_playing: bool,
    /// Previous playhead for detecting seeks
    prev_playhead: f32,
    /// Previous spec signature so we can rebuild the engine the moment the
    /// user touches an audio inspector slider mid-playback. The hash
    /// covers every field that changes the audible output (volume, pan,
    /// pitch, filter cutoffs, reverb, mute, source list, …) so two
    /// frames with identical signatures don't restart the sinks.
    prev_audio_signature: u64,
    /// Cached source count — kept around for status reporting and quick
    /// "did we add a track" debug checks; the live-update logic itself
    /// uses the signature above.
    prev_audio_source_count: usize,
    /// Native pixels-per-point captured on the first frame. Used to
    /// restore system DPI when the user sets UI scale back to "Auto".
    native_ppp: Option<f32>,

    /// Number of V-key release events to ignore as "already-handled
    /// Ctrl+V chords". See [`Self::handle_shortcuts`] for why this
    /// exists — short version: `egui-winit` suppresses the V key
    /// PRESS event on Ctrl+V *when the OS clipboard contains no
    /// text* (e.g. screenshot-only data), so we synthesise paste
    /// from the V *release* event. When egui-winit DID push an
    /// `Event::Paste` (clipboard had text), the matching V release
    /// will arrive a few frames later and would otherwise trigger
    /// a second paste — so for every Paste we observe we reserve a
    /// release-skip slot here, decremented when that release lands.
    pending_v_release_skips: u32,

    /// Track whether Ctrl/Cmd was held during the most recent frames.
    /// Used as a fallback when the user releases Ctrl *before* V —
    /// the V release event then has `modifiers.command == false`,
    /// but we still want to treat that as a Ctrl+V chord. Counted
    /// down each frame; reset to a small grace window every frame
    /// where Ctrl is observed held.
    ctrl_held_grace_frames: u32,

    /// Wall-clock instant of the most recent internal Ctrl+C / Ctrl+X
    /// inside this app, used as the "last action wins" tiebreaker on
    /// Ctrl+V: when the OS clipboard contains BOTH an image and our
    /// in-app clipboard is non-empty (e.g. user copied an image in a
    /// browser, then immediately did Ctrl+C in our app to copy a
    /// canvas selection), the user's expectation is that the LATEST
    /// copy wins. If `last_internal_copy_at` is recent enough we
    /// duplicate the in-app clipboard items first; otherwise the OS
    /// clipboard image takes priority. `None` = no internal copy in
    /// this session, so OS clipboard always wins.
    last_internal_copy_at: Option<std::time::Instant>,

    /// Deferred tab switch / close / open / quit while the unsaved
    /// changes dialog is shown.
    pending_scene_exit: Option<SceneExitAction>,
    /// Set when the user chose "Don't save" on quit — next close
    /// request is allowed through without prompting again.
    force_app_close: bool,
    /// Cached autosave rows for the File menu. Scanned on a worker
    /// thread so opening the menu never blocks on filesystem metadata.
    autosave_menu_entries: Vec<crate::autosave::AutosaveEntry>,
    autosave_menu_loading: bool,
    autosave_menu_last_refresh: Option<std::time::Instant>,
}

fn audio_actor_overlap(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let actor_end = actor.t_out.unwrap_or(scene.output.duration);
    let audio_start = audio.t_in;
    let audio_end = audio.t_out.unwrap_or(scene.output.duration);
    (actor_end.min(audio_end) - actor_start.max(audio_start)).max(0.0)
}

fn audio_actor_gap(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let actor_end = actor.t_out.unwrap_or(scene.output.duration);
    let audio_start = audio.t_in;
    let audio_end = audio.t_out.unwrap_or(scene.output.duration);
    if audio_end < actor_start {
        actor_start - audio_end
    } else if actor_end < audio_start {
        audio_start - actor_end
    } else {
        0.0
    }
}

fn audio_actor_score(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let source_penalty = if actor.source == audio.source {
        0.0
    } else {
        10_000.0
    };
    let overlap_penalty = if audio_actor_overlap(scene, actor, audio) > 0.0 {
        0.0
    } else {
        1_000.0 + audio_actor_gap(scene, actor, audio)
    };
    source_penalty
        + overlap_penalty
        + (actor_start - audio.t_in).abs()
        + (actor.source_start - audio.source_start).abs() * 2.0
}

fn best_audio_actor_by_id(
    scene: &Scene,
    actor_id: &str,
    audio: &memstroy_core::AudioTrack,
) -> Option<usize> {
    scene
        .actors
        .iter()
        .enumerate()
        .filter(|(_, actor)| actor.id == actor_id)
        .min_by(|(_, a), (_, b)| {
            audio_actor_score(scene, a, audio).total_cmp(&audio_actor_score(scene, b, audio))
        })
        .map(|(idx, _)| idx)
}

fn infer_actor_for_audio_in_scene(scene: &Scene, audio_idx: usize) -> Option<usize> {
    let audio = scene.audio.get(audio_idx)?;
    if audio.deleted {
        return None;
    }

    if let Some(parent_id) = audio.parent_actor.as_deref() {
        if let Some(idx) = best_audio_actor_by_id(scene, parent_id, audio) {
            return Some(idx);
        }
    }

    if let Some(actor_id) = audio.id.strip_suffix("_audio") {
        if let Some(idx) = best_audio_actor_by_id(scene, actor_id, audio) {
            return Some(idx);
        }
    }

    scene
        .actors
        .iter()
        .enumerate()
        .filter(|(_, actor)| actor.source == audio.source)
        .filter(|(_, actor)| audio_actor_overlap(scene, actor, audio) > 0.0)
        .min_by(|(_, a), (_, b)| {
            audio_actor_score(scene, a, audio).total_cmp(&audio_actor_score(scene, b, audio))
        })
        .map(|(idx, _)| idx)
}

impl App {
    pub fn new(rt: Runtime) -> Self {
        let (tx, rx) = channel();
        let mut state = EditorState::new();
        state.tokio_handle = Some(rt.handle().clone());
        // Hand the JobEvent sender to the editor state so the canvas
        // paint loop can dispatch background image-effects bake jobs
        // (see `image_fx_worker::submit_image_fx_job`). Cloned so the
        // App keeps its own Sender for the existing job spawners.
        state.image_fx_tx = Some(tx.clone());
        state.reload_library();
        {
            let videos_dir = state.videos_dir();
            let assets_root = state.assets_root.clone();
            let tx_scan = tx.clone();
            rt.spawn(async move {
                let generated = crate::jobs::generate_video_library_thumbnails(&videos_dir).await;
                if generated == 0 {
                    return;
                }
                let snap = tokio::task::spawn_blocking(move || {
                    EditorState::scan_library_snapshot(assets_root)
                })
                .await
                .ok();
                if let Some(snap) = snap {
                    let _ = tx_scan.send(JobEvent::LibraryScanned(snap));
                }
            });
        }

        // ── Auto-bootstrap the local memstroy-assets-server ──
        // The Library panel's "Refresh" / shared-asset endpoints expect
        // a server running at `state.server_url`. Previously the user
        // had to remember to launch `cargo run -p memstroy-assets-server`
        // in a second terminal, and the GUI just printed connection
        // errors when they didn't. We now spin one up in-process on the
        // same tokio runtime, indexing whatever `assets/` directory the
        // editor is rooted at. If the bind fails (port already taken,
        // another server instance is already running, etc.) the GUI
        // still works — the network calls just talk to the existing
        // server through the same loopback URL.
        //
        // Client-distribution builds opt out of this entirely: the
        // bundle ships without an in-tree `assets/` dir or the
        // `memstroy-assets-server` binary, and `state.server_url` is
        // baked at compile time to point at the operator's remote
        // server. Spawning a local one would mask configuration
        // mistakes (the editor would silently serve "" assets from a
        // brand-new empty cache dir) instead of surfacing them.
        if crate::state::LIBRARY_LOCAL_ONLY {
            tracing::info!("library local-only mode: skipping assets-server bootstrap");
        } else if crate::build_info::IS_CLIENT_BUILD {
            tracing::info!(
                server_url = %state.server_url,
                "client build: skipping in-process assets-server, using remote"
            );
        } else {
            #[cfg(feature = "local-server")]
            Self::spawn_local_assets_server(rt.handle(), &state);

            #[cfg(not(feature = "local-server"))]
            tracing::info!("local-server feature disabled: skipping in-process assets-server");
        }

        // Construct the audio engine and immediately apply the master
        // volume from the persisted settings. That way the very first
        // playback obeys the user's saved level instead of the engine's
        // default 1.0.
        let mut audio_engine = AudioEngine::new();
        audio_engine.set_master_volume(state.settings.master_volume);

        // Recovery: if an autosave from a previous session exists we don't
        // pop a modal anymore. The autosave is exposed as a menu entry under
        // File > Open last autosave, so the user can open it on demand
        // without an interruption on launch.

        let mut app = Self {
            rt: Some(rt),
            state,
            tx,
            rx,
            frame_extract_results: Vec::new(),
            waveform_extract_results: Vec::new(),
            library_reload_debounce_until: None,
            library_reload_in_progress: false,
            audio_engine,
            was_playing: false,
            prev_playhead: 0.0,
            prev_audio_signature: 0,
            prev_audio_source_count: 0,
            native_ppp: None,
            pending_v_release_skips: 0,
            ctrl_held_grace_frames: 0,
            last_internal_copy_at: None,
            pending_scene_exit: None,
            force_app_close: false,
            autosave_menu_entries: Vec::new(),
            autosave_menu_loading: false,
            autosave_menu_last_refresh: None,
        };
        app.refresh_autosave_menu_async();
        app
    }

    /// If the active tab has edits, queue destructive exit actions and
    /// return false. Switching/opening scenes is intentionally immediate:
    /// autosave keeps recovery coverage, and the workflow often jumps
    /// between projects to copy/paste fragments.
    fn request_scene_exit(&mut self, action: SceneExitAction) -> bool {
        if self.pending_scene_exit.is_some() {
            return false;
        }
        let prompt_for_unsaved =
            matches!(action, SceneExitAction::Quit | SceneExitAction::CloseTab(_));
        if prompt_for_unsaved && self.state.active_tab_is_dirty() {
            self.pending_scene_exit = Some(action);
            false
        } else {
            self.commit_scene_exit(action);
            true
        }
    }

    fn commit_scene_exit(&mut self, action: SceneExitAction) {
        let needs_close = matches!(action, SceneExitAction::Quit);
        self.state.apply_scene_exit_action(action);
        if needs_close {
            self.force_app_close = true;
        }
    }

    fn refresh_autosave_menu_async(&mut self) {
        if self.autosave_menu_loading {
            return;
        }
        self.autosave_menu_loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let entries = crate::autosave::list_entries();
            let _ = tx.send(JobEvent::AutosavesListed(entries));
        });
    }

    /// Try to save the active tab for an exit flow. Returns true when
    /// the scene has no remaining unsaved edits.
    fn save_active_tab_for_exit(&mut self) -> bool {
        self.save_scene();
        !self.state.active_tab_is_dirty()
    }

    fn show_unsaved_changes_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_scene_exit.clone() else {
            return;
        };

        let mut dismiss = false;
        let mut save_and_proceed = false;
        let mut discard_and_proceed = false;

        egui::Window::new(crate::i18n::t("Unsaved changes"))
            .id(egui::Id::new("unsaved_changes_dialog"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.label(crate::i18n::t(
                    "This scene has changes that have not been saved. Save before leaving?",
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(crate::i18n::t("Save")).clicked() {
                        save_and_proceed = true;
                    }
                    if ui.button(crate::i18n::t("Don't save")).clicked() {
                        discard_and_proceed = true;
                    }
                    if ui.button(crate::i18n::t("Cancel")).clicked() {
                        dismiss = true;
                    }
                });
            });

        if dismiss {
            self.pending_scene_exit = None;
            return;
        }
        if save_and_proceed {
            if self.save_active_tab_for_exit() {
                let action = self.pending_scene_exit.take().unwrap_or(action);
                self.commit_scene_exit(action);
                if self.force_app_close {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
            return;
        }
        if discard_and_proceed {
            let action = self.pending_scene_exit.take().unwrap_or(action);
            self.commit_scene_exit(action);
            if self.force_app_close {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    /// Spin up a `memstroy-assets-server` instance on the same tokio
    /// runtime as the GUI, parsing the address out of `state.server_url`.
    /// Failures (bad URL, port already bound by another instance, etc.)
    /// are logged but do not abort start-up — the GUI's HTTP calls fall
    /// through to whatever (if anything) is already listening on that
    /// port, which keeps developer workflows where the server is run
    /// separately working unchanged.
    ///
    /// Only available when the `local-server` feature is enabled. Client
    /// builds (via `scripts/package-client.ps1`) disable this feature to
    /// avoid pulling in the heavy `memstroy-assets-server` dependency tree.
    #[cfg(feature = "local-server")]
    fn spawn_local_assets_server(handle: &tokio::runtime::Handle, state: &EditorState) {
        // Parse `host:port` out of `state.server_url`. We accept either
        // `http://host:port` or just `host:port`.
        let raw = state.server_url.trim();
        let stripped = raw
            .strip_prefix("http://")
            .or_else(|| raw.strip_prefix("https://"))
            .unwrap_or(raw)
            .trim_end_matches('/')
            .trim_end_matches("/api");
        let host_port = stripped.split('/').next().unwrap_or(stripped);
        let addr: std::net::SocketAddr = match host_port.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    server_url = %raw,
                    error = %e,
                    "could not parse asset server URL — skipping in-process bootstrap"
                );
                return;
            }
        };

        // Anchor the asset root at the editor's current `assets_root` so
        // the server indexes the same files the GUI's local library tab
        // surfaces. The GUI now uses `~/.memstroy/cache/` directly as
        // assets_root, so we pass it directly to the server without
        // appending "assets" subdirectory.
        let root = state.assets_root.clone();
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!(
                root = %root.display(),
                error = %e,
                "could not create asset root for in-process server"
            );
            return;
        }

        // The server is fully async, so spawn it onto our runtime. Any
        // bind error inside `start()` is logged by the server itself
        // and the join handle simply resolves — the GUI keeps running.
        let store = memstroy_assets_server::AssetStore::new();
        if let Err(e) = store.index_dir(&root) {
            tracing::warn!(
                root = %root.display(),
                error = %e,
                "failed to index asset root for in-process server"
            );
        }
        let _ = handle.enter();
        let _join = handle.spawn(async move {
            let _ = memstroy_assets_server::start(addr, store).await;
        });
        tracing::info!(
            addr = %addr,
            root = %root.display(),
            "spawned in-process memstroy-assets-server"
        );
    }

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                JobEvent::Status(s) => self.state.status = s,
                JobEvent::RenderLog(line) => {
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.last_log = line.clone();
                        // Parse progress from CPU compositor log lines:
                        //
                        //   "[12.5%] Encoding 120 frames..."   ← stage update
                        //   "frame 30/120 (25.0%)"             ← per-frame
                        //
                        // Falling back to the legacy ffmpeg formats
                        // (`frame=  120` / `time=00:00:04.00`) for
                        // users who flip `MEMSTROY_RENDER_BACKEND=ffmpeg`.
                        if let Some(p) = parse_compositor_percent(&line) {
                            rp.progress = (p / 100.0).clamp(0.0, 1.0);
                        } else if let Some((cur, total)) = parse_compositor_frame(&line) {
                            if total > 0 {
                                rp.progress = (cur as f32 / total as f32).clamp(0.0, 1.0);
                            }
                        } else if let Some(time_progress) = parse_ffmpeg_time(&line) {
                            let total = self.state.scene.output.duration;
                            if total > 0.0 {
                                rp.progress = (time_progress / total).clamp(0.0, 1.0);
                            }
                        } else if let Some(frame_num) = parse_ffmpeg_frame(&line) {
                            let total_frames = (self.state.scene.output.duration
                                * self.state.scene.output.fps as f32)
                                as u32;
                            if total_frames > 0 {
                                rp.progress =
                                    (frame_num as f32 / total_frames as f32).clamp(0.0, 1.0);
                            }
                        }
                    }
                }
                JobEvent::RenderOutputChosen(Some(path)) => {
                    self.start_render_to_path(path);
                }
                JobEvent::RenderOutputChosen(None) => {
                    if self
                        .state
                        .status
                        .contains(crate::i18n::t("Choosing export path"))
                    {
                        self.state.status.clear();
                    }
                }
                JobEvent::RenderFinished(Ok(p)) => {
                    self.state.status =
                        format!("{} {}", crate::i18n::t("\u{2705} Rendered:"), p.display());
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                        rp.progress = 1.0;
                        // Freeze the elapsed counter at the moment the
                        // render actually finished. Without this the
                        // window keeps ticking the timer until the
                        // user clicks Close, which is exactly the
                        // "после рендера видео в окне рендера
                        // останавливай счетчик" report.
                        if rp.finished_elapsed.is_none() {
                            rp.finished_elapsed = Some(rp.started.elapsed());
                        }
                    }
                }
                JobEvent::RenderFinished(Err(e)) => {
                    self.state.status =
                        format!("{} {}", crate::i18n::t("\u{274C} Render failed:"), e);
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                        rp.error = Some(e);
                        if rp.finished_elapsed.is_none() {
                            rp.finished_elapsed = Some(rp.started.elapsed());
                        }
                    }
                }
                JobEvent::RefreshProgress(msg) => {
                    self.state.status = format!("↻ {}", msg);
                }
                JobEvent::RefreshLibraryReloaded(msg) => {
                    // Debounce rescans so metadata batches don't stall
                    // the UI thread on every single clip.
                    self.state.status = format!("↻ {}", msg);
                    self.state.library_reload_pending = true;
                    self.library_reload_debounce_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(350));
                }
                JobEvent::RefreshFinished(Ok(summary)) => {
                    self.state.refreshing = false;
                    self.state.library_reload_pending = true;
                    self.library_reload_debounce_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(350));
                    self.state.status = format!(
                        "{} {} {}, {} {}",
                        crate::i18n::t("Refresh done!"),
                        summary.new_clips,
                        crate::i18n::t("new clips,"),
                        summary.total_clips,
                        crate::i18n::t("total in library"),
                    );
                    if summary.failed > 0 {
                        self.state.status.push_str(&format!(
                            " ({} {})",
                            summary.failed,
                            crate::i18n::t("failed"),
                        ));
                    }
                }
                JobEvent::RefreshFinished(Err(e)) => {
                    self.state.refreshing = false;
                    self.state.status =
                        format!("{} {}", crate::i18n::t("\u{274C} Refresh failed:"), e);
                }
                JobEvent::WebSearchFinished {
                    page_offset,
                    target,
                    result,
                } => match target {
                    crate::jobs::WebSearchTarget::Panel => {
                        self.state.web_image_search.searching = false;
                        match result {
                            Ok((mut hits, next_offset)) => {
                                let n = hits.len();
                                let is_first_page = page_offset == 0;
                                if is_first_page {
                                    self.state.web_image_search.results = hits;
                                } else {
                                    let cap = crate::web_image_search::MAX_TOTAL_RESULTS;
                                    let cur = self.state.web_image_search.results.len();
                                    if cur < cap {
                                        let take = (cap - cur).min(hits.len());
                                        if take < hits.len() {
                                            hits.truncate(take);
                                        }
                                        self.state.web_image_search.results.extend(hits);
                                    }
                                }
                                self.state.web_image_search.next_offset =
                                    next_offset.filter(|&o| o != page_offset);
                                if !is_first_page {
                                    self.state.web_image_search.page_count += 1;
                                }
                                self.state.web_image_search.status = if n == 0 {
                                    if is_first_page {
                                        crate::i18n::t("No results.").to_string()
                                    } else {
                                        crate::i18n::t("(no more results)").to_string()
                                    }
                                } else if is_first_page {
                                    format!(
                                        "{} {} {}.",
                                        crate::i18n::t("Got"),
                                        n,
                                        crate::i18n::t("result(s)")
                                    )
                                } else {
                                    format!(
                                        "{} +{} ({} {})",
                                        "+",
                                        n,
                                        crate::i18n::t("total"),
                                        self.state.web_image_search.results.len()
                                    )
                                };
                            }
                            Err(e) => {
                                if page_offset == 0 {
                                    self.state.web_image_search.results.clear();
                                }
                                self.state.web_image_search.next_offset = None;
                                self.state.web_image_search.status = format!("Error: {}", e);
                            }
                        }
                    }
                    crate::jobs::WebSearchTarget::Canvas => {
                        crate::canvas_image_search::ingest_canvas_search_result(
                            &mut self.state,
                            page_offset,
                            result,
                        );
                    }
                },
                JobEvent::WebImageDownloaded {
                    request_id: _,
                    image_url,
                    place_on_canvas,
                    result,
                } => {
                    // Update the matching row in the panel (downloading
                    // flag, local_path, last_error) regardless of
                    // outcome so the spinner clears and the user can
                    // see the error tooltip if any.
                    crate::web_image_search::ingest_download_result(
                        &mut self.state.web_image_search,
                        &image_url,
                        &result,
                    );
                    if self.state.canvas_image_search.is_some() {
                        let canvas_hit = self
                            .state
                            .canvas_image_search
                            .as_ref()
                            .and_then(|s| {
                                s.results
                                    .iter()
                                    .any(|h| h.image_url == image_url)
                                    .then_some(())
                            })
                            .is_some();
                        if canvas_hit {
                            if let Ok(ref asset) = result {
                                let new_lib_asset = crate::state::LibraryAsset {
                                    id: asset.id.clone(),
                                    path: asset.path.clone(),
                                    label: asset.label.clone(),
                                    thumbnail: asset.thumbnail.clone(),
                                    downloaded: true,
                                    server_id: None,
                                    duration_secs: None,
                                    width: None,
                                    height: None,
                                };
                                if !self
                                    .state
                                    .library
                                    .images
                                    .iter()
                                    .any(|a| a.path == new_lib_asset.path)
                                {
                                    self.state.library.images.push(new_lib_asset);
                                    self.state.library.images.sort_by(|a, b| {
                                        a.label
                                            .to_ascii_lowercase()
                                            .cmp(&b.label.to_ascii_lowercase())
                                    });
                                }
                                self.state.library_dir_fingerprint =
                                    self.state.compute_library_dir_fingerprint();
                            }
                            crate::canvas_image_search::ingest_canvas_download_result(
                                &mut self.state,
                                &image_url,
                                &result,
                            );
                            return;
                        }
                    }
                    match result {
                        Ok(asset) => {
                            // Instead of a full reload_library() which
                            // rebuilds all asset lists and causes every
                            // thumbnail to re-enter the load budget queue,
                            // just append the new asset to the images list
                            // and update the fingerprint so the periodic
                            // auto-rescan doesn't trigger a redundant full
                            // reload on the next tick.
                            let new_lib_asset = crate::state::LibraryAsset {
                                id: asset.id.clone(),
                                path: asset.path.clone(),
                                label: asset.label.clone(),
                                thumbnail: asset.thumbnail.clone(),
                                downloaded: true,
                                server_id: None,
                                duration_secs: None,
                                width: None,
                                height: None,
                            };
                            // Only add if not already present (avoid dupes
                            // if the auto-rescan raced us).
                            if !self
                                .state
                                .library
                                .images
                                .iter()
                                .any(|a| a.path == new_lib_asset.path)
                            {
                                self.state.library.images.push(new_lib_asset);
                                self.state.library.images.sort_by(|a, b| {
                                    a.label
                                        .to_ascii_lowercase()
                                        .cmp(&b.label.to_ascii_lowercase())
                                });
                            }
                            // Bump the fingerprint so the periodic rescan
                            // doesn't immediately trigger a full rebuild.
                            self.state.library_dir_fingerprint =
                                self.state.compute_library_dir_fingerprint();
                            self.state.last_library_rescan = Some(std::time::Instant::now());

                            if self.state.asset_drag.pending_web_image_url.as_deref()
                                == Some(image_url.as_str())
                            {
                                self.state.asset_drag.dragging = Some(asset.path.clone());
                                self.state.asset_drag.pending_web_image_url = None;
                                self.state.asset_drag.kind = crate::state::AssetDragKind::Image;
                                self.state.asset_drag.label = asset.label.clone();
                                self.state.asset_drag.thumbnail =
                                    asset.thumbnail.clone().or_else(|| Some(asset.path.clone()));
                                self.state.asset_drag.server_id = None;
                                self.state.asset_drag.downloaded = true;
                                self.state.asset_drag.duration_secs = None;
                                self.state.asset_drag.width = None;
                                self.state.asset_drag.height = None;
                            }

                            self.state.web_image_search.status =
                                format!("{} -> {}", crate::i18n::t("Saved"), asset.label);
                            if place_on_canvas {
                                let _idx = self.state.add_image_overlay_at_playhead(&asset);
                                self.state.library_tab = crate::state::LibraryTab::Images;
                                self.state.status =
                                    format!("{} -> {}", crate::i18n::t("Web image"), asset.label,);
                            }
                        }
                        Err(e) => {
                            self.state.web_image_search.status =
                                format!("{} {}", crate::i18n::t("Download failed:"), e);
                        }
                    }
                }
                JobEvent::AiBgRemoveFinished {
                    overlay_idx,
                    path,
                    result,
                } => {
                    crate::canvas_image_search::ingest_ai_bg_remove_result(
                        &mut self.state,
                        overlay_idx,
                        &path,
                        result,
                    );
                }
                JobEvent::ImageFxReady(result) => {
                    self.handle_image_fx_ready(ctx, result);
                }
                JobEvent::ClipDownloaded {
                    server_id,
                    result,
                    drop_target,
                } => {
                    self.handle_clip_downloaded(server_id, result, drop_target);
                }
                JobEvent::ServerAssetsPageLoaded {
                    tab,
                    query,
                    offset: _,
                    limit: _,
                    result,
                } => match result {
                    Ok(page) => {
                        let count = page.items.len();
                        self.state.ingest_server_assets_page(tab, &query, page);
                        self.state.status = if count == 0 {
                            crate::i18n::t("No more server assets.").into()
                        } else {
                            format!("{} {}", crate::i18n::t("Loaded server assets:"), count)
                        };
                    }
                    Err(e) => {
                        self.state
                            .set_server_assets_page_error(tab, &query, e.clone());
                        self.state.status =
                            format!("{} {}", crate::i18n::t("Server assets failed:"), e);
                    }
                },
                JobEvent::CanvasServerAssetsLoaded {
                    kind,
                    query,
                    result,
                } => {
                    crate::canvas_image_search::ingest_canvas_server_assets_result(
                        &mut self.state,
                        kind,
                        &query,
                        result,
                    );
                }
                JobEvent::ServerAssetCountsLoaded { result } => match result {
                    Ok(counts) => {
                        let first_load = self.state.server_asset_counts.is_none();
                        let total = counts.total;
                        self.state.apply_server_asset_counts(counts);
                        if first_load {
                            self.state.status =
                                format!("{} {}", crate::i18n::t("Server assets:"), total);
                        }
                    }
                    Err(e) => {
                        self.state.set_server_asset_counts_error(e.clone());
                        self.state.status =
                            format!("{} {}", crate::i18n::t("Server counts failed:"), e);
                    }
                },
                JobEvent::ServerAssetPreviewLoaded {
                    tab,
                    server_id,
                    result,
                } => match result {
                    Ok(path) => {
                        self.state.mark_server_preview_loaded(tab, &server_id, path);
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        self.state.mark_server_preview_failed(tab, &server_id, e);
                    }
                },
                JobEvent::ServerAssetDownloaded {
                    server_id,
                    kind,
                    result,
                    drop_target,
                } => {
                    self.handle_server_asset_downloaded(server_id, kind, result, drop_target);
                }
                JobEvent::VideoDurationProbed {
                    actor_id,
                    path,
                    duration,
                } => {
                    self.handle_video_duration_probed(actor_id, path, duration);
                }
                JobEvent::AutosavesListed(entries) => {
                    self.autosave_menu_entries = entries;
                    self.autosave_menu_loading = false;
                    self.autosave_menu_last_refresh = Some(std::time::Instant::now());
                }
                JobEvent::LibraryScanned(snap) => {
                    self.library_reload_in_progress = false;
                    self.state.apply_library_snapshot(snap);
                }
                JobEvent::LibraryReloadAborted => {
                    self.library_reload_in_progress = false;
                }
            }
        }
    }

    /// Finalise a lazy clip download. Reload the library so the new
    /// `.mp4` file replaces its server-stub entry, then if a drop
    /// target was specified, spawn an actor / canvas drop at that
    /// position.
    fn handle_clip_downloaded(
        &mut self,
        server_id: String,
        result: Result<std::path::PathBuf, String>,
        drop_target: crate::jobs::ClipDropTarget,
    ) {
        match result {
            Ok(path) => {
                self.state.pending_clip_downloads.remove(&path);
                self.schedule_library_reload();

                // Fast path: the file is already readable, place the
                // actor immediately.
                if EditorState::is_usable_local_video(&path) {
                    self.place_drop_target(&path, drop_target);
                    self.invalidate_preview_for_path(&path);
                    self.state.status = format!(
                        "{} {}",
                        crate::i18n::t("\u{2B07} Clip downloaded:"),
                        server_id
                    );
                    return;
                }

                // Slow path: the file exists but isn't yet usable.
                // Don't block the UI thread — queue the drop for the
                // next frame(s) to retry. flush_deferred_clip_placements
                // will pick it up once is_usable_local_video flips to
                // true (or drop it after the 10-second deadline).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                self.state.deferred_clip_placements.push((
                    path.clone(),
                    drop_target,
                    server_id.clone(),
                    deadline,
                ));
                self.state.status = format!(
                    "{} {}",
                    crate::i18n::t("\u{23F3} Waiting for clip to finalize:"),
                    server_id
                );
            }
            Err(e) => {
                let safe = crate::jobs::sanitise_id(&server_id);
                self.state.pending_clip_downloads.retain(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|stem| stem != safe)
                        .unwrap_or(true)
                });
                self.state.status = format!(
                    "{} {}: {}",
                    crate::i18n::t("\u{274C} Clip download failed:"),
                    server_id,
                    e
                );
            }
        }
    }

    /// Place a finished clip download according to its original drop
    /// target. Shared between the immediate path and the deferred queue
    /// retry.
    fn place_drop_target(
        &mut self,
        path: &std::path::Path,
        drop_target: crate::jobs::ClipDropTarget,
    ) {
        use crate::jobs::ClipDropTarget;
        let pb = path.to_path_buf();
        match drop_target {
            ClipDropTarget::CanvasAt { world_x, world_y } => {
                crate::panels::add_actor_from_clip_at_canvas(
                    &mut self.state,
                    &pb,
                    [world_x, world_y],
                );
            }
            ClipDropTarget::TimelineAt { t } => {
                crate::panels::add_actor_from_clip_at_time(&mut self.state, &pb, t);
            }
            ClipDropTarget::ExistingActor { actor_id } => {
                if !crate::panels::finalize_pending_actor_download(&mut self.state, &actor_id, &pb)
                {
                    self.state.status =
                        crate::i18n::t("Downloaded placeholder disappeared.").into();
                }
            }
            ClipDropTarget::SequenceSlot { actor_id } => {
                if let Some(idx) = self
                    .state
                    .scene
                    .actors
                    .iter()
                    .position(|actor| actor.id == actor_id)
                {
                    let _ = crate::panels::fill_footage_sequence_slot(&mut self.state, idx, &pb);
                } else {
                    self.state.status = crate::i18n::t("Footage slot disappeared.").into();
                }
            }
            ClipDropTarget::None => {}
        }
    }

    fn handle_server_asset_downloaded(
        &mut self,
        server_id: String,
        kind: crate::state::AssetDragKind,
        result: Result<std::path::PathBuf, String>,
        drop_target: crate::jobs::ServerAssetDropTarget,
    ) {
        match result {
            Ok(path) => {
                self.state.pending_clip_downloads.remove(&path);
                self.state
                    .mark_server_asset_downloaded(kind, &server_id, path.clone());
                self.place_server_asset_drop_target(&path, kind, drop_target);
                self.invalidate_preview_for_path(&path);
                self.state.status = format!(
                    "{} {}",
                    crate::i18n::t("Server asset downloaded:"),
                    server_id
                );
            }
            Err(e) => {
                self.state.pending_clip_downloads.retain(|p| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|stem| stem != crate::jobs::sanitise_id(&server_id))
                        .unwrap_or(true)
                });
                self.state.status = format!(
                    "{} {}: {}",
                    crate::i18n::t("Server asset download failed:"),
                    server_id,
                    e
                );
            }
        }
    }

    fn place_server_asset_drop_target(
        &mut self,
        path: &std::path::Path,
        kind: crate::state::AssetDragKind,
        drop_target: crate::jobs::ServerAssetDropTarget,
    ) {
        let path = path.to_path_buf();
        match drop_target {
            crate::jobs::ServerAssetDropTarget::CanvasAt { world_x, world_y } => match kind {
                crate::state::AssetDragKind::Video => {
                    crate::panels::add_actor_from_video_at_canvas(
                        &mut self.state,
                        &path,
                        [world_x, world_y],
                    );
                }
                crate::state::AssetDragKind::Image | crate::state::AssetDragKind::Particle => {
                    let asset = library_asset_from_download(&path, kind);
                    crate::panels::add_library_asset_at_playhead(&mut self.state, &asset, kind);
                    position_last_overlay_at_world(&mut self.state, [world_x, world_y]);
                }
                crate::state::AssetDragKind::Sound => {
                    let asset = library_asset_from_download(&path, kind);
                    crate::panels::add_library_asset_at_playhead(&mut self.state, &asset, kind);
                }
                _ => {}
            },
            crate::jobs::ServerAssetDropTarget::TimelineAt { t, track_idx } => {
                let saved_t = self.state.playhead;
                self.state.playhead = t;
                match kind {
                    crate::state::AssetDragKind::Video => {
                        if let Some(new_idx) =
                            crate::panels::add_actor_from_video_at_time(&mut self.state, &path, t)
                        {
                            if let Some(track_idx) = track_idx {
                                self.state
                                    .actor_track_assignments
                                    .insert(new_idx, track_idx);
                            }
                        }
                    }
                    crate::state::AssetDragKind::Image
                    | crate::state::AssetDragKind::Particle
                    | crate::state::AssetDragKind::Sound => {
                        let asset = library_asset_from_download(&path, kind);
                        crate::panels::add_library_asset_at_playhead(&mut self.state, &asset, kind);
                        if let Some(track_idx) = track_idx {
                            match (kind, self.state.selection) {
                                (
                                    crate::state::AssetDragKind::Image
                                    | crate::state::AssetDragKind::Particle,
                                    crate::state::Selection::Overlay(new_idx),
                                ) => {
                                    self.state
                                        .overlay_track_assignments
                                        .insert(new_idx, track_idx);
                                }
                                (
                                    crate::state::AssetDragKind::Sound,
                                    crate::state::Selection::Audio(new_idx),
                                ) => {
                                    self.state
                                        .audio_track_assignments
                                        .insert(new_idx, track_idx);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                self.state.playhead = saved_t;
            }
            crate::jobs::ServerAssetDropTarget::None => {}
            crate::jobs::ServerAssetDropTarget::ExistingActor { actor_id } => {
                if !crate::panels::finalize_pending_actor_download(
                    &mut self.state,
                    &actor_id,
                    &path,
                ) {
                    self.state.status =
                        crate::i18n::t("Downloaded placeholder disappeared.").into();
                }
            }
        }
    }

    /// Retry any clip drops whose download finished but whose .mp4
    /// wasn't readable yet on the frame the event arrived. Called once
    /// per UI frame from `update()`.
    fn flush_deferred_clip_placements(&mut self) {
        if self.state.deferred_clip_placements.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let pending = std::mem::take(&mut self.state.deferred_clip_placements);
        let mut keep = Vec::with_capacity(pending.len());
        for (path, drop_target, server_id, deadline) in pending {
            if EditorState::is_usable_local_video(&path) {
                self.place_drop_target(&path, drop_target);
                self.invalidate_preview_for_path(&path);
                self.state.status = format!(
                    "{} {}",
                    crate::i18n::t("\u{2B07} Clip downloaded:"),
                    server_id
                );
            } else if now >= deadline {
                let detail = if path.is_file() {
                    std::fs::metadata(&path)
                        .map(|m| format!("{} bytes", m.len()))
                        .unwrap_or_else(|_| "size unknown".into())
                } else {
                    "file missing".into()
                };
                self.state.status = format!(
                    "{} {} ({}): {}",
                    crate::i18n::t("\u{274C} Clip download incomplete:"),
                    server_id,
                    detail,
                    path.display()
                );
            } else {
                keep.push((path, drop_target, server_id, deadline));
            }
        }
        self.state.deferred_clip_placements = keep;
    }

    /// Reset preview caches for any layer using `path` and queue ffmpeg warmup.
    fn invalidate_preview_for_path(&mut self, path: &std::path::Path) {
        for (idx, actor) in self.state.scene.actors.iter().enumerate() {
            if actor.source != path {
                continue;
            }
            if idx < self.state.frame_caches.len() {
                let source = actor.source.clone();
                self.state.frame_caches[idx] = crate::video_cache::FrameCache::new(source, idx);
            }
            if idx < self.frame_extract_results.len() {
                if let Ok(mut slot) = self.frame_extract_results[idx].lock() {
                    *slot = None;
                }
            }
        }
        for (idx, track) in self.state.scene.audio.iter().enumerate() {
            if track.source == path && idx < self.state.audio_waveforms.len() {
                self.state.audio_waveforms[idx] = crate::state::AudioWaveform::default();
                if idx < self.waveform_extract_results.len() {
                    if let Ok(mut slot) = self.waveform_extract_results[idx].lock() {
                        *slot = None;
                    }
                }
            }
        }
        self.state.request_media_preview = true;
    }

    fn handle_video_duration_probed(
        &mut self,
        actor_id: String,
        path: std::path::PathBuf,
        duration: Option<f32>,
    ) {
        self.state.duration_probe_pending.remove(&path);
        let Some(duration) = duration.filter(|d| *d > 0.01) else {
            return;
        };
        self.state
            .video_duration_cache
            .insert(path.clone(), duration);
        if self.state.asset_drag.dragging.as_ref() == Some(&path)
            && self.state.asset_drag.duration_secs.is_none()
        {
            self.state.asset_drag.duration_secs = Some(duration);
        }
        let mut touched = Vec::new();
        for (idx, actor) in self.state.scene.actors.iter_mut().enumerate() {
            if actor.source != path {
                continue;
            }
            if !actor_id.is_empty() && actor.id != actor_id {
                continue;
            }
            crate::split_crop::reconcile_actor_t_out_for_source(actor, duration);
            touched.push(idx);
        }
        for idx in touched {
            crate::panels::sync_audio_to_actor(&mut self.state, idx);
        }
        self.state.request_media_preview = true;
    }

    /// Coalesce rapid `reload_library` requests from background workers
    /// so metadata sync does not stall the UI thread every few clips.
    fn schedule_library_reload(&mut self) {
        self.state.library_reload_pending = true;
        const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(350);
        self.library_reload_debounce_until = Some(std::time::Instant::now() + DEBOUNCE);
    }

    fn flush_pending_library_reload(&mut self) {
        if !self.state.library_reload_pending || self.library_reload_in_progress {
            return;
        }
        let due = self
            .library_reload_debounce_until
            .map(|t| std::time::Instant::now() >= t)
            .unwrap_or(true);
        if !due {
            return;
        }
        self.state.library_reload_pending = false;
        self.library_reload_debounce_until = None;
        self.library_reload_in_progress = true;
        let assets_root = self.state.assets_root.clone();
        let tx = self.tx.clone();
        self.rt.as_ref().unwrap().spawn(async move {
            let videos_dir = assets_root.join("assets").join("videos");
            let _ = crate::jobs::generate_video_library_thumbnails(&videos_dir).await;
            let snap = tokio::task::spawn_blocking(move || {
                EditorState::scan_library_snapshot(assets_root)
            })
            .await
            .ok();
            if let Some(snap) = snap {
                let _ = tx.send(JobEvent::LibraryScanned(snap));
            } else {
                let _ = tx.send(JobEvent::LibraryReloadAborted);
            }
        });
    }

    fn maybe_request_library_reload_repaint(&self, ctx: &egui::Context) {
        if self.state.library_reload_pending
            && self
                .library_reload_debounce_until
                .is_some_and(|t| std::time::Instant::now() < t)
        {
            ctx.request_repaint();
        }
    }

    /// Finalise an image-effects bake by uploading the RGBA buffer as
    /// an `egui::TextureHandle` (which has to happen on the UI thread)
    /// and stashing the result in `state.image_fx_cache` keyed by
    /// `(path, sig)`. Failures are stored too so the canvas knows not
    /// to keep retrying the same broken bake every frame.
    fn handle_image_fx_ready(
        &mut self,
        ctx: &egui::Context,
        result: crate::image_fx_worker::ImageFxResult,
    ) {
        match result.outcome {
            Ok(baked) => {
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [baked.width as usize, baked.height as usize],
                    &baked.rgba,
                );
                let name = format!(
                    "img_overlay_fx_{}_{:x}",
                    result
                        .path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("anon"),
                    result.sig,
                );
                let texture = ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR);
                let slot = crate::image_fx_cache::ImageFxSlot {
                    texture,
                    size: [baked.width, baked.height],
                    crop: baked.crop,
                };
                self.state
                    .image_fx_cache
                    .put_ready(result.path, result.sig, slot);
            }
            Err(reason) => {
                self.state
                    .image_fx_cache
                    .put_failed(result.path, result.sig, reason);
            }
        }
    }

    /// Modern scrolling tab strip. Each tab is a rounded pill with hover &
    /// active states, an inline rename mode (double-click), and a compact
    /// close button. Wraps in a horizontal scroll area so many tabs stay
    /// usable without breaking the toolbar layout.
    fn scene_tab_bar(&mut self, ui: &mut egui::Ui) {
        let num_tabs = self.state.scene_tabs.len();
        let mut switch_to: Option<usize> = None;
        let mut close_tab: Option<usize> = None;
        let mut commit_rename: Option<(usize, String)> = None;
        let mut start_rename: Option<(usize, String)> = None;
        let active = self.state.active_tab;
        let editing = self.state.editing_tab_idx;

        let avail_w = ui.available_width();
        let plus_w = 30.0_f32;
        let scroll_w = (avail_w - plus_w - 8.0).max(120.0);

        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal()
                .id_source("scene_tabs_scroll")
                .max_width(scroll_w)
                .auto_shrink([true, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    for i in 0..num_tabs {
                        let is_active = i == active;
                        let is_editing = editing == Some(i);
                        let tab_name = self.state.scene_tabs[i].name.clone();
                        let dirty = self.state.tab_is_dirty(i);

                        let (fill, stroke_col, text_col, accent) = if is_active {
                            (
                                Color32::from_rgb(44, 42, 28),
                                Color32::from_rgb(255, 242, 0),
                                Color32::from_rgb(255, 255, 255),
                                Some(Color32::from_rgb(255, 242, 0)),
                            )
                        } else {
                            (
                                Color32::from_rgb(30, 28, 18),
                                Color32::from_rgb(54, 52, 36),
                                Color32::from_rgb(160, 160, 180),
                                None,
                            )
                        };

                        let frame = egui::Frame::none()
                            .fill(fill)
                            .rounding(Rounding::same(7.0))
                            .stroke(Stroke::new(1.0, stroke_col))
                            .inner_margin(egui::Margin {
                                left: 10.0,
                                right: 6.0,
                                top: 4.0,
                                bottom: 4.0,
                            });

                        let inner = frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if let Some(col) = accent {
                                    let (dot_rect, _) = ui.allocate_exact_size(
                                        Vec2::splat(7.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(dot_rect.center(), 3.5, col);
                                }

                                if is_editing {
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut self.state.editing_tab_buf)
                                            .desired_width(120.0)
                                            .font(egui::TextStyle::Body),
                                    );
                                    resp.request_focus();
                                    let lost_focus = resp.lost_focus();
                                    let enter_pressed =
                                        ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let escape_pressed =
                                        ui.input(|i| i.key_pressed(egui::Key::Escape));
                                    if escape_pressed {
                                        commit_rename = Some((usize::MAX, String::new()));
                                    } else if enter_pressed || lost_focus {
                                        commit_rename =
                                            Some((i, self.state.editing_tab_buf.clone()));
                                    }
                                } else {
                                    let full_label = if dirty {
                                        format!("\u{2022} {}", tab_name)
                                    } else {
                                        tab_name.clone()
                                    };
                                    let label = truncate_chars(&full_label, 28);
                                    let resp = ui.add_sized(
                                        [150.0, 18.0],
                                        egui::Label::new(
                                            RichText::new(label).size(12.0).color(text_col),
                                        )
                                        .truncate()
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    );
                                    let resp = if full_label.chars().count() > 28 {
                                        resp.on_hover_text(full_label)
                                    } else {
                                        resp
                                    };
                                    if resp.clicked() && !is_active {
                                        switch_to = Some(i);
                                        close_tab = None;
                                    }
                                    if resp.double_clicked() {
                                        start_rename = Some((i, tab_name.clone()));
                                    }
                                }

                                // Close button is always rendered — closing the
                                // last tab resets it to a fresh "Untitled" so
                                // the user always has a working scene.
                                let close_btn = egui::Button::new(
                                    RichText::new("\u{00D7}").size(13.0).color(if is_active {
                                        Color32::from_rgb(220, 220, 240)
                                    } else {
                                        Color32::from_rgb(120, 120, 140)
                                    }),
                                )
                                .frame(false)
                                .min_size(Vec2::new(16.0, 16.0));
                                let close_resp = ui.add(close_btn);
                                let close_resp = if num_tabs > 1 {
                                    close_resp.on_hover_text(crate::i18n::t("Close tab"))
                                } else {
                                    close_resp.on_hover_text(crate::i18n::t(
                                        "Reset to a fresh untitled scene",
                                    ))
                                };
                                if close_resp.clicked() {
                                    close_tab = Some(i);
                                    // Make sure a same-frame click on the
                                    // surrounding frame (which used to fall
                                    // through to switch_to) doesn't override
                                    // the close request.
                                    switch_to = None;
                                }
                            });
                        });
                        // The close button and the label both have their own
                        // click handling above. Re-interacting on the whole
                        // frame here used to swallow the close button's click
                        // (the frame's hit-rect overlaps the button), which is
                        // why the X "did nothing" on inactive tabs. Suppressed
                        // entirely now — clicking dead space inside the tab
                        // pill no longer switches; click the label instead.
                        let _ = inner;
                    }
                });

            ui.add_space(4.0);

            let plus_btn = egui::Button::new(
                RichText::new("+")
                    .size(15.0)
                    .strong()
                    .color(Color32::from_rgb(255, 230, 120)),
            )
            .fill(Color32::from_rgb(44, 38, 16))
            .rounding(Rounding::same(7.0))
            .stroke(Stroke::new(1.0, Color32::from_rgb(120, 100, 40)))
            .min_size(Vec2::new(26.0, 22.0));
            if ui
                .add(plus_btn)
                .on_hover_text(crate::i18n::t("New scene tab"))
                .clicked()
            {
                self.request_scene_exit(SceneExitAction::NewTab);
            }
        });

        if let Some((idx, name)) = start_rename {
            self.state.editing_tab_idx = Some(idx);
            self.state.editing_tab_buf = name;
        }
        if let Some((idx, new_name)) = commit_rename {
            if idx != usize::MAX && idx < self.state.scene_tabs.len() && !new_name.trim().is_empty()
            {
                self.state.scene_tabs[idx].name = new_name.trim().to_string();
            }
            self.state.editing_tab_idx = None;
            self.state.editing_tab_buf.clear();
        }
        if let Some(idx) = switch_to {
            self.request_scene_exit(SceneExitAction::SwitchTab(idx));
        }
        if let Some(idx) = close_tab {
            self.request_scene_exit(SceneExitAction::CloseTab(idx));
        }
    }

    fn load_autosave_entry(&mut self, entry: &crate::autosave::AutosaveEntry) {
        let is_memstroy = entry
            .scene_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.eq_ignore_ascii_case("memstroy"))
            .unwrap_or(false);
        let load_result = if is_memstroy {
            self.state
                .load_memstroy(&entry.scene_path)
                .map_err(|e| e.to_string())
        } else {
            Scene::load(&entry.scene_path).map_err(|e| e.to_string())
        };

        match load_result {
            Ok(scene) => {
                self.state.scene = scene;
                // Restore the original project path when autosave metadata
                // knows it, so Save after recovery targets the user's file.
                self.state.scene_path = entry.meta.as_ref().and_then(|m| m.original_path.clone());
                self.state.status = crate::i18n::t("\u{2705} Recovered scene loaded.").into();

                if self.state.active_tab < self.state.scene_tabs.len() {
                    let idx = self.state.active_tab;
                    let tab_name = entry
                        .meta
                        .as_ref()
                        .map(|m| m.name.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| self.state.scene_tabs[idx].name.clone());
                    self.state.scene_tabs[idx].name = tab_name;
                    self.state.scene_tabs[idx].path = self.state.scene_path.clone();
                    self.state.scene_tabs[idx].scene = self.state.scene.clone();
                }
                self.state.frame_caches.clear();
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.playhead = 0.0;
            }
            Err(e) => {
                self.state.status = format!("{} {e}", crate::i18n::t("\u{274C} Recovery failed:"));
            }
        }
    }

    fn menu(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        use crate::i18n::t;
        egui::menu::bar(ui, |ui| {
            ui.menu_button(RichText::new(t("File")).strong(), |ui| {
                let row_height = 20.0;
                let menu_row = |ui: &mut egui::Ui, icon: &str, text: String| -> bool {
                    let width = 220.0;
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::click());
                    let visible_text = truncate_chars(&text, 36);
                    let truncated = visible_text != text;
                    let resp = if truncated {
                        resp.on_hover_text(text.clone())
                    } else {
                        resp
                    };
                    let visuals = ui.style().interact(&resp);
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 3.0, visuals.bg_fill);
                    }
                    let icon_pos = egui::pos2(rect.left() + 9.0, rect.center().y);
                    let text_pos = egui::pos2(rect.left() + 34.0, rect.center().y);
                    ui.painter().text(
                        icon_pos,
                        egui::Align2::LEFT_CENTER,
                        icon,
                        egui::FontId::monospace(12.0),
                        Color32::from_rgb(190, 190, 205),
                    );
                    let text_clip = egui::Rect::from_min_max(
                        egui::pos2(text_pos.x, rect.top()),
                        egui::pos2(rect.right() - 6.0, rect.bottom()),
                    );
                    ui.painter().with_clip_rect(text_clip).text(
                        text_pos,
                        egui::Align2::LEFT_CENTER,
                        visible_text,
                        egui::FontId::proportional(12.0),
                        visuals.text_color(),
                    );
                    resp.clicked()
                };

                if menu_row(ui, "+", t("New scene").to_string()) {
                    self.request_scene_exit(SceneExitAction::NewScene);
                    ui.close_menu();
                }
                if menu_row(ui, "O", t("Open scene...").to_string()) {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(t("Memstroy Project"), &["memstroy"])
                        .add_filter(t("Scene"), &["yaml", "yml", "json"])
                        .pick_file()
                    {
                        let is_memstroy = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.eq_ignore_ascii_case("memstroy"))
                            .unwrap_or(false);
                        self.request_scene_exit(SceneExitAction::OpenScene { path, is_memstroy });
                    }
                    ui.close_menu();
                }
                if menu_row(ui, "S", t("Save scene").to_string()) {
                    self.save_scene();
                    ui.close_menu();
                }
                if menu_row(ui, "S+", t("Save scene as...").to_string()) {
                    self.save_as();
                    ui.close_menu();
                }
                ui.separator();

                // Autosaves are flattened into the File menu. The scan
                // itself is cached and refreshed on a worker thread so a
                // menu open never stalls the top bar on filesystem IO.
                let autosave_cache_stale = self
                    .autosave_menu_last_refresh
                    .map(|t| t.elapsed() > std::time::Duration::from_secs(5))
                    .unwrap_or(true);
                if autosave_cache_stale && !self.autosave_menu_loading {
                    self.refresh_autosave_menu_async();
                }
                let autosave_entries = self.autosave_menu_entries.clone();
                if autosave_entries.is_empty() {
                    let label = if self.autosave_menu_loading {
                        t("Open autosave (loading)").to_string()
                    } else {
                        t("Open autosave (none)").to_string()
                    };
                    let _ = menu_row(ui, "R", label);
                } else {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(t("Open autosave"))
                            .size(11.0)
                            .color(Color32::from_rgb(160, 160, 176)),
                    );
                    const MAX_AUTOSAVE_ROWS: usize = 8;
                    for entry in autosave_entries.iter().take(MAX_AUTOSAVE_ROWS) {
                        let display_name = entry
                            .meta
                            .as_ref()
                            .map(|m| {
                                if !m.name.is_empty() {
                                    m.name.clone()
                                } else if let Some(p) = m.original_path.as_ref() {
                                    p.file_stem()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("Scene")
                                        .to_string()
                                } else {
                                    t("Untitled").to_string()
                                }
                            })
                            .unwrap_or_else(|| {
                                entry
                                    .scene_path
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("Scene")
                                    .to_string()
                            });
                        let age = crate::autosave::format_age(entry.mtime);
                        let label = format!("{display_name}  -  {age}");
                        if menu_row(ui, "R", label) {
                            self.load_autosave_entry(entry);
                            ui.close_menu();
                        }
                    }
                }
                ui.separator();
                if menu_row(ui, "EX", t("Export").to_string()) {
                    self.run_render();
                    ui.close_menu();
                }
                ui.separator();
                if menu_row(ui, "*", t("Settings...").to_string()) {
                    self.state.settings_open = true;
                    ui.close_menu();
                }
                ui.separator();
                if menu_row(ui, "X", t("Exit").to_string()) {
                    if self.request_scene_exit(SceneExitAction::Quit) {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    ui.close_menu();
                }
            });

            // ── View menu ─────────────────────────────────────────
            // Single home for every floating-window toggle in the
            // editor. Adding new floating windows should only need a
            // checkbox here.
            ui.menu_button(RichText::new(t("View")).strong(), |ui| {
                ui.checkbox(&mut self.state.web_image_search_open, t("Web Image Search"));
                ui.checkbox(&mut self.state.curve_editor_open, t("Curve Editor"));
                // Skeleton editor used to live as a floating window
                // here. It now belongs to the inspector for any video
                // layer element (actor / video overlay), so the menu
                // entry was retired alongside the window.
            });

            // Status indicator on the right
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.state.refreshing {
                    ui.spinner();
                    ui.label(
                        RichText::new(t("refreshing..."))
                            .color(Color32::from_rgb(255, 200, 50))
                            .size(11.0),
                    );
                }
                // The render progress bar that used to live here was
                // removed per user request — the dedicated render
                // window (`show_render_progress_window`) is the single
                // source of truth for render status. Duplicating it on
                // the menu bar made the toolbar busy and the user
                // explicitly asked for "шкала рендера не нужна, оставь
                // только в окне рендера".

                // Show status text (trimmed)
                let status = &self.state.status;
                if !status.is_empty() && !status.starts_with("__") {
                    let display = if status.chars().count() > 60 {
                        let t: String = status.chars().take(57).collect();
                        format!("{}...", t)
                    } else {
                        status.clone()
                    };
                    ui.label(
                        RichText::new(display)
                            .size(11.0)
                            .color(Color32::from_rgb(160, 160, 180)),
                    );
                }
            });
        });
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Two-tier gating:
        //
        // 1. **Plain-key shortcuts** (Space, Delete, Esc) only fire
        //    when nothing wants keyboard input — typing into a text
        //    overlay or a search box should not play/pause, delete
        //    things, or close popups behind the user's back.
        //
        // 2. **Modifier-based shortcuts** (Ctrl+Z/Y/D/C/V) are routed
        //    through `consume_key` for the chord. There is one
        //    additional subtlety here: egui rewrites Ctrl+C, Ctrl+X
        //    and Ctrl+V into `Event::Copy` / `Event::Cut` /
        //    `Event::Paste(text)` *before* it dispatches to widgets,
        //    and depending on the platform / IME stack the original
        //    `Event::Key { key: C, modifiers: ctrl, … }` may or may
        //    not also be present. That is why the previous
        //    `consume_key`-only approach silently broke for some
        //    users: on those platforms only `Event::Copy` was
        //    delivered and `consume_key(.., Key::C)` never fired.
        //    The deep fix is to drain BOTH the synthetic
        //    Copy/Cut/Paste events *and* the raw Ctrl+C/Ctrl+V key
        //    chords. Whichever shows up, our handler runs exactly
        //    once and consumes the event so a focused TextEdit
        //    can't process it again.
        let typing = ctx.wants_keyboard_input();

        let modifiers = ctx.input(|i| i.modifiers);
        let ctrl = modifiers.ctrl || modifiers.mac_cmd;

        // ── Modifier-based shortcuts — always run unless a text
        //    widget has focus. ──
        //
        // Ctrl+Z / Ctrl+Y / Ctrl+D have semantics specific to a
        // focused TextEdit (per-field undo, redo, duplicate-line)
        // that the user expects to take precedence when they're
        // editing text. Gating these behind `!typing` lets the
        // text widget see the chord natively.
        if ctrl && !typing {
            // Ctrl+Z = Undo (NOT Shift+Z, which is redo).
            if !modifiers.shift
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z))
            {
                self.state.undo();
            }
            // Ctrl+Shift+Z or Ctrl+Y = Redo
            let redo_z = ctx.input_mut(|i| {
                i.consume_key(
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    egui::Key::Z,
                )
            });
            let redo_y = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y));
            if redo_z || redo_y {
                self.state.redo();
            }
            // Ctrl+D = duplicate
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)) {
                self.duplicate_selected();
            }
        }

        // ── Ctrl+C / Ctrl+X / Ctrl+V — handled via THREE complementary
        //    paths that, together, work around a long-standing bug in
        //    egui-winit's Ctrl+V handling.
        //
        //    The bug, copied verbatim from `egui-winit-0.28.1::lib.rs`:
        //
        //        if is_paste_command(modifiers, active_key) {
        //            if let Some(contents) = self.clipboard.get() {
        //                if !contents.is_empty() {
        //                    push Event::Paste(contents);
        //                }
        //            }
        //            return;          // <-- key event NOT pushed
        //        }
        //
        //    When the OS clipboard contains ONLY image bytes (a
        //    screenshot, "Copy image" from a browser, a bitmap from
        //    Telegram/Figma, …) `clipboard.get()` returns an empty
        //    string and egui-winit pushes NEITHER `Event::Paste` NOR
        //    `Event::Key { key: V, pressed: true, … }`. The user's
        //    Ctrl+V is invisible to egui — the previous chord-only and
        //    drained-only fixes both saw nothing and silently ignored
        //    the chord.
        //
        //    The dual-path workaround:
        //
        //      A. `swallow_clipboard_events` drains every synthetic
        //         `Event::Copy` / `Event::Cut` / `Event::Paste(_)` so a
        //         focused TextEdit can't steal the chord and so we can
        //         tell on the press frame "egui-winit produced a paste
        //         event for this Ctrl+V" (clipboard had text).
        //
        //      B. `consume_key(.., Key::V)` chord fallback — covers
        //         the rare case where the OS / IME delivers a real key
        //         event without the synthetic counterpart.
        //
        //      C. **V-key RELEASE detection** — the actual fix for the
        //         "no text in clipboard" path. egui-winit's early
        //         `return` only happens inside `if pressed`, so the
        //         RELEASE event for V is ALWAYS pushed. When we see a
        //         V release with the Ctrl/Cmd modifier (or while Ctrl
        //         was held in a recent frame) and we have NOT just
        //         consumed an `Event::Paste` for this chord, we treat
        //         it as the Ctrl+V we missed and fire the paste
        //         handler. A small `pending_v_release_skips` counter
        //         dedupes against the matching release that arrives
        //         after a successful drained-Paste path.
        //
        //    The same dedupe trick is NOT needed for Copy / Cut: those
        //    are idempotent (re-copying the same selection has no
        //    visible effect), and egui-winit ALWAYS pushes
        //    `Event::Copy` / `Event::Cut` regardless of clipboard
        //    state — there is no `if !contents.is_empty()` gate on
        //    that path.
        //
        // ── Inspector TextEdit / DragValue carve-out ──
        //
        // When the user is typing into a TextEdit (or editing a
        // numeric field via its temporary text-edit popup) we MUST
        // leave Ctrl+C / Ctrl+X / Ctrl+V to the focused widget so
        // they act on the field's text — copy a phrase, paste a
        // number — instead of duplicating the canvas selection.
        // `wants_keyboard_input()` flips to true precisely while a
        // text widget owns focus, so we use it to gate both the
        // event drain and the chord/release fallbacks. Without this
        // gate the inspector behaved as the user reported: pressing
        // Ctrl+V in a text field pasted layers onto the canvas
        // instead of the clipboard text into the field.
        let drained = swallow_clipboard_events(ctx, typing);
        if typing {
            // Suspend the canvas-side copy/paste pipeline entirely
            // for this frame. We still drained "presence" booleans
            // above, but we don't act on them — the focused widget
            // handles its own clipboard ops, and the V-release
            // fallback below is short-circuited.
            return;
        }
        let chord_copy = ctrl
            && !modifiers.shift
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::C));
        let chord_cut = ctrl
            && !modifiers.shift
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::X));
        let chord_paste = ctrl
            && !modifiers.shift
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::V));

        // ── Path C: V-key RELEASE fallback ─────────────────────────
        //
        // Update the "Ctrl was held recently" grace window so we still
        // recognise Ctrl+V chords when the user releases Ctrl *before*
        // V. Without this, V's release event has `modifiers.command =
        // false` and we'd miss the chord.
        if ctrl {
            self.ctrl_held_grace_frames = 4;
        } else if self.ctrl_held_grace_frames > 0 {
            self.ctrl_held_grace_frames -= 1;
        }
        let ctrl_recently_held = self.ctrl_held_grace_frames > 0;

        // Count V-release events delivered this frame whose modifier
        // state OR our recent-history grace window indicates a Ctrl+V
        // chord. We don't `consume_key` these — they're not chord
        // events from egui's POV, they're plain key releases — so we
        // count them by inspecting the raw event vec.
        let v_release_count: u32 = ctx.input(|i| {
            i.events
                .iter()
                .filter(|ev| match ev {
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers: m,
                        ..
                    } => m.command || m.ctrl || ctrl_recently_held,
                    _ => false,
                })
                .count() as u32
        });

        // For every `Event::Paste` we just drained, the matching V
        // release will land in this or a later frame. Reserve a skip
        // slot so we don't double-fire the paste handler when that
        // release shows up.
        //
        // ── Order matters ──
        // Match *existing* `pending_v_release_skips` against the
        // V-release events we observed THIS frame BEFORE incrementing
        // for `drained.pasted`. Otherwise, in the rapid-succession
        // case `[image-only Ctrl+V][text Ctrl+V]` where Release 1 and
        // the second drained Paste arrive in the same frame, the
        // freshly-incremented skip would consume Release 1 and the
        // image-only paste would silently disappear.
        let matched_releases = self.pending_v_release_skips.min(v_release_count);
        self.pending_v_release_skips -= matched_releases;
        let unmatched_v_releases = v_release_count - matched_releases;
        let release_paste = unmatched_v_releases > 0;

        if drained.pasted {
            self.pending_v_release_skips = self.pending_v_release_skips.saturating_add(1);
        }

        // Suppress double-fire when both the synthetic Event::Copy
        // and the raw Ctrl+C key arrived for the same physical press.
        let do_copy = drained.copied || chord_copy;
        let do_cut = drained.cut || chord_cut;
        let do_paste = drained.pasted || chord_paste || release_paste;

        if do_copy || do_cut {
            let copying_keyframes = !self.state.selected_keyframes.is_empty();
            let n = if copying_keyframes {
                self.state.copy_selected_keyframes_to_clipboard()
            } else {
                self.state.copy_selection_to_clipboard()
            };
            if n > 0 {
                self.state.status = format!(
                    "{} {} {}",
                    crate::i18n::t("Copied"),
                    n,
                    if copying_keyframes {
                        if n == 1 {
                            crate::i18n::t("keyframe to clipboard")
                        } else {
                            crate::i18n::t("keyframes to clipboard")
                        }
                    } else if n == 1 {
                        crate::i18n::t("item to clipboard")
                    } else {
                        crate::i18n::t("items to clipboard")
                    }
                );
                // Remember when this copy happened so the next Ctrl+V
                // knows to prefer our in-app clipboard over an OS
                // clipboard image — covers the "user copied an image
                // in a browser, then did Ctrl+C in our app to grab a
                // canvas selection, expects Ctrl+V to duplicate the
                // canvas selection, not paste the older browser
                // image" workflow.
                self.last_internal_copy_at = Some(std::time::Instant::now());

                if do_cut {
                    // Ctrl+X = copy + delete primary / multi-selection.
                    self.delete_selected();
                }
            }
        }

        if do_paste {
            // Decide which clipboard wins when BOTH the OS clipboard
            // contains an image AND our in-app clipboard is non-empty.
            //
            // "Last action wins" — if the user did Ctrl+C inside our
            // app within the last 30 seconds we duplicate the in-app
            // clipboard items first; otherwise the OS clipboard image
            // takes priority. This matches the behaviour the user
            // expects from Figma / Telegram-style paste while still
            // letting them duplicate the canvas selection they JUST
            // copied without an unrelated browser screenshot
            // hijacking the chord.
            const INTERNAL_COPY_PRIORITY_WINDOW: std::time::Duration =
                std::time::Duration::from_secs(30);
            let prefer_internal = !self.state.clipboard.is_empty()
                && self
                    .last_internal_copy_at
                    .map(|t| t.elapsed() < INTERNAL_COPY_PRIORITY_WINDOW)
                    .unwrap_or(false);

            let mut handled = false;
            if !self.state.keyframe_clipboard.is_empty() {
                let n = self.state.paste_keyframes_from_clipboard();
                if n > 0 {
                    self.state.status = format!(
                        "{} {} {}",
                        crate::i18n::t("Pasted"),
                        n,
                        if n == 1 {
                            crate::i18n::t("keyframe at the playhead")
                        } else {
                            crate::i18n::t("keyframes at the playhead")
                        },
                    );
                    handled = true;
                }
            }
            if prefer_internal {
                let n = self.state.paste_clipboard();
                if n > 0 {
                    self.state.status = format!(
                        "{} {} {}",
                        crate::i18n::t("Pasted"),
                        n,
                        if n == 1 {
                            crate::i18n::t("item at the playhead")
                        } else {
                            crate::i18n::t("items at the playhead")
                        },
                    );
                    handled = true;
                }
            }
            if !handled {
                let pasted_image = self.try_paste_image_from_system_clipboard();
                if pasted_image {
                    handled = true;
                }
            }
            if !handled {
                // Fallback: in-app clipboard (covers the common
                // "user did Ctrl+C in our app, then Ctrl+V some
                // time later" case where the priority window has
                // expired but no OS clipboard image is available).
                let n = self.state.paste_clipboard();
                if n > 0 {
                    self.state.status = format!(
                        "{} {} {}",
                        crate::i18n::t("Pasted"),
                        n,
                        if n == 1 {
                            crate::i18n::t("item at the playhead")
                        } else {
                            crate::i18n::t("items at the playhead")
                        },
                    );
                    handled = true;
                }
            }
            if !handled {
                self.state.status = crate::i18n::t("Clipboard is empty").into();
            }
        }

        // Plain-key shortcuts — gated by typing focus.
        if typing {
            return;
        }

        // (The skeleton editor used to gate plain-key shortcuts here
        // because its own floating window owned Space / Arrow / Home /
        // End. The window has been retired and replaced by an
        // inspector section that does not steal those keys, so the
        // gate is no longer needed.)

        let step_left =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft));
        let step_right =
            ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight));
        if step_left || step_right {
            let fps = self.state.scene.output.fps.max(1) as f32;
            let dt = 1.0 / fps;
            let dir = if step_right { 1.0 } else { -1.0 };
            self.state.playing = false;
            self.state.playhead =
                (self.state.playhead + dir * dt).clamp(0.0, self.state.scene.output.duration);
            self.state.status = format!(
                "{} {}",
                crate::i18n::t("Frame"),
                (self.state.playhead * fps).round() as i64
            );
        }

        ctx.input(|i| {
            // Space = Play/Pause
            if i.key_pressed(egui::Key::Space) {
                self.state.playing = !self.state.playing;
                if self.state.playing {
                    self.state.status = crate::i18n::t("\u{25B6} Playing").into();
                } else {
                    self.state.status = crate::i18n::t("\u{23F8} Paused").into();
                }
            }
            // Delete key = remove selected element (only when no
            // modifier — Ctrl+Delete is reserved for future use).
            //
            // Suppressed when the user has a per-parameter keyframe
            // selection on the timeline: in that case the timeline's
            // own Delete handler (`delete_selected_keyframes`) owns
            // the gesture and we don't want the layer to vanish out
            // from under the user when they meant to delete a kf.
            if !ctrl
                && self.state.selected_keyframes.is_empty()
                && (i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
            {
                self.delete_selected();
            }
            // Escape clears the canvas multi-selection so the user can
            // exit a marquee paint without affecting other shortcuts.
            // It also disarms any mask / crop tool so the user can fall
            // back to the default transform mode without hunting for
            // the toolbar button.
            if i.key_pressed(egui::Key::Escape) {
                if self.state.canvas_image_search.is_some()
                    || self.state.canvas_image_search_draft.is_some()
                    || self.state.canvas_image_search_rmb_pending.is_some()
                {
                    crate::canvas_image_search::cancel_canvas_image_search(&mut self.state);
                } else if !self.state.canvas_selection.is_empty() {
                    self.state.canvas_selection.clear();
                }
                if self.state.mask_tool != crate::state::MaskTool::None {
                    self.state.mask_tool = crate::state::MaskTool::None;
                    self.state.mask_draft_points.clear();
                    self.state.mask_segment_cursor_uv = None;
                    // Also drop any in-flight mask-draw carrier so a
                    // segment-mask polygon under construction doesn't
                    // leak into the regular transform pipeline. The
                    // other mask tools commit on pointer-release so
                    // their carrier is short-lived, but the segment
                    // tool keeps its `DrawMask` mode alive across
                    // clicks until the polygon is closed — Esc is
                    // the only place that gesture can be aborted
                    // mid-flight.
                    if matches!(
                        self.state.canvas_drag.mode,
                        crate::state::CanvasDragMode::DrawMask { .. }
                    ) {
                        self.state.canvas_drag.mode = crate::state::CanvasDragMode::None;
                    }
                    self.state.status = crate::i18n::t("Mask tool cancelled").into();
                }
            }
        });
    }

    /// Try to paste an image from the OS clipboard onto the canvas.
    /// Returns `true` when an image was actually consumed (so the
    /// caller skips the in-process clipboard fallback). Returns
    /// `false` when there is no image on the clipboard, when the
    /// clipboard backend is unavailable on this platform, or when the
    /// PNG encode / library write fails — the user gets a status
    /// message describing the cause and the regular Ctrl+V keeps
    /// working for in-app duplication.
    ///
    /// On success the function:
    ///   1. Encodes the clipboard pixels to a fresh PNG inside
    ///      `assets/images/clipboard_<ts>.png`.
    ///   2. Refreshes the library so the file shows up on the Images tab.
    ///   3. Spawns an [`Overlay::Image`] at the current playhead, on
    ///      the first empty video lane (or a brand-new lane at the top
    ///      when every lane is busy).
    ///   4. Switches the library tab + selection to the new entry so
    ///      the user immediately sees what they pasted.
    fn try_paste_image_from_system_clipboard(&mut self) -> bool {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(?err, "system clipboard unavailable");
                return false;
            }
        };
        let image = match clipboard.get_image() {
            Ok(img) => img,
            Err(err) => {
                tracing::debug!(?err, "clipboard does not contain an image");
                return false;
            }
        };
        let width = image.width as u32;
        let height = image.height as u32;
        let bytes: Vec<u8> = image.bytes.into_owned();
        let asset = match self
            .state
            .save_clipboard_image_to_library(&bytes, width, height)
        {
            Ok(a) => a,
            Err(err) => {
                self.state.status =
                    format!("{} {}", crate::i18n::t("Clipboard image save failed:"), err);
                return true; // we tried; don't fall through to internal paste
            }
        };
        let _idx = self.state.add_image_overlay_at_playhead(&asset);
        self.state.library_tab = crate::state::LibraryTab::Images;
        self.state.status = format!(
            "{} \u{2192} {} ({}\u{00D7}{})",
            crate::i18n::t("Pasted clipboard image"),
            asset.label,
            width,
            height
        );
        true
    }

    /// Render the body of the curve-editor floating window. Binds to
    /// exactly one selected element (actor / overlay / audio / render
    /// frame). When nothing is selected or multiple elements are
    /// selected, the panel clears and asks the user to pick one.
    fn draw_curve_editor_body(&mut self, ui: &mut egui::Ui) {
        use crate::curve_editor::{
            curve_editor_panel, CurveEditorTarget, PROP_OPACITY, PROP_POS_X, PROP_POS_Y,
            PROP_ROTATION, PROP_SCALE,
        };
        use crate::i18n::t;

        fn curve_editor_single_selection(state: &crate::state::EditorState) -> Option<Selection> {
            if state.canvas_selection.len() > 1 || state.multi_select.len() > 1 {
                return None;
            }
            if state.canvas_selection.len() == 1 {
                let s = state.canvas_selection[0];
                if matches!(
                    s,
                    Selection::Actor(_)
                        | Selection::Overlay(_)
                        | Selection::Audio(_)
                        | Selection::RenderFrame
                ) {
                    return Some(s);
                }
            }
            match state.selection {
                Selection::Actor(_)
                | Selection::Overlay(_)
                | Selection::Audio(_)
                | Selection::RenderFrame => Some(state.selection),
                _ => None,
            }
        }

        fn clear_curve_editor_state(state: &mut crate::state::EditorState) {
            state.curve_editor_property = PROP_SCALE;
            state.curve_editor_selected.clear();
            state.curve_editor_marquee = None;
            state.curve_editor_multi_drag = false;
            state.curve_editor_multi_drag_delta = egui::Vec2::ZERO;
        }

        let Some(sel) = curve_editor_single_selection(&self.state) else {
            clear_curve_editor_state(&mut self.state);
            ui.label(
                egui::RichText::new(t("Select a single element to edit its properties."))
                    .italics()
                    .color(Color32::from_rgb(140, 140, 160)),
            );
            return;
        };

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum TargetKind {
            Actor,
            Overlay,
            Audio,
            RenderFrame,
        }

        let kind = match sel {
            Selection::Actor(i) if i < self.state.scene.actors.len() => TargetKind::Actor,
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => TargetKind::Overlay,
            Selection::Audio(i) if i < self.state.scene.audio.len() => TargetKind::Audio,
            Selection::RenderFrame => TargetKind::RenderFrame,
            _ => {
                clear_curve_editor_state(&mut self.state);
                ui.label(
                    egui::RichText::new(t(
                        "Select an actor, overlay or audio layer to edit its curves.",
                    ))
                    .italics()
                    .color(Color32::from_rgb(140, 140, 160)),
                );
                return;
            }
        };

        let duration = self.state.scene.output.duration;
        let playhead = self.state.playhead;

        match kind {
            TargetKind::Actor => {
                if let Selection::Actor(i) = sel {
                    let scene = &mut self.state.scene;
                    let actor_id = scene.actors[i].id.clone();
                    let canvas_idx = scene
                        .canvas_layouts
                        .iter()
                        .position(|cl| cl.element_id == actor_id);
                    let clip_start = scene.actors[i].t_in.unwrap_or(0.0);
                    let clip_end = scene.actors[i].t_out.unwrap_or(duration);
                    let canvas_layout =
                        canvas_idx.map(|idx| &mut scene.canvas_layouts[idx].keyframes);
                    let a = &mut scene.actors[i];
                    let target = CurveEditorTarget::Actor {
                        layout: &mut a.layout,
                        animated_params: &mut a.animated_params,
                        clip_start,
                        clip_end,
                        canvas_layout,
                    };
                    curve_editor_panel(
                        ui,
                        target,
                        duration,
                        &mut self.state.curve_editor_property,
                        playhead,
                        &mut self.state.curve_editor_marquee,
                        &mut self.state.curve_editor_selected,
                        &mut self.state.curve_editor_multi_drag,
                        &mut self.state.curve_editor_multi_drag_delta,
                        &mut self.state.curve_editor_pan_offset,
                        &mut self.state.curve_editor_zoom,
                        &mut self.state.curve_editor_panning,
                    );
                    // Effect animated params — show a scalar curve
                    // editor for each animated effect parameter.
                    let actor_t_in = a.t_in.unwrap_or(0.0);
                    let t_local = (playhead - actor_t_in).max(0.0);
                    for (fx_idx, eff) in a.effects.iter_mut().enumerate() {
                        let kind_label = eff.kind.label().to_string();
                        let animated_keys: Vec<String> =
                            eff.animated_params.iter().cloned().collect();
                        for key in animated_keys {
                            let param_label_owned = format!(
                                "{} {}",
                                kind_label,
                                match key.as_str() {
                                    "intensity" => "Intensity",
                                    "p0" => "Param",
                                    "p1" => "Param 2",
                                    "p2" => "Param 3",
                                    "p3" => "Param 4",
                                    other => other,
                                }
                            );
                            let static_val =
                                crate::curve_editor::effect_param_static_value(eff, &key);
                            let range = crate::curve_editor::effect_param_value_range(&key);
                            let kfs = eff.param_kfs.entry(key.clone()).or_default();
                            ui.add_space(6.0);
                            ui.separator();
                            let target = CurveEditorTarget::EffectParam {
                                kfs,
                                animated_params: &mut eff.animated_params,
                                param_id: &key,
                                param_label: &param_label_owned,
                                param_color: crate::curve_editor::effect_param_color(fx_idx),
                                value_range: range,
                                static_value: static_val,
                                t_local,
                            };
                            curve_editor_panel(
                                ui,
                                target,
                                duration,
                                &mut self.state.curve_editor_property,
                                playhead,
                                &mut self.state.curve_editor_marquee,
                                &mut self.state.curve_editor_selected,
                                &mut self.state.curve_editor_multi_drag,
                                &mut self.state.curve_editor_multi_drag_delta,
                                &mut self.state.curve_editor_pan_offset,
                                &mut self.state.curve_editor_zoom,
                                &mut self.state.curve_editor_panning,
                            );
                        }
                    }
                }
            }
            TargetKind::Overlay => {
                if let Selection::Overlay(i) = sel {
                    let scene = &mut self.state.scene;
                    let ov_id = match scene.overlays.get(i) {
                        Some(memstroy_core::Overlay::Text(o)) => o.id.clone(),
                        Some(memstroy_core::Overlay::Image(o)) => o.id.clone(),
                        Some(memstroy_core::Overlay::Video(o)) => o.id.clone(),
                        None => return,
                    };
                    let canvas_idx = scene
                        .canvas_layouts
                        .iter()
                        .position(|cl| cl.element_id == ov_id);
                    let canvas_layout =
                        canvas_idx.map(|idx| &mut scene.canvas_layouts[idx].keyframes);
                    let ov = &mut scene.overlays[i];
                    let (layout, animated, t_in, t_out) = match ov {
                        memstroy_core::Overlay::Text(o) => {
                            (&mut o.layout, &mut o.animated_params, o.t_in, o.t_out)
                        }
                        memstroy_core::Overlay::Image(o) => {
                            (&mut o.layout, &mut o.animated_params, o.t_in, o.t_out)
                        }
                        memstroy_core::Overlay::Video(o) => {
                            (&mut o.layout, &mut o.animated_params, o.t_in, o.t_out)
                        }
                    };
                    let clip_duration = (t_out - t_in).max(0.0);
                    let target = CurveEditorTarget::Overlay {
                        layout,
                        animated_params: animated,
                        t_in,
                        clip_duration,
                        canvas_layout,
                    };
                    curve_editor_panel(
                        ui,
                        target,
                        duration,
                        &mut self.state.curve_editor_property,
                        playhead,
                        &mut self.state.curve_editor_marquee,
                        &mut self.state.curve_editor_selected,
                        &mut self.state.curve_editor_multi_drag,
                        &mut self.state.curve_editor_multi_drag_delta,
                        &mut self.state.curve_editor_pan_offset,
                        &mut self.state.curve_editor_zoom,
                        &mut self.state.curve_editor_panning,
                    );
                    // Effect animated params for overlay.
                    let overlay_t_in = t_in;
                    let t_local = (playhead - overlay_t_in).max(0.0);
                    let effects: &mut Vec<memstroy_core::Effect> = match ov {
                        memstroy_core::Overlay::Text(o) => &mut o.effects,
                        memstroy_core::Overlay::Image(o) => &mut o.effects,
                        memstroy_core::Overlay::Video(o) => &mut o.effects,
                    };
                    for (fx_idx, eff) in effects.iter_mut().enumerate() {
                        let kind_label = eff.kind.label().to_string();
                        let animated_keys: Vec<String> =
                            eff.animated_params.iter().cloned().collect();
                        for key in animated_keys {
                            let param_label_owned = format!(
                                "{} {}",
                                kind_label,
                                match key.as_str() {
                                    "intensity" => "Intensity",
                                    "p0" => "Param",
                                    "p1" => "Param 2",
                                    "p2" => "Param 3",
                                    "p3" => "Param 4",
                                    other => other,
                                }
                            );
                            let static_val =
                                crate::curve_editor::effect_param_static_value(eff, &key);
                            let range = crate::curve_editor::effect_param_value_range(&key);
                            let kfs = eff.param_kfs.entry(key.clone()).or_default();
                            ui.add_space(6.0);
                            ui.separator();
                            let target = CurveEditorTarget::EffectParam {
                                kfs,
                                animated_params: &mut eff.animated_params,
                                param_id: &key,
                                param_label: &param_label_owned,
                                param_color: crate::curve_editor::effect_param_color(fx_idx),
                                value_range: range,
                                static_value: static_val,
                                t_local,
                            };
                            curve_editor_panel(
                                ui,
                                target,
                                duration,
                                &mut self.state.curve_editor_property,
                                playhead,
                                &mut self.state.curve_editor_marquee,
                                &mut self.state.curve_editor_selected,
                                &mut self.state.curve_editor_multi_drag,
                                &mut self.state.curve_editor_multi_drag_delta,
                                &mut self.state.curve_editor_pan_offset,
                                &mut self.state.curve_editor_zoom,
                                &mut self.state.curve_editor_panning,
                            );
                        }
                    }
                }
            }
            TargetKind::Audio => {
                if let Selection::Audio(i) = sel {
                    if let Some(audio) = self.state.scene.audio.get_mut(i) {
                        // Audio has 3 keyframable scalar params:
                        // Volume / Speed / Pan. Re-use the property
                        // selector slot for these so the user can
                        // switch between them without yet another
                        // toolbar.
                        const AUDIO_VOLUME: usize = 0;
                        const AUDIO_SPEED: usize = 1;
                        const AUDIO_PAN: usize = 2;
                        let mut audio_prop = match self.state.curve_editor_property {
                            PROP_SCALE | PROP_POS_X | PROP_POS_Y | PROP_OPACITY | PROP_ROTATION
                                if self.state.curve_editor_property < 3 =>
                            {
                                self.state.curve_editor_property
                            }
                            _ => 0,
                        };
                        // Audio property tabs.
                        ui.horizontal(|ui| {
                            for (i_prop, label) in [
                                (AUDIO_VOLUME, "Volume"),
                                (AUDIO_SPEED, "Speed"),
                                (AUDIO_PAN, "Pan"),
                            ] {
                                if ui
                                    .selectable_label(audio_prop == i_prop, t(label))
                                    .clicked()
                                {
                                    audio_prop = i_prop;
                                }
                            }
                        });
                        self.state.curve_editor_property = audio_prop;
                        ui.add_space(2.0);
                        let t_local = (playhead - audio.t_in).max(0.0);
                        let (kfs, label, color, range, static_v, param_id) = match audio_prop {
                            AUDIO_SPEED => (
                                &mut audio.speed_kfs,
                                crate::i18n::t("Speed"),
                                Color32::from_rgb(255, 180, 80),
                                (0.05_f32, 16.0_f32),
                                audio.speed,
                                "speed",
                            ),
                            AUDIO_PAN => (
                                &mut audio.pan_kfs,
                                crate::i18n::t("Pan"),
                                Color32::from_rgb(180, 220, 255),
                                (-1.0_f32, 1.0_f32),
                                audio.pan,
                                "pan",
                            ),
                            _ => (
                                &mut audio.volume_kfs,
                                crate::i18n::t("Volume"),
                                Color32::from_rgb(120, 220, 160),
                                (0.0_f32, 4.0_f32),
                                audio.volume,
                                "volume",
                            ),
                        };
                        let target = CurveEditorTarget::Audio {
                            kfs,
                            animated_params: &mut audio.animated_params,
                            param_id,
                            param_label: label,
                            param_color: color,
                            value_range: range,
                            static_value: static_v,
                            t_local,
                        };
                        curve_editor_panel(
                            ui,
                            target,
                            duration,
                            &mut self.state.curve_editor_property,
                            playhead,
                            &mut self.state.curve_editor_marquee,
                            &mut self.state.curve_editor_selected,
                            &mut self.state.curve_editor_multi_drag,
                            &mut self.state.curve_editor_multi_drag_delta,
                            &mut self.state.curve_editor_pan_offset,
                            &mut self.state.curve_editor_zoom,
                            &mut self.state.curve_editor_panning,
                        );
                    }
                }
            }
            TargetKind::RenderFrame => {
                let rf = &mut self.state.scene.render_frame;
                let target = CurveEditorTarget::RenderFrame {
                    layout: &mut rf.layout,
                    animated_params: &mut rf.animated_params,
                };
                curve_editor_panel(
                    ui,
                    target,
                    duration,
                    &mut self.state.curve_editor_property,
                    playhead,
                    &mut self.state.curve_editor_marquee,
                    &mut self.state.curve_editor_selected,
                    &mut self.state.curve_editor_multi_drag,
                    &mut self.state.curve_editor_multi_drag_delta,
                    &mut self.state.curve_editor_pan_offset,
                    &mut self.state.curve_editor_zoom,
                    &mut self.state.curve_editor_panning,
                );
            }
        }
    }

    /// Resolve the most likely actor owner for an audio row.
    ///
    /// Primary key is `parent_actor` when present. For unlinked rows we try:
    /// 1) id convention `<actor_id>_audio`, then
    /// 2) same source file + overlapping time window (best score).
    fn infer_actor_for_audio_track(&self, audio_idx: usize) -> Option<usize> {
        infer_actor_for_audio_in_scene(&self.state.scene, audio_idx)
    }

    /// Mark audio row as deleted and mute the corresponding actor's
    /// embedded fallback audio (if we can resolve one).
    fn mark_audio_track_deleted(&mut self, audio_idx: usize) {
        if audio_idx >= self.state.scene.audio.len() || self.state.scene.audio[audio_idx].deleted {
            return;
        }
        let actor_idx = self.infer_actor_for_audio_track(audio_idx);
        self.state.scene.audio[audio_idx].deleted = true;
        if let Some(ai) = actor_idx {
            if let Some(actor) = self.state.scene.actors.get_mut(ai) {
                actor.mute_audio = true;
            }
        }
    }

    fn delete_selected(&mut self) {
        if let Some(track_idx) = self.state.selected_track {
            if self.state.track_is_empty(track_idx) {
                self.state.mutate_state(|s| {
                    let _ = s.delete_empty_track(track_idx);
                });
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.status = crate::i18n::t("Layer deleted.").into();
                return;
            }
            let n = self.state.select_track_contents(track_idx);
            if n > 0 {
                self.state.selected_track = None;
                self.state.status =
                    crate::i18n::t("Layer contains elements; selected them instead.").into();
                return;
            }
        }

        // ── Multi-select delete ──
        //
        // When `canvas_selection` holds more than one element, delete
        // every entry, not just the primary. We snapshot the full
        // editor state ONCE at the top so the entire batch collapses
        // into one undo step (and so Ctrl+Z restores both the scene
        // tree and every layer assignment we wiped).
        //
        // Each per-kind branch below already shifts the relevant
        // index-based side tables (frame_caches, audio_waveforms,
        // *_track_assignments), so the only extra trick we need here
        // is to walk the selection in **DESCENDING** index order per
        // kind. Removing higher indices first leaves every lower
        // index valid as we go, so the precomputed entries don't
        // require remapping. The kinds themselves are independent
        // (actors / overlays / audio / backgrounds live in separate
        // Vecs) so the order across kinds doesn't matter.
        let multi_targets: Vec<Selection> = if state_canvas_selection_count(self) > 1
            || self.state.multi_select.len() > 1
        {
            let mut targets = self.state.canvas_selection.clone();
            for &actor_idx in &self.state.multi_select {
                let sel = Selection::Actor(actor_idx);
                if !targets.contains(&sel) {
                    targets.push(sel);
                }
            }
            if !matches!(self.state.selection, Selection::None)
                && !targets.contains(&self.state.selection)
            {
                targets.push(self.state.selection);
            }
            targets
        } else {
            Vec::new()
        };

        if !multi_targets.is_empty() {
            // Single undo entry for the whole batch.
            self.state.last_drag_group = None;
            self.state.undo.push_full(self.state.build_undo_snapshot());

            // Bucket selections by kind, then delete in descending
            // index order. Keep the helper-driven cascade behaviour
            // (deleting an actor also wipes its bound audio rows,
            // deleting an audio with a `parent_actor` cascades to
            // the parent actor, etc.) by routing each entry through
            // the existing per-kind branches below — but with the
            // undo push already done, so the inner branches must NOT
            // re-snapshot.
            let mut actor_idxs: Vec<usize> = Vec::new();
            let mut overlay_idxs: Vec<usize> = Vec::new();
            let mut audio_idxs: Vec<usize> = Vec::new();
            let mut bg_idxs: Vec<usize> = Vec::new();
            for sel in &multi_targets {
                match *sel {
                    Selection::Actor(i) => actor_idxs.push(i),
                    Selection::Overlay(i) => overlay_idxs.push(i),
                    Selection::Audio(i) => audio_idxs.push(i),
                    Selection::Background(i) => bg_idxs.push(i),
                    _ => {}
                }
            }
            actor_idxs.sort_unstable();
            actor_idxs.dedup();
            overlay_idxs.sort_unstable();
            overlay_idxs.dedup();
            audio_idxs.sort_unstable();
            audio_idxs.dedup();
            bg_idxs.sort_unstable();
            bg_idxs.dedup();

            // Deleting a selected bound audio row should delete the
            // video actor it belongs to, just like the single-selection
            // audio branch below. Add those parent actors to the actor
            // bucket before subtree expansion and before we filter bound
            // audio rows out of the explicit audio bucket.
            for &aui in &audio_idxs {
                let Some(parent_id) = self
                    .state
                    .scene
                    .audio
                    .get(aui)
                    .and_then(|audio| audio.parent_actor.clone())
                else {
                    continue;
                };
                if let Some(actor_idx) = self
                    .state
                    .scene
                    .actors
                    .iter()
                    .position(|actor| actor.id == parent_id)
                {
                    actor_idxs.push(actor_idx);
                }
            }
            actor_idxs.sort_unstable();
            actor_idxs.dedup();

            // ── Cascade preview ──
            // Audio rows whose `parent_actor` matches an actor we're
            // about to remove get cleaned up by
            // `remove_audio_bound_to_actor` already. To avoid
            // double-cleanup (and the index-shift drift that comes
            // with it) drop those audio indices BEFORE we run the
            // audio loop.
            let actor_ids_being_deleted: std::collections::HashSet<String> = actor_idxs
                .iter()
                .filter_map(|i| self.state.scene.actors.get(*i).map(|a| a.id.clone()))
                .collect();
            audio_idxs.retain(|aui| {
                let parent = self
                    .state
                    .scene
                    .audio
                    .get(*aui)
                    .and_then(|a| a.parent_actor.clone());
                match parent {
                    Some(pid) => !actor_ids_being_deleted.contains(&pid),
                    None => true,
                }
            });

            // Walk descending so removing higher indices keeps
            // lower ones valid.
            let mut family_actor_idxs: Vec<usize> = Vec::new();
            let mut family_overlay_idxs: Vec<usize> = Vec::new();
            for i in actor_idxs.iter().copied() {
                if i >= self.state.scene.actors.len() {
                    continue;
                }
                let subtree = self
                    .state
                    .scene
                    .collect_element_subtree_ids(&self.state.scene.actors[i].id);
                for id in subtree {
                    if let Some(ai) = self.state.scene.actors.iter().position(|a| a.id == id) {
                        family_actor_idxs.push(ai);
                    }
                    if let Some(oi) = self.state.scene.overlays.iter().position(|ov| match ov {
                        memstroy_core::Overlay::Text(o) => o.id == *id,
                        memstroy_core::Overlay::Image(o) => o.id == *id,
                        memstroy_core::Overlay::Video(o) => o.id == *id,
                    }) {
                        family_overlay_idxs.push(oi);
                    }
                }
            }
            // Direct overlay picks (text, images, FX, …) were collected
            // into `overlay_idxs` but never deleted — only actor-subtree
            // expansion ran. Expand each selected overlay's subtree too.
            for i in overlay_idxs.iter().copied() {
                if i >= self.state.scene.overlays.len() {
                    continue;
                }
                let root_id = match &self.state.scene.overlays[i] {
                    memstroy_core::Overlay::Text(o) => o.id.clone(),
                    memstroy_core::Overlay::Image(o) => o.id.clone(),
                    memstroy_core::Overlay::Video(o) => o.id.clone(),
                };
                let subtree = self.state.scene.collect_element_subtree_ids(&root_id);
                for id in subtree {
                    if let Some(ai) = self.state.scene.actors.iter().position(|a| a.id == id) {
                        family_actor_idxs.push(ai);
                    }
                    if let Some(oi) = self.state.scene.overlays.iter().position(|ov| match ov {
                        memstroy_core::Overlay::Text(o) => o.id == *id,
                        memstroy_core::Overlay::Image(o) => o.id == *id,
                        memstroy_core::Overlay::Video(o) => o.id == *id,
                    }) {
                        family_overlay_idxs.push(oi);
                    }
                }
            }
            family_actor_idxs.sort_unstable();
            family_actor_idxs.dedup();
            family_overlay_idxs.sort_unstable();
            family_overlay_idxs.dedup();

            let mut layout_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for &i in &family_actor_idxs {
                if let Some(a) = self.state.scene.actors.get(i) {
                    layout_ids.extend(self.state.scene.collect_element_subtree_ids(&a.id));
                }
            }
            for &i in &family_overlay_idxs {
                if i >= self.state.scene.overlays.len() {
                    continue;
                }
                let root_id = match &self.state.scene.overlays[i] {
                    memstroy_core::Overlay::Text(o) => o.id.clone(),
                    memstroy_core::Overlay::Image(o) => o.id.clone(),
                    memstroy_core::Overlay::Video(o) => o.id.clone(),
                };
                layout_ids.extend(self.state.scene.collect_element_subtree_ids(&root_id));
            }
            self.state
                .scene
                .canvas_layouts
                .retain(|cl| !layout_ids.contains(&cl.element_id));

            for i in family_actor_idxs.iter().rev().copied() {
                if i >= self.state.scene.actors.len() {
                    continue;
                }
                crate::panels::collapse_footage_sequence_gap_for_actor(&mut self.state, i);
                let actor_id = self.state.scene.actors[i].id.clone();
                let _removed_audio =
                    crate::panels::remove_audio_bound_to_actor(&mut self.state, &actor_id);
                self.state.scene.actors.remove(i);
                if i < self.state.frame_caches.len() {
                    self.state.frame_caches.remove(i);
                }
                if i < self.frame_extract_results.len() {
                    self.frame_extract_results.remove(i);
                }
                crate::panels::shift_assignments_after_remove(
                    &mut self.state.actor_track_assignments,
                    i,
                );
            }
            for i in family_overlay_idxs.iter().rev().copied() {
                if i >= self.state.scene.overlays.len() {
                    continue;
                }
                self.state.scene.overlays.remove(i);
                crate::panels::shift_assignments_after_remove(
                    &mut self.state.overlay_track_assignments,
                    i,
                );
            }
            crate::canvas_image_search::on_overlays_removed(&mut self.state, &family_overlay_idxs);
            for i in audio_idxs.into_iter().rev() {
                if i >= self.state.scene.audio.len() {
                    continue;
                }
                self.mark_audio_track_deleted(i);
            }
            for i in bg_idxs.into_iter().rev() {
                if i >= self.state.scene.backgrounds.len() {
                    continue;
                }
                self.state.scene.backgrounds.remove(i);
            }

            self.state.selection = Selection::None;
            self.state.canvas_selection.clear();
            self.state.multi_select.clear();
            self.state.status = crate::i18n::t("Selected layers deleted.").into();
            return;
        }

        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let actor_id = self.state.scene.actors[i].id.clone();
                let subtree = self.state.scene.collect_element_subtree_ids(&actor_id);
                let mut actor_idxs: Vec<usize> = subtree
                    .iter()
                    .filter_map(|id| self.state.scene.actors.iter().position(|a| &a.id == id))
                    .collect();
                let mut overlay_idxs: Vec<usize> = subtree
                    .iter()
                    .filter_map(|id| {
                        self.state.scene.overlays.iter().position(|ov| match ov {
                            memstroy_core::Overlay::Text(o) => &o.id == id,
                            memstroy_core::Overlay::Image(o) => &o.id == id,
                            memstroy_core::Overlay::Video(o) => &o.id == id,
                        })
                    })
                    .collect();
                actor_idxs.sort_unstable();
                actor_idxs.dedup();
                overlay_idxs.sort_unstable();
                overlay_idxs.dedup();

                for idx in actor_idxs.iter().rev().copied() {
                    let aid = self.state.scene.actors[idx].id.clone();
                    let _removed_audio =
                        crate::panels::remove_audio_bound_to_actor(&mut self.state, &aid);
                }
                self.state.mutate_state(|s| {
                    s.scene
                        .canvas_layouts
                        .retain(|cl| !subtree.contains(&cl.element_id));
                    for idx in overlay_idxs.iter().rev().copied() {
                        if idx < s.scene.overlays.len() {
                            s.scene.overlays.remove(idx);
                        }
                        crate::panels::shift_assignments_after_remove(
                            &mut s.overlay_track_assignments,
                            idx,
                        );
                    }
                    for idx in actor_idxs.iter().rev().copied() {
                        if idx < s.scene.actors.len() {
                            crate::panels::collapse_footage_sequence_gap_for_actor(s, idx);
                            s.scene.actors.remove(idx);
                        }
                        crate::panels::shift_assignments_after_remove(
                            &mut s.actor_track_assignments,
                            idx,
                        );
                    }
                    s.scene.purge_orphan_canvas_layouts();
                });
                for idx in actor_idxs.iter().rev().copied() {
                    if idx < self.state.frame_caches.len() {
                        self.state.frame_caches.remove(idx);
                    }
                    if idx < self.frame_extract_results.len() {
                        self.frame_extract_results.remove(idx);
                    }
                }
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = crate::i18n::t("Actor deleted.").into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let root_id = match &self.state.scene.overlays[i] {
                    memstroy_core::Overlay::Text(o) => o.id.clone(),
                    memstroy_core::Overlay::Image(o) => o.id.clone(),
                    memstroy_core::Overlay::Video(o) => o.id.clone(),
                };
                let subtree = self.state.scene.collect_element_subtree_ids(&root_id);
                let mut overlay_idxs: Vec<usize> = subtree
                    .iter()
                    .filter_map(|id| {
                        self.state.scene.overlays.iter().position(|ov| match ov {
                            memstroy_core::Overlay::Text(o) => &o.id == id,
                            memstroy_core::Overlay::Image(o) => &o.id == id,
                            memstroy_core::Overlay::Video(o) => &o.id == id,
                        })
                    })
                    .collect();
                let mut actor_idxs: Vec<usize> = subtree
                    .iter()
                    .filter_map(|id| self.state.scene.actors.iter().position(|a| &a.id == id))
                    .collect();
                overlay_idxs.sort_unstable();
                overlay_idxs.dedup();
                actor_idxs.sort_unstable();
                actor_idxs.dedup();

                for idx in actor_idxs.iter().rev().copied() {
                    let aid = self.state.scene.actors[idx].id.clone();
                    let _removed_audio =
                        crate::panels::remove_audio_bound_to_actor(&mut self.state, &aid);
                }
                self.state.mutate_state(|s| {
                    s.scene
                        .canvas_layouts
                        .retain(|cl| !subtree.contains(&cl.element_id));
                    for idx in overlay_idxs.iter().rev().copied() {
                        if idx < s.scene.overlays.len() {
                            s.scene.overlays.remove(idx);
                        }
                        crate::panels::shift_assignments_after_remove(
                            &mut s.overlay_track_assignments,
                            idx,
                        );
                    }
                    for idx in actor_idxs.iter().rev().copied() {
                        if idx < s.scene.actors.len() {
                            crate::panels::collapse_footage_sequence_gap_for_actor(s, idx);
                            s.scene.actors.remove(idx);
                        }
                        crate::panels::shift_assignments_after_remove(
                            &mut s.actor_track_assignments,
                            idx,
                        );
                    }
                    s.scene.purge_orphan_canvas_layouts();
                });
                for idx in actor_idxs.iter().rev().copied() {
                    if idx < self.state.frame_caches.len() {
                        self.state.frame_caches.remove(idx);
                    }
                    if idx < self.frame_extract_results.len() {
                        self.frame_extract_results.remove(idx);
                    }
                }
                crate::canvas_image_search::on_overlays_removed(&mut self.state, &overlay_idxs);
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = crate::i18n::t("Overlay deleted.").into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                self.state.mutate(|s| {
                    s.backgrounds.remove(i);
                });
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = crate::i18n::t("Background deleted.").into();
            }
            Selection::Audio(i) if i < self.state.scene.audio.len() => {
                // Skip if already deleted
                if self.state.scene.audio[i].deleted {
                    return;
                }

                // Cascade audio → parent actor: deleting a sound that
                // is bound to a video clip via `parent_actor` removes
                // the parent clip too, which then removes every
                // SIBLING audio bound to the same parent through the
                // existing `remove_audio_bound_to_actor` path. This
                // closes the "delete attached layers together" loop
                // in the audio→video direction (the actor branch
                // above already handles the video→audio direction).
                let parent_actor_id = self.state.scene.audio[i].parent_actor.clone();
                if let Some(parent_id) = parent_actor_id {
                    if let Some(actor_idx) = self
                        .state
                        .scene
                        .actors
                        .iter()
                        .position(|a| a.id == parent_id)
                    {
                        // Re-target the selection at the parent and
                        // recurse — the existing actor branch already
                        // wipes every bound audio (including this
                        // one), the matching frame_caches /
                        // frame_extract_results entries, and the
                        // actor_track_assignments map. We then drop
                        // back through the recursion with a clean
                        // selection.
                        self.state.selection = Selection::Actor(actor_idx);
                        self.delete_selected();
                        return;
                    }
                    // Orphaned binding (parent already gone) — fall
                    // through to a plain audio remove so the user can
                    // still get rid of the row.
                }
                // Mark as deleted instead of removing from array to prevent
                // index shifts that cause other audio tracks to "jump" layers.
                // For unlinked rows we still try to infer the originating
                // actor and mute its embedded fallback audio.
                self.mark_audio_track_deleted(i);
                // No need to remove from side-tables since we're not
                // removing from the array — the track is just marked
                // as deleted and will be hidden in the UI.
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = crate::i18n::t("Audio deleted.").into();
            }
            _ => {}
        }
    }

    fn duplicate_selected(&mut self) {
        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let mut dup = self.state.scene.actors[i].clone();
                dup.id = format!("{}_copy", dup.id);
                let new_idx = self.state.scene.actors.len();
                self.state.mutate(move |s| {
                    s.actors.push(dup);
                });
                self.state.selection = Selection::Actor(new_idx);
                self.state.status = crate::i18n::t("Actor duplicated.").into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let mut dup = self.state.scene.overlays[i].clone();
                match &mut dup {
                    memstroy_core::Overlay::Text(t) => t.id = format!("{}_copy", t.id),
                    memstroy_core::Overlay::Image(im) => im.id = format!("{}_copy", im.id),
                    memstroy_core::Overlay::Video(v) => v.id = format!("{}_copy", v.id),
                }
                let new_idx = self.state.scene.overlays.len();
                self.state.mutate(move |s| {
                    s.overlays.push(dup);
                });
                self.state.selection = Selection::Overlay(new_idx);
                self.state.status = crate::i18n::t("Overlay duplicated.").into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                let mut dup = self.state.scene.backgrounds[i].clone();
                dup.id = format!("{}_copy", dup.id);
                let new_idx = self.state.scene.backgrounds.len();
                self.state.mutate(move |s| {
                    s.backgrounds.push(dup);
                });
                self.state.selection = Selection::Background(new_idx);
                self.state.status = crate::i18n::t("Background duplicated.").into();
            }
            _ => {}
        }
    }

    /// Split the selected element at the current playhead position.
    /// Creates two adjacent elements: [original_start..playhead] and [playhead..original_end].
    fn split_at_playhead(&mut self, cut_t: Option<f32>) {
        let t = cut_t.unwrap_or(self.state.playhead);
        match self.state.selection {
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                self.split_background_at(i, t);
            }
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                // Splitting a video clip cascades to every audio that
                // declared this actor as its `parent_actor`. Without
                // this the user would have to manually re-cut each
                // bound audio track at the same playhead — and the
                // sync_audio_to_actor pass would silently snap the
                // audio back to the LEFT half's window the next
                // frame, so the right half's audio would simply
                // vanish. This is the user-facing "разрезанные слои
                // с прикреплёнными аудио тоже должны разрезаться"
                // requirement.
                self.split_actor_with_cascade(i, t);
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                self.split_overlay_at(i, t);
            }
            Selection::Audio(i) if i < self.state.scene.audio.len() => {
                // Splitting an audio with `parent_actor` cascades the
                // other direction: the parent video clip and every
                // sibling audio bound to the same parent are split at
                // the same playhead so the bound pair (and any
                // additional bindings) stay in lock-step.
                self.split_audio_with_cascade(i, t);
            }
            _ => {
                self.state.status = crate::i18n::t("\u{26A0} Select an element to split.").into();
            }
        }
    }

    /// Split the background segment at index `i` at scene-time `t`.
    /// Returns `true` when the split happened (`t` is strictly inside
    /// the segment); otherwise leaves the scene untouched and posts a
    /// status message.
    fn split_background_at(&mut self, i: usize, t: f32) -> bool {
        if i >= self.state.scene.backgrounds.len() {
            return false;
        }
        let bg = &self.state.scene.backgrounds[i];
        let start = bg.start;
        let end = bg.start + bg.duration;
        if t <= start || t >= end {
            self.state.status =
                crate::i18n::t("\u{26A0} Playhead is outside this background's range.").into();
            return false;
        }
        let mut right = bg.clone();
        right.id =
            crate::panels::unique_background_id_in_scene(&self.state.scene.backgrounds, &right.id);
        right.start = t;
        right.duration = end - t;
        let left_dur = t - start;
        self.state.mutate(move |s| {
            s.backgrounds[i].duration = left_dur;
            s.backgrounds.insert(i + 1, right);
        });
        self.state.status = crate::i18n::t("\u{2702} Background split at playhead.").into();
        true
    }

    /// Split the actor at index `i` at scene-time `t`. Returns
    /// `Some((right_idx, right_id))` so the caller can re-parent any
    /// audio that was bound to this actor over to the right half.
    /// Returns `None` when `t` lies outside the actor's window.
    ///
    /// Actor `layout` keyframes are stored in **scene-time**
    /// (see `Scene::Actor` in memstroy-core, and
    /// `canvas_preview` / renderer both call
    /// `keyframe::sample(&actor.layout, scene_t)`). The split
    /// therefore partitions the kfs by their absolute `kf.t` against
    /// the cut point `t` — no local-time conversion is needed and
    /// the right half's kfs keep their scene-time as-is.
    fn split_actor_at(&mut self, i: usize, t: f32) -> Option<(usize, String)> {
        if i >= self.state.scene.actors.len() {
            return None;
        }
        let a = &self.state.scene.actors[i];
        let start = a.t_in.unwrap_or(0.0);
        let end = a.t_out.unwrap_or(self.state.scene.output.duration);
        if t <= start || t >= end {
            self.state.status =
                crate::i18n::t("\u{26A0} Playhead is outside this actor's range.").into();
            return None;
        }
        let mut right = a.clone();
        // Disambiguate the new id against every actor (and the
        // original) so a second cut on the same clip doesn't
        // produce duplicate ids that confuse `parent_actor`
        // bindings down the line.
        right.id = crate::panels::unique_actor_id_in_scene(&self.state.scene.actors, &a.id);
        let right_id = right.id.clone();
        right.t_in = Some(t);
        right.t_out = Some(end);
        right.source_start = if a.mellstroy_footage.edge_frame {
            a.source_start
        } else {
            a.source_start + (t - start) * a.speed.max(0.0001)
        };
        let original_lane = self.state.actor_track_assignments.get(&i).copied();
        self.state.mutate(move |s| {
            s.actors[i].t_out = Some(t);
            s.actors.insert(i + 1, right);
        });
        crate::split_crop::finish_actor_split(&mut self.state.scene, i, i + 1, t, t);
        let pivot = i + 1;
        let mut shifted: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::with_capacity(self.state.actor_track_assignments.len() + 1);
        for (k, v) in self.state.actor_track_assignments.iter() {
            let new_k = if *k >= pivot { *k + 1 } else { *k };
            shifted.insert(new_k, *v);
        }
        if let Some(lane) = original_lane {
            shifted.insert(pivot, lane);
        }
        self.state.actor_track_assignments = shifted;
        // Frame caches mirror the actors Vec by index — slot a
        // placeholder in for the right half so the rest of the
        // cache table stays index-aligned with the scene. We pass
        // the actual source path so `ensure_frame_caches` can
        // detect it needs extraction and kick it off on the next
        // frame — previously the empty PathBuf meant the right half
        // had no preview until a full re-extract was triggered.
        let right_source = self.state.scene.actors[pivot].source.clone();
        crate::video_cache::FrameCache::insert_for_actor_split(
            &mut self.state.frame_caches,
            i,
            pivot,
            right_source,
        );
        if pivot <= self.frame_extract_results.len() {
            self.frame_extract_results
                .insert(pivot, std::sync::Arc::new(std::sync::Mutex::new(None)));
        }

        Some((pivot, right_id))
    }

    /// Split actor `i` at `t`, then split every audio with
    /// `parent_actor == actor.id` at the same scene-time and re-parent
    /// the right-half audio rows over to the freshly inserted
    /// right-half actor's id.
    fn split_actor_with_cascade(&mut self, i: usize, t: f32) {
        if i >= self.state.scene.actors.len() {
            return;
        }
        let actor_id = self.state.scene.actors[i].id.clone();

        // Capture bound-audio indices BEFORE we mutate the audio Vec.
        // Sorted descending so consecutive splits don't shift earlier
        // entries (each split inserts at au_idx+1, only affecting
        // strictly higher indices).
        let mut bound_audio: Vec<usize> = self
            .state
            .scene
            .audio
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_actor.as_deref() == Some(actor_id.as_str()))
            .map(|(idx, _)| idx)
            .collect();
        bound_audio.sort_unstable_by(|a, b| b.cmp(a));

        let Some((_right_idx, right_actor_id)) = self.split_actor_at(i, t) else {
            return;
        };

        for au_idx in bound_audio {
            if au_idx >= self.state.scene.audio.len() {
                continue;
            }
            // Tolerate the case where the bound audio's window doesn't
            // contain the playhead (clip was previously trimmed past
            // `t`). We just skip those rows instead of erroring out;
            // the actor split already happened so partial cascade is
            // strictly better than aborting.
            let au = &self.state.scene.audio[au_idx];
            let au_start = au.t_in;
            let au_end = au.t_out.unwrap_or(self.state.scene.output.duration);
            if t <= au_start || t >= au_end {
                continue;
            }
            let _ = self.split_audio_at(au_idx, t, Some(right_actor_id.clone()));
        }

        self.state.status = crate::i18n::t("\u{2702} Actor split at playhead.").into();
    }

    /// Split the overlay at index `i` at scene-time `t`. Overlays have
    /// no parent / child relationships so this never cascades.
    ///
    /// Overlay `layout` keyframes are stored in **clip-local time**
    /// (sampled with `kf.t = scene_t - t_in`), so the right half's
    /// kfs need their `t` rebased to the new t_in. Video overlays
    /// also bump `source_start` so playback continues seamlessly
    /// across the cut.
    fn split_overlay_at(&mut self, i: usize, t: f32) -> bool {
        if i >= self.state.scene.overlays.len() {
            return false;
        }
        let ov = &self.state.scene.overlays[i];
        let (start, end) = match ov {
            memstroy_core::Overlay::Text(txt) => (txt.t_in, txt.t_out),
            memstroy_core::Overlay::Image(im) => (im.t_in, im.t_out),
            memstroy_core::Overlay::Video(v) => (v.t_in, v.t_out),
        };
        if t <= start || t >= end {
            self.state.status =
                crate::i18n::t("\u{26A0} Playhead is outside this overlay's range.").into();
            return false;
        }
        let mut right = ov.clone();
        match &mut right {
            memstroy_core::Overlay::Text(txt) => {
                txt.id =
                    crate::panels::unique_overlay_id_in_scene(&self.state.scene.overlays, &txt.id);
            }
            memstroy_core::Overlay::Image(im) => {
                im.id =
                    crate::panels::unique_overlay_id_in_scene(&self.state.scene.overlays, &im.id);
            }
            memstroy_core::Overlay::Video(v) => {
                v.id = crate::panels::unique_overlay_id_in_scene(&self.state.scene.overlays, &v.id);
            }
        }
        let original_overlay_lane = self.state.overlay_track_assignments.get(&i).copied();
        self.state.mutate(move |s| {
            match &mut s.overlays[i] {
                memstroy_core::Overlay::Text(txt) => txt.t_out = t,
                memstroy_core::Overlay::Image(im) => im.t_out = t,
                memstroy_core::Overlay::Video(v) => v.t_out = t,
            }
            s.overlays.insert(i + 1, right);
        });
        crate::split_crop::finish_overlay_split(&mut self.state.scene, i, i + 1, t, t);
        let pivot = i + 1;
        let mut shifted: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::with_capacity(
                self.state.overlay_track_assignments.len() + 1,
            );
        for (k, v) in self.state.overlay_track_assignments.iter() {
            let new_k = if *k >= pivot { *k + 1 } else { *k };
            shifted.insert(new_k, *v);
        }
        if let Some(lane) = original_overlay_lane {
            shifted.insert(pivot, lane);
        }
        self.state.overlay_track_assignments = shifted;

        self.state.status = crate::i18n::t("\u{2702} Overlay split at playhead.").into();
        true
    }

    /// Split the audio track at index `i` at scene-time `t`. When
    /// `right_parent` is `Some`, that string overrides the right
    /// half's `parent_actor`; otherwise the right half inherits the
    /// original's `parent_actor` value. Returns the right-half
    /// audio's index, or `None` when `t` lies outside the audio's
    /// playable window.
    fn split_audio_at(&mut self, i: usize, t: f32, right_parent: Option<String>) -> Option<usize> {
        if i >= self.state.scene.audio.len() {
            return None;
        }
        let au = &self.state.scene.audio[i];
        let start = au.t_in;
        let end = au.t_out.unwrap_or(self.state.scene.output.duration);
        if t <= start || t >= end {
            self.state.status =
                crate::i18n::t("\u{26A0} Playhead is outside this audio's range.").into();
            return None;
        }
        let mut right = au.clone();
        right.id = crate::panels::unique_audio_id_in_scene(&self.state.scene.audio, &au.id);
        right.t_in = t;
        right.t_out = Some(end);
        right.source_start = au.source_start + (t - start).max(0.0) * au.speed.max(0.0001);
        if let Some(rp) = right_parent {
            right.parent_actor = Some(rp);
        }
        // Audio per-param keyframe vectors live in CLIP-LOCAL time.
        // Split them at `local_split = t - t_in_original`: the left
        // half keeps kfs with `kf.t <= local_split`, the right half
        // keeps `kf.t >= local_split` and rebases their times so
        // `kf.t = 0` matches the right half's `t_in`.
        let local_split = (t - start).max(0.0);
        let crop_right = |kfs: &mut Vec<memstroy_core::Keyframe<f32>>, edge: f32| {
            kfs.retain(|kf| kf.t >= edge - 1.0e-3);
            for kf in kfs.iter_mut() {
                kf.t = (kf.t - edge).max(0.0);
            }
        };
        crop_right(&mut right.volume_kfs, local_split);
        crop_right(&mut right.speed_kfs, local_split);
        crop_right(&mut right.pitch_kfs, local_split);
        crop_right(&mut right.pan_kfs, local_split);
        crop_right(&mut right.low_pass_kfs, local_split);
        crop_right(&mut right.high_pass_kfs, local_split);
        crop_right(&mut right.reverb_kfs, local_split);
        // Resolve the audio's CURRENT lane. If no explicit assignment
        // exists (round-robin fallback was being used), compute what
        // lane the timeline draws this audio on RIGHT NOW so both
        // split halves end up on the same lane.
        let original_lane = self
            .state
            .audio_track_assignments
            .get(&i)
            .copied()
            .unwrap_or_else(|| {
                let audio_tracks: Vec<usize> = (0..self.state.tracks.len())
                    .filter(|ti| self.state.tracks[*ti].kind == crate::state::TrackKind::Audio)
                    .collect();
                if audio_tracks.is_empty() {
                    0
                } else {
                    audio_tracks[i % audio_tracks.len()]
                }
            });
        let local_split_left = local_split;
        self.state.mutate(move |s| {
            s.audio[i].t_out = Some(t);
            // Crop left-half kfs whose clip-local time runs past
            // the cut. Done one field at a time so the borrow
            // checker is happy with sequential `&mut` borrows of
            // disjoint fields.
            let crop = |kfs: &mut Vec<memstroy_core::Keyframe<f32>>, edge: f32| {
                kfs.retain(|kf| kf.t <= edge + 1.0e-3);
            };
            crop(&mut s.audio[i].volume_kfs, local_split_left);
            crop(&mut s.audio[i].speed_kfs, local_split_left);
            crop(&mut s.audio[i].pitch_kfs, local_split_left);
            crop(&mut s.audio[i].pan_kfs, local_split_left);
            crop(&mut s.audio[i].low_pass_kfs, local_split_left);
            crop(&mut s.audio[i].high_pass_kfs, local_split_left);
            crop(&mut s.audio[i].reverb_kfs, local_split_left);
            s.audio.insert(i + 1, right);
        });
        let pivot = i + 1;
        crate::panels::shift_assignments_for_insert(&mut self.state.audio_track_assignments, pivot);
        // Ensure BOTH halves have explicit assignments to the same
        // lane. Without an explicit assignment for the LEFT half, the
        // round-robin fallback in the timeline draw might place it on
        // a different lane than the resolved `original_lane` we used
        // for the RIGHT half — and the user would see the audio jump
        // between lanes after split.
        self.state.audio_track_assignments.insert(i, original_lane);
        self.state
            .audio_track_assignments
            .insert(pivot, original_lane);
        if pivot <= self.state.audio_waveforms.len() {
            self.state
                .audio_waveforms
                .insert(pivot, crate::state::AudioWaveform::default());
        }
        if pivot <= self.waveform_extract_results.len() {
            self.waveform_extract_results
                .insert(pivot, std::sync::Arc::new(std::sync::Mutex::new(None)));
        }
        Some(pivot)
    }

    /// Split the audio at index `i` at `t`. When the audio has a
    /// `parent_actor`, also split that actor and every sibling audio
    /// bound to the same parent — same cascade as
    /// `split_actor_with_cascade`, just initiated from the audio
    /// side. The originating audio is split inline (with the right
    /// half re-parented to the new right-half actor) so the
    /// cascade pass never tries to double-cut it.
    fn split_audio_with_cascade(&mut self, i: usize, t: f32) {
        if i >= self.state.scene.audio.len() {
            return;
        }
        let parent_actor_id = self.state.scene.audio[i].parent_actor.clone();

        let Some(parent_id) = parent_actor_id else {
            // Standalone audio — just split the one row.
            if self.split_audio_at(i, t, None).is_some() {
                self.state.status = crate::i18n::t("\u{2702} Audio split at playhead.").into();
            }
            return;
        };

        // Locate the parent actor by id. If it's gone (orphaned
        // binding), fall back to a standalone audio split so the
        // user still gets the cut they asked for.
        let parent_idx = match self
            .state
            .scene
            .actors
            .iter()
            .position(|a| a.id == parent_id)
        {
            Some(p) => p,
            None => {
                if self.split_audio_at(i, t, None).is_some() {
                    self.state.status = crate::i18n::t("\u{2702} Audio split at playhead.").into();
                }
                return;
            }
        };

        // Capture every sibling audio bound to the same parent
        // BEFORE any split (they all need the same right-actor id
        // for re-parenting). Sorted descending so insertions don't
        // invalidate later entries.
        let mut sibling_audio: Vec<usize> = self
            .state
            .scene
            .audio
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_actor.as_deref() == Some(parent_id.as_str()))
            .map(|(idx, _)| idx)
            .collect();
        sibling_audio.sort_unstable_by(|a, b| b.cmp(a));

        let Some((_right_actor_idx, right_actor_id)) = self.split_actor_at(parent_idx, t) else {
            return;
        };

        // Split every captured audio row. The originating audio is
        // included in this list because it has the same parent_actor.
        // All right halves get re-parented to right_actor_id.
        for au_idx in sibling_audio {
            if au_idx >= self.state.scene.audio.len() {
                continue;
            }
            let au = &self.state.scene.audio[au_idx];
            let au_start = au.t_in;
            let au_end = au.t_out.unwrap_or(self.state.scene.output.duration);
            if t <= au_start || t >= au_end {
                continue;
            }
            let _ = self.split_audio_at(au_idx, t, Some(right_actor_id.clone()));
        }

        self.state.status = crate::i18n::t("\u{2702} Audio split at playhead.").into();
    }

    /// Merge the selected element with its next sibling of the same kind.
    /// The merged result spans from the selected's start to the sibling's end.
    fn merge_next(&mut self) {
        match self.state.selection {
            Selection::Background(i) if i + 1 < self.state.scene.backgrounds.len() => {
                let next_end = self.state.scene.backgrounds[i + 1].start
                    + self.state.scene.backgrounds[i + 1].duration;
                let start = self.state.scene.backgrounds[i].start;
                self.state.mutate(move |s| {
                    s.backgrounds[i].duration = next_end - start;
                    s.backgrounds.remove(i + 1);
                });
                self.state.status = crate::i18n::t("Backgrounds merged.").into();
            }
            Selection::Actor(i) if i + 1 < self.state.scene.actors.len() => {
                let next = self.state.scene.actors[i + 1].clone();
                self.state.mutate(move |s| {
                    let end = next.t_out.unwrap_or(s.output.duration);
                    s.actors[i].t_out = Some(end);
                    // Merge keyframes from the next actor.
                    for kf in &next.layout {
                        if !s.actors[i].layout.iter().any(|k| (k.t - kf.t).abs() < 0.01) {
                            s.actors[i].layout.push(kf.clone());
                        }
                    }
                    s.actors[i]
                        .layout
                        .sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
                    s.actors.remove(i + 1);
                });
                self.state.status = crate::i18n::t("Actors merged.").into();
            }
            Selection::Overlay(i) if i + 1 < self.state.scene.overlays.len() => {
                let next_end = match &self.state.scene.overlays[i + 1] {
                    memstroy_core::Overlay::Text(t) => t.t_out,
                    memstroy_core::Overlay::Image(im) => im.t_out,
                    memstroy_core::Overlay::Video(v) => v.t_out,
                };
                self.state.mutate(move |s| {
                    match &mut s.overlays[i] {
                        memstroy_core::Overlay::Text(t) => t.t_out = next_end,
                        memstroy_core::Overlay::Image(im) => im.t_out = next_end,
                        memstroy_core::Overlay::Video(v) => v.t_out = next_end,
                    }
                    s.overlays.remove(i + 1);
                });
                self.state.status = crate::i18n::t("Overlays merged.").into();
            }
            _ => {
                self.state.status =
                    crate::i18n::t("\u{26A0} Select an element with a next sibling to merge.")
                        .into();
            }
        }
    }

    /// Start waveform extraction for all audio tracks that don't yet have waveform data.
    /// Spawns background tasks (similar to frame extraction pattern) that call
    /// `AudioWaveform::extract_peaks()` and store results in shared slots.
    fn start_waveform_extraction(&mut self) {
        let num_audio = self.state.scene.audio.len();
        if num_audio == 0 {
            return;
        }

        // Ensure audio_waveforms vec is sized to match audio tracks
        while self.state.audio_waveforms.len() < num_audio {
            self.state
                .audio_waveforms
                .push(crate::state::AudioWaveform::default());
        }

        let mut started = 0usize;
        for audio_idx in 0..num_audio {
            let wf = &self.state.audio_waveforms[audio_idx];
            if wf.ready || wf.extracting || wf.failed {
                continue;
            }

            let source = self.state.scene.audio[audio_idx].source.clone();
            if !source.exists() {
                self.state.audio_waveforms[audio_idx].failed = true;
                self.state.audio_waveforms[audio_idx].extracting = false;
                self.state.audio_waveforms[audio_idx].extracting_since = None;
                continue;
            }

            // Mark as extracting
            self.state.audio_waveforms[audio_idx].extracting = true;
            self.state.audio_waveforms[audio_idx].failed = false;
            self.state.audio_waveforms[audio_idx].extracting_since =
                Some(std::time::Instant::now());
            started += 1;

            let source_clone = source.clone();
            // Use a shared slot to communicate results back
            let result_slot: Arc<Mutex<Option<Option<(Vec<f32>, f32)>>>> =
                Arc::new(Mutex::new(None));
            let slot_clone = result_slot.clone();

            self.rt.as_ref().unwrap().spawn(async move {
                let peaks = tokio::task::spawn_blocking(move || {
                    crate::state::AudioWaveform::extract_peaks(&source_clone, 512)
                })
                .await
                .ok()
                .flatten();
                if let Ok(mut slot) = slot_clone.lock() {
                    *slot = Some(peaks);
                }
            });

            // Store the result slot for polling — we reuse audio_waveforms to check later
            // Since we can't easily add another vec, we'll poll inline.
            // Instead, store via a simpler approach: poll on next frame via the waveform's fields.
            // We'll use a dedicated polling vec similar to frame_extract_results.
            // For simplicity, store in a field on the waveform struct itself isn't possible (no Arc).
            // Use the existing pattern: store slots in a parallel vec.
            if self.waveform_extract_results.len() <= audio_idx {
                while self.waveform_extract_results.len() <= audio_idx {
                    self.waveform_extract_results
                        .push(Arc::new(Mutex::new(None)));
                }
            }
            self.waveform_extract_results[audio_idx] = result_slot;
        }

        if started > 0 {
            self.state.status = crate::i18n::t("Extracting audio waveforms...").into();
        }
    }

    /// Poll for waveform extraction completion across all audio tracks.
    fn poll_waveform_extraction(&mut self) {
        const WF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
        for audio_idx in 0..self.waveform_extract_results.len() {
            if audio_idx >= self.state.audio_waveforms.len() {
                break;
            }
            let wf = &mut self.state.audio_waveforms[audio_idx];
            if wf.ready {
                continue;
            }

            if wf.extracting
                && wf
                    .extracting_since
                    .is_some_and(|t| t.elapsed() > WF_TIMEOUT)
            {
                wf.extracting = false;
                wf.failed = true;
                wf.extracting_since = None;
                self.state.status = format!(
                    "{} {} {}",
                    crate::i18n::t("\u{274C} Waveform failed:"),
                    crate::i18n::t("audio"),
                    audio_idx
                );
                continue;
            }

            if let Ok(mut slot) = self.waveform_extract_results[audio_idx].lock() {
                if let Some(result) = slot.take() {
                    wf.extracting = false;
                    wf.extracting_since = None;
                    match result {
                        Some((peaks, duration)) => {
                            wf.ready = true;
                            wf.failed = false;
                            wf.peaks = peaks;
                            wf.duration = duration;
                            self.state.status = format!(
                                "{} ({} {}): {:.1}s",
                                crate::i18n::t("\u{2705} Waveform ready"),
                                crate::i18n::t("audio"),
                                audio_idx,
                                duration
                            );
                        }
                        None => {
                            wf.failed = true;
                            self.state.status = format!(
                                "{} {} {}",
                                crate::i18n::t("\u{274C} Waveform failed:"),
                                crate::i18n::t("audio"),
                                audio_idx
                            );
                        }
                    }
                }
            }
        }
    }

    /// Start frame extraction for ALL actors in the scene.
    fn start_frame_extraction(&mut self) {
        let num_actors = self.state.scene.actors.len();
        if num_actors == 0 {
            return;
        }

        // Ensure frame_caches and frame_extract_results are sized to match actors
        while self.state.frame_caches.len() < num_actors {
            let idx = self.state.frame_caches.len();
            let source = self.state.scene.actors[idx].source.clone();
            self.state
                .frame_caches
                .push(crate::video_cache::FrameCache::new(source, idx));
        }
        while self.frame_extract_results.len() < num_actors {
            self.frame_extract_results.push(Arc::new(Mutex::new(None)));
        }

        // Adaptive extraction resolution: when many actors are present,
        // extract at lower resolution to reduce disk I/O and memory
        // pressure. The visual quality at 320px is still sufficient for
        // preview purposes (the canvas typically shows actors at 200-400px
        // on screen anyway).
        let extract_width: u32 = crate::video_cache::adaptive_extract_width(num_actors);

        for actor_idx in 0..num_actors {
            let source = self.state.scene.actors[actor_idx].source.clone();

            if !EditorState::is_usable_local_video(&source) {
                continue;
            }

            // Skip if this actor's cache is already ready or extracting
            if let Some(fc) = self.state.frame_caches.get(actor_idx) {
                if (fc.is_ready() || fc.extracting) && fc.source == source {
                    continue;
                }
            }

            // Initialize or re-initialize the cache for this actor
            let mut cache = crate::video_cache::FrameCache::new(source.clone(), actor_idx);
            cache.extracting = true;
            cache.failed = false;
            cache.extract_started_at = Some(std::time::Instant::now());
            self.state.frame_caches[actor_idx] = cache;

            let result_slot = self.frame_extract_results[actor_idx].clone();
            // Clear any previous result
            if let Ok(mut slot) = result_slot.lock() {
                *slot = None;
            }

            let width = extract_width;
            let rt_handle = self.rt.as_ref().unwrap().handle().clone();
            rt_handle.spawn(async move {
                tokio::task::spawn_blocking(move || {
                    crate::video_cache::extract_frames_blocking_with_scale(
                        source,
                        width,
                        move |outcome| {
                            if let Ok(mut slot) = result_slot.lock() {
                                *slot = Some(outcome);
                            }
                        },
                    );
                })
                .await;
            });
        }

        self.state.status = crate::i18n::t("Extracting preview frames...").into();
    }

    /// Poll for frame extraction completion across all actors.
    fn poll_frame_extraction(&mut self) {
        const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        for actor_idx in 0..self.frame_extract_results.len() {
            if actor_idx < self.state.frame_caches.len() {
                let fc = &self.state.frame_caches[actor_idx];
                if fc.extracting
                    && !fc.ready
                    && fc
                        .extract_started_at
                        .is_some_and(|t| t.elapsed() > EXTRACT_TIMEOUT)
                {
                    self.state.frame_caches[actor_idx].extracting = false;
                    self.state.frame_caches[actor_idx].failed = true;
                    self.state.frame_caches[actor_idx].extract_started_at = None;
                    self.state.status = format!(
                        "{} {} {}",
                        crate::i18n::t("\u{274C} Preview frames failed:"),
                        crate::i18n::t("actor"),
                        actor_idx
                    );
                    continue;
                }
            }

            let outcome = if let Ok(mut slot) = self.frame_extract_results[actor_idx].lock() {
                slot.take()
            } else {
                None
            };
            let Some(outcome) = outcome else {
                continue;
            };
            if actor_idx >= self.state.scene.actors.len() {
                continue;
            }

            match outcome {
                Ok((duration, frame_count, cache_dir)) => {
                    let actor_t_in = self.state.scene.actors[actor_idx].t_in.unwrap_or(0.0);
                    let local_for_anim = (self.state.playhead - actor_t_in).max(0.0);
                    let path = self.state.scene.actors[actor_idx].source.clone();
                    let cur_out = self.state.scene.actors[actor_idx].t_out;
                    let ck = self.state.scene.actors[actor_idx].chroma_key.clone();
                    let cc = self.state.scene.actors[actor_idx]
                        .color_correction
                        .sampled_at(local_for_anim);
                    let effects: Vec<memstroy_core::Effect> = self.state.scene.actors[actor_idx]
                        .effects
                        .iter()
                        .map(|e| e.sampled_at(local_for_anim))
                        .collect();

                    self.state.duration_probe_pending.remove(&path);
                    self.state
                        .video_duration_cache
                        .insert(path.clone(), duration);
                    if self.state.asset_drag.dragging.as_ref() == Some(&path)
                        && self.state.asset_drag.duration_secs.is_none()
                    {
                        self.state.asset_drag.duration_secs = Some(duration);
                    }
                    let before = cur_out;
                    crate::split_crop::reconcile_actor_t_out_for_source(
                        &mut self.state.scene.actors[actor_idx],
                        duration,
                    );
                    if self.state.scene.actors[actor_idx].t_out != before {
                        crate::panels::sync_audio_to_actor(&mut self.state, actor_idx);
                    }

                    if let Some(fc) = self.state.frame_caches.get_mut(actor_idx) {
                        fc.set_ready(duration, frame_count, cache_dir);
                        fc.ensure_processed_preload(0.0, &ck, &cc, &effects);
                    }
                    self.state.status = format!(
                        "{} ({} {}): {} {} ({:.1}s)",
                        crate::i18n::t("\u{2705} Preview ready"),
                        crate::i18n::t("actor"),
                        actor_idx,
                        frame_count,
                        crate::i18n::t("frames"),
                        duration
                    );
                }
                Err(()) => {
                    if let Some(fc) = self.state.frame_caches.get_mut(actor_idx) {
                        fc.extracting = false;
                        fc.ready = false;
                        fc.failed = true;
                        fc.extract_started_at = None;
                    }
                    self.state.status = format!(
                        "{} {} {}",
                        crate::i18n::t("\u{274C} Preview frames failed:"),
                        crate::i18n::t("actor"),
                        actor_idx
                    );
                }
            }
        }
    }

    fn save_scene(&mut self) {
        if let Some(path) = self.state.scene_path.clone() {
            let is_memstroy = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("memstroy"))
                .unwrap_or(false);
            if is_memstroy {
                match self.state.save_memstroy(&path) {
                    Ok(()) => {
                        self.state.status = crate::i18n::t("\u{2705} Saved (.memstroy).").into();
                    }
                    Err(e) => {
                        self.state.status =
                            format!("{} {e}", crate::i18n::t("\u{274C} Save failed:"))
                    }
                }
            } else {
                match self.state.scene.save(&path) {
                    Ok(()) => {
                        self.state.status = crate::i18n::t("\u{2705} Saved.").into();
                        // Save layout alongside scene
                        let layout_path = path.with_extension("layout.json");
                        self.state.save_layout(&layout_path);
                    }
                    Err(e) => {
                        self.state.status =
                            format!("{} {e}", crate::i18n::t("\u{274C} Save failed:"))
                    }
                }
            }
        } else {
            self.save_as();
        }
    }

    fn save_as(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            // .memstroy is the project-native bundle (scene + layout in
            // a single JSON file). YAML / JSON remain available for
            // CLI / version-control friendliness.
            .add_filter(crate::i18n::t("Memstroy Project"), &["memstroy"])
            .add_filter(crate::i18n::t("Scene YAML"), &["yaml", "yml"])
            .add_filter(crate::i18n::t("Scene JSON"), &["json"])
            .set_file_name("project.memstroy")
            .save_file()
        {
            let is_memstroy = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("memstroy"))
                .unwrap_or(false);
            let result = if is_memstroy {
                self.state.save_memstroy(&path).map_err(|e| e.to_string())
            } else {
                self.state
                    .scene
                    .save(&path)
                    .map_err(|e| e.to_string())
                    .map(|_| {
                        let layout_path = path.with_extension("layout.json");
                        self.state.save_layout(&layout_path);
                    })
            };
            match result {
                Ok(()) => {
                    self.state.scene_path = Some(path.clone());
                    self.state.status = crate::i18n::t("\u{2705} Saved.").into();
                    if self.state.active_tab < self.state.scene_tabs.len() {
                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(crate::i18n::t("Scene"))
                            .to_string();
                        self.state.scene_tabs[self.state.active_tab].name = name;
                        self.state.scene_tabs[self.state.active_tab].path = Some(path.clone());
                        self.state.sync_scene_to_tab();
                    }
                    self.state.mark_active_tab_saved();
                }
                Err(e) => {
                    self.state.status = format!("{} {e}", crate::i18n::t("\u{274C} Save failed:"))
                }
            }
        }
    }

    fn run_render(&mut self) {
        let tx = self.tx.clone();
        self.state.status = crate::i18n::t("Choosing export path...").into();
        self.rt.as_ref().unwrap().spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter("MP4", &["mp4"])
                .save_file()
                .await
                .map(|file| file.path().to_path_buf());
            let _ = tx.send(JobEvent::RenderOutputChosen(picked));
        });
    }

    fn start_render_to_path(&mut self, path: std::path::PathBuf) {
        self.state.render_progress = Some(crate::state::RenderProgress {
            started: std::time::Instant::now(),
            last_log: String::new(),
            done: false,
            error: None,
            progress: 0.0,
            finished_elapsed: None,
        });
        // Use the scene's actual output resolution for the render.
        // The render-frame defines what portion of the canvas ends up
        // in the output; its `resolution` field IS the output file's
        // pixel dimensions. The user controls this via the inspector
        // panel (or script). We sync `output.resolution` to match
        // `render_frame.resolution` so both sides of the pipeline
        // (canvas preview's `pos * rf.resolution` and the renderer's
        // `pos * output.resolution`) agree on world-pixel coordinates.
        // Without this sync, elements positioned via the legacy
        // normalised `[0..1]` layout drift off-frame in the export.
        let mut scene_for_render = self.state.scene.clone();
        scene_for_render.output.resolution = scene_for_render.render_frame.resolution;
        scene_for_render.output.duration = scene_timeline_end(&scene_for_render).max(0.05);
        // Stamp `z_order` on every actor and overlay from the editor's
        // timeline-track assignments. Without this the renderer falls
        // back to its legacy ordering (text-behind-actors → actors →
        // image/video on top), which silently drops Mellstroy clips
        // BEHIND any image overlay that happens to live on a lower
        // track even though the preview correctly draws the clip on
        // top. See `populate_render_z_order` for the full mapping.
        crate::jobs::populate_render_z_order(&self.state, &mut scene_for_render);
        spawn_render(
            self.rt.as_ref().unwrap().handle(),
            self.tx.clone(),
            scene_for_render,
            self.state.assets_root.clone(),
            path,
        );
        self.state.status = format!("▶ {}", crate::i18n::t("Exporting..."),);
    }

    fn run_refresh(&mut self) {
        if self.state.refreshing {
            return;
        }
        if crate::state::LIBRARY_LOCAL_ONLY {
            self.state.reload_library();
            self.state.status = crate::i18n::t("Local library refreshed.").into();
            return;
        }
        self.state
            .reset_server_page_for_tab(self.state.library_tab, self.state.library_search.clone());
        self.state.reload_library();
        self.state.status = crate::i18n::t("Server library refresh requested.").into();
    }

    // ─── AUTO-SAVE / RECOVERY ────────────────────────────────────────

    /// Render the floating "Render progress" window.
    ///
    /// Surfaces a big progress bar, elapsed time, the most recent
    /// ffmpeg status line, and a clear done / failed indicator. Was
    /// added because the user reported "должен быть виден прогресс
    /// рендера" — the previous status-bar progress bar was easy to
    /// miss and, due to the ffmpeg `\r`-line bug fixed in
    /// `runner.rs::read_stderr_progress`, never updated until the
    /// encode finished anyway.
    ///
    /// The window auto-opens when a render starts (i.e.
    /// `state.render_progress` is `Some(_)`), shows the live `progress`
    /// fraction parsed from ffmpeg's stderr, and becomes a "result
    /// dialog" with a Close button once the render completes / fails.
    /// Closing clears `state.render_progress`.
    fn show_render_progress_window(&mut self, ctx: &egui::Context) {
        // Snapshot the progress so we don't hold an immutable borrow
        // through the whole closure (we need to reassign on dismiss).
        let Some(rp) = self.state.render_progress.clone() else {
            return;
        };
        // Freeze the elapsed counter once the render is done. The
        // RenderFinished handler stamps `finished_elapsed`; until
        // then we tick live off `started.elapsed()`.
        let elapsed = rp.finished_elapsed.unwrap_or_else(|| rp.started.elapsed());
        let elapsed_secs = elapsed.as_secs_f32();
        let progress = rp.progress.clamp(0.0, 1.0);
        let mut dismiss = false;

        let title = if rp.error.is_some() {
            format!("\u{274C} {}", crate::i18n::t("Render failed"))
        } else if rp.done {
            format!("\u{2705} {}", crate::i18n::t("Render complete"))
        } else {
            format!("▶ {}", crate::i18n::t("Rendering..."))
        };

        egui::Window::new(title)
            .id(egui::Id::new("render_progress_window"))
            // Free-floating: seed a default position on first open
            // (top-right of the screen, where the render bar used to
            // live) but DON'T anchor — the user can then drag the
            // window anywhere and egui remembers the position across
            // frames. Earlier we anchored it to RIGHT_TOP every
            // frame, so dragging it had no visible effect.
            .default_pos(egui::pos2(800.0, 16.0))
            .default_size([360.0, 180.0])
            .min_width(280.0)
            .max_width(520.0)
            .collapsible(true)
            .resizable(true)
            .movable(true)
            .show(ctx, |ui| {
                use egui::{Color32, RichText};
                ui.add_space(4.0);
                // Big progress bar (full width, ~22 px tall) with
                // percentage label inside — matches the visibility the
                // user asked for.
                let pct_label = if rp.done {
                    "100%".to_string()
                } else {
                    format!("{:.0}%", progress * 100.0)
                };
                let bar = egui::ProgressBar::new(if rp.done { 1.0 } else { progress })
                    .desired_width(ui.available_width())
                    .text(RichText::new(&pct_label).strong().size(14.0));
                ui.add(bar);
                ui.add_space(6.0);

                // Elapsed time + estimated total. ETA is only computed
                // once we have a non-trivial progress fraction (≥ 5%)
                // so the early-frame number isn't dominated by ffmpeg
                // initialisation cost.
                let elapsed_str = format_elapsed(elapsed_secs);
                let eta_str = if rp.done || rp.error.is_some() {
                    String::new()
                } else if progress >= 0.05 && progress < 0.999 {
                    let total = elapsed_secs / progress.max(0.001);
                    let remaining = (total - elapsed_secs).max(0.0);
                    format!(
                        " \u{2014} {} {}",
                        crate::i18n::t("ETA"),
                        format_elapsed(remaining)
                    )
                } else {
                    String::new()
                };
                ui.label(
                    RichText::new(format!(
                        "{} {}{}",
                        crate::i18n::t("Elapsed"),
                        elapsed_str,
                        eta_str,
                    ))
                    .size(11.0)
                    .color(Color32::from_rgb(180, 180, 200)),
                );
                ui.add_space(4.0);

                // Most recent ffmpeg status line. Truncated so an
                // ultra-long path inside the line doesn't blow the
                // window's width back open.
                let last = rp.last_log.clone();
                if !last.is_empty() {
                    let trimmed: String = if last.chars().count() > 200 {
                        let mut s: String = last.chars().take(197).collect();
                        s.push_str("...");
                        s
                    } else {
                        last
                    };
                    ui.label(
                        RichText::new(trimmed)
                            .size(10.0)
                            .italics()
                            .color(Color32::from_rgb(150, 150, 170)),
                    );
                }

                // Error detail.
                if let Some(err) = rp.error.as_ref() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(err)
                            .size(11.0)
                            .color(Color32::from_rgb(255, 140, 140)),
                    );
                }

                // Close / dismiss button. While the render is still in
                // flight the button is disabled — clicking through it
                // would orphan the rendering subprocess but leave its
                // result silently uncollected, which is worse than
                // forcing the user to wait.
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_close = rp.done || rp.error.is_some();
                    if ui
                        .add_enabled(can_close, egui::Button::new(crate::i18n::t("Close")))
                        .clicked()
                    {
                        dismiss = true;
                    }
                });
            });

        if dismiss {
            self.state.render_progress = None;
        }
    }

    // ─── AUTO-SAVE / RECOVERY ────────────────────────────────────────

    /// Periodically saves the current scene into the autosave manager's
    /// per-project slot. Triggered from `update()`. Updates
    /// `last_autosave` and shows a 2 s toast.
    fn tick_autosave(&mut self) {
        let interval = self.state.autosave_interval;
        let due = match self.state.last_autosave {
            Some(t) => t.elapsed().as_secs_f32() > interval,
            None => true, // schedule the first autosave shortly after launch
        };
        if !due {
            return;
        }

        // First call (None): just stamp the timer so we don't autosave on the very
        // first frame; we'll wait the configured interval before writing.
        if self.state.last_autosave.is_none() {
            self.state.last_autosave = Some(std::time::Instant::now());
            return;
        }

        // Resolve the slot id from the active tab. Saved tabs hash on
        // their canonical path so the same project always reuses the
        // same slot across editor restarts; Untitled tabs hash on the
        // tab's session-stable seed so they update one consistent slot
        // for the whole session.
        let active = self.state.active_tab;
        let (slot, tab_name, original_path) = if active < self.state.scene_tabs.len() {
            let tab = &self.state.scene_tabs[active];
            let slot = crate::autosave::slot_id(tab.path.as_deref(), tab.autosave_seed);
            (slot, tab.name.clone(), tab.path.clone())
        } else {
            // Defensive fallback: no active tab somehow. Use the legacy
            // single-file path so we still produce *something* the user
            // can recover.
            let slot = crate::autosave::slot_id(self.state.scene_path.as_deref(), 0);
            (slot, "Scene".to_string(), self.state.scene_path.clone())
        };

        let path = crate::autosave::slot_scene_path(&slot);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match self.state.save_memstroy(&path) {
            Ok(()) => {
                crate::autosave::write_meta(
                    &slot,
                    &crate::autosave::AutosaveMeta {
                        name: tab_name,
                        original_path,
                        saved_at_ms: crate::autosave::now_unix_ms(),
                    },
                );
                self.state.last_autosave = Some(std::time::Instant::now());
                self.state.autosave_toast_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                self.state.status = crate::i18n::t("Auto-saved").into();
            }
            Err(e) => {
                self.state.status = format!("{} {e}", crate::i18n::t("\u{26A0} Autosave failed:"));
            }
        }
    }

    /// Render the recovery modal when an autosave from a previous launch was
    /// detected. Lets the user restore, discard, or postpone the decision.
    fn show_recovery_dialog(&mut self, ctx: &egui::Context) {
        if !self.state.recovery_dialog_open {
            return;
        }
        let Some(autosave_path) = self.state.recovery_pending.clone() else {
            self.state.recovery_dialog_open = false;
            return;
        };

        let mut close = false;
        let mut decision: Option<&'static str> = None;

        egui::Window::new(crate::i18n::t("Recover scene?"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("\u{26A0} A recovered scene was found."))
                        .size(14.0)
                        .strong()
                        .color(Color32::from_rgb(255, 200, 80)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(autosave_path.display().to_string())
                        .size(10.0)
                        .color(Color32::from_rgb(160, 160, 180)),
                );
                ui.add_space(8.0);
                ui.label(crate::i18n::t("Restore the auto-saved scene?"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let yes = egui::Button::new(
                        RichText::new(crate::i18n::t("Yes, restore")).color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(60, 160, 80));
                    if ui.add(yes).clicked() {
                        decision = Some("yes");
                        close = true;
                    }
                    let no = egui::Button::new(
                        RichText::new(crate::i18n::t("No, discard")).color(Color32::WHITE),
                    )
                    .fill(Color32::from_rgb(200, 60, 60));
                    if ui.add(no).clicked() {
                        decision = Some("no");
                        close = true;
                    }
                    if ui.button(crate::i18n::t("Later")).clicked() {
                        decision = Some("later");
                        close = true;
                    }
                });
            });

        if !close {
            return;
        }

        match decision {
            Some("yes") => {
                let is_memstroy = autosave_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.eq_ignore_ascii_case("memstroy"))
                    .unwrap_or(false);
                if is_memstroy {
                    match self.state.load_memstroy(&autosave_path) {
                        Ok(scene) => {
                            self.state.scene = scene;
                            self.state.scene_path = None;
                            self.state.status =
                                crate::i18n::t("\u{2705} Recovered scene loaded.").into();
                        }
                        Err(e) => {
                            self.state.status =
                                format!("{} {e}", crate::i18n::t("\u{274C} Recovery failed:"));
                        }
                    }
                } else {
                    match Scene::load(&autosave_path) {
                        Ok(scene) => {
                            self.state.scene = scene;
                            self.state.scene_path = None;
                            self.state.status =
                                crate::i18n::t("\u{2705} Recovered scene loaded.").into();
                        }
                        Err(e) => {
                            self.state.status =
                                format!("{} {e}", crate::i18n::t("\u{274C} Recovery failed:"));
                        }
                    }
                }
            }
            Some("no") => {
                let _ = std::fs::remove_file(&autosave_path);
                self.state.status = crate::i18n::t("Recovery discarded.").into();
            }
            Some("later") => {
                self.state.status = crate::i18n::t("Recovery postponed.").into();
            }
            _ => {}
        }
        self.state.recovery_dialog_open = false;
        self.state.recovery_pending = None;
    }

    // ─── TITLE TEMPLATES PICKER ──────────────────────────────────────

    /// Modal-style egui Window listing built-in title templates as cards.
    /// Clicking a card adds an overlay at the playhead with a 3-second window.
    fn show_title_picker(&mut self, ctx: &egui::Context) {
        if !self.state.title_picker_open {
            return;
        }

        let mut open = self.state.title_picker_open;
        let playhead = self.state.playhead;
        let scene_dur = self.state.scene.output.duration;
        let mut chosen: Option<usize> = None;

        egui::Window::new(crate::i18n::t("Add Title"))
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .default_height(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(crate::i18n::t("Pick a title template"))
                        .strong()
                        .size(14.0)
                        .color(Color32::WHITE),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(crate::i18n::t(
                        "Adds a 3-second text overlay at the playhead. \
                        Edit text/style afterwards in the Inspector.",
                    ))
                    .size(11.0)
                    .color(Color32::from_rgb(160, 160, 180)),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for (i, tpl) in crate::title_templates::TEMPLATES.iter().enumerate() {
                            let frame = egui::Frame::none()
                                .fill(Color32::from_rgb(36, 34, 22))
                                .rounding(Rounding::same(8.0))
                                .stroke(Stroke::new(1.0, Color32::from_rgb(62, 60, 42)))
                                .inner_margin(egui::Margin::same(8.0));

                            let resp = frame
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(tpl.icon).size(22.0));
                                        ui.vertical(|ui| {
                                            ui.label(RichText::new(tpl.name).strong().size(13.0));
                                            ui.label(
                                                RichText::new(tpl.description)
                                                    .size(10.0)
                                                    .color(Color32::from_rgb(160, 160, 180)),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "\u{201C}{}\u{201D}",
                                                    tpl.default_text
                                                ))
                                                .size(10.0)
                                                .italics()
                                                .color(Color32::from_rgb(200, 200, 220)),
                                            );
                                        });
                                    });
                                })
                                .response;
                            if resp.interact(egui::Sense::click()).clicked() {
                                chosen = Some(i);
                            }
                            ui.add_space(4.0);
                        }
                    });
            });

        self.state.title_picker_open = open;

        if let Some(idx) = chosen {
            let templates = crate::title_templates::TEMPLATES;
            if idx < templates.len() {
                let tpl: &'static crate::title_templates::TitleTemplate = &templates[idx];
                let t_in = playhead;
                let t_out = (t_in + 1.0).min(scene_dur.max(t_in + 0.1)).max(t_in + 0.1);
                let mut new_idx_out: usize = 0;
                self.state.mutate(|scene| {
                    new_idx_out =
                        crate::title_templates::add_template_to_scene(scene, tpl, t_in, t_out);
                });
                self.state.selection = Selection::Overlay(new_idx_out);
                self.state.status =
                    format!("{} {}", crate::i18n::t("\u{2728} Added title:"), tpl.name);
                self.state.title_picker_open = false;
            }
        }
    }
}

pub(crate) fn scene_timeline_end(scene: &Scene) -> f32 {
    let mut max_end = 0.0_f32;
    for actor in &scene.actors {
        max_end = max_end.max(actor.t_out.unwrap_or(scene.output.duration));
    }
    for bg in &scene.backgrounds {
        max_end = max_end.max(bg.start + bg.duration);
    }
    for ov in &scene.overlays {
        let end = match ov {
            memstroy_core::Overlay::Text(t) => t.t_out,
            memstroy_core::Overlay::Image(i) => i.t_out,
            memstroy_core::Overlay::Video(v) => v.t_out,
        };
        max_end = max_end.max(end);
    }
    for audio in &scene.audio {
        if audio.deleted {
            continue;
        }
        max_end = max_end.max(audio.t_out.unwrap_or(scene.output.duration));
    }
    max_end
}

fn library_asset_from_download(
    path: &std::path::Path,
    kind: crate::state::AssetDragKind,
) -> crate::state::LibraryAsset {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("server_asset")
        .to_string();
    let thumbnail = if matches!(
        kind,
        crate::state::AssetDragKind::Image | crate::state::AssetDragKind::Particle
    ) {
        Some(path.to_path_buf())
    } else {
        None
    };
    crate::state::LibraryAsset {
        id: id.clone(),
        path: path.to_path_buf(),
        label: id,
        thumbnail,
        downloaded: true,
        server_id: None,
        duration_secs: None,
        width: None,
        height: None,
    }
}

fn position_last_overlay_at_world(state: &mut EditorState, world: [f32; 2]) {
    let rf = &state.scene.render_frame;
    let [rw, rh] = rf.resolution;
    let world_w = (rw as f32).max(1.0);
    let world_h = (rh as f32).max(1.0);
    if let Some(last) = state.scene.overlays.last_mut() {
        let layout = match last {
            memstroy_core::Overlay::Image(im) => &mut im.layout,
            memstroy_core::Overlay::Video(v) => &mut v.layout,
            memstroy_core::Overlay::Text(t) => &mut t.layout,
        };
        if let Some(kf) = layout.first_mut() {
            kf.value.pos = [
                crate::editor_limits::clamp_pos_norm(world[0] / world_w),
                crate::editor_limits::clamp_pos_norm(world[1] / world_h),
            ];
        }
    }
    let overlay_idx = state.scene.overlays.len().saturating_sub(1);
    crate::canvas_image_search::sync_overlay_canvas_world_center(state, overlay_idx, world);
}

/// Parse ffmpeg time output like "time=00:00:04.00" and return seconds.
fn parse_ffmpeg_time(line: &str) -> Option<f32> {
    let time_prefix = "time=";
    let idx = line.find(time_prefix)?;
    let after = &line[idx + time_prefix.len()..];
    let time_str = after.split_whitespace().next().unwrap_or("");
    // Parse HH:MM:SS.xx format
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 3 {
        let hours: f32 = parts[0].parse().ok()?;
        let minutes: f32 = parts[1].parse().ok()?;
        let seconds: f32 = parts[2].parse().ok()?;
        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    } else {
        None
    }
}

/// Parse ffmpeg frame output like "frame=  120" and return frame number.
fn parse_ffmpeg_frame(line: &str) -> Option<u32> {
    let frame_prefix = "frame=";
    let idx = line.find(frame_prefix)?;
    let after = &line[idx + frame_prefix.len()..];
    let num_str = after.trim_start().split_whitespace().next().unwrap_or("");
    num_str.parse().ok()
}

/// Parse the CPU compositor's stage marker, e.g. `[12.5%] Encoding ...`.
/// Returns the percent (0..100) or `None` when the line isn't a stage.
fn parse_compositor_percent(line: &str) -> Option<f32> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find('%')?;
    let pct_str = &rest[..close];
    pct_str.trim().parse::<f32>().ok()
}

/// Parse the CPU compositor's per-frame marker `frame N/M (PCT%)`.
/// Returns `(N, M)` or `None`.
fn parse_compositor_frame(line: &str) -> Option<(u32, u32)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("frame ")?;
    let mut parts = rest.split_whitespace();
    let nm = parts.next()?;
    let mut nm_iter = nm.split('/');
    let n: u32 = nm_iter.next()?.trim().parse().ok()?;
    let m: u32 = nm_iter.next()?.trim().parse().ok()?;
    Some((n, m))
}

/// Format a wall-clock duration like `0:42`, `2:13`, `1:05:21`. Used
/// by the render-progress window to show elapsed time and ETA without
/// pulling in the `humantime` crate just for this.
fn format_elapsed(secs: f32) -> String {
    let total = secs.max(0.0).floor() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// Counters returned by [`swallow_clipboard_events`] so the caller can
/// tell which kinds of synthetic clipboard events showed up this frame.
/// All three flags are set independently so e.g. Ctrl+X (which becomes
/// `Event::Cut`) only fires the cut path, not the copy + paste paths.
#[derive(Default, Clone, Copy)]
struct ClipboardDrain {
    /// At least one `egui::Event::Copy` was drained.
    copied: bool,
    /// At least one `egui::Event::Cut` was drained.
    cut: bool,
    /// At least one `egui::Event::Paste(_)` was drained. The paste
    /// payload itself is discarded — our paste handler reads the OS
    /// clipboard directly via `arboard` (so it can also recover image
    /// bytes, which `Event::Paste` only carries as text), and falls
    /// back to the in-process `EditorState::clipboard` when the OS
    /// clipboard is empty.
    pasted: bool,
}

/// Remove every `Event::Copy` / `Event::Cut` / `Event::Paste(_)`
/// from egui's input queue for the current frame and report which
/// kinds were present.
///
/// Why this exists: egui rewrites Ctrl+C / Ctrl+X / Ctrl+V into these
/// synthetic events *before* dispatching them to focused widgets. On
/// some platforms (notably Windows in a focused TextEdit) the raw
/// `Event::Key { key: C, modifiers: ctrl, … }` is *not* delivered at
/// all — only `Event::Copy` is. That is why a previous fix that
/// relied solely on `consume_key(.., Key::C)` left some users with
/// silent Ctrl+C/V on the canvas: their TextEdit had focus, egui
/// converted the chord to `Event::Copy`, and our handler never saw
/// the corresponding `Event::Key`.
///
/// Draining the events here both (a) drives our copy/paste handler
/// from the synthetic path, and (b) prevents a focused TextEdit from
/// running its own copy/paste logic on the same physical keypress.
/// The combination means Ctrl+C/V always reaches the editor's clip
/// clipboard regardless of whether a text input is focused.
///
/// **Caller-controlled gating.** When a TextEdit / DragValue is
/// focused the user expects Ctrl+C / Ctrl+V to act on the field's
/// contents — copying selected text, pasting numbers into a value
/// box — not on the canvas selection. The `let_text_widget_handle`
/// flag, set by the shortcut handler when `wants_keyboard_input()`
/// is true, leaves the synthetic events in the queue so the focused
/// widget can process them natively. We still report which kinds
/// were observed so the chord-fallback path stays consistent, but
/// no events are removed.
fn swallow_clipboard_events(ctx: &egui::Context, let_text_widget_handle: bool) -> ClipboardDrain {
    let mut out = ClipboardDrain::default();
    ctx.input_mut(|input| {
        input.events.retain(|ev| match ev {
            egui::Event::Copy => {
                out.copied = true;
                // Keep the event for the focused widget when the
                // caller asked us to defer to it.
                let_text_widget_handle
            }
            egui::Event::Cut => {
                out.cut = true;
                let_text_widget_handle
            }
            egui::Event::Paste(_) => {
                out.pasted = true;
                let_text_widget_handle
            }
            _ => true,
        });
    });
    out
}

impl Drop for App {
    fn drop(&mut self) {
        tracing::info!("App dropping, shutting down tokio runtime immediately...");
        // Take the runtime out of the Option and call shutdown_background
        // This will NOT wait for tasks to complete - they will be aborted immediately
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
            tracing::info!("Tokio runtime shutdown initiated (non-blocking)");
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Apply UI scale from settings ──
        // Capture the native pixels_per_point on the very first frame
        // so we can restore it when the user picks "Auto" (0.0).
        if self.native_ppp.is_none() {
            self.native_ppp = Some(ctx.pixels_per_point());
        }
        let ui_scale = self.state.settings.ui_scale;
        if ui_scale > 0.01 {
            ctx.set_pixels_per_point(ui_scale);
        } else if let Some(native) = self.native_ppp {
            // "Auto" — restore system DPI.
            ctx.set_pixels_per_point(native);
        }

        self.pump_events(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.force_app_close {
            if self.pending_scene_exit.is_none() && self.state.active_tab_is_dirty() {
                self.pending_scene_exit = Some(SceneExitAction::Quit);
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
        }

        self.flush_deferred_clip_placements();
        self.flush_pending_library_reload();
        self.maybe_request_library_reload_repaint(ctx);
        self.poll_frame_extraction();
        self.poll_waveform_extraction();
        // Pick up sinks built on the background audio-load thread (see
        // `audio_engine.rs`). Cheap when there's nothing pending; lets
        // playback start without freezing the UI thread on file decode.
        self.audio_engine.poll_pending();

        // Auto-start frame extraction for actors that have source files
        if !self.state.scene.actors.is_empty() {
            let needs_extraction = self
                .state
                .scene
                .actors
                .iter()
                .enumerate()
                .any(|(i, actor)| {
                    EditorState::is_usable_local_video(&actor.source)
                        && self
                            .state
                            .frame_caches
                            .get(i)
                            .map(|fc| fc.failed || (!fc.is_ready() && !fc.extracting))
                            .unwrap_or(true)
                });
            if needs_extraction {
                self.start_frame_extraction();
            }
        }

        // Auto-start waveform extraction for any audio tracks that don't yet
        // have a ready waveform — fixes "audio doesn't load from waveform"
        // when a track is dropped on the timeline.
        if !self.state.scene.audio.is_empty() {
            let needs_wf = self.state.scene.audio.iter().enumerate().any(|(i, au)| {
                !au.deleted
                    && au.source.exists()
                    && self
                        .state
                        .audio_waveforms
                        .get(i)
                        .is_none_or(|wf| !wf.ready && !wf.extracting && !wf.failed)
            });
            if needs_wf {
                self.start_waveform_extraction();
            }
        }

        if self.state.request_media_preview {
            self.state.request_media_preview = false;
            self.start_frame_extraction();
            self.start_waveform_extraction();
            ctx.request_repaint();
        }

        // The skeleton-editor floating window with its own keyboard
        // transport (Space / Arrow / Home / End) was retired. The
        // inspector section that replaced it shares the main scene's
        // playhead, so no per-window key consumption is needed here.

        // Keyboard shortcuts — capture frame snapshot *before* shortcuts
        // so destructive edits (Delete) get a consistent undo baseline.
        self.state.frame_undo_fallback_suppressed = false;
        let frame_start_scene = self.state.build_undo_snapshot();

        self.handle_shortcuts(ctx);

        // ── Auto-rescan local asset directories ──
        // Cheap mtime-fingerprint poll (debounced to ~2 s) that picks
        // up files dropped into `assets/{images,sounds,videos,
        // particles}/` by an external tool (file manager, screenshot
        // app, OneDrive sync, …). Without this poll the editor was
        // frozen on whatever the library showed at startup and the
        // user had to paste/drag a file inside the editor first to
        // force a `reload_library` — which the user reported as
        // "картинки подгружаются из локального кеша в проект только
        // после добавления первой картинки".
        self.state.auto_rescan_local_library_if_due();

        // ── Frame-level "auto undo snapshot on press" ──
        //
        // Capture the scene the moment any pointer button transitions
        // to pressed-this-frame, so any inspector edit that happens
        // during the resulting gesture (slider drag, checkbox toggle,
        // ComboBox pick, …) can be turned into exactly one undo entry
        // even when it doesn't go through `mutate_drag`. The matching
        // "push or discard" runs at the end of the frame after the UI
        // has had a chance to mutate `state.scene`.
        let pressed_this_frame = ctx.input(|i| i.pointer.any_pressed());
        if pressed_this_frame && self.state.pre_press_scene.is_none() {
            self.state.pre_press_scene = Some(self.state.build_undo_snapshot());
        }

        // Snapshot the scene at the *very* start of every frame so we
        // can fall back to a "scene differs at end of frame, no pointer
        // gesture in flight" undo entry. This catches edits that don't
        // go through a pointer press/release at all — keyboard typing
        // into a `DragValue`, arrow-key nudges, popup menu picks, etc.
        // Without it, those edits would never get an undo snapshot, and
        // the user only sees ONE history entry for an entire session
        // (which manifests as "Ctrl+Z bounces between two states").
        // Note: `frame_start_scene` is captured above, before shortcuts.

        let any_pointer_down = ctx.input(|i| {
            i.pointer.primary_down() || i.pointer.secondary_down() || i.pointer.middle_down()
        });

        // Play/pause: advance playhead
        if self.state.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1); // cap at 100ms
            self.state.playback_speed =
                crate::editor_limits::clamp_playback_speed(self.state.playback_speed);
            self.state.playhead += dt * self.state.playback_speed;

            // Loop preview: clamp playhead within the loop region.
            if self.state.loop_mode {
                if let Some((ls, le)) = self.state.loop_region {
                    let (ls, le) = if ls <= le { (ls, le) } else { (le, ls) };
                    if self.state.playhead > le || self.state.playhead < ls {
                        self.state.playhead = ls;
                    }
                }
            }

            // Wrap when the playhead reaches the end of the longest layer
            // (which `panels::timeline()` keeps in sync with `output.duration`).
            // The loop button now governs this: when loop_mode is OFF the
            // playhead stops at the end (and playback is paused) instead of
            // restarting; when ON it wraps to 0 (or to the loop region start
            // handled above).
            if self.state.playhead >= self.state.scene.output.duration || self.state.playhead < 0.0
            {
                if self.state.loop_mode {
                    self.state.playhead = 0.0;
                } else {
                    self.state.playhead = self.state.scene.output.duration;
                    self.state.playing = false;
                }
            }
            // Repaint at the scene's output FPS (capped) so we don't burn
            // CPU/GPU at 120+ Hz when the output is e.g. 30 fps.
            let target_fps = (self.state.scene.output.fps as f32).max(15.0).min(120.0);
            let dt_target = std::time::Duration::from_secs_f32(1.0 / target_fps);
            ctx.request_repaint_after(dt_target);
        }

        // Auto-preview via ffmpeg has been replaced by the canvas frame
        // caches, which give a much faster preview. We deliberately keep the
        // pipeline disabled here to avoid spawning ffmpeg every time the
        // playhead moves — that was the dominant source of editor lag.

        // Only request continuous repaints while playing. When paused, repaint
        // happens on user input (mouse / keyboard / drag) automatically.
        // (request_repaint_after above handles the scheduling.)

        // Apply modern dark style
        apply_style(ctx);

        // Top menu bar
        egui::TopBottomPanel::top("menu")
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(30, 28, 20))
                    .inner_margin(6.0),
            )
            .show(ctx, |ui| self.menu(ctx, ui));

        // ── Tab bar for multiple scenes ──
        egui::TopBottomPanel::top("tab_bar")
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 21, 14))
                    .inner_margin(egui::Margin {
                        left: 8.0,
                        right: 8.0,
                        top: 4.0,
                        bottom: 0.0,
                    }),
            )
            .exact_height(34.0)
            .show(ctx, |ui| {
                self.scene_tab_bar(ui);
            });

        // Left panel: Library + Refresh button
        egui::SidePanel::left("library")
            .resizable(true)
            .default_width(300.0)
            .width_range(140.0..=560.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(26, 25, 18))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                panels::library(ui, &mut self.state, || {
                    // This closure doesn't have access to self, so we use a flag
                });

                // Refresh button at top of library
            });

        // Check if refresh was requested via flag
        if self.state.status == "__REFRESH_REQUESTED__" {
            self.state.status = String::new();
            self.run_refresh();
        }
        if self.state.status == "__DELETE_SELECTED__" {
            self.state.status = String::new();
            self.delete_selected();
        }
        if self.state.status == "__DUPLICATE_SELECTED__" {
            self.state.status = String::new();
            self.duplicate_selected();
        }
        if self.state.status == "__SPLIT_AT_PLAYHEAD__" {
            self.state.status = String::new();
            self.split_at_playhead(None);
        }
        if self.state.status == "__MERGE_NEXT__" {
            self.state.status = String::new();
            self.merge_next();
        }

        // Handle frame extraction request
        if self.state.status == "__EXTRACT_FRAMES__" {
            self.state.status = String::new();
            self.start_frame_extraction();
            self.start_waveform_extraction();
        }

        // ── OS Drag-and-Drop: accept files from Windows Explorer ──
        // Every dropped file is ALWAYS copied into the matching local
        // library subfolder (assets/videos / images / sounds /
        // particles), so the user can re-use it without re-importing.
        // If the drop landed OUTSIDE the library panel, the resulting
        // copied file is also added to the scene at the playhead — so
        // the user gets the legacy "drop on the canvas" behaviour AND
        // a permanent library entry from the same gesture.
        let dropped_files: Vec<_> = ctx.input(|i| i.raw.dropped_files.clone());
        // Use `latest_pos` with `hover_pos` fallback. egui can drop the
        // hover position to None on the same frame the OS drop event
        // arrives, which would mis-route the file to the canvas. The
        // latest_pos snapshot survives the gap.
        let drop_pointer = ctx.input(|i| i.pointer.latest_pos().or_else(|| i.pointer.hover_pos()));
        let lib_rect = self.state.library_panel_rect;
        let pointer_in_library = match (drop_pointer, lib_rect) {
            (Some(p), Some(r)) => r.contains(p),
            _ => false,
        };
        if !dropped_files.is_empty() {
            for file in &dropped_files {
                let Some(path) = &file.path else {
                    continue;
                };
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let is_video = ["mp4", "mov", "webm", "avi", "mkv", "m4v"].contains(&ext.as_str());
                let is_image = ["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str());
                let is_audio =
                    ["mp3", "wav", "ogg", "flac", "aac", "m4a", "opus"].contains(&ext.as_str());
                if !(is_video || is_image || is_audio) {
                    continue;
                }

                // ── Step 1: copy the file into the local library. ──
                let dest_dir = if is_video {
                    self.state.videos_dir()
                } else if is_image {
                    // Default to Images. Particles share the same
                    // file extensions but live in a separate folder;
                    // route to Particles only when that tab is
                    // currently visible AND the drop landed on the
                    // library panel — otherwise images dropped on the
                    // canvas always go into Images.
                    if pointer_in_library
                        && self.state.library_tab == crate::state::LibraryTab::Particles
                    {
                        self.state.particles_dir()
                    } else {
                        self.state.images_dir()
                    }
                } else {
                    self.state.sounds_dir()
                };
                if let Err(err) = std::fs::create_dir_all(&dest_dir) {
                    self.state.status = format!(
                        "{} {}: {}",
                        crate::i18n::t("Couldn't create"),
                        dest_dir.display(),
                        err
                    );
                    continue;
                }
                let file_name = path
                    .file_name()
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| std::ffi::OsString::from("import"));
                let mut dest = dest_dir.join(&file_name);
                // Avoid clobbering an existing file with the same name
                // — append a numeric suffix until we find a free slot.
                // If a file with identical bytes already exists in the
                // library we re-use it instead of duplicating.
                if dest.exists() {
                    let same_bytes = match (std::fs::read(path), std::fs::read(&dest)) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => false,
                    };
                    if !same_bytes {
                        let mut suffix = 1;
                        while dest.exists() {
                            let stem = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("import");
                            let new_name = format!("{}_{}.{}", stem, suffix, ext);
                            dest = dest_dir.join(new_name);
                            suffix += 1;
                            if suffix > 1000 {
                                break;
                            }
                        }
                    }
                }
                let copied_ok = if dest.exists() {
                    // Same file already present — skip the copy but
                    // still treat the operation as successful.
                    true
                } else {
                    match std::fs::copy(path, &dest) {
                        Ok(_) => true,
                        Err(err) => {
                            self.state.status = format!(
                                "{} {}: {}",
                                crate::i18n::t("Couldn't import"),
                                path.display(),
                                err
                            );
                            false
                        }
                    }
                };
                if !copied_ok {
                    continue;
                }

                // Refresh the library through the same background path
                // used by external file drops. That path generates
                // missing video thumbnails before the scan, so OS-dropped
                // videos get preview images just like files copied into
                // assets/videos directly.
                self.schedule_library_reload();
                if pointer_in_library {
                    self.state.library_tab = if is_video {
                        crate::state::LibraryTab::Videos
                    } else if is_image
                        && self.state.library_tab != crate::state::LibraryTab::Particles
                    {
                        crate::state::LibraryTab::Images
                    } else if is_audio {
                        crate::state::LibraryTab::Sounds
                    } else {
                        self.state.library_tab
                    };
                    self.state.status = format!(
                        "{} {}",
                        crate::i18n::t("Imported into library:"),
                        dest.display()
                    );
                    // Drop landed inside the library panel — no scene
                    // add, the user only wants the asset registered.
                    continue;
                }

                // ── Step 2: drop landed on canvas / timeline → also
                //    add the (now-library-resident) copy to the scene
                //    on its own brand-new / first-empty layer.
                if is_video {
                    // User-imported videos are regular video elements:
                    // they get audio/chroma sidecars, but not Mellstroy
                    // footage sequence controls by default.
                    crate::panels::add_actor_from_video_asset(&mut self.state, &dest);
                } else if is_image {
                    let t_in = self.state.playhead;
                    let t_out = (t_in + 1.0)
                        .min(self.state.scene.output.duration.max(t_in + 0.1))
                        .max(t_in + 0.1);
                    let id_base = dest
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| format!("img_{s}"))
                        .unwrap_or_else(|| format!("img_{}", self.state.scene.overlays.len() + 1));
                    let source = dest.clone();
                    let mut placed_id = id_base.clone();
                    self.state.mutate_state(|s| {
                        placed_id = crate::state::unique_overlay_id(&s.scene.overlays, &id_base);
                        let lane = s.pick_or_create_empty_video_lane_for_range(t_in, t_out);
                        let new_idx = s.scene.overlays.len();
                        s.scene.overlays.push(memstroy_core::Overlay::Image(
                            memstroy_core::ImageOverlay {
                                id: placed_id.clone(),
                                source: source.clone(),
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
                                z_order: 0,
                                parent_id: None,
                            },
                        ));
                        s.overlay_track_assignments.insert(new_idx, lane);
                        s.selection = Selection::Overlay(new_idx);
                    });
                    self.state.status = format!(
                        "{} {} ({})",
                        crate::i18n::t("Dropped image:"),
                        placed_id,
                        crate::i18n::t("saved to library")
                    );
                } else if is_audio {
                    let id = dest
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("audio_{}", self.state.scene.audio.len() + 1));
                    self.state.scene.audio.push(memstroy_core::AudioTrack {
                        id: id.clone(),
                        source: dest.clone(),
                        t_in: self.state.playhead,
                        ..Default::default()
                    });
                    self.state.selection = Selection::Audio(self.state.scene.audio.len() - 1);
                    self.state.status = format!(
                        "{} {} ({})",
                        crate::i18n::t("Dropped audio:"),
                        id,
                        crate::i18n::t("saved to library")
                    );
                }
            }
        }

        // Handle eyedropper activation
        if self.state.status == "__EYEDROPPER_ON__" {
            self.state.status = String::new();
            self.state.eyedropper_active = true;
        }

        // Pose detection used to be triggered from the (now-removed) clip
        // editor's "Detect Pose" button. The status sentinel and runner
        // are gone; if the feature returns it should land as a button on
        // the actor inspector instead.

        // ── Audio engine synchronisation ──
        // Build the list of audio sources currently scheduled in the scene:
        //   - explicit AudioTrack entries (state.scene.audio)
        //   - actor video clips (their embedded audio streams) — but only
        //     when no AudioTrack already references the same source path,
        //     because otherwise we'd hear the clip's audio twice.
        // The engine ignores anything without a decodable audio stream, so
        // including every actor unconditionally is safe.
        let build_sources = |state: &EditorState| -> Vec<crate::audio_engine::AudioSourceSpec> {
            let mut out = Vec::new();
            let resolve_audio_path = |path: &std::path::PathBuf| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    state.assets_root.join(path)
                }
            };
            let explicit_actor_audio: std::collections::HashSet<usize> = state
                .scene
                .audio
                .iter()
                .enumerate()
                .filter(|(_, a)| !a.deleted)
                .filter_map(|(idx, _)| infer_actor_for_audio_in_scene(&state.scene, idx))
                .collect();
            for a in &state.scene.audio {
                if a.deleted {
                    continue;
                }
                // Sample every animatable field at the playhead's
                // clip-local time so a freshly-built sink reflects the
                // user's animated values at the current moment. The
                // engine then plays back those static values; live
                // mid-stream updates happen by detecting a change in
                // `signature()` between frames and rebuilding.
                let t_local = (state.playhead - a.t_in).max(0.0);
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: resolve_audio_path(&a.source),
                    t_in: a.t_in,
                    t_out: a.t_out,
                    source_start: a.source_start,
                    volume: a.volume_at(t_local),
                    speed: a.speed_at(t_local),
                    pitch_semitones: a.pitch_at(t_local),
                    pan: a.pan_at(t_local),
                    low_pass_hz: a.low_pass_at(t_local),
                    high_pass_hz: a.high_pass_at(t_local),
                    fade_in: a.fade_in,
                    fade_out: a.fade_out,
                    mute: a.mute,
                    reverb: a.reverb_at(t_local),
                });
            }
            for (actor_idx, actor) in state.scene.actors.iter().enumerate() {
                if !actor.visible {
                    continue;
                }
                // Honour explicit user intent: if the actor's embedded audio
                // was muted (e.g. because the user deleted/unlinked+deleted
                // its audio row), do not auto-mix the fallback source.
                if actor.mute_audio {
                    continue;
                }
                // An explicit row bound to this actor is the source of
                // truth for that clip's audio, including mute/volume and
                // split windows. Do not dedupe by source path: after a
                // split or copy, multiple actor clips can legitimately
                // use the same file and must each schedule their own
                // audio window.
                if explicit_actor_audio.contains(&actor_idx) {
                    continue;
                }
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: resolve_audio_path(&actor.source),
                    t_in: actor.t_in.unwrap_or(0.0),
                    t_out: actor.t_out,
                    source_start: actor.source_start,
                    volume: 1.0,
                    speed: actor.speed,
                    ..Default::default()
                });
            }
            out
        };

        // Cheap whole-spec-list signature: XOR of every spec's signature
        // (order-independent, hash-friendly). Differs whenever any
        // animatable field changes, so the live-update branch below
        // restarts the sinks the instant the user moves a slider.
        let signature_of = |specs: &[crate::audio_engine::AudioSourceSpec]| -> u64 {
            let mut h: u64 = specs.len() as u64;
            for s in specs {
                h = h.wrapping_mul(0x100000001b3).wrapping_add(s.signature());
            }
            h
        };

        // Detect a seek (playhead jumped further than a sane frame's worth).
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let expected_step = if self.state.playing {
            dt * self.state.playback_speed.abs()
        } else {
            0.0
        };
        let actual_delta = self.state.playhead - self.prev_playhead;
        let seeked = (actual_delta - expected_step).abs() > 0.15 || actual_delta < -0.05;

        if self.state.playing && !self.was_playing {
            // Transition: paused → playing. Start playback at the current playhead.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.prev_audio_signature = signature_of(&sources);
            self.audio_engine
                .play_sources(&sources, self.state.playhead);
        } else if !self.state.playing && self.was_playing {
            // Transition: playing → paused.
            self.audio_engine.pause();
        } else if self.state.playing && seeked {
            // Seek while playing — restart from the new position so audio stays in sync.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.prev_audio_signature = signature_of(&sources);
            self.audio_engine
                .play_sources(&sources, self.state.playhead);
        } else if self.state.playing {
            // Live mid-playback rebuild: any time a spec field changes
            // (slider edits, kf track edits, mute toggle, …) the new
            // signature differs from the previous one, so we re-fire
            // play_sources at the current playhead. This is the
            // simplest path to "audio param changes apply in real time"
            // without a full DSP-control plumb-through; the rebuild is
            // ~tens of ms because the sinks are constructed on a worker
            // thread.
            let sources = build_sources(&self.state);
            let new_sig = signature_of(&sources);
            let count_changed = sources.len() != self.prev_audio_source_count;
            let params_changed = new_sig != self.prev_audio_signature;
            if count_changed || params_changed {
                self.prev_audio_source_count = sources.len();
                self.prev_audio_signature = new_sig;
                self.audio_engine
                    .play_sources(&sources, self.state.playhead);
            }
        }

        self.was_playing = self.state.playing;
        self.prev_playhead = self.state.playhead;

        // Right panel: Inspector
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(350.0)
            .width_range(180.0..=620.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(26, 25, 18))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                panels::inspector(ui, &mut self.state);
            });

        // Bottom panel: Timeline
        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(true)
            .default_height(280.0)
            .height_range(140.0..=720.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 21, 12))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                panels::timeline(ui, &mut self.state);
            });

        // Timeline razor/split: drain after the timeline panel has handled
        // the click so the cut uses the click position and the exact clicked
        // element (not whatever was previously selected).
        if let Some((sel, cut_t)) = self.state.pending_timeline_split.take() {
            self.state.selection = sel;
            self.split_at_playhead(Some(cut_t));
        }

        // Central panel: Preview
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(18, 17, 12))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                crate::canvas_preview::canvas_preview(ui, &mut self.state);
            });

        // Node editor was removed.

        // Curve editor floating window
        if self.state.curve_editor_open {
            let mut curve_open = self.state.curve_editor_open;
            // Free-floating: only seed a default position the FIRST
            // time the window opens (`default_pos`); after that the
            // user can drag it anywhere on screen and egui remembers
            // the position across frames. Earlier we used `.anchor(
            // LEFT_BOTTOM, …)` which pinned the window to the
            // bottom-left corner every frame — exactly the user's
            // "окно… нужно иметь возможность свободно двигать"
            // report.
            egui::Window::new(crate::i18n::t("Curve Editor"))
                .open(&mut curve_open)
                .default_pos(egui::pos2(20.0, 400.0))
                .default_size([600.0, 240.0])
                .min_height(120.0)
                .max_height(400.0)
                .resizable(true)
                .collapsible(true)
                .movable(true)
                .show(ctx, |ui| {
                    ui.set_max_height(380.0);
                    self.draw_curve_editor_body(ui);
                });
            self.state.curve_editor_open = curve_open;
        }

        // The standalone "Skeleton Editor" floating window was retired —
        // every piece of skeleton authoring is now embedded into the
        // inspector for any video-layer element, and points are placed
        // by dragging directly on the main canvas.

        // Title-templates picker (popup grid of preset captions)
        self.show_title_picker(ctx);

        // Auto-save tick + recovery modal
        self.tick_autosave();
        self.show_unsaved_changes_dialog(ctx);
        self.show_recovery_dialog(ctx);

        // Settings dialog (File > Settings...). Always called; the
        // function early-returns when `state.settings_open` is false.
        crate::settings::show_settings_dialog(ctx, &mut self.state, &mut self.audio_engine);

        // Web image search floating window. Self-contained; the
        // function early-returns when the toggle is off so this
        // call is cheap on every frame.
        if self.state.web_image_search_open {
            crate::web_image_search::show_window(ctx, &mut self.state, &self.tx);
        }

        // Render-progress floating window. Surfaces a big, clearly-
        // visible progress bar with elapsed time, ffmpeg's most recent
        // status line and an explicit done/failed state, so the user
        // never has to hunt for the tiny menu-bar bar (which was the
        // user's complaint — "должен быть виден прогресс рендера").
        // Self-closes when `render_progress` is None or after the user
        // dismisses a finished/failed render.
        self.show_render_progress_window(ctx);

        // Repaint scheduling:
        // - When playing with ready frame cache: 16ms (~60fps)
        // - When playing without frame cache: 33ms (~30fps)
        // - When idle/paused: only repaint if jobs are running (reactive mode)
        if self.state.playing {
            if self.state.frame_caches.iter().any(|fc| fc.is_ready()) {
                // Adaptive frame rate: when many actors are playing
                // simultaneously, reduce the repaint cadence to 30fps
                // (33ms) instead of 60fps (16ms). This halves the
                // number of texture uploads and per-pixel effect passes
                // per second, which is the dominant cost with 6+ actors.
                // The visual difference between 30fps and 60fps preview
                // is negligible for editing purposes.
                let ready_count = self
                    .state
                    .frame_caches
                    .iter()
                    .filter(|fc| fc.is_ready())
                    .count();
                let interval_ms = if ready_count >= 5 { 33 } else { 16 };
                ctx.request_repaint_after(std::time::Duration::from_millis(interval_ms));
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
            }
        } else if self.state.refreshing
            || self.state.render_progress.as_ref().is_some_and(|p| !p.done)
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // When idle/paused with no jobs: don't request repaint (reactive mode)

        // ── Frame-level "auto undo snapshot on release" ──
        //
        // Counterpart to the press-snapshot at the top of update(). On
        // pointer release we either:
        //   * commit the snapshot to the undo stack (one entry per
        //     gesture) when the gesture actually mutated the scene AND
        //     no `mutate_drag` token fired (so we don't double-push for
        //     canvas / timeline drags), OR
        //   * discard the snapshot when the gesture had no effect on
        //     the scene (a click that didn't change anything).
        //
        // `last_drag_group.is_some()` is the signal that `mutate_drag`
        // has already pushed an undo entry for this gesture. It's
        // cleared at the top of update() when no pointer button is
        // down, so by the time we get here on a release frame the
        // value still reflects whether the JUST-ENDED gesture used a
        // drag-token. Comparison uses serde_yaml (cheap on small
        // scenes) — derived `PartialEq` would be ideal but the deep
        // recursive Float fields make that fragile.
        let released_this_frame = ctx.input(|i| i.pointer.any_released());
        let any_pointer_still_down = ctx.input(|i| {
            i.pointer.primary_down() || i.pointer.secondary_down() || i.pointer.middle_down()
        });
        let mut release_block_handled_undo = false;
        if released_this_frame && !any_pointer_still_down {
            if let Some(pre) = self.state.pre_press_scene.take() {
                let mutate_drag_handled = self.state.last_drag_group.is_some();
                release_block_handled_undo = true;
                if !mutate_drag_handled {
                    let pre_yaml = serde_yaml::to_string(&pre.scene).unwrap_or_default();
                    let cur_yaml = serde_yaml::to_string(&self.state.scene).unwrap_or_default();
                    let assignments_changed = pre.actor_track_assignments
                        != self.state.actor_track_assignments
                        || pre.overlay_track_assignments != self.state.overlay_track_assignments
                        || pre.audio_track_assignments != self.state.audio_track_assignments;
                    if pre_yaml != cur_yaml || assignments_changed {
                        self.state.undo.push_full(pre);
                    }
                }
            }
        }

        // ── Frame-level fallback: catch edits that bypass pointer ──
        //
        // The press/release path above only fires when the user actually
        // released a mouse button this frame. Edits driven purely by
        // the keyboard (typing into a DragValue, arrow-key nudges,
        // ComboBox keyboard navigation, …) can mutate `state.scene`
        // without ever touching `pre_press_scene`, which is what made
        // Ctrl+Z bounce between just the two states the press/release
        // mechanism happened to capture.
        //
        // To restore granular undo, compare the scene captured at the
        // very start of this frame to the scene at the end. When the
        // user is NOT in the middle of a pointer gesture AND no drag
        // group is in flight AND no release fired this frame (so the
        // press/release path didn't already handle it), push the start-
        // of-frame snapshot as a fresh undo entry whenever the scene
        // actually changed.
        if !any_pointer_still_down
            && self.state.last_drag_group.is_none()
            && !release_block_handled_undo
            && self.state.pre_press_scene.is_none()
            && !self.state.frame_undo_fallback_suppressed
        {
            let pre_yaml = serde_yaml::to_string(&frame_start_scene.scene).unwrap_or_default();
            let cur_yaml = serde_yaml::to_string(&self.state.scene).unwrap_or_default();
            let assignments_changed = frame_start_scene.actor_track_assignments
                != self.state.actor_track_assignments
                || frame_start_scene.overlay_track_assignments
                    != self.state.overlay_track_assignments
                || frame_start_scene.audio_track_assignments != self.state.audio_track_assignments;
            if pre_yaml != cur_yaml || assignments_changed {
                self.state.undo.push_full(frame_start_scene);
            }
        }

        // End the drag-undo group only after release / fallback undo ran.
        // Clearing it at the top of the frame (before the UI) made
        // `mutate_drag_handled` always false on pointer-up, so Ctrl+Z
        // pushed a duplicate snapshot for every canvas / timeline drag.
        if !any_pointer_down {
            self.state.end_drag_group();
        }

        // ── Toast notifications ──
        // Show startup toast notification
        if let Some(until) = self.state.startup_toast_until {
            if std::time::Instant::now() < until {
                egui::Window::new("")
                    .title_bar(false)
                    .resizable(false)
                    .collapsible(false)
                    .anchor(egui::Align2::CENTER_TOP, [0.0, 50.0])
                    .frame(
                        egui::Frame::popup(&ctx.style())
                            .fill(Color32::from_rgb(40, 38, 26))
                            .inner_margin(egui::Margin::same(16.0)),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("!").size(18.0));
                            ui.label(
                                RichText::new(
                                    "Следите за обновлениями и жалуйтесь на баги в telegram: ",
                                )
                                .size(13.0)
                                .color(Color32::from_rgb(220, 220, 220)),
                            );
                            ui.hyperlink_to(
                                RichText::new("https://t.me/memstroy_inator")
                                    .size(13.0)
                                    .color(Color32::from_rgb(255, 242, 0)),
                                "https://t.me/memstroy_inator",
                            );
                        });
                    });
            } else {
                self.state.startup_toast_until = None;
            }
        }
    }
}

/// Helper for `delete_selected`: how many entries the canvas
/// multi-selection currently holds. Pulled out so the multi-select
/// short-circuit at the top of `delete_selected` reads cleanly.
fn state_canvas_selection_count(app: &App) -> usize {
    app.state.canvas_selection.len()
}

/// Apply a modern dark theme built around shades of `#fff200`
/// (HSL 57°, 100 %, 50 %). Backgrounds are near-black yellows,
/// interactive widget states climb the yellow tonal scale, and the
/// pure brand colour is reserved for selection / accent strokes so it
/// stays attention-grabbing instead of drowning every panel in saturated
/// yellow.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();

    // Background colors — neutral warm darks (R ≈ G > B by a few
    // points). Reads as "warm grey" rather than the previous cool
    // blue-grey, but stays low-saturation so panels don't fight with
    // foreground accents.
    visuals.panel_fill = Color32::from_rgb(28, 26, 18); // ~10 % L
    visuals.window_fill = Color32::from_rgb(34, 32, 22); // ~12 % L
    visuals.extreme_bg_color = Color32::from_rgb(18, 17, 10); // ~6 % L

    // Widget colors — climb the warm-grey scale, ending on a saturated
    // brand yellow for the pressed/active state so interactions still
    // pop.
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(40, 38, 26);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(48, 46, 30);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(82, 78, 32);
    visuals.widgets.active.bg_fill = Color32::from_rgb(204, 193, 0);

    // Strokes — bright yellow on hover/active gives the button "glow".
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(255, 242, 0));
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(255, 246, 102));

    // Accent colors — the pure #fff200 lives here so selections /
    // links pop against the dark yellow chassis.
    visuals.selection.bg_fill = Color32::from_rgb(204, 193, 0);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(255, 242, 0));
    visuals.hyperlink_color = Color32::from_rgb(255, 246, 102);

    // Rounded corners
    visuals.widgets.noninteractive.rounding = Rounding::same(6.0);
    visuals.widgets.inactive.rounding = Rounding::same(6.0);
    visuals.widgets.hovered.rounding = Rounding::same(6.0);
    visuals.widgets.active.rounding = Rounding::same(6.0);
    visuals.window_rounding = Rounding::same(10.0);

    // Text — neutral white reads cleanly on the deep yellow chassis
    // without dragging the brand colour into prose.
    visuals.override_text_color = Some(Color32::WHITE);

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);

    ctx.set_style(style);
}
