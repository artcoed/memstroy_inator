//! Main eframe application: wires panels together and dispatches jobs.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, ViewportCommand, Rounding, Stroke, Vec2};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::jobs::{spawn_refresh, spawn_render, JobEvent};
use crate::panels;
use crate::state::{EditorState, Selection};
use crate::image_editor;
use crate::audio_engine::AudioEngine;

pub struct App {
    rt: Runtime,
    state: EditorState,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
    /// Per-actor extraction results. Key = actor index.
    frame_extract_results: Vec<Arc<Mutex<Option<(f32, usize, std::path::PathBuf)>>>>,
    /// Per-audio-track waveform extraction results.
    waveform_extract_results: Vec<Arc<Mutex<Option<(Vec<f32>, f32)>>>>,
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
        if crate::build_info::IS_CLIENT_BUILD {
            tracing::info!(
                server_url = %state.server_url,
                "client build: skipping in-process assets-server, using remote"
            );
        } else {
            Self::spawn_local_assets_server(rt.handle(), &state);
        }

        // Construct the audio engine and immediately apply the master
        // volume from the persisted settings. That way the very first
        // playback obeys the user's saved level instead of the engine's
        // default 1.0.
        let mut audio_engine = AudioEngine::new();
        audio_engine.set_master_volume(state.settings.master_volume);

        // Recovery: if an autosave from a previous session exists and is newer
        // than the user's current scene file, surface a recovery dialog.
        let autosave_path = EditorState::autosave_path();
        if autosave_path.exists() {
            let autosave_modified = std::fs::metadata(&autosave_path)
                .and_then(|m| m.modified())
                .ok();
            // If there is no scene file, any autosave is a candidate.
            // If there is one, only show recovery when the autosave is newer.
            let should_offer = match (&state.scene_path, autosave_modified) {
                (None, Some(_)) => true,
                (Some(p), Some(am)) => match std::fs::metadata(p).and_then(|m| m.modified()) {
                    Ok(scene_modified) => am > scene_modified,
                    Err(_) => true,
                },
                _ => true,
            };
            if should_offer {
                state.recovery_pending = Some(autosave_path);
                state.recovery_dialog_open = true;
            }
        }

        Self {
            rt,
            state,
            tx,
            rx,
            frame_extract_results: Vec::new(),
            waveform_extract_results: Vec::new(),
            audio_engine,
            was_playing: false,
            prev_playhead: 0.0,
            prev_audio_signature: 0,
            prev_audio_source_count: 0,
        }
    }

    /// Spin up a `memstroy-assets-server` instance on the same tokio
    /// runtime as the GUI, parsing the address out of `state.server_url`.
    /// Failures (bad URL, port already bound by another instance, etc.)
    /// are logged but do not abort start-up — the GUI's HTTP calls fall
    /// through to whatever (if anything) is already listening on that
    /// port, which keeps developer workflows where the server is run
    /// separately working unchanged.
    fn spawn_local_assets_server(
        handle: &tokio::runtime::Handle,
        state: &EditorState,
    ) {
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
        // surfaces. Existing subdirectories (`assets/mellstroy/`,
        // `assets/sounds/`, ...) are picked up by the walker.
        let root = state.assets_root.join("assets");
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
                        // Parse ffmpeg progress from log lines
                        // Look for "frame= 120" or "time=00:00:04.00"
                        if let Some(time_progress) = parse_ffmpeg_time(&line) {
                            let total = self.state.scene.output.duration;
                            if total > 0.0 {
                                rp.progress = (time_progress / total).clamp(0.0, 1.0);
                            }
                        } else if let Some(frame_num) = parse_ffmpeg_frame(&line) {
                            let total_frames = (self.state.scene.output.duration
                                * self.state.scene.output.fps as f32) as u32;
                            if total_frames > 0 {
                                rp.progress = (frame_num as f32 / total_frames as f32).clamp(0.0, 1.0);
                            }
                        }
                    }
                }
                JobEvent::RenderFinished(Ok(p)) => {
                    self.state.status = format!("\u{2705} Rendered: {}", p.display());
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                    }
                }
                JobEvent::RenderFinished(Err(e)) => {
                    self.state.status = format!("\u{274C} Render failed: {}", e);
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                        rp.error = Some(e);
                    }
                }
                JobEvent::RefreshProgress(msg) => {
                    self.state.status = format!("\u{1F504} {}", msg);
                }
                JobEvent::RefreshLibraryReloaded(msg) => {
                    // Mid-refresh partial reload: the worker has
                    // mirrored a fresh batch of clips to disk, so
                    // re-scan the local library now instead of
                    // waiting for the whole refresh to finish. This
                    // is what restores the live "watch the catalogue
                    // grow" feel during a 400+ clip ingest — without
                    // it, the panel would stay frozen for the entire
                    // multi-minute download.
                    self.state.status = format!("\u{1F504} {}", msg);
                    self.state.reload_library();
                }
                JobEvent::RefreshFinished(Ok(summary)) => {
                    self.state.refreshing = false;
                    self.state.reload_library();
                    self.state.status = format!(
                        "\u{1F389} Refresh done! {} new clips, {} total in library",
                        summary.new_clips, summary.total_clips
                    );
                    if summary.failed > 0 {
                        self.state.status.push_str(&format!(
                            " ({} failed)",
                            summary.failed
                        ));
                    }
                }
                JobEvent::RefreshFinished(Err(e)) => {
                    self.state.refreshing = false;
                    self.state.status = format!("\u{274C} Refresh failed: {}", e);
                }
                JobEvent::WebSearchFinished {
                    page_offset,
                    result,
                } => {
                    self.state.web_image_search.searching = false;
                    match result {
                        Ok((mut hits, next_offset)) => {
                            // Reject responses that come back at the
                            // same start offset we already loaded —
                            // happens when DDG echoes the same `s` in
                            // its `next` field at end-of-results, and
                            // would otherwise grow `results` forever.
                            let n = hits.len();
                            let is_first_page = page_offset == 0;
                            if is_first_page {
                                self.state.web_image_search.results = hits;
                            } else {
                                // Append, but keep the total bounded.
                                let cap = crate::web_image_search::MAX_TOTAL_RESULTS;
                                let cur = self.state.web_image_search.results.len();
                                if cur < cap {
                                    let take = (cap - cur).min(hits.len());
                                    if take < hits.len() {
                                        hits.truncate(take);
                                    }
                                    self.state
                                        .web_image_search
                                        .results
                                        .extend(hits);
                                }
                            }
                            // Advance the cursor only if it differs
                            // from the request's offset; otherwise we
                            // treat it as "no more pages".
                            self.state.web_image_search.next_offset = next_offset
                                .filter(|&o| o != page_offset);
                            if !is_first_page {
                                self.state.web_image_search.page_count += 1;
                            }
                            self.state.web_image_search.status = if n == 0 {
                                if is_first_page {
                                    "\u{1F50D} No results.".to_string()
                                } else {
                                    "(no more results)".to_string()
                                }
                            } else if is_first_page {
                                format!("\u{2705} Got {} result(s).", n)
                            } else {
                                format!(
                                    "\u{2795} +{} (total {})",
                                    n,
                                    self.state.web_image_search.results.len()
                                )
                            };
                        }
                        Err(e) => {
                            if page_offset == 0 {
                                self.state.web_image_search.results.clear();
                            }
                            // On a paged-fetch error keep existing
                            // results but stop offering "Load more"
                            // for this query.
                            self.state.web_image_search.next_offset = None;
                            self.state.web_image_search.status =
                                format!("\u{274C} {}", e);
                        }
                    }
                }
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
                    match result {
                        Ok(asset) => {
                            // Refresh library so the file appears on
                            // the Images tab (matches Ctrl+V flow).
                            self.state.reload_library();
                            self.state.web_image_search.status = format!(
                                "\u{2705} Saved \u{2192} {}",
                                asset.label,
                            );
                            if place_on_canvas {
                                let _idx =
                                    self.state.add_image_overlay_at_playhead(&asset);
                                self.state.library_tab =
                                    crate::state::LibraryTab::Images;
                                self.state.status = format!(
                                    "\u{1F310} Web image \u{2192} {}",
                                    asset.label,
                                );
                            }
                        }
                        Err(e) => {
                            self.state.web_image_search.status =
                                format!("\u{274C} Download failed: {}", e);
                        }
                    }
                }
                JobEvent::ImageFxReady(result) => {
                    self.handle_image_fx_ready(ctx, result);
                }
            }
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
                let texture =
                    ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR);
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
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    for i in 0..num_tabs {
                        let is_active = i == active;
                        let is_editing = editing == Some(i);
                        let tab_name = self.state.scene_tabs[i].name.clone();
                        // "Dirty" = tab has content but no save path yet.
                        let dirty = self
                            .state
                            .scene_tabs
                            .get(i)
                            .map(|t| t.path.is_none() && !t.scene.actors.is_empty())
                            .unwrap_or(false);

                        let (fill, stroke_col, text_col, accent) = if is_active {
                            (
                                Color32::from_rgb(40, 40, 58),
                                Color32::from_rgb(120, 100, 220),
                                Color32::from_rgb(255, 255, 255),
                                Some(Color32::from_rgb(140, 100, 255)),
                            )
                        } else {
                            (
                                Color32::from_rgb(26, 26, 36),
                                Color32::from_rgb(50, 50, 70),
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
                                        egui::TextEdit::singleline(
                                            &mut self.state.editing_tab_buf,
                                        )
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
                                        commit_rename = Some((
                                            i,
                                            self.state.editing_tab_buf.clone(),
                                        ));
                                    }
                                } else {
                                    let label = if dirty {
                                        format!("\u{2022} {}", tab_name)
                                    } else {
                                        tab_name.clone()
                                    };
                                    let resp = ui.add(
                                        egui::Label::new(
                                            RichText::new(label).size(12.0).color(text_col),
                                        )
                                        .selectable(false)
                                        .sense(egui::Sense::click()),
                                    );
                                    if resp.clicked() && !is_active {
                                        switch_to = Some(i);
                                    }
                                    if resp.double_clicked() {
                                        start_rename = Some((i, tab_name.clone()));
                                    }
                                }

                                // Close button is always rendered — closing the
                                // last tab resets it to a fresh "Untitled" so
                                // the user always has a working scene.
                                let close_btn = egui::Button::new(
                                    RichText::new("\u{00D7}")
                                        .size(13.0)
                                        .color(if is_active {
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
                                    close_resp.on_hover_text(crate::i18n::t("Reset to a fresh untitled scene"))
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
                    .color(Color32::from_rgb(160, 220, 160)),
            )
            .fill(Color32::from_rgb(28, 36, 28))
            .rounding(Rounding::same(7.0))
            .stroke(Stroke::new(1.0, Color32::from_rgb(50, 80, 50)))
            .min_size(Vec2::new(26.0, 22.0));
            if ui.add(plus_btn).on_hover_text(crate::i18n::t("New scene tab")).clicked() {
                self.state.new_tab();
            }
        });

        if let Some((idx, name)) = start_rename {
            self.state.editing_tab_idx = Some(idx);
            self.state.editing_tab_buf = name;
        }
        if let Some((idx, new_name)) = commit_rename {
            if idx != usize::MAX
                && idx < self.state.scene_tabs.len()
                && !new_name.trim().is_empty()
            {
                self.state.scene_tabs[idx].name = new_name.trim().to_string();
            }
            self.state.editing_tab_idx = None;
            self.state.editing_tab_buf.clear();
        }
        if let Some(idx) = switch_to {
            self.state.switch_tab(idx);
        }
        if let Some(idx) = close_tab {
            self.state.close_tab(idx);
        }
    }

    fn menu(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        use crate::i18n::t;
        egui::menu::bar(ui, |ui| {
            ui.menu_button(RichText::new(t("\u{1F4C1} File")).strong(), |ui| {
                if ui.button(t("\u{2728} New scene")).clicked() {
                    self.state.scene = Scene::default();
                    self.state.scene_path = None;
                    self.state.status = t("\u{2728} New scene created.").into();
                    ui.close_menu();
                }
                if ui.button(t("\u{1F4C2} Open scene...")).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Memstroy Project", &["memstroy"])
                        .add_filter("Scene", &["yaml", "yml", "json"])
                        .pick_file()
                    {
                        let is_memstroy = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.eq_ignore_ascii_case("memstroy"))
                            .unwrap_or(false);
                        let load_res: Result<Scene, String> = if is_memstroy {
                            self.state.load_memstroy(&path)
                        } else {
                            Scene::load(&path).map_err(|e| e.to_string())
                        };
                        match load_res {
                            Ok(s) => {
                                self.state.scene = s;
                                self.state.scene_path = Some(path.clone());
                                self.state.status = t("\u{2705} Scene loaded.").into();
                                // Sidecar layout for non-bundle formats.
                                if !is_memstroy {
                                    let layout_path = path.with_extension("layout.json");
                                    self.state.load_layout(&layout_path);
                                }
                                // Update tab name
                                let name = path.file_stem().and_then(|s| s.to_str())
                                    .unwrap_or("Scene").to_string();
                                if self.state.active_tab < self.state.scene_tabs.len() {
                                    self.state.scene_tabs[self.state.active_tab].name = name;
                                    self.state.scene_tabs[self.state.active_tab].path = Some(path.clone());
                                    self.state.scene_tabs[self.state.active_tab].scene = self.state.scene.clone();
                                }
                            }
                            Err(e) => self.state.status = format!("\u{274C} Open failed: {e}"),
                        }
                    }
                    ui.close_menu();
                }
                if ui.button(t("\u{1F4BE} Save scene")).clicked() {
                    self.save_scene();
                    ui.close_menu();
                }
                if ui.button(t("\u{1F4BE} Save scene as...")).clicked() {
                    self.save_as();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(t("\u{2699} Settings...")).clicked() {
                    self.state.settings_open = true;
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(t("\u{1F6AA} Exit")).clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });

            ui.menu_button(RichText::new(t("\u{1F3AC} Render")).strong(), |ui| {
                if ui.button(t("\u{1F3A5} Render full clip...")).clicked() {
                    self.run_render();
                    ui.close_menu();
                }
            });

            // ── View menu ─────────────────────────────────────────
            // Single home for every floating-window toggle in the
            // editor. Adding new floating windows should only need a
            // checkbox here.
            ui.menu_button(RichText::new(t("\u{1F441} View")).strong(), |ui| {
                ui.checkbox(
                    &mut self.state.web_image_search_open,
                    t("\u{1F310} Web Image Search"),
                );
                ui.checkbox(
                    &mut self.state.curve_editor_open,
                    t("\u{1F4C8} Curve Editor"),
                );
                ui.checkbox(
                    &mut self.state.image_editor_open,
                    t("\u{1F5BC} Image Editor"),
                );
                ui.checkbox(
                    &mut self.state.skeleton_editor.open,
                    t("\u{2692} Skeleton Editor"),
                );
            });

            // Status indicator on the right
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.state.refreshing {
                    ui.spinner();
                    ui.label(RichText::new(t("refreshing...")).color(Color32::from_rgb(255, 200, 50)).size(11.0));
                } else if let Some(rp) = &self.state.render_progress {
                    if !rp.done {
                        ui.add(egui::ProgressBar::new(rp.progress).text(format!("{:.0}%", rp.progress * 100.0)));
                    }
                }
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
        // 2. **Modifier-based shortcuts** (Ctrl+Z/Y/D/C/V) fire
        //    UNCONDITIONALLY via `consume_key`. This is the "deep
        //    fix" the user asked for: previously the whole handler
        //    bailed on `wants_keyboard_input`, which meant Ctrl+C and
        //    Ctrl+V silently no-op'd whenever any TextEdit retained
        //    focus (e.g. after typing in the title overlay's caption,
        //    even if focus subsequently moved to the canvas). The
        //    `consume_key` API also stops the focused text widget
        //    from running its own copy/paste logic for the same key,
        //    so we don't get a double dispatch.
        let typing = ctx.wants_keyboard_input();

        let modifiers = ctx.input(|i| i.modifiers);
        let ctrl = modifiers.ctrl || modifiers.mac_cmd;

        // Modifier-based shortcuts — always run.
        if ctrl {
            // Ctrl+Z = Undo (NOT Shift+Z, which is redo).
            if !modifiers.shift && ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)
            }) {
                self.state.undo();
            }
            // Ctrl+Shift+Z or Ctrl+Y = Redo
            let redo_z = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z)
            });
            let redo_y = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)
            });
            if redo_z || redo_y {
                self.state.redo();
            }
            // Ctrl+D = duplicate
            if ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)
            }) {
                self.duplicate_selected();
            }
            // Ctrl+C = copy current selection
            if ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::C)
            }) {
                let n = self.state.copy_selection_to_clipboard();
                if n > 0 {
                    self.state.status = format!(
                        "\u{1F4CB} Copied {} item{} to clipboard",
                        n,
                        if n == 1 { "" } else { "s" }
                    );
                }
            }
            // Ctrl+V = paste
            if ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::V)
            }) {
                let pasted_image = self.try_paste_image_from_system_clipboard();
                if !pasted_image {
                    let n = self.state.paste_clipboard();
                    if n > 0 {
                        self.state.status = format!(
                            "\u{1F4CB} Pasted {} item{} at the playhead",
                            n,
                            if n == 1 { "" } else { "s" },
                        );
                    } else {
                        self.state.status =
                            "\u{1F4CB} Clipboard is empty".into();
                    }
                }
            }
        }

        // Plain-key shortcuts — gated by typing focus.
        if typing {
            return;
        }

        ctx.input(|i| {
            // Space = Play/Pause
            if i.key_pressed(egui::Key::Space) {
                self.state.playing = !self.state.playing;
                if self.state.playing {
                    self.state.status = "\u{25B6} Playing".into();
                } else {
                    self.state.status = "\u{23F8} Paused".into();
                }
            }
            // Delete key = remove selected element (only when no
            // modifier — Ctrl+Delete is reserved for future use).
            if !ctrl
                && (i.key_pressed(egui::Key::Delete)
                    || i.key_pressed(egui::Key::Backspace))
            {
                self.delete_selected();
            }
            // Escape clears the canvas multi-selection so the user can
            // exit a marquee paint without affecting other shortcuts.
            // It also disarms any mask / crop tool so the user can fall
            // back to the default transform mode without hunting for
            // the toolbar button.
            if i.key_pressed(egui::Key::Escape) {
                if !self.state.canvas_selection.is_empty() {
                    self.state.canvas_selection.clear();
                }
                if self.state.mask_tool != crate::state::MaskTool::None {
                    self.state.mask_tool = crate::state::MaskTool::None;
                    self.state.mask_draft_points.clear();
                    self.state.status = "Mask tool cancelled".into();
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
                self.state.status = format!("Clipboard image save failed: {}", err);
                return true; // we tried; don't fall through to internal paste
            }
        };
        let _idx = self.state.add_image_overlay_at_playhead(&asset);
        self.state.library_tab = crate::state::LibraryTab::Images;
        self.state.status = format!(
            "\u{1F4CB} Pasted clipboard image \u{2192} {} ({}\u{00D7}{})",
            asset.label, width, height
        );
        true
    }

    /// Render the body of the curve-editor floating window. Resolves
    /// the active curve target (Actor / Overlay / Audio) and shows a
    /// dropdown picker when the user has multiple compatible elements
    /// multi-selected so they can choose which one's curve to edit.
    fn draw_curve_editor_body(&mut self, ui: &mut egui::Ui) {
        use crate::curve_editor::{
            curve_editor_panel, CurveEditorTarget, PROP_OPACITY, PROP_POS_X,
            PROP_POS_Y, PROP_ROTATION, PROP_SCALE,
        };
        use crate::i18n::t;

        // Build the list of candidate targets the curve editor can bind
        // to right now. Each entry has a short label, the resolved
        // selection it points at, and a kind tag the renderer uses to
        // route into the right CurveEditorTarget variant.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum CandidateKind {
            Actor,
            Overlay,
            Audio,
        }
        let mut candidates: Vec<(String, Selection, CandidateKind)> = Vec::new();
        // Single primary selection always comes first so it stays the
        // default when the popup opens.
        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let id = self.state.scene.actors[i].id.clone();
                candidates.push((
                    format!("{} {}", t("Actor"), id),
                    Selection::Actor(i),
                    CandidateKind::Actor,
                ));
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let id = match &self.state.scene.overlays[i] {
                    memstroy_core::Overlay::Text(o) => o.id.clone(),
                    memstroy_core::Overlay::Image(o) => o.id.clone(),
                    memstroy_core::Overlay::Video(o) => o.id.clone(),
                };
                candidates.push((
                    format!("{} {}", t("Overlay"), id),
                    Selection::Overlay(i),
                    CandidateKind::Overlay,
                ));
            }
            Selection::Audio(i) if i < self.state.scene.audio.len() => {
                let id = self.state.scene.audio[i].id.clone();
                candidates.push((
                    format!("{} {}", t("Audio"), id),
                    Selection::Audio(i),
                    CandidateKind::Audio,
                ));
            }
            _ => {}
        }
        // Multi-selected actors (Ctrl+click) extend the candidate set so
        // the user can flick between them without leaving the editor.
        for &mi in &self.state.multi_select {
            if mi >= self.state.scene.actors.len() {
                continue;
            }
            let already_in_list = candidates
                .iter()
                .any(|(_, sel, _)| matches!(*sel, Selection::Actor(j) if j == mi));
            if already_in_list {
                continue;
            }
            let id = self.state.scene.actors[mi].id.clone();
            candidates.push((
                format!("{} {}", t("Actor"), id),
                Selection::Actor(mi),
                CandidateKind::Actor,
            ));
        }

        if candidates.is_empty() {
            ui.label(
                egui::RichText::new(t(
                    "Select an actor, overlay or audio layer to edit its curves.",
                ))
                .italics()
                .color(Color32::from_rgb(140, 140, 160)),
            );
            return;
        }

        // ── Element picker (only shown when there's more than one) ──
        let mut chosen_idx = self
            .state
            .curve_editor_active_idx
            .min(candidates.len().saturating_sub(1));
        if candidates.len() > 1 {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(t("Element"))
                        .size(11.0)
                        .color(Color32::from_rgb(160, 160, 180)),
                );
                egui::ComboBox::from_id_source("curve_editor_element_picker")
                    .selected_text(&candidates[chosen_idx].0)
                    .show_ui(ui, |ui| {
                        for (i, (label, _sel, _kind)) in
                            candidates.iter().enumerate()
                        {
                            ui.selectable_value(&mut chosen_idx, i, label);
                        }
                    });
            });
            ui.add_space(2.0);
        }
        self.state.curve_editor_active_idx = chosen_idx;

        let (_label, sel, kind) = candidates[chosen_idx].clone();
        let duration = self.state.scene.output.duration;
        let playhead = self.state.playhead;

        match kind {
            CandidateKind::Actor => {
                if let Selection::Actor(i) = sel {
                    if let Some(a) = self.state.scene.actors.get_mut(i) {
                        let target = CurveEditorTarget::Actor {
                            layout: &mut a.layout,
                            animated_params: &mut a.animated_params,
                        };
                        curve_editor_panel(
                            ui,
                            target,
                            duration,
                            &mut self.state.curve_editor_property,
                            playhead,
                        );
                    }
                }
            }
            CandidateKind::Overlay => {
                if let Selection::Overlay(i) = sel {
                    if let Some(ov) = self.state.scene.overlays.get_mut(i) {
                        let (layout, animated, t_in) = match ov {
                            memstroy_core::Overlay::Text(o) => (
                                &mut o.layout,
                                &mut o.animated_params,
                                o.t_in,
                            ),
                            memstroy_core::Overlay::Image(o) => (
                                &mut o.layout,
                                &mut o.animated_params,
                                o.t_in,
                            ),
                            memstroy_core::Overlay::Video(o) => (
                                &mut o.layout,
                                &mut o.animated_params,
                                o.t_in,
                            ),
                        };
                        let target = CurveEditorTarget::Overlay {
                            layout,
                            animated_params: animated,
                            t_in,
                        };
                        curve_editor_panel(
                            ui,
                            target,
                            duration,
                            &mut self.state.curve_editor_property,
                            playhead,
                        );
                    }
                }
            }
            CandidateKind::Audio => {
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
                        let (kfs, label, color, range, static_v, param_id) =
                            match audio_prop {
                                AUDIO_SPEED => (
                                    &mut audio.speed_kfs,
                                    "Speed",
                                    Color32::from_rgb(255, 180, 80),
                                    (0.05_f32, 16.0_f32),
                                    audio.speed,
                                    "speed",
                                ),
                                AUDIO_PAN => (
                                    &mut audio.pan_kfs,
                                    "Pan",
                                    Color32::from_rgb(180, 220, 255),
                                    (-1.0_f32, 1.0_f32),
                                    audio.pan,
                                    "pan",
                                ),
                                _ => (
                                    &mut audio.volume_kfs,
                                    "Volume",
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
                        );
                    }
                }
            }
        }
    }

    fn delete_selected(&mut self) {
        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let actor_id = self.state.scene.actors[i].id.clone();
                // Bound audio (the AudioTrack created when the clip was
                // dropped) follows the actor on delete so we never leak
                // orphaned audio rows.
                let removed_audio = crate::panels::remove_audio_bound_to_actor(
                    &mut self.state, &actor_id);
                // App-side `waveform_extract_results` mirrors the
                // audio Vec by index. `remove_audio_bound_to_actor`
                // gives us the indices in scene-Vec order so removing
                // back-to-front keeps every later index valid as we
                // splice. (Front-to-back would tear the table.)
                let mut sorted = removed_audio.clone();
                sorted.sort_unstable();
                for ai in sorted.into_iter().rev() {
                    if ai < self.waveform_extract_results.len() {
                        self.waveform_extract_results.remove(ai);
                    }
                }
                self.state.mutate(|s| { s.actors.remove(i); });
                // Keep every side-table that mirrors the actors Vec in
                // lock-step with the new index space. Without the
                // assignments shift, the actor that used to sit at
                // `i+1` keeps its old assignments[i+1] entry which now
                // belongs to a different actor — that's the "layers
                // jump" the user reported when deleting.
                if i < self.state.frame_caches.len() {
                    self.state.frame_caches.remove(i);
                }
                if i < self.frame_extract_results.len() {
                    self.frame_extract_results.remove(i);
                }
                crate::panels::shift_assignments_after_remove(
                    &mut self.state.actor_track_assignments, i);
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = "\u{1F5D1} Actor deleted.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                self.state.mutate(|s| { s.overlays.remove(i); });
                crate::panels::shift_assignments_after_remove(
                    &mut self.state.overlay_track_assignments, i);
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = "\u{1F5D1} Overlay deleted.".into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                self.state.mutate(|s| { s.backgrounds.remove(i); });
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = "\u{1F5D1} Background deleted.".into();
            }
            Selection::Audio(i) if i < self.state.scene.audio.len() => {
                self.state.mutate(|s| { s.audio.remove(i); });
                if i < self.state.audio_waveforms.len() {
                    self.state.audio_waveforms.remove(i);
                }
                if i < self.waveform_extract_results.len() {
                    self.waveform_extract_results.remove(i);
                }
                crate::panels::shift_assignments_after_remove(
                    &mut self.state.audio_track_assignments, i);
                self.state.selection = Selection::None;
                self.state.canvas_selection.clear();
                self.state.multi_select.clear();
                self.state.status = "\u{1F5D1} Audio deleted.".into();
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
                self.state.mutate(move |s| { s.actors.push(dup); });
                self.state.selection = Selection::Actor(new_idx);
                self.state.status = "\u{1F4CB} Actor duplicated.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let mut dup = self.state.scene.overlays[i].clone();
                match &mut dup {
                    memstroy_core::Overlay::Text(t) => t.id = format!("{}_copy", t.id),
                    memstroy_core::Overlay::Image(im) => im.id = format!("{}_copy", im.id),
                    memstroy_core::Overlay::Video(v) => v.id = format!("{}_copy", v.id),
                }
                let new_idx = self.state.scene.overlays.len();
                self.state.mutate(move |s| { s.overlays.push(dup); });
                self.state.selection = Selection::Overlay(new_idx);
                self.state.status = "\u{1F4CB} Overlay duplicated.".into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                let mut dup = self.state.scene.backgrounds[i].clone();
                dup.id = format!("{}_copy", dup.id);
                let new_idx = self.state.scene.backgrounds.len();
                self.state.mutate(move |s| { s.backgrounds.push(dup); });
                self.state.selection = Selection::Background(new_idx);
                self.state.status = "\u{1F4CB} Background duplicated.".into();
            }
            _ => {}
        }
    }

    /// Split the selected element at the current playhead position.
    /// Creates two adjacent elements: [original_start..playhead] and [playhead..original_end].
    fn split_at_playhead(&mut self) {
        let t = self.state.playhead;
        match self.state.selection {
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                let bg = &self.state.scene.backgrounds[i];
                let start = bg.start;
                let end = bg.start + bg.duration;
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this background's range.".into();
                    return;
                }
                let mut right = bg.clone();
                right.id = format!("{}_R", right.id);
                right.start = t;
                right.duration = end - t;
                let left_dur = t - start;
                self.state.mutate(move |s| {
                    s.backgrounds[i].duration = left_dur;
                    s.backgrounds.insert(i + 1, right);
                });
                self.state.status = "\u{2702} Background split at playhead.".into();
            }
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let a = &self.state.scene.actors[i];
                let start = a.t_in.unwrap_or(0.0);
                let end = a.t_out.unwrap_or(self.state.scene.output.duration);
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this actor's range.".into();
                    return;
                }
                let mut right = a.clone();
                right.id = format!("{}_R", right.id);
                right.t_in = Some(t);
                right.t_out = Some(end);
                // Correct source_start for the right half
                right.source_start = a.source_start + (t - start);
                // Keep only keyframes in each half (by LOCAL time relative to actor start).
                let local_split = t - start;
                // Right half: keep keyframes at or after the split point, shift them to start from 0
                right.layout.retain(|kf| kf.t >= local_split);
                for kf in right.layout.iter_mut() {
                    kf.t -= local_split;
                }
                // If right half has no keyframes, add one at t=0 with the last known state
                if right.layout.is_empty() {
                    let last_state = a.layout.last().map(|k| k.value).unwrap_or_default();
                    right.layout.push(memstroy_core::Keyframe::new(0.0, last_state));
                }
                let local_split_for_left = local_split;
                // Capture the original lane so the right half stays on the
                // same track (otherwise the new index falls back to "first
                // video lane" and the user perceives it as a layer jump).
                let original_lane = self.state.actor_track_assignments.get(&i).copied();
                self.state.mutate(move |s| {
                    s.actors[i].t_out = Some(t);
                    // Left half: keep keyframes at or before the split point
                    s.actors[i].layout.retain(|kf| kf.t <= local_split_for_left);
                    // If left half has no keyframes, add one at t=0 with default state
                    if s.actors[i].layout.is_empty() {
                        s.actors[i].layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::ActorState::default()));
                    }
                    s.actors.insert(i + 1, right);
                });
                // Shift every actor_track_assignments entry >= i+1 by +1 to
                // account for the insertion, then bind the new right half
                // to the original lane.
                let pivot = i + 1;
                let mut shifted: std::collections::HashMap<usize, usize> =
                    std::collections::HashMap::with_capacity(
                        self.state.actor_track_assignments.len() + 1,
                    );
                for (k, v) in self.state.actor_track_assignments.iter() {
                    let new_k = if *k >= pivot { *k + 1 } else { *k };
                    shifted.insert(new_k, *v);
                }
                if let Some(lane) = original_lane {
                    shifted.insert(pivot, lane);
                }
                self.state.actor_track_assignments = shifted;
                self.state.status = "\u{2702} Actor split at playhead.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let ov = &self.state.scene.overlays[i];
                let (start, end) = match ov {
                    memstroy_core::Overlay::Text(txt) => (txt.t_in, txt.t_out),
                    memstroy_core::Overlay::Image(im) => (im.t_in, im.t_out),
                    memstroy_core::Overlay::Video(v) => (v.t_in, v.t_out),
                };
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this overlay's range.".into();
                    return;
                }
                let mut right = ov.clone();
                let local_split = t - start;
                match &mut right {
                    memstroy_core::Overlay::Text(txt) => {
                        txt.id = format!("{}_R", txt.id);
                        txt.t_in = t;
                        txt.layout.retain(|kf| kf.t >= local_split);
                        for kf in txt.layout.iter_mut() { kf.t -= local_split; }
                        if txt.layout.is_empty() {
                            txt.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                    memstroy_core::Overlay::Image(im) => {
                        im.id = format!("{}_R", im.id);
                        im.t_in = t;
                        im.layout.retain(|kf| kf.t >= local_split);
                        for kf in im.layout.iter_mut() { kf.t -= local_split; }
                        if im.layout.is_empty() {
                            im.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                    memstroy_core::Overlay::Video(v) => {
                        v.id = format!("{}_R", v.id);
                        v.t_in = t;
                        v.layout.retain(|kf| kf.t >= local_split);
                        for kf in v.layout.iter_mut() { kf.t -= local_split; }
                        if v.layout.is_empty() {
                            v.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                }
                let local_split_left = local_split;
                // Preserve the overlay's lane assignment for the right half.
                let original_overlay_lane =
                    self.state.overlay_track_assignments.get(&i).copied();
                self.state.mutate(move |s| {
                    match &mut s.overlays[i] {
                        memstroy_core::Overlay::Text(txt) => {
                            txt.t_out = t;
                            txt.layout.retain(|kf| kf.t <= local_split_left);
                            if txt.layout.is_empty() {
                                txt.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
                        }
                        memstroy_core::Overlay::Image(im) => {
                            im.t_out = t;
                            im.layout.retain(|kf| kf.t <= local_split_left);
                            if im.layout.is_empty() {
                                im.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
                        }
                        memstroy_core::Overlay::Video(v) => {
                            v.t_out = t;
                            v.layout.retain(|kf| kf.t <= local_split_left);
                            if v.layout.is_empty() {
                                v.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
                        }
                    }
                    s.overlays.insert(i + 1, right);
                });
                // Shift overlay_track_assignments to account for the insert
                // and bind the right half to the original lane.
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
                self.state.status = "\u{2702} Overlay split at playhead.".into();
            }
            _ => {
                self.state.status = "\u{26A0} Select an element to split.".into();
            }
        }
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
                self.state.status = "\u{1F517} Backgrounds merged.".into();
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
                    s.actors[i].layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
                    s.actors.remove(i + 1);
                });
                self.state.status = "\u{1F517} Actors merged.".into();
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
                self.state.status = "\u{1F517} Overlays merged.".into();
            }
            _ => {
                self.state.status = "\u{26A0} Select an element with a next sibling to merge.".into();
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
            self.state.audio_waveforms.push(crate::state::AudioWaveform::default());
        }

        for audio_idx in 0..num_audio {
            let wf = &self.state.audio_waveforms[audio_idx];
            if wf.ready || wf.extracting {
                continue;
            }

            let source = self.state.scene.audio[audio_idx].source.clone();
            if !source.exists() {
                continue;
            }

            // Mark as extracting
            self.state.audio_waveforms[audio_idx].extracting = true;

            let source_clone = source.clone();
            // Use a shared slot to communicate results back
            let result_slot: Arc<Mutex<Option<(Vec<f32>, f32)>>> = Arc::new(Mutex::new(None));
            let slot_clone = result_slot.clone();

            self.rt.spawn(async move {
                let peaks = crate::state::AudioWaveform::extract_peaks(&source_clone, 512);
                if let Ok(mut slot) = slot_clone.lock() {
                    *slot = peaks;
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
                    self.waveform_extract_results.push(Arc::new(Mutex::new(None)));
                }
            }
            self.waveform_extract_results[audio_idx] = result_slot;
        }

        self.state.status = "\u{1F3B5} Extracting audio waveforms...".into();
    }

    /// Poll for waveform extraction completion across all audio tracks.
    fn poll_waveform_extraction(&mut self) {
        for audio_idx in 0..self.waveform_extract_results.len() {
            if audio_idx >= self.state.audio_waveforms.len() { break; }
            if self.state.audio_waveforms[audio_idx].ready { continue; }

            if let Ok(mut slot) = self.waveform_extract_results[audio_idx].lock() {
                if let Some((peaks, duration)) = slot.take() {
                    self.state.audio_waveforms[audio_idx].peaks = peaks;
                    self.state.audio_waveforms[audio_idx].duration = duration;
                    self.state.audio_waveforms[audio_idx].ready = true;
                    self.state.audio_waveforms[audio_idx].extracting = false;
                    self.state.status = format!(
                        "\u{2705} Waveform ready (audio {}): {:.1}s",
                        audio_idx, duration
                    );
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
            self.state.frame_caches.push(crate::video_cache::FrameCache::new(source, idx));
        }
        while self.frame_extract_results.len() < num_actors {
            self.frame_extract_results.push(Arc::new(Mutex::new(None)));
        }

        for actor_idx in 0..num_actors {
            let source = self.state.scene.actors[actor_idx].source.clone();

            if !source.exists() {
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
            self.state.frame_caches[actor_idx] = cache;

            let result_slot = self.frame_extract_results[actor_idx].clone();
            // Clear any previous result
            if let Ok(mut slot) = result_slot.lock() {
                *slot = None;
            }

            crate::video_cache::FrameCache::start_extraction(
                source,
                self.rt.handle(),
                move |duration, frame_count, cache_dir| {
                    if let Ok(mut slot) = result_slot.lock() {
                        *slot = Some((duration, frame_count, cache_dir));
                    }
                },
            );
        }

        self.state.status = "\u{1F3AC} Extracting preview frames...".into();
    }

    /// Poll for frame extraction completion across all actors.
    fn poll_frame_extraction(&mut self) {
        for actor_idx in 0..self.frame_extract_results.len() {
            if let Ok(mut slot) = self.frame_extract_results[actor_idx].lock() {
                if let Some((duration, frame_count, cache_dir)) = slot.take() {
                    if let Some(fc) = self.state.frame_caches.get_mut(actor_idx) {
                        fc.set_ready(duration, frame_count, cache_dir);
                        self.state.status = format!(
                            "\u{2705} Preview ready (actor {}): {} frames ({:.1}s)",
                            actor_idx, frame_count, duration
                        );
                    }
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
                        self.state.status = "\u{2705} Saved (.memstroy).".into();
                    }
                    Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
                }
            } else {
                match self.state.scene.save(&path) {
                    Ok(()) => {
                        self.state.status = "\u{2705} Saved.".into();
                        // Save layout alongside scene
                        let layout_path = path.with_extension("layout.json");
                        self.state.save_layout(&layout_path);
                    }
                    Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
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
            .add_filter("Memstroy Project", &["memstroy"])
            .add_filter("Scene YAML", &["yaml", "yml"])
            .add_filter("Scene JSON", &["json"])
            .set_file_name("project.memstroy")
            .save_file()
        {
            let is_memstroy = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.eq_ignore_ascii_case("memstroy"))
                .unwrap_or(false);
            let result = if is_memstroy {
                self.state
                    .save_memstroy(&path)
                    .map_err(|e| e.to_string())
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
                    self.state.status = "\u{2705} Saved.".into();
                    if self.state.active_tab < self.state.scene_tabs.len() {
                        let name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("Scene")
                            .to_string();
                        self.state.scene_tabs[self.state.active_tab].name = name;
                        self.state.scene_tabs[self.state.active_tab].path =
                            Some(path.clone());
                    }
                }
                Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
            }
        }
    }

    fn run_render(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MP4", &["mp4"])
            .save_file()
        else {
            return;
        };
        self.state.render_progress = Some(crate::state::RenderProgress {
            started: std::time::Instant::now(),
            last_log: String::new(),
            done: false,
            error: None,
            progress: 0.0,
        });
        // Force the project-native 1080x1920 (9:16 vertical) output for
        // every render. The user explicitly asked for "render quality
        // always 1080/1920" — overriding the scene's resolution at this
        // boundary keeps the rendered MP4 byte-for-byte aligned with the
        // editor's preview canvas, which is hard-coded to the same size
        // (see `inspector_nothing` in panels.rs).
        let mut scene_for_render = self.state.scene.clone();
        scene_for_render.output.resolution = [1080, 1920];
        spawn_render(
            self.rt.handle(),
            self.tx.clone(),
            scene_for_render,
            self.state.assets_root.clone(),
            path,
        );
        self.state.status = "\u{1F3A5} Rendering at 1080x1920...".into();
    }

    fn run_refresh(&mut self) {
        if self.state.refreshing {
            return;
        }
        self.state.refreshing = true;
        self.state.status = "\u{1F504} Refreshing clips via assets-server...".into();
        spawn_refresh(
            self.rt.handle(),
            self.tx.clone(),
            self.state.server_url.clone(),
            self.state.tg_channel.clone(),
            self.state.tg_limit,
            self.state.clips_dir(),
        );
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
        let elapsed = rp.started.elapsed();
        let elapsed_secs = elapsed.as_secs_f32();
        let progress = rp.progress.clamp(0.0, 1.0);
        let mut dismiss = false;

        let title = if rp.error.is_some() {
            format!("\u{274C} {}", crate::i18n::t("Render failed"))
        } else if rp.done {
            format!("\u{2705} {}", crate::i18n::t("Render complete"))
        } else {
            format!("\u{1F3AC} {}", crate::i18n::t("Rendering..."))
        };

        egui::Window::new(title)
            .id(egui::Id::new("render_progress_window"))
            .anchor(egui::Align2::RIGHT_TOP, [-16.0, 16.0])
            .default_size([360.0, 180.0])
            .min_width(280.0)
            .max_width(520.0)
            .collapsible(true)
            .resizable(true)
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
                    format!(" \u{2014} ETA {}", format_elapsed(remaining))
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
                        .add_enabled(
                            can_close,
                            egui::Button::new(crate::i18n::t("Close")),
                        )
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

    /// Periodically saves the current scene to `~/.memstroy/autosave.scene.yaml`.
    /// Triggered from `update()`. Updates `last_autosave` and shows a 2 s toast.
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

        let path = EditorState::autosave_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match self.state.scene.save(&path) {
            Ok(()) => {
                self.state.last_autosave = Some(std::time::Instant::now());
                self.state.autosave_toast_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                self.state.status = "\u{1F4BE} Auto-saved".into();
            }
            Err(e) => {
                self.state.status = format!("\u{26A0} Autosave failed: {e}");
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
                    let yes = egui::Button::new(RichText::new(crate::i18n::t("Yes, restore")).color(Color32::WHITE))
                        .fill(Color32::from_rgb(60, 160, 80));
                    if ui.add(yes).clicked() {
                        decision = Some("yes");
                        close = true;
                    }
                    let no = egui::Button::new(RichText::new(crate::i18n::t("No, discard")).color(Color32::WHITE))
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
            Some("yes") => match Scene::load(&autosave_path) {
                Ok(scene) => {
                    self.state.scene = scene;
                    self.state.scene_path = None;
                    self.state.status = "\u{2705} Recovered scene loaded.".into();
                }
                Err(e) => {
                    self.state.status = format!("\u{274C} Recovery failed: {e}");
                }
            },
            Some("no") => {
                let _ = std::fs::remove_file(&autosave_path);
                self.state.status = "\u{1F5D1} Recovery discarded.".into();
            }
            Some("later") => {
                self.state.status = "Recovery postponed.".into();
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
                        .color(Color32::from_rgb(180, 140, 255)),
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
                                .fill(Color32::from_rgb(32, 32, 48))
                                .rounding(Rounding::same(8.0))
                                .stroke(Stroke::new(1.0, Color32::from_rgb(60, 60, 80)))
                                .inner_margin(egui::Margin::same(8.0));

                            let resp = frame
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(tpl.icon).size(22.0));
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(tpl.name).strong().size(13.0),
                                            );
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
                let t_out = (t_in + 3.0).min(scene_dur.max(t_in + 0.1));
                let mut new_idx_out: usize = 0;
                self.state.mutate(|scene| {
                    new_idx_out = crate::title_templates::add_template_to_scene(
                        scene, tpl, t_in, t_out,
                    );
                });
                self.state.selection = Selection::Overlay(new_idx_out);
                self.state.status = format!("\u{2728} Added title: {}", tpl.name);
                self.state.title_picker_open = false;
            }
        }
    }
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

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events(ctx);
        self.poll_frame_extraction();
        self.poll_waveform_extraction();
        // Pick up sinks built on the background audio-load thread (see
        // `audio_engine.rs`). Cheap when there's nothing pending; lets
        // playback start without freezing the UI thread on file decode.
        self.audio_engine.poll_pending();

        // Auto-start frame extraction for actors that have source files
        if !self.state.scene.actors.is_empty() {
            let needs_extraction = self.state.scene.actors.iter().enumerate().any(|(i, actor)| {
                actor.source.exists()
                    && self.state.frame_caches.get(i).map(|fc| !fc.is_ready() && !fc.extracting).unwrap_or(true)
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
                au.source.exists()
                    && self.state.audio_waveforms.get(i)
                        .map(|wf| !wf.ready && !wf.extracting)
                        .unwrap_or(true)
            });
            if needs_wf {
                self.start_waveform_extraction();
            }
        }

        // Keyboard shortcuts
        self.handle_shortcuts(ctx);

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
            self.state.pre_press_scene = Some(self.state.scene.clone());
        }

        // Snapshot the scene at the *very* start of every frame so we
        // can fall back to a "scene differs at end of frame, no pointer
        // gesture in flight" undo entry. This catches edits that don't
        // go through a pointer press/release at all — keyboard typing
        // into a `DragValue`, arrow-key nudges, popup menu picks, etc.
        // Without it, those edits would never get an undo snapshot, and
        // the user only sees ONE history entry for an entire session
        // (which manifests as "Ctrl+Z bounces between two states").
        let frame_start_scene = self.state.scene.clone();

        // ── End the active drag-undo group when no mouse button is down ──
        // The undo/redo system snapshots once per drag gesture by tracking
        // a `last_drag_group` token. The token must be cleared as soon as
        // the gesture ends (no pointer button held), so the *next* drag
        // pushes a fresh undo entry instead of being absorbed into the
        // previous one. See `EditorState::mutate_drag` for details.
        // (`state.timeline_drag.dragging_clip` is also cleared on drag-end,
        // but that's owned by `panels::timeline` itself — don't touch it
        // from here or its lane-commit logic stops firing.)
        let any_pointer_down = ctx.input(|i| {
            i.pointer.primary_down()
                || i.pointer.secondary_down()
                || i.pointer.middle_down()
        });
        if !any_pointer_down {
            self.state.end_drag_group();
        }

        // Play/pause: advance playhead
        if self.state.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1); // cap at 100ms
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
            if self.state.playhead >= self.state.scene.output.duration
                || self.state.playhead < 0.0
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
            .frame(egui::Frame::none().fill(Color32::from_rgb(25, 25, 35)).inner_margin(6.0))
            .show(ctx, |ui| self.menu(ctx, ui));

        // ── Tab bar for multiple scenes ──
        egui::TopBottomPanel::top("tab_bar")
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(18, 18, 26))
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
            .width_range(180.0..=560.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 22, 32))
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
            self.split_at_playhead();
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
        let drop_pointer = ctx.input(|i| {
            i.pointer.latest_pos().or_else(|| i.pointer.hover_pos())
        });
        let lib_rect = self.state.library_panel_rect;
        let pointer_in_library = match (drop_pointer, lib_rect) {
            (Some(p), Some(r)) => r.contains(p),
            _ => false,
        };
        if !dropped_files.is_empty() {
            for file in &dropped_files {
                let Some(path) = &file.path else { continue; };
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let is_video = ["mp4", "mov", "webm", "avi", "mkv", "m4v"]
                    .contains(&ext.as_str());
                let is_image =
                    ["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str());
                let is_audio = ["mp3", "wav", "ogg", "flac", "aac", "m4a", "opus"]
                    .contains(&ext.as_str());
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
                        && self.state.library_tab
                            == crate::state::LibraryTab::Particles
                    {
                        self.state.particles_dir()
                    } else {
                        self.state.images_dir()
                    }
                } else {
                    self.state.sounds_dir()
                };
                if let Err(err) = std::fs::create_dir_all(&dest_dir) {
                    self.state.status =
                        format!("Couldn't create {}: {}", dest_dir.display(), err);
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
                                "Couldn't import {}: {}",
                                path.display(),
                                err
                            );
                            false
                        }
                    }
                };
                if !copied_ok { continue; }

                // Refresh the library so the new file shows up on the
                // panel. Switch the visible tab to the matching kind
                // when the drop landed on the library panel itself
                // (otherwise leave the user where they are).
                self.state.reload_library();
                if pointer_in_library {
                    self.state.library_tab = if is_video {
                        crate::state::LibraryTab::Videos
                    } else if is_image
                        && self.state.library_tab
                            != crate::state::LibraryTab::Particles
                    {
                        crate::state::LibraryTab::Images
                    } else if is_audio {
                        crate::state::LibraryTab::Sounds
                    } else {
                        self.state.library_tab
                    };
                    self.state.status = format!(
                        "Imported into library: {}",
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
                    // `add_actor_from_clip` creates a matching AudioTrack
                    // and pre-loads any per-clip chroma/skeleton sidecars.
                    crate::panels::add_actor_from_clip(&mut self.state, &dest);
                    // Pin the freshly added actor onto the first empty
                    // video lane (or a newly-inserted one) so canvas
                    // drops always create a clean layer instead of
                    // stacking on top of whatever was already on V1.
                    if let Some(new_idx) =
                        self.state.scene.actors.len().checked_sub(1)
                    {
                        let t = self.state.playhead;
                        let lane = self.state.pick_or_create_empty_video_lane_at(t);
                        self.state.actor_track_assignments.insert(new_idx, lane);
                    }
                } else if is_image {
                    let id = dest.file_stem().and_then(|s| s.to_str())
                        .map(|s| format!("img_{}", s))
                        .unwrap_or_else(|| format!("img_{}", self.state.scene.overlays.len() + 1));
                    let overlay = memstroy_core::Overlay::Image(memstroy_core::ImageOverlay {
                        id: id.clone(),
                        source: dest.clone(),
                        t_in: self.state.playhead,
                        t_out: (self.state.playhead + 3.0).min(self.state.scene.output.duration),
                        layout: vec![memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default())],
                        modifiers: Vec::new(),
                        skeleton_attachment: None,
                        effects: Vec::new(),
                        animated_params: Default::default(),
                        chroma_key: None,
                    });
                    self.state.scene.overlays.push(overlay);
                    let new_idx = self.state.scene.overlays.len() - 1;
                    let t = self.state.playhead;
                    let lane = self.state.pick_or_create_empty_video_lane_at(t);
                    self.state.overlay_track_assignments.insert(new_idx, lane);
                    self.state.selection = Selection::Overlay(new_idx);
                    self.state.status = format!("Dropped image: {} (saved to library)", id);
                } else if is_audio {
                    let id = dest.file_stem().and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("audio_{}", self.state.scene.audio.len() + 1));
                    self.state.scene.audio.push(memstroy_core::AudioTrack {
                        id: id.clone(),
                        source: dest.clone(),
                        t_in: self.state.playhead,
                        ..Default::default()
                    });
                    self.state.selection = Selection::Audio(self.state.scene.audio.len() - 1);
                    self.state.status = format!("Dropped audio: {} (saved to library)", id);
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
            let mut seen: std::collections::HashSet<std::path::PathBuf> =
                std::collections::HashSet::new();
            for a in &state.scene.audio {
                // Sample every animatable field at the playhead's
                // clip-local time so a freshly-built sink reflects the
                // user's animated values at the current moment. The
                // engine then plays back those static values; live
                // mid-stream updates happen by detecting a change in
                // `signature()` between frames and rebuilding.
                let t_local = (state.playhead - a.t_in).max(0.0);
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: a.source.clone(),
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
                seen.insert(a.source.clone());
            }
            for actor in &state.scene.actors {
                if !actor.visible { continue; }
                if seen.contains(&actor.source) { continue; }
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: actor.source.clone(),
                    t_in: actor.t_in.unwrap_or(0.0),
                    t_out: actor.t_out,
                    source_start: actor.source_start,
                    volume: 1.0,
                    speed: 1.0,
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
        let expected_step = if self.state.playing { dt * self.state.playback_speed.abs() } else { 0.0 };
        let actual_delta = self.state.playhead - self.prev_playhead;
        let seeked = (actual_delta - expected_step).abs() > 0.15
            || actual_delta < -0.05;

        if self.state.playing && !self.was_playing {
            // Transition: paused → playing. Start playback at the current playhead.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.prev_audio_signature = signature_of(&sources);
            self.audio_engine.play_sources(&sources, self.state.playhead);
        } else if !self.state.playing && self.was_playing {
            // Transition: playing → paused.
            self.audio_engine.pause();
        } else if self.state.playing && seeked {
            // Seek while playing — restart from the new position so audio stays in sync.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.prev_audio_signature = signature_of(&sources);
            self.audio_engine.play_sources(&sources, self.state.playhead);
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
                self.audio_engine.play_sources(&sources, self.state.playhead);
            }
        }

        self.was_playing = self.state.playing;
        self.prev_playhead = self.state.playhead;

        // Right panel: Inspector
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(350.0)
            .width_range(220.0..=620.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 22, 32))
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
                    .fill(Color32::from_rgb(18, 18, 28))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                panels::timeline(ui, &mut self.state);
            });

        // Central panel: Preview
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(15, 15, 22))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                crate::canvas_preview::canvas_preview(ui, &mut self.state);
            });

        // Node editor was removed.

        // Curve editor floating window
        if self.state.curve_editor_open {
            let mut curve_open = self.state.curve_editor_open;
            egui::Window::new("Curve Editor")
                .open(&mut curve_open)
                .default_size([600.0, 240.0])
                .resizable(true)
                .collapsible(true)
                .anchor(egui::Align2::LEFT_BOTTOM, [10.0, -10.0])
                .show(ctx, |ui| {
                    self.draw_curve_editor_body(ui);
                });
            self.state.curve_editor_open = curve_open;
        }

        // Image editor floating window (replaces the old clip editor —
        // image-only editing logic that doesn't apply to videos).
        if self.state.image_editor_open {
            self.state.image_editor_open = image_editor::image_editor_window(ctx, &mut self.state);
        }

        // Skeleton editor floating window
        crate::skeleton_editor::skeleton_editor_window(ctx, &mut self.state);

        // Title-templates picker (popup grid of preset captions)
        self.show_title_picker(ctx);

        // Auto-save tick + recovery modal
        self.tick_autosave();
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
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
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
            i.pointer.primary_down()
                || i.pointer.secondary_down()
                || i.pointer.middle_down()
        });
        let mut release_block_handled_undo = false;
        if released_this_frame && !any_pointer_still_down {
            if let Some(pre) = self.state.pre_press_scene.take() {
                let mutate_drag_handled = self.state.last_drag_group.is_some();
                release_block_handled_undo = true;
                if !mutate_drag_handled {
                    let pre_yaml = serde_yaml::to_string(&pre).unwrap_or_default();
                    let cur_yaml =
                        serde_yaml::to_string(&self.state.scene).unwrap_or_default();
                    if pre_yaml != cur_yaml {
                        self.state.undo.push(&pre);
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
        {
            let pre_yaml =
                serde_yaml::to_string(&frame_start_scene).unwrap_or_default();
            let cur_yaml =
                serde_yaml::to_string(&self.state.scene).unwrap_or_default();
            if pre_yaml != cur_yaml {
                self.state.undo.push(&frame_start_scene);
            }
        }
    }
}

/// Apply a modern dark theme with accent colors.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();

    // Background colors
    visuals.panel_fill = Color32::from_rgb(20, 20, 30);
    visuals.window_fill = Color32::from_rgb(28, 28, 40);
    visuals.extreme_bg_color = Color32::from_rgb(12, 12, 18);

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(35, 35, 50);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(40, 40, 58);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(60, 60, 90);
    visuals.widgets.active.bg_fill = Color32::from_rgb(80, 60, 180);

    // Accent colors
    visuals.selection.bg_fill = Color32::from_rgb(100, 60, 200);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(180, 140, 255));
    visuals.hyperlink_color = Color32::from_rgb(140, 100, 255);

    // Rounded corners
    visuals.widgets.noninteractive.rounding = Rounding::same(6.0);
    visuals.widgets.inactive.rounding = Rounding::same(6.0);
    visuals.widgets.hovered.rounding = Rounding::same(6.0);
    visuals.widgets.active.rounding = Rounding::same(6.0);
    visuals.window_rounding = Rounding::same(10.0);

    // Text
    visuals.override_text_color = Some(Color32::from_rgb(220, 220, 240));

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);

    ctx.set_style(style);
}
