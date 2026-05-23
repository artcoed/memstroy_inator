//! UI panels — Premiere Pro-style timeline, modern inspector, drag&drop.

use std::path::PathBuf;

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::i18n::t;
use crate::state::{AssetDragKind, EditorState, LibraryTab, Selection, TrackKind};


// ─── DRAG MODE FOR TIMELINE CLIPS ────────────────────────────────────
//
// Captured once at `drag_started` and stashed in egui's per-id temp memory
// for the duration of the drag, so the mode never flips mid-drag (which
// previously happened when the clip moved out from under the pointer's
// initial edge zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipDragMode {
    Move,
    TrimLeft,
    TrimRight,
}


// ─── COLORS ──────────────────────────────────────────────────────────

const COL_BG_TRACK: Color32 = Color32::from_rgb(24, 24, 34);
const COL_BG_TRACK_ALT: Color32 = Color32::from_rgb(28, 28, 38);
const COL_RULER: Color32 = Color32::from_rgb(32, 32, 44);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 60, 60);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 160);
const COL_TEXT: Color32 = Color32::from_rgb(220, 220, 240);
const COL_CLIP_ACTOR: Color32 = Color32::from_rgb(220, 130, 50);
const COL_CLIP_BG: Color32 = Color32::from_rgb(60, 130, 220);
const COL_CLIP_OVERLAY: Color32 = Color32::from_rgb(80, 200, 120);
const COL_CLIP_AUDIO: Color32 = Color32::from_rgb(50, 180, 180);
const COL_SELECTED: Color32 = Color32::from_rgb(255, 220, 80);


// ─── LIBRARY ─────────────────────────────────────────────────────────

/// Asset library panel: a tabbed browser over Mellstroy clips, sounds,
/// PNG stickers, and particle presets. Each tab shares the same drag
/// model — picking up an entry sets `state.asset_drag` and the canvas /
/// timeline drop targets handle the rest.
///
/// **Refresh model**: the library no longer has a manual Refresh button
/// or visible Server / channel / limit fields. Instead, the GUI auto-
/// triggers a refresh when:
///   1. The user types into the search box (the in-flight server fetch
///      is debounced so we don't refire every keystroke).
///   2. The clips list is scrolled near its bottom (infinite-scroll
///      style — gives the user "more clips" without leaving the panel).
/// The `memstroy-assets-server` instance is expected to be running and
/// to periodically re-ingest from Telegram on its own; the GUI only
/// asks it to deliver more.
pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    // Capture the panel rect so the OS-level file-drop handler in `app.rs`
    // can route drops onto this region into the Videos / Images / Sounds
    // sub-folder rather than dropping straight onto the timeline.
    state.library_panel_rect = Some(ui.max_rect());

    ui.label(RichText::new(crate::i18n::t("Library")).size(16.0).strong());
    ui.add_space(4.0);

    // Tab bar — Clips / Videos / Sounds / Images. The legacy
    // "Particles" tab was retired (per user request) but the on-disk
    // particles folder is still scanned in case existing scenes
    // reference assets from there.
    ui.horizontal_wrapped(|ui| {
        let tabs: [(LibraryTab, &str, &'static str); 4] = [
            (LibraryTab::Clips,     "\u{1F3AC} ", "Clips"),
            (LibraryTab::Videos,    "\u{1F4FD} ", "Videos"),
            (LibraryTab::Sounds,    "\u{1F50A} ", "Sounds"),
            (LibraryTab::Images,    "\u{1F5BC} ", "Images"),
        ];
        // Migrate any session that's still pointed at the now-hidden
        // Particles tab back to Images so the panel doesn't end up
        // blank.
        if state.library_tab == LibraryTab::Particles {
            state.library_tab = LibraryTab::Images;
        }
        for (tab, icon, key) in tabs {
            let label = format!("{}{}", icon, crate::i18n::t(key));
            if ui.selectable_label(state.library_tab == tab, label).clicked() {
                state.library_tab = tab;
            }
        }
    });
    ui.add_space(4.0);

    // ── Search field. Typing here triggers an auto-refresh when the
    // active tab is `Clips`, so the editor pulls fresh posts that match
    // the new query (the assets-server scrapes the channel; the GUI
    // doesn't care which subset of the channel matches client-side). ──
    let search_resp = ui.add(
        egui::TextEdit::singleline(&mut state.library_search)
            .hint_text(crate::i18n::t("Search library..."))
            .desired_width(ui.available_width()),
    );
    let search_changed = search_resp.changed()
        || state.prev_library_search_tab != state.library_tab;
    let search_committed = search_resp.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if search_changed || search_committed {
        state.prev_library_search = state.library_search.clone();
        state.prev_library_search_tab = state.library_tab;
        // Only the Clips tab talks to the server; other tabs rescan
        // their local directories instead.
        if state.library_tab == LibraryTab::Clips {
            maybe_auto_refresh(state, /*force=*/ search_committed);
        }
    }
    ui.add_space(2.0);

    let hint_text = match state.library_tab {
        LibraryTab::Clips => t("Drag a clip onto the canvas or timeline. The library auto-updates from the assets-server (which periodically ingests from Telegram)."),
        LibraryTab::Videos => t("User-imported videos. Drop a video file from your file manager into this panel to add it. Drag a row onto the canvas or timeline to spawn an actor."),
        LibraryTab::Sounds => t("Drop a sound onto the timeline to add it as an audio track. Drop audio files from your file manager here to import."),
        LibraryTab::Images => t("Drag a sticker onto the canvas to add it as an image overlay. Drop image files from your file manager here to import."),
        LibraryTab::Particles => t("Drag a particle onto the canvas — it spawns with spin + pulse modifiers."),
    };
    ui.label(
        RichText::new(hint_text)
            .size(9.0)
            .italics()
            .color(COL_TEXT_DIM),
    );
    ui.add_space(6.0);

    match state.library_tab {
        LibraryTab::Clips => library_clips_tab(ui, state),
        LibraryTab::Videos => library_assets_tab(ui, state, AssetDragKind::Video),
        LibraryTab::Sounds => library_assets_tab(ui, state, AssetDragKind::Sound),
        LibraryTab::Images => library_assets_tab(ui, state, AssetDragKind::Image),
        LibraryTab::Particles => library_assets_tab(ui, state, AssetDragKind::Particle),
    }
}

/// Fire a server refresh, debounced. `force=true` bypasses the debounce
/// (used by Enter-in-search-box, where the user has clearly committed).
fn maybe_auto_refresh(state: &mut EditorState, force: bool) {
    if state.refreshing {
        return;
    }
    if !force {
        if let Some(t) = state.last_auto_refresh {
            if t.elapsed() < std::time::Duration::from_millis(500) {
                return;
            }
        }
    }
    state.last_auto_refresh = Some(std::time::Instant::now());
    state.status = "__REFRESH_REQUESTED__".into();
}

/// Render a "Local | Global" split inside the library panel — kept
/// here as dead code in case future iterations want to bring back a
/// per-tab user/global division. The current UI flattens the list
/// because the "Local" half had been an empty placeholder anyway.
#[allow(dead_code)]
fn library_split_panel<L, G>(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id_salt: &str,
    local_section: L,
    global_section: G,
) where
    L: FnOnce(&mut egui::Ui, &mut EditorState),
    G: FnOnce(&mut egui::Ui, &mut EditorState),
{
    let total_rect = ui.available_rect_before_wrap();
    let total_h = total_rect.height().max(80.0);
    // Reserve room for two headers + the drag handle.
    let header_h = 18.0_f32;
    let handle_h = 6.0_f32;
    let inner_h = (total_h - 2.0 * header_h - handle_h).max(40.0);
    let split = state.library_split.clamp(0.05, 0.95);
    let local_h = (split * inner_h).max(20.0);
    let global_h = (inner_h - local_h).max(20.0);

    // ─── Local section header + body ─────────────────────────────────
    ui.label(
        RichText::new(t("Local (your imports)"))
            .size(10.0)
            .strong()
            .color(Color32::from_rgb(180, 220, 180)),
    );
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), local_h),
        egui::Layout::top_down(egui::Align::LEFT),
        |ui| {
            ui.set_min_size(Vec2::new(ui.available_width(), local_h));
            local_section(ui, state);
        },
    );

    // ─── Draggable splitter ──────────────────────────────────────────
    let handle_id = ui.make_persistent_id(id_salt);
    let (handle_rect, handle_resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), handle_h), Sense::click_and_drag());
    let hovered = handle_resp.hovered();
    let dragging = handle_resp.dragged();
    let _ = handle_id;

    // Render the handle as a thin gradient bar with a centered grip,
    // brighter when hovered/dragged so the affordance is obvious.
    let bar_col = if dragging {
        Color32::from_rgb(180, 140, 255)
    } else if hovered {
        Color32::from_rgb(120, 100, 200)
    } else {
        Color32::from_rgb(60, 60, 80)
    };
    ui.painter().rect_filled(handle_rect, Rounding::same(2.0), bar_col);
    // Draw a small "≡" grip to make the affordance obvious without
    // depending on font glyph coverage. Three short horizontal lines.
    let grip_col = Color32::from_rgba_premultiplied(255, 255, 255, 180);
    let cx = handle_rect.center().x;
    let cy = handle_rect.center().y;
    for dx in [-9.0_f32, 0.0, 9.0] {
        ui.painter().line_segment(
            [egui::pos2(cx + dx - 3.0, cy), egui::pos2(cx + dx + 3.0, cy)],
            Stroke::new(1.0, grip_col),
        );
    }
    if hovered || dragging {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    if dragging {
        let dy = handle_resp.drag_delta().y;
        if inner_h > 0.0 {
            let new_split = split + dy / inner_h;
            state.library_split = new_split.clamp(0.05, 0.95);
        }
    }
    // Double-click resets to 50/50.
    if handle_resp.double_clicked() {
        state.library_split = 0.5;
    }

    // ─── Global section header + body ────────────────────────────────
    ui.label(
        RichText::new(t("Global (built-in / browser)"))
            .size(10.0)
            .strong()
            .color(Color32::from_rgb(180, 200, 255)),
    );
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), global_h),
        egui::Layout::top_down(egui::Align::LEFT),
        |ui| {
            ui.set_min_size(Vec2::new(ui.available_width(), global_h));
            global_section(ui, state);
        },
    );
}

/// Render the Mellstroy clip browser content. Refresh is now implicit:
/// scrolling near the bottom of the list asks the assets-server for
/// more, and editing the search field re-fires the request. Server URL
/// / channel / limit no longer have UI controls — they live as plain
/// EditorState fields so the assets-server can be configured from
/// outside (or via project settings) without surfacing infrastructure
/// in the editor chrome.
fn library_clips_tab(ui: &mut egui::Ui, state: &mut EditorState) {
    let search_lower = state.library_search.to_lowercase();
    let clip_count = state.library.mellstroy_clips.len();

    // ── Header row: clip count + channel badge + manual Refresh ──
    // The library was historically refresh-on-search-only, but that
    // left users with an empty cache (first launch / fresh install)
    // with no obvious affordance: scrolling did nothing because the
    // list was empty, and the search box still had to be tabbed into
    // to fire a request. Surface the channel and an explicit
    // Refresh button so the flow is discoverable.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} ({})", t("Clips"), clip_count))
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(220, 130, 50)),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!("@{}", state.tg_channel))
                .size(10.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Disable the button while a refresh is already running
            // so we don't fire concurrent ingest requests.
            let label = if state.refreshing {
                format!("\u{1F504} {}", t("refreshing..."))
            } else {
                format!("\u{1F504} {}", t("Refresh from Telegram"))
            };
            let resp = ui.add_enabled(
                !state.refreshing,
                egui::Button::new(RichText::new(label).size(11.0)),
            );
            if resp.clicked() {
                maybe_auto_refresh(state, /*force=*/ true);
            }
        });
    });
    // Clips are server-managed by design — the Refresh action POSTs
    // to the in-process memstroy-assets-server which scrapes Telegram
    // on the GUI's behalf. Surface that explicitly so users know which
    // controls drive this list. Display the *connect-able* URL so the
    // user never sees a confusing wildcard host like `0.0.0.0:8765`,
    // which is a valid bind address but cannot be used as a request
    // target on Windows / macOS.
    ui.label(
        RichText::new(format!(
            "\u{1F310} {}: {}",
            crate::i18n::t("server"),
            crate::state::rewrite_server_url_for_client(&state.server_url)
        ))
        .size(9.0)
        .italics()
        .color(COL_TEXT_DIM),
    );
    if state.refreshing {
        ui.label(
            RichText::new(crate::i18n::t("refreshing..."))
                .size(10.0)
                .italics()
                .color(Color32::from_rgb(255, 200, 80)),
        );
    }
    ui.add_space(2.0);

    // ── First-show auto-fetch ──
    // When the panel opens with an empty local cache and we haven't
    // tried yet, kick a refresh automatically. This is what users
    // expect from "open the editor and see Mellstroy clips" without
    // having to discover the search-box trick. Guarded by
    // `last_auto_refresh.is_none()` so we only do it once per session
    // — subsequent empty states (e.g. user wipes the cache mid-session)
    // can still trigger via the Refresh button above.
    if state.library.mellstroy_clips.is_empty()
        && state.last_auto_refresh.is_none()
        && !state.refreshing
    {
        maybe_auto_refresh(state, /*force=*/ true);
    }

    let scroll_out = egui::ScrollArea::vertical()
        .id_source("library_clips_scroll")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            if state.library.mellstroy_clips.is_empty() {
                let hint = if state.refreshing {
                    t("Fetching clips from the server...")
                } else {
                    t("No clips yet — click Refresh from Telegram above to fetch the latest ones.")
                };
                ui.label(
                    RichText::new(hint)
                        .italics()
                        .color(COL_TEXT_DIM)
                        .size(11.0),
                );
                return;
            }
            for idx in 0..state.library.mellstroy_clips.len() {
                let clip = &state.library.mellstroy_clips[idx];
                if !search_lower.is_empty() {
                    let clean = clean_clip_text(&clip.description).to_lowercase();
                    let id_str = clip.id.to_string();
                    if !clean.contains(&search_lower) && !id_str.contains(&search_lower) {
                        continue;
                    }
                }
                let clip = state.library.mellstroy_clips[idx].clone();
                clip_card(ui, state, &clip);
            }
        });

    // ── Auto-refresh on near-bottom scroll ──
    // When the visible viewport ends within ~80 px of the content's
    // bottom AND the list is non-empty AND we're not already refreshing,
    // ask the server for more clips. The debounce inside
    // `maybe_auto_refresh` prevents storming on every paint.
    let viewport_bottom = scroll_out.state.offset.y + scroll_out.inner_rect.height();
    let near_bottom = viewport_bottom + 80.0 >= scroll_out.content_size.y
        && scroll_out.content_size.y > scroll_out.inner_rect.height();
    if near_bottom && !state.library.mellstroy_clips.is_empty() {
        maybe_auto_refresh(state, /*force=*/ false);
    }
}

/// Render a generic LibraryAsset list (sounds / images / particles / videos).
/// All four share the same row layout — only the title colour and the
/// drop semantics differ, which we encode via `AssetDragKind`.
fn library_assets_tab(ui: &mut egui::Ui, state: &mut EditorState, kind: AssetDragKind) {
    let (title, dir, title_color) = match kind {
        AssetDragKind::Sound => (
            "Sounds",
            state.sounds_dir(),
            Color32::from_rgb(120, 200, 255),
        ),
        AssetDragKind::Image => (
            "Images",
            state.images_dir(),
            Color32::from_rgb(180, 255, 180),
        ),
        AssetDragKind::Particle => (
            "Particles",
            state.particles_dir(),
            Color32::from_rgb(255, 220, 120),
        ),
        AssetDragKind::Video => (
            "Videos",
            state.videos_dir(),
            Color32::from_rgb(220, 130, 50),
        ),
        // Reused for clips elsewhere; not expected here.
        _ => return,
    };

    let assets: &[crate::state::LibraryAsset] = match kind {
        AssetDragKind::Sound => &state.library.sounds,
        AssetDragKind::Image => &state.library.images,
        AssetDragKind::Particle => &state.library.particles,
        AssetDragKind::Video => &state.library.videos,
        _ => return,
    };

    let search_lower = state.library_search.to_lowercase();
    let count = assets.len();
    ui.label(
        RichText::new(format!("{} ({})", title, count))
            .size(12.0)
            .strong()
            .color(title_color),
    );
    ui.add_space(2.0);

    if count == 0 {
        ui.label(
            RichText::new(format!(
                "Empty. Drop files into {}.",
                dir.display()
            ))
            .italics()
            .color(COL_TEXT_DIM)
            .size(10.0),
        );
        return;
    }

    let scroll_id = format!("library_{}_scroll", title.to_lowercase());
    // Snapshot the row data so the borrow checker is happy with the
    // mutable `state` we pass to `library_asset_card`.
    // Partition the rows into "Local" (anything under the editor's
    // local asset directory `dir`) and "Server" (everything else —
    // typically files the in-process memstroy-assets-server pulled
    // from the network or a shared cache). The user explicitly asked
    // for this distinction so they can tell at a glance which assets
    // they own and which need a live server connection to update.
    let local_dir = dir.clone();
    let mut local_rows: Vec<crate::state::LibraryAsset> = Vec::new();
    let mut server_rows: Vec<crate::state::LibraryAsset> = Vec::new();
    for a in assets.iter() {
        if !search_lower.is_empty()
            && !a.label.to_lowercase().contains(&search_lower)
            && !a.id.to_lowercase().contains(&search_lower)
        {
            continue;
        }
        if a.path.starts_with(&local_dir) {
            local_rows.push(a.clone());
        } else {
            server_rows.push(a.clone());
        }
    }

    egui::ScrollArea::vertical()
        .id_source(scroll_id)
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "\u{1F4C1} {} ({})",
                    crate::i18n::t("Local (your library)"),
                    local_rows.len()
                ))
                .size(11.0)
                .strong()
                .color(Color32::from_rgb(180, 220, 180)),
            );
            ui.label(
                RichText::new(format!(
                    "{}: {}",
                    crate::i18n::t("dir"),
                    local_dir.display()
                ))
                .size(9.0)
                .italics()
                .color(COL_TEXT_DIM),
            );
            if local_rows.is_empty() {
                ui.label(
                    RichText::new(crate::i18n::t(
                        "(empty — drop files into the local directory above)"
                    ))
                    .size(10.0)
                    .italics()
                    .color(COL_TEXT_DIM),
                );
            } else {
                for asset in &local_rows {
                    library_asset_card(ui, state, asset, kind, title_color);
                }
            }
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!(
                    "\u{1F310} {} ({})",
                    crate::i18n::t("Server (auto-fetched)"),
                    server_rows.len()
                ))
                .size(11.0)
                .strong()
                .color(Color32::from_rgb(180, 200, 255)),
            );
            ui.label(
                RichText::new(format!(
                    "{}: {}",
                    crate::i18n::t("source"),
                    crate::state::rewrite_server_url_for_client(&state.server_url)
                ))
                .size(9.0)
                .italics()
                .color(COL_TEXT_DIM),
            );
            if server_rows.is_empty() {
                ui.label(
                    RichText::new(crate::i18n::t(
                        "(none — server hasn't ingested anything in this category yet)"
                    ))
                    .size(10.0)
                    .italics()
                    .color(COL_TEXT_DIM),
                );
            } else {
                for asset in &server_rows {
                    library_asset_card(ui, state, asset, kind, title_color);
                }
            }
        });
}

/// Compact card for a single sound / image / particle entry. Mirrors
/// `clip_card` but uses the LibraryAsset schema and the appropriate
/// drag kind so canvas / timeline drop targets construct the right
/// scene element on release.
fn library_asset_card(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    asset: &crate::state::LibraryAsset,
    kind: AssetDragKind,
    accent: Color32,
) {
    let avail_w = ui.available_width().max(80.0);

    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 32, 48))
        .rounding(Rounding::same(4.0))
        .inner_margin(egui::Margin::same(3.0))
        .stroke(Stroke::new(1.0, Color32::from_rgb(50, 50, 70)));

    let card_resp = frame.show(ui, |ui| {
        ui.set_min_width(avail_w - 6.0);
        ui.horizontal(|ui| {
            let thumb_size = Vec2::new(48.0, 48.0);
            if let Some(thumb) = &asset.thumbnail {
                let uri = format!("file://{}", thumb.display());
                ui.add(
                    egui::Image::from_uri(uri)
                        .fit_to_exact_size(thumb_size)
                        .maintain_aspect_ratio(false)
                        .rounding(Rounding::same(3.0)),
                );
            } else {
                let (rect, _) = ui.allocate_exact_size(thumb_size, Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 55));
                let icon = match kind {
                    AssetDragKind::Sound => "\u{1F50A}",
                    AssetDragKind::Image => "\u{1F5BC}",
                    AssetDragKind::Particle => "\u{2728}",
                    AssetDragKind::Video => "\u{1F4FD}",
                    _ => "?",
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    icon,
                    egui::FontId::proportional(22.0),
                    accent,
                );
            }
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), thumb_size.y),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(RichText::new(&asset.label).size(11.0).color(COL_TEXT));
                    if let Some(name) = asset.path.file_name().and_then(|s| s.to_str()) {
                        ui.label(RichText::new(name).size(9.0).color(COL_TEXT_DIM));
                    }
                },
            );
        });
    }).response;

    let card_resp = card_resp.interact(Sense::click_and_drag());
    if card_resp.dragged() {
        state.asset_drag.dragging = Some(asset.path.clone());
        state.asset_drag.kind = kind;
        state.asset_drag.label = asset.label.clone();
        state.asset_drag.thumbnail = asset.thumbnail.clone();
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            state.asset_drag.pos = [pos.x, pos.y];
        }
    }
    if card_resp.double_clicked() {
        // Convenience: double-click adds at playhead at the canvas centre.
        add_library_asset_at_playhead(state, asset, kind);
    }
    ui.add_space(2.0);
}

/// Spawn a scene element from a LibraryAsset at the current playhead /
/// canvas centre. Used by double-click — drag-drop has its own
/// handlers in `canvas_preview` and the timeline panel.
pub(crate) fn add_library_asset_at_playhead(
    state: &mut EditorState,
    asset: &crate::state::LibraryAsset,
    kind: AssetDragKind,
) {
    let t = state.playhead;
    let dur = state.scene.output.duration;
    match kind {
        AssetDragKind::Sound => {
            // Probe the audio file's duration so the new clip carries an
            // explicit `t_out`. Two things follow from this:
            //   1. The timeline auto-length pass (run every frame in
            //      `timeline()`) will now extend `scene.output.duration`
            //      to fit the sound when it's longer than the current
            //      timeline — same behaviour video clips already get.
            //   2. The lane-picker below can do a real range-overlap
            //      check instead of falling back to a single-point one.
            // `probe_video_duration` shells out to `ffprobe -show_entries
            // format=duration`, which works for any media container,
            // audio included.
            let clip_duration = probe_video_duration(&asset.path);
            let t_in = t.max(0.0);
            let t_out = t_in + clip_duration.max(0.1);
            state.scene.audio.push(memstroy_core::AudioTrack {
                id: asset.id.clone(),
                source: asset.path.clone(),
                t_in,
                t_out: Some(t_out),
                ..Default::default()
            });
            let new_idx = state.scene.audio.len() - 1;
            // Always pin the new sound onto its own free lane so it
            // never overlaps something already on the timeline; if
            // every existing audio lane is busy at this range (or
            // there are no audio lanes yet), insert a fresh A-lane
            // right after the video stack and use that. Same rule
            // canvas-dropped images / particles / video clips already
            // follow, applied to audio.
            let lane = state.pick_or_create_empty_audio_lane_for_range(t_in, t_out);
            state.audio_track_assignments.insert(new_idx, lane);
            state.selection = Selection::Audio(new_idx);
            state.status = format!("Added sound: {}", asset.id);
        }
        AssetDragKind::Image => {
            let overlay = Overlay::Image(ImageOverlay {
                id: asset.id.clone(),
                source: asset.path.clone(),
                t_in: t,
                t_out: (t + 3.0).min(dur),
                layout: vec![Keyframe::new(0.0, OverlayState::default())],
                modifiers: Vec::new(),
                skeleton_attachment: None,
                effects: Vec::new(),
                animated_params: Default::default(),
                chroma_key: None,
            });
            state.scene.overlays.push(overlay);
            let new_idx = state.scene.overlays.len() - 1;
            // Always land the new image overlay on its own lane so a
            // drop never silently replaces an image already on the
            // default overlay row.
            let lane = state.pick_or_create_empty_video_lane_at(t);
            state.overlay_track_assignments.insert(new_idx, lane);
            state.selection = Selection::Overlay(new_idx);
            state.status = format!("Added image: {}", asset.id);
        }
        AssetDragKind::Particle => {
            // Particle = image overlay with a spin + pulse + wobble preset
            // baked in so it looks alive on drop. Users are free to tune
            // or remove the modifiers from the inspector afterwards.
            let mut modifiers = Vec::new();
            modifiers.push(TrackModifier {
                t_start: 0.0,
                t_end: f32::MAX,
                enabled: true,
                kind: ModifierKind::Spin { speed_dps: 90.0 },
            });
            modifiers.push(TrackModifier {
                t_start: 0.0,
                t_end: f32::MAX,
                enabled: true,
                kind: ModifierKind::Pulse { freq_hz: 1.5, amp_scale: 0.15 },
            });
            modifiers.push(TrackModifier {
                t_start: 0.0,
                t_end: f32::MAX,
                enabled: true,
                kind: ModifierKind::Wobble {
                    freq_hz: 1.0, amp_x: 12.0, amp_y: 12.0, amp_rot_deg: 0.0, phase: 0.0,
                },
            });
            let overlay = Overlay::Image(ImageOverlay {
                id: format!("particle_{}", asset.id),
                source: asset.path.clone(),
                t_in: t,
                t_out: (t + 4.0).min(dur),
                layout: vec![Keyframe::new(0.0, OverlayState::default())],
                modifiers,
                skeleton_attachment: None,
                effects: Vec::new(),
                animated_params: Default::default(),
                chroma_key: None,
            });
            state.scene.overlays.push(overlay);
            let new_idx = state.scene.overlays.len() - 1;
            // Same rule as plain images — a particle drop must never
            // clobber an existing layer.
            let lane = state.pick_or_create_empty_video_lane_at(t);
            state.overlay_track_assignments.insert(new_idx, lane);
            state.selection = Selection::Overlay(new_idx);
            state.status = format!("Added particle: {}", asset.id);
        }
        AssetDragKind::Video => {
            // Treat a Video drop the same way as a Clip from the
            // Mellstroy library — spawn an actor from the path. The
            // helper handles per-clip chroma/skeleton sidecars and
            // creates a bound audio track.
            add_actor_from_clip(state, &asset.path);
        }
        AssetDragKind::Clip | AssetDragKind::None => {}
    }
}

fn clip_card(ui: &mut egui::Ui, state: &mut EditorState, clip: &crate::state::LibraryClip) {
    // Stretch to the full available width of the library column rather than
    // sizing to the inner row's content.
    let avail_w = ui.available_width().max(80.0);

    // Resolve a human-readable title once so the card and the placeholder
    // thumbnail share the same source of truth. The Telegram caption,
    // when present, is the canonical name; we fall back to a generic
    // "Untitled clip" string only when the sidecar truly has nothing —
    // numeric ids are deliberately NOT surfaced as the row title because
    // users complained about clips appearing as "цифры в названиях".
    let desc = clean_clip_text(&clip.description);
    let title: String = if desc.is_empty() {
        crate::i18n::t("Untitled clip").to_string()
    } else {
        desc
    };
    // First grapheme-ish letter of the title for the placeholder
    // thumbnail. Falls back to the channel-style hash when even that
    // is unavailable so the row is still distinguishable.
    let initial: String = title
        .chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());

    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 32, 48))
        .rounding(Rounding::same(4.0))
        .inner_margin(egui::Margin::same(3.0))
        .stroke(Stroke::new(1.0, Color32::from_rgb(50, 50, 70)));

    let card_resp = frame.show(ui, |ui| {
        ui.set_min_width(avail_w - 6.0); // account for inner_margin

        ui.horizontal(|ui| {
            let thumb_size = Vec2::new(48.0, 48.0);
            if let Some(thumb) = &clip.thumbnail {
                let uri = format!("file://{}", thumb.display());
                ui.add(
                    egui::Image::from_uri(uri)
                        .fit_to_exact_size(thumb_size)
                        .maintain_aspect_ratio(false)
                        .rounding(Rounding::same(3.0)),
                );
            } else {
                // No thumbnail yet — paint a tinted placeholder with the
                // first letter of the caption so the card remains
                // recognisable at a glance instead of just being a grey
                // square with a numeric id.
                let (rect, _) = ui.allocate_exact_size(thumb_size, Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 55));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &initial,
                    egui::FontId::proportional(20.0),
                    Color32::from_rgb(190, 160, 220),
                );
            }

            // Vertical text column claims the rest of the available width
            // so that even short labels don't shrink the card.
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), thumb_size.y),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.set_min_width(ui.available_width());
                    // The Telegram caption gets the prominent slot. The
                    // numeric id used to live above it but cluttered the
                    // row — moved into a hover tooltip so power users can
                    // still see it without it dominating the panel.
                    let label_resp = ui.add(
                        egui::Label::new(
                            RichText::new(&title).size(11.5).color(COL_TEXT),
                        )
                        .truncate(),
                    );
                    label_resp.on_hover_text(format!("#{}", clip.id));
                },
            );
        });
    }).response;

    // Whole-card click + drag handling. The card is the drag source for the
    // timeline (drop target inside the timeline area decides what happens).
    let card_resp = card_resp.interact(Sense::click_and_drag());
    if card_resp.dragged() {
        state.asset_drag.dragging = Some(clip.path.clone());
        state.asset_drag.kind = AssetDragKind::Clip;
        state.asset_drag.label = clip_drag_label(clip);
        state.asset_drag.thumbnail = clip.thumbnail.clone();
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            state.asset_drag.pos = [pos.x, pos.y];
        }
    }
    if card_resp.double_clicked() {
        // Convenience: double-click adds at playhead without needing to drag.
        add_actor_from_clip(state, &clip.path);
    }
    ui.add_space(2.0);
}

/// Compact human-readable label for a clip (used by the drag preview).
fn clip_drag_label(clip: &crate::state::LibraryClip) -> String {
    let desc = clean_clip_text(&clip.description);
    if desc.is_empty() {
        crate::i18n::t("Untitled clip").to_string()
    } else if desc.chars().count() > 28 {
        format!("{}\u{2026}", desc.chars().take(26).collect::<String>())
    } else {
        desc
    }
}


// ─── INSPECTOR ───────────────────────────────────────────────────────

pub fn inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(crate::i18n::t("Inspector")).size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if state.eyedropper_active {
                ui.label(RichText::new("PICK").size(10.0).color(Color32::from_rgb(255, 200, 50)));
            }
        });
    });
    ui.separator();
    ui.add_space(4.0);

    // Wrap everything below the header in a vertical scroll area so the
    // inspector remains usable when a layer has more parameters than the
    // panel can show in one go (long Effects lists, animated_params,
    // etc.). `auto_shrink([true, false])` lets the panel shrink
    // HORIZONTALLY when the user drags the divider in (the previous
    // `[false; 2]` pinned the inner content to the panel's available
    // width AND propagated the content's intrinsic min back outward,
    // which combined with `slider_width = avail - 88` produced a
    // self-reinforcing growth loop — once stretched, the panel could
    // not be dragged narrower without immediately popping back open).
    egui::ScrollArea::vertical()
        .id_source("inspector_scroll")
        .auto_shrink([true, false])
        .show(ui, |ui| {
            // Cap the inner content's max width so a long DragValue /
            // ComboBox / Slider row cannot push the parent SidePanel
            // wider than its own `width_range`. Without this cap, egui
            // honours the inner UI's measured min-width and the panel
            // grows past 620 px and refuses to shrink back.
            let avail = ui.available_width();
            ui.set_max_width(avail);
            // Reserve a sane slider width for parameter rows. The
            // previous `(avail - 88).max(140)` formula assumed no
            // sibling widgets on the row; in practice every animatable
            // row contains: diamond toggle (~14 px) + label (~80 px) +
            // slider + DragValue (~64 px) + small spacing (~24 px),
            // so the slider's intrinsic min was `avail` itself —
            // that's what made the panel grow on every frame.
            // Cap at a smaller value so the row's measured min is
            // always strictly less than `avail`.
            ui.spacing_mut().slider_width =
                (avail * 0.55).clamp(110.0, 240.0);
            inspector_body(ui, state);
        });
}

fn inspector_body(ui: &mut egui::Ui, state: &mut EditorState) {
    // ── Multi-select short-circuit ──
    // When the user has lassoed more than one element on the canvas (or
    // Ctrl-clicked clips on the timeline), the inspector switches into
    // a relative-edit mode. We display widgets for the parameters that
    // are common to every element in the set (position, scale,
    // rotation, opacity, flip), but their semantics is *delta*: edits
    // are broadcast as offsets / multipliers to every member, so two
    // elements with different starting positions both move by the same
    // amount instead of snapping to identical absolute values.
    if state.canvas_selection.len() > 1 {
        inspector_multiselect(ui, state);
        return;
    }

    match state.selection {
        Selection::None => {
            inspector_nothing(ui, state);
        }
        Selection::Actor(i) => {
            if i < state.scene.actors.len() {
                inspector_actor(ui, state, i);
            }
        }
        Selection::Overlay(i) => {
            if i < state.scene.overlays.len() {
                inspector_overlay(ui, state, i);
            }
        }
        Selection::Background(i) => {
            if i < state.scene.backgrounds.len() {
                inspector_background(ui, state, i);
            }
        }
        Selection::Audio(i) => {
            if i < state.scene.audio.len() {
                inspector_audio(ui, state, i);
            }
        }
        Selection::Camera(_) => {
            ui.label(t("Camera editing coming soon."));
        }
        Selection::RenderFrame => {
            inspector_render_frame(ui, state);
        }
    }
}


fn inspector_nothing(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.add_space(20.0);
    ui.label(RichText::new(crate::i18n::t("Select a clip on the timeline")).italics().color(COL_TEXT_DIM).size(13.0));
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);

    // Output settings — fixed 1080x1920 9:16 short format.
    // FPS and duration are intentionally not user-editable here: FPS is
    // pinned by the format and the scene's duration grows automatically
    // to fit whatever is on the timeline.
    ui.label(RichText::new(t("Output")).size(14.0).strong().color(Color32::from_rgb(100, 200, 255)));
    ui.add_space(4.0);
    ui.label(RichText::new("1080x1920 (9:16)").size(12.0).color(COL_TEXT_DIM));
    ui.add_space(4.0);

    let _ = state; // currently unused beyond the labels
}

/// Multi-select inspector. Shows the parameters that are common to
/// every element in `state.canvas_selection`, with edits applied as
/// *deltas* (offset for position / rotation / opacity, multiplier for
/// scale) so each element keeps its individual starting value relative
/// to the group. Only the focused (`state.selection`) element drives
/// the displayed slider position; while the user drags, every selected
/// element moves by the same delta as the focused one.
fn inspector_multiselect(ui: &mut egui::Ui, state: &mut EditorState) {
    let n = state.canvas_selection.len();
    let playhead = state.playhead;

    // Header.
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} {}", n, t("elements selected")))
                .size(14.0)
                .strong()
                .color(Color32::from_rgb(255, 200, 80)),
        );
        if ui.small_button("Esc").on_hover_text(t("Clear multi-selection")).clicked() {
            state.canvas_selection.clear();
        }
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new(t("Edits below are applied as deltas to every element."))
            .italics()
            .size(10.0)
            .color(COL_TEXT_DIM),
    );
    ui.add_space(8.0);

    // Persistent "session" values. Each accumulates as the user drags
    // the corresponding widget across frames; on every change we
    // compute the delta vs the previous frame's value and broadcast it.
    let pos_x_id = ui.make_persistent_id("multi_pos_x");
    let pos_y_id = ui.make_persistent_id("multi_pos_y");
    let scale_id = ui.make_persistent_id("multi_scale");
    let rot_id = ui.make_persistent_id("multi_rot");
    let op_id = ui.make_persistent_id("multi_op");

    let mut pos_x_last: f32 = ui.data(|d| d.get_temp(pos_x_id).unwrap_or(0.0_f32));
    let mut pos_y_last: f32 = ui.data(|d| d.get_temp(pos_y_id).unwrap_or(0.0_f32));
    let mut scale_last: f32 = ui.data(|d| d.get_temp(scale_id).unwrap_or(1.0_f32));
    let mut rot_last: f32 = ui.data(|d| d.get_temp(rot_id).unwrap_or(0.0_f32));
    let mut op_last: f32 = ui.data(|d| d.get_temp(op_id).unwrap_or(0.0_f32));

    // ── Position ──
    ui.label(RichText::new(t("Position")).size(11.0).strong());
    ui.horizontal(|ui| {
        ui.label("ΔX:");
        let mut cur = pos_x_last;
        let r = ui.add(egui::DragValue::new(&mut cur).speed(0.005).fixed_decimals(3));
        if r.changed() {
            let delta = cur - pos_x_last;
            if delta.abs() > 1.0e-7 {
                multi_apply_pos_delta(state, delta, 0.0, playhead);
            }
            pos_x_last = cur;
            ui.data_mut(|d| d.insert_temp(pos_x_id, pos_x_last));
        }
        ui.label("ΔY:");
        let mut cur = pos_y_last;
        let r = ui.add(egui::DragValue::new(&mut cur).speed(0.005).fixed_decimals(3));
        if r.changed() {
            let delta = cur - pos_y_last;
            if delta.abs() > 1.0e-7 {
                multi_apply_pos_delta(state, 0.0, delta, playhead);
            }
            pos_y_last = cur;
            ui.data_mut(|d| d.insert_temp(pos_y_id, pos_y_last));
        }
        if ui.small_button(t("Reset")).clicked() {
            ui.data_mut(|d| {
                d.insert_temp(pos_x_id, 0.0_f32);
                d.insert_temp(pos_y_id, 0.0_f32);
            });
        }
    });

    ui.add_space(4.0);

    // ── Scale (multiplicative) ──
    ui.label(RichText::new(t("Scale (multiplier)")).size(11.0).strong());
    ui.horizontal(|ui| {
        ui.label("×:");
        let mut cur = scale_last.max(0.01);
        let r = ui.add(egui::Slider::new(&mut cur, 0.1..=10.0).logarithmic(true));
        if r.changed() && cur > 0.0 {
            let factor = cur / scale_last.max(0.0001);
            if (factor - 1.0).abs() > 1.0e-5 {
                multi_apply_scale_factor(state, factor, playhead);
            }
            scale_last = cur;
            ui.data_mut(|d| d.insert_temp(scale_id, scale_last));
        }
        if ui.small_button(t("Reset")).clicked() {
            ui.data_mut(|d| d.insert_temp(scale_id, 1.0_f32));
        }
    });

    ui.add_space(4.0);

    // ── Rotation (additive degrees) ──
    ui.label(RichText::new(t("Rotation")).size(11.0).strong());
    ui.horizontal(|ui| {
        ui.label("Δ\u{00B0}:");
        let mut cur = rot_last;
        let r = ui.add(
            egui::DragValue::new(&mut cur)
                .range(-3600.0..=3600.0)
                .speed(0.5)
                .suffix("\u{00B0}"),
        );
        if r.changed() {
            let delta = cur - rot_last;
            if delta.abs() > 1.0e-4 {
                multi_apply_rotation_delta(state, delta, playhead);
            }
            rot_last = cur;
            ui.data_mut(|d| d.insert_temp(rot_id, rot_last));
        }
        if ui.small_button(t("Reset")).clicked() {
            ui.data_mut(|d| d.insert_temp(rot_id, 0.0_f32));
        }
    });

    ui.add_space(4.0);

    // ── Opacity (additive 0..1) ──
    ui.label(RichText::new(t("Opacity")).size(11.0).strong());
    ui.horizontal(|ui| {
        ui.label("Δ:");
        let mut cur = op_last;
        let r = ui.add(egui::Slider::new(&mut cur, -1.0..=1.0));
        if r.changed() {
            let delta = cur - op_last;
            if delta.abs() > 1.0e-5 {
                multi_apply_opacity_delta(state, delta, playhead);
            }
            op_last = cur;
            ui.data_mut(|d| d.insert_temp(op_id, op_last));
        }
        if ui.small_button(t("Reset")).clicked() {
            ui.data_mut(|d| d.insert_temp(op_id, 0.0_f32));
        }
    });

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // Quick toggles broadcasting absolute values (these don't need a
    // delta semantic — Visible / Flip are boolean enough to apply uniformly).
    ui.horizontal(|ui| {
        if ui.button(t("Flip X all")).on_hover_text(t("Toggle horizontal flip on every selected element")).clicked() {
            multi_toggle_flip_x(state, playhead);
        }
        if ui.button(t("Flip Y all")).clicked() {
            multi_toggle_flip_y(state, playhead);
        }
    });
}

/// Apply a position delta (in normalised scene coords) to every
/// element in `state.canvas_selection`, writing through the same
/// keyframe path that single-element edits use so the per-param
/// animation toggles continue to behave correctly.
fn multi_apply_pos_delta(state: &mut EditorState, dx: f32, dy: f32, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    if dx.abs() > 1.0e-7 {
                        kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                            playhead, param_ids::POS_X, false,
                            |s| s.pos[0] += dx);
                    }
                    if dy.abs() > 1.0e-7 {
                        kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                            playhead, param_ids::POS_Y, false,
                            |s| s.pos[1] += dy);
                    }
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    if dx.abs() > 1.0e-7 {
                        kf_anim::write_overlay_param(layout, animated,
                            playhead, param_ids::POS_X, false,
                            |s| s.pos[0] += dx);
                    }
                    if dy.abs() > 1.0e-7 {
                        kf_anim::write_overlay_param(layout, animated,
                            playhead, param_ids::POS_Y, false,
                            |s| s.pos[1] += dy);
                    }
                }
            }
            _ => {}
        }
    }
}

fn multi_apply_scale_factor(state: &mut EditorState, factor: f32, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                        playhead, param_ids::SCALE, false,
                        |s| s.scale = (s.scale * factor).clamp(0.01, 50.0));
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    kf_anim::write_overlay_param(layout, animated,
                        playhead, param_ids::SCALE, false,
                        |s| s.scale = (s.scale * factor).clamp(0.01, 50.0));
                }
            }
            _ => {}
        }
    }
}

fn multi_apply_rotation_delta(state: &mut EditorState, ddeg: f32, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                        playhead, param_ids::ROTATION, false,
                        |s| s.rotation_deg += ddeg);
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    kf_anim::write_overlay_param(layout, animated,
                        playhead, param_ids::ROTATION, false,
                        |s| s.rotation_deg += ddeg);
                }
            }
            _ => {}
        }
    }
}

fn multi_apply_opacity_delta(state: &mut EditorState, dop: f32, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                        playhead, param_ids::OPACITY, false,
                        |s| s.opacity = (s.opacity + dop).clamp(0.0, 1.0));
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    kf_anim::write_overlay_param(layout, animated,
                        playhead, param_ids::OPACITY, false,
                        |s| s.opacity = (s.opacity + dop).clamp(0.0, 1.0));
                }
            }
            _ => {}
        }
    }
}

fn multi_toggle_flip_x(state: &mut EditorState, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                        playhead, param_ids::FLIP_X, false,
                        |s| s.flip_x_anim = -s.flip_x_anim);
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    kf_anim::write_overlay_param(layout, animated,
                        playhead, param_ids::FLIP_X, false,
                        |s| s.flip_x_anim = -s.flip_x_anim);
                }
            }
            _ => {}
        }
    }
}

fn multi_toggle_flip_y(state: &mut EditorState, playhead: f32) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let targets: Vec<Selection> = state.canvas_selection.clone();
    for sel in targets {
        match sel {
            Selection::Actor(ai) => {
                if let Some(a) = state.scene.actors.get_mut(ai) {
                    kf_anim::write_actor_param(&mut a.layout, &mut a.animated_params,
                        playhead, param_ids::FLIP_Y, false,
                        |s| s.flip_y_anim = -s.flip_y_anim);
                }
            }
            Selection::Overlay(oi) => {
                if let Some(ov) = state.scene.overlays.get_mut(oi) {
                    let (layout, animated) = overlay_layout_and_params(ov);
                    kf_anim::write_overlay_param(layout, animated,
                        playhead, param_ids::FLIP_Y, false,
                        |s| s.flip_y_anim = -s.flip_y_anim);
                }
            }
            _ => {}
        }
    }
}

/// Helper: get a mutable reference to an overlay's layout vector and
/// animated_params set (the three Overlay variants store them under
/// different field names, but every variant has both).
fn overlay_layout_and_params(
    ov: &mut Overlay,
) -> (
    &mut Vec<Keyframe<OverlayState>>,
    &mut std::collections::BTreeSet<String>,
) {
    match ov {
        Overlay::Text(t) => (&mut t.layout, &mut t.animated_params),
        Overlay::Image(im) => (&mut im.layout, &mut im.animated_params),
        Overlay::Video(v) => (&mut v.layout, &mut v.animated_params),
    }
}

fn inspector_actor(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let actor_count = state.scene.actors.len();
    let cache_count = state.frame_caches.len();

    // Header with name (delete button removed — use Delete/Backspace shortcut
    // or right-click on the timeline clip instead).
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{}: {}", t("Actor"), state.scene.actors[i].id))
            .strong().size(14.0).color(COL_CLIP_ACTOR));
    });
    ui.add_space(2.0);
    ui.label(RichText::new(
        state.scene.actors[i].source.file_name().and_then(|s| s.to_str()).unwrap_or("(source)")
    ).size(10.0).color(COL_TEXT_DIM));
    ui.add_space(6.0);

    // Tab bar: Transform | Masks | Effects
    ui.horizontal(|ui| {
        if ui.selectable_label(state.inspector_tab == 0, t("Transform")).clicked() { state.inspector_tab = 0; }
        if ui.selectable_label(state.inspector_tab == 1, t("Masks")).clicked() { state.inspector_tab = 1; }
        if ui.selectable_label(state.inspector_tab == 2, t("Effects")).clicked() { state.inspector_tab = 2; }
    });
    ui.separator();
    ui.add_space(4.0);

    match state.inspector_tab {
        0 => {
            inspector_actor_transform(ui, state, i);
            inspector_actor_speed(ui, state, i);
        }
        1 => inspector_actor_masks(ui, state, i),
        2 => inspector_actor_effects(ui, state, i, actor_count, cache_count),
        _ => {
            inspector_actor_transform(ui, state, i);
            inspector_actor_speed(ui, state, i);
        }
    }
}


// ─── Per-parameter keyframe strip helpers (transform layouts) ────────
//
// Two thin helpers that adapt the generic `kf_anim::keyframe_strip`
// widget to the actor / overlay shared layout vector. They only
// render when the param is in `animated_params` AND the layout
// actually has multiple kfs whose value for THIS param differs.
// Drag horizontal moves the kf time; right-click opens the easing
// menu. Click is a no-op (the timeline ruler is the place to
// navigate; the inspector strip is for editing).

fn inspector_actor_param_strip<F>(
    ui: &mut egui::Ui,
    layout: &mut Vec<Keyframe<memstroy_core::ActorState>>,
    is_animated: bool,
    t_in_scene: f32,
    playhead_scene: f32,
    get: F,
    salt: impl std::hash::Hash + Copy,
) where
    F: Fn(&memstroy_core::ActorState) -> f32,
{
    if !is_animated || layout.len() < 2 {
        return;
    }
    const EPS: f32 = 1.0e-4;
    // Pick the kf indices where the param's value actually changes.
    // The first kf is always shown so the user has an anchor at t=0.
    let mut indices: Vec<usize> = Vec::with_capacity(layout.len());
    indices.push(0);
    for i in 1..layout.len() {
        if (get(&layout[i].value) - get(&layout[i - 1].value)).abs() > EPS {
            indices.push(i);
        }
    }

    // Convert scene-time → clip-local for display so every transform
    // strip starts at 0 like the audio param strips do. The drag path
    // converts back when writing.
    let times_local: Vec<f32> = indices
        .iter()
        .map(|&i| (layout[i].t - t_in_scene).max(0.0))
        .collect();
    let easings: Vec<memstroy_core::Easing> =
        indices.iter().map(|&i| layout[i].easing).collect();

    let playhead_local = (playhead_scene - t_in_scene).max(0.0);
    let max_kf_t = times_local.iter().cloned().fold(0.0_f32, f32::max);
    let dur = max_kf_t.max(playhead_local).max(1.0);

    let interaction = crate::kf_anim::keyframe_strip(
        ui,
        &times_local,
        &easings,
        dur,
        Some(playhead_local),
        salt,
    );

    if let Some((strip_idx, new_t_local)) = interaction.dragged_idx_to {
        if let Some(&kf_idx) = indices.get(strip_idx) {
            layout[kf_idx].t = (new_t_local + t_in_scene).max(0.0);
            layout
                .sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    if let Some((strip_idx, easing)) = interaction.easing_changed {
        if let Some(&kf_idx) = indices.get(strip_idx) {
            layout[kf_idx].easing = easing;
        }
    }
}

fn inspector_overlay_param_strip<F>(
    ui: &mut egui::Ui,
    layout: &mut Vec<Keyframe<memstroy_core::OverlayState>>,
    is_animated: bool,
    playhead_local: f32,
    get: F,
    salt: impl std::hash::Hash + Copy,
) where
    F: Fn(&memstroy_core::OverlayState) -> f32,
{
    if !is_animated || layout.len() < 2 {
        return;
    }
    const EPS: f32 = 1.0e-4;
    let mut indices: Vec<usize> = Vec::with_capacity(layout.len());
    indices.push(0);
    for i in 1..layout.len() {
        if (get(&layout[i].value) - get(&layout[i - 1].value)).abs() > EPS {
            indices.push(i);
        }
    }
    let times: Vec<f32> = indices.iter().map(|&i| layout[i].t.max(0.0)).collect();
    let easings: Vec<memstroy_core::Easing> =
        indices.iter().map(|&i| layout[i].easing).collect();

    let max_kf_t = times.iter().cloned().fold(0.0_f32, f32::max);
    let dur = max_kf_t.max(playhead_local).max(1.0);

    let interaction = crate::kf_anim::keyframe_strip(
        ui,
        &times,
        &easings,
        dur,
        Some(playhead_local.max(0.0)),
        salt,
    );

    if let Some((strip_idx, new_t)) = interaction.dragged_idx_to {
        if let Some(&kf_idx) = indices.get(strip_idx) {
            layout[kf_idx].t = new_t.max(0.0);
            layout
                .sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    if let Some((strip_idx, easing)) = interaction.easing_changed {
        if let Some(&kf_idx) = indices.get(strip_idx) {
            layout[kf_idx].easing = easing;
        }
    }
}

/// Per-param keyframe strip for the render frame inspector.
/// REMOVED: the render-frame keyframe strips moved out of the
/// inspector and onto the dedicated "Render Frame" row of the layer
/// panel — see `render_frame_animated_params` /
/// `render_frame_expansion` and the strip-render branch inside the
/// timeline RF row block. Kept this comment as a breadcrumb so a
/// future contributor doesn't reintroduce a duplicate.


fn inspector_actor_transform(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    use crate::kf_anim;
    use memstroy_core::param_ids;

    let playhead = state.playhead;
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new(t("Position & Scale")).size(12.0).strong());
    ui.add_space(4.0);

    // Sample the eased current value at the playhead — this is read-only
    // and never mutates `layout`. The widget below is bound to a temp
    // copy, and only `.changed()` triggers a write through `kf_anim`.
    let cur = kf_anim::sample_actor(&a.layout, playhead);

    let kf_count = a.layout.len();
    if kf_count <= 1 {
        ui.label(
            RichText::new(t("Static value (no keyframes yet)"))
                .size(9.0).color(COL_TEXT_DIM).italics(),
        );
    } else {
        ui.label(
            RichText::new(format!("{} keyframes \u{2022} {} animated params",
                kf_count, a.animated_params.len()))
                .size(9.0).color(COL_TEXT_DIM).italics(),
        );
    }

    let highlight = state.kf_highlight.clone();

    // Scene-time anchor for strip-time → scene-time conversions. Actor
    // kfs are stored in scene-time, but the strip displays clip-local
    // for consistency with the audio param strips and the timeline
    // ruler beneath the layer.
    let t_in_scene = a.t_in.unwrap_or(0.0);

    // ── Position X / Y ──
    let mut new_x = cur.pos[0];
    let mut new_y = cur.pos[1];
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::POS_X, ("act_pos_x", i));
        ui.label(param_label(highlight.is_active(param_ids::POS_X), t("X:")));
        let r = ui.add(egui::DragValue::new(&mut new_x).range(-2.0..=3.0).speed(0.005));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::POS_X, false,
                |s| s.pos[0] = new_x);
        }
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::POS_Y, ("act_pos_y", i));
        ui.label(param_label(highlight.is_active(param_ids::POS_Y), t("Y:")));
        let r = ui.add(egui::DragValue::new(&mut new_y).range(-2.0..=3.0).speed(0.005));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::POS_Y, false,
                |s| s.pos[1] = new_y);
        }
    });
    let pos_x_anim = a.animated_params.contains(param_ids::POS_X);
    let pos_y_anim = a.animated_params.contains(param_ids::POS_Y);
    inspector_actor_param_strip(
        ui, &mut a.layout, pos_x_anim, t_in_scene, playhead,
        |s| s.pos[0], ("act_strip_pos_x", i),
    );
    inspector_actor_param_strip(
        ui, &mut a.layout, pos_y_anim, t_in_scene, playhead,
        |s| s.pos[1], ("act_strip_pos_y", i),
    );

    // ── Scale ──
    let mut new_scale = cur.scale;
    let mut new_scale_y = cur.scale_y;
    // Per-actor "lock" between Scale X and Scale Y. Default = LOCKED so
    // proportional scaling is the out-of-the-box behaviour. The user
    // unlocks via the chain glyph next to the X slider.
    let lock_id = ui.make_persistent_id(("actor_scale_lock", i));
    let mut linked: bool = ui.data(|d| d.get_temp(lock_id).unwrap_or(true));

    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::SCALE, ("act_scale", i));
        ui.label(param_label(highlight.is_active(param_ids::SCALE), "Scale X:"));
        let r = ui.add(egui::Slider::new(&mut new_scale, 0.05..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::SCALE, false,
                |s| s.scale = new_scale);
            if linked {
                new_scale_y = 1.0;
                kf_anim::write_actor_param(
                    &mut a.layout, &mut a.animated_params, playhead,
                    param_ids::SCALE_Y, false,
                    |s| s.scale_y = 1.0);
            }
        }
        let chain = if linked { "\u{1F517}" } else { "\u{26D3}" };
        if ui.small_button(chain)
            .on_hover_text(if linked {
                "Scale X and Scale Y are linked — click to unlink"
            } else {
                "Scale X and Scale Y are independent — click to link"
            })
            .clicked()
        {
            linked = !linked;
            ui.data_mut(|d| d.insert_temp(lock_id, linked));
            if linked {
                new_scale_y = 1.0;
                kf_anim::write_actor_param(
                    &mut a.layout, &mut a.animated_params, playhead,
                    param_ids::SCALE_Y, false,
                    |s| s.scale_y = 1.0);
            }
        } else {
            ui.data_mut(|d| d.insert_temp(lock_id, linked));
        }
    });

    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::SCALE_Y, ("act_sy", i));
        ui.label(param_label(highlight.is_active(param_ids::SCALE_Y), "Scale Y:"))
            .on_hover_text("Independent Y-axis scale. Linked to Scale X by default.");
        if linked {
            // ── Linked mode: the Y slider mirrors the uniform Scale X.
            //
            // The previous code wrote `scale_y = 1.0` *and* `scale =
            // new_sy * cur.scale` on every frame. Because `new_sy` was
            // taken from `cur.scale_y` (which we kept resetting to 1.0)
            // the displayed value was always 1.0 — but the slider
            // detected each tiny pointer motion as a fresh change of
            // scale_y, so each frame multiplied scale by ~1.01 and the
            // image grew without bound, eventually filling the canvas
            // (the "Scale Y bug fills background" report).
            //
            // Correct behaviour: in linked mode the slider value IS the
            // uniform scale. Drag → set scale to that value, scale_y
            // stays at 1.0.
            let mut linked_scale = cur.scale;
            let r = ui.add(egui::Slider::new(&mut linked_scale, 0.05..=5.0).logarithmic(true));
            if r.changed() && linked_scale.is_finite() && linked_scale > 0.0 {
                kf_anim::write_actor_param(
                    &mut a.layout, &mut a.animated_params, playhead,
                    param_ids::SCALE, false,
                    |s| s.scale = linked_scale);
                kf_anim::write_actor_param(
                    &mut a.layout, &mut a.animated_params, playhead,
                    param_ids::SCALE_Y, false,
                    |s| s.scale_y = 1.0);
            }
        } else {
            let r = ui.add(egui::Slider::new(&mut new_scale_y, 0.1..=5.0).logarithmic(true));
            if r.changed() {
                kf_anim::write_actor_param(
                    &mut a.layout, &mut a.animated_params, playhead,
                    param_ids::SCALE_Y, false,
                    |s| s.scale_y = new_scale_y);
            }
        }
    });
    let scale_anim = a.animated_params.contains(param_ids::SCALE);
    let scale_y_anim = a.animated_params.contains(param_ids::SCALE_Y);
    inspector_actor_param_strip(
        ui, &mut a.layout, scale_anim, t_in_scene, playhead,
        |s| s.scale, ("act_strip_scale", i),
    );
    inspector_actor_param_strip(
        ui, &mut a.layout, scale_y_anim, t_in_scene, playhead,
        |s| s.scale_y, ("act_strip_scale_y", i),
    );

    // ── Rotation (dial + numeric) ──
    let mut new_rot = cur.rotation_deg;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::ROTATION, ("act_rot", i));
        ui.label(param_label(highlight.is_active(param_ids::ROTATION), "Rotation"));
        let prev_rot = new_rot;
        circular_rotation_widget(ui, ("actor_rot", i), &mut new_rot, 90.0);
        let mut dial_changed = (new_rot - prev_rot).abs() > 1.0e-4;
        ui.vertical(|ui| {
            let r = ui.add(
                egui::DragValue::new(&mut new_rot)
                    .range(-3600.0..=3600.0)
                    .speed(0.5)
                    .suffix("\u{00B0}")
                    .fixed_decimals(1),
            );
            if r.changed() { dial_changed = true; }
            if ui.small_button("0\u{00B0}").clicked() {
                new_rot = 0.0;
                dial_changed = true;
            }
        });
        if dial_changed {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::ROTATION, false,
                |s| s.rotation_deg = new_rot);
        }
    });
    let rot_anim = a.animated_params.contains(param_ids::ROTATION);
    inspector_actor_param_strip(
        ui, &mut a.layout, rot_anim, t_in_scene, playhead,
        |s| s.rotation_deg, ("act_strip_rot", i),
    );

    // ── Opacity ──
    let mut new_op = cur.opacity;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::OPACITY, ("act_op", i));
        ui.label(param_label(highlight.is_active(param_ids::OPACITY), "Opacity:"));
        let r = ui.add(egui::Slider::new(&mut new_op, 0.0..=1.0));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::OPACITY, false,
                |s| s.opacity = new_op);
        }
    });
    let op_anim = a.animated_params.contains(param_ids::OPACITY);
    inspector_actor_param_strip(
        ui, &mut a.layout, op_anim, t_in_scene, playhead,
        |s| s.opacity, ("act_strip_op", i),
    );

    // ── Flip X / Y ──
    let mut new_fx = cur.flip_x_anim;
    let mut new_fy = cur.flip_y_anim;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::FLIP_X, ("act_fx", i));
        ui.label(param_label(highlight.is_active(param_ids::FLIP_X), "Flip X:"));
        let r = ui.add(egui::Slider::new(&mut new_fx, -1.0..=1.0));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::FLIP_X, false,
                |s| s.flip_x_anim = new_fx);
        }
        // Mirror (\u{21B6}) shortcut button intentionally removed — drag the slider to -1 manually.
    });
    let flip_x_anim = a.animated_params.contains(param_ids::FLIP_X);
    inspector_actor_param_strip(
        ui, &mut a.layout, flip_x_anim, t_in_scene, playhead,
        |s| s.flip_x_anim, ("act_strip_flip_x", i),
    );
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::FLIP_Y, ("act_fy", i));
        ui.label(param_label(highlight.is_active(param_ids::FLIP_Y), "Flip Y:"));
        let r = ui.add(egui::Slider::new(&mut new_fy, -1.0..=1.0));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::FLIP_Y, false,
                |s| s.flip_y_anim = new_fy);
        }
    });
    let flip_y_anim = a.animated_params.contains(param_ids::FLIP_Y);
    inspector_actor_param_strip(
        ui, &mut a.layout, flip_y_anim, t_in_scene, playhead,
        |s| s.flip_y_anim, ("act_strip_flip_y", i),
    );

    ui.add_space(8.0);
    ui.checkbox(&mut a.visible, "Visible");

    // Animation modifiers — wobble / shake / pulse / spin.
    ui.add_space(8.0);
    inspector_modifiers(ui, &mut a.modifiers, ("actor_mods", i));
}

/// Color a param label gold when its kf was just clicked from the timeline.
/// The label is automatically run through `i18n::t()` so call sites can
/// keep passing English source strings.
fn param_label(highlighted: bool, text: &'static str) -> RichText {
    let translated = t(text);
    if highlighted {
        RichText::new(translated).size(11.0).strong()
            .color(Color32::from_rgb(255, 220, 80))
            .background_color(Color32::from_rgba_premultiplied(80, 60, 0, 80))
    } else {
        RichText::new(translated).size(11.0)
    }
}

/// Same visual treatment as [`param_label`] but takes a runtime `&str`
/// the caller has already translated. Used by the audio inspector
/// helpers, where labels like `"Pitch (semitones)"` are composed via
/// `i18n::t(...)` at the call site (returning a `&'static str` that
/// shouldn't be translated again, or a `String` for keys with runtime
/// formatting). Mirrors the gold-highlight when a timeline kf was just
/// clicked so the user can trace which audio param a kf belongs to —
/// the same affordance the actor / overlay / render-frame inspectors
/// already give for video params.
fn param_label_str(highlighted: bool, text: &str) -> RichText {
    if highlighted {
        RichText::new(text).size(11.0).strong()
            .color(Color32::from_rgb(255, 220, 80))
            .background_color(Color32::from_rgba_premultiplied(80, 60, 0, 80))
    } else {
        RichText::new(text).size(11.0)
    }
}

/// Compact circular rotation dial. The pointer angle relative to the
/// dial centre is mapped directly to `*deg` so the user always lands on
/// the exact value they aim at — no slider scrubbing needed. Double-
/// clicking resets to 0°. Holding Shift snaps to 15° increments.
fn circular_rotation_widget(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + Copy,
    deg: &mut f32,
    size: f32,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let radius = size * 0.45;

    // Background ring.
    painter.circle_filled(center, radius, Color32::from_rgb(28, 28, 38));
    painter.circle_stroke(center, radius, Stroke::new(1.0, Color32::from_rgb(70, 70, 90)));

    // Tick marks every 30°.
    for k in 0..12 {
        let a = (k as f32) / 12.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let p1 = center + Vec2::new(a.cos(), a.sin()) * (radius - 4.0);
        let p2 = center + Vec2::new(a.cos(), a.sin()) * radius;
        let stroke = if k % 3 == 0 { 1.5 } else { 0.7 };
        painter.line_segment([p1, p2], Stroke::new(stroke, Color32::from_rgb(110, 110, 130)));
    }

    let _ = salt;

    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let dx = p.x - center.x;
            let dy = p.y - center.y;
            if dx.abs() > 0.001 || dy.abs() > 0.001 {
                // Convention: 0° points up; positive rotates clockwise so
                // the dial reads like a wall clock.
                let mut new_deg = dy.atan2(dx).to_degrees() + 90.0;
                if new_deg > 180.0 { new_deg -= 360.0; }
                if new_deg < -180.0 { new_deg += 360.0; }
                let shift_held = ui.input(|i| i.modifiers.shift);
                if shift_held {
                    new_deg = (new_deg / 15.0).round() * 15.0;
                }
                *deg = new_deg;
            }
        }
    }
    if resp.double_clicked() {
        *deg = 0.0;
    }

    // Marker line from centre to the current angle.
    let rad = deg.to_radians() - std::f32::consts::FRAC_PI_2;
    let tip = center + Vec2::new(rad.cos(), rad.sin()) * (radius - 4.0);
    painter.line_segment([center, tip], Stroke::new(2.5, Color32::from_rgb(255, 220, 80)));
    painter.circle_filled(tip, 4.0, Color32::from_rgb(255, 220, 80));
    painter.circle_filled(center, 3.0, Color32::from_rgb(180, 180, 200));

    // Numeric readout in the centre.
    painter.text(
        Pos2::new(center.x, center.y + radius * 0.5),
        egui::Align2::CENTER_CENTER,
        format!("{:.1}\u{00B0}", deg),
        egui::FontId::proportional(10.0),
        Color32::from_rgb(200, 200, 220),
    );
}

/// Modifier-stack inspector. Shows each modifier as an editable card
/// with its parameters and an "x" remove button. The "+ Add" menu adds
/// a new modifier of a chosen kind. Empty list shows a hint instead of
/// a tall blank panel.
fn inspector_modifiers(
    ui: &mut egui::Ui,
    modifiers: &mut Vec<TrackModifier>,
    salt: impl std::hash::Hash + Copy,
) {
    egui::CollapsingHeader::new(
        RichText::new(t("Animation Modifiers")).size(12.0).strong()
            .color(Color32::from_rgb(150, 200, 255)),
    )
    .id_source(("modifier_collapse", salt))
    .default_open(false)
    .show(ui, |ui| {
        if modifiers.is_empty() {
            ui.label(RichText::new(t(
                "No modifiers. Add one to perturb the animation \
                 (wobble/shake/pulse/spin).",
            )).size(10.0).color(COL_TEXT_DIM).italics());
        } else {
            let mut to_remove: Option<usize> = None;
            for (mi, m) in modifiers.iter_mut().enumerate() {
                let kind_label = m.kind_label();
                let header_color = match m.kind {
                    ModifierKind::Wobble { .. } => Color32::from_rgb(120, 200, 255),
                    ModifierKind::Shake { .. } => Color32::from_rgb(255, 160, 100),
                    ModifierKind::Pulse { .. } => Color32::from_rgb(255, 220, 100),
                    ModifierKind::Spin { .. } => Color32::from_rgb(180, 255, 150),
                    ModifierKind::Walk { .. } => Color32::from_rgb(220, 180, 255),
                };
                egui::Frame::none()
                    .fill(Color32::from_rgb(28, 28, 38))
                    .rounding(Rounding::same(4.0))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(50, 50, 70)))
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut m.enabled, "");
                            ui.label(RichText::new(kind_label).strong().size(11.0).color(header_color));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("x").on_hover_text(t("Remove modifier")).clicked() {
                                    to_remove = Some(mi);
                                }
                            });
                        });
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(t("Range")).size(10.0).color(COL_TEXT_DIM));
                            ui.add(egui::DragValue::new(&mut m.t_start)
                                .range(0.0..=600.0).speed(0.05).suffix("s"));
                            ui.label("\u{2192}");
                            // Render f32::MAX as "∞".
                            if m.t_end >= 1.0e9 {
                                if ui.small_button("\u{221E}").clicked() {
                                    m.t_end = (m.t_start + 1.0).max(1.0);
                                }
                            } else {
                                ui.add(egui::DragValue::new(&mut m.t_end)
                                    .range(0.0..=600.0).speed(0.05).suffix("s"));
                                if ui.small_button("\u{221E}").on_hover_text(t("Always active")).clicked() {
                                    m.t_end = f32::MAX;
                                }
                            }
                        });
                        ui.add_space(2.0);
                        match &mut m.kind {
                            ModifierKind::Wobble { freq_hz, amp_x, amp_y, amp_rot_deg, phase } => {
                                ui.add(egui::Slider::new(freq_hz, 0.1..=10.0).text(t("Freq Hz")));
                                ui.add(egui::Slider::new(amp_x, 0.0..=120.0).text(t("Amp X (px)")));
                                ui.add(egui::Slider::new(amp_y, 0.0..=120.0).text(t("Amp Y (px)")));
                                ui.add(egui::Slider::new(amp_rot_deg, 0.0..=45.0).text(t("Amp Rot \u{00B0}")));
                                ui.add(egui::Slider::new(phase, 0.0..=std::f32::consts::TAU).text(t("Phase")));
                            }
                            ModifierKind::Shake { freq_hz, amp_x, amp_y, seed } => {
                                ui.add(egui::Slider::new(freq_hz, 1.0..=40.0).text(t("Freq Hz")));
                                ui.add(egui::Slider::new(amp_x, 0.0..=80.0).text(t("Amp X (px)")));
                                ui.add(egui::Slider::new(amp_y, 0.0..=80.0).text(t("Amp Y (px)")));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(t("Seed")).size(10.0));
                                    ui.add(egui::DragValue::new(seed).range(0..=u32::MAX).speed(1.0));
                                });
                            }
                            ModifierKind::Pulse { freq_hz, amp_scale } => {
                                ui.add(egui::Slider::new(freq_hz, 0.1..=10.0).text(t("Freq Hz")));
                                ui.add(egui::Slider::new(amp_scale, -0.5..=0.5).text(t("Amp Scale")));
                            }
                            ModifierKind::Spin { speed_dps } => {
                                ui.add(egui::Slider::new(speed_dps, -720.0..=720.0).text(t("Speed \u{00B0}/s")));
                            }
                            ModifierKind::Walk { freq_hz, amp_deg, bob_y, phase } => {
                                ui.add(egui::Slider::new(freq_hz, 0.2..=6.0).text(t("Cadence Hz")));
                                ui.add(egui::Slider::new(amp_deg, 0.0..=45.0).text(t("Sway \u{00B0}")));
                                ui.add(egui::Slider::new(bob_y, 0.0..=40.0).text(t("Bob Y (px)")));
                                ui.add(egui::Slider::new(phase, 0.0..=std::f32::consts::TAU).text(t("Phase")));
                            }
                        }
                    });
                ui.add_space(3.0);
            }
            if let Some(ri) = to_remove {
                modifiers.remove(ri);
            }
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new(t("+ Wobble")).size(10.0)).on_hover_text(
                t("Smooth sinusoidal sway")
            ).clicked() {
                modifiers.push(TrackModifier::wobble());
            }
            if ui.button(RichText::new(t("+ Shake")).size(10.0)).on_hover_text(
                t("High-frequency jitter")
            ).clicked() {
                modifiers.push(TrackModifier::shake());
            }
            if ui.button(RichText::new(t("+ Pulse")).size(10.0)).on_hover_text(
                t("Periodic scale breathing")
            ).clicked() {
                modifiers.push(TrackModifier::pulse());
            }
            if ui.button(RichText::new(t("+ Spin")).size(10.0)).on_hover_text(
                t("Continuous rotation")
            ).clicked() {
                modifiers.push(TrackModifier::spin());
            }
            if ui.button(RichText::new(t("+ Walk")).size(10.0)).on_hover_text(
                t("Pendulum rotation imitating a walking gait (rocks left/right around upright)")
            ).clicked() {
                modifiers.push(TrackModifier::walk());
            }
        });
    });
}


/// Inspector for the per-element effect stack. Effects are evaluated
/// top-down on top of chroma key and colour correction; the user can
/// reorder them with the up/down arrows, mute individual entries, drop
/// in any number of presets from the "+ Add" dropdown, and tune the
/// per-effect parameters with simple sliders.
fn inspector_effect_stack(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    salt: impl std::hash::Hash + Copy,
    t_local: f32,
) {
    let header = RichText::new(format!(
        "Effect Stack ({})",
        effects.len(),
    )).size(12.0).strong().color(Color32::from_rgb(255, 180, 220));

    egui::CollapsingHeader::new(header)
        .id_source(("effect_collapse", salt))
        .default_open(false)
        .show(ui, |ui| {
            if effects.is_empty() {
                ui.label(RichText::new(
                    "No effects. Add one with the buttons below — \
                     stack as many as you like, in any order.",
                ).size(10.0).color(COL_TEXT_DIM).italics());
            } else {
                let mut to_remove: Option<usize> = None;
                let mut to_swap: Option<(usize, usize)> = None;
                let count = effects.len();
                for (ei, eff) in effects.iter_mut().enumerate() {
                    let label = eff.kind.label();
                    egui::Frame::none()
                        .fill(Color32::from_rgb(34, 28, 38))
                        .rounding(Rounding::same(4.0))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(60, 50, 70)))
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut eff.enabled, "");
                                ui.label(RichText::new(label).strong().size(11.0)
                                    .color(Color32::from_rgb(255, 200, 240)));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("x").on_hover_text("Remove effect").clicked() {
                                        to_remove = Some(ei);
                                    }
                                    if ei + 1 < count {
                                        if ui.small_button("\u{2193}").on_hover_text("Move down").clicked() {
                                            to_swap = Some((ei, ei + 1));
                                        }
                                    }
                                    if ei > 0 {
                                        if ui.small_button("\u{2191}").on_hover_text("Move up").clicked() {
                                            to_swap = Some((ei, ei - 1));
                                        }
                                    }
                                });
                            });
                            ui.add_space(2.0);
                            // Master intensity row. Diamond marks
                            // `"intensity"` as animatable; the slider
                            // honours the per-param keyframe track when
                            // the diamond is ON.
                            inspector_effect_anim_slider(
                                ui,
                                eff,
                                "intensity",
                                "Intensity",
                                0.0..=1.0,
                                false,
                                t_local,
                                ("eff_int", ei),
                            );
                            ui.add_space(2.0);
                            inspector_effect_kind_params(ui, eff, salt, ei, t_local);
                        });
                    ui.add_space(3.0);
                }
                if let Some((a, b)) = to_swap {
                    effects.swap(a, b);
                }
                if let Some(ri) = to_remove {
                    effects.remove(ri);
                }
            }

            ui.add_space(6.0);
            // Compact "+ Add effect" dropdown listing every preset. Using
            // a ComboBox keeps the width compact even with 20+ entries —
            // a horizontal grid of buttons would wrap awkwardly.
            ui.horizontal(|ui| {
                ui.label(RichText::new("+ Add effect:").size(11.0).strong());
                let mut to_add: Option<usize> = None;
                // Hide masks from the generic effect picker — they live
                // in the dedicated "Masks" inspector tool now and the
                // bare "Mask" / "Mask (ellipse)" labels in the picker
                // were confusing because users couldn't tell how to
                // apply them. Cropping is also a mask-style operation
                // but is kept here because it's commonly used as a
                // straight effect (no canvas tool to arm).
                let presets: Vec<memstroy_core::Effect> = memstroy_core::all_effect_presets()
                    .into_iter()
                    .filter(|e| !matches!(e.kind, memstroy_core::EffectKind::Mask { .. }))
                    .collect();
                egui::ComboBox::from_id_source(("effect_add", salt))
                    .selected_text("choose…")
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        for (i, p) in presets.iter().enumerate() {
                            if ui.selectable_label(false, p.kind.label()).clicked() {
                                to_add = Some(i);
                            }
                        }
                    });
                if let Some(idx) = to_add {
                    if let Some(p) = presets.get(idx) {
                        effects.push(p.clone());
                    }
                }
                if ui.small_button("clear").on_hover_text("Remove all effects").clicked() {
                    effects.clear();
                }
            });
        });
}

/// Render the per-kind parameter sliders for a single effect entry. Kept
/// as a standalone fn so each variant has its own minimal UI without
/// clogging up `inspector_effect_stack`.
///
/// Every numeric parameter gets a small "Animated" diamond on its left
/// (matching the per-param toggle style used elsewhere in the
/// inspector). Toggling the diamond ON makes future edits write
/// keyframes into `eff.param_kfs[<key>]` at `t_local`; OFF makes
/// edits land on the static field on the `EffectKind` variant. The
/// runtime preview / renderer pipe reads the animated value via
/// `Effect::sampled_at(t_local)` so the inspector and the picture
/// stay in sync.
///
/// Parameter keys follow the documented `param_ids::fx_param`
/// convention: `"intensity"` for the master amount, `"p0"` for the
/// first per-kind parameter, and `"p1"` for the (optional) second.
fn inspector_effect_kind_params(
    ui: &mut egui::Ui,
    eff: &mut Effect,
    _salt: impl std::hash::Hash + Copy,
    ei: usize,
    t_local: f32,
) {
    use memstroy_core::EffectKind as K;
    // For variants with no parameters, just emit a "no parameters"
    // hint and bail out — there's nothing to animate or edit.
    if matches!(
        &eff.kind,
        K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV | K::OldFilm | K::Vhs
    ) {
        ui.label(
            RichText::new("No parameters.")
                .size(10.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );
        return;
    }
    match &eff.kind {
        K::Blur { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Radius (px)", 0.0..=80.0, false, t_local, ("eff_blur", ei)),
        K::Sharpen { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Amount", 0.0..=3.0, false, t_local, ("eff_sharpen", ei)),
        K::HueShift { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Hue \u{00B0}", -180.0..=180.0, false, t_local, ("eff_hue", ei)),
        K::Vignette { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Strength", 0.0..=1.0, false, t_local, ("eff_vignette", ei)),
        K::Pixelate { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Block size (px)", 2.0..=80.0, false, t_local, ("eff_pix", ei)),
        K::Posterize { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Levels", 2.0..=32.0, false, t_local, ("eff_post", ei)),
        K::Glow { .. } => {
            inspector_effect_anim_slider(
                ui, eff, "p0", "Radius (px)", 0.0..=64.0, false, t_local, ("eff_glow_r", ei));
            inspector_effect_anim_slider(
                ui, eff, "p1", "Glow strength", 0.0..=2.0, false, t_local, ("eff_glow_i", ei));
        }
        K::Brightness { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Amount", -1.0..=1.0, false, t_local, ("eff_bri", ei)),
        K::Contrast { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Amount", -1.0..=1.0, false, t_local, ("eff_con", ei)),
        K::Saturation { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Amount", -1.0..=1.0, false, t_local, ("eff_sat", ei)),
        K::EdgeDetect { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Threshold", 0.0..=1.0, false, t_local, ("eff_edge", ei)),
        K::ChromaticAberration { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Offset (px)", 0.0..=24.0, false, t_local, ("eff_ca", ei)),
        K::Noise { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Amount", 0.0..=1.0, false, t_local, ("eff_noise", ei)),
        K::Wave { .. } => {
            inspector_effect_anim_slider(
                ui, eff, "p0", "Amplitude (px)", 0.0..=40.0, false, t_local, ("eff_wave_a", ei));
            inspector_effect_anim_slider(
                ui, eff, "p1", "Wavelength (px)", 4.0..=400.0, false, t_local, ("eff_wave_w", ei));
        }
        K::Glitch { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Strength", 0.0..=1.0, false, t_local, ("eff_glitch", ei)),
        K::Bloom { .. } => inspector_effect_anim_slider(
            ui, eff, "p0", "Radius (px)", 0.0..=80.0, false, t_local, ("eff_bloom", ei)),
        K::Crop { .. } => {
            // Photoshop-style "Crop" — four normalised insets in 0..0.49.
            inspector_effect_anim_slider(
                ui, eff, "p0", "Crop left", 0.0..=0.49, false, t_local, ("eff_crop_l", ei));
            inspector_effect_anim_slider(
                ui, eff, "p1", "Crop top", 0.0..=0.49, false, t_local, ("eff_crop_t", ei));
            inspector_effect_anim_slider(
                ui, eff, "p2", "Crop right", 0.0..=0.49, false, t_local, ("eff_crop_r", ei));
            inspector_effect_anim_slider(
                ui, eff, "p3", "Crop bottom", 0.0..=0.49, false, t_local, ("eff_crop_b", ei));
        }
        K::Mask { .. } => {
            // Mask: feather slider + invert toggle + per-shape geometry
            // sliders. Each geometry slider is animatable through the
            // same diamond+slider widget the rest of the effect-stack
            // already uses, so an animated `rect_left` on a Mask
            // produces a Rect whose left edge is sampled from
            // `param_kfs["rect_left"]` at every frame. The "Repaint"
            // button arms the matching canvas tool so the user can
            // overwrite the geometry with a fresh drag.
            inspector_effect_anim_slider(
                ui, eff, "p0", "Feather", 0.0..=0.5, false, t_local, ("eff_mask_f", ei));
            // Snapshot the shape variant up front so the animatable
            // sliders below can call back into eff (which the
            // `if let &mut` borrow would otherwise pin).
            let shape_kind = match &eff.kind {
                memstroy_core::EffectKind::Mask { shape, .. } => match shape {
                    memstroy_core::MaskShape::Rect { .. } => 0,
                    memstroy_core::MaskShape::Ellipse { .. } => 1,
                    memstroy_core::MaskShape::Polygon { .. } => 2,
                },
                _ => 0,
            };
            match shape_kind {
                0 => {
                    inspector_effect_anim_slider(
                        ui, eff, "rect_left",   "Left",   0.0..=1.0, false, t_local, ("eff_mask_rl", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "rect_top",    "Top",    0.0..=1.0, false, t_local, ("eff_mask_rt", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "rect_right",  "Right",  0.0..=1.0, false, t_local, ("eff_mask_rr", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "rect_bottom", "Bottom", 0.0..=1.0, false, t_local, ("eff_mask_rb", ei));
                }
                1 => {
                    inspector_effect_anim_slider(
                        ui, eff, "ellipse_cx", "Center X", 0.0..=1.0, false, t_local, ("eff_mask_ecx", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "ellipse_cy", "Center Y", 0.0..=1.0, false, t_local, ("eff_mask_ecy", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "ellipse_rx", "Radius X", 0.0..=1.0, false, t_local, ("eff_mask_erx", ei));
                    inspector_effect_anim_slider(
                        ui, eff, "ellipse_ry", "Radius Y", 0.0..=1.0, false, t_local, ("eff_mask_ery", ei));
                }
                _ => {
                    // Polygon: per-vertex animation isn't wired —
                    // surface a hint so the user knows where they
                    // stand instead of staring at an empty section.
                    ui.label(
                        RichText::new(crate::i18n::t(
                            "Freehand polygon vertices animate as a fixed shape; per-vertex keyframes are a future addition.",
                        ))
                        .size(10.0)
                        .italics()
                        .color(COL_TEXT_DIM),
                    );
                }
            }
            if let memstroy_core::EffectKind::Mask { invert, shape, .. } = &mut eff.kind {
                ui.horizontal(|ui| {
                    ui.checkbox(invert, "Invert");
                    let kind_label = match shape {
                        memstroy_core::MaskShape::Rect { .. } => "Rectangle".to_string(),
                        memstroy_core::MaskShape::Ellipse { .. } => "Ellipse".to_string(),
                        memstroy_core::MaskShape::Polygon { points } => {
                            format!("Polygon ({} points)", points.len())
                        }
                    };
                    ui.label(
                        RichText::new(format!("Shape: {}", kind_label))
                            .size(10.0)
                            .color(COL_TEXT_DIM),
                    );
                });
            }
        }
        K::ColorKey { .. } => {
            // Colour-key mask: similarity / blend / spill sliders +
            // the picked colour swatch + invert. The colour itself
            // is set by the canvas eyedropper tool — the inspector
            // exposes a swatch so the user can fine-tune by hand.
            inspector_effect_anim_slider(
                ui, eff, "p0", "Similarity", 0.0..=1.0, false, t_local, ("eff_ck_sim", ei));
            inspector_effect_anim_slider(
                ui, eff, "p1", "Blend",      0.0..=1.0, false, t_local, ("eff_ck_blend", ei));
            inspector_effect_anim_slider(
                ui, eff, "p2", "Spill",      0.0..=1.0, false, t_local, ("eff_ck_spill", ei));
            if let memstroy_core::EffectKind::ColorKey { color, invert, .. } = &mut eff.kind {
                ui.horizontal(|ui| {
                    let mut rgb = [
                        color[0] as f32 / 255.0,
                        color[1] as f32 / 255.0,
                        color[2] as f32 / 255.0,
                    ];
                    ui.label("Key colour");
                    if ui.color_edit_button_rgb(&mut rgb).changed() {
                        color[0] = (rgb[0] * 255.0).round().clamp(0.0, 255.0) as u8;
                        color[1] = (rgb[1] * 255.0).round().clamp(0.0, 255.0) as u8;
                        color[2] = (rgb[2] * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                    ui.checkbox(invert, "Invert");
                });
            }
        }
        K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV
            | K::OldFilm | K::Vhs => unreachable!(),
    }
}

/// Resolve the static value of an `EffectKind` parameter for the given
/// param `key`. Returns `None` for variants that don't own the
/// requested key (e.g. asking for `"p1"` on `Blur`).
fn effect_kind_param_get(
    kind: &memstroy_core::EffectKind,
    key: &str,
) -> Option<f32> {
    use memstroy_core::EffectKind as K;
    match kind {
        K::Blur { radius } if key == "p0" => Some(*radius),
        K::Sharpen { amount } if key == "p0" => Some(*amount),
        K::HueShift { degrees } if key == "p0" => Some(*degrees),
        K::Vignette { strength } if key == "p0" => Some(*strength),
        K::Pixelate { block_size } if key == "p0" => Some(*block_size),
        K::Posterize { levels } if key == "p0" => Some(*levels as f32),
        K::Glow { radius, .. } if key == "p0" => Some(*radius),
        K::Glow { intensity, .. } if key == "p1" => Some(*intensity),
        K::Brightness { amount } if key == "p0" => Some(*amount),
        K::Contrast { amount } if key == "p0" => Some(*amount),
        K::Saturation { amount } if key == "p0" => Some(*amount),
        K::EdgeDetect { threshold } if key == "p0" => Some(*threshold),
        K::ChromaticAberration { offset } if key == "p0" => Some(*offset),
        K::Noise { amount } if key == "p0" => Some(*amount),
        K::Wave { amplitude, .. } if key == "p0" => Some(*amplitude),
        K::Wave { wavelength, .. } if key == "p1" => Some(*wavelength),
        K::Glitch { strength } if key == "p0" => Some(*strength),
        K::Bloom { radius } if key == "p0" => Some(*radius),
        K::Crop { left, .. } if key == "p0" => Some(*left),
        K::Crop { top, .. } if key == "p1" => Some(*top),
        K::Crop { right, .. } if key == "p2" => Some(*right),
        K::Crop { bottom, .. } if key == "p3" => Some(*bottom),
        K::Mask { feather, .. } if key == "p0" => Some(*feather),
        // Per-shape scalars on a Mask. The keys mirror the param ids
        // sampled in `Effect::sampled_at` so animating any of these
        // makes the geometry vary over time.
        K::Mask { shape: memstroy_core::MaskShape::Rect { left, .. }, .. } if key == "rect_left" => Some(*left),
        K::Mask { shape: memstroy_core::MaskShape::Rect { top, .. }, .. } if key == "rect_top" => Some(*top),
        K::Mask { shape: memstroy_core::MaskShape::Rect { right, .. }, .. } if key == "rect_right" => Some(*right),
        K::Mask { shape: memstroy_core::MaskShape::Rect { bottom, .. }, .. } if key == "rect_bottom" => Some(*bottom),
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { cx, .. }, .. } if key == "ellipse_cx" => Some(*cx),
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { cy, .. }, .. } if key == "ellipse_cy" => Some(*cy),
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { rx, .. }, .. } if key == "ellipse_rx" => Some(*rx),
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { ry, .. }, .. } if key == "ellipse_ry" => Some(*ry),
        K::ColorKey { similarity, .. } if key == "p0" => Some(*similarity),
        K::ColorKey { blend, .. } if key == "p1" => Some(*blend),
        K::ColorKey { spill, .. } if key == "p2" => Some(*spill),
        _ => None,
    }
}

/// Write the static value of an `EffectKind` parameter at the given
/// param `key`. No-ops for variants that don't own the key.
fn effect_kind_param_set(
    kind: &mut memstroy_core::EffectKind,
    key: &str,
    new_val: f32,
) {
    use memstroy_core::EffectKind as K;
    match kind {
        K::Blur { radius } if key == "p0" => *radius = new_val,
        K::Sharpen { amount } if key == "p0" => *amount = new_val,
        K::HueShift { degrees } if key == "p0" => *degrees = new_val,
        K::Vignette { strength } if key == "p0" => *strength = new_val,
        K::Pixelate { block_size } if key == "p0" => *block_size = new_val,
        K::Posterize { levels } if key == "p0" => {
            *levels = (new_val as u32).clamp(2, 32);
        }
        K::Glow { radius, .. } if key == "p0" => *radius = new_val,
        K::Glow { intensity, .. } if key == "p1" => *intensity = new_val,
        K::Brightness { amount } if key == "p0" => *amount = new_val,
        K::Contrast { amount } if key == "p0" => *amount = new_val,
        K::Saturation { amount } if key == "p0" => *amount = new_val,
        K::EdgeDetect { threshold } if key == "p0" => *threshold = new_val,
        K::ChromaticAberration { offset } if key == "p0" => *offset = new_val,
        K::Noise { amount } if key == "p0" => *amount = new_val,
        K::Wave { amplitude, .. } if key == "p0" => *amplitude = new_val,
        K::Wave { wavelength, .. } if key == "p1" => *wavelength = new_val,
        K::Glitch { strength } if key == "p0" => *strength = new_val,
        K::Bloom { radius } if key == "p0" => *radius = new_val,
        K::Crop { left, .. } if key == "p0" => *left = new_val.clamp(0.0, 0.49),
        K::Crop { top, .. } if key == "p1" => *top = new_val.clamp(0.0, 0.49),
        K::Crop { right, .. } if key == "p2" => *right = new_val.clamp(0.0, 0.49),
        K::Crop { bottom, .. } if key == "p3" => *bottom = new_val.clamp(0.0, 0.49),
        K::Mask { feather, .. } if key == "p0" => *feather = new_val.clamp(0.0, 0.5),
        // Per-shape scalars on a Mask. UV-coordinate clamps mirror
        // `MaskShape::contains_uv` semantics — the rect edges live in
        // 0..1 and the ellipse centres / radii are unconstrained on
        // the upper end so users can pull the shape briefly outside
        // the frame for animation overshoots.
        K::Mask { shape: memstroy_core::MaskShape::Rect { left, .. }, .. } if key == "rect_left" => {
            *left = new_val.clamp(0.0, 1.0);
        }
        K::Mask { shape: memstroy_core::MaskShape::Rect { top, .. }, .. } if key == "rect_top" => {
            *top = new_val.clamp(0.0, 1.0);
        }
        K::Mask { shape: memstroy_core::MaskShape::Rect { right, .. }, .. } if key == "rect_right" => {
            *right = new_val.clamp(0.0, 1.0);
        }
        K::Mask { shape: memstroy_core::MaskShape::Rect { bottom, .. }, .. } if key == "rect_bottom" => {
            *bottom = new_val.clamp(0.0, 1.0);
        }
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { cx, .. }, .. } if key == "ellipse_cx" => {
            *cx = new_val.clamp(-0.5, 1.5);
        }
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { cy, .. }, .. } if key == "ellipse_cy" => {
            *cy = new_val.clamp(-0.5, 1.5);
        }
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { rx, .. }, .. } if key == "ellipse_rx" => {
            *rx = new_val.clamp(0.0, 1.5);
        }
        K::Mask { shape: memstroy_core::MaskShape::Ellipse { ry, .. }, .. } if key == "ellipse_ry" => {
            *ry = new_val.clamp(0.0, 1.5);
        }
        K::ColorKey { similarity, .. } if key == "p0" => {
            *similarity = new_val.clamp(0.0, 1.0);
        }
        K::ColorKey { blend, .. } if key == "p1" => {
            *blend = new_val.clamp(0.0, 1.0);
        }
        K::ColorKey { spill, .. } if key == "p2" => {
            *spill = new_val.clamp(0.0, 1.0);
        }
        _ => {}
    }
}

/// Generic "animatable" param row for an `Effect`. Renders a left-aligned
/// diamond toggle, the slider, and (when animated) a small "+ kf" button
/// that re-anchors the kf at the current playhead. Reads the displayed
/// value from `eff.param_kfs[key]` when animated, otherwise from the
/// static field on the `EffectKind` variant. On change, writes into
/// `param_kfs` (animated) or the variant's field (static).
///
/// The `key` argument selects which Effect parameter we're driving:
/// `"intensity"` for the master amount or `"p0"` / `"p1"` for the
/// per-kind primary / secondary numeric fields. Keys that don't apply
/// to the current variant short-circuit to a no-op.
fn inspector_effect_anim_slider(
    ui: &mut egui::Ui,
    eff: &mut Effect,
    key: &'static str,
    label: &str,
    range: std::ops::RangeInclusive<f32>,
    logarithmic: bool,
    t_local: f32,
    salt: impl std::hash::Hash + Copy,
) {
    // Resolve current static value: master "intensity" lives on the
    // Effect, the per-kind keys live inside the variant payload.
    let static_value: f32 = match key {
        "intensity" => eff.intensity,
        _ => match effect_kind_param_get(&eff.kind, key) {
            Some(v) => v,
            None => return, // variant doesn't own this key
        },
    };
    let is_animated = eff.animated_params.contains(key);
    let mut display = if is_animated {
        eff.param_kfs
            .get(key)
            .filter(|kfs| !kfs.is_empty())
            .map(|kfs| memstroy_core::keyframe::sample(kfs, t_local).unwrap_or(static_value))
            .unwrap_or(static_value)
    } else {
        static_value
    };

    ui.horizontal(|ui| {
        // Diamond toggle: clicking flips membership in animated_params.
        // When toggled ON we also seed a starter kf at `t_local` with
        // the current static value so the slider doesn't visually jump
        // to zero on first edit.
        if crate::kf_anim::animated_toggle(ui, &mut eff.animated_params, key, salt) {
            if eff.animated_params.contains(key) {
                let entry = eff.param_kfs.entry(key.to_string()).or_default();
                if entry.is_empty() {
                    entry.push(memstroy_core::Keyframe::new(t_local.max(0.0), static_value));
                }
            }
        }
        ui.label(label);
        let mut slider = egui::Slider::new(&mut display, range.clone());
        if logarithmic {
            slider = slider.logarithmic(true);
        }
        let resp = ui.add(slider);
        if resp.changed() {
            if is_animated {
                let entry = eff.param_kfs.entry(key.to_string()).or_default();
                if entry.is_empty() {
                    entry.push(memstroy_core::Keyframe::new(t_local.max(0.0), display));
                } else {
                    memstroy_core::upsert_keyframe(entry, t_local.max(0.0), display);
                }
            } else if key == "intensity" {
                eff.intensity = display;
            } else {
                effect_kind_param_set(&mut eff.kind, key, display);
            }
        }
    });

    if is_animated {
        // Per-param keyframe strip — replaces the old "+ kf at playhead"
        // / "Clear kfs" buttons. Drag a diamond to move it; right-click
        // for the interpolation menu. The strip self-sizes to the
        // largest kf time.
        if let Some(kf_vec) = eff.param_kfs.get_mut(key) {
            if !kf_vec.is_empty() {
                let max_t = kf_vec.iter().map(|k| k.t).fold(0.0_f32, f32::max);
                let dur = max_t.max(t_local).max(1.0);
                let times: Vec<f32> = kf_vec.iter().map(|k| k.t).collect();
                let easings: Vec<memstroy_core::Easing> =
                    kf_vec.iter().map(|k| k.easing).collect();
                let interaction = crate::kf_anim::keyframe_strip(
                    ui,
                    &times,
                    &easings,
                    dur,
                    Some(t_local.max(0.0)),
                    ("effect_kf_strip", key, salt),
                );
                crate::kf_anim::apply_strip_to_f32_kfs(kf_vec, &interaction);
            }
        }
    }
}


/// Inspector row for the actor's playback speed. Uses a `DragValue`
/// (no slider) so the user can dial in arbitrary multipliers without
/// bumping into a fixed range. Editing speed:
///
/// * compresses / stretches the actor's timeline window (`t_out` is
///   recomputed from the source clip's duration so the picture covers
///   the same number of source-seconds at the new rate),
/// * mirrors the change onto any audio track bound to the actor via
///   `parent_actor` so linked layers stay in lock-step on the timeline,
/// * keeps `source_start` / `t_in` fixed — the in-edge of the clip
///   doesn't move, only the out-edge.
fn inspector_actor_speed(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    use crate::i18n::t;
    if i >= state.scene.actors.len() { return; }

    ui.add_space(10.0);
    ui.separator();
    ui.label(
        RichText::new(t("Playback speed"))
            .size(12.0)
            .strong()
            .color(COL_TEXT_DIM),
    );
    ui.add_space(2.0);

    // Snapshot fields we need for the post-edit cascade so we can let
    // the mutable borrow on `actors[i]` end before touching the audio
    // vec / frame-cache map.
    let source_duration = state
        .frame_caches
        .get(i)
        .filter(|fc| fc.is_ready())
        .map(|fc| fc.duration)
        .unwrap_or(0.0);

    let mut new_speed: f32;
    let actor_id;
    let t_in;
    let source_start;
    {
        let a = &mut state.scene.actors[i];
        new_speed = a.speed;
        actor_id = a.id.clone();
        t_in = a.t_in.unwrap_or(0.0);
        source_start = a.source_start.max(0.0);

        ui.horizontal(|ui| {
            ui.label(t("Speed"));
            // No slider — DragValue lets the user dial in 0.05x..16x or
            // anything in between with the keyboard / mouse-drag.
            let resp = ui.add(
                egui::DragValue::new(&mut new_speed)
                    .speed(0.01)
                    .range(0.05..=16.0)
                    .fixed_decimals(3)
                    .suffix("x"),
            )
            .on_hover_text(t(
                "Numeric speed multiplier. The clip's bar on the timeline shrinks when speeding up and stretches when slowing down. Bound audio follows automatically.",
            ));
            if resp.changed() && new_speed.is_finite() && new_speed > 0.0 {
                a.speed = new_speed.max(0.05);
            }
            // Quick reset.
            if ui.small_button("1×").on_hover_text(t("Reset to 1.0x")).clicked() {
                a.speed = 1.0;
                new_speed = 1.0;
            }
        });

        if source_duration > 0.0 {
            let visible_dur =
                ((source_duration - source_start).max(0.0)) / a.speed.max(0.0001);
            ui.label(
                RichText::new(format!(
                    "{}: {:.2}s  \u{2022}  {}: {:.2}s",
                    t("Source"), source_duration,
                    t("Visible"), visible_dur,
                ))
                .size(9.0)
                .color(COL_TEXT_DIM),
            );
        }
    }

    // ── Cascade: when speed changed, also rewrite t_out so the
    // timeline bar shrinks/stretches in sync with the visible duration,
    // then mirror onto bound audio so linked layers move together.
    let speed_now = state.scene.actors[i].speed.max(0.0001);
    if source_duration > 0.0 {
        let visible_dur =
            ((source_duration - source_start).max(0.0)) / speed_now;
        let new_t_out = t_in + visible_dur.max(0.05);
        let cur_t_out = state.scene.actors[i].t_out.unwrap_or(0.0);
        if (cur_t_out - new_t_out).abs() > 1.0e-3 {
            state.scene.actors[i].t_out = Some(new_t_out);
        }
    }

    // Sync to bound audio — same id-based link that move/trim already
    // uses in `sync_audio_to_actor`. Also mirror the speed value so the
    // audio playback rate matches the picture.
    let actor_speed = state.scene.actors[i].speed;
    for au in state.scene.audio.iter_mut() {
        if au.parent_actor.as_deref() == Some(&actor_id) {
            au.speed = actor_speed.max(0.05);
        }
    }
    sync_audio_to_actor(state, i);
}

/// "Masks" inspector tab body for actors. Lists every mask-style
/// effect (`EffectKind::Mask` and `EffectKind::Crop`) that already
/// lives on the actor and exposes a clear set of "+ Add mask"
/// shortcuts. Each shortcut also arms the matching canvas tool
/// (`state.mask_tool`) so the user can immediately paint the shape
/// without diving back into the toolbar.
fn inspector_actor_masks(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let effects: &mut Vec<memstroy_core::Effect> = &mut state.scene.actors[i].effects;
    let mask_tool = &mut state.mask_tool;
    inspector_masks_section(ui, effects, mask_tool, ("actor_masks", i));
}

/// Shared masks UI used by the actor inspector tab and the per-overlay
/// inspector column. Mutates the effects list in place: removes the
/// trailing mask entry on "Reset", commits a "+ Add" preset and arms
/// the matching canvas tool, edits feather / invert on existing
/// entries.
fn inspector_masks_section(
    ui: &mut egui::Ui,
    effects: &mut Vec<memstroy_core::Effect>,
    mask_tool: &mut crate::state::MaskTool,
    salt: impl std::hash::Hash + Copy,
) {
    use memstroy_core::{Effect, EffectKind, MaskShape};
    use crate::state::MaskTool;

    ui.label(
        RichText::new(crate::i18n::t("Masks"))
            .size(13.0)
            .strong()
            .color(Color32::from_rgb(255, 200, 120)),
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new(crate::i18n::t(
            "Masks hide / reveal parts of the layer. Pick a shape, then drag on the canvas to paint it.",
        ))
        .size(10.0)
        .italics()
        .color(COL_TEXT_DIM),
    );
    ui.add_space(6.0);

    // ── Existing masks ──
    let mut to_remove: Option<usize> = None;
    for (ei, eff) in effects.iter_mut().enumerate() {
        let label = match &eff.kind {
            EffectKind::Mask { shape, .. } => match shape {
                MaskShape::Rect { .. } => crate::i18n::t("Rectangle mask"),
                MaskShape::Ellipse { .. } => crate::i18n::t("Ellipse mask"),
                MaskShape::Polygon { .. } => crate::i18n::t("Freehand mask"),
            },
            EffectKind::Crop { .. } => crate::i18n::t("Crop"),
            EffectKind::ColorKey { .. } => crate::i18n::t("Color key mask"),
            _ => continue,
        };
        egui::Frame::none()
            .fill(Color32::from_rgb(34, 28, 38))
            .rounding(Rounding::same(4.0))
            .stroke(Stroke::new(1.0, Color32::from_rgb(70, 60, 100)))
            .inner_margin(egui::Margin::same(6.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut eff.enabled, "");
                    ui.label(
                        RichText::new(label)
                            .strong()
                            .size(11.5)
                            .color(Color32::from_rgb(255, 220, 180)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("x").on_hover_text("Remove mask").clicked() {
                            to_remove = Some(ei);
                        }
                    });
                });
                ui.add_space(4.0);
                match &mut eff.kind {
                    EffectKind::Mask { feather, invert, shape } => {
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Feather"));
                            ui.add(egui::Slider::new(feather, 0.0..=0.5));
                        });
                        ui.checkbox(invert, crate::i18n::t("Invert (hide inside)"));
                        // "Repaint" arms the canvas tool that matches
                        // this mask's shape so the user can overwrite
                        // its geometry with a fresh drag. Polygon
                        // masks expose two repaint modes side-by-side
                        // (freehand drag-trail and segment click-by-
                        // click) because both author the same
                        // `MaskShape::Polygon` data — the user picks
                        // whichever input style suits their gesture.
                        match shape {
                            MaskShape::Rect { .. } => {
                                let label = format!(
                                    "\u{270E} {} {}",
                                    crate::i18n::t("Repaint"),
                                    shape_kind_name(shape),
                                );
                                if ui.button(label).clicked() {
                                    *mask_tool = MaskTool::RectMask;
                                }
                            }
                            MaskShape::Ellipse { .. } => {
                                let label = format!(
                                    "\u{270E} {} {}",
                                    crate::i18n::t("Repaint"),
                                    shape_kind_name(shape),
                                );
                                if ui.button(label).clicked() {
                                    *mask_tool = MaskTool::EllipseMask;
                                }
                            }
                            MaskShape::Polygon { .. } => {
                                ui.horizontal(|ui| {
                                    if ui
                                        .button(format!(
                                            "\u{270D} {} {}",
                                            crate::i18n::t("Repaint"),
                                            crate::i18n::t("freehand"),
                                        ))
                                        .on_hover_text(crate::i18n::t(
                                            "Drag a continuous trail across the canvas to redraw this polygon.",
                                        ))
                                        .clicked()
                                    {
                                        *mask_tool = MaskTool::FreehandMask;
                                    }
                                    if ui
                                        .button(format!(
                                            "\u{2B20} {} {}",
                                            crate::i18n::t("Repaint"),
                                            crate::i18n::t("segments"),
                                        ))
                                        .on_hover_text(crate::i18n::t(
                                            "Click on the canvas to plant new polygon vertices; click near the first or double-click to close.",
                                        ))
                                        .clicked()
                                    {
                                        *mask_tool = MaskTool::SegmentMask;
                                    }
                                });
                            }
                        }
                    }
                    EffectKind::Crop { left, top, right, bottom } => {
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Left"));
                            ui.add(egui::Slider::new(left, 0.0..=0.49));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Top"));
                            ui.add(egui::Slider::new(top, 0.0..=0.49));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Right"));
                            ui.add(egui::Slider::new(right, 0.0..=0.49));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Bottom"));
                            ui.add(egui::Slider::new(bottom, 0.0..=0.49));
                        });
                        // No "Repaint" button for legacy crop entries
                        // — the dedicated rectangle-crop canvas tool
                        // was retired in favour of the unified
                        // Rectangle mask. Sliders remain so old scenes
                        // can still adjust their crop insets in place.
                    }
                    EffectKind::ColorKey { color, similarity, blend, spill, invert } => {
                        // Compact per-entry editor for colour-key
                        // masks. The picked colour is shown as a
                        // swatch — clicking it opens the standard
                        // egui colour picker so the user can fine-
                        // tune what the eyedropper sampled. The
                        // similarity / blend / spill sliders mirror
                        // the FFmpeg `chromakey` filter parameters.
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Key colour"));
                            let mut rgb = [
                                color[0] as f32 / 255.0,
                                color[1] as f32 / 255.0,
                                color[2] as f32 / 255.0,
                            ];
                            if ui.color_edit_button_rgb(&mut rgb).changed() {
                                color[0] = (rgb[0] * 255.0).round().clamp(0.0, 255.0) as u8;
                                color[1] = (rgb[1] * 255.0).round().clamp(0.0, 255.0) as u8;
                                color[2] = (rgb[2] * 255.0).round().clamp(0.0, 255.0) as u8;
                            }
                            ui.label(format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2]));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Similarity"));
                            ui.add(egui::Slider::new(similarity, 0.0..=1.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Blend"));
                            ui.add(egui::Slider::new(blend, 0.0..=1.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Spill"));
                            ui.add(egui::Slider::new(spill, 0.0..=1.0));
                        });
                        ui.checkbox(invert, crate::i18n::t("Invert (keep matching)"));
                        // "Repick" arms the eyedropper so the user
                        // can resample the colour without leaving
                        // the inspector. The next click on the
                        // canvas overwrites this entry's colour.
                        if ui
                            .button(format!("\u{1F4A7} {}", crate::i18n::t("Re-pick")))
                            .on_hover_text(crate::i18n::t(
                                "Arms the eyedropper — next click on the canvas resamples this mask's colour.",
                            ))
                            .clicked()
                        {
                            *mask_tool = MaskTool::Eyedropper;
                        }
                    }
                    _ => {}
                }
            });
        ui.add_space(3.0);
    }
    if let Some(idx) = to_remove {
        effects.remove(idx);
    }

    if effects.iter().all(|e| !matches!(e.kind, EffectKind::Mask { .. } | EffectKind::Crop { .. } | EffectKind::ColorKey { .. })) {
        ui.label(
            RichText::new(crate::i18n::t("No masks yet."))
                .size(10.5)
                .italics()
                .color(COL_TEXT_DIM),
        );
        ui.add_space(4.0);
    }

    // ── Add-mask shortcuts ──
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("+ {}", crate::i18n::t("Add mask")))
            .size(11.0)
            .strong(),
    );
    egui::Grid::new(("masks_add_grid", salt))
        .num_columns(2)
        .spacing([6.0, 6.0])
        .show(ui, |ui| {
            // Rectangle mask serves as both "rectangle" and "crop" in the
            // unified tool set — the legacy Crop button used to push an
            // EffectKind::Crop here, but the two were nearly identical
            // from the user's POV so we kept the more flexible mask
            // form.
            if ui.button(crate::i18n::t("\u{25AD} Rectangle / Crop")).clicked() {
                effects.push(Effect::mask_rect());
                *mask_tool = MaskTool::RectMask;
            }
            if ui.button(crate::i18n::t("\u{2B2D} Ellipse")).clicked() {
                effects.push(Effect::mask_ellipse());
                *mask_tool = MaskTool::EllipseMask;
            }
            ui.end_row();
            if ui.button(crate::i18n::t("\u{270D} Freehand")).clicked() {
                effects.push(memstroy_core::Effect::mask_freehand());
                *mask_tool = MaskTool::FreehandMask;
            }
            // Segment selection mask. The bilingual sister tool to
            // Freehand: the user lays down polygon vertices click-by-
            // click instead of dragging a continuous trail. Useful
            // for hard-edged outlines (sticker borders, geometric
            // logos) where freehand drag is too noisy. Same shape
            // type (`MaskShape::Polygon`) so the renderer / FFmpeg
            // export pipeline doesn't need a new branch — only the
            // editor input handler differs.
            if ui
                .button(crate::i18n::t("\u{2B20} Segment selection"))
                .on_hover_text(crate::i18n::t(
                    "Click on the canvas to plant polygon vertices; click near the first point or double-click to close. Right-click pops the last vertex.",
                ))
                .clicked()
            {
                effects.push(memstroy_core::Effect::mask_freehand());
                *mask_tool = MaskTool::SegmentMask;
            }
            ui.end_row();
            // Eyedropper colour-key mask. Clicking arms the canvas
            // tool AND pushes a placeholder ColorKey effect so the
            // user can already tweak similarity / blend without
            // having to commit a click first; the eyedropper click
            // later overwrites the placeholder colour with the
            // picked pixel.
            if ui
                .button(crate::i18n::t("\u{1F4A7} Eyedropper"))
                .on_hover_text(crate::i18n::t(
                    "Click on the canvas to pick a colour to mask out (works on actors and image overlays).",
                ))
                .clicked()
            {
                effects.push(memstroy_core::Effect::color_key());
                *mask_tool = MaskTool::Eyedropper;
            }
            ui.end_row();
        });

    // Active tool indicator + "Stop drawing" affordance.
    if mask_tool.is_active() {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "\u{1F58C} {}: {}",
                    crate::i18n::t("Drawing"),
                    mask_tool.label(),
                ))
                .color(Color32::from_rgb(255, 200, 80))
                .size(11.0),
            );
            if ui.button(crate::i18n::t("Stop")).clicked() {
                *mask_tool = MaskTool::None;
            }
        });
    }
}

fn shape_kind_name(shape: &memstroy_core::MaskShape) -> &'static str {
    use memstroy_core::MaskShape;
    match shape {
        MaskShape::Rect { .. } => "rectangle",
        MaskShape::Ellipse { .. } => "ellipse",
        MaskShape::Polygon { .. } => "polygon",
    }
}

fn inspector_actor_effects(ui: &mut egui::Ui, state: &mut EditorState, i: usize, _actor_count: usize, _cache_count: usize) {
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new(t("Chroma Key")).size(12.0).strong().color(Color32::from_rgb(100, 255, 100)));
    ui.add_space(4.0);

    // Eyedropper
    let mut chroma_changed = false;
    ui.horizontal(|ui| {
        if state.eyedropper_active {
            ui.label(RichText::new(t("Click preview to pick color...")).color(Color32::from_rgb(255, 200, 50)).size(11.0));
        } else if ui.button(t("Eyedropper")).on_hover_text(t("Pick color from preview")).clicked() {
            state.eyedropper_active = true;
        }
        ui.label(t("Key:"));
        if color_edit_u8(ui, &mut a.chroma_key.key_color) {
            chroma_changed = true;
        }
    });

    ui.add_space(4.0);
    if ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text(t("Similarity"))).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text(t("Blend"))).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text(t("Spill"))).changed() {
        chroma_changed = true;
    }

    // Persist chroma settings as a sidecar next to the source clip so they
    // follow the asset across projects.
    if chroma_changed {
        let src = state.scene.actors[i].source.clone();
        let chroma = state.scene.actors[i].chroma_key.clone();
        let _ = chroma.save_alongside_clip(&src);
    }

    ui.add_space(12.0);

    // Color Correction — pro-grade inspector.
    egui::CollapsingHeader::new(
        RichText::new(t("Color Correction")).size(12.0).strong().color(Color32::from_rgb(200, 180, 255))
    ).default_open(true).show(ui, |ui| {
        color_correction_inspector(ui, state, i);
    });

    ui.add_space(12.0);

    // Skeleton Attachments
    inspector_actor_skeleton_attachments(ui, state, i);

    ui.add_space(12.0);
    // Effect stack — generic post-process effects layered on top of CC.
    // Effect param keyframes are stored in clip-local time so they
    // travel with the actor when its `t_in` shifts on the timeline.
    let actor_t_in = state.scene.actors[i].t_in.unwrap_or(0.0);
    let fx_t_local = (state.playhead - actor_t_in).max(0.0);
    let a = &mut state.scene.actors[i];
    inspector_effect_stack(ui, &mut a.effects, ("actor_fx", i), fx_t_local);
}

// ─── PROFESSIONAL COLOR CORRECTION INSPECTOR ─────────────────────────
//
// Three-tab grading panel:
//   1. Basic — brightness / contrast / saturation / temperature sliders
//      (the legacy quick-look controls).
//   2. Wheels — Lift / Gamma / Gain colour wheels (DaVinci-style). Each wheel
//      is a 2D pad mapped to per-RGB channel offsets. A separate slider on
//      the right of every wheel controls the master amount applied to all
//      three channels uniformly.
//   3. Curves — Master + R / G / B tone curves with click-to-add and
//      drag-to-move control points; right-click removes intermediate points.
//
// All three tabs feed into the same `ColorCorrection` struct and the apply
// pipeline is shared with the export path (see `apply_effects_cpu`).

/// Animatable param row for a `ColorCorrection` scalar field. Mirrors
/// `inspector_effect_anim_slider` but writes into the CC struct's own
/// `kfs` / `animated_params`. Supported keys: `"brightness"`,
/// `"contrast"`, `"saturation"`, `"temperature"` (the four scalar
/// fields). Unknown keys short-circuit to a no-op.
fn inspector_cc_anim_slider(
    ui: &mut egui::Ui,
    cc: &mut memstroy_core::ColorCorrection,
    key: &'static str,
    label: &str,
    range: std::ops::RangeInclusive<f32>,
    t_local: f32,
    salt: impl std::hash::Hash + Copy,
) {
    // Pull the current static value via the param key.
    let static_value: f32 = match key {
        "brightness" => cc.brightness,
        "contrast" => cc.contrast,
        "saturation" => cc.saturation,
        "temperature" => cc.temperature,
        _ => return,
    };
    let is_animated = cc.animated_params.contains(key);
    let mut display = if is_animated {
        cc.kfs
            .get(key)
            .filter(|kfs| !kfs.is_empty())
            .map(|kfs| memstroy_core::keyframe::sample(kfs, t_local).unwrap_or(static_value))
            .unwrap_or(static_value)
    } else {
        static_value
    };
    ui.horizontal(|ui| {
        if crate::kf_anim::animated_toggle(ui, &mut cc.animated_params, key, salt) {
            if cc.animated_params.contains(key) {
                let entry = cc.kfs.entry(key.to_string()).or_default();
                if entry.is_empty() {
                    entry.push(memstroy_core::Keyframe::new(t_local.max(0.0), static_value));
                }
            }
        }
        ui.label(label);
        let resp = ui.add(egui::Slider::new(&mut display, range.clone()));
        if resp.changed() {
            if is_animated {
                let entry = cc.kfs.entry(key.to_string()).or_default();
                if entry.is_empty() {
                    entry.push(memstroy_core::Keyframe::new(t_local.max(0.0), display));
                } else {
                    memstroy_core::upsert_keyframe(entry, t_local.max(0.0), display);
                }
            } else {
                match key {
                    "brightness" => cc.brightness = display,
                    "contrast" => cc.contrast = display,
                    "saturation" => cc.saturation = display,
                    "temperature" => cc.temperature = display,
                    _ => {}
                }
            }
        }
    });
    if is_animated {
        // Per-param keyframe strip — replaces the old buttons. Drag to
        // move kfs in time; right-click for the easing menu.
        if let Some(kf_vec) = cc.kfs.get_mut(key) {
            if !kf_vec.is_empty() {
                let max_t = kf_vec.iter().map(|k| k.t).fold(0.0_f32, f32::max);
                let dur = max_t.max(t_local).max(1.0);
                let times: Vec<f32> = kf_vec.iter().map(|k| k.t).collect();
                let easings: Vec<memstroy_core::Easing> =
                    kf_vec.iter().map(|k| k.easing).collect();
                let interaction = crate::kf_anim::keyframe_strip(
                    ui,
                    &times,
                    &easings,
                    dur,
                    Some(t_local.max(0.0)),
                    ("cc_kf_strip", key, salt),
                );
                crate::kf_anim::apply_strip_to_f32_kfs(kf_vec, &interaction);
            }
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CcTab {
    #[default]
    Basic,
    Wheels,
    Curves,
}

fn color_correction_inspector(ui: &mut egui::Ui, state: &mut EditorState, actor_idx: usize) {
    // Persist the active tab inside egui's per-id memory so it survives
    // selection switches without polluting EditorState.
    let tab_id = ui.id().with(("cc_tab", actor_idx));
    let mut tab: CcTab = ui.data_mut(|d| *d.get_temp_mut_or_default::<CcTab>(tab_id));

    ui.horizontal(|ui| {
        if ui
            .selectable_label(tab == CcTab::Basic, RichText::new(t("Basic")).size(11.0))
            .clicked()
        {
            tab = CcTab::Basic;
        }
        if ui
            .selectable_label(tab == CcTab::Wheels, RichText::new(t("Wheels")).size(11.0))
            .clicked()
        {
            tab = CcTab::Wheels;
        }
        if ui
            .selectable_label(tab == CcTab::Curves, RichText::new(t("Curves")).size(11.0))
            .clicked()
        {
            tab = CcTab::Curves;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button(t("Reset all")).clicked() {
                state.scene.actors[actor_idx].color_correction =
                    memstroy_core::ColorCorrection::default();
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(tab_id, tab));
    ui.add_space(4.0);

    // CC keyframes are stored in clip-local time so they shift with the
    // host actor. Compute the frame at which to read/write before we
    // grab a `&mut` to the colour-correction struct.
    let cc_t_local = (state.playhead - state.scene.actors[actor_idx].t_in.unwrap_or(0.0)).max(0.0);
    let cc = &mut state.scene.actors[actor_idx].color_correction;
    match tab {
        CcTab::Basic => {
            inspector_cc_anim_slider(
                ui, cc, "brightness", t("Brightness"), -1.0..=1.0, cc_t_local, ("cc_b", actor_idx));
            inspector_cc_anim_slider(
                ui, cc, "contrast", t("Contrast"), 0.0..=3.0, cc_t_local, ("cc_c", actor_idx));
            inspector_cc_anim_slider(
                ui, cc, "saturation", t("Saturation"), 0.0..=3.0, cc_t_local, ("cc_s", actor_idx));
            inspector_cc_anim_slider(
                ui, cc, "temperature", t("Temperature"), -1.0..=1.0, cc_t_local, ("cc_t", actor_idx));
        }
        CcTab::Wheels => {
            // Lift wheel: neutral 0, range ±0.5
            color_wheel_widget(ui, t("Lift"),  &mut cc.lift,  [0.0; 3], 0.5, -0.5..=0.5);
            ui.add_space(6.0);
            color_wheel_widget(ui, t("Gamma"), &mut cc.gamma, [1.0; 3], 1.0,  0.2..=4.0);
            ui.add_space(6.0);
            color_wheel_widget(ui, t("Gain"),  &mut cc.gain,  [1.0; 3], 1.0,  0.0..=4.0);
        }
        CcTab::Curves => {
            curve_editor_widget(ui, t("Master"), &mut cc.curves.master, Color32::from_rgb(220, 220, 220));
            ui.add_space(4.0);
            curve_editor_widget(ui, t("Red"),    &mut cc.curves.red,    Color32::from_rgb(255, 100, 100));
            ui.add_space(4.0);
            curve_editor_widget(ui, t("Green"),  &mut cc.curves.green,  Color32::from_rgb(100, 220, 120));
            ui.add_space(4.0);
            curve_editor_widget(ui, t("Blue"),   &mut cc.curves.blue,   Color32::from_rgb(100, 160, 255));
            ui.add_space(2.0);
            ui.label(RichText::new(t("Click empty area: add point  •  Drag: move  •  Right-click: remove"))
                .size(9.0).color(COL_TEXT_DIM));
        }
    }
}

/// DaVinci-style colour wheel:
///   - 2D pad whose XY position maps to per-RGB channel deltas through a
///     hexagonal RGB layout (R at 0°, G at 120°, B at 240°). The inverse
///     mapping uses unit vectors (cos θ, sin θ) along each primary so a pure
///     direction along R lifts only the red channel, etc.
///   - A vertical slider on the right that nudges the *master* value (uniform
///     RGB shift), with reasonable clamps so the channels stay in their
///     valid range.
///   - Double-click on the wheel: snap back to neutral.
fn color_wheel_widget(
    ui: &mut egui::Ui,
    label: &str,
    rgb: &mut [f32; 3],
    neutral: [f32; 3],
    half_extent: f32,
    master_range: std::ops::RangeInclusive<f32>,
) {
    ui.label(RichText::new(label).size(11.0).strong().color(COL_TEXT));
    ui.horizontal(|ui| {
        let size = 110.0_f32;
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let center = rect.center();
        let radius = size * 0.46;

        // Hue ring background.
        let segments = 60;
        for k in 0..segments {
            let a0 = (k as f32) / (segments as f32) * std::f32::consts::TAU;
            let a1 = ((k + 1) as f32) / (segments as f32) * std::f32::consts::TAU;
            let mid = (a0 + a1) * 0.5;
            let hue = mid / std::f32::consts::TAU;
            let col = hsv_to_color32(hue, 0.85, 1.0);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center,
                    center + Vec2::new(a0.cos() * radius, a0.sin() * radius),
                    center + Vec2::new(a1.cos() * radius, a1.sin() * radius),
                ],
                col,
                Stroke::NONE,
            ));
        }
        // Inner neutral disk (so the centre reads "no tint").
        painter.circle_filled(center, radius * 0.40, Color32::from_rgb(40, 40, 50));
        painter.circle_stroke(center, radius, Stroke::new(1.0, Color32::from_rgb(70, 70, 90)));

        // Locate the marker from the current rgb values.
        let dr = rgb[0] - neutral[0];
        let dg = rgb[1] - neutral[1];
        let db = rgb[2] - neutral[2];
        // Inverse of the per-axis projection used in the drag handler.
        // Each primary contributes its own unit vector at 0° / 120° / 240°.
        let mx = (dr * 1.0 + dg * (-0.5) + db * (-0.5)) / half_extent.max(1e-4) * radius;
        let my = -(dr * 0.0 + dg * 0.866 + db * (-0.866)) / half_extent.max(1e-4) * radius;
        let marker = center + Vec2::new(mx.clamp(-radius, radius), my.clamp(-radius, radius));
        painter.circle_filled(marker, 5.0, Color32::WHITE);
        painter.circle_stroke(marker, 5.5, Stroke::new(1.5, Color32::BLACK));

        // Drag handler: project pointer onto the wheel and reverse-map to RGB.
        if resp.dragged() || resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let mut delta = pos - center;
                let dist = delta.length();
                if dist > radius {
                    delta *= radius / dist;
                }
                let dx = delta.x / radius;
                let dy = -delta.y / radius;
                let r_amt = dx * 1.0 + dy * 0.0;
                let g_amt = dx * (-0.5) + dy * 0.866;
                let b_amt = dx * (-0.5) + dy * (-0.866);
                rgb[0] = neutral[0] + r_amt * half_extent;
                rgb[1] = neutral[1] + g_amt * half_extent;
                rgb[2] = neutral[2] + b_amt * half_extent;
            }
        }
        if resp.double_clicked() {
            *rgb = neutral;
        }

        ui.add_space(8.0);

        // Master slider — uniform RGB nudge. Compute the current "master"
        // value from the average of the three channels so the slider stays
        // in sync when the user uses the wheel.
        ui.vertical(|ui| {
            let mut master = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
            ui.label(RichText::new("Master").size(10.0).color(COL_TEXT_DIM));
            let resp = ui.add(
                egui::Slider::new(&mut master, master_range.clone())
                    .show_value(true)
                    .vertical()
                    .step_by(0.001),
            );
            if resp.changed() {
                let avg = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
                let delta = master - avg;
                rgb[0] += delta;
                rgb[1] += delta;
                rgb[2] += delta;
            }
            ui.label(RichText::new(format!("R {:.2}", rgb[0])).size(9.0).color(Color32::from_rgb(255, 120, 120)));
            ui.label(RichText::new(format!("G {:.2}", rgb[1])).size(9.0).color(Color32::from_rgb(120, 220, 130)));
            ui.label(RichText::new(format!("B {:.2}", rgb[2])).size(9.0).color(Color32::from_rgb(120, 170, 255)));
            if ui.small_button("Reset").clicked() {
                *rgb = neutral;
            }
        });
    });
}

/// Editable tone-curve widget. Click an empty area to add a point, drag a
/// point to move it, right-click a point to delete it (endpoints stay
/// permanent and only move vertically). A diagonal reference is drawn behind
/// the curve so deviations from identity are easy to read.
fn curve_editor_widget(
    ui: &mut egui::Ui,
    label: &str,
    points: &mut Vec<[f32; 2]>,
    line_color: Color32,
) {
    ui.label(RichText::new(label).size(10.0).color(COL_TEXT));
    let size = Vec2::new(ui.available_width().min(220.0), 110.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, Rounding::same(2.0), Color32::from_rgb(20, 20, 28));
    for k in 1..4 {
        let f = k as f32 / 4.0;
        let x = rect.min.x + rect.width() * f;
        let y = rect.min.y + rect.height() * f;
        painter.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            Stroke::new(0.5, Color32::from_rgb(45, 45, 60)),
        );
        painter.line_segment(
            [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
            Stroke::new(0.5, Color32::from_rgb(45, 45, 60)),
        );
    }
    // Diagonal identity reference.
    painter.line_segment(
        [egui::pos2(rect.min.x, rect.max.y), egui::pos2(rect.max.x, rect.min.y)],
        Stroke::new(0.7, Color32::from_rgb(60, 60, 80)),
    );

    // Always keep the endpoints sorted at the front/back. The widget treats
    // the first and last points as fixed-x endpoints; intermediate points
    // are sorted by x so insertions stay valid.
    points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));

    let to_screen = |p: [f32; 2]| -> egui::Pos2 {
        egui::pos2(
            rect.min.x + p[0].clamp(0.0, 1.0) * rect.width(),
            rect.max.y - p[1].clamp(0.0, 1.0) * rect.height(),
        )
    };
    let from_screen = |p: egui::Pos2| -> [f32; 2] {
        [
            ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0),
            (1.0 - (p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0),
        ]
    };

    // Drag id stashes the index of the point currently grabbed.
    let drag_id = ui.id().with(("curve_drag_idx", label));

    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let mut hit: Option<usize> = None;
            for (k, &p) in points.iter().enumerate() {
                if (to_screen(p) - pos).length() < 8.0 {
                    hit = Some(k);
                    break;
                }
            }
            let idx = if let Some(k) = hit {
                k
            } else {
                let np = from_screen(pos);
                points.push(np);
                points.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
                points
                    .iter()
                    .position(|p| (p[0] - np[0]).abs() < 1e-4 && (p[1] - np[1]).abs() < 1e-4)
                    .unwrap_or(0)
            };
            ui.data_mut(|d| d.insert_temp(drag_id, idx));
        }
    }

    if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let idx: Option<usize> = ui.data(|d| d.get_temp(drag_id));
            if let Some(idx) = idx {
                if idx < points.len() {
                    let np = from_screen(pos);
                    let last = points.len() - 1;
                    let is_endpoint = idx == 0 || idx == last;
                    if is_endpoint {
                        points[idx][1] = np[1];
                    } else {
                        // Defensive clamp: when neighbours are squeezed
                        // closer than 0.002 the naive `xmin = left+0.001;
                        // xmax = right-0.001` ordering inverts and
                        // `f32::clamp(min, max)` panics with `min > max`
                        // (the exact `1.001 > 1.0` panic users have hit
                        // when both neighbours sit on the right endpoint).
                        // Snap to the midpoint instead so the drag is a
                        // no-op rather than a crash.
                        let mut xmin = points[idx - 1][0] + 0.001;
                        let mut xmax = points[idx + 1][0] - 0.001;
                        if xmin > xmax {
                            let mid = 0.5 * (points[idx - 1][0] + points[idx + 1][0]);
                            xmin = mid;
                            xmax = mid;
                        }
                        points[idx][0] = np[0].clamp(xmin, xmax);
                        points[idx][1] = np[1];
                    }
                }
            }
        }
    }

    // Right-click removes an intermediate point (endpoints are sticky).
    if resp.secondary_clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let mut to_remove: Option<usize> = None;
            for (k, &p) in points.iter().enumerate() {
                if (to_screen(p) - pos).length() < 8.0 && k != 0 && k != points.len() - 1 {
                    to_remove = Some(k);
                    break;
                }
            }
            if let Some(k) = to_remove {
                points.remove(k);
            }
        }
    }

    // Render the curve with a denser sampling so tone-curve LUT changes
    // (256 entries) stay smooth in the inspector preview.
    let mut samples: Vec<egui::Pos2> = Vec::with_capacity(64);
    for i in 0..=63 {
        let x = i as f32 / 63.0;
        let y = memstroy_core::ToneCurves::sample(points, x);
        samples.push(to_screen([x, y]));
    }
    painter.add(egui::Shape::line(samples, Stroke::new(1.5, line_color)));

    for &p in points.iter() {
        let sp = to_screen(p);
        painter.circle_filled(sp, 4.0, line_color);
        painter.circle_stroke(sp, 4.0, Stroke::new(1.0, Color32::BLACK));
    }
}

/// Convert `(hue, saturation, value)` (each in 0..1) to an `egui::Color32`.
/// Only used by the colour-wheel widget to paint its hue ring.
fn hsv_to_color32(h: f32, s: f32, v: f32) -> Color32 {
    let h = h.fract();
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    Color32::from_rgb(
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

/// Skeleton-attachments inspector. Replaces the legacy "skeleton +
/// point" combo selector. Now shows a colored, browsable list of every
/// point defined in every skeleton template attached to this clip's
/// source. Each row is also a drop target: drag any chip from the
/// "Bind elements" list at the top onto a point row to attach.
///
/// The chips are drag sources for both overlays and other actors. The
/// drop zones write the binding to the *attaching* element (the chip),
/// matching the existing `Actor.skeleton_attachments` semantics where
/// the followee actor's id is referenced via `skeleton_id`.
fn inspector_actor_skeleton_attachments(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    egui::CollapsingHeader::new(
        RichText::new("Skeleton Attachment Points").size(12.0).strong()
            .color(Color32::from_rgb(180, 120, 255))
    ).default_open(true).show(ui, |ui| {
        // Resolve which templates apply to this actor's source clip. A
        // skeleton may match by template name, by source-clip path or
        // by source-clip filename — same matching rules as the renderer.
        let actor_source = state.scene.actors[i].source.clone();
        let actor_id = state.scene.actors[i].id.clone();
        let templates: Vec<(usize, String)> = state
            .scene
            .skeleton_templates
            .iter()
            .enumerate()
            .filter(|(_, tmpl)| {
                tmpl.source_clip == actor_source
                    || tmpl.source_clip.file_name() == actor_source.file_name()
            })
            .map(|(idx, tmpl)| (idx, tmpl.name.clone()))
            .collect();

        if templates.is_empty() {
            ui.label(RichText::new(
                "No skeleton bound to this clip yet.\n\
                 Open View \u{2192} Skeleton Editor and save a \
                 sidecar next to the source file."
            ).size(10.0).italics().color(COL_TEXT_DIM));
            return;
        }

        // ── Compact "bind a layer" picker (replaces the row of
        //     drag-source chips with layer names — those felt like a
        //     duplicate of the timeline's layer panel). The user picks
        //     one element + one point and clicks Attach. The eventual
        //     plan is to make timeline layer rows themselves act as
        //     drag sources to skeleton point rows; until that ships,
        //     this picker is the supported attach path. ──
        let element_options: Vec<(crate::state::AttachableElement, String)> = {
            let mut v: Vec<(crate::state::AttachableElement, String)> = Vec::new();
            for oi in 0..state.scene.overlays.len() {
                let label = match &state.scene.overlays[oi] {
                    Overlay::Text(t)   => format!("T:{}",  ellipsis(&t.id, 14)),
                    Overlay::Image(im) => format!("I:{}",  ellipsis(&im.id, 14)),
                    Overlay::Video(v)  => format!("V:{}",  ellipsis(&v.id, 14)),
                };
                v.push((crate::state::AttachableElement::Overlay(oi), label));
            }
            for ai in 0..state.scene.actors.len() {
                if ai == i { continue; }
                v.push((
                    crate::state::AttachableElement::Actor(ai),
                    format!("A:{}", ellipsis(&state.scene.actors[ai].id, 14)),
                ));
            }
            v
        };

        if !element_options.is_empty() {
            // Persist the picker selection across paints so the user can
            // keep tweaking after a successful attach.
            let pick_id = ui.make_persistent_id(("skel_attach_pick", i));
            let mut pick_idx: usize = ui.data(|d| d.get_temp(pick_id).unwrap_or(0));
            if pick_idx >= element_options.len() { pick_idx = 0; }
            let mut commit_pick: Option<(crate::state::AttachableElement, String, String)> = None;

            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Layer:").size(10.0).color(COL_TEXT_DIM));
                egui::ComboBox::from_id_source(("skel_layer_pick", i))
                    .selected_text(element_options[pick_idx].1.clone())
                    .show_ui(ui, |ui| {
                        for (k, (_, label)) in element_options.iter().enumerate() {
                            if ui.selectable_label(k == pick_idx, label).clicked() {
                                pick_idx = k;
                            }
                        }
                    });
                ui.data_mut(|d| d.insert_temp(pick_id, pick_idx));

                // Walk every (template, point) pair and offer a small
                // "+ <point>" button for each. Compact, predictable.
                for (tmpl_idx, tmpl_name) in &templates {
                    let template = &state.scene.skeleton_templates[*tmpl_idx];
                    for (point_name, _) in &template.points {
                        let lbl = format!("\u{2192} {}.{}",
                            ellipsis(tmpl_name, 8), ellipsis(point_name, 12));
                        if ui
                            .small_button(lbl)
                            .on_hover_text(format!(
                                "Attach '{}' to '{}.{}'",
                                element_options[pick_idx].1, tmpl_name, point_name
                            ))
                            .clicked()
                        {
                            commit_pick = Some((
                                element_options[pick_idx].0,
                                tmpl_name.clone(),
                                point_name.clone(),
                            ));
                        }
                    }
                }
            });

            if let Some((src, skel_id, point_name)) = commit_pick {
                attach_element_to_skeleton_point(state, src, &skel_id, &point_name);
                state.status = format!("Attached to {}.{}", skel_id, point_name);
            }
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new("Tip: Alt+drag a clip on the timeline → drop on a point row to attach.")
                .size(9.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );
        ui.add_space(2.0);

        // ── Per-template point list with drop zones ──
        // The drop zones listen for `element_drag.source` on pointer
        // release. The supported authoring path is now Alt+drag from a
        // timeline clip bar (set in the actor / overlay drag arms of
        // `timeline()`); the picker above remains as a click-to-attach
        // fallback for when the user wants a precise menu choice.
        let dragging_label = state.element_drag.label.clone();
        let dragging = state.element_drag.source;
        let pointer_released = ui.input(|i| i.pointer.any_released());
        let pointer_pos = ui.input(|i| i.pointer.hover_pos());
        let mut commit_attach: Option<(crate::state::AttachableElement, String, String)> = None;

        for (tmpl_idx, tmpl_name) in &templates {
            let template = &state.scene.skeleton_templates[*tmpl_idx];
            let point_keys: Vec<(String, [u8; 3])> = template.points
                .iter()
                .map(|(name, p)| (name.clone(), p.color))
                .collect();

            ui.label(
                RichText::new(format!("\u{1F9B4} {}", tmpl_name))
                    .size(11.0).strong().color(Color32::from_rgb(220, 200, 255)),
            );
            if point_keys.is_empty() {
                ui.label(RichText::new("  (no points defined)").size(9.0)
                    .italics().color(COL_TEXT_DIM));
                continue;
            }

            for (point_name, color) in point_keys.iter() {
                // Existing bindings for this template+point.
                let attached_now: Vec<(usize, usize, String)> = state
                    .scene
                    .actors
                    .iter()
                    .enumerate()
                    .flat_map(|(ai, a)| {
                        a.skeleton_attachments.iter().enumerate()
                            .filter(|(_, att)| {
                                (att.skeleton_id == *tmpl_name
                                    || matches_skeleton_id(&att.skeleton_id, &actor_id))
                                    && att.point_name == *point_name
                            })
                            .map(move |(att_i, _)| (ai, att_i, format!("A:{}", a.id)))
                    })
                    .collect();
                let overlay_attached: Vec<(usize, String)> = state
                    .scene
                    .overlays
                    .iter()
                    .enumerate()
                    .filter_map(|(oi, ov)| {
                        let att = match ov {
                            Overlay::Text(t) => t.skeleton_attachment.as_ref(),
                            Overlay::Image(im) => im.skeleton_attachment.as_ref(),
                            Overlay::Video(v) => v.skeleton_attachment.as_ref(),
                        }?;
                        if (att.skeleton_id == *tmpl_name
                                || matches_skeleton_id(&att.skeleton_id, &actor_id))
                            && att.point_name == *point_name
                        {
                            Some((oi, format!("O:{}", overlay_id(ov))))
                        } else { None }
                    })
                    .collect();

                let row_h = 26.0;
                let (row_rect, row_resp) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), row_h),
                    Sense::hover(),
                );

                let hovered_drop = dragging.is_some()
                    && pointer_pos.map(|p| row_rect.contains(p)).unwrap_or(false);

                let bg = if hovered_drop {
                    Color32::from_rgb(50, 80, 100)
                } else {
                    Color32::from_rgb(28, 28, 38)
                };
                let painter = ui.painter_at(row_rect);
                painter.rect_filled(row_rect, Rounding::same(4.0), bg);
                let stroke_col = if hovered_drop {
                    Color32::from_rgb(120, 200, 255)
                } else {
                    Color32::from_rgb(50, 50, 70)
                };
                painter.rect_stroke(row_rect, Rounding::same(4.0), Stroke::new(1.0, stroke_col));

                let dot_pos = Pos2::new(row_rect.min.x + 12.0, row_rect.center().y);
                painter.circle_filled(dot_pos, 5.0,
                    Color32::from_rgb(color[0], color[1], color[2]));
                painter.text(
                    Pos2::new(row_rect.min.x + 24.0, row_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    point_name,
                    egui::FontId::proportional(11.0),
                    COL_TEXT,
                );

                // Bound chip(s) shown on the right side of the row.
                let mut chip_x = row_rect.max.x - 6.0;
                for (oi, label) in overlay_attached.iter().rev() {
                    let chip_size = Vec2::new(26.0 + (label.len() as f32) * 5.0, 18.0);
                    let chip_rect = Rect::from_min_size(
                        Pos2::new(chip_x - chip_size.x, row_rect.center().y - chip_size.y * 0.5),
                        chip_size,
                    );
                    painter.rect_filled(chip_rect, Rounding::same(3.0),
                        Color32::from_rgb(60, 100, 70));
                    painter.text(chip_rect.center(), egui::Align2::CENTER_CENTER,
                        label, egui::FontId::proportional(9.0), COL_TEXT);
                    let resp = ui.interact(
                        chip_rect,
                        ui.id().with(("rm_ovr_attach", *oi, point_name)),
                        Sense::click(),
                    );
                    if resp.clicked() {
                        match &mut state.scene.overlays[*oi] {
                            Overlay::Text(t) => t.skeleton_attachment = None,
                            Overlay::Image(im) => im.skeleton_attachment = None,
                            Overlay::Video(v) => v.skeleton_attachment = None,
                        }
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    chip_x -= chip_size.x + 4.0;
                }
                for (ai, _att_i, label) in attached_now.iter().rev() {
                    let chip_size = Vec2::new(28.0 + (label.len() as f32) * 5.0, 18.0);
                    let chip_rect = Rect::from_min_size(
                        Pos2::new(chip_x - chip_size.x, row_rect.center().y - chip_size.y * 0.5),
                        chip_size,
                    );
                    painter.rect_filled(chip_rect, Rounding::same(3.0),
                        Color32::from_rgb(110, 70, 40));
                    painter.text(chip_rect.center(), egui::Align2::CENTER_CENTER,
                        label, egui::FontId::proportional(9.0), COL_TEXT);
                    // Click chip → remove that binding.
                    let resp = ui.interact(
                        chip_rect,
                        ui.id().with(("rm_actor_attach", *ai, point_name)),
                        Sense::click(),
                    );
                    if resp.clicked() {
                        // Remove the matching attachment entry on the bound actor.
                        state.scene.actors[*ai].skeleton_attachments.retain(|att| {
                            !((att.skeleton_id == *tmpl_name
                                    || matches_skeleton_id(&att.skeleton_id, &actor_id))
                                && att.point_name == *point_name)
                        });
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    chip_x -= chip_size.x + 4.0;
                }

                if hovered_drop {
                    painter.text(
                        row_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("\u{2935} drop {} here", dragging_label),
                        egui::FontId::proportional(10.0),
                        Color32::from_rgb(220, 240, 255),
                    );
                    if pointer_released {
                        if let Some(src) = dragging {
                            commit_attach = Some((src, tmpl_name.clone(), point_name.clone()));
                        }
                    }
                }
                let _ = row_resp;
                ui.add_space(2.0);
            }
            ui.add_space(4.0);
        }

        if let Some((src, skel_id, point_name)) = commit_attach {
            attach_element_to_skeleton_point(state, src, &skel_id, &point_name);
            state.element_drag.source = None;
            state.element_drag.label.clear();
            state.status = format!("Attached to {}.{}", skel_id, point_name);
        }

        // Clear any stale drag once the pointer is up — handles the case
        // where the user dropped outside any drop zone.
        if pointer_released && state.element_drag.source.is_some() {
            state.element_drag.source = None;
            state.element_drag.label.clear();
        }
    });
}

/// Lightweight match: accepts the bound element's stored `skeleton_id`
/// equal to the template name (canonical case) or to the host actor's id
/// (legacy convenience). Mirrors the existing resolver semantics.
fn matches_skeleton_id(stored: &str, actor_id: &str) -> bool {
    stored == actor_id
}

fn overlay_id(ov: &Overlay) -> String {
    match ov {
        Overlay::Text(t) => t.id.clone(),
        Overlay::Image(im) => im.id.clone(),
        Overlay::Video(v) => v.id.clone(),
    }
}

/// Visual chip + drag source for the skeleton attach panel.
/// Currently unused (the inspector uses a compact ComboBox + Attach
/// buttons instead of a chip row), kept for the future cross-panel
/// drag-from-layers implementation.
#[allow(dead_code)]
fn element_drag_chip(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + Copy,
    label: &str,
    accent: Color32,
) -> egui::Response {
    let id = ui.id().with(salt);
    let pad = Vec2::new(6.0, 4.0);
    let text_size = ui.fonts(|f| {
        f.layout_no_wrap(label.into(), egui::FontId::proportional(10.0), Color32::WHITE)
    });
    let chip_size = Vec2::new(text_size.size().x + pad.x * 2.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(chip_size, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let bg = if resp.dragged() {
        Color32::from_rgb(80, 100, 80)
    } else {
        Color32::from_rgb(40, 40, 55)
    };
    painter.rect_filled(rect, Rounding::same(3.0), bg);
    painter.rect_stroke(rect, Rounding::same(3.0), Stroke::new(1.0, accent));
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, label,
        egui::FontId::proportional(10.0), COL_TEXT);
    let _ = id;
    resp
}

/// Commit a drag-and-drop / picker-driven attach: write the binding
/// into the source element's `skeleton_attachment` field (overlays) or
/// push into the source actor's `skeleton_attachments` list. Only ONE
/// element may bind to a given (skeleton_id, point_name) at a time —
/// any prior binding (overlay or actor) on the same target is cleared
/// before the new one is committed. This matches the user-visible
/// rule that each skeleton point is occupied by at most one layer.
fn attach_element_to_skeleton_point(
    state: &mut EditorState,
    src: crate::state::AttachableElement,
    skeleton_id: &str,
    point_name: &str,
) {
    // ── 1. Clear any existing bindings (across all elements) at the
    //       target slot, so each (skeleton, point) pair holds at most
    //       one element. This is what stops duplicate chips piling up
    //       on the same row in the inspector.
    for ov in &mut state.scene.overlays {
        let slot = match ov {
            Overlay::Text(t) => &mut t.skeleton_attachment,
            Overlay::Image(im) => &mut im.skeleton_attachment,
            Overlay::Video(v) => &mut v.skeleton_attachment,
        };
        if slot
            .as_ref()
            .map(|att| att.skeleton_id == skeleton_id && att.point_name == point_name)
            .unwrap_or(false)
        {
            *slot = None;
        }
    }
    for actor in &mut state.scene.actors {
        actor.skeleton_attachments.retain(|att| {
            !(att.skeleton_id == skeleton_id && att.point_name == point_name)
        });
    }

    // ── 2. Apply the new binding to the source element.
    let attachment = memstroy_core::SkeletonAttachment {
        skeleton_id: skeleton_id.into(),
        point_name: point_name.into(),
        offset: [0.0, 0.0],
        scale: 1.0,
        follow_rotation: false,
    };
    match src {
        crate::state::AttachableElement::Overlay(oi) => {
            if oi >= state.scene.overlays.len() { return; }
            match &mut state.scene.overlays[oi] {
                Overlay::Text(t) => t.skeleton_attachment = Some(attachment),
                Overlay::Image(im) => im.skeleton_attachment = Some(attachment),
                Overlay::Video(v) => v.skeleton_attachment = Some(attachment),
            }
        }
        crate::state::AttachableElement::Actor(ai) => {
            if ai >= state.scene.actors.len() { return; }
            state.scene.actors[ai].skeleton_attachments.push(attachment);
        }
    }
}

fn inspector_overlay(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let duration = state.scene.output.duration;
    let overlay_count = state.scene.overlays.len();
    let playhead = state.playhead;

    let ov = &mut state.scene.overlays[i];

    match ov {
        Overlay::Text(t) => {
            // Returns Option<TextAction> for backward compat — currently
            // unused since the layer-order buttons were removed.
            let _ = inspector_text_overlay(ui, t, i, overlay_count, duration, playhead);
        }
        Overlay::Image(im) => {
            ui.label(RichText::new(format!("{}: {}", t("Image"), im.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            // The In / Out time controls were intentionally removed
            // from the inspector — the user adjusts an image's visible
            // window by dragging its clip edges in the layer panel.
            // Trying to maintain two sources of truth (drag handles +
            // numeric fields) was the source of multiple "image keeps
            // jumping back to old t_in" bugs.
            inspector_overlay_state_widgets(
                ui, &mut im.layout, &mut im.animated_params, playhead, i, "img",
                state.kf_highlight.clone());
            ui.add_space(8.0);
            inspector_modifiers(ui, &mut im.modifiers, ("img_mods", i));
            ui.add_space(8.0);
            inspector_masks_section(ui, &mut im.effects, &mut state.mask_tool, ("img_masks", i));
            ui.add_space(8.0);
            let fx_t_local = (playhead - im.t_in).max(0.0);
            inspector_effect_stack(ui, &mut im.effects, ("img_fx", i), fx_t_local);
        }
        Overlay::Video(v) => {
            ui.label(RichText::new(format!("{}: {}", t("Video"), v.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(t("In:"));
                ui.add(egui::DragValue::new(&mut v.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label(t("Out:"));
                ui.add(egui::DragValue::new(&mut v.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });

            // ── Playback speed (DragValue, not a slider, so the user
            // can dial in arbitrary values without being clamped to a
            // small range) ──
            //
            // We capture the *old* speed BEFORE the widget writes the
            // new value. The clip's reference "1× length" is derived
            // from the current window times the old speed, then the new
            // window is reconstructed from that same reference at the
            // new speed. This keeps the math self-contained without
            // needing a per-frame ffprobe.
            let old_speed = v.speed.max(0.0001);
            let cur_dur = (v.t_out - v.t_in).max(0.05);
            let one_x_dur = cur_dur * old_speed;

            let mut new_speed = v.speed;
            let mut speed_changed = false;
            ui.horizontal(|ui| {
                ui.label(crate::i18n::t("Speed"));
                let resp = ui.add(
                    egui::DragValue::new(&mut new_speed)
                        .speed(0.01)
                        .range(0.05..=16.0)
                        .fixed_decimals(3)
                        .suffix("x"),
                )
                .on_hover_text(crate::i18n::t(
                    "Numeric speed multiplier — clip width on the timeline scales with this value.",
                ));
                if resp.changed() && new_speed.is_finite() && new_speed > 0.0 {
                    v.speed = new_speed.max(0.05);
                    speed_changed = true;
                }
                if ui.small_button("1\u{00D7}")
                    .on_hover_text(crate::i18n::t("Reset to 1.0x"))
                    .clicked()
                {
                    v.speed = 1.0;
                    speed_changed = true;
                }
            });
            if speed_changed {
                let new_dur = (one_x_dur / v.speed.max(0.0001)).max(0.05);
                v.t_out = v.t_in + new_dur;
            }
            ui.label(
                RichText::new(format!(
                    "{}: {:.2}s  \u{2022}  1\u{00D7} ref: {:.2}s",
                    crate::i18n::t("Visible"), v.t_out - v.t_in, one_x_dur
                ))
                .size(9.0)
                .color(COL_TEXT_DIM),
            );

            inspector_overlay_state_widgets(
                ui, &mut v.layout, &mut v.animated_params, playhead, i, "vid",
                state.kf_highlight.clone());
            ui.add_space(8.0);
            inspector_modifiers(ui, &mut v.modifiers, ("vid_mods", i));
            ui.add_space(8.0);
            inspector_masks_section(ui, &mut v.effects, &mut state.mask_tool, ("vid_masks", i));
            ui.add_space(8.0);
            let fx_t_local = (playhead - v.t_in).max(0.0);
            inspector_effect_stack(ui, &mut v.effects, ("vid_fx", i), fx_t_local);
        }
    }
}

/// Shared "transform" widget block for the three overlay flavours
/// (Image / Video / Text). Reads sampled values from `layout` and only
/// writes through `kf_anim::write_overlay_param` when the user actually
/// edits a control — this is the per-overlay equivalent of the actor's
/// new inspector flow and the fix for the "infinite keyframes during
/// playback / timeline scrub" bug.
fn inspector_overlay_state_widgets(
    ui: &mut egui::Ui,
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &mut std::collections::BTreeSet<String>,
    playhead: f32,
    salt_idx: usize,
    salt_kind: &'static str,
    highlight: crate::kf_anim::KfHighlight,
) {
    use crate::kf_anim;
    use memstroy_core::param_ids;

    let cur = kf_anim::sample_overlay(layout, playhead);

    let mut new_x = cur.pos[0];
    let mut new_y = cur.pos[1];
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::POS_X, (salt_kind, "px", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::POS_X), "X:"));
        let r = ui.add(egui::DragValue::new(&mut new_x).speed(0.005));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::POS_X, false, |s| s.pos[0] = new_x);
        }
        kf_anim::animated_toggle(ui, animated_params, param_ids::POS_Y, (salt_kind, "py", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::POS_Y), "Y:"));
        let r = ui.add(egui::DragValue::new(&mut new_y).speed(0.005));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::POS_Y, false, |s| s.pos[1] = new_y);
        }
    });
    let pos_x_anim = animated_params.contains(param_ids::POS_X);
    let pos_y_anim = animated_params.contains(param_ids::POS_Y);
    inspector_overlay_param_strip(
        ui, layout, pos_x_anim, playhead,
        |s| s.pos[0], (salt_kind, "strip_px", salt_idx),
    );
    inspector_overlay_param_strip(
        ui, layout, pos_y_anim, playhead,
        |s| s.pos[1], (salt_kind, "strip_py", salt_idx),
    );

    let mut new_scale = cur.scale;
    let mut new_sy = cur.scale_y;
    // Per-element "lock" between Scale X and Scale Y. Persisted in the
    // egui memory under a stable id so each overlay/actor remembers
    // whether the user wants synchronised scaling. Default = LOCKED so
    // the first thing a user gets is the proportional behaviour they
    // expect; click the chain glyph to break it.
    let lock_id = ui.make_persistent_id(("scale_lock", salt_kind, salt_idx));
    let mut linked: bool = ui.data(|d| d.get_temp(lock_id).unwrap_or(true));

    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::SCALE, (salt_kind, "sc", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::SCALE), "Scale X:"));
        let r = ui.add(egui::Slider::new(&mut new_scale, 0.05..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::SCALE, false, |s| s.scale = new_scale);
            if linked {
                // Mirror the X edit onto Y. Note: scale_y is a Y-stretch
                // multiplier ON TOP of `scale`, so to keep total Y scale
                // unchanged when a user is in lock-mode and edits X we
                // simply hold scale_y at 1.0 (uniform) — that's the
                // intent of "synced X and Y scale".
                new_sy = 1.0;
                kf_anim::write_overlay_param(layout, animated_params, playhead,
                    param_ids::SCALE_Y, false, |s| s.scale_y = 1.0);
            }
        }
        // Link/unlink chain glyph. \u{1F517} = 🔗, \u{1F494} = 💔 (broken).
        let chain = if linked { "\u{1F517}" } else { "\u{26D3}" };
        if ui.small_button(chain)
            .on_hover_text(if linked {
                "Scale X and Scale Y are linked — click to unlink"
            } else {
                "Scale X and Scale Y are independent — click to link"
            })
            .clicked()
        {
            linked = !linked;
            ui.data_mut(|d| d.insert_temp(lock_id, linked));
            if linked {
                // Re-syncing forces Y to follow X (uniform).
                new_sy = 1.0;
                kf_anim::write_overlay_param(layout, animated_params, playhead,
                    param_ids::SCALE_Y, false, |s| s.scale_y = 1.0);
            }
        } else {
            ui.data_mut(|d| d.insert_temp(lock_id, linked));
        }
    });

    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::SCALE_Y, (salt_kind, "sy", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::SCALE_Y), "Scale Y:"));
        if linked {
            // Linked mode: the Y slider edits the *uniform* scale.
            // See actor inspector for the rationale (the previous
            // multiplicative formula caused exponential runaway).
            let mut linked_scale = cur.scale;
            let r = ui.add(egui::Slider::new(&mut linked_scale, 0.05..=5.0).logarithmic(true));
            if r.changed() && linked_scale.is_finite() && linked_scale > 0.0 {
                kf_anim::write_overlay_param(layout, animated_params, playhead,
                    param_ids::SCALE, false, |s| s.scale = linked_scale);
                kf_anim::write_overlay_param(layout, animated_params, playhead,
                    param_ids::SCALE_Y, false, |s| s.scale_y = 1.0);
            }
        } else {
            let r = ui.add(egui::Slider::new(&mut new_sy, 0.1..=5.0).logarithmic(true));
            if r.changed() {
                kf_anim::write_overlay_param(layout, animated_params, playhead,
                    param_ids::SCALE_Y, false, |s| s.scale_y = new_sy);
            }
        }
    });
    let scale_anim = animated_params.contains(param_ids::SCALE);
    let scale_y_anim = animated_params.contains(param_ids::SCALE_Y);
    inspector_overlay_param_strip(
        ui, layout, scale_anim, playhead,
        |s| s.scale, (salt_kind, "strip_sc", salt_idx),
    );
    inspector_overlay_param_strip(
        ui, layout, scale_y_anim, playhead,
        |s| s.scale_y, (salt_kind, "strip_sy", salt_idx),
    );

    let mut new_rot = cur.rotation_deg;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::ROTATION, (salt_kind, "rot", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::ROTATION), "Rotation"));
        let prev_rot = new_rot;
        circular_rotation_widget(ui, (salt_kind, "rot_w", salt_idx), &mut new_rot, 80.0);
        let mut dial_changed = (new_rot - prev_rot).abs() > 1.0e-4;
        let r = ui.add(egui::DragValue::new(&mut new_rot)
            .range(-3600.0..=3600.0).speed(0.5).suffix("\u{00B0}").fixed_decimals(1));
        if r.changed() { dial_changed = true; }
        if dial_changed {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::ROTATION, false, |s| s.rotation_deg = new_rot);
        }
    });
    let rot_anim = animated_params.contains(param_ids::ROTATION);
    inspector_overlay_param_strip(
        ui, layout, rot_anim, playhead,
        |s| s.rotation_deg, (salt_kind, "strip_rot", salt_idx),
    );

    let mut new_op = cur.opacity;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::OPACITY, (salt_kind, "op", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::OPACITY), "Opacity:"));
        let r = ui.add(egui::Slider::new(&mut new_op, 0.0..=1.0));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::OPACITY, false, |s| s.opacity = new_op);
        }
    });
    let op_anim = animated_params.contains(param_ids::OPACITY);
    inspector_overlay_param_strip(
        ui, layout, op_anim, playhead,
        |s| s.opacity, (salt_kind, "strip_op", salt_idx),
    );

    let mut new_fx = cur.flip_x_anim;
    let mut new_fy = cur.flip_y_anim;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::FLIP_X, (salt_kind, "fx", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::FLIP_X), "Flip X:"));
        let r = ui.add(egui::Slider::new(&mut new_fx, -1.0..=1.0));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::FLIP_X, false, |s| s.flip_x_anim = new_fx);
        }
    });
    let flip_x_anim = animated_params.contains(param_ids::FLIP_X);
    inspector_overlay_param_strip(
        ui, layout, flip_x_anim, playhead,
        |s| s.flip_x_anim, (salt_kind, "strip_fx", salt_idx),
    );
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::FLIP_Y, (salt_kind, "fy", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::FLIP_Y), "Flip Y:"));
        let r = ui.add(egui::Slider::new(&mut new_fy, -1.0..=1.0));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::FLIP_Y, false, |s| s.flip_y_anim = new_fy);
        }
    });
    let flip_y_anim = animated_params.contains(param_ids::FLIP_Y);
    inspector_overlay_param_strip(
        ui, layout, flip_y_anim, playhead,
        |s| s.flip_y_anim, (salt_kind, "strip_fy", salt_idx),
    );
}

/// Inspector layer-order actions for a text overlay. The buttons that
/// produced these actions were removed when the timeline track row became
/// the single source of truth for stacking, but the type is kept (and
/// still returned by `inspector_text_overlay`) so any future re-introduction
/// of explicit per-text overrides slots in without churn.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum TextAction {
    LayerUp,
    LayerDown,
    ToFront,
    ToBack,
}

fn inspector_text_overlay(
    ui: &mut egui::Ui,
    t: &mut TextOverlay,
    idx: usize,
    _total: usize,
    _duration: f32,
    playhead: f32,
) -> Option<TextAction> {
    // Header (delete button removed — use Delete/Backspace shortcut or
    // right-click on the timeline clip).
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Text: {}", ellipsis(&t.text, 16)))
            .strong().size(14.0).color(COL_CLIP_OVERLAY));
    });
    ui.add_space(4.0);

    // Text content
    ui.label(RichText::new(crate::i18n::t("Text:")).size(11.0).strong());
    ui.add(
        egui::TextEdit::multiline(&mut t.text)
            .desired_rows(2)
            // Subtract a small epsilon from `available_width` so the
            // multiline widget never measures *exactly* equal to the
            // panel's available width — egui rounds widget sizes up,
            // and a 1-px overrun re-pumps the inspector's SidePanel
            // wider on the next frame, which is what made the Inspector
            // panel drift wider every time the user clicked on a text
            // overlay.
            .desired_width((ui.available_width() - 4.0).max(80.0)),
    );
    ui.add_space(8.0);

    // ─── Position / rotation / opacity (size is driven by font_size) ───
    // Inspector reads the eased current value at the playhead and only
    // writes through `kf_anim::write_overlay_param` on actual edits, so
    // simply drawing the inspector during playback / scrubbing no
    // longer auto-inserts keyframes.
    inspector_overlay_state_widgets(
        ui, &mut t.layout, &mut t.animated_params, playhead, idx, "text",
        crate::kf_anim::KfHighlight::default());
    ui.add_space(8.0);

    // ─── Font ─────────────────────────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Font")).size(12.0).strong().color(Color32::from_rgb(180, 220, 255)),
    ).default_open(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Family:"));
            // The picker now enumerates every TTF/OTF found on the OS
            // (Windows / macOS / Linux), groups by the human-readable
            // family name from each file's `name` table, and lazily
            // loads the chosen face into egui so the canvas preview
            // can actually render it. The bundled `Default` /
            // `Monospace` entries are kept at the top as quick fallbacks.
            let avail_w = ui.available_width();
            egui::ComboBox::from_id_source("text_font_family")
                .selected_text(t.style.font.clone())
                .width((avail_w - 8.0).clamp(80.0, 320.0))
                .show_ui(ui, |ui| {
                    // Bundled families first.
                    for fam in BUNDLED_FONTS {
                        ui.selectable_value(&mut t.style.font, fam.to_string(), *fam);
                    }
                    let system = crate::system_fonts::available_families();
                    if !system.is_empty() {
                        ui.separator();
                        ui.label(
                            RichText::new(crate::i18n::t("System fonts"))
                                .size(10.0)
                                .color(Color32::from_rgb(140, 140, 160)),
                        );
                        for entry in system {
                            ui.selectable_value(
                                &mut t.style.font,
                                entry.family.clone(),
                                &entry.family,
                            );
                        }
                    }
                });
            // Lazily register the chosen family with egui so the canvas
            // preview can pick it up next frame. Cheap (idempotent
            // hashmap lookup) for already-loaded families.
            let _ = crate::system_fonts::ensure_font_loaded(ui.ctx(), &t.style.font);
        });

        // ── Size: compact "−  [drag value]  +" trio.
        // The "Preset:" row was removed — the −/+ buttons cover quick
        // adjustments and the drag value covers the full range.
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Size:"));
            if ui.small_button("\u{2212}").on_hover_text(crate::i18n::t("Decrease (-4)")).clicked() {
                t.style.font_size = (t.style.font_size - 4.0).max(8.0);
            }
            ui.add(
                egui::DragValue::new(&mut t.style.font_size)
                    .range(8.0..=512.0)
                    .speed(0.5)
                    .suffix(" px"),
            );
            if ui.small_button("+").on_hover_text(crate::i18n::t("Increase (+4)")).clicked() {
                t.style.font_size = (t.style.font_size + 4.0).min(512.0);
            }
        });

        ui.horizontal(|ui| {
            ui.checkbox(&mut t.style.bold, crate::i18n::t("Bold"))
                .on_hover_text(crate::i18n::t(
                    "Synthesised on the bundled font by repainting glyphs \
                     with sub-pixel offsets",
                ));
            ui.checkbox(&mut t.style.italic, crate::i18n::t("Italic"))
                .on_hover_text(crate::i18n::t("Slants glyphs ~13° to the right"));
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Color:"));
            color_edit_u8(ui, &mut t.style.color);
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Align:"));
            ui.selectable_value(&mut t.style.align, TextAlign::Left, crate::i18n::t("Left"));
            ui.selectable_value(&mut t.style.align, TextAlign::Center, crate::i18n::t("Center"));
            ui.selectable_value(&mut t.style.align, TextAlign::Right, crate::i18n::t("Right"));
        });
    });
    ui.add_space(4.0);

    // ─── Stroke (glyph outline) ───────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Stroke")).size(12.0).strong().color(Color32::from_rgb(255, 200, 120)),
    ).default_open(true).show(ui, |ui| {
        let mut has_outline = t.style.outline.is_some();
        ui.checkbox(&mut has_outline, crate::i18n::t("Stroke text"));
        if has_outline && t.style.outline.is_none() {
            t.style.outline = Some([0, 0, 0]);
            if t.style.outline_width <= 0.0 { t.style.outline_width = 4.0; }
        }
        if !has_outline {
            t.style.outline = None;
        }

        if let Some(oc) = t.style.outline.as_mut() {
            ui.horizontal(|ui| {
                ui.label(crate::i18n::t("Color:"));
                color_edit_u8(ui, oc);
                ui.label(crate::i18n::t("Width:"));
                ui.add(egui::DragValue::new(&mut t.style.outline_width)
                    .range(0.0..=20.0).speed(0.1));
            });
        }
    });
    ui.add_space(4.0);

    // ─── Background plate ─────────────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Background plate")).size(12.0).strong().color(Color32::from_rgb(180, 255, 180)),
    ).default_open(true).show(ui, |ui| {
        let mut has_box = t.style.box_color.is_some();
        ui.checkbox(&mut has_box, crate::i18n::t("Enable plate"));
        if has_box && t.style.box_color.is_none() {
            t.style.box_color = Some([255, 255, 255]);
        }
        if !has_box {
            t.style.box_color = None;
        }

        if t.style.box_color.is_some() {
            ui.horizontal(|ui| {
                ui.label("Type:");
                egui::ComboBox::from_id_source("text_box_kind")
                    .selected_text(format!("{:?}", t.style.box_kind))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Solid, crate::i18n::t("Solid"));
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Gradient, crate::i18n::t("Gradient"));
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::OutlineOnly, crate::i18n::t("Outline only"));
                        ui.selectable_value(
                            &mut t.style.box_kind,
                            TextBoxKind::Wrap,
                            crate::i18n::t("Wrap (per-line)"),
                        )
                        .on_hover_text(crate::i18n::t(
                            "Each line of text gets its own plate hugging that line's width — \
                             produces the uneven, line-following background look used on \
                             title-card memes.",
                        ));
                        ui.selectable_value(
                            &mut t.style.box_kind,
                            TextBoxKind::FitText,
                            crate::i18n::t("Fit text"),
                        )
                        .on_hover_text(crate::i18n::t(
                            "Plate hugs the text glyphs as tightly as possible, ignoring \
                             box_padding and box_extra_left/right. Use this when the user \
                             wants a halo right around the text instead of a container-shaped \
                             background.",
                        ));
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::None, crate::i18n::t("None (text only)"));
                    });
            });

            if matches!(t.style.box_kind, TextBoxKind::Solid | TextBoxKind::Gradient | TextBoxKind::Wrap) {
                if let Some(bc) = t.style.box_color.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::t("Color:")); color_edit_u8(ui, bc);
                    });
                }
            }
            if matches!(t.style.box_kind, TextBoxKind::Gradient) {
                if t.style.box_gradient_end.is_none() {
                    t.style.box_gradient_end = Some([60, 60, 60]);
                }
                if let Some(end) = t.style.box_gradient_end.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::t("Gradient end:")); color_edit_u8(ui, end);
                    });
                }
            }

            ui.add(egui::Slider::new(&mut t.style.box_opacity, 0.0..=1.0).text(crate::i18n::t("Opacity")));
            ui.add(egui::Slider::new(&mut t.style.box_padding, 0.0..=80.0).text(crate::i18n::t("Padding")));
            ui.add(egui::Slider::new(&mut t.style.box_corner_radius, 0.0..=80.0).text(crate::i18n::t("Corner radius")));

            // Asymmetric horizontal extension: widens the plate to the
            // left or to the right WITHOUT changing the text scale.
            // Combined with TextAlign::{Left,Center,Right}, this lets
            // the user place the text anywhere inside a wider banner —
            // exactly the use-case the previous "scale only" controls
            // couldn't address.
            ui.label(
                RichText::new(crate::i18n::t("Asymmetric width (px)"))
                    .size(10.0)
                    .color(COL_TEXT_DIM),
            );
            ui.add(
                egui::Slider::new(&mut t.style.box_extra_left, 0.0..=400.0)
                    .text(crate::i18n::t("Extra left")),
            );
            ui.add(
                egui::Slider::new(&mut t.style.box_extra_right, 0.0..=400.0)
                    .text(crate::i18n::t("Extra right")),
            );

            // Plate border (independent of glyph stroke)
            let mut has_border = t.style.box_outline_color.is_some() || t.style.box_outline_width > 0.0;
            ui.checkbox(&mut has_border, crate::i18n::t("Plate border"));
            if has_border && t.style.box_outline_color.is_none() {
                t.style.box_outline_color = Some([0, 0, 0]);
            }
            if !has_border {
                t.style.box_outline_color = None;
                t.style.box_outline_width = 0.0;
            }
            if let Some(boc) = t.style.box_outline_color.as_mut() {
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::t("Color:")); color_edit_u8(ui, boc);
                    ui.label(crate::i18n::t("Width:"));
                    ui.add(egui::DragValue::new(&mut t.style.box_outline_width)
                        .range(0.0..=20.0).speed(0.1));
                });
            }
        }
    });

    ui.add_space(8.0);
    inspector_modifiers(ui, &mut t.modifiers, ("text_mods", idx));
    // NOTE: the per-effect "Effect stack" UI is intentionally NOT
    // surfaced for text overlays. The live preview path (this file)
    // does not yet rasterise text into a buffer that `apply_effect_stack_cpu`
    // could process, so showing the editor here would advertise an
    // effect that visibly never runs. The model field
    // `TextOverlay::effects` is still preserved in the scene schema so
    // older project files load and re-save unchanged, and the eventual
    // preview-side effect plumbing can be re-enabled here without a
    // schema migration. (User requested either real effects or no UI.)

    // Layer/z-index actions are no longer exposed from the inspector — the
    // timeline track row order alone determines stacking.
    None
}

// Logical font-family options always shown at the top of the picker,
// regardless of what's installed on the host. They map to egui's
// built-in `FontFamily::{Proportional, Monospace}` and are guaranteed
// to render even when no TTFs are installed (e.g. headless CI).
// System fonts discovered by `system_fonts::available_families()` are
// appended below these in `inspector_text_overlay`.
const BUNDLED_FONTS: &[&str] = &[
    "Default",
    "Monospace",
];


/// Inspector for the render frame (output area). Exposes position,
/// rotation, and size in world pixels just like any other element.
fn inspector_render_frame(ui: &mut egui::Ui, state: &mut EditorState) {
    use crate::kf_anim;
    use memstroy_core::param_ids;
    let rf_t_local = state.playhead;
    let rf = &mut state.scene.render_frame;
    // Output resolution is fixed at 1080x1920 — every export goes through
    // `app.rs` which overrides the scene's resolution to that value, and
    // exposing a per-scene W/H knob misled users into thinking they were
    // changing the export size when in reality it only altered the
    // editor preview's logical extent. Force-reset any stale value the
    // user may have saved into the scene file from earlier builds.
    rf.resolution = [1080, 1920];
    let [rw, rh] = rf.resolution;

    ui.label(
        RichText::new(t("Render Frame"))
            .strong()
            .size(14.0)
            .color(Color32::from_rgb(255, 120, 120)),
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new(t("The output region. Move/resize/rotate it like any element."))
            .size(10.0)
            .color(COL_TEXT_DIM),
    );
    ui.label(
        RichText::new(format!("\u{2192} Output: {}\u{00D7}{} (fixed)", rw, rh))
            .size(10.0)
            .color(COL_TEXT_DIM),
    );
    ui.add_space(8.0);

    // Sample current values via the eased kf-aware sampler so the
    // displayed numbers reflect the playhead position, just like the
    // inspector does for actor / overlay layouts. Without this the
    // widgets always showed the first kf's value, even when later
    // kfs animated the frame off-screen.
    let cur_state = memstroy_core::keyframe::sample(&rf.layout, rf_t_local)
        .unwrap_or_default();
    let mut new_pos_x = cur_state.pos.x;
    let mut new_pos_y = cur_state.pos.y;
    let mut new_rot = cur_state.rotation_deg;
    // Inspector exposes the inverse of `zoom` as "Scale" — see the
    // comment on the slider below.
    let mut new_scale = 1.0_f32 / cur_state.zoom.max(1e-4);

    // ─── Position ─────────────────────────────────────────────
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(
            ui,
            &mut rf.animated_params,
            param_ids::POS_X,
            ("rf_pos_x",),
        );
        ui.label(t("X:"));
        let r_x = ui.add(egui::DragValue::new(&mut new_pos_x).speed(0.5));
        kf_anim::animated_toggle(
            ui,
            &mut rf.animated_params,
            param_ids::POS_Y,
            ("rf_pos_y",),
        );
        ui.label(t("Y:"));
        let r_y = ui.add(egui::DragValue::new(&mut new_pos_y).speed(0.5));
        if r_x.changed() {
            kf_anim::write_render_frame_param(
                &mut rf.layout,
                &mut rf.animated_params,
                rf_t_local,
                param_ids::POS_X,
                false,
                |s| s.pos.x = new_pos_x,
            );
        }
        if r_y.changed() {
            kf_anim::write_render_frame_param(
                &mut rf.layout,
                &mut rf.animated_params,
                rf_t_local,
                param_ids::POS_Y,
                false,
                |s| s.pos.y = new_pos_y,
            );
        }
    });
    ui.add_space(4.0);

    // ─── Rotation ─────────────────────────────────────────────
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(
            ui,
            &mut rf.animated_params,
            param_ids::ROTATION,
            ("rf_rot",),
        );
        ui.label(t("Rotation"));
        let prev_rot = new_rot;
        circular_rotation_widget(ui, ("rf_rot",), &mut new_rot, 80.0);
        let mut dial_changed = (new_rot - prev_rot).abs() > 1.0e-4;
        ui.vertical(|ui| {
            let r = ui.add(
                egui::DragValue::new(&mut new_rot)
                    .range(-3600.0..=3600.0)
                    .speed(0.5)
                    .suffix("\u{00B0}")
                    .fixed_decimals(1),
            );
            if r.changed() { dial_changed = true; }
            if ui.small_button("0\u{00B0}").clicked() {
                new_rot = 0.0;
                dial_changed = true;
            }
        });
        if dial_changed {
            kf_anim::write_render_frame_param(
                &mut rf.layout,
                &mut rf.animated_params,
                rf_t_local,
                param_ids::ROTATION,
                false,
                |s| s.rotation_deg = new_rot,
            );
        }
    });
    ui.add_space(4.0);

    // ─── Scale (= 1 / zoom) ───────────────────────────────────
    // Scale here is the inverse of the legacy `zoom` field: scale = 1
    // means the frame's world size matches the output resolution 1:1;
    // scale > 1 enlarges the frame on the canvas; scale < 1 shrinks
    // it. We expose this as the user-facing concept because
    // "scale" reads more intuitively for an animatable element than
    // "zoom" did.
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(
            ui,
            &mut rf.animated_params,
            param_ids::SCALE,
            ("rf_scale",),
        );
        let r = ui.add(
            egui::Slider::new(&mut new_scale, 0.1..=20.0)
                .text(t("Scale"))
                .logarithmic(true),
        );
        if r.changed() {
            let new_zoom = (1.0 / new_scale.max(1e-4)).clamp(0.001, 1000.0);
            kf_anim::write_render_frame_param(
                &mut rf.layout,
                &mut rf.animated_params,
                rf_t_local,
                param_ids::SCALE,
                false,
                |s| s.zoom = new_zoom,
            );
        }
    });

    // ─── Per-param keyframe strips ──────────────────────────────
    // Render-frame keyframes used to show up here in the inspector,
    // duplicated with the per-clip diamonds in the timeline. Per
    // user request the strips now live ONLY under the dedicated
    // "Render Frame" row in the layer panel, mirroring every other
    // layer's keyframe rows. The inspector keeps the toggle
    // diamonds + value widgets above so users can still author kfs
    // from here; they're just no longer shown as a separate strip.
    ui.add_space(6.0);

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    // Output resolution / W,H widgets removed: render is always
    // performed at 1080x1920 and per-scene resolution edits were a
    // common source of "exported video doesn't match preview"
    // confusion. The legacy `rf.resolution` field is force-reset to
    // [1080, 1920] above so legacy scene files can no longer carry
    // a stale value through the inspector.

    // Animation modifiers (wobble/shake/pulse/spin) — perturb the
    // render-frame's eased keyframe state at preview/export time so the
    // user can add live camera-style motion without authoring every kf.
    ui.add_space(10.0);
    inspector_modifiers(ui, &mut rf.modifiers, "rf_mods");
    ui.add_space(8.0);
    // Render frame is scene-time anchored — effect kfs use scene-time.
    inspector_effect_stack(ui, &mut rf.effects, "rf_fx", rf_t_local);
}

fn inspector_background(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let b = &mut state.scene.backgrounds[i];
    ui.label(RichText::new(format!("{}: {}", t("Background"), b.id)).strong().size(14.0).color(COL_CLIP_BG));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(t("Start:"));
        ui.add(egui::DragValue::new(&mut b.start).speed(0.02).suffix("s"));
        ui.label(t("Duration:"));
        ui.add(egui::DragValue::new(&mut b.duration).speed(0.02).suffix("s"));
    });
    egui::ComboBox::from_label("Fit")
        .selected_text(format!("{:?}", b.fit))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.fit, Fit::Cover, "Cover");
            ui.selectable_value(&mut b.fit, Fit::Contain, "Contain");
            ui.selectable_value(&mut b.fit, Fit::Stretch, "Stretch");
            ui.selectable_value(&mut b.fit, Fit::Original, "Original");
        });
    egui::ComboBox::from_label("Transition")
        .selected_text(format!("{:?}", b.transition))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.transition, Transition::Cut, "Cut");
            ui.selectable_value(&mut b.transition, Transition::Snap, "Snap");
            ui.selectable_value(&mut b.transition, Transition::Fade, "Fade");
        });
}

fn inspector_audio(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    use crate::i18n::t;
    let _ = state.scene.output.duration;

    // Snapshot the kf-highlight up front so the per-param rows can
    // gold-flash their labels when the user just clicked a kf in the
    // timeline — mirrors what `inspector_actor_transform` does. We
    // clone instead of borrowing because the body re-borrows
    // `state.scene.audio[i]` mutably below.
    let kf_highlight = state.kf_highlight.clone();

    // Snapshot fields needed for the post-edit cascade so the borrow on
    // `audio` can end before we touch the actor / frame-cache vecs.
    let parent_actor_id: Option<String>;
    let speed_changed: bool;
    let new_speed_value: f32;
    {
        let audio = &mut state.scene.audio[i];
        parent_actor_id = audio.parent_actor.clone();

        ui.label(RichText::new(format!("{}: {}", t("Audio"), audio.id)).strong().size(14.0).color(COL_CLIP_AUDIO));
        ui.add_space(4.0);

        // Clip-local time at the playhead — keyframes for volume / speed
        // are stored in clip-local seconds so the same edits apply when the
        // user moves the audio along the timeline.
        let t_local = (state.playhead - audio.t_in).max(0.0);

        // ── Volume ───────────────────────────────────────────────────────
        // Volume uses the *numeric* widget (DragValue) so the user can
        // type / drag a precise value, matching how Speed works. The
        // earlier Slider version was the user's "volume should not be
        // a slider, it should be a value like Speed" complaint.
        inspector_audio_param_ex(
            ui,
            i,
            t("Volume"),
            "volume",
            0.0..=4.0,
            false, // not logarithmic
            true,  // numeric DragValue
            t_local,
            &mut audio.volume,
            &mut audio.volume_kfs,
            &mut audio.animated_params,
            &kf_highlight,
        );

        // ── Speed (DragValue — no slider so the user can dial in
        // arbitrary multipliers) ────────────────────────────────────────
        // When changed, the bar on the timeline shrinks/expands to
        // reflect the new playback length, mirroring the new video
        // behaviour. Bound layers (an actor that owns this audio's
        // `parent_actor` link) are updated by the cascade below.
        //
        // Speed cannot delegate to `inspector_audio_param_ex` because it
        // has unique side effects: editing the value resizes the visible
        // window via `t_out` (using the cached "1× length") and
        // cascades onto the linked actor below. The widget therefore
        // stays inline but uses the SAME shared primitives every other
        // inspector relies on — `kf_anim::animated_toggle` for the
        // painted diamond and `param_label_str` for the gold-highlight
        // label.
        let old_speed = audio.speed.max(0.0001);
        let cur_dur = match audio.t_out {
            Some(o) => (o - audio.t_in).max(0.05),
            None => 0.0,
        };
        let one_x_dur = cur_dur * old_speed;
        let mut new_speed = audio.speed;
        let mut speed_dirty = false;
        ui.horizontal(|ui| {
            // Shared painted-diamond toggle. Seeds a kf at t=0 the
            // moment animation is turned on so the per-param strip
            // below has something to drag, mirroring what the volume
            // / pitch / pan rows do via `inspector_audio_param_ex`.
            let was_on = audio.animated_params.contains("speed");
            let _toggled = crate::kf_anim::animated_toggle(
                ui,
                &mut audio.animated_params,
                "speed",
                ("audio_param", i, "speed"),
            );
            if !was_on
                && audio.animated_params.contains("speed")
                && audio.speed_kfs.is_empty()
            {
                audio.speed_kfs.push(memstroy_core::Keyframe::new(0.0, audio.speed));
            }

            let speed_animated = audio.animated_params.contains("speed");
            ui.label(param_label_str(kf_highlight.is_active("speed"), t("Speed")));
            // When animated, sample the kf track at the playhead so the
            // displayed value tracks the animation curve like the other
            // audio params do; otherwise show the static value.
            let mut display = if speed_animated && !audio.speed_kfs.is_empty() {
                memstroy_core::keyframe::sample(&audio.speed_kfs, t_local)
                    .unwrap_or(audio.speed)
            } else {
                new_speed
            };
            let resp = ui.add(
                egui::DragValue::new(&mut display)
                    .speed(0.01)
                    .range(0.05..=16.0)
                    .fixed_decimals(3)
                    .suffix("x"),
            )
            .on_hover_text(t(
                "Numeric playback speed. The clip bar on the timeline shrinks when speeding up and stretches when slowing down. If this audio is bound to a video clip, both update together.",
            ));
            if resp.changed() && display.is_finite() && display > 0.0 {
                if speed_animated {
                    if audio.speed_kfs.is_empty() {
                        audio.speed_kfs.push(memstroy_core::Keyframe::new(0.0, audio.speed));
                    }
                    memstroy_core::upsert_keyframe(&mut audio.speed_kfs, t_local, display);
                    // The static field still drives the timeline's
                    // visible duration; treat the keyframe edit as
                    // "set the static to whatever the user just typed
                    // at the playhead" so the bar resizes too.
                    audio.speed = display.max(0.05);
                    new_speed = audio.speed;
                    speed_dirty = true;
                } else {
                    audio.speed = display.max(0.05);
                    new_speed = audio.speed;
                    speed_dirty = true;
                }
            }
            if ui.small_button("1\u{00D7}").on_hover_text(t("Reset to 1.0x")).clicked() {
                audio.speed = 1.0;
                new_speed = 1.0;
                speed_dirty = true;
            }
        });
        // Per-param strip for speed kfs — same widget the actor /
        // overlay / render-frame inspectors use.
        let speed_animated = audio.animated_params.contains("speed");
        if speed_animated && !audio.speed_kfs.is_empty() {
            let max_kf_t = audio.speed_kfs.iter().map(|k| k.t).fold(0.0_f32, f32::max);
            let dur = max_kf_t.max(t_local).max(1.0);
            let times: Vec<f32> = audio.speed_kfs.iter().map(|k| k.t).collect();
            let easings: Vec<memstroy_core::Easing> =
                audio.speed_kfs.iter().map(|k| k.easing).collect();
            let interaction = crate::kf_anim::keyframe_strip(
                ui,
                &times,
                &easings,
                dur,
                Some(t_local.max(0.0)),
                ("audio_kf_strip", i, "speed"),
            );
            crate::kf_anim::apply_strip_to_f32_kfs(&mut audio.speed_kfs, &interaction);
        }
        if speed_dirty && cur_dur > 0.0 {
            // Resize the visible window to match the new speed using
            // the cached "1× length" reference so the math is symmetric.
            let new_dur = (one_x_dur / audio.speed.max(0.0001)).max(0.05);
            audio.t_out = Some(audio.t_in + new_dur);
        }
        speed_changed = speed_dirty;
        new_speed_value = audio.speed;

        if audio.speed.abs() < 0.05 {
            audio.speed = 0.05;
        }
    }

    // ── Cascade speed onto the linked actor (and its bar width) ──
    if speed_changed {
        if let Some(parent) = parent_actor_id {
            let parent_idx = state
                .scene
                .actors
                .iter()
                .position(|a| a.id == parent);
            if let Some(ai) = parent_idx {
                state.scene.actors[ai].speed = new_speed_value.max(0.05);
                let source_duration = state
                    .frame_caches
                    .get(ai)
                    .filter(|fc| fc.is_ready())
                    .map(|fc| fc.duration)
                    .unwrap_or(0.0);
                let t_in = state.scene.actors[ai].t_in.unwrap_or(0.0);
                let source_start = state.scene.actors[ai].source_start.max(0.0);
                if source_duration > 0.0 {
                    let visible_dur = ((source_duration - source_start).max(0.0))
                        / new_speed_value.max(0.0001);
                    state.scene.actors[ai].t_out = Some(t_in + visible_dur.max(0.05));
                }
                sync_audio_to_actor(state, ai);
            }
        }
    }

    let audio = &mut state.scene.audio[i];
    let t_local = (state.playhead - audio.t_in).max(0.0);

    // ── Pitch / Pan / Mute (animatable) ──────────────────────────────
    // Pitch / pan / reverb / filter cutoffs go through the same
    // `inspector_audio_param` pipeline as volume so the user gets the
    // diamond toggle, animated playhead-tracking, and per-param kf
    // strip out of the box.
    ui.add_space(6.0);
    inspector_audio_param(
        ui,
        i,
        &crate::i18n::t("Pitch (semitones)"),
        "pitch",
        -24.0..=24.0,
        false,
        t_local,
        &mut audio.pitch_semitones,
        &mut audio.pitch_kfs,
        &mut audio.animated_params,
        &kf_highlight,
    );
    inspector_audio_param(
        ui,
        i,
        &crate::i18n::t("Pan"),
        "pan",
        -1.0..=1.0,
        false,
        t_local,
        &mut audio.pan,
        &mut audio.pan_kfs,
        &mut audio.animated_params,
        &kf_highlight,
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut audio.mute, crate::i18n::t("Mute"));
    });

    // ── Reverb (animatable) ──────────────────────────────────────────
    ui.add_space(6.0);
    ui.label(RichText::new(crate::i18n::t("Audio effects")).strong().size(12.0).color(COL_TEXT_DIM));
    inspector_audio_param(
        ui,
        i,
        &crate::i18n::t("Reverb"),
        "reverb",
        0.0..=1.0,
        false,
        t_local,
        &mut audio.reverb,
        &mut audio.reverb_kfs,
        &mut audio.animated_params,
        &kf_highlight,
    );

    // ── Filters (animatable cutoffs) ─────────────────────────────────
    ui.add_space(6.0);
    ui.label(RichText::new(crate::i18n::t("Filters")).strong().size(12.0).color(COL_TEXT_DIM));

    // Low-pass: a checkbox owns whether the filter is enabled. When ON
    // the cutoff Hz row is keyframable through the same widget as the
    // other params; we mirror the slider's value into the `Option<u32>`
    // static field so disabling the filter still reads the user's last
    // chosen cutoff back.
    ui.horizontal(|ui| {
        let mut lp_on = audio.low_pass_hz.is_some();
        if ui.checkbox(&mut lp_on, crate::i18n::t("Enable low-pass filter")).changed() {
            audio.low_pass_hz = if lp_on { Some(8000) } else { None };
        }
    });
    if audio.low_pass_hz.is_some() {
        // Pull the Option<u32> static value into a dedicated f32 mirror
        // so `inspector_audio_param` can treat it like every other
        // animatable knob. The slider edit writes back to the option
        // when not animated; the kf path stores the f32 value.
        let mut lp_static_f32 = audio.low_pass_hz.map(|v| v as f32).unwrap_or(8000.0);
        inspector_audio_param(
            ui,
            i,
            &crate::i18n::t("Low-pass cutoff (Hz)"),
            "low_pass",
            100.0..=20000.0,
            true, // logarithmic — wide audible-range scale
            t_local,
            &mut lp_static_f32,
            &mut audio.low_pass_kfs,
            &mut audio.animated_params,
            &kf_highlight,
        );
        // Persist the (possibly edited) static value back as u32.
        audio.low_pass_hz = Some(lp_static_f32.clamp(20.0, 22000.0) as u32);
    }

    ui.horizontal(|ui| {
        let mut hp_on = audio.high_pass_hz.is_some();
        if ui.checkbox(&mut hp_on, crate::i18n::t("Enable high-pass filter")).changed() {
            audio.high_pass_hz = if hp_on { Some(120) } else { None };
        }
    });
    if audio.high_pass_hz.is_some() {
        let mut hp_static_f32 = audio.high_pass_hz.map(|v| v as f32).unwrap_or(120.0);
        inspector_audio_param(
            ui,
            i,
            &crate::i18n::t("High-pass cutoff (Hz)"),
            "high_pass",
            20.0..=8000.0,
            true,
            t_local,
            &mut hp_static_f32,
            &mut audio.high_pass_kfs,
            &mut audio.animated_params,
            &kf_highlight,
        );
        audio.high_pass_hz = Some(hp_static_f32.clamp(20.0, 22000.0) as u32);
    }

    // (Reset audio effects button removed — destructive bulk edits are
    // better expressed via undo, and the per-row animation toggle now
    // gives users finer-grained control over which params they want to
    // clear.)

    ui.add_space(6.0);
    // ── Actor link / unlink controls ──
    //
    // Shows the current binding and exposes a single button to attach
    // (when standalone) or detach (when bound). When bound, every
    // movement / trim of the parent actor mirrors onto this audio
    // track, and the reverse direction is now also wired up so the
    // user can drag the audio bar and see the actor follow.
    //
    // Attach uses a tiny ComboBox listing every actor that doesn't
    // already have a bound audio track — picking one writes the
    // `parent_actor` field. Detach simply clears the binding.
    let parent_now = audio.parent_actor.clone();
    ui.label(RichText::new(t("Link to actor")).strong().size(12.0).color(COL_TEXT_DIM));
    if let Some(parent_id) = parent_now.as_ref() {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("\u{1F517} {}", parent_id))
                    .size(11.0)
                    .color(Color32::from_rgb(180, 220, 180)),
            );
            if ui
                .small_button(t("Unlink"))
                .on_hover_text(t(
                    "Detach this audio from its parent actor. The actor stops syncing and the audio becomes standalone.",
                ))
                .clicked()
            {
                audio.parent_actor = None;
            }
        });
        ui.label(
            RichText::new(t(
                "Bound to an actor — moves and trims with its parent clip (and vice versa).",
            ))
            .size(10.0)
            .italics()
            .color(COL_TEXT_DIM),
        );
    } else {
        // Build the list of attach candidates from a snapshot so we can
        // mutate audio.parent_actor inside the closure without a double
        // borrow.
        let candidates: Vec<String> = state
            .scene
            .actors
            .iter()
            .map(|a| a.id.clone())
            .collect();
        let pick_id = ui.id().with(("audio_link_pick", i));
        let mut pick_idx: usize = ui.data(|d| d.get_temp(pick_id).unwrap_or(0));
        if pick_idx >= candidates.len() {
            pick_idx = 0;
        }
        // Re-borrow audio mutably (we dropped it via parent_now.clone() above
        // for the read; the previous mutable borrow ends with the if-arm).
        let audio = &mut state.scene.audio[i];
        ui.horizontal(|ui| {
            if candidates.is_empty() {
                ui.label(
                    RichText::new(t("No actors yet — add one first."))
                        .size(10.0)
                        .italics()
                        .color(COL_TEXT_DIM),
                );
            } else {
                let label_now = candidates
                    .get(pick_idx)
                    .cloned()
                    .unwrap_or_else(|| "—".into());
                egui::ComboBox::from_id_source(("audio_link_cmb", i))
                    .selected_text(label_now)
                    .width(160.0)
                    .show_ui(ui, |ui| {
                        for (k, name) in candidates.iter().enumerate() {
                            if ui.selectable_label(k == pick_idx, name).clicked() {
                                pick_idx = k;
                            }
                        }
                    });
                ui.data_mut(|d| d.insert_temp(pick_id, pick_idx));
                if ui
                    .small_button(t("Link"))
                    .on_hover_text(t(
                        "Bind this audio to the chosen actor so move/trim stays in sync both ways.",
                    ))
                    .clicked()
                {
                    if let Some(name) = candidates.get(pick_idx) {
                        audio.parent_actor = Some(name.clone());
                    }
                }
            }
        });
        ui.label(
            RichText::new(t("Standalone music — independent of any actor."))
                .size(10.0)
                .italics()
                .color(COL_TEXT_DIM),
        );
    }
}

/// Render one row of the audio inspector with a left-aligned animation
/// toggle (matching the per-param diamond used in the video inspector).
/// When the toggle is ON, edits to the slider write a keyframe at the
/// playhead's clip-local time; otherwise edits change the static value.
/// A horizontal keyframe strip is rendered directly under the slider
/// when the param is animated — drag a diamond to move its time, or
/// right-click for the interpolation menu. The "+ kf at playhead" /
/// "Clear kfs" buttons were removed: editing the slider while a kf is
/// at the playhead already upserts (so the button was redundant), and
/// clearing all kfs is reachable through Delete on the timeline.
#[allow(clippy::too_many_arguments)]
fn inspector_audio_param(
    ui: &mut egui::Ui,
    audio_idx: usize,
    label: &str,
    param_id: &str,
    range: std::ops::RangeInclusive<f32>,
    logarithmic: bool,
    t_local: f32,
    static_value: &mut f32,
    kfs: &mut Vec<memstroy_core::Keyframe<f32>>,
    animated: &mut std::collections::BTreeSet<String>,
    kf_highlight: &crate::kf_anim::KfHighlight,
) {
    inspector_audio_param_ex(
        ui, audio_idx, label, param_id, range, logarithmic, false, t_local,
        static_value, kfs, animated, kf_highlight,
    );
}

/// Extended audio param row that lets the caller request a numeric
/// `DragValue` widget instead of the default `Slider`. The user wants
/// volume to behave the same way as Speed — a precise typed/dragged
/// value rather than a constrained slider — so we keep a thin
/// dispatch on top of the legacy 9-arg helper for backward compat.
///
/// Visually this row is built out of the SAME primitives as the actor /
/// overlay / render-frame inspectors:
///
///   - [`kf_anim::animated_toggle`] for the per-param "is animated"
///     diamond (painted directly because egui's default font doesn't
///     render the Unicode diamond glyphs — the previous Unicode-glyph
///     toggle showed up as an empty square on most installs).
///   - [`param_label_str`] for the label so it gets the gold highlight
///     flash when the user clicks the matching kf in the timeline.
///   - [`kf_anim::keyframe_strip`] + [`kf_anim::apply_strip_to_f32_kfs`]
///     for the time-graph under each animated param so the user can
///     drag kf times and right-click for interpolation flavour.
#[allow(clippy::too_many_arguments)]
fn inspector_audio_param_ex(
    ui: &mut egui::Ui,
    audio_idx: usize,
    label: &str,
    param_id: &str,
    range: std::ops::RangeInclusive<f32>,
    logarithmic: bool,
    numeric: bool,
    t_local: f32,
    static_value: &mut f32,
    kfs: &mut Vec<memstroy_core::Keyframe<f32>>,
    animated: &mut std::collections::BTreeSet<String>,
    kf_highlight: &crate::kf_anim::KfHighlight,
) {
    use crate::kf_anim;

    let is_animated_pre = animated.contains(param_id);

    // Display value: when animated, sample the kf track at the playhead
    // so the slider reflects the current animated value; otherwise the
    // static field.
    let mut display = if is_animated_pre && !kfs.is_empty() {
        memstroy_core::keyframe::sample(kfs, t_local).unwrap_or(*static_value)
    } else {
        *static_value
    };

    ui.horizontal(|ui| {
        // Shared painted-diamond toggle — same widget used by every
        // other inspector. Replaces the hand-rolled Unicode-glyph
        // toggle that rendered as an empty square on default fonts.
        let was_on = animated.contains(param_id);
        let _toggled = kf_anim::animated_toggle(
            ui,
            animated,
            param_id,
            ("audio_param", audio_idx, param_id),
        );
        // Seed a kf at clip-local t=0 the moment the user turns
        // animation ON so the per-param strip below has at least one
        // diamond to drag, mirroring the previous behaviour.
        if !was_on && animated.contains(param_id) && kfs.is_empty() {
            kfs.push(memstroy_core::Keyframe::new(0.0, *static_value));
        }

        ui.label(param_label_str(kf_highlight.is_active(param_id), label));

        let resp = if numeric {
            // Numeric mode (e.g. Volume): expose the value via DragValue
            // so the user can type or drag without being constrained to
            // the slider's visual gradient. Mirrors the Speed widget.
            ui.add(
                egui::DragValue::new(&mut display)
                    .range(range.clone())
                    .speed(0.01)
                    .fixed_decimals(3),
            )
        } else {
            let mut slider = egui::Slider::new(&mut display, range.clone());
            if logarithmic {
                slider = slider.logarithmic(true);
                if param_id == "speed" {
                    slider = slider.suffix("x");
                }
            }
            ui.add(slider)
        };

        let is_animated = animated.contains(param_id);
        if resp.changed() {
            if is_animated {
                if kfs.is_empty() {
                    kfs.push(memstroy_core::Keyframe::new(0.0, *static_value));
                }
                memstroy_core::upsert_keyframe(kfs, t_local, display);
            } else {
                *static_value = display;
            }
        }
    });

    // Per-parameter keyframe strip — drawn ONLY when the param is
    // animated. When the user toggles animation on without entering any
    // edits yet, the strip will show a single seed kf at t=0 (placed
    // by the toggle handler above) so they have something to drag.
    let is_animated = animated.contains(param_id);
    if is_animated && !kfs.is_empty() {
        // Use the larger of the clip's local playhead and the largest
        // kf time as the strip's right edge so the user can always see
        // every kf they've authored, even when the playhead is at 0.
        let max_kf_t = kfs.iter().map(|k| k.t).fold(0.0_f32, f32::max);
        let dur = max_kf_t.max(t_local).max(1.0).max(0.1);
        let times: Vec<f32> = kfs.iter().map(|k| k.t).collect();
        let easings: Vec<memstroy_core::Easing> =
            kfs.iter().map(|k| k.easing).collect();
        let interaction = kf_anim::keyframe_strip(
            ui,
            &times,
            &easings,
            dur,
            Some(t_local.max(0.0)),
            ("audio_kf_strip", audio_idx, param_id),
        );
        kf_anim::apply_strip_to_f32_kfs(kfs, &interaction);
    }
}


// ─── SNAP HELPER ─────────────────────────────────────────────────────

/// Snap a time value to the closest target if within threshold.
/// Returns the snapped value if close enough, otherwise returns `t` unchanged.
fn snap_time(t: f32, targets: &[f32], threshold: f32) -> f32 {
    let mut best = t;
    let mut best_dist = threshold;
    for &target in targets {
        let dist = (t - target).abs();
        if dist < best_dist {
            best = target;
            best_dist = dist;
        }
    }
    best
}

/// Collect all clip edges (start/end times) from the scene, excluding a specific actor index.
fn collect_clip_edges(state: &EditorState, exclude_actor: Option<usize>) -> Vec<f32> {
    let mut edges = Vec::new();
    let duration = state.scene.output.duration;

    for (i, a) in state.scene.actors.iter().enumerate() {
        if exclude_actor == Some(i) { continue; }
        edges.push(a.t_in.unwrap_or(0.0));
        edges.push(a.t_out.unwrap_or(duration));
    }
    for bg in &state.scene.backgrounds {
        edges.push(bg.start);
        edges.push(bg.start + bg.duration);
    }
    for ov in &state.scene.overlays {
        let (s, e) = match ov {
            Overlay::Text(t) => (t.t_in, t.t_out),
            Overlay::Image(im) => (im.t_in, im.t_out),
            Overlay::Video(v) => (v.t_in, v.t_out),
        };
        edges.push(s);
        edges.push(e);
    }
    for au in &state.scene.audio {
        edges.push(au.t_in);
        edges.push(au.t_out.unwrap_or(duration));
    }
    edges
}


// ─── OVERLAP TRIMMING ON LAYER ───────────────────────────────────────
//
// "On one layer, only one clip can exist at a time." When a clip is
// moved, trimmed, or freshly placed, the helpers below trim every
// other clip on the SAME timeline lane that ends up overlapping the
// mover. Four cases per overlapping victim:
//
//   1. Victim is fully inside the mover  → remove the victim entirely.
//   2. Mover is fully inside the victim → split the victim around the
//      mover (left half keeps `t_in`, right half keeps `t_out`).
//   3. Victim's right edge crosses mover's left edge → trim victim
//      right edge down to `mover.t_in`.
//   4. Victim's left edge crosses mover's right edge → trim victim
//      left edge up to `mover.t_out`.
//
// All writes happen inside the caller-supplied `mutate_drag` token so
// the entire compound change collapses into ONE undo step.
//
// `enforce_no_overlap_on_layer` returns the mover's possibly-shifted
// index in case its index changed (removals at lower indices, splits
// inserting before it, etc.). Callers that need to keep using the
// mover's index after the call should swap it for the returned value.

#[derive(Clone, Copy, Debug)]
pub(crate) enum MovedClipKind {
    Actor(usize),
    Overlay(usize),
    Audio(usize),
    Background(usize),
}

impl MovedClipKind {
    fn to_pending(self) -> crate::state::PendingOverlapMover {
        use crate::state::PendingOverlapMover as P;
        match self {
            MovedClipKind::Actor(i) => P::Actor(i),
            MovedClipKind::Overlay(i) => P::Overlay(i),
            MovedClipKind::Audio(i) => P::Audio(i),
            MovedClipKind::Background(i) => P::Background(i),
        }
    }
}

impl From<crate::state::PendingOverlapMover> for MovedClipKind {
    fn from(p: crate::state::PendingOverlapMover) -> Self {
        use crate::state::PendingOverlapMover as P;
        match p {
            P::Actor(i) => MovedClipKind::Actor(i),
            P::Overlay(i) => MovedClipKind::Overlay(i),
            P::Audio(i) => MovedClipKind::Audio(i),
            P::Background(i) => MovedClipKind::Background(i),
        }
    }
}

/// Defer a mover's overlap-trim pass to the end of the drag. The
/// caller supplies the same `MovedClipKind` it would otherwise pass
/// to `enforce_no_overlap_on_layer`; we record it on the timeline
/// drag state and the timeline panel's pointer-up handler drains the
/// queue, calling `enforce_no_overlap_on_layer` once per unique
/// mover. This keeps neighbouring clips intact while the user is
/// still dragging — they're only trimmed on release, the moment the
/// final position is committed.
fn defer_overlap_resolution(state: &mut EditorState, mover: MovedClipKind) {
    state.timeline_drag.pending_overlap.push(mover.to_pending());
}

// ─── TIMELINE MARQUEE (RUBBER-BAND) SELECTION ────────────────────────
//
// Drag from an empty area of the tracks viewport to lasso a group of
// clips. Pressing Ctrl/Shift while starting the drag *adds* to the
// existing multi-selection; otherwise it replaces the set. Implemented
// in screen-space because the timeline already operates in screen
// pixels — there's no benefit to converting through a world-space
// anchor like the canvas marquee uses.

/// Compute the X-range of a clip on the timeline given its (t_in, t_out)
/// in scene-time. Returns `Some((x0, x1))` when at least part of the clip
/// is visible on the current scroll / zoom; otherwise `None`.
fn clip_screen_x_range(
    t_in: f32,
    t_out: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
) -> Option<(f32, f32)> {
    let x0 = (t_in - scroll) * pps + track_left;
    let x1 = (t_out - scroll) * pps + track_left;
    let x0c = x0.clamp(track_left, track_right);
    let x1c = x1.clamp(track_left, track_right);
    if x1c > x0c + 0.001 {
        Some((x0c, x1c))
    } else {
        None
    }
}

/// Iterate every clip on the timeline (actors, overlays, audio,
/// backgrounds, render frame) and call `f(selection, screen_rect)`
/// when the clip has a visible screen rectangle on the current
/// scroll / zoom. Used by the marquee commit pass.
fn for_each_clip_screen_rect(
    state: &EditorState,
    track_rows: &[(f32, f32)],
    tracks_rect: egui::Rect,
    rf_row_h: f32,
    v_scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    mut f: impl FnMut(Selection, egui::Rect),
) {
    let scene_dur = state.scene.output.duration.max(0.0);

    // ── Render frame row sits above all real tracks ──
    {
        let row_top = tracks_rect.min.y - v_scroll;
        let row_bot = row_top + rf_row_h;
        if row_bot > tracks_rect.min.y - 1.0 && row_top < tracks_rect.max.y + 1.0 {
            if let Some((x0, x1)) =
                clip_screen_x_range(0.0, scene_dur, state.timeline_scroll, pps, track_left, track_right)
            {
                f(
                    Selection::RenderFrame,
                    egui::Rect::from_min_max(
                        egui::pos2(x0, row_top),
                        egui::pos2(x1, row_bot),
                    ),
                );
            }
        }
    }

    let video_indices: Vec<usize> = state.video_track_indices();
    let default_overlay_lane = if video_indices.len() >= 2 {
        video_indices[1]
    } else {
        video_indices.first().copied().unwrap_or(0)
    };
    let first_video_lane = video_indices.first().copied().unwrap_or(0);

    // ── Backgrounds (pinned to the topmost video lane in the panel) ──
    if let Some(&(top, bot)) = track_rows.first() {
        for (bi, bg) in state.scene.backgrounds.iter().enumerate() {
            if let Some((x0, x1)) = clip_screen_x_range(
                bg.start,
                bg.start + bg.duration,
                state.timeline_scroll,
                pps,
                track_left,
                track_right,
            ) {
                f(
                    Selection::Background(bi),
                    egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, bot)),
                );
            }
        }
        let _ = first_video_lane; // currently unused beyond the assignment
    }

    // ── Actors ──
    for (ai, actor) in state.scene.actors.iter().enumerate() {
        let lane = state
            .actor_track_assignments
            .get(&ai)
            .copied()
            .unwrap_or(first_video_lane);
        let Some(&(top, bot)) = track_rows.get(lane) else { continue; };
        let t_in = actor.t_in.unwrap_or(0.0);
        let t_out = actor.t_out.unwrap_or(scene_dur);
        if let Some((x0, x1)) = clip_screen_x_range(
            t_in, t_out, state.timeline_scroll, pps, track_left, track_right,
        ) {
            f(
                Selection::Actor(ai),
                egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, bot)),
            );
        }
    }

    // ── Overlays ──
    for (oi, ov) in state.scene.overlays.iter().enumerate() {
        let lane = state
            .overlay_track_assignments
            .get(&oi)
            .copied()
            .unwrap_or(default_overlay_lane);
        let Some(&(top, bot)) = track_rows.get(lane) else { continue; };
        let (t_in, t_out) = match ov {
            Overlay::Text(t) => (t.t_in, t.t_out),
            Overlay::Image(im) => (im.t_in, im.t_out),
            Overlay::Video(v) => (v.t_in, v.t_out),
        };
        if let Some((x0, x1)) = clip_screen_x_range(
            t_in, t_out, state.timeline_scroll, pps, track_left, track_right,
        ) {
            f(
                Selection::Overlay(oi),
                egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, bot)),
            );
        }
    }

    // ── Audio ──
    let audio_indices: Vec<usize> = state.audio_track_indices();
    for (aui, au) in state.scene.audio.iter().enumerate() {
        let lane = state
            .audio_track_assignments
            .get(&aui)
            .copied()
            .unwrap_or_else(|| {
                if audio_indices.is_empty() {
                    0
                } else {
                    audio_indices[aui % audio_indices.len()]
                }
            });
        let Some(&(top, bot)) = track_rows.get(lane) else { continue; };
        let t_in = au.t_in;
        let t_out = au.t_out.unwrap_or(scene_dur);
        if let Some((x0, x1)) = clip_screen_x_range(
            t_in, t_out, state.timeline_scroll, pps, track_left, track_right,
        ) {
            f(
                Selection::Audio(aui),
                egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, bot)),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn timeline_marquee_update(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    tracks_rect: egui::Rect,
    track_rows: &[(f32, f32)],
    rf_row_h: f32,
    v_scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
) {
    // Don't compete for the pointer with active clip / asset drags.
    let drag_in_flight = state.timeline_drag.dragging_clip.is_some()
        || state.asset_drag.dragging.is_some();

    let id = egui::Id::new(("timeline_marquee_interact",));
    // Calling `ui.interact()` with the same id as the early-registration
    // call at the top of `timeline()` updates the widget's WidgetRect
    // in place — egui's `WidgetRects::insert` REPLACES at the existing
    // position, preserving registration order so clips registered
    // between the two calls stay topmost in the hit-test. Without the
    // early registration this widget would be the topmost interactive
    // rectangle at every clip position, and egui would give every
    // clip click to the marquee instead of the per-clip handler —
    // exactly the "single click does not select" bug the user
    // reported. The lasso-on-drag and clear-on-empty-click branches
    // below still see this response normally.
    let resp = ui.interact(tracks_rect, id, egui::Sense::click_and_drag());

    // ── Marquee start guard ──
    //
    // The user explicitly does NOT want the marquee firing on a single
    // click — only on a deliberate drag from empty space. egui's
    // `drag_started` already debounces by an internal pixel threshold,
    // but we add an extra ~5 px of slack on top so a tiny twitch
    // between press and release stays a click and never paints a
    // 1×1 lasso that wipes the selection. The lasso is still committed
    // on release with the standard 2-px corner test.
    const MARQUEE_MIN_TRAVEL_PX: f32 = 5.0;

    // Detect the first frame of a press that lands inside the tracks
    // viewport. We use the raw input layer (not `resp.drag_started()`)
    // because we want to react on the very first frame of the press,
    // BEFORE the user has moved past the drag threshold — that way a
    // pure click on empty space (no movement at all) still ends up
    // setting `timeline_marquee_pending` and the release branch can
    // treat it as "click on empty area → clear selection".
    let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
    let press_origin = ui.input(|i| i.pointer.press_origin());

    if state.timeline_marquee.is_none()
        && state.timeline_marquee_pending.is_none()
        && primary_pressed
        && !drag_in_flight
    {
        if let Some(p) = press_origin {
            // Reject presses on the header column / scrollbars by
            // checking that the press point lies inside the tracks
            // rectangle proper.
            if tracks_rect.contains(p) {
                // Walk the on-screen clip rects and bail out if the
                // press lands on top of any of them. The press will
                // then propagate to the per-clip drag handler instead
                // of being captured here. Use the PRESS ORIGIN
                // (not the live interact pos) so a small mouse
                // wobble during the press doesn't reclassify a
                // clip-click as a marquee start.
                let mut on_clip = false;
                for_each_clip_screen_rect(
                    state,
                    track_rows,
                    tracks_rect,
                    rf_row_h,
                    v_scroll,
                    pps,
                    track_left,
                    track_right,
                    |_sel, clip_rect| {
                        if clip_rect.contains(p) {
                            on_clip = true;
                        }
                    },
                );
                // Also reject presses that land on the playhead-scrub
                // strip — that gesture owns the press already.
                let on_playhead = state.timeline_scrubbing_playhead;
                if !on_clip && !on_playhead {
                    // Remember the press point. Until the pointer
                    // moves more than `MARQUEE_MIN_TRAVEL_PX` we
                    // keep it pending — a release without movement
                    // is treated as "click on empty area" and clears
                    // the selection; movement past the threshold
                    // promotes it into a real lasso.
                    state.timeline_marquee_pending = Some(p);
                }
            }
        }
    }
    // Suppress the unused warning for `resp.drag_started()`: it's now
    // entirely subsumed by the `primary_pressed` path above. The
    // response itself is still useful as a side-effect (it forces
    // the widget to be registered for hit-tests), so we keep the
    // call.
    let _ = resp;

    // Promote a pending press into a real marquee once the pointer has
    // travelled past the threshold.
    if state.timeline_marquee.is_none() {
        if let Some(start) = state.timeline_marquee_pending {
            if let Some(p) = ui
                .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
            {
                if (p - start).length() > MARQUEE_MIN_TRAVEL_PX {
                    state.timeline_marquee = Some(crate::state::TimelineMarquee {
                        start,
                        end: p,
                    });
                }
            }
        }
    }

    // Update / draw the live marquee while dragging.
    if let Some(mut m) = state.timeline_marquee {
        if let Some(p) = ui
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
        {
            m.end = p;
            state.timeline_marquee = Some(m);
        }
        let painter = ui.painter_at(tracks_rect);
        let rect = m.rect();
        painter.rect_filled(
            rect,
            Rounding::ZERO,
            Color32::from_rgba_premultiplied(255, 220, 80, 30),
        );
        painter.rect_stroke(
            rect,
            Rounding::ZERO,
            Stroke::new(1.0, Color32::from_rgb(255, 220, 80)),
        );
    }

    // Commit on pointer release.
    let any_pointer_down = ui.input(|i| i.pointer.any_down());
    if !any_pointer_down {
        // Drop any pending press that never grew into a real marquee.
        let pending = state.timeline_marquee_pending.take();
        if let Some(m) = state.timeline_marquee.take() {
            let rect = m.rect();
            // Reject zero-size lassos — treat as a click that just
            // clears the selection (the canvas marquee uses the same
            // 2-px threshold for parity).
            if rect.width().abs() < 2.0 || rect.height().abs() < 2.0 {
                return;
            }
            let extend = ui.input(|i| {
                i.modifiers.ctrl || i.modifiers.shift || i.modifiers.command
            });
            if !extend {
                state.canvas_selection.clear();
            }
            let mut hits: Vec<Selection> = Vec::new();
            for_each_clip_screen_rect(
                state,
                track_rows,
                tracks_rect,
                rf_row_h,
                v_scroll,
                pps,
                track_left,
                track_right,
                |sel, clip_rect| {
                    if rect.intersects(clip_rect) {
                        hits.push(sel);
                    }
                },
            );
            for h in hits {
                if !state.canvas_selection.contains(&h) {
                    state.canvas_selection.push(h);
                }
            }
            // Update the primary selection so the inspector still has
            // a focused element. Prefer keeping the existing primary
            // when it's still in the new set; otherwise pick the
            // first hit (which corresponds to the topmost / scene-
            // earliest clip caught by the lasso).
            if state.canvas_selection.is_empty() {
                if !extend {
                    state.selection = Selection::None;
                }
            } else if !state.canvas_selection.iter().any(|s| *s == state.selection) {
                state.selection = state.canvas_selection[0];
            }
            return;
        }
        // No marquee was ever started — but if the user pressed-and-
        // released on empty space (no clip, no playhead, no scrollbar
        // hit), treat that as "click to clear selection" per the
        // explicit user request. We re-run the on-clip check using
        // the captured pending press position so the clip click
        // handlers — which fired on the same press during their own
        // `interact()` calls — keep ownership of clip clicks.
        if let Some(p) = pending {
            if tracks_rect.contains(p) {
                let mut on_clip = false;
                for_each_clip_screen_rect(
                    state,
                    track_rows,
                    tracks_rect,
                    rf_row_h,
                    v_scroll,
                    pps,
                    track_left,
                    track_right,
                    |_sel, clip_rect| {
                        if clip_rect.contains(p) {
                            on_clip = true;
                        }
                    },
                );
                if !on_clip {
                    state.canvas_selection.clear();
                    state.multi_select.clear();
                    state.selection = Selection::None;
                }
            }
        }
    }
}

/// Enforce the "one clip per layer at a time" rule for the given
/// mover. Returns the (possibly updated) mover identifier.
pub(crate) fn enforce_no_overlap_on_layer(
    state: &mut EditorState,
    mover: MovedClipKind,
    token: u64,
) -> MovedClipKind {
    let duration = state.scene.output.duration.max(0.0);

    // Resolve the mover's lane and time window. Anything that doesn't
    // resolve (out-of-range index, zero-length window, etc.) bails out
    // early so we never touch other clips for an invalid mover.
    let (lane_kind, m_in, m_out) = match resolve_mover(state, mover, duration) {
        Some(t) => t,
        None => return mover,
    };
    if m_out - m_in < 1.0e-4 {
        return mover;
    }

    // ── First pass: classify every potential victim. ──
    // We do this read-only so we can decide on the SET of operations
    // before any indices shift. Splits / removals / trims are then
    // applied in an order that keeps the mover's index valid.
    let mut splits: Vec<SplitOp> = Vec::new();
    let mut removes: Vec<RemoveOp> = Vec::new();
    let mut trims: Vec<TrimOp> = Vec::new();

    classify_victims(
        state,
        lane_kind,
        mover,
        m_in,
        m_out,
        &mut splits,
        &mut removes,
        &mut trims,
    );

    // ── Apply the operations. ──
    //
    // Order matters because every Remove/Split potentially shifts the
    // mover's own index. We track that via `mover_kind`.
    let mut mover_kind = mover;

    // Splits first: they only apply to victims that contain the mover
    // (and there can be at most one such victim per lane). Splitting
    // inserts a new clip at victim_idx + 1; if victim_idx < mover_idx
    // (same kind), mover_idx must shift by +1.
    for op in splits {
        mover_kind = apply_split(state, op, mover_kind, token);
    }

    // Removes next, descending order so removing a higher index first
    // never invalidates lower ones we still want to remove.
    removes.sort_by(|a, b| b.victim_idx.cmp(&a.victim_idx));
    for op in removes {
        mover_kind = apply_remove(state, op, mover_kind, token);
    }

    // Trims last: pure mutations, no index shifts.
    for op in trims {
        apply_trim(state, op, token);
    }

    // Bound audio rows that follow a parent actor must stay in sync
    // with their actor's window after any trim / split — re-run the
    // bookkeeping once at the end.
    if let MovedClipKind::Actor(ai) = mover_kind {
        if ai < state.scene.actors.len() {
            sync_audio_to_actor(state, ai);
        }
    }

    mover_kind
}

/// Identifies a timeline "layer" — either a track-indexed video lane
/// (host of actors + overlays), an audio lane (host of audio rows), or
/// the global background lane (host of `Scene::backgrounds`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LayerKind {
    Video(usize),
    Audio(usize),
    Background,
}

#[derive(Clone, Copy, Debug)]
enum VictimKind {
    Actor(usize),
    Overlay(usize),
    Audio(usize),
    Background(usize),
}

#[derive(Clone, Copy, Debug)]
struct TrimOp {
    victim: VictimKind,
    /// New time window after trim — interpreted as `[new_t_in, new_t_out]`
    /// in scene-time. Background uses (start, start + duration).
    new_t_in: f32,
    new_t_out: f32,
}

#[derive(Clone, Copy, Debug)]
struct RemoveOp {
    victim: VictimKind,
    /// Pulled out so the post-pass can sort by descending index without
    /// matching on the enum.
    victim_idx: usize,
}

#[derive(Clone, Copy, Debug)]
struct SplitOp {
    victim: VictimKind,
    /// Cut the victim into a left half ending at `m_in` and a right
    /// half starting at `m_out`.
    m_in: f32,
    m_out: f32,
}

fn resolve_mover(
    state: &EditorState,
    mover: MovedClipKind,
    duration: f32,
) -> Option<(LayerKind, f32, f32)> {
    match mover {
        MovedClipKind::Actor(ai) => {
            if ai >= state.scene.actors.len() { return None; }
            let a = &state.scene.actors[ai];
            let lane = state
                .actor_track_assignments
                .get(&ai)
                .copied()
                .unwrap_or_else(|| {
                    state
                        .video_track_indices()
                        .first()
                        .copied()
                        .unwrap_or(0)
                });
            Some((
                LayerKind::Video(lane),
                a.t_in.unwrap_or(0.0),
                a.t_out.unwrap_or(duration),
            ))
        }
        MovedClipKind::Overlay(oi) => {
            if oi >= state.scene.overlays.len() { return None; }
            let (t_in, t_out) = match &state.scene.overlays[oi] {
                Overlay::Text(t) => (t.t_in, t.t_out),
                Overlay::Image(im) => (im.t_in, im.t_out),
                Overlay::Video(v) => (v.t_in, v.t_out),
            };
            let lane = state
                .overlay_track_assignments
                .get(&oi)
                .copied()
                .unwrap_or_else(|| default_overlay_lane(state));
            Some((LayerKind::Video(lane), t_in, t_out))
        }
        MovedClipKind::Audio(aui) => {
            if aui >= state.scene.audio.len() { return None; }
            let a = &state.scene.audio[aui];
            let lane = state
                .audio_track_assignments
                .get(&aui)
                .copied()
                .unwrap_or(0);
            Some((LayerKind::Audio(lane), a.t_in, a.t_out.unwrap_or(duration)))
        }
        MovedClipKind::Background(bi) => {
            if bi >= state.scene.backgrounds.len() { return None; }
            let bg = &state.scene.backgrounds[bi];
            Some((LayerKind::Background, bg.start, bg.start + bg.duration))
        }
    }
}

/// Mirror of `canvas_preview::default_overlay_track` — picks the second
/// video lane when one exists, else the first, else 0.
fn default_overlay_lane(state: &EditorState) -> usize {
    let video_tracks: Vec<usize> = (0..state.tracks.len())
        .filter(|i| state.tracks[*i].kind == TrackKind::Video)
        .collect();
    if video_tracks.len() >= 2 {
        video_tracks[1]
    } else if !video_tracks.is_empty() {
        video_tracks[0]
    } else {
        0
    }
}

fn classify_victims(
    state: &EditorState,
    lane: LayerKind,
    mover: MovedClipKind,
    m_in: f32,
    m_out: f32,
    splits: &mut Vec<SplitOp>,
    removes: &mut Vec<RemoveOp>,
    trims: &mut Vec<TrimOp>,
) {
    let duration = state.scene.output.duration.max(0.0);

    // Helper: given a victim's window, decide what to do.
    let mut classify =
        |victim: VictimKind, v_in: f32, v_out: f32, mover_skip: bool| {
            if mover_skip { return; }
            if v_out - v_in < 1.0e-4 { return; }
            // No overlap at all → leave it.
            if v_out <= m_in || v_in >= m_out {
                return;
            }
            let victim_idx = match victim {
                VictimKind::Actor(i) => i,
                VictimKind::Overlay(i) => i,
                VictimKind::Audio(i) => i,
                VictimKind::Background(i) => i,
            };
            // Victim is fully inside mover → remove.
            if v_in >= m_in && v_out <= m_out {
                removes.push(RemoveOp { victim, victim_idx });
                return;
            }
            // Mover is fully inside victim → split victim around mover.
            if v_in < m_in && v_out > m_out {
                splits.push(SplitOp { victim, m_in, m_out });
                return;
            }
            // Right-edge overlap: victim's tail intrudes into mover.
            if v_in < m_in && v_out > m_in && v_out <= m_out {
                trims.push(TrimOp {
                    victim,
                    new_t_in: v_in,
                    new_t_out: m_in,
                });
                return;
            }
            // Left-edge overlap: victim's head intrudes into mover.
            if v_in >= m_in && v_in < m_out && v_out > m_out {
                trims.push(TrimOp {
                    victim,
                    new_t_in: m_out,
                    new_t_out: v_out,
                });
            }
        };

    match lane {
        LayerKind::Video(track_idx) => {
            // Actors on the same video lane.
            let first_video = state
                .video_track_indices()
                .first()
                .copied()
                .unwrap_or(0);
            for ai in 0..state.scene.actors.len() {
                let assigned = state
                    .actor_track_assignments
                    .get(&ai)
                    .copied()
                    .unwrap_or(first_video);
                if assigned != track_idx { continue; }
                let actor = &state.scene.actors[ai];
                let t_in = actor.t_in.unwrap_or(0.0);
                let t_out = actor.t_out.unwrap_or(duration);
                let skip = matches!(mover, MovedClipKind::Actor(mai) if mai == ai);
                classify(VictimKind::Actor(ai), t_in, t_out, skip);
            }

            // Overlays on the same video lane.
            let default_lane = default_overlay_lane(state);
            for oi in 0..state.scene.overlays.len() {
                let assigned = state
                    .overlay_track_assignments
                    .get(&oi)
                    .copied()
                    .unwrap_or(default_lane);
                if assigned != track_idx { continue; }
                let (t_in, t_out) = match &state.scene.overlays[oi] {
                    Overlay::Text(t) => (t.t_in, t.t_out),
                    Overlay::Image(im) => (im.t_in, im.t_out),
                    Overlay::Video(v) => (v.t_in, v.t_out),
                };
                let skip = matches!(mover, MovedClipKind::Overlay(moi) if moi == oi);
                classify(VictimKind::Overlay(oi), t_in, t_out, skip);
            }
        }
        LayerKind::Audio(track_idx) => {
            for aui in 0..state.scene.audio.len() {
                let assigned = state
                    .audio_track_assignments
                    .get(&aui)
                    .copied()
                    .unwrap_or(0);
                if assigned != track_idx { continue; }
                let a = &state.scene.audio[aui];
                let t_in = a.t_in;
                let t_out = a.t_out.unwrap_or(duration);
                let skip = matches!(mover, MovedClipKind::Audio(mai) if mai == aui);
                classify(VictimKind::Audio(aui), t_in, t_out, skip);
            }
        }
        LayerKind::Background => {
            for bi in 0..state.scene.backgrounds.len() {
                let bg = &state.scene.backgrounds[bi];
                let t_in = bg.start;
                let t_out = bg.start + bg.duration;
                let skip = matches!(mover, MovedClipKind::Background(mbi) if mbi == bi);
                classify(VictimKind::Background(bi), t_in, t_out, skip);
            }
        }
    }
}

/// Apply a victim trim (left edge, right edge, or both) to the scene.
/// Mutates the scene in place inside a `mutate_drag` block keyed on
/// `token` so the operation stays inside the same undo group as the
/// caller's own writes.
fn apply_trim(state: &mut EditorState, op: TrimOp, token: u64) {
    let TrimOp { victim, new_t_in, new_t_out } = op;
    if new_t_out - new_t_in < 1.0e-4 {
        // Pathological case — would produce a zero-length clip. The
        // caller's classify pass shouldn't generate these, but be safe.
        return;
    }
    state.mutate_drag(token, |s| match victim {
        VictimKind::Actor(i) => {
            if i >= s.actors.len() { return; }
            let a = &mut s.actors[i];
            let old_in = a.t_in.unwrap_or(0.0);
            // Bump source_start when shifting the in-edge so the
            // visible content doesn't slip under the trim.
            let shift_in = new_t_in - old_in;
            if shift_in.abs() > 1.0e-6 {
                a.source_start = (a.source_start + shift_in).max(0.0);
            }
            a.t_in = Some(new_t_in);
            a.t_out = Some(new_t_out);
            a.layout
                .retain(|kf| kf.t >= new_t_in - 1.0e-3 && kf.t <= new_t_out + 1.0e-3);
            if a.layout.is_empty() {
                a.layout.push(memstroy_core::Keyframe::new(
                    new_t_in,
                    memstroy_core::ActorState::default(),
                ));
            }
        }
        VictimKind::Overlay(i) => {
            if i >= s.overlays.len() { return; }
            // Overlay kfs are clip-local. Shifting the in-edge requires
            // re-anchoring every kf by `-(new_t_in - old_t_in)`.
            let (old_t_in, old_t_out, layout): (
                f32,
                f32,
                &mut Vec<memstroy_core::Keyframe<memstroy_core::OverlayState>>,
            ) = match &mut s.overlays[i] {
                Overlay::Text(t) => {
                    let oi = t.t_in;
                    let oo = t.t_out;
                    t.t_in = new_t_in;
                    t.t_out = new_t_out;
                    (oi, oo, &mut t.layout)
                }
                Overlay::Image(im) => {
                    let oi = im.t_in;
                    let oo = im.t_out;
                    im.t_in = new_t_in;
                    im.t_out = new_t_out;
                    (oi, oo, &mut im.layout)
                }
                Overlay::Video(v) => {
                    let oi = v.t_in;
                    let oo = v.t_out;
                    v.t_in = new_t_in;
                    v.t_out = new_t_out;
                    (oi, oo, &mut v.layout)
                }
            };
            let shift = new_t_in - old_t_in;
            let _ = old_t_out;
            if shift.abs() > 1.0e-6 {
                for kf in layout.iter_mut() {
                    kf.t -= shift;
                }
            }
            let max_local = (new_t_out - new_t_in).max(0.0) + 1.0e-3;
            layout.retain(|kf| kf.t >= -1.0e-3 && kf.t <= max_local);
            for kf in layout.iter_mut() { kf.t = kf.t.max(0.0); }
            if layout.is_empty() {
                layout.push(memstroy_core::Keyframe::new(
                    0.0,
                    memstroy_core::OverlayState::default(),
                ));
            }
        }
        VictimKind::Audio(i) => {
            if i >= s.audio.len() { return; }
            let au = &mut s.audio[i];
            let old_in = au.t_in;
            let shift_in = new_t_in - old_in;
            if shift_in.abs() > 1.0e-6 {
                au.source_start = (au.source_start + shift_in).max(0.0);
            }
            au.t_in = new_t_in;
            au.t_out = Some(new_t_out);
        }
        VictimKind::Background(i) => {
            if i >= s.backgrounds.len() { return; }
            let bg = &mut s.backgrounds[i];
            bg.start = new_t_in;
            bg.duration = (new_t_out - new_t_in).max(0.05);
        }
    });
}

/// Apply a victim removal. Cleans up bookkeeping side-tables (frame
/// caches, audio waveforms, *_track_assignments) and shifts the
/// mover's index when the removed clip was the same kind and lived at
/// a smaller index.
fn apply_remove(
    state: &mut EditorState,
    op: RemoveOp,
    mover: MovedClipKind,
    token: u64,
) -> MovedClipKind {
    let mut mover_out = mover;
    let RemoveOp { victim, victim_idx: _ } = op;
    state.mutate_drag(token, |s| match victim {
        VictimKind::Actor(i) => {
            if i >= s.actors.len() { return; }
            s.actors.remove(i);
        }
        VictimKind::Overlay(i) => {
            if i >= s.overlays.len() { return; }
            s.overlays.remove(i);
        }
        VictimKind::Audio(i) => {
            if i >= s.audio.len() { return; }
            s.audio.remove(i);
        }
        VictimKind::Background(i) => {
            if i >= s.backgrounds.len() { return; }
            s.backgrounds.remove(i);
        }
    });
    // Side-tables that mirror scene Vec indices.
    match victim {
        VictimKind::Actor(i) => {
            if i < state.frame_caches.len() {
                state.frame_caches.remove(i);
            }
            shift_assignments_after_remove(&mut state.actor_track_assignments, i);
            if let MovedClipKind::Actor(mai) = mover_out {
                if i < mai { mover_out = MovedClipKind::Actor(mai - 1); }
            }
        }
        VictimKind::Overlay(i) => {
            shift_assignments_after_remove(&mut state.overlay_track_assignments, i);
            if let MovedClipKind::Overlay(moi) = mover_out {
                if i < moi { mover_out = MovedClipKind::Overlay(moi - 1); }
            }
        }
        VictimKind::Audio(i) => {
            if i < state.audio_waveforms.len() {
                state.audio_waveforms.remove(i);
            }
            shift_assignments_after_remove(&mut state.audio_track_assignments, i);
            if let MovedClipKind::Audio(mai) = mover_out {
                if i < mai { mover_out = MovedClipKind::Audio(mai - 1); }
            }
        }
        VictimKind::Background(i) => {
            if let MovedClipKind::Background(mbi) = mover_out {
                if i < mbi { mover_out = MovedClipKind::Background(mbi - 1); }
            }
        }
    }
    if state.selection == sel_for_victim(victim) {
        // Selected element vanished — drop the focus to None so the
        // inspector falls back to its "nothing selected" view.
        state.selection = Selection::None;
    }
    mover_out
}

fn sel_for_victim(victim: VictimKind) -> Selection {
    match victim {
        VictimKind::Actor(i) => Selection::Actor(i),
        VictimKind::Overlay(i) => Selection::Overlay(i),
        VictimKind::Audio(i) => Selection::Audio(i),
        VictimKind::Background(i) => Selection::Background(i),
    }
}

/// Decrement every map value pointing at an index >= `removed`. Map
/// entries with key == `removed` are deleted, and entries with key
/// > `removed` are re-keyed down by one — same scheme used by
/// `EditorState::insert_video_track_at_*` for inserts, but in reverse.
pub(crate) fn shift_assignments_after_remove(
    map: &mut std::collections::HashMap<usize, usize>,
    removed: usize,
) {
    let mut new_map: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(map.len());
    for (k, v) in map.iter() {
        if *k == removed { continue; }
        let nk = if *k > removed { *k - 1 } else { *k };
        new_map.insert(nk, *v);
    }
    *map = new_map;
}

/// Apply a victim split. The original clip becomes the LEFT half
/// (ends at `m_in`); a clone becomes the RIGHT half (starts at `m_out`)
/// and is inserted at index + 1. Track assignments are shifted so
/// existing entries at indices >= victim+1 move up by one, and the
/// new right-half clip inherits the same lane.
fn apply_split(
    state: &mut EditorState,
    op: SplitOp,
    mover: MovedClipKind,
    token: u64,
) -> MovedClipKind {
    let SplitOp { victim, m_in, m_out } = op;
    let mut mover_out = mover;
    match victim {
        VictimKind::Actor(i) => {
            if i >= state.scene.actors.len() { return mover_out; }
            // Clone first (immutable read), trim left and insert right.
            let original = state.scene.actors[i].clone();
            let mut right = original.clone();
            right.id = unique_actor_id_in_scene(&state.scene.actors, &right.id);
            // Right half: shift source_start so playback continues,
            // crop kfs to scene-time >= m_out (actor kfs are scene-time).
            let original_t_in = original.t_in.unwrap_or(0.0);
            let original_t_out = original.t_out.unwrap_or(state.scene.output.duration);
            right.t_in = Some(m_out);
            right.t_out = Some(original_t_out);
            right.source_start = original.source_start + (m_out - original_t_in).max(0.0);
            right.layout.retain(|kf| kf.t >= m_out - 1.0e-3);
            if right.layout.is_empty() {
                let last = original.layout.last().map(|k| k.value).unwrap_or_default();
                right.layout.push(memstroy_core::Keyframe::new(m_out, last));
            }
            // Bound audio (if any) belongs to the left half — wipe the
            // parent_actor binding on the right half so a later
            // sync_bound_audio_lanes doesn't try to follow it.
            // (The audio row stays attached to the original actor on
            // the left.)
            state.mutate_drag(token, |s| {
                // Trim left half.
                let a = &mut s.actors[i];
                a.t_out = Some(m_in);
                a.layout.retain(|kf| kf.t <= m_in + 1.0e-3);
                if a.layout.is_empty() {
                    a.layout.push(memstroy_core::Keyframe::new(
                        m_in,
                        memstroy_core::ActorState::default(),
                    ));
                }
                // Insert right half at i+1.
                s.actors.insert(i + 1, right);
            });
            // Side-table bookkeeping.
            shift_assignments_for_insert(&mut state.actor_track_assignments, i + 1);
            // The right half inherits the original's lane.
            let original_lane = state.actor_track_assignments.get(&i).copied();
            if let Some(lane) = original_lane {
                state.actor_track_assignments.insert(i + 1, lane);
            }
            // Frame caches: insert a placeholder at i+1 so subsequent
            // actors don't drift relative to their caches.
            if i + 1 <= state.frame_caches.len() {
                state.frame_caches.insert(
                    i + 1,
                    crate::video_cache::FrameCache::new(
                        std::path::PathBuf::new(),
                        i + 1,
                    ),
                );
            }
            // Mover index shift: any actor with index > i moves up by 1
            // because we inserted right-half AT i+1.
            if let MovedClipKind::Actor(mai) = mover_out {
                if mai >= i + 1 { mover_out = MovedClipKind::Actor(mai + 1); }
            }
        }
        VictimKind::Overlay(i) => {
            if i >= state.scene.overlays.len() { return mover_out; }
            let original = state.scene.overlays[i].clone();
            let mut right = original.clone();
            // Mutate left half + insert right.
            // Overlay kfs are clip-local: right's kfs need to subtract
            // (m_out - original.t_in) from each kf time so kf=0 maps
            // to the new t_in.
            let (original_t_in, original_t_out) = match &original {
                Overlay::Text(t) => (t.t_in, t.t_out),
                Overlay::Image(im) => (im.t_in, im.t_out),
                Overlay::Video(v) => (v.t_in, v.t_out),
            };
            let local_split = m_out - original_t_in;
            // Right-half kfs filtered + re-anchored.
            match &mut right {
                Overlay::Text(t) => {
                    t.id = unique_overlay_id_in_scene(&state.scene.overlays, &t.id);
                    t.t_in = m_out;
                    t.t_out = original_t_out;
                    t.layout.retain(|kf| kf.t >= local_split - 1.0e-3);
                    for kf in t.layout.iter_mut() { kf.t -= local_split; kf.t = kf.t.max(0.0); }
                    if t.layout.is_empty() {
                        t.layout.push(memstroy_core::Keyframe::new(
                            0.0,
                            memstroy_core::OverlayState::default(),
                        ));
                    }
                }
                Overlay::Image(im) => {
                    im.id = unique_overlay_id_in_scene(&state.scene.overlays, &im.id);
                    im.t_in = m_out;
                    im.t_out = original_t_out;
                    im.layout.retain(|kf| kf.t >= local_split - 1.0e-3);
                    for kf in im.layout.iter_mut() { kf.t -= local_split; kf.t = kf.t.max(0.0); }
                    if im.layout.is_empty() {
                        im.layout.push(memstroy_core::Keyframe::new(
                            0.0,
                            memstroy_core::OverlayState::default(),
                        ));
                    }
                }
                Overlay::Video(v) => {
                    v.id = unique_overlay_id_in_scene(&state.scene.overlays, &v.id);
                    v.t_in = m_out;
                    v.t_out = original_t_out;
                    v.source_start = v.source_start + local_split.max(0.0);
                    v.layout.retain(|kf| kf.t >= local_split - 1.0e-3);
                    for kf in v.layout.iter_mut() { kf.t -= local_split; kf.t = kf.t.max(0.0); }
                    if v.layout.is_empty() {
                        v.layout.push(memstroy_core::Keyframe::new(
                            0.0,
                            memstroy_core::OverlayState::default(),
                        ));
                    }
                }
            }
            let local_split_left = m_in - original_t_in;
            state.mutate_drag(token, |s| {
                // Trim left half.
                match &mut s.overlays[i] {
                    Overlay::Text(t) => {
                        t.t_out = m_in;
                        t.layout.retain(|kf| kf.t <= local_split_left + 1.0e-3);
                        if t.layout.is_empty() {
                            t.layout.push(memstroy_core::Keyframe::new(
                                0.0,
                                memstroy_core::OverlayState::default(),
                            ));
                        }
                    }
                    Overlay::Image(im) => {
                        im.t_out = m_in;
                        im.layout.retain(|kf| kf.t <= local_split_left + 1.0e-3);
                        if im.layout.is_empty() {
                            im.layout.push(memstroy_core::Keyframe::new(
                                0.0,
                                memstroy_core::OverlayState::default(),
                            ));
                        }
                    }
                    Overlay::Video(v) => {
                        v.t_out = m_in;
                        v.layout.retain(|kf| kf.t <= local_split_left + 1.0e-3);
                        if v.layout.is_empty() {
                            v.layout.push(memstroy_core::Keyframe::new(
                                0.0,
                                memstroy_core::OverlayState::default(),
                            ));
                        }
                    }
                }
                s.overlays.insert(i + 1, right);
            });
            shift_assignments_for_insert(&mut state.overlay_track_assignments, i + 1);
            let original_lane = state.overlay_track_assignments.get(&i).copied();
            if let Some(lane) = original_lane {
                state.overlay_track_assignments.insert(i + 1, lane);
            }
            if let MovedClipKind::Overlay(moi) = mover_out {
                if moi >= i + 1 { mover_out = MovedClipKind::Overlay(moi + 1); }
            }
        }
        VictimKind::Audio(i) => {
            if i >= state.scene.audio.len() { return mover_out; }
            let original = state.scene.audio[i].clone();
            let mut right = original.clone();
            right.id = unique_audio_id_in_scene(&state.scene.audio, &right.id);
            let original_t_in = original.t_in;
            let original_t_out =
                original.t_out.unwrap_or(state.scene.output.duration);
            right.t_in = m_out;
            right.t_out = Some(original_t_out);
            right.source_start = original.source_start + (m_out - original_t_in).max(0.0);
            // The right half drops its parent_actor binding so the
            // sync_audio_to_actor pass doesn't try to drag it back to
            // the left half's window. The user can re-bind manually.
            right.parent_actor = None;
            state.mutate_drag(token, |s| {
                let au = &mut s.audio[i];
                au.t_out = Some(m_in);
                s.audio.insert(i + 1, right);
            });
            shift_assignments_for_insert(&mut state.audio_track_assignments, i + 1);
            let original_lane = state.audio_track_assignments.get(&i).copied();
            if let Some(lane) = original_lane {
                state.audio_track_assignments.insert(i + 1, lane);
            }
            if i + 1 <= state.audio_waveforms.len() {
                state.audio_waveforms.insert(
                    i + 1,
                    crate::state::AudioWaveform::default(),
                );
            }
            if let MovedClipKind::Audio(mai) = mover_out {
                if mai >= i + 1 { mover_out = MovedClipKind::Audio(mai + 1); }
            }
        }
        VictimKind::Background(i) => {
            if i >= state.scene.backgrounds.len() { return mover_out; }
            let original = state.scene.backgrounds[i].clone();
            let mut right = original.clone();
            right.id = unique_background_id_in_scene(&state.scene.backgrounds, &right.id);
            right.start = m_out;
            right.duration = (original.start + original.duration - m_out).max(0.05);
            state.mutate_drag(token, |s| {
                s.backgrounds[i].duration = (m_in - s.backgrounds[i].start).max(0.05);
                s.backgrounds.insert(i + 1, right);
            });
            if let MovedClipKind::Background(mbi) = mover_out {
                if mbi >= i + 1 { mover_out = MovedClipKind::Background(mbi + 1); }
            }
        }
    }
    mover_out
}

fn shift_assignments_for_insert(
    map: &mut std::collections::HashMap<usize, usize>,
    inserted: usize,
) {
    let mut new_map: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(map.len() + 1);
    for (k, v) in map.iter() {
        let nk = if *k >= inserted { *k + 1 } else { *k };
        new_map.insert(nk, *v);
    }
    *map = new_map;
}

fn unique_actor_id_in_scene(actors: &[memstroy_core::Actor], base: &str) -> String {
    let mut candidate = format!("{}_R", base);
    let mut n = 2;
    while actors.iter().any(|a| a.id == candidate) {
        candidate = format!("{}_R{}", base, n);
        n += 1;
    }
    candidate
}

fn unique_overlay_id_in_scene(overlays: &[Overlay], base: &str) -> String {
    let mut candidate = format!("{}_R", base);
    let mut n = 2;
    while overlays.iter().any(|o| match o {
        Overlay::Text(t) => t.id == candidate,
        Overlay::Image(im) => im.id == candidate,
        Overlay::Video(v) => v.id == candidate,
    }) {
        candidate = format!("{}_R{}", base, n);
        n += 1;
    }
    candidate
}

fn unique_audio_id_in_scene(audios: &[memstroy_core::AudioTrack], base: &str) -> String {
    let mut candidate = format!("{}_R", base);
    let mut n = 2;
    while audios.iter().any(|a| a.id == candidate) {
        candidate = format!("{}_R{}", base, n);
        n += 1;
    }
    candidate
}

fn unique_background_id_in_scene(
    bgs: &[memstroy_core::Background],
    base: &str,
) -> String {
    let mut candidate = format!("{}_R", base);
    let mut n = 2;
    while bgs.iter().any(|b| b.id == candidate) {
        candidate = format!("{}_R{}", base, n);
        n += 1;
    }
    candidate
}


// ─── TIMELINE ────────────────────────────────────────────────────────

pub fn timeline(ui: &mut egui::Ui, state: &mut EditorState) {
    // While any drag-related interaction is in progress on the timeline
    // (clip drag, asset drop, scrollbar grip, ruler scrub), force a
    // repaint every frame so motion stays smooth even when the egui
    // reactive scheduler would otherwise delay the next update. Without
    // this the user sees the "застревание" (stuck) feel where the
    // dragged element trails the cursor by a frame or two.
    let any_pointer_down = ui.input(|i| i.pointer.any_down());
    let drag_in_flight = state.timeline_drag.dragging_clip.is_some()
        || state.asset_drag.dragging.is_some();
    if any_pointer_down || drag_in_flight {
        ui.ctx().request_repaint();
    }

    // ── Toolbar ──
    ui.horizontal(|ui| {
        // ── Play / Pause transport ──
        // Single inline toggle button — the icon swaps between ▶ and ⏸
        // depending on `state.playing`, mirroring the Space shortcut.
        // The previous separate Stop button was removed so the user has
        // a single, unambiguous transport control; the playhead can be
        // rewound by clicking the timeline ruler or pressing Home.
        let play_glyph = if state.playing { "\u{23F8}" } else { "\u{25B6}" }; // ⏸ / ▶
        let play_label = if state.playing { t("Pause (Space)") } else { t("Play (Space)") };
        let play_color = if state.playing {
            Color32::from_rgb(255, 200, 80)
        } else {
            Color32::from_rgb(120, 220, 140)
        };
        let play_btn = egui::Button::new(
            RichText::new(play_glyph).size(15.0).color(play_color),
        )
        .min_size(Vec2::new(30.0, 22.0));
        if ui.add(play_btn).on_hover_text(play_label).clicked() {
            state.playing = !state.playing;
            state.status = if state.playing {
                t("\u{25B6} Playing").into()
            } else {
                t("\u{23F8} Paused").into()
            };
        }

        ui.separator();

        ui.add(egui::DragValue::new(&mut state.playback_speed).range(0.1..=8.0).speed(0.05).prefix("x"));

        ui.separator();

        // Time display
        let duration = state.scene.output.duration;
        ui.label(RichText::new(format_time(state.playhead)).size(13.0).strong().color(COL_TEXT));
        ui.label(RichText::new(format!("/ {}", format_time(duration))).size(11.0).color(COL_TEXT_DIM));

        ui.separator();

        // Split tool — when armed, clicking on a clip cuts it at the click position.
        let split_color = if state.split_tool_active { Color32::from_rgb(255, 80, 80) } else { COL_TEXT };
        if ui.button(RichText::new("\u{2702}").color(split_color))
            .on_hover_text(t("Split tool: click anywhere on a clip to cut it at that position"))
            .clicked()
        {
            state.split_tool_active = !state.split_tool_active;
        }

        // Add Text tool
        if ui.button(RichText::new("\u{1F520} +T").color(Color32::from_rgb(140, 220, 255)))
            .on_hover_text(t("Add text overlay at playhead"))
            .clicked()
        {
            add_text_overlay(state);
        }

        // Extract Frame tool — bake the canvas at the current playhead
        // into a fresh image asset + image overlay layer. Honours the
        // canvas multi-selection: when the user has Ctrl-clicked /
        // marquee-selected a subset of layers, only those are
        // composited (transparent background); otherwise the whole
        // frame is captured (including the scene background colour).
        let extract_hover = if !state.canvas_selection.is_empty() {
            t("Extract selected layers as image at playhead")
        } else if !matches!(state.selection, Selection::None | Selection::RenderFrame) {
            t("Extract selected layer as image at playhead")
        } else {
            t("Extract current frame as image layer at playhead")
        };
        if ui.button(RichText::new("\u{1F4F8}").color(Color32::from_rgb(255, 200, 120)))
            .on_hover_text(extract_hover)
            .clicked()
        {
            match crate::frame_snapshot::extract_frame_to_image_layer(state) {
                Ok(_idx) => {
                    // Status string is set inside the extractor with
                    // a contextual summary (full frame vs subset, plus
                    // a count of any layers we had to skip).
                }
                Err(e) => {
                    state.status = format!("\u{274C} {}: {}", t("Extract frame failed"), e);
                }
            }
        }

        ui.separator();

        // Loop preview toggle
        let loop_color = if state.loop_mode { Color32::from_rgb(255, 180, 80) } else { COL_TEXT_DIM };
        if ui
            .button(RichText::new(format!("\u{1F501} {}", t("Loop"))).size(11.0).color(loop_color))
            .on_hover_text(t(
                "Loop preview: Shift+click on the ruler to set loop start, Shift+click again for end. \
                Shift+drag = define a region.",
            ))
            .clicked()
        {
            state.loop_mode = !state.loop_mode;
            if !state.loop_mode {
                state.loop_pending_start = None;
            }
        }

        ui.separator();

        // ── Explicit "+ Layer" buttons ──
        //
        // Adds a fresh empty video / audio lane to the timeline at the
        // top of its respective block. The drag-to-margin gesture also
        // creates lanes (and remains the canonical way to drop a clip
        // straight onto a new lane), but it only fires on drag-end so
        // it's awkward for users who just want a blank scratch lane.
        // These buttons remove the "max 1 new layer per session" feel
        // by giving an unconditional, repeatable creation path.
        if ui
            .button(RichText::new(t("+ V Layer")).size(11.0).color(Color32::from_rgb(140, 220, 255)))
            .on_hover_text(t("Add a new empty video layer at the top of the panel"))
            .clicked()
        {
            state.mutate(|_| {});
            let _ = state.insert_video_track_at_top();
            state.status = t("\u{2728} New video layer.").into();
        }
        if ui
            .button(RichText::new(t("+ A Layer")).size(11.0).color(Color32::from_rgb(120, 220, 200)))
            .on_hover_text(t("Add a new empty audio layer below the existing audio block"))
            .clicked()
        {
            state.mutate(|_| {});
            state.add_audio_track();
            state.status = t("\u{2728} New audio layer.").into();
        }

        // The render-scale display lives on the canvas (render
        // window) only — the user explicitly asked for it to be
        // removed from the timeline / layers panel "tabs" toolbar
        // since the same information is already visible (and
        // adjustable) over the preview itself.
    });
    ui.add_space(2.0);

    // ── Track area: explicit layout with custom scrollbars ──
    //
    // Layout:
    //   ┌─ ruler (header_w | track_area_w)        ──────┐ (top)
    //   │ ┌──────────┬──────────────────────────┬───┐ │
    //   │ │ track    │ tracks viewport (clipped)│ V │ │
    //   │ │ headers  │                          │ S │ │
    //   │ │          │                          │ B │ │
    //   │ └──────────┴──────────────────────────┴───┘ │
    //   │           horizontal scrollbar             │
    //   └────────────────────────────────────────────┘
    let header_width = 80.0_f32;
    let v_sb_w = 14.0_f32;
    let h_sb_h = 14.0_f32;
    let ruler_height = 22.0_f32;
    let total_avail = ui.available_size_before_wrap();
    let track_area_width = (total_avail.x - header_width - v_sb_w - 6.0).max(120.0);
    let viewport_h = (total_avail.y - ruler_height - h_sb_h - 8.0).max(60.0);

    // ── Auto-length: expand/shrink timeline to fit longest content ──
    {
        let mut max_end: f32 = 0.0;
        for a in &state.scene.actors {
            max_end = max_end.max(a.t_out.unwrap_or(0.0));
        }
        for bg in &state.scene.backgrounds {
            max_end = max_end.max(bg.start + bg.duration);
        }
        for ov in &state.scene.overlays {
            let end = match ov {
                Overlay::Text(t) => t.t_out,
                Overlay::Image(im) => im.t_out,
                Overlay::Video(v) => v.t_out,
            };
            max_end = max_end.max(end);
        }
        for au in &state.scene.audio {
            max_end = max_end.max(au.t_out.unwrap_or(0.0));
        }
        // Auto-fit: timeline length is the end of the longest layer.
        // No padding — when the playhead reaches the last clip's end the
        // loop wraps immediately back to 0 instead of running through dead
        // air.
        let target_duration = max_end.max(2.0);
        state.scene.output.duration = target_duration;
    }

    let duration = state.scene.output.duration.max(0.01);

    // Reserve and compute the master rect for the whole timeline area.
    let master_size = Vec2::new(
        header_width + track_area_width + v_sb_w + 6.0,
        ruler_height + viewport_h + h_sb_h + 8.0,
    );
    let (master_rect, _master_resp) =
        ui.allocate_exact_size(master_size, Sense::hover());

    // Sub-rects.
    let ruler_rect = egui::Rect::from_min_max(
        egui::pos2(master_rect.min.x, master_rect.min.y),
        egui::pos2(
            master_rect.min.x + header_width + track_area_width + 4.0,
            master_rect.min.y + ruler_height,
        ),
    );
    let header_col_rect = egui::Rect::from_min_max(
        egui::pos2(master_rect.min.x, master_rect.min.y + ruler_height + 2.0),
        egui::pos2(
            master_rect.min.x + header_width,
            master_rect.min.y + ruler_height + 2.0 + viewport_h,
        ),
    );
    let tracks_rect = egui::Rect::from_min_max(
        egui::pos2(
            master_rect.min.x + header_width + 2.0,
            master_rect.min.y + ruler_height + 2.0,
        ),
        egui::pos2(
            master_rect.min.x + header_width + 2.0 + track_area_width,
            master_rect.min.y + ruler_height + 2.0 + viewport_h,
        ),
    );
    let v_sb_rect = egui::Rect::from_min_max(
        egui::pos2(
            tracks_rect.max.x + 2.0,
            tracks_rect.min.y,
        ),
        egui::pos2(
            tracks_rect.max.x + 2.0 + v_sb_w,
            tracks_rect.max.y,
        ),
    );
    let h_sb_rect = egui::Rect::from_min_max(
        egui::pos2(
            tracks_rect.min.x,
            tracks_rect.max.y + 4.0,
        ),
        egui::pos2(
            tracks_rect.max.x,
            tracks_rect.max.y + 4.0 + h_sb_h,
        ),
    );

    // Painters.
    let ruler_painter = ui.painter_at(ruler_rect);
    let header_painter = ui.painter_at(header_col_rect);
    let tracks_painter = ui.painter_at(tracks_rect);

    // Background fills.
    header_painter.rect_filled(header_col_rect, Rounding::ZERO, Color32::from_rgb(26, 26, 36));
    tracks_painter.rect_filled(tracks_rect, Rounding::ZERO, COL_BG_TRACK);

    let pps = state.timeline_zoom; // pixels per second

    // Mouse wheel inside the tracks viewport: vertical scroll (and Shift = horizontal).
    let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
    let pointer_in_viewport = ui
        .input(|i| i.pointer.hover_pos())
        .map(|p| tracks_rect.contains(p) || header_col_rect.contains(p))
        .unwrap_or(false);
    if pointer_in_viewport && scroll_delta.y.abs() > 0.1 {
        let shift = ui.input(|i| i.modifiers.shift);
        if shift {
            // Shift+wheel = horizontal pan (in seconds).
            state.timeline_scroll =
                (state.timeline_scroll - scroll_delta.y / pps.max(1.0)).max(0.0);
        } else {
            // Plain wheel = vertical pan (in pixels).
            state.timeline_v_scroll = (state.timeline_v_scroll - scroll_delta.y).max(0.0);
        }
    }
    if pointer_in_viewport && scroll_delta.x.abs() > 0.1 {
        // Horizontal wheel (touchpad) = horizontal pan in seconds.
        state.timeline_scroll =
            (state.timeline_scroll - scroll_delta.x / pps.max(1.0)).max(0.0);
    }

    // ── Ruler ──
    let ruler_resp = ui.interact(
        ruler_rect,
        ui.make_persistent_id("timeline_ruler"),
        Sense::click_and_drag(),
    );
    let painter = ruler_painter;

    let track_left = ruler_rect.min.x + header_width + 2.0;
    let track_right = track_left + track_area_width;

    // ── Viewport time range (used to cull off-screen clips before any
    // per-clip work runs). Note `pps == timeline_zoom` and we already
    // guard against zero in the wheel handlers above. A small slack of
    // half a pixel either side keeps clips that are touching the edge
    // visible while scrolling.
    let viewport_t_min = state.timeline_scroll - 0.5 / pps.max(1.0);
    let viewport_t_max = state.timeline_scroll + track_area_width / pps.max(1.0) + 0.5 / pps.max(1.0);
    let in_viewport = |a: f32, b: f32| -> bool { b >= viewport_t_min && a <= viewport_t_max };
    let ruler_track_rect = egui::Rect::from_min_max(
        egui::pos2(track_left, ruler_rect.min.y),
        egui::pos2(track_right, ruler_rect.max.y),
    );
    painter.rect_filled(ruler_track_rect, Rounding::ZERO, COL_RULER);

    // Time markers on ruler
    draw_ruler_marks(&painter, ruler_track_rect, state.timeline_scroll, pps, duration);

    // Playhead on ruler
    let ph_x = time_to_x(state.playhead, state.timeline_scroll, pps, track_left, track_right);
    if let Some(x) = ph_x {
        let tri = 5.0;
        painter.add(egui::Shape::convex_polygon(
            vec![egui::pos2(x - tri, ruler_rect.min.y), egui::pos2(x + tri, ruler_rect.min.y), egui::pos2(x, ruler_rect.min.y + tri * 1.5)],
            COL_PLAYHEAD, Stroke::NONE));
        painter.line_segment([egui::pos2(x, ruler_rect.min.y + tri * 1.5), egui::pos2(x, ruler_rect.max.y)],
            Stroke::new(1.5, COL_PLAYHEAD));
    }

    // Click ruler to seek
    if ruler_resp.clicked() || ruler_resp.dragged() {
        if let Some(pos) = ruler_resp.interact_pointer_pos() {
            if pos.x >= track_left && pos.x <= track_right {
                let clicked_t = x_to_time(pos.x, state.timeline_scroll, pps, track_left)
                    .clamp(0.0, duration);
                let shift_held = ui.input(|i| i.modifiers.shift);

                if state.loop_mode && shift_held {
                    // Shift+drag to define a region
                    if ruler_resp.dragged() {
                        let press = ruler_resp.interact_pointer_pos().unwrap_or(pos);
                        let press_t = x_to_time(press.x, state.timeline_scroll, pps, track_left)
                            .clamp(0.0, duration);
                        let drag_t = clicked_t;
                        let (a, b) = if press_t <= drag_t {
                            (press_t, drag_t)
                        } else {
                            (drag_t, press_t)
                        };
                        if (b - a).abs() > 0.01 {
                            state.loop_region = Some((a, b));
                            state.status =
                                format!("\u{1F501} Loop region: {:.2}s - {:.2}s", a, b);
                        }
                        state.loop_pending_start = None;
                    } else if ruler_resp.clicked() {
                        match state.loop_pending_start.take() {
                            None => {
                                state.loop_pending_start = Some(clicked_t);
                                state.status = format!(
                                    "\u{1F501} Loop start set to {:.2}s. Shift+click for end.",
                                    clicked_t
                                );
                            }
                            Some(start) => {
                                let (a, b) = if start <= clicked_t {
                                    (start, clicked_t)
                                } else {
                                    (clicked_t, start)
                                };
                                if (b - a).abs() > 0.01 {
                                    state.loop_region = Some((a, b));
                                    state.status = format!(
                                        "\u{1F501} Loop region: {:.2}s - {:.2}s",
                                        a, b
                                    );
                                }
                            }
                        }
                    }
                } else {
                    state.playhead = clicked_t;
                }
            }
        }
    }

    // Draw loop region band on the ruler.
    if state.loop_mode {
        if let Some((ls, le)) = state.loop_region {
            let (ls, le) = if ls <= le { (ls, le) } else { (le, ls) };
            let lx0 = (ls - state.timeline_scroll) * pps + track_left;
            let lx1 = (le - state.timeline_scroll) * pps + track_left;
            let lx0c = lx0.clamp(track_left, track_right);
            let lx1c = lx1.clamp(track_left, track_right);
            if lx1c > lx0c {
                let band = egui::Rect::from_min_max(
                    egui::pos2(lx0c, ruler_rect.min.y),
                    egui::pos2(lx1c, ruler_rect.max.y),
                );
                painter.rect_filled(
                    band,
                    Rounding::ZERO,
                    Color32::from_rgba_premultiplied(255, 180, 80, 60),
                );
                // Handles at start/end
                let handle_color = Color32::from_rgb(255, 180, 80);
                painter.line_segment(
                    [egui::pos2(lx0c, ruler_rect.min.y), egui::pos2(lx0c, ruler_rect.max.y)],
                    Stroke::new(2.0, handle_color),
                );
                painter.line_segment(
                    [egui::pos2(lx1c, ruler_rect.min.y), egui::pos2(lx1c, ruler_rect.max.y)],
                    Stroke::new(2.0, handle_color),
                );
            }
        }
        // Pending start marker
        if let Some(start) = state.loop_pending_start {
            let lx = (start - state.timeline_scroll) * pps + track_left;
            if lx >= track_left && lx <= track_right {
                painter.line_segment(
                    [egui::pos2(lx, ruler_rect.min.y), egui::pos2(lx, ruler_rect.max.y)],
                    Stroke::new(1.5, Color32::from_rgba_premultiplied(255, 220, 120, 200)),
                );
            }
        }
    }


    // ── Playhead scrub strip in the tracks viewport ──
    //
    // The playhead is a thin vertical line that crosses every track
    // row. The user wants to grab it directly to scrub the timeline
    // — even when the line visually overlaps a clip bar — instead of
    // accidentally selecting / dragging the clip below it. We do this
    // BEFORE the per-clip / marquee interactors run so the press is
    // captured by the scrub before any other widget claims it.
    //
    // The strip is ~10 px wide on either side of the playhead screen-X
    // (covering one full pixel-width of slop on both sides of the
    // 1.5 px line, plus an extra few for fingers and trackpads). Its
    // hit-test is registered persistently across frames so a drag
    // continues to scrub even after the playhead has moved away from
    // the original press position.
    //
    // Activation is FROZEN to the press frame: we only check the
    // strip on the very first frame of the press (`primary_pressed`).
    // Without this freeze, auto-playback could drift the playhead
    // toward an unrelated press position on a later frame and
    // erroneously trigger scrubbing for a click that landed on a
    // clip — exactly the "click on a clip is interpreted as scrub"
    // / "single click does not select" symptom the user reported.
    {
        let pps_now = state.timeline_zoom;
        let any_pointer_down = ui.input(|i| i.pointer.any_down());
        let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
        let press_origin = ui.input(|i| i.pointer.press_origin());

        if primary_pressed && !state.timeline_scrubbing_playhead {
            if let (Some(ph_x_at_press), Some(p)) = (
                time_to_x(state.playhead, state.timeline_scroll, pps_now, track_left, track_right),
                press_origin,
            ) {
                // Slightly wider hit zone than the visual line so users
                // can grab it without surgical precision — addresses
                // the user feedback that clicks on the timeline scale
                // (the playhead line, "шкала таймлайна") were being
                // absorbed by the clip behind it.
                let strip_half = 10.0_f32;
                if (p.x - ph_x_at_press).abs() <= strip_half
                    && p.y >= tracks_rect.min.y
                    && p.y <= tracks_rect.max.y
                    // Skip when an asset / clip drag is already in
                    // flight from a previous frame.
                    && state.timeline_drag.dragging_clip.is_none()
                    && state.asset_drag.dragging.is_none()
                {
                    state.timeline_scrubbing_playhead = true;
                }
            }
        }

        // While scrubbing, drive the playhead from the live pointer
        // position. Force a repaint so motion stays smooth even when
        // the egui reactive scheduler would otherwise sleep.
        if state.timeline_scrubbing_playhead {
            ui.ctx().request_repaint();
            if let Some(p) = ui.input(|i| {
                i.pointer.interact_pos().or_else(|| i.pointer.hover_pos())
            }) {
                let new_t = x_to_time(p.x, state.timeline_scroll, pps_now, track_left)
                    .clamp(0.0, duration);
                state.playhead = new_t;
            }
            // Tell every per-clip interactor below to bow out for the
            // rest of this frame so the press isn't double-handled
            // (e.g. clip select, drag, trim). Cleared at the end of
            // the timeline function.
            ui.data_mut(|d| {
                d.insert_temp::<bool>(egui::Id::new("timeline_input_lock"), true);
            });
            // Clear on release.
            if !any_pointer_down {
                state.timeline_scrubbing_playhead = false;
            }
        }
    }

    // Pre-frame: ensure the input lock starts each frame fresh. The
    // scrubbing block above re-asserts it when needed; downstream
    // per-clip interactors observe the latest value.
    if !state.timeline_scrubbing_playhead {
        ui.data_mut(|d| {
            d.insert_temp::<bool>(egui::Id::new("timeline_input_lock"), false);
        });
    }

    // ── Early marquee widget registration ──
    //
    // Register the marquee's hit-test rectangle BEFORE any clip
    // interact() calls so egui's hit-test treats clips as the topmost
    // (later-registered) widgets. Without this the marquee — which
    // covers the entire tracks viewport with `Sense::click_and_drag()`
    // — would be the topmost interactive widget at every clip
    // position, and egui's hit-test (see `hit_test_on_close` in
    // egui::hit_test) gives clicks/drags to the LAST registered
    // widget at the pointer position. The user's report
    // "single click on a layer panel element doesn't select it,
    // probably because of false multi-select triggering" is exactly
    // this: every clip click was being intercepted by the marquee
    // interactor, the clip's `clicked()` returned false, and the
    // selection never updated.
    //
    // We don't process the marquee logic here — the response is
    // re-fetched at the end of the function via `read_response` so
    // the rest of the marquee state machine still runs after the
    // clip loop has had a chance to set `state.timeline_drag` /
    // `state.canvas_selection`. This matches egui's documented
    // pattern for "register early, react late" interactions.
    let marquee_id = egui::Id::new(("timeline_marquee_interact",));
    let _early_marquee_resp =
        ui.interact(tracks_rect, marquee_id, egui::Sense::click_and_drag());


    // ── Track rows ──
    let mut to_select: Option<Selection> = None;

    let v_zoom = state.timeline_v_zoom.max(0.1);
    let num_tracks = state.tracks.len();

    // Height of the dedicated Render Frame row pinned to the top of the
    // tracks viewport. Mirrors the base track height so it visually
    // matches the rest of the panel.
    const RF_ROW_BASE_H: f32 = 40.0;
    let rf_row_h: f32 = RF_ROW_BASE_H * v_zoom;

    // ── Pre-compute per-track row rectangles for vertical drag-resolution ──
    // (used by clip-drag handlers below to figure out which track the pointer
    // currently hovers over, and whether the user is dragging above the
    // topmost video / below the bottommost audio so we can auto-create a new
    // layer in that direction). The "expansion" added by the per-param
    // keyframe rows of the currently-selected layer is included here so the
    // hit-test recognises the whole row as one lane. The `mask_above`
    // strip on layers with animated mask params is ALSO included here —
    // earlier this only summed `tk.height + expansion`, so dropping a
    // clip onto the mask-row strip was misclassified as a gap (potentially
    // creating a stray new layer) instead of landing on the host row.
    let mut track_rows: Vec<(f32, f32)> = Vec::with_capacity(num_tracks);
    {
        // Reserve space for the Render Frame row at the top — every
        // real track is shifted down by `rf_row_h` (the diamond strip)
        // PLUS `rf_expansion` (the per-param keyframe rows that show
        // up directly under the diamond strip when the render frame
        // is the active selection, mirroring how every other layer
        // expands beneath itself).
        let rf_expansion = render_frame_expansion(state, v_zoom);
        let mut acc = rf_row_h + rf_expansion;
        for (ti, tk) in state.tracks.iter().enumerate() {
            let h = tk.height * v_zoom
                + selected_layer_mask_above_height(state, ti, v_zoom)
                + selected_layer_expansion(state, ti, v_zoom);
            let top = tracks_rect.min.y + acc - state.timeline_v_scroll;
            let bot = top + h;
            track_rows.push((top, bot));
            acc += h;
        }
    }
    let pointer_y: Option<f32> = ui.input(|i| {
        // While dragging, `hover_pos` returns None as soon as the
        // cursor leaves the timeline panel, which used to break the
        // cross-lane drag the moment the user moved the mouse over the
        // inspector. `interact_pos` keeps reporting the latest pointer
        // sample for the duration of the drag, so cross-lane drops
        // continue to fire correctly. We still fall back to
        // `hover_pos` for the no-drag case so plain hover highlighting
        // stays accurate.
        i.pointer
            .interact_pos()
            .or_else(|| i.pointer.hover_pos())
            .map(|p| p.y)
    });
    let any_pointer_down = ui.input(|i| i.pointer.any_down());

    // Classify a pointer Y into a drop target relative to the current
    // track layout. `current_assigned` is the row the dragged clip is
    // already on; we add a hysteresis band around its centre line so a
    // small Y wobble during a horizontal drag doesn't pop the clip onto
    // a neighbouring lane. Lanes only switch once the pointer travels
    // visibly into a different row, and "new lane" intents only fire
    // when the pointer is well past the topmost / bottommost row.
    #[derive(Clone, Copy)]
    enum DropIntent {
        ToVideoRow(usize),
        ToAudioRow(usize),
        NewVideoTop,
        NewVideoBottom,
        NewAudioTop,
        NewAudioBottom,
        Outside,
    }
    let video_indices: Vec<usize> = state.video_track_indices();
    let audio_indices: Vec<usize> = state.audio_track_indices();
    let track_kinds: Vec<TrackKind> =
        state.tracks.iter().map(|t| t.kind).collect();
    let new_lane_margin = 16.0_f32;

    let classify_pointer_y = |py: f32, current_assigned: Option<usize>| -> DropIntent {
        // Hysteresis: if the pointer is still within the dragged clip's
        // own row, keep it there. Generous bounds (3 px overshoot) avoid
        // jumpy hand-offs at the row borders.
        if let Some(cur) = current_assigned {
            if cur < track_rows.len() && cur < track_kinds.len() {
                let (top, bot) = track_rows[cur];
                if py >= top - 3.0 && py < bot + 3.0 {
                    return match track_kinds[cur] {
                        TrackKind::Video => DropIntent::ToVideoRow(cur),
                        TrackKind::Audio => DropIntent::ToAudioRow(cur),
                    };
                }
            }
        }

        // Otherwise pick whichever row the pointer is currently inside.
        for &i in &video_indices {
            let (top, bot) = track_rows[i];
            if py >= top && py < bot {
                return DropIntent::ToVideoRow(i);
            }
        }
        for &i in &audio_indices {
            let (top, bot) = track_rows[i];
            if py >= top && py < bot {
                return DropIntent::ToAudioRow(i);
            }
        }

        let first_video_top = video_indices.first().map(|&i| track_rows[i].0);
        let last_video_bot = video_indices.last().map(|&i| track_rows[i].1);
        let first_audio_top = audio_indices.first().map(|&i| track_rows[i].0);
        let last_audio_bot = audio_indices.last().map(|&i| track_rows[i].1);

        if let Some(t) = first_video_top {
            if py < t - new_lane_margin {
                return DropIntent::NewVideoTop;
            }
        } else if let Some(at) = first_audio_top {
            if py < at - new_lane_margin {
                return DropIntent::NewVideoTop;
            }
        }

        if let (Some(vb), Some(at)) = (last_video_bot, first_audio_top) {
            if py >= vb + new_lane_margin && py < at - new_lane_margin {
                let mid = (vb + at) * 0.5;
                return if py < mid {
                    DropIntent::NewVideoBottom
                } else {
                    DropIntent::NewAudioTop
                };
            }
        }
        if let (Some(vb), None) = (last_video_bot, first_audio_top) {
            if py >= vb + new_lane_margin {
                return DropIntent::NewVideoBottom;
            }
        }

        if let Some(b) = last_audio_bot {
            if py >= b + new_lane_margin {
                return DropIntent::NewAudioBottom;
            }
        }

        DropIntent::Outside
    };

    // Total scaled height needed to fit all tracks at the current v_zoom,
    // including any per-param keyframe-row expansion on the selected layer.
    // The dedicated Render Frame row is added once at the top.
    //
    // We pad the total with `BOTTOM_GUTTER` so the user can ALWAYS scroll
    // the bottom-most lane fully into the visible area, with a generous
    // empty band beneath it. Without enough gutter the last row's
    // bottom edge sits exactly at the viewport bottom — combined with
    // sub-pixel rounding inside the scrollbar's pan-fraction round-trip
    // this caused the last layer to "пропадать" (disappear) at maximum
    // scroll. A full row-height of empty space keeps the last lane
    // visible regardless of v_zoom rounding. Sized to comfortably clear
    // the tallest possible row at max v_zoom (audio is 48 px × 8 = 384
    // px) so even on a short timeline panel the bottom-most audio lane
    // is fully reachable instead of getting clipped by the scrollbar's
    // last-pixel round-down.
    const BOTTOM_GUTTER: f32 = 160.0;
    let total_tracks_h: f32 = rf_row_h
        + render_frame_expansion(state, v_zoom)
        + (0..num_tracks)
            .map(|i| {
                // Match the per-frame layout exactly:
                //   effective_track_h = mask_above + track_h + expansion
                // Earlier this only summed `track_h + expansion`, so any
                // selected actor/overlay with animated mask params (which
                // contributes `mask_above`) would push the actual content
                // taller than the scroll budget — making the bottom rows
                // unreachable and causing the scrollbar thumb to jump
                // when switching between layers with vs. without mask
                // animations (e.g. clicking from an animated actor onto
                // an audio clip below).
                state.tracks[i].height * v_zoom
                    + selected_layer_mask_above_height(state, i, v_zoom)
                    + selected_layer_expansion(state, i, v_zoom)
            })
            .sum::<f32>()
        + BOTTOM_GUTTER;
    let max_v_scroll = (total_tracks_h - viewport_h).max(0.0);
    state.timeline_v_scroll = state.timeline_v_scroll.max(0.0).min(max_v_scroll);
    let v_scroll = state.timeline_v_scroll;

    // Aggregate the kf-row click hits into a single update step at the
    // end of the loop. Avoids interleaving mutable borrows of state with
    // the clip-draw / drag handlers above.
    let mut param_row_clicks: Vec<(crate::kf_anim::SelectedLayer, ParamRowClick)> = Vec::new();
    // Easing-change requests collected from the per-param kf rows
    // (right-click menu). Drained at the bottom of the timeline
    // pass so we never hold a mutable borrow on `state` while the
    // row is being drawn.
    let mut param_row_easing_changes: Vec<(crate::kf_anim::SelectedLayer, ParamRowEasingChange)> =
        Vec::new();

    // ── Render Frame row (always at the top of the panel) ──
    // The render frame is the "where do we crop the output" rectangle,
    // not a regular layer — but the user thinks of it as one and
    // expects to see its keyframes in the timeline. We give it its own
    // dedicated row above all tracks. The row scrolls with the rest of
    // the timeline (it's not sticky) so users with very tall scenes
    // can still scroll past it.
    //
    // When the render frame is the active selection, we also expand
    // the row downward with one diamond row per animated parameter —
    // exactly like every other layer expands. This way the user
    // works with render-frame keyframes from the SAME place as
    // actor / overlay keyframes (the inspector used to host these
    // strips, but per the user request we moved them onto the
    // layer panel for consistency).
    let rf_expansion = render_frame_expansion(state, v_zoom);
    {
        let row_top = tracks_rect.min.y - v_scroll;
        let diamond_bot = row_top + rf_row_h;
        let row_bot = diamond_bot + rf_expansion;

        if row_bot >= tracks_rect.min.y - 1.0 && row_top <= tracks_rect.max.y + 1.0 {
            let row_rect = egui::Rect::from_min_max(
                egui::pos2(tracks_rect.min.x, row_top),
                egui::pos2(tracks_rect.max.x, diamond_bot),
            );
            let painter_rf = &tracks_painter;
            // The render frame row participates in the canvas multi-
            // selection just like every other layer — highlight the
            // row whenever it's the primary selection OR it's part of
            // a marquee / Ctrl-click multi-selection set.
            let rf_selected = state.selection == Selection::RenderFrame
                || state.canvas_selection.contains(&Selection::RenderFrame);
            let bg_color = if rf_selected {
                Color32::from_rgb(58, 38, 38)
            } else {
                Color32::from_rgb(40, 28, 32)
            };
            painter_rf.rect_filled(row_rect, Rounding::ZERO, bg_color);
            // Top/bottom separators so the row stands out from the
            // ruler and the regular tracks. The bottom separator
            // sits below the param-rows expansion when present so
            // it always reads as the floor of the whole render-
            // frame block.
            painter_rf.line_segment(
                [
                    egui::pos2(tracks_rect.min.x, row_top),
                    egui::pos2(tracks_rect.max.x, row_top),
                ],
                Stroke::new(1.0, Color32::from_rgb(120, 60, 60)),
            );
            painter_rf.line_segment(
                [
                    egui::pos2(tracks_rect.min.x, row_bot),
                    egui::pos2(tracks_rect.max.x, row_bot),
                ],
                Stroke::new(1.0, Color32::from_rgb(120, 60, 60)),
            );

            // Header label on the left column.
            let hdr_rect = egui::Rect::from_min_max(
                egui::pos2(header_col_rect.min.x, row_top),
                egui::pos2(header_col_rect.max.x, row_bot),
            );
            header_painter.rect_filled(
                hdr_rect,
                Rounding::ZERO,
                Color32::from_rgb(60, 30, 30),
            );
            header_painter.text(
                egui::pos2(
                    hdr_rect.center().x,
                    row_top + rf_row_h * 0.5,
                ),
                egui::Align2::CENTER_CENTER,
                "Render Frame",
                egui::FontId::proportional(11.0),
                Color32::from_rgb(255, 180, 180),
            );

            // Diamond row showing every render-frame keyframe at its
            // scene-time. Re-uses the same renderer used for actor /
            // overlay clip-bars so the diamonds look identical.
            let content_rect_rf = egui::Rect::from_min_max(
                egui::pos2(tracks_rect.min.x, row_top + 1.0),
                egui::pos2(tracks_rect.max.x, diamond_bot - 1.0),
            );
            let scene_dur = state.scene.output.duration.max(0.0);
            let rf_layout = state.scene.render_frame.layout.clone();
            draw_keyframe_diamonds(
                painter_rf,
                content_rect_rf,
                0.0,
                scene_dur,
                &rf_layout,
                state.timeline_scroll,
                pps,
                track_left,
                track_right,
                rf_selected,
                true, // render-frame kfs are scene-time anchored
            );

            // ── Per-param keyframe rows under the render-frame
            // diamond strip. Mirrors the per-track `draw_param_kf_rows`
            // path used for actors / overlays so the visuals
            // (alternating row tints, label gutter, diamond
            // selection / drag) are byte-for-byte identical between
            // the two layer kinds. The render frame is scene-time
            // anchored and runs the entire scene duration, so the
            // "clip" for the strip-attachment math spans
            // 0..scene_duration.
            if rf_expansion > 4.0 {
                if let Some(params) = render_frame_animated_params(state) {
                    let layer_label = crate::kf_anim::SelectedLayer::RenderFrame;
                    let param_kf_pairs =
                        compute_param_change_points(state, Selection::RenderFrame);
                    let clip_x_start =
                        (0.0 - state.timeline_scroll) * pps + track_left;
                    let clip_x_end =
                        (scene_dur - state.timeline_scroll) * pps + track_left;
                    let outcome = draw_param_kf_rows(
                        ui,
                        painter_rf,
                        &layer_label,
                        &params,
                        &param_kf_pairs,
                        diamond_bot,
                        rf_expansion,
                        track_left,
                        track_right,
                        pps,
                        state.timeline_scroll,
                        state.playhead,
                        &state.selected_keyframes,
                        clip_x_start,
                        clip_x_end,
                    );
                    for hit in outcome.click_hits {
                        param_row_clicks.push((layer_label.clone(), hit));
                    }
                    for ec in outcome.easing_changes {
                        param_row_easing_changes.push((layer_label.clone(), ec));
                    }
                }
            }

            // Click anywhere inside the render-frame DIAMOND strip to
            // select it. The expansion area below has its own
            // per-row click handlers (registered by
            // `draw_param_kf_rows` above), so we deliberately scope
            // the row-select interactor to the diamond-only band —
            // otherwise a click on a parameter diamond would fight
            // the row click and lose the seek-to-keyframe gesture.
            let row_id = egui::Id::new(("timeline_rf_row",));
            let row_resp = ui.interact(content_rect_rf, row_id, Sense::click());
            if row_resp.clicked() {
                // Mirror every other layer kind: plain click clears
                // any prior multi-selection then re-seeds canvas_selection
                // with just the render frame, while Ctrl+click toggles
                // it in/out of the existing set without touching the
                // primary selection.
                let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                let target_sel = Selection::RenderFrame;
                if ctrl_held {
                    if let Some(pos) = state.canvas_selection.iter().position(|&s| s == target_sel) {
                        state.canvas_selection.remove(pos);
                    } else {
                        state.canvas_selection.push(target_sel);
                    }
                } else {
                    state.multi_select.clear();
                    state.canvas_selection.clear();
                    state.canvas_selection.push(target_sel);
                }
                to_select = Some(target_sel);
            }
        }
    }

    let mut acc_y = rf_row_h + rf_expansion;
    for track_idx in 0..num_tracks {
        let track = &state.tracks[track_idx];
        let track_h = track.height * v_zoom;
        // Animated mask params (if any) get their own diamond rows
        // ABOVE the clip bar — mirroring the transform-param rows
        // below. Layers without animated mask params yield 0 here so
        // the existing layout is unchanged in the common case.
        let mask_above = selected_layer_mask_above_height(state, track_idx, v_zoom);
        let expansion = selected_layer_expansion(state, track_idx, v_zoom);
        let effective_track_h = mask_above + track_h + expansion;
        let track_kind = track.kind;
        let track_name = track.name.clone();
        let track_muted = track.muted;
        let track_locked = track.locked;

        let row_top = tracks_rect.min.y + acc_y - v_scroll;
        let row_bot = row_top + effective_track_h;
        acc_y += effective_track_h;

        // Cull tracks fully outside the viewport.
        if row_bot < tracks_rect.min.y - 1.0 || row_top > tracks_rect.max.y + 1.0 {
            continue;
        }

        let row_rect = egui::Rect::from_min_max(
            egui::pos2(tracks_rect.min.x, row_top),
            egui::pos2(tracks_rect.max.x, row_bot),
        );
        let painter = &tracks_painter;

        // Track background (alternating).
        let bg = if track_idx % 2 == 0 { COL_BG_TRACK } else { COL_BG_TRACK_ALT };
        painter.rect_filled(row_rect, Rounding::ZERO, bg);

        // Track header (left column, drawn with the header painter so it
        // isn't clipped by the tracks viewport).
        let hdr_rect = egui::Rect::from_min_max(
            egui::pos2(header_col_rect.min.x, row_top),
            egui::pos2(header_col_rect.max.x, row_bot),
        );
        header_painter.rect_filled(hdr_rect, Rounding::ZERO, Color32::from_rgb(30, 30, 42));
        header_painter.text(
            hdr_rect.center(),
            egui::Align2::CENTER_CENTER,
            &track_name,
            egui::FontId::proportional(11.0),
            if track_muted { COL_TEXT_DIM } else { COL_TEXT },
        );

        // Clip area = portion of the row between the mask-row strip
        // (above) and the per-param expansion (below). draw_clip /
        // draw_keyframe_diamonds only draw inside this rect so the
        // sub-rows on either side have clean space.
        let clip_top = row_top + mask_above;
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(tracks_rect.min.x, clip_top + 1.0),
            egui::pos2(tracks_rect.max.x, clip_top + track_h - 1.0),
        );

        // Draw clips on this track
        match track_kind {
            TrackKind::Video => {
                // Draw backgrounds on track 0
                if track_idx == 0 {
                    // The vec can shrink mid-iteration when
                    // `enforce_no_overlap_on_layer` removes neighbours
                    // on the same lane. The bounds check at the top of
                    // each iteration body makes us tolerate that.
                    for bi in 0..state.scene.backgrounds.len() {
                        if bi >= state.scene.backgrounds.len() { break; }
                        let bg_elem = &state.scene.backgrounds[bi];
                        let clip_start = bg_elem.start;
                        let clip_end = bg_elem.start + bg_elem.duration;
                        // Cull off-screen background clips before any
                        // per-clip allocation / interaction work.
                        if !in_viewport(clip_start, clip_end) { continue; }
                        // Highlight the layer row when EITHER the primary
                        // selection points at this background OR the
                        // multi-selection set contains it. Without the
                        // second check, only the primary item ever lit
                        // up while a marquee / Ctrl-click selection had
                        // grabbed several items — the user reported
                        // that "the layers panel still shows just one
                        // selected" even though the canvas correctly
                        // outlines all of them.
                        let target_sel = Selection::Background(bi);
                        let sel = state.selection == target_sel
                            || state.canvas_selection.contains(&target_sel);
                        let bg_id = egui::Id::new(("timeline_clip", "background", bi));
                        if let Some(clicked) = draw_clip(ui, painter, content_rect, &bg_elem.id, bg_id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            COL_CLIP_BG, sel, track_h, track_locked, state.split_tool_active)
                        {
                            if clicked == f32::INFINITY {
                                // Trim left: pull the start forward, keep the
                                // end fixed (Premiere "ripple-trim from in").
                                let dx = ui.input(|i| i.pointer.delta().x);
                                let delta_t = dx / pps;
                                let new_start = (clip_start + delta_t).max(0.0).min(clip_end - 0.1);
                                let new_dur = (clip_end - new_start).max(0.1);
                                let token = EditorState::drag_token("trim_bg_left", bi);
                                state.mutate_drag(token, |s| {
                                    s.backgrounds[bi].start = new_start;
                                    s.backgrounds[bi].duration = new_dur;
                                });
                                let updated = enforce_no_overlap_on_layer(
                                    state,
                                    MovedClipKind::Background(bi),
                                    token,
                                );
                                let bi_eff = match updated {
                                    MovedClipKind::Background(i) => i,
                                    _ => bi,
                                };
                                to_select = Some(Selection::Background(bi_eff));
                            } else if clicked == f32::NEG_INFINITY {
                                // Trim right: stretch / shrink the duration.
                                let dx = ui.input(|i| i.pointer.delta().x);
                                let delta_t = dx / pps;
                                let new_dur = (clip_end - clip_start + delta_t).max(0.1);
                                let token = EditorState::drag_token("trim_bg_right", bi);
                                state.mutate_drag(token, |s| {
                                    s.backgrounds[bi].duration = new_dur;
                                });
                                let updated = enforce_no_overlap_on_layer(
                                    state,
                                    MovedClipKind::Background(bi),
                                    token,
                                );
                                let bi_eff = match updated {
                                    MovedClipKind::Background(i) => i,
                                    _ => bi,
                                };
                                to_select = Some(Selection::Background(bi_eff));
                            } else if clicked < 0.0 {
                                let new_start = (-clicked).max(0.0);
                                let dur = clip_end - clip_start;
                                let token = EditorState::drag_token("move_bg", bi);
                                state.mutate_drag(token, |s| {
                                    s.backgrounds[bi].start = new_start;
                                    s.backgrounds[bi].duration = dur;
                                });
                                // Defer overlap-trim until the drag ends.
                                defer_overlap_resolution(state, MovedClipKind::Background(bi));
                                let bi_eff = bi;
                                to_select = Some(Selection::Background(bi_eff));
                            } else if state.split_tool_active {
                                to_select = Some(Selection::Background(bi));
                                state.playhead = clicked;
                                state.status = "__SPLIT_AT_PLAYHEAD__".into();
                            } else {
                                let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                                let target_sel = Selection::Background(bi);
                                if ctrl_held {
                                    if let Some(pos) = state.canvas_selection.iter().position(|&s| s == target_sel) {
                                        state.canvas_selection.remove(pos);
                                    } else {
                                        state.canvas_selection.push(target_sel);
                                    }
                                } else {
                                    // Plain (non-Ctrl) click: drop every
                                    // prior selection so the inspector
                                    // can't stay in multi-select mode
                                    // after this single click.
                                    state.multi_select.clear();
                                    state.canvas_selection.clear();
                                    state.canvas_selection.push(target_sel);
                                }
                                to_select = Some(target_sel);
                            }
                        }
                    }
                }

                // Draw actors assigned to this video lane. The default
                // assignment for actors without an explicit entry in
                // `actor_track_assignments` is the topmost video lane, so
                // freshly dropped clips show up immediately.
                let video_tracks: Vec<usize> = (0..num_tracks)
                    .filter(|ti| state.tracks[*ti].kind == TrackKind::Video)
                    .collect();

                for ai in 0..state.scene.actors.len() {
                    // The vec can shrink mid-iteration when
                    // `enforce_no_overlap_on_layer` removes a colliding
                    // actor on the same lane (PANIC fix: previously this
                    // walked off the end of `state.scene.actors`).
                    if ai >= state.scene.actors.len() { break; }
                    let assigned_track = if let Some(&assigned) = state.actor_track_assignments.get(&ai) {
                        assigned
                    } else {
                        video_tracks.first().copied().unwrap_or(0)
                    };
                    if assigned_track != track_idx { continue; }

                    let actor = &state.scene.actors[ai];
                    let clip_start = actor.t_in.unwrap_or(0.0);
                    let clip_end = actor.t_out.unwrap_or(duration);
                    // Cull off-screen actor clips. The `draw_clip` call
                    // would early-return None anyway, but we can avoid all
                    // the surrounding bookkeeping (transition indicator,
                    // keyframe diamond, snapshot of layout, etc.) by
                    // skipping the iteration outright.
                    if !in_viewport(clip_start, clip_end) { continue; }
                    let trans_in = actor.transition_in;
                    let trans_out = actor.transition_out;
                    let trans_dur = actor.transition_duration;
                    // See the matching comment in the Background branch:
                    // a layer row is highlighted when EITHER the primary
                    // selection or the canvas multi-selection contains
                    // this actor. This restores the "all selected
                    // clips light up in the layers panel" behaviour the
                    // user reported as broken after marquee select.
                    let target_sel = Selection::Actor(ai);
                    let sel = state.selection == target_sel
                        || state.canvas_selection.contains(&target_sel);
                    let actor_id = egui::Id::new(("timeline_clip", "actor", ai));
                    if let Some(clicked) = draw_clip(ui, painter, content_rect, &actor.id, actor_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        COL_CLIP_ACTOR, sel, track_h, track_locked, state.split_tool_active)
                    {
                        if clicked == f32::INFINITY {
                            // Trim left edge: adjust t_in
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_in = (clip_start + delta_t).max(0.0).min(clip_end - 0.1);
                            let token = EditorState::drag_token("trim_actor_left", ai);
                            state.mutate_drag(token, |s| {
                                s.actors[ai].t_in = Some(new_in);
                                // Crop scene-time keyframes that fall before the new in-edge.
                                s.actors[ai].layout.retain(|kf| kf.t >= new_in - 1.0e-3);
                                if s.actors[ai].layout.is_empty() {
                                    s.actors[ai].layout.push(memstroy_core::Keyframe::new(
                                        new_in,
                                        memstroy_core::ActorState::default(),
                                    ));
                                }
                            });
                            // Defer overlap-trim until the drag ends —
                            // neighbours stay intact while the clip is
                            // still being moved.
                            defer_overlap_resolution(state, MovedClipKind::Actor(ai));
                            let ai_eff = ai;
                            // Bound audio: shift its in-edge by the same delta and
                            // advance source_start so the playback head doesn't slip.
                            sync_audio_to_actor(state, ai_eff);
                            to_select = Some(Selection::Actor(ai_eff));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right edge: adjust t_out.
                            //
                            // Hard-cap the new out-edge against the source clip's
                            // duration so the user can't stretch the clip past
                            // its real footage. Without this clamp the timeline
                            // happily lets you drag the right edge into infinity
                            // and the trailing area plays as a frozen black frame.
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let mut new_out = (clip_end + delta_t).max(clip_start + 0.1);
                            // Source duration upper bound, only applied when the
                            // frame cache for the actor has finished probing.
                            // `t_out_max = t_in + max_clip_dur` where
                            // `max_clip_dur = source_duration - source_start`.
                            let source_start = state.scene.actors[ai].source_start;
                            if let Some(fc) = state.frame_caches.get(ai) {
                                if fc.is_ready() && fc.duration > 0.0 {
                                    let max_clip_dur =
                                        (fc.duration - source_start).max(0.1);
                                    let max_out = clip_start + max_clip_dur;
                                    if new_out > max_out {
                                        new_out = max_out;
                                    }
                                }
                            }
                            let token = EditorState::drag_token("trim_actor_right", ai);
                            state.mutate_drag(token, |s| {
                                s.actors[ai].t_out = Some(new_out);
                                // Crop scene-time keyframes that fall after the new out-edge.
                                s.actors[ai].layout.retain(|kf| kf.t <= new_out + 1.0e-3);
                                if s.actors[ai].layout.is_empty() {
                                    let t = s.actors[ai].t_in.unwrap_or(0.0);
                                    s.actors[ai].layout.push(memstroy_core::Keyframe::new(
                                        t,
                                        memstroy_core::ActorState::default(),
                                    ));
                                }
                            });
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Actor(ai),
                                token,
                            );
                            let ai_eff = match updated {
                                MovedClipKind::Actor(i) => i,
                                _ => ai,
                            };
                            sync_audio_to_actor(state, ai_eff);
                            to_select = Some(Selection::Actor(ai_eff));
                        } else if clicked < 0.0 {
                            // Drag: move the actor's time window
                            let mut new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;

                            // ── Undo snapshot is now handled by mutate_drag below.
                            // Track the active drag for lane-routing UI hints.
                            if state.timeline_drag.dragging_clip.is_none() {
                                state.timeline_drag.dragging_clip = Some(ai);
                                state.timeline_drag.pending_new_lane = None;
                                state.timeline_drag.start_pointer_y = pointer_y;
                            }

                            // ── Alt-modifier: ALSO publish an
                            // ElementDrag so the user can drag the
                            // clip directly from the timeline onto a
                            // skeleton-attachment-point row in the
                            // inspector. The existing drop zones in
                            // `inspector_actor_skeleton_attachments`
                            // listen for `state.element_drag.source`
                            // on pointer release. Without the modifier
                            // the drag stays a pure timeline move.
                            if ui.input(|i| i.modifiers.alt) {
                                state.element_drag.source =
                                    Some(crate::state::AttachableElement::Actor(ai));
                                state.element_drag.label = format!(
                                    "A:{}",
                                    state.scene.actors[ai].id
                                );
                                if let Some(p) = ui.input(|i| {
                                    i.pointer
                                        .interact_pos()
                                        .or_else(|| i.pointer.hover_pos())
                                }) {
                                    state.element_drag.pos = [p.x, p.y];
                                }
                            }

                            // ── Resolve the destination track from the pointer's Y position ──
                            // Actors only ever land on video lanes. Dropping
                            // into the gap above the topmost video row, or
                            // between the video and audio blocks, queues a
                            // "new lane on this side" intent that will be
                            // committed only when the drag ENDS.
                            // Lane lock: if the pointer hasn't moved
                            // vertically more than `LANE_LOCK_THRESHOLD`
                            // pixels from the drag origin, freeze the
                            // dragged clip on its current lane. This
                            // kills the wobble where a horizontal drag
                            // accidentally pops onto a neighbouring row.
                            const LANE_LOCK_THRESHOLD: f32 = 14.0;
                            let lane_locked = match (state.timeline_drag.start_pointer_y, pointer_y) {
                                (Some(y0), Some(y1)) => (y1 - y0).abs() < LANE_LOCK_THRESHOLD,
                                _ => false,
                            };
                            if lane_locked {
                                // Skip lane reassignment entirely. Time
                                // moves still apply (they came from
                                // `total_dx` in draw_clip and are written
                                // below in the same arm).
                            } else if let Some(py) = pointer_y {
                                let cur = state.actor_track_assignments.get(&ai).copied();
                                match classify_pointer_y(py, cur) {
                                    DropIntent::ToVideoRow(idx) => {
                                        state.actor_track_assignments.insert(ai, idx);
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                    DropIntent::NewVideoTop => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::VideoTopForActor(ai));
                                    }
                                    DropIntent::NewVideoBottom => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::VideoBottomForActor(ai));
                                    }
                                    _ => {
                                        // Pointer is over an audio lane or
                                        // outside the panel — keep the
                                        // current assignment, drop the
                                        // queued intent so we don't create
                                        // a stray lane on release.
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                }
                            } else {
                                state.actor_track_assignments.insert(ai, track_idx);
                            }

                            // ── Snap-to-edges logic ──
                            if state.snap_enabled {
                                let new_end = new_start + dur;
                                let mut snap_targets = collect_clip_edges(state, Some(ai));
                                snap_targets.push(state.playhead);
                                // Pixel-aware snap window: ~3 px on screen so the
                                // clip glides smoothly under the cursor instead of
                                // jumping in 0.1 s (≈ 8 px) chunks.
                                let threshold = (3.0 / state.timeline_zoom.max(1.0)).max(0.001);

                                let snapped_start = snap_time(new_start, &snap_targets, threshold);
                                let snapped_end = snap_time(new_end, &snap_targets, threshold);

                                // Prefer start snap, fall back to end snap
                                if (snapped_start - new_start).abs() < threshold {
                                    new_start = snapped_start;
                                } else if (snapped_end - new_end).abs() < threshold {
                                    new_start = snapped_end - dur;
                                }
                            }

                            // ── Move keyframes with the clip ──
                            // Actor keyframes are stored in scene-time, so a
                            // drag must shift them by the same delta to keep
                            // them visually attached to the clip bar.
                            let dt_kfs = new_start - clip_start;
                            let token = EditorState::drag_token("move_actor", ai);
                            state.mutate_drag(token, |s| {
                                s.actors[ai].t_in = Some(new_start);
                                s.actors[ai].t_out = Some(new_start + dur);
                                if dt_kfs.abs() > 1.0e-6 {
                                    for kf in s.actors[ai].layout.iter_mut() {
                                        kf.t += dt_kfs;
                                    }
                                }
                            });
                            // Same-lane overlap rule: trim/split/remove
                            // any clips that this move just collided
                            // with on the actor's current lane. The
                            // helper may shift `ai` if it removes /
                            // splits clips at lower indices.
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Actor(ai),
                                token,
                            );
                            let ai_eff = match updated {
                                MovedClipKind::Actor(i) => i,
                                _ => ai,
                            };
                            sync_audio_to_actor(state, ai_eff);
                            to_select = Some(Selection::Actor(ai_eff));
                        } else if state.split_tool_active {
                            to_select = Some(Selection::Actor(ai));
                            state.playhead = clicked;
                            state.status = "__SPLIT_AT_PLAYHEAD__".into();
                        } else {
                            // ── Ctrl+click multi-select ──
                            //
                            // Two stores have to stay in sync:
                            //   * `multi_select` — the legacy actor-only set,
                            //     kept for backward compatibility.
                            //   * `canvas_selection` — the cross-element set
                            //     read by the inspector and the canvas
                            //     gizmos. Without keeping this in sync, the
                            //     inspector still showed the "N elements
                            //     selected" banner from a previous marquee
                            //     after the user single-clicked a clip in
                            //     the layer panel (bug: regular select
                            //     "stopped working").
                            let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                            let target_sel = Selection::Actor(ai);
                            if ctrl_held {
                                if let Some(pos) = state.multi_select.iter().position(|&x| x == ai) {
                                    state.multi_select.remove(pos);
                                } else {
                                    state.multi_select.push(ai);
                                }
                                if let Some(pos) = state.canvas_selection.iter().position(|&s| s == target_sel) {
                                    state.canvas_selection.remove(pos);
                                } else {
                                    state.canvas_selection.push(target_sel);
                                }
                            } else {
                                // Plain (non-Ctrl) click: replace the
                                // whole multi-selection with just this
                                // layer. Mirrors the canvas's plain-
                                // click behaviour exactly so the
                                // inspector never stays stuck in its
                                // multi-select branch after the user
                                // single-clicks a row in the layer
                                // panel. Without the explicit `push`
                                // back in canvas_selection, downstream
                                // canvas drags wouldn't see the
                                // primary in the snapshot list either.
                                state.multi_select.clear();
                                state.canvas_selection.clear();
                                state.canvas_selection.push(target_sel);
                            }
                            to_select = Some(target_sel);
                        }
                    }

                    // The click handler can call `enforce_no_overlap_on_layer`
                    // which deletes overlapping actors on the same lane.
                    // If that wiped out the actor at our current index,
                    // skip the indicator/keyframe draw to avoid the
                    // "index out of bounds" panic that used to fire here.
                    if ai >= state.scene.actors.len() { continue; }

                    // Transition indicators on the clip bar (faded gradient near edges).
                    draw_transition_indicators(
                        painter,
                        content_rect,
                        clip_start,
                        clip_end,
                        trans_in,
                        trans_out,
                        trans_dur,
                        state.timeline_scroll,
                        pps,
                        track_left,
                        track_right,
                    );

                    // Keyframe diamonds on the clip bar were removed
                    // per user request: they used to aggregate every
                    // layer keyframe (regardless of which parameter
                    // owned it), giving the misleading impression that
                    // *all* parameters were animated together. The
                    // per-parameter rows in the expansion area below
                    // already show the keyframes that actually belong
                    // to each parameter — see `draw_param_kf_rows`.
                    let _ = (clip_start, clip_end);
                }

                // Draw overlays assigned to this video track. Overlays
                // without an explicit assignment fall back to the second
                // video lane (or the first when only one exists), which
                // keeps newly added text/image/video overlays visible
                // without a manual placement step.
                let video_tracks_local: Vec<usize> = (0..num_tracks)
                    .filter(|ti| state.tracks[*ti].kind == TrackKind::Video)
                    .collect();
                let default_overlay_track = if video_tracks_local.len() >= 2 {
                    video_tracks_local[1]
                } else if !video_tracks_local.is_empty() {
                    video_tracks_local[0]
                } else {
                    0
                };
                for oi in 0..state.scene.overlays.len() {
                    // Tolerate mid-iteration removal (overlap-resolution
                    // helper can splice/delete overlays on the same lane).
                    if oi >= state.scene.overlays.len() { break; }
                    let assigned = state
                        .overlay_track_assignments
                        .get(&oi)
                        .copied()
                        .unwrap_or(default_overlay_track);
                    if assigned != track_idx { continue; }

                    let ov = &state.scene.overlays[oi];
                    let (clip_start, clip_end, label) = match ov {
                        Overlay::Text(t) => (t.t_in, t.t_out, format!("T: {}", ellipsis(&t.text, 10))),
                        Overlay::Image(im) => (im.t_in, im.t_out, format!("I: {}", im.id)),
                        Overlay::Video(v) => (v.t_in, v.t_out, format!("V: {}", v.id)),
                    };
                    if !in_viewport(clip_start, clip_end) { continue; }
                    // See the matching comment in the Background branch
                    // above. Highlight the row when it's the primary
                    // selection OR is part of the canvas multi-selection.
                    let target_sel = Selection::Overlay(oi);
                    let sel = state.selection == target_sel
                        || state.canvas_selection.contains(&target_sel);
                    let ov_id = egui::Id::new(("timeline_clip", "overlay", oi));
                    if let Some(clicked) = draw_clip(ui, painter, content_rect, &label, ov_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        COL_CLIP_OVERLAY, sel, track_h, track_locked, state.split_tool_active)
                    {
                        if clicked == f32::INFINITY {
                            // Trim left edge.
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_in = (clip_start + delta_t).max(0.0).min(clip_end - 0.1);
                            let shift = new_in - clip_start;
                            let token = EditorState::drag_token("trim_overlay_left", oi);
                            state.mutate_drag(token, |s| {
                                let layout: &mut Vec<memstroy_core::Keyframe<memstroy_core::OverlayState>> = match &mut s.overlays[oi] {
                                    Overlay::Text(t) => { t.t_in = new_in; &mut t.layout }
                                    Overlay::Image(im) => { im.t_in = new_in; &mut im.layout }
                                    Overlay::Video(v) => { v.t_in = new_in; &mut v.layout }
                                };
                                // Overlay kfs are clip-local, so trimming
                                // the in-edge shifts every kf's local
                                // time by `-shift` to keep its scene-time
                                // anchor stable. Kfs that fall before
                                // local_t = 0 are dropped.
                                if shift.abs() > 1.0e-6 {
                                    for kf in layout.iter_mut() { kf.t -= shift; }
                                    layout.retain(|kf| kf.t >= -1.0e-3);
                                    for kf in layout.iter_mut() { kf.t = kf.t.max(0.0); }
                                }
                                if layout.is_empty() {
                                    layout.push(memstroy_core::Keyframe::new(
                                        0.0,
                                        memstroy_core::OverlayState::default(),
                                    ));
                                }
                            });
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Overlay(oi),
                                token,
                            );
                            let oi_eff = match updated {
                                MovedClipKind::Overlay(i) => i,
                                _ => oi,
                            };
                            to_select = Some(Selection::Overlay(oi_eff));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right edge.
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_out = (clip_end + delta_t).max(clip_start + 0.1);
                            let token = EditorState::drag_token("trim_overlay_right", oi);
                            state.mutate_drag(token, |s| {
                                let (t_in_v, layout): (f32, &mut Vec<memstroy_core::Keyframe<memstroy_core::OverlayState>>) = match &mut s.overlays[oi] {
                                    Overlay::Text(t) => { t.t_out = new_out; (t.t_in, &mut t.layout) }
                                    Overlay::Image(im) => { im.t_out = new_out; (im.t_in, &mut im.layout) }
                                    Overlay::Video(v) => { v.t_out = new_out; (v.t_in, &mut v.layout) }
                                };
                                // Drop kfs whose local time runs past the
                                // new clip duration — they're outside the
                                // visible window after the trim.
                                let max_local = (new_out - t_in_v).max(0.0) + 1.0e-3;
                                layout.retain(|kf| kf.t <= max_local);
                                if layout.is_empty() {
                                    layout.push(memstroy_core::Keyframe::new(
                                        0.0,
                                        memstroy_core::OverlayState::default(),
                                    ));
                                }
                            });
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Overlay(oi),
                                token,
                            );
                            let oi_eff = match updated {
                                MovedClipKind::Overlay(i) => i,
                                _ => oi,
                            };
                            to_select = Some(Selection::Overlay(oi_eff));
                        } else if clicked < 0.0 {
                            // Drag: move the overlay's time window.
                            let new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;
                            let new_end = new_start + dur;
                            // Track active drag for lane-lock & new-lane intents.
                            if state.timeline_drag.dragging_clip.is_none() {
                                state.timeline_drag.dragging_clip = Some(oi);
                                state.timeline_drag.pending_new_lane = None;
                                state.timeline_drag.start_pointer_y = pointer_y;
                            }
                            // Alt-modifier — see the actor arm above for
                            // why this turns the timeline drag into a
                            // skeleton-attach drag-source.
                            if ui.input(|i| i.modifiers.alt) {
                                state.element_drag.source =
                                    Some(crate::state::AttachableElement::Overlay(oi));
                                let lbl = match &state.scene.overlays[oi] {
                                    Overlay::Text(t) => format!("T:{}", t.id),
                                    Overlay::Image(im) => format!("I:{}", im.id),
                                    Overlay::Video(v) => format!("V:{}", v.id),
                                };
                                state.element_drag.label = lbl;
                                if let Some(p) = ui.input(|i| {
                                    i.pointer
                                        .interact_pos()
                                        .or_else(|| i.pointer.hover_pos())
                                }) {
                                    state.element_drag.pos = [p.x, p.y];
                                }
                            }
                            let token = EditorState::drag_token("move_overlay", oi);
                            state.mutate_drag(token, |s| {
                                match &mut s.overlays[oi] {
                                    Overlay::Text(t) => { t.t_in = new_start; t.t_out = new_end; }
                                    Overlay::Image(im) => { im.t_in = new_start; im.t_out = new_end; }
                                    Overlay::Video(v) => { v.t_in = new_start; v.t_out = new_end; }
                                }
                            });

                            // Vertical: re-assign track based on pointer Y.
                            // Overlays only land on video lanes, mirroring
                            // the actor drag rules. Layer creation is
                            // deferred to drag-end. Lane lock is the same
                            // 14-px hysteresis used for actors so the
                            // dragged overlay stops "wobbling" between
                            // adjacent lanes during a horizontal move.
                            const LANE_LOCK_THRESHOLD: f32 = 14.0;
                            let lane_locked = match (state.timeline_drag.start_pointer_y, pointer_y) {
                                (Some(y0), Some(y1)) => (y1 - y0).abs() < LANE_LOCK_THRESHOLD,
                                _ => false,
                            };
                            if lane_locked {
                                // Skip lane reassignment entirely.
                            } else if let Some(py) = pointer_y {
                                let cur = state.overlay_track_assignments.get(&oi).copied();
                                match classify_pointer_y(py, cur) {
                                    DropIntent::ToVideoRow(idx) => {
                                        state.overlay_track_assignments.insert(oi, idx);
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                    DropIntent::NewVideoTop => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::VideoTopForOverlay(oi));
                                    }
                                    DropIntent::NewVideoBottom => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::VideoBottomForOverlay(oi));
                                    }
                                    _ => {
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                }
                            }
                            // Defer overlap-trim until the drag ends —
                            // neighbours stay intact while the overlay
                            // is still being moved.
                            defer_overlap_resolution(state, MovedClipKind::Overlay(oi));
                            let oi_eff = oi;
                            to_select = Some(Selection::Overlay(oi_eff));
                        } else if state.split_tool_active {
                            to_select = Some(Selection::Overlay(oi));
                            state.playhead = clicked;
                            state.status = "__SPLIT_AT_PLAYHEAD__".into();
                        } else {
                            // Plain click clears the cross-element
                            // multi-selection so the inspector returns
                            // to single-element mode (Ctrl+click below
                            // toggles the clicked overlay in/out of
                            // the canvas_selection set instead).
                            let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                            let target_sel = Selection::Overlay(oi);
                            if ctrl_held {
                                if let Some(pos) = state.canvas_selection.iter().position(|&s| s == target_sel) {
                                    state.canvas_selection.remove(pos);
                                } else {
                                    state.canvas_selection.push(target_sel);
                                }
                            } else {
                                // Plain (non-Ctrl) click: drop every
                                // prior selection (including the
                                // legacy actor multi_select set, which
                                // would otherwise leak across layer
                                // kinds) and re-seed canvas_selection
                                // with just this overlay so the
                                // inspector / canvas treat it as a
                                // proper single-element selection.
                                state.multi_select.clear();
                                state.canvas_selection.clear();
                                state.canvas_selection.push(target_sel);
                            }
                            to_select = Some(target_sel);
                        }
                    }
                    // The overlay click handler may have removed
                    // colliding overlays via overlap-resolution; bail
                    // out before reading the (possibly invalid) index.
                    if oi >= state.scene.overlays.len() { continue; }
                    // Keyframe diamonds for overlays were also retired
                    // (see actor branch above for the reasoning) — the
                    // per-parameter rows in the expansion area carry
                    // the diamonds for the parameters the user
                    // actually animated.
                    let _ = (clip_start, clip_end);
                    let _ = match &state.scene.overlays[oi] {
                        Overlay::Text(t) => &t.layout,
                        Overlay::Image(im) => &im.layout,
                        Overlay::Video(v) => &v.layout,
                    };
                }
            }
            TrackKind::Audio => {
                let audio_tracks: Vec<usize> = (0..num_tracks).filter(|ti| state.tracks[*ti].kind == TrackKind::Audio).collect();

                for aui in 0..state.scene.audio.len() {
                    // Tolerate mid-iteration removal.
                    if aui >= state.scene.audio.len() { break; }
                    // Use explicit assignment if set, otherwise round-robin across audio tracks.
                    let target_track_idx = if let Some(&t) = state.audio_track_assignments.get(&aui) {
                        t
                    } else if audio_tracks.is_empty() {
                        0
                    } else {
                        audio_tracks[aui % audio_tracks.len()]
                    };
                    if target_track_idx != track_idx { continue; }

                    let audio = &state.scene.audio[aui];
                    let clip_start = audio.t_in;
                    let clip_end = audio.t_out.unwrap_or(duration);
                    let audio_source_start = audio.source_start;
                    let audio_speed = audio.speed;
                    if !in_viewport(clip_start, clip_end) { continue; }
                    // Highlight when this audio is either the primary
                    // selection or in the canvas multi-selection set.
                    let target_sel = Selection::Audio(aui);
                    let sel = state.selection == target_sel
                        || state.canvas_selection.contains(&target_sel);
                    let audio_id = egui::Id::new(("timeline_clip", "audio", aui));
                    if let Some(clicked) = draw_audio_clip(ui, painter, content_rect, &audio.id, audio_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        sel, track_h, track_locked, state.split_tool_active,
                        state.audio_waveforms.get(aui),
                        audio_source_start,
                        audio_speed)
                    {
                        if clicked == f32::INFINITY {
                            // Trim left: walk t_in forward and bump
                            // source_start by the same delta so the playback
                            // offset stays consistent (the audio doesn't
                            // appear to skip ahead under the user's hand).
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_in = (clip_start + delta_t).max(0.0).min(clip_end - 0.1);
                            let actual_delta = new_in - clip_start;
                            let token = EditorState::drag_token("trim_audio_left", aui);
                            state.mutate_drag(token, |s| {
                                let prev_src = s.audio[aui].source_start;
                                s.audio[aui].t_in = new_in;
                                s.audio[aui].source_start =
                                    (prev_src + actual_delta).max(0.0);
                            });
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Audio(aui),
                                token,
                            );
                            let aui_eff = match updated {
                                MovedClipKind::Audio(i) => i,
                                _ => aui,
                            };
                            to_select = Some(Selection::Audio(aui_eff));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right: extend / shrink the audible window.
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_out = (clip_end + delta_t).max(clip_start + 0.1);
                            let token = EditorState::drag_token("trim_audio_right", aui);
                            state.mutate_drag(token, |s| {
                                s.audio[aui].t_out = Some(new_out);
                            });
                            let updated = enforce_no_overlap_on_layer(
                                state,
                                MovedClipKind::Audio(aui),
                                token,
                            );
                            let aui_eff = match updated {
                                MovedClipKind::Audio(i) => i,
                                _ => aui,
                            };
                            to_select = Some(Selection::Audio(aui_eff));
                        } else if clicked < 0.0 {
                            // Drag: move the audio clip horizontally.
                            let new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;
                            // Track active drag for lane-lock & new-lane intents.
                            if state.timeline_drag.dragging_clip.is_none() {
                                state.timeline_drag.dragging_clip = Some(aui);
                                state.timeline_drag.pending_new_lane = None;
                                state.timeline_drag.start_pointer_y = pointer_y;
                            }
                            let token = EditorState::drag_token("move_audio", aui);
                            // Capture parent_actor binding (if any) so we
                            // can mirror the audio's horizontal move onto
                            // the video clip it's linked to. This makes
                            // audio<->video movement bidirectional —
                            // moving the actor already syncs its bound
                            // audio via `sync_audio_to_actor`; this path
                            // closes the loop in the other direction.
                            let parent_actor_id = state.scene.audio[aui].parent_actor.clone();
                            let prev_t_in = state.scene.audio[aui].t_in;
                            state.mutate_drag(token, |s| {
                                s.audio[aui].t_in = new_start;
                                s.audio[aui].t_out = Some(new_start + dur);
                            });
                            if let Some(parent_id) = parent_actor_id {
                                let dt = new_start - prev_t_in;
                                if dt.abs() > 1.0e-6 {
                                    if let Some(parent_idx) =
                                        state.scene.actors.iter().position(|a| a.id == parent_id)
                                    {
                                        // Shift the parent actor's window
                                        // by the same delta, dragging its
                                        // scene-time keyframes along.
                                        let _ = state.mutate_drag(token, |s| {
                                            let actor = &mut s.actors[parent_idx];
                                            let new_in = (actor.t_in.unwrap_or(0.0) + dt).max(0.0);
                                            let cur_dur = actor
                                                .t_out
                                                .map(|out| out - actor.t_in.unwrap_or(0.0))
                                                .unwrap_or(0.0);
                                            actor.t_in = Some(new_in);
                                            if cur_dur > 0.0 {
                                                actor.t_out = Some(new_in + cur_dur);
                                            }
                                            for kf in actor.layout.iter_mut() {
                                                kf.t += dt;
                                            }
                                        });
                                    }
                                }
                            }

                            // Vertical: only allow audio to land on audio
                            // lanes. Lane creation is deferred to drag-end
                            // via state.timeline_drag.pending_new_lane.
                            // Lane-lock hysteresis used to skip lane
                            // reassignment for primarily-horizontal drags
                            // — but the user explicitly asked for sound
                            // to move freely between layers, so we now
                            // honour every Y-direction motion as soon as
                            // the pointer crosses into a new row.
                            if let Some(py) = pointer_y {
                                let cur = state.audio_track_assignments.get(&aui).copied();
                                match classify_pointer_y(py, cur) {
                                    DropIntent::ToAudioRow(idx) => {
                                        // Detach from parent_actor when
                                        // the user drops the audio onto
                                        // an audio lane that does NOT
                                        // mirror the parent actor's
                                        // video lane. Without this the
                                        // sync_bound_audio_lanes pass
                                        // (running on the next frame
                                        // that the pointer is up) would
                                        // immediately yank the audio
                                        // back onto the parent's mirror
                                        // lane, defeating the move.
                                        let was_bound = state
                                            .scene
                                            .audio
                                            .get(aui)
                                            .map(|a| a.parent_actor.is_some())
                                            .unwrap_or(false);
                                        if was_bound {
                                            // Resolve the parent's mirror lane:
                                            // same vt_pos in audio_track_indices
                                            // as the parent's vt_pos in video_track_indices.
                                            let parent_id = state
                                                .scene
                                                .audio[aui]
                                                .parent_actor
                                                .clone();
                                            let mirror_lane =
                                                parent_id.as_ref().and_then(|id| {
                                                    state
                                                        .scene
                                                        .actors
                                                        .iter()
                                                        .position(|a| &a.id == id)
                                                        .and_then(|ai| {
                                                            let videos =
                                                                state.video_track_indices();
                                                            let parent_lane = state
                                                                .actor_track_assignments
                                                                .get(&ai)
                                                                .copied()
                                                                .or_else(|| {
                                                                    videos.first().copied()
                                                                })?;
                                                            let pos = videos
                                                                .iter()
                                                                .position(|&t| t == parent_lane)?;
                                                            state
                                                                .audio_track_indices()
                                                                .get(pos)
                                                                .copied()
                                                        })
                                                });
                                            if mirror_lane != Some(idx) {
                                                state.scene.audio[aui].parent_actor = None;
                                            }
                                        }
                                        state.audio_track_assignments.insert(aui, idx);
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                    DropIntent::NewAudioTop => {
                                        // Detach so the new lane sticks.
                                        if let Some(au) = state.scene.audio.get_mut(aui) {
                                            au.parent_actor = None;
                                        }
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::AudioTopForAudio(aui));
                                    }
                                    DropIntent::NewAudioBottom => {
                                        if let Some(au) = state.scene.audio.get_mut(aui) {
                                            au.parent_actor = None;
                                        }
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::AudioBottomForAudio(aui));
                                    }
                                    _ => {
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                }
                            }
                            // Defer overlap-trim until the drag ends —
                            // neighbours stay intact while the audio is
                            // still being moved.
                            defer_overlap_resolution(state, MovedClipKind::Audio(aui));
                            let aui_eff = aui;
                            to_select = Some(Selection::Audio(aui_eff));
                        } else {
                            // Plain click clears the cross-element
                            // multi-selection so the inspector returns
                            // to single-element mode (Ctrl+click toggles
                            // this audio track in/out of the canvas_selection set).
                            let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                            let target_sel = Selection::Audio(aui);
                            if ctrl_held {
                                if let Some(pos) = state.canvas_selection.iter().position(|&s| s == target_sel) {
                                    state.canvas_selection.remove(pos);
                                } else {
                                    state.canvas_selection.push(target_sel);
                                }
                            } else {
                                // Plain (non-Ctrl) click: replace the
                                // whole multi-selection with just this
                                // audio row so a single click never
                                // leaves stray entries in
                                // canvas_selection from a previous
                                // marquee / Ctrl-click set.
                                state.multi_select.clear();
                                state.canvas_selection.clear();
                                state.canvas_selection.push(target_sel);
                            }
                            to_select = Some(target_sel);
                        }
                    }
                }
            }
        }

        // ── Per-param keyframe rows (only for the currently-selected
        // layer's track). Renders a stack of small rows below the clip
        // bar with one diamond per keyframe time on the layer. Clicking
        // a diamond seeks the playhead and flashes the matching control
        // in the inspector.
        if expansion > 4.0 {
            if let Some((sel_layer, params)) = selected_layer_animated_params(state, track_idx) {
                let layer_label = match sel_layer {
                    Selection::Actor(ai) => crate::kf_anim::SelectedLayer::Actor(ai),
                    Selection::Overlay(oi) => crate::kf_anim::SelectedLayer::Overlay(oi),
                    _ => crate::kf_anim::SelectedLayer::RenderFrame,
                };
                // Compute the selected layer's clip range in scene-time so
                // the param rows can attach visually to the clip bar.
                let (clip_start_t, clip_end_t) = match sel_layer {
                    Selection::Actor(ai) => {
                        let a = &state.scene.actors[ai];
                        (a.t_in.unwrap_or(0.0), a.t_out.unwrap_or(duration))
                    }
                    Selection::Overlay(oi) => {
                        match &state.scene.overlays[oi] {
                            Overlay::Text(t) => (t.t_in, t.t_out),
                            Overlay::Image(im) => (im.t_in, im.t_out),
                            Overlay::Video(v) => (v.t_in, v.t_out),
                        }
                    }
                    _ => (0.0, duration),
                };
                let clip_x_start = (clip_start_t - state.timeline_scroll) * pps + track_left;
                let clip_x_end = (clip_end_t - state.timeline_scroll) * pps + track_left;
                // Per-parameter keyframe lists. Filtering to actual
                // change-points means a layer with 5 scale edits and 2
                // opacity edits shows 5 diamonds in the Scale row and 2
                // in the Opacity row — instead of all 7 in every row,
                // which the user explicitly called out as confusing.
                let param_kf_pairs = compute_param_change_points(state, sel_layer);
                let outcome = draw_param_kf_rows(
                    ui,
                    painter,
                    &layer_label,
                    &params,
                    &param_kf_pairs,
                    clip_top + track_h,
                    expansion,
                    track_left,
                    track_right,
                    pps,
                    state.timeline_scroll,
                    state.playhead,
                    &state.selected_keyframes,
                    clip_x_start,
                    clip_x_end,
                );
                for hit in outcome.click_hits {
                    param_row_clicks.push((layer_label.clone(), hit));
                }
                for ec in outcome.easing_changes {
                    param_row_easing_changes.push((layer_label.clone(), ec));
                }
            }
        }

        // ── Per-mask keyframe rows (above the clip). Mirrors the
        // transform-row block but draws into the strip we already
        // reserved at the top of the row via `mask_above`. Layers
        // with no animated mask params reserve no space and so this
        // block is a no-op for the common case.
        if mask_above > 4.0 {
            if let Some((sel_layer, rows)) = selected_layer_mask_param_rows(state, track_idx) {
                if !rows.is_empty() {
                    let layer_label = match sel_layer {
                        Selection::Actor(ai) => crate::kf_anim::SelectedLayer::Actor(ai),
                        Selection::Overlay(oi) => crate::kf_anim::SelectedLayer::Overlay(oi),
                        _ => crate::kf_anim::SelectedLayer::RenderFrame,
                    };
                    let (clip_start_t, clip_end_t) = match sel_layer {
                        Selection::Actor(ai) => {
                            let a = &state.scene.actors[ai];
                            (a.t_in.unwrap_or(0.0), a.t_out.unwrap_or(duration))
                        }
                        Selection::Overlay(oi) => {
                            match &state.scene.overlays[oi] {
                                Overlay::Text(t) => (t.t_in, t.t_out),
                                Overlay::Image(im) => (im.t_in, im.t_out),
                                Overlay::Video(v) => (v.t_in, v.t_out),
                            }
                        }
                        _ => (0.0, duration),
                    };
                    let clip_x_start =
                        (clip_start_t - state.timeline_scroll) * pps + track_left;
                    let clip_x_end = (clip_end_t - state.timeline_scroll) * pps + track_left;
                    let outcome = draw_mask_param_kf_rows(
                        ui,
                        painter,
                        &layer_label,
                        &rows,
                        row_top,
                        mask_above,
                        track_left,
                        track_right,
                        pps,
                        state.timeline_scroll,
                        state.playhead,
                        &state.selected_keyframes,
                        clip_x_start,
                        clip_x_end,
                    );
                    for hit in outcome.click_hits {
                        param_row_clicks.push((layer_label.clone(), hit));
                    }
                }
            }
        }

        // Playhead line on each track
        let ph_x = time_to_x(state.playhead, state.timeline_scroll, pps, track_left, track_right);
        if let Some(x) = ph_x {
            painter.line_segment(
                [egui::pos2(x, row_rect.min.y), egui::pos2(x, row_rect.max.y)],
                Stroke::new(1.0, COL_PLAYHEAD));
        }
    }

    // ── Per-param keyframe row click → seek + selection update ──
    if !param_row_clicks.is_empty() {
        // Always switch to the inspector's Transform tab so the user
        // sees the row that owns the clicked keyframe.
        state.inspector_tab = 0;
        for (layer_label, hit) in &param_row_clicks {
            // Update playhead.
            state.playhead = hit.seek_to.clamp(0.0, state.scene.output.duration);
            // Highlight the param row in the inspector for ~1.5 s so
            // the user can trace the click visually.
            state.kf_highlight.set(hit.param_id.clone());
            let entry = crate::kf_anim::SelectedKeyframe {
                layer: layer_label.clone(),
                param_id: hit.param_id.clone(),
                t: hit.t,
            };
            if hit.extend {
                if !state.selected_keyframes.contains(&entry) {
                    state.selected_keyframes.push(entry);
                } else {
                    state.selected_keyframes.retain(|e| e != &entry);
                }
            } else {
                state.selected_keyframes.clear();
                state.selected_keyframes.push(entry);
            }
        }
    }

    // ── Per-param keyframe row right-click → easing change ──
    if !param_row_easing_changes.is_empty() {
        let selected_kfs = state.selected_keyframes.clone();
        for (layer_label, change) in &param_row_easing_changes {
            // Build the list of (layer, t) pairs the change should
            // touch. Right-clicking a kf that's part of the current
            // multi-selection batches the easing onto every selected
            // kf; otherwise it's a one-off edit.
            let mut targets: Vec<(crate::kf_anim::SelectedLayer, f32)> = Vec::new();
            if change.apply_to_selection && !selected_kfs.is_empty() {
                for sk in &selected_kfs {
                    targets.push((sk.layer.clone(), sk.t));
                }
            } else {
                targets.push((layer_label.clone(), change.t));
            }
            for (layer, t) in targets {
                apply_easing_to_layer_kf(state, &layer, t, change.easing);
            }
        }
    }

    // ── Delete key removes selected keyframes from their layer ──
    let delete_pressed = ui.input(|i| {
        i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)
    });
    if delete_pressed && !state.selected_keyframes.is_empty() {
        delete_selected_keyframes(state);
    }

    // ── Auto-bump vertical zoom when the selection changes to a layer
    // with several animated parameters. Keeps the param rows readable
    // without forcing the user to drag the v-zoom by hand. Computed
    // once per selection change so we don't keep re-applying it every
    // paint while the layer is selected.
    if state.last_v_zoom_selection != Some(state.selection) {
        state.last_v_zoom_selection = Some(state.selection);
        let n_params = match state.selection {
            Selection::Actor(ai) => state.scene.actors.get(ai)
                .map(|a| a.animated_params.len()).unwrap_or(0),
            Selection::Overlay(oi) => state.scene.overlays.get(oi).map(|ov| match ov {
                Overlay::Text(t) => t.animated_params.len(),
                Overlay::Image(im) => im.animated_params.len(),
                Overlay::Video(v) => v.animated_params.len(),
            }).unwrap_or(0),
            _ => 0,
        };
        if n_params >= 3 {
            // Keep current zoom if it's already big enough; otherwise
            // grow proportionally to the param count so all rows fit.
            let want_h = 60.0 + (n_params as f32) * PARAM_ROW_BASE * 1.6;
            let target_zoom = (want_h / 40.0).clamp(1.0, 3.5);
            if state.timeline_v_zoom < target_zoom {
                state.timeline_v_zoom = target_zoom;
            }
        }
    }

    // ── Apply pending new-layer creation ONLY on drag end ──
    // During the drag we just stored the intent in `pending_new_lane`;
    // committing it would create stray empty lanes every time the
    // pointer crossed a gap on its way somewhere else. Now that the
    // mouse is up we can safely create the lane and snap the dragged
    // clip onto it. The intent is taken (cleared) regardless of
    // outcome so the next drag starts from a clean slate.
    if !any_pointer_down && state.timeline_drag.dragging_clip.is_some() {
        if let Some(intent) = state.timeline_drag.pending_new_lane.take() {
            use crate::state::NewLaneIntent;
            match intent {
                NewLaneIntent::VideoTopForActor(actor_idx) => {
                    let new_idx = state.insert_video_track_at_top();
                    state.actor_track_assignments.insert(actor_idx, new_idx);
                    state.status = "\u{2728} New video layer created on top.".into();
                }
                NewLaneIntent::VideoBottomForActor(actor_idx) => {
                    let new_idx = state.insert_video_track_at_bottom();
                    state.actor_track_assignments.insert(actor_idx, new_idx);
                    state.status = "\u{2728} New video layer created.".into();
                }
                NewLaneIntent::VideoTopForOverlay(overlay_idx) => {
                    let new_idx = state.insert_video_track_at_top();
                    state.overlay_track_assignments.insert(overlay_idx, new_idx);
                    state.status = "\u{2728} New video layer created on top.".into();
                }
                NewLaneIntent::VideoBottomForOverlay(overlay_idx) => {
                    let new_idx = state.insert_video_track_at_bottom();
                    state.overlay_track_assignments.insert(overlay_idx, new_idx);
                    state.status = "\u{2728} New video layer created.".into();
                }
                NewLaneIntent::AudioTopForAudio(audio_idx) => {
                    let new_idx = state.insert_audio_track_at_top();
                    state.audio_track_assignments.insert(audio_idx, new_idx);
                    state.status = "\u{2728} New audio layer created.".into();
                }
                NewLaneIntent::AudioBottomForAudio(audio_idx) => {
                    state.add_audio_track();
                    let new_track_idx = state.tracks.len() - 1;
                    state.audio_track_assignments.insert(audio_idx, new_track_idx);
                    state.status = "\u{2728} New audio layer created at bottom.".into();
                }
            }
        }
    }

    // ── Mirror bound audio onto the audio lane that matches the parent
    // actor's video lane. Standalone audio (parent_actor = None) keeps the
    // user's own placement. New audio lanes are appended on demand.
    //
    // SKIPPED while the user is actively dragging a clip — the sync
    // would otherwise immediately overwrite a hand-driven cross-lane
    // move and the user would see the bar snap back the instant they
    // release the mouse. The audio drag handler clears `parent_actor`
    // when a bound audio is dropped on a non-parent lane, so once the
    // drag ends `sync_bound_audio_lanes` is only applied to audio
    // rows the user actually chose to keep bound.
    let pointer_down = ui.input(|i| i.pointer.any_down());
    let timeline_drag_active =
        pointer_down && state.timeline_drag.dragging_clip.is_some();
    if !timeline_drag_active {
        sync_bound_audio_lanes(state);
    }

    // ── Keep the global ordering invariant: video tracks above audio
    // tracks. Cheap when already sorted.
    state.enforce_track_order();

    // Empty state
    if state.scene.actors.is_empty() && state.scene.overlays.is_empty()
        && state.scene.backgrounds.is_empty() && state.scene.audio.is_empty() {
        tracks_painter.text(
            tracks_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drag clips from the library to add them here",
            egui::FontId::proportional(12.0),
            COL_TEXT_DIM,
        );
    }

    // ── Custom scrollbars ──
    // Horizontal scrollbar drives both pan (timeline_scroll in seconds) and
    // local zoom (timeline_zoom in pixels-per-second). The thumb edges can be
    // dragged to resize the visible window.
    let visible_secs_h = (track_area_width / pps.max(1.0)).max(0.0);
    let total_h = duration.max(visible_secs_h.max(0.5));
    let view_a_h = (state.timeline_scroll / total_h).clamp(0.0, 1.0);
    let view_b_h = ((state.timeline_scroll + visible_secs_h) / total_h).clamp(view_a_h, 1.0);
    let (new_a_h, new_b_h) = stretchable_scrollbar(
        ui,
        h_sb_rect,
        true, // horizontal
        view_a_h,
        view_b_h,
    );
    {
        let new_window_secs = ((new_b_h - new_a_h) * total_h).max(0.05);
        state.timeline_scroll = (new_a_h * total_h).max(0.0);
        state.timeline_zoom = (track_area_width / new_window_secs).clamp(2.0, 2000.0);
    }

    // Vertical scrollbar drives both pan (timeline_v_scroll in pixels) and
    // local zoom (timeline_v_zoom multiplier on track heights).
    //
    // The thumb represents the visible window in CONTENT space, so its
    // size MUST be `viewport_h / total_tracks_h` (clamped). Previously
    // we used `1 / v_zoom`, which assumed the natural content height at
    // v_zoom=1 always equalled the viewport. With many tracks (or a
    // short panel) the content overflows even at v_zoom=1, but the
    // synthetic `1/v_zoom` formula reported a full-bar thumb — so
    // dragging the bar did nothing AND the post-frame map of `pan_frac`
    // back onto `view_a_v` rounded to 0, snapping `timeline_v_scroll`
    // to the top every frame. Net effect: the bottom-most audio row
    // was unreachable no matter how the user tried to scroll
    // (wheel, drag, stretch).
    //
    // The new contract: the thumb size is the real visibility ratio.
    // Stretching the thumb (resize via the edges) is reinterpreted as
    // a zoom change such that the new size matches the new content /
    // viewport ratio. Dragging the thumb middle pans as before.
    const V_ZOOM_MIN: f32 = 1.0;
    const V_ZOOM_MAX: f32 = 8.0;
    let max_v_scroll = (total_tracks_h - viewport_h).max(0.0);
    // Smallest thumb the widget will let the user produce when dragging
    // the resize grips — kept consistent with the v_zoom upper bound
    // (1 / V_ZOOM_MAX) so users can still stretch the thumb shorter to
    // zoom in even when the content is just barely overflowing.
    let min_thumb_frac = (1.0_f32 / V_ZOOM_MAX).min(1.0);
    let thumb_size_frac = (viewport_h / total_tracks_h.max(1.0))
        .clamp(min_thumb_frac, 1.0);
    let pan_frac = if max_v_scroll > 0.0 {
        (state.timeline_v_scroll / max_v_scroll).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let view_a_v = pan_frac * (1.0 - thumb_size_frac);
    let view_b_v = (view_a_v + thumb_size_frac).min(1.0);
    let (new_a_v, new_b_v) = stretchable_scrollbar(
        ui,
        v_sb_rect,
        false, // vertical
        view_a_v,
        view_b_v,
    );
    {
        let new_thumb_size = (new_b_v - new_a_v).clamp(min_thumb_frac, 1.0);
        // The user resized the thumb if its size changed. Translate the
        // new thumb size into a v_zoom by inverting the visibility
        // ratio: smaller thumb ↔ more total content ↔ larger v_zoom.
        // We solve `viewport_h / new_total = new_thumb_size` for
        // `new_v_zoom`, treating total_tracks_h as roughly linear in
        // v_zoom (the constant `BOTTOM_GUTTER` and small per-row
        // padding terms are absorbed into the residual — close enough
        // for a UI control where one pixel of slop is invisible).
        let zoom_changed = (new_thumb_size - thumb_size_frac).abs() > 1.0e-4;
        let new_v_zoom = if zoom_changed {
            let zoom_ratio = thumb_size_frac / new_thumb_size.max(1.0e-4);
            (v_zoom * zoom_ratio).clamp(V_ZOOM_MIN, V_ZOOM_MAX)
        } else {
            v_zoom
        };
        state.timeline_v_zoom = new_v_zoom;

        // Recompute total content height with the (possibly updated)
        // v_zoom, then map the new pan fraction back onto the new
        // scrollable range. This keeps the visible top-of-viewport
        // anchored to the same content fraction across zoom changes
        // and makes "drag thumb to bottom" reliably hit the last row
        // (including the bottom-most audio lane, which is the bug
        // this whole rewrite addresses).
        let new_total_tracks_h: f32 = RF_ROW_BASE_H * new_v_zoom
            + render_frame_expansion(state, new_v_zoom)
            + (0..num_tracks)
                .map(|i| {
                    state.tracks[i].height * new_v_zoom
                        + selected_layer_mask_above_height(state, i, new_v_zoom)
                        + selected_layer_expansion(state, i, new_v_zoom)
                })
                .sum::<f32>()
            + BOTTOM_GUTTER;
        let new_max_v_scroll = (new_total_tracks_h - viewport_h).max(0.0);
        let denom = (1.0 - new_thumb_size).max(1.0e-4);
        let new_pan_frac = (new_a_v / denom).clamp(0.0, 1.0);
        state.timeline_v_scroll = (new_pan_frac * new_max_v_scroll).max(0.0);
    }

    if let Some(sel) = to_select {
        state.selection = sel;
    }

    // ── Library clip drag-to-track: drop handling ──
    // When the user releases a clip dragged from the library, decide which
    // video lane it lands on and at what time, then add it as an actor.
    // Skip when the release point is outside this timeline panel's master
    // rect, so a drop on the canvas can be handled by the canvas-preview
    // panel instead of being absorbed here as a "new layer".
    let mouse_released = ui.input(|i| i.pointer.any_released());
    if state.asset_drag.dragging.is_some() && mouse_released {
        let mouse_pos = ui.input(|i| i.pointer.hover_pos());
        if let Some(pos) = mouse_pos {
            // Only consume the drop when the cursor is over the timeline.
            if !master_rect.contains(pos) {
                // Leave asset_drag intact; the canvas (or whoever else
                // owns the cursor) gets a chance to accept it.
                return;
            }
            // Resolve which track the drop is over using the same row-rect
            // table the clip-drag handlers use, so a clip lands on the
            // exact lane the user pointed at. Returns None if the cursor is
            // outside any existing row (we then create a new layer).
            let drop_track: Option<usize> = (|| {
                for (i, (top, bot)) in track_rows.iter().enumerate() {
                    if pos.y >= *top && pos.y < *bot {
                        return Some(i);
                    }
                }
                None
            })();

            // Determine time position from X
            let drop_time = x_to_time(pos.x, state.timeline_scroll, pps, track_left)
                .clamp(0.0, duration);

            let asset_path = state.asset_drag.dragging.clone().unwrap();
            let kind = state.asset_drag.kind;

            if matches!(kind, AssetDragKind::Clip | AssetDragKind::Video) {
                // Pick the destination video lane:
                //   1. The lane under the cursor when it is an unlocked
                //      video lane AND has nothing currently occupying
                //      `drop_time`. Reusing a non-empty lane would
                //      overwrite the existing layer, which the user
                //      explicitly does not want.
                //   2. Otherwise, the first empty video lane at
                //      `drop_time`, falling back to a freshly-inserted
                //      lane at the top of the video stack so the new
                //      clip always lands on its own row.
                let cursor_lane_empty = drop_track
                    .filter(|i| state.tracks[*i].kind == TrackKind::Video
                        && !state.tracks[*i].locked)
                    .filter(|i| {
                        // Walk the actors+overlays already pinned to
                        // this lane and check that none of them spans
                        // `drop_time`.
                        let mut occupied = false;
                        for (ai, a) in state.scene.actors.iter().enumerate() {
                            let assigned = state
                                .actor_track_assignments
                                .get(&ai)
                                .copied()
                                .unwrap_or_else(|| {
                                    state.video_track_indices()
                                        .first().copied().unwrap_or(0)
                                });
                            if assigned != *i { continue; }
                            let t_in = a.t_in.unwrap_or(0.0);
                            let t_out = a.t_out.unwrap_or(duration);
                            if drop_time >= t_in && drop_time <= t_out {
                                occupied = true;
                                break;
                            }
                        }
                        if occupied { return false; }
                        let default_overlay_lane = {
                            let v = state.video_track_indices();
                            if v.len() >= 2 { v[1] } else { v.first().copied().unwrap_or(0) }
                        };
                        for (oi, ov) in state.scene.overlays.iter().enumerate() {
                            let assigned = state
                                .overlay_track_assignments
                                .get(&oi)
                                .copied()
                                .unwrap_or(default_overlay_lane);
                            if assigned != *i { continue; }
                            let (t_in, t_out) = match ov {
                                memstroy_core::Overlay::Text(o) => (o.t_in, o.t_out),
                                memstroy_core::Overlay::Image(o) => (o.t_in, o.t_out),
                                memstroy_core::Overlay::Video(o) => (o.t_in, o.t_out),
                            };
                            if drop_time >= t_in && drop_time <= t_out {
                                occupied = true;
                                break;
                            }
                        }
                        !occupied
                    });
                let assigned = match cursor_lane_empty {
                    Some(t) => t,
                    None => state.pick_or_create_empty_video_lane_at(drop_time),
                };
                add_actor_from_clip_at_time(state, &asset_path, drop_time);
                if let Some(new_idx) = state.scene.actors.len().checked_sub(1) {
                    state.actor_track_assignments.insert(new_idx, assigned);
                    // The bound audio (added by add_actor_from_clip_at_time)
                    // mirrors the actor's lane via sync_bound_audio_lanes()
                    // at the end of the frame. The destination lane is
                    // guaranteed empty at `drop_time`, so we deliberately
                    // skip the same-lane overlap pass that used to delete
                    // neighbours here.
                }
            } else if matches!(kind, AssetDragKind::Sound | AssetDragKind::Image | AssetDragKind::Particle) {
                // Build a LibraryAsset proxy from the drag payload and
                // delegate to the per-kind spawner. The element lands
                // at the drop time; the spawner pins it onto the first
                // empty lane (or a freshly-inserted one) so a drop
                // never silently replaces an existing layer.
                let id = asset_path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("asset")
                    .to_string();
                let asset_label = state.asset_drag.label.clone();
                let asset = crate::state::LibraryAsset {
                    id: id.clone(),
                    path: asset_path.clone(),
                    label: if asset_label.is_empty() { id } else { asset_label },
                    thumbnail: state.asset_drag.thumbnail.clone(),
                };
                let saved_t = state.playhead;
                state.playhead = drop_time;
                add_library_asset_at_playhead(state, &asset, kind);
                state.playhead = saved_t;
            }

            // Clear the drag state.
            state.asset_drag.dragging = None;
            state.asset_drag.kind = AssetDragKind::None;
            state.asset_drag.label.clear();
            state.asset_drag.thumbnail = None;
        }
    }

    // ── Draw visual indicator while dragging from library ──
    if let Some(ref _dragged_path) = state.asset_drag.dragging {
        // Follow the cursor live so the preview tracks the drop intent.
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            state.asset_drag.pos = [pos.x, pos.y];
        }
        let drag_pos = egui::pos2(state.asset_drag.pos[0], state.asset_drag.pos[1]);

        // Highlight the destination video / audio lane and emit a
        // human-readable label depending on the drag kind.
        let dest_label = match state.asset_drag.kind {
            AssetDragKind::Clip | AssetDragKind::Video | AssetDragKind::Sound | AssetDragKind::Image | AssetDragKind::Particle => {
                let want_video = !matches!(state.asset_drag.kind, AssetDragKind::Sound);
                let target = (|| {
                    for (i, (top, bot)) in track_rows.iter().enumerate() {
                        if drag_pos.y >= *top && drag_pos.y < *bot {
                            let lane_kind_ok = if want_video {
                                state.tracks[i].kind == TrackKind::Video
                            } else {
                                state.tracks[i].kind == TrackKind::Audio
                            };
                            if lane_kind_ok {
                                return Some((*top, *bot, state.tracks[i].name.clone()));
                            }
                        }
                    }
                    None
                })();
                if let Some((top, bot, name)) = target {
                    let highlight = egui::Rect::from_min_max(
                        egui::pos2(tracks_rect.min.x, top),
                        egui::pos2(tracks_rect.max.x, bot),
                    );
                    let col = if want_video {
                        Color32::from_rgba_premultiplied(120, 220, 120, 30)
                    } else {
                        Color32::from_rgba_premultiplied(120, 200, 240, 30)
                    };
                    ui.painter().rect_filled(highlight, Rounding::same(2.0), col);
                    ui.painter().rect_stroke(
                        highlight,
                        Rounding::same(2.0),
                        Stroke::new(1.5, Color32::from_rgb(120, 220, 120)),
                    );
                    format!("\u{2192} {}", name)
                } else if want_video {
                    "\u{2192} New layer".to_string()
                } else {
                    "\u{2192} Audio lane".to_string()
                }
            }
            AssetDragKind::None => String::new(),
        };

        // Floating preview card next to the cursor: thumbnail + label.
        let label = if state.asset_drag.label.is_empty() {
            "Drop here".to_string()
        } else {
            state.asset_drag.label.clone()
        };
        let label_text = if dest_label.is_empty() { label.clone() }
            else { format!("{}    {}", label, dest_label) };

        let card_w = 180.0_f32;
        let card_h = 56.0_f32;
        // Anchor the preview slightly below-right of the cursor.
        let anchor = drag_pos + egui::vec2(14.0, 10.0);
        let card_rect = egui::Rect::from_min_size(anchor, Vec2::new(card_w, card_h));
        ui.painter().rect_filled(
            card_rect,
            Rounding::same(6.0),
            Color32::from_rgba_premultiplied(20, 20, 30, 230),
        );
        ui.painter().rect_stroke(
            card_rect,
            Rounding::same(6.0),
            Stroke::new(1.5, Color32::from_rgb(255, 200, 50)),
        );
        let thumb_size = Vec2::splat(48.0);
        let thumb_rect = egui::Rect::from_min_size(card_rect.min + egui::vec2(4.0, 4.0), thumb_size);
        if let Some(thumb) = &state.asset_drag.thumbnail {
            // Use a real image (egui's image-loaders are already installed).
            let uri = format!("file://{}", thumb.display());
            let img = egui::Image::from_uri(uri)
                .fit_to_exact_size(thumb_size)
                .maintain_aspect_ratio(false)
                .rounding(Rounding::same(3.0))
                .tint(Color32::from_white_alpha(220));
            img.paint_at(ui, thumb_rect);
        } else {
            ui.painter().rect_filled(thumb_rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 60));
            let icon = match state.asset_drag.kind {
                AssetDragKind::Clip | AssetDragKind::Video => "\u{1F3AC}",
                AssetDragKind::Sound => "\u{1F50A}",
                AssetDragKind::Image => "\u{1F5BC}",
                AssetDragKind::Particle => "\u{2728}",
                AssetDragKind::None => "?",
            };
            ui.painter().text(
                thumb_rect.center(),
                egui::Align2::CENTER_CENTER,
                icon,
                egui::FontId::proportional(24.0),
                Color32::from_rgb(255, 200, 50),
            );
        }
        // Two-line label: name + destination hint.
        let text_anchor = thumb_rect.right_top() + egui::vec2(6.0, 2.0);
        ui.painter().text(
            text_anchor,
            egui::Align2::LEFT_TOP,
            &label_text,
            egui::FontId::proportional(11.0),
            Color32::from_rgb(240, 240, 255),
        );
        // Drop-time pill at the bottom of the card.
        let drop_t_secs = x_to_time(drag_pos.x, state.timeline_scroll, pps, track_left)
            .clamp(0.0, duration);
        ui.painter().text(
            card_rect.left_bottom() + egui::vec2(6.0, -4.0),
            egui::Align2::LEFT_BOTTOM,
            format!("@ {:.2}s", drop_t_secs),
            egui::FontId::proportional(10.0),
            COL_TEXT_DIM,
        );
    }

    // ── Reset drag state when mouse is released (no active drag) ──
    let any_dragging = ui.input(|i| i.pointer.any_down());
    if !any_dragging {
        // Drain any deferred overlap-resolution requests collected
        // during the gesture and apply them once. We dedupe so the
        // same clip isn't resolved repeatedly when the user drags
        // through several track positions.
        let pending = std::mem::take(&mut state.timeline_drag.pending_overlap);
        if !pending.is_empty() {
            let drop_token = EditorState::drag_token("overlap_resolve_end", 0);
            state.mutate_drag(drop_token, |_| {});
            let mut seen: std::collections::HashSet<crate::state::PendingOverlapMover> =
                std::collections::HashSet::new();
            // The mover indices may shift as enforce_* deletes / splits
            // neighbours; for a single mover we just apply once.
            for entry in pending {
                if !seen.insert(entry) { continue; }
                let _ = enforce_no_overlap_on_layer(state, entry.into(), drop_token);
            }
            state.end_drag_group();
        }

        state.timeline_drag.dragging_clip = None;
        state.timeline_drag.pending_new_lane = None;
        state.timeline_drag.start_pointer_y = None;
        // Clear any in-flight element-drag that wasn't consumed by a
        // skeleton drop zone (e.g. user Alt-dragged a clip but
        // released over the timeline). The inspector's drop zones
        // already clear it on a successful attach; this is the
        // fallback for "released elsewhere".
        if state.element_drag.source.is_some() {
            state.element_drag.source = None;
            state.element_drag.label.clear();
        }
    }

    // ── Marquee (rubber-band) selection on the timeline ──
    // Mirrors the canvas marquee but in screen coords. Triggered when
    // the user starts dragging on an empty area of the tracks viewport
    // (no clip / asset / clip-trim drag in flight). On release, every
    // clip whose screen rectangle intersects the lasso is added to
    // `state.canvas_selection` (the same multi-selection set used by
    // the canvas, so Ctrl+C copies them all together).
    timeline_marquee_update(
        ui,
        state,
        tracks_rect,
        track_rows.as_slice(),
        rf_row_h,
        v_scroll,
        pps,
        track_left,
        track_right,
    );
}


/// Stretchable scrollbar widget. Shared between the horizontal time scrollbar
/// and the vertical track scrollbar.
///
/// `view_a_frac`/`view_b_frac` are the start/end of the visible window as
/// fractions of the total content (both in [0, 1], `a <= b`).
///
/// Returns the new (a, b) after user interaction this frame:
///   - dragging the thumb body pans (a and b shift by the same amount);
///   - dragging the thumb's leading/trailing edge resizes the visible window
///     (this is the "local zoom" the user controls by stretching the scrollbar).
fn stretchable_scrollbar(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    horizontal: bool,
    view_a_frac: f32,
    view_b_frac: f32,
) -> (f32, f32) {
    let painter = ui.painter_at(rect);

    // Scrollbar track background.
    let bg_rounding = if horizontal { rect.height() * 0.5 } else { rect.width() * 0.5 };
    painter.rect_filled(rect, Rounding::same(bg_rounding), Color32::from_rgb(20, 20, 30));

    let track_len = if horizontal { rect.width() } else { rect.height() }.max(1.0);
    let cross = if horizontal { rect.height() } else { rect.width() };
    // Edge-grip zone for "stretch the thumb" vs "pan the thumb".
    //
    // The user explicitly called out that grabbing the resize ends of
    // the local-zoom thumb was difficult, so we want this generous
    // (12 px on each side comfortably catches imprecise targeting on a
    // standard 14-px scrollbar). BUT the zone must never grow so wide
    // that it consumes the entire thumb — at maximum local zoom the
    // thumb shrinks to ~1/V_ZOOM_MAX of the track length. A bare-minimum
    // 12-px edge zone on a 14-px-wide vertical scrollbar with a short
    // viewport (~150 px) produces an ~18 px thumb, where two 12-px
    // edges overlap completely and **every** click hits Mode::ResizeStart.
    // The user is then physically unable to drag the thumb down — every
    // drag just shrinks the visible window. That's the
    // "audio layer unreachable at max vertical zoom" report: the
    // bottom audio rows existed in the layout, but the only thing the
    // scrollbar would let the user do was *zoom out* instead of *pan
    // down*.
    //
    // The remedy is to keep the edge zones from ever covering more
    // than 1/3 of the thumb each, leaving at least 1/3 in the middle
    // for Mode::Pan. We then enforce a tiny absolute floor (3 px) so
    // the resize affordance still exists even on very short scrollbars.
    let view_window_frac = (view_b_frac - view_a_frac).clamp(0.0, 1.0);
    let thumb_pixels = (view_window_frac * track_len).max(1.0);
    let preferred_edge_zone = (cross * 0.9).max(12.0);
    let edge_zone = preferred_edge_zone
        .min((thumb_pixels / 3.0).max(3.0))
        .max(3.0);
    let min_window_frac = (10.0 / track_len).min(0.5);

    let a = view_a_frac.clamp(0.0, 1.0);
    let b = view_b_frac.clamp(a + min_window_frac.min(0.001), 1.0);

    let id = ui.make_persistent_id((
        "scrollbar",
        rect.min.x as i32,
        rect.min.y as i32,
        horizontal,
    ));
    // Expand the interaction rect on the cross-axis (and a few pixels
    // on the main axis) so users can click slightly outside the
    // visual bar and still grab it. The painter still draws inside
    // `rect`, so the visual thickness is unchanged. 6 px on each side
    // of the cross-axis turns a 14-px bar into a 26-px hit area.
    let hit_pad_cross = 6.0_f32;
    let hit_pad_main = 2.0_f32;
    let hit_rect = if horizontal {
        rect.expand2(Vec2::new(hit_pad_main, hit_pad_cross))
    } else {
        rect.expand2(Vec2::new(hit_pad_cross, hit_pad_main))
    };
    let resp = ui.interact(hit_rect, id, Sense::click_and_drag());

    // Compute thumb pixel range along the primary axis.
    let main_min = if horizontal { rect.min.x } else { rect.min.y };
    let thumb_l = main_min + a * track_len;
    let thumb_r = main_min + b * track_len;

    // Persist drag mode across frames.
    let mode_key = id.with("mode");
    #[derive(Clone, Copy, PartialEq)]
    enum Mode { None, Pan, ResizeStart, ResizeEnd }
    let stored_raw: Option<u8> = ui.ctx().memory(|m| m.data.get_temp(mode_key));
    let mut mode = match stored_raw {
        Some(0) => Mode::Pan,
        Some(1) => Mode::ResizeStart,
        Some(2) => Mode::ResizeEnd,
        _ => Mode::None,
    };

    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            let coord = if horizontal { p.x } else { p.y };
            mode = if (coord - thumb_l).abs() < edge_zone {
                Mode::ResizeStart
            } else if (coord - thumb_r).abs() < edge_zone {
                Mode::ResizeEnd
            } else {
                Mode::Pan
            };
            let raw: u8 = match mode {
                Mode::Pan => 0,
                Mode::ResizeStart => 1,
                Mode::ResizeEnd => 2,
                Mode::None => 3,
            };
            ui.ctx().memory_mut(|m| m.data.insert_temp(mode_key, raw));
        }
    }
    // Only reset the captured drag mode once the user has actually
    // RELEASED the primary pointer button. The previous code reset on
    // any frame where `dragged()` was false (which can happen for a
    // single frame when the pointer momentarily stops moving while
    // still pressed) — that lost the captured mode and forced a
    // re-detection on the next motion, producing the "застревание"
    // (stuck) feel the user reported when stretching scrollbar grips.
    let pointer_released = ui.input(|i| !i.pointer.primary_down());
    if pointer_released && !resp.drag_started() && !resp.dragged() {
        mode = Mode::None;
        ui.ctx().memory_mut(|m| m.data.insert_temp(mode_key, 3u8));
    }

    // Hover cursor hint.
    if resp.hovered() {
        if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
            let coord = if horizontal { p.x } else { p.y };
            if (coord - thumb_l).abs() < edge_zone || (coord - thumb_r).abs() < edge_zone {
                ui.ctx().set_cursor_icon(if horizontal {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                });
            } else if coord >= thumb_l && coord <= thumb_r {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
    }

    let mut new_a = a;
    let mut new_b = b;

    if resp.dragged() {
        // Force a repaint next frame so smooth pointer motion produces
        // smooth scrollbar updates even when nothing else in the UI
        // changes between the two frames.
        ui.ctx().request_repaint();
        let d_pixels = if horizontal {
            resp.drag_delta().x
        } else {
            resp.drag_delta().y
        };
        let d_frac = d_pixels / track_len;
        match mode {
            Mode::Pan => {
                let w = b - a;
                new_a = (a + d_frac).clamp(0.0, (1.0 - w).max(0.0));
                new_b = new_a + w;
            }
            Mode::ResizeStart => {
                new_a = (a + d_frac).clamp(0.0, (b - min_window_frac).max(0.0));
            }
            Mode::ResizeEnd => {
                new_b = (b + d_frac).clamp((a + min_window_frac).min(1.0), 1.0);
            }
            Mode::None => {}
        }
    } else if resp.clicked() {
        // Click on track outside thumb: jump (centre thumb at click).
        if let Some(p) = resp.interact_pointer_pos() {
            let coord = if horizontal { p.x } else { p.y };
            let frac = ((coord - main_min) / track_len).clamp(0.0, 1.0);
            let w = b - a;
            new_a = (frac - w * 0.5).clamp(0.0, (1.0 - w).max(0.0));
            new_b = new_a + w;
        }
    }

    // Draw thumb at the (possibly updated) position.
    let display_a = new_a;
    let display_b = new_b.max(new_a + (10.0 / track_len).min(0.999));
    let main_l = main_min + display_a * track_len;
    let main_r = (main_min + display_b * track_len).min(if horizontal { rect.max.x } else { rect.max.y });

    let thumb_rect = if horizontal {
        egui::Rect::from_min_max(
            egui::pos2(main_l, rect.min.y + 2.0),
            egui::pos2(main_r, rect.max.y - 2.0),
        )
    } else {
        egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, main_l),
            egui::pos2(rect.max.x - 2.0, main_r),
        )
    };
    let thumb_color = if resp.hovered() || resp.dragged() {
        Color32::from_rgb(140, 140, 180)
    } else {
        Color32::from_rgb(90, 90, 130)
    };
    painter.rect_filled(thumb_rect, Rounding::same(bg_rounding * 0.8), thumb_color);

    // Draw the two stretch grips inside the thumb edges.
    let grip_color = Color32::from_rgba_premultiplied(255, 255, 255, 90);
    if horizontal {
        let grip_w = 2.0;
        let inset_x = 3.0;
        let y0 = thumb_rect.min.y + 2.0;
        let y1 = thumb_rect.max.y - 2.0;
        if thumb_rect.width() > 14.0 {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(thumb_rect.min.x + inset_x, y0),
                    egui::pos2(thumb_rect.min.x + inset_x + grip_w, y1),
                ),
                Rounding::ZERO, grip_color,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(thumb_rect.max.x - inset_x - grip_w, y0),
                    egui::pos2(thumb_rect.max.x - inset_x, y1),
                ),
                Rounding::ZERO, grip_color,
            );
        }
    } else {
        let grip_h = 2.0;
        let inset_y = 3.0;
        let x0 = thumb_rect.min.x + 2.0;
        let x1 = thumb_rect.max.x - 2.0;
        if thumb_rect.height() > 14.0 {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, thumb_rect.min.y + inset_y),
                    egui::pos2(x1, thumb_rect.min.y + inset_y + grip_h),
                ),
                Rounding::ZERO, grip_color,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, thumb_rect.max.y - inset_y - grip_h),
                    egui::pos2(x1, thumb_rect.max.y - inset_y),
                ),
                Rounding::ZERO, grip_color,
            );
        }
    }

    (new_a, new_b)
}


/// Draw a single clip bar on the timeline. Returns Some(time) if clicked (for split or select).
/// Returns special sentinel values for edge-trim drags:
/// - `f32::INFINITY` signals "trim left edge"
/// - `f32::NEG_INFINITY` signals "trim right edge"
/// - Negative values signal whole-clip drag (new start time encoded as `-new_start`)
/// Shows ResizeHorizontal cursor when hovering within 5px of left/right edge.
///
/// `clip_id` MUST be stable across frames for the same clip (do not include the
/// clip's time in the id, or egui's drag tracking breaks the moment the clip
/// position changes — the user has to release and re-click on every frame).
#[allow(clippy::too_many_arguments)]
fn draw_clip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    content_rect: egui::Rect,
    label: &str,
    clip_id: egui::Id,
    clip_start: f32,
    clip_end: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    color: Color32,
    selected: bool,
    _track_h: f32,
    locked: bool,
    split_mode: bool,
) -> Option<f32> {
    let x_start = (clip_start - scroll) * pps + track_left;
    let x_end = (clip_end - scroll) * pps + track_left;

    // Clip is off-screen
    if x_end < track_left || x_start > track_right { return None; }

    let x_start = x_start.max(track_left);
    let x_end = x_end.min(track_right);

    if x_end - x_start < 2.0 { return None; }

    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(x_start, content_rect.min.y + 2.0),
        egui::pos2(x_end, content_rect.max.y - 2.0),
    );

    // Fill
    let fill = if selected {
        Color32::from_rgb(color.r().saturating_add(30), color.g().saturating_add(30), color.b().saturating_add(30))
    } else { color };
    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Color-coded left stripe (3px wide, brighter) for visual clip identification
    {
        let stripe_w = 3.0_f32;
        let stripe_rect = egui::Rect::from_min_max(
            egui::pos2(bar_rect.min.x, bar_rect.min.y + 1.0),
            egui::pos2(bar_rect.min.x + stripe_w, bar_rect.max.y - 1.0),
        );
        let stripe_color = Color32::from_rgb(
            color.r().saturating_add(60),
            color.g().saturating_add(60),
            color.b().saturating_add(60),
        );
        painter.rect_filled(stripe_rect, Rounding::same(2.0), stripe_color);
    }

    // Selection border
    if selected {
        painter.rect_stroke(bar_rect.expand(1.0), Rounding::same(5.0), Stroke::new(2.0, COL_SELECTED));
    }

    // Label inside
    if bar_rect.width() > 30.0 {
        let text = if bar_rect.width() > 80.0 { label.to_string() } else { ellipsis(label, 6) };
        painter.text(
            egui::pos2(bar_rect.min.x + 8.0, bar_rect.center().y),
            egui::Align2::LEFT_CENTER, &text,
            egui::FontId::proportional(10.0), Color32::WHITE);
    }

    // Interaction (click/drag with edge-trim zones).
    //
    // The id MUST be stable across frames for the same clip. Hashing the
    // clip's time into the id (as we used to) caused the id to change every
    // frame during a drag — egui then dropped its drag state and the user
    // had to release and re-press for each pixel of motion. The caller now
    // supplies a stable per-clip id.
    let id = clip_id;
    let sense = if locked { Sense::hover() } else { Sense::click_and_drag() };
    let resp = ui.interact(bar_rect, id, sense);

    // ── Global gestures that lock out per-clip interaction ──
    //
    // The timeline function may have already claimed the press for
    // higher-priority gestures (playhead scrubbing, in-flight asset
    // drag). Bail out early so the user sees the dragged playhead /
    // dropped asset instead of the clip stealing the click. We still
    // returned the `resp` above so egui's hover-cursor logic on lower
    // widgets keeps working.
    let global_lock = ui
        .data(|d| d.get_temp::<bool>(egui::Id::new("timeline_input_lock")))
        .unwrap_or(false);
    if global_lock {
        return None;
    }

    // Edge detection for hover cursor (purely cosmetic; the actual drag mode
    // is captured once at drag_started below and locked for the rest of the
    // drag, so the cursor flicker doesn't affect behaviour).
    let hover_pos = ui.input(|i| i.pointer.hover_pos());

    // Generous hit zone: 9 px on each side of the bar's edge so the
    // resize handles are easy to grab on regular and touch displays.
    // Keeping the visual handle width at 5 px (drawn below) keeps the
    // bar looking clean while the hit-test forgives small targeting
    // mistakes — addresses the explicit user feedback that the
    // stretch handles were "очень трудно схватиться".
    const TRIM_HIT_HALFWIDTH: f32 = 9.0;
    let near_left_edge = hover_pos
        .map(|p| (p.x - bar_rect.min.x).abs() < TRIM_HIT_HALFWIDTH)
        .unwrap_or(false);
    let near_right_edge = hover_pos
        .map(|p| (p.x - bar_rect.max.x).abs() < TRIM_HIT_HALFWIDTH)
        .unwrap_or(false);

    if resp.hovered() && !locked {
        if split_mode {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if near_left_edge || near_right_edge {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    // ── Visual trim handles ──
    //
    // Two slim white bars at the very left/right edges so the user can see
    // where to grab to change the clip's duration. Highlighted when the
    // pointer is near, dim when not, hidden when the bar is locked or in
    // split mode (cosmetic; the hit-test is still active).
    if !locked && !split_mode {
        draw_clip_trim_handles(painter, bar_rect, near_left_edge, near_right_edge);
    }

    if resp.clicked() {
        if split_mode {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = x_to_time(pos.x, scroll, pps, track_left);
                return Some(t);
            }
        }
        return Some(clip_start); // signal selection
    }

    // ── Drag handling ────────────────────────────────────────────────
    //
    // Strategy:
    //   * On `drag_started`, freeze the drag mode (Move / TrimLeft /
    //     TrimRight) and snapshot the clip's original start time and the
    //     pointer's press origin. We stash these in egui's per-id temp
    //     memory so they survive across frames.
    //   * On every subsequent `dragged` frame, recompute the proposed new
    //     position from the *total* pointer displacement since press origin,
    //     not from per-frame deltas applied on top of an already-mutated
    //     value. This avoids feedback loops with snapping (where a snapped
    //     position would re-feed itself and cause jitter or sticking) and
    //     keeps motion 1:1 with the cursor.
    let mode_id = id.with("drag_mode");
    let origin_id = id.with("press_origin_x");
    let original_start_id = id.with("original_start");

    if resp.drag_started() && !locked && !split_mode {
        let press_x = ui
            .input(|i| i.pointer.press_origin())
            .map(|p| p.x)
            .unwrap_or(bar_rect.center().x);
        // Match the visible hover hit zone (TRIM_HIT_HALFWIDTH = 9 px)
        // so the cursor and the actual drag-mode capture stay in sync.
        let mode = if (press_x - bar_rect.min.x).abs() < TRIM_HIT_HALFWIDTH {
            ClipDragMode::TrimLeft
        } else if (press_x - bar_rect.max.x).abs() < TRIM_HIT_HALFWIDTH {
            ClipDragMode::TrimRight
        } else {
            ClipDragMode::Move
        };
        ui.data_mut(|d| {
            d.insert_temp(mode_id, mode);
            d.insert_temp(origin_id, press_x);
            d.insert_temp(original_start_id, clip_start);
        });
    }

    if resp.dragged() && !locked && !split_mode {
        let mode: Option<ClipDragMode> = ui.data(|d| d.get_temp(mode_id));
        let press_x: Option<f32> = ui.data(|d| d.get_temp(origin_id));
        let original_start: Option<f32> = ui.data(|d| d.get_temp(original_start_id));
        let cur_x = ui
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
            .map(|p| p.x);

        if let (Some(mode), Some(px), Some(os), Some(cx)) =
            (mode, press_x, original_start, cur_x)
        {
            let total_dx = cx - px;
            match mode {
                ClipDragMode::TrimLeft => return Some(f32::INFINITY),
                ClipDragMode::TrimRight => return Some(f32::NEG_INFINITY),
                ClipDragMode::Move => {
                    let total_dt = total_dx / pps;
                    return Some(-(os + total_dt));
                }
            }
        }
        return Some(clip_start); // fall back to bare select
    }

    None
}


/// Paint the two slim trim handles on the leading and trailing edge of a
/// timeline clip bar. Highlighted when the pointer is currently near the
/// corresponding edge so users get the same affordance Premiere/Resolve
/// give them. Called for actors / overlays / backgrounds / audio — any
/// row whose duration can be stretched on the timeline.
fn draw_clip_trim_handles(
    painter: &egui::Painter,
    bar_rect: egui::Rect,
    near_left: bool,
    near_right: bool,
) {
    if bar_rect.width() < 8.0 {
        return;
    }
    // Wider, higher-contrast handles than the legacy 3px slivers — the
    // user feedback was that resize handles weren't discoverable. We
    // also draw the classic two-bar grip glyph when the pointer is
    // near so it's obvious the edge is interactive.
    let handle_w = 5.0_f32;
    let dim = Color32::from_rgba_premultiplied(255, 255, 255, 80);
    let hot = Color32::from_rgb(255, 255, 255);
    let grip_col = Color32::from_rgb(20, 20, 30);

    // Left handle.
    let left = egui::Rect::from_min_max(
        bar_rect.min,
        egui::pos2(bar_rect.min.x + handle_w, bar_rect.max.y),
    );
    painter.rect_filled(left, Rounding::same(2.0), if near_left { hot } else { dim });
    if near_left {
        painter.rect_stroke(
            left.expand2(Vec2::new(1.0, 0.0)),
            Rounding::same(2.5),
            Stroke::new(1.0, Color32::BLACK),
        );
        // Two-bar grip glyph for the unambiguous "drag-edge" cursor.
        let cy = left.center().y;
        let g_x1 = left.center().x - 1.0;
        let g_x2 = left.center().x + 1.0;
        let g_h = (left.height() * 0.5).min(8.0);
        painter.line_segment(
            [egui::pos2(g_x1, cy - g_h * 0.5), egui::pos2(g_x1, cy + g_h * 0.5)],
            Stroke::new(1.0, grip_col),
        );
        painter.line_segment(
            [egui::pos2(g_x2, cy - g_h * 0.5), egui::pos2(g_x2, cy + g_h * 0.5)],
            Stroke::new(1.0, grip_col),
        );
    }

    // Right handle.
    let right = egui::Rect::from_min_max(
        egui::pos2(bar_rect.max.x - handle_w, bar_rect.min.y),
        bar_rect.max,
    );
    painter.rect_filled(right, Rounding::same(2.0), if near_right { hot } else { dim });
    if near_right {
        painter.rect_stroke(
            right.expand2(Vec2::new(1.0, 0.0)),
            Rounding::same(2.5),
            Stroke::new(1.0, Color32::BLACK),
        );
        let cy = right.center().y;
        let g_x1 = right.center().x - 1.0;
        let g_x2 = right.center().x + 1.0;
        let g_h = (right.height() * 0.5).min(8.0);
        painter.line_segment(
            [egui::pos2(g_x1, cy - g_h * 0.5), egui::pos2(g_x1, cy + g_h * 0.5)],
            Stroke::new(1.0, grip_col),
        );
        painter.line_segment(
            [egui::pos2(g_x2, cy - g_h * 0.5), egui::pos2(g_x2, cy + g_h * 0.5)],
            Stroke::new(1.0, grip_col),
        );
    }
}


/// Draw an audio clip with waveform visualization.
///
/// `source_start` is the offset INTO THE SOURCE FILE that maps to
/// `clip_start` on the timeline; `clip_dur_total` is the original
/// (unclipped-by-viewport) clip length in scene-time seconds. The two
/// together let us pick the right slice of the pre-computed peaks vector
/// — when the user trims either edge of the clip, the waveform CROPS
/// instead of stretching, because the bar pixels keep mapping to the
/// same source-time positions.
#[allow(clippy::too_many_arguments)]
fn draw_audio_clip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    content_rect: egui::Rect,
    label: &str,
    clip_id: egui::Id,
    clip_start: f32,
    clip_end: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    selected: bool,
    _track_h: f32,
    locked: bool,
    _split_mode: bool,
    waveform: Option<&crate::state::AudioWaveform>,
    source_start: f32,
    // Audio playback speed multiplier — `1.0` = neutral, `2.0` = the
    // source plays twice as fast (so each visible bar pixel maps to
    // twice as many source-time seconds), `0.5` = half-speed (each
    // pixel maps to half as many). Without this the waveform would
    // keep showing the source at native rate while the bar shrinks /
    // stretches with speed, leaving the peaks visually misaligned
    // with what the user hears.
    speed: f32,
) -> Option<f32> {
    let x_start_full = (clip_start - scroll) * pps + track_left;
    let x_end_full = (clip_end - scroll) * pps + track_left;

    if x_end_full < track_left || x_start_full > track_right { return None; }

    let x_start = x_start_full.max(track_left);
    let x_end = x_end_full.min(track_right);
    if x_end - x_start < 2.0 { return None; }

    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(x_start, content_rect.min.y + 2.0),
        egui::pos2(x_end, content_rect.max.y - 2.0),
    );

    // Fill
    let fill = if selected { Color32::from_rgb(70, 200, 200) } else { COL_CLIP_AUDIO };
    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Draw waveform or fallback visualization
    if let Some(wf) = waveform {
        if wf.ready && !wf.peaks.is_empty() && wf.duration > 0.001 {
            let bar_w = bar_rect.width();
            let bar_h = bar_rect.height();
            let center_y = bar_rect.center().y;
            let num_samples = (bar_w as usize).min(wf.peaks.len()).max(1);

            if num_samples > 1 {
                // Map each on-screen pixel x to a source-file time, then
                // pick the corresponding peak. Because we use the FULL
                // (unclipped) bar's left/right to compute source-time,
                // viewport scrolling and edge-trims both crop the
                // waveform correctly — the visible bar is the audible
                // window, drawn at the file's natural amplitude scale.
                use egui::epaint::{Mesh, Vertex, WHITE_UV};
                let color = Color32::from_rgba_premultiplied(255, 255, 255, 120);
                let bar_pixel_w = (bar_w / num_samples as f32).max(1.0);
                let mut mesh = Mesh::default();
                mesh.vertices.reserve(num_samples * 4);
                mesh.indices.reserve(num_samples * 6);

                let full_bar_w = (x_end_full - x_start_full).max(1.0);
                // Source-time at the LEFT edge of the visible bar.
                // `speed` converts scene-time → source-time so the
                // waveform compresses or stretches in lock-step with
                // the audio you actually hear.
                let speed_clamped = speed.max(0.0001);
                let visible_offset_pix = x_start - x_start_full;
                let source_t_at_visible_start = source_start
                    + (visible_offset_pix / full_bar_w) * (clip_end - clip_start) * speed_clamped;
                let source_t_per_pixel = (clip_end - clip_start) * speed_clamped / full_bar_w;
                let peaks_per_sec = wf.peaks.len() as f32 / wf.duration;

                for i in 0..num_samples {
                    let pix_in_visible = (i as f32 / num_samples as f32) * bar_w;
                    let source_t =
                        source_t_at_visible_start + pix_in_visible * source_t_per_pixel;
                    let peak_idx = (source_t * peaks_per_sec) as isize;
                    let peak = if peak_idx < 0 {
                        0.0
                    } else if (peak_idx as usize) < wf.peaks.len() {
                        wf.peaks[peak_idx as usize]
                    } else {
                        0.0
                    };
                    let h = peak * bar_h * 0.4;
                    let x = bar_rect.min.x + pix_in_visible;
                    let i0 = mesh.vertices.len() as u32;
                    mesh.vertices.push(Vertex { pos: egui::pos2(x, center_y - h), uv: WHITE_UV, color });
                    mesh.vertices.push(Vertex { pos: egui::pos2(x + bar_pixel_w, center_y - h), uv: WHITE_UV, color });
                    mesh.vertices.push(Vertex { pos: egui::pos2(x + bar_pixel_w, center_y + h), uv: WHITE_UV, color });
                    mesh.vertices.push(Vertex { pos: egui::pos2(x, center_y + h), uv: WHITE_UV, color });
                    mesh.indices.extend_from_slice(&[i0, i0 + 1, i0 + 2, i0, i0 + 2, i0 + 3]);
                }
                painter.add(egui::Shape::Mesh(mesh));
            }
        } else if wf.extracting {
            // Show "loading" state
            painter.text(
                bar_rect.center(), egui::Align2::CENTER_CENTER,
                "Loading...", egui::FontId::proportional(9.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 100));
        } else {
            // Not started yet - show placeholder bars
            draw_placeholder_waveform(painter, bar_rect);
        }
    } else {
        // No waveform object yet - show placeholder
        draw_placeholder_waveform(painter, bar_rect);
    }

    // Selection border
    if selected {
        painter.rect_stroke(bar_rect.expand(1.0), Rounding::same(5.0), Stroke::new(2.0, COL_SELECTED));
    }

    // Label
    if bar_rect.width() > 40.0 {
        painter.text(
            egui::pos2(bar_rect.min.x + 4.0, bar_rect.min.y + 4.0),
            egui::Align2::LEFT_TOP, label,
            egui::FontId::proportional(9.0), Color32::WHITE);
    }

    // Interaction.
    //
    // Same stable-id + press-origin strategy as `draw_clip` — see the
    // comment there for the rationale. Audio clips support edge-trim
    // (left = adjust t_in + source_start, right = adjust t_out) plus
    // whole-clip move, mirroring video-clip behaviour.
    let id = clip_id;
    let sense = if locked { Sense::hover() } else { Sense::click_and_drag() };
    let resp = ui.interact(bar_rect, id, sense);

    let hover_pos = ui.input(|i| i.pointer.hover_pos());
    let near_left_edge = hover_pos.map(|p| (p.x - bar_rect.min.x).abs() < 5.0).unwrap_or(false);
    let near_right_edge = hover_pos.map(|p| (p.x - bar_rect.max.x).abs() < 5.0).unwrap_or(false);

    if resp.hovered() && !locked {
        if near_left_edge || near_right_edge {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    // Trim affordance bars on the audio clip's edges.
    if !locked {
        draw_clip_trim_handles(painter, bar_rect, near_left_edge, near_right_edge);
    }

    if resp.clicked() { return Some(clip_start); }

    let mode_id = id.with("drag_mode");
    let origin_id = id.with("press_origin_x");
    let original_start_id = id.with("original_start");

    if resp.drag_started() && !locked {
        let press_x = ui
            .input(|i| i.pointer.press_origin())
            .map(|p| p.x)
            .unwrap_or(bar_rect.center().x);
        let mode = if (press_x - bar_rect.min.x).abs() < 6.0 {
            ClipDragMode::TrimLeft
        } else if (press_x - bar_rect.max.x).abs() < 6.0 {
            ClipDragMode::TrimRight
        } else {
            ClipDragMode::Move
        };
        ui.data_mut(|d| {
            d.insert_temp(mode_id, mode);
            d.insert_temp(origin_id, press_x);
            d.insert_temp(original_start_id, clip_start);
        });
    }

    if resp.dragged() && !locked {
        let mode: Option<ClipDragMode> = ui.data(|d| d.get_temp(mode_id));
        let press_x: Option<f32> = ui.data(|d| d.get_temp(origin_id));
        let original_start: Option<f32> = ui.data(|d| d.get_temp(original_start_id));
        let cur_x = ui
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
            .map(|p| p.x);

        if let (Some(mode), Some(px), Some(os), Some(cx)) =
            (mode, press_x, original_start, cur_x)
        {
            let total_dt = (cx - px) / pps;
            return match mode {
                ClipDragMode::TrimLeft => Some(f32::INFINITY),
                ClipDragMode::TrimRight => Some(f32::NEG_INFINITY),
                ClipDragMode::Move => Some(-(os + total_dt)),
            };
        }
        return Some(clip_start);
    }

    None
}


/// Draw placeholder waveform bars for audio clips that haven't been analyzed yet.
fn draw_placeholder_waveform(painter: &egui::Painter, bar_rect: egui::Rect) {
    let bar_w = bar_rect.width();
    let bar_h = bar_rect.height();
    let center_y = bar_rect.center().y;
    let num_bars = ((bar_w / 4.0) as usize).max(3).min(50);
    for i in 0..num_bars {
        let x = bar_rect.min.x + (i as f32 / num_bars as f32) * bar_w + 2.0;
        let h = bar_h * 0.15 * (1.0 + ((i as f32 * 0.7).sin() * 0.5));
        painter.line_segment(
            [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
            Stroke::new(1.5, Color32::from_rgba_premultiplied(180, 220, 220, 80)),
        );
    }
}


// ─── PER-PARAM KEYFRAME ROWS (timeline expansion for selected layer) ─

/// Pixels per animated param row in the timeline expansion area, before
/// vertical zoom is applied.
const PARAM_ROW_BASE: f32 = 14.0;

/// Animated-param ids on the layer that owns a track, ordered for
/// stable on-screen rendering. Returns empty when no animatable layer
/// is selected on the given track.
fn selected_layer_animated_params(
    state: &EditorState,
    track_idx: usize,
) -> Option<(Selection, Vec<String>)> {
    let video_tracks: Vec<usize> = state.video_track_indices();
    let default_overlay_track = if video_tracks.len() >= 2 {
        video_tracks[1]
    } else {
        video_tracks.first().copied().unwrap_or(0)
    };

    match state.selection {
        Selection::Actor(ai) => {
            let assigned = state
                .actor_track_assignments
                .get(&ai)
                .copied()
                .unwrap_or_else(|| video_tracks.first().copied().unwrap_or(0));
            if assigned != track_idx {
                return None;
            }
            let a = state.scene.actors.get(ai)?;
            let params = ordered_animated(&a.animated_params);
            Some((state.selection, params))
        }
        Selection::Overlay(oi) => {
            let assigned = state
                .overlay_track_assignments
                .get(&oi)
                .copied()
                .unwrap_or(default_overlay_track);
            if assigned != track_idx {
                return None;
            }
            let ap = match state.scene.overlays.get(oi)? {
                Overlay::Text(t) => &t.animated_params,
                Overlay::Image(im) => &im.animated_params,
                Overlay::Video(v) => &v.animated_params,
            };
            let params = ordered_animated(ap);
            Some((state.selection, params))
        }
        _ => None,
    }
}

/// Stable ordering for the param rows so the visual layout doesn't
/// shuffle as the user toggles params on/off.
fn ordered_animated(set: &std::collections::BTreeSet<String>) -> Vec<String> {
    use memstroy_core::param_ids::*;
    let mut out = Vec::with_capacity(set.len());
    for known in [POS_X, POS_Y, SCALE, SCALE_Y, ROTATION, OPACITY, FLIP_X, FLIP_Y] {
        if set.contains(known) {
            out.push(known.to_string());
        }
    }
    // Then any remaining unknown / future ids in BTreeSet order.
    for id in set.iter() {
        if !out.iter().any(|s| s == id) {
            out.push(id.clone());
        }
    }
    out
}

/// Extra height to add to a track row so the per-param keyframe rows of
/// the currently-selected layer fit underneath the clip bar.
fn selected_layer_expansion(state: &EditorState, track_idx: usize, v_zoom: f32) -> f32 {
    let Some((_, params)) = selected_layer_animated_params(state, track_idx) else {
        return 0.0;
    };
    if params.is_empty() {
        return 0.0;
    }
    (params.len() as f32) * PARAM_ROW_BASE * v_zoom + 4.0
}

/// Animated-param ids on the render frame, ordered for stable on-
/// screen rendering. Returns `None` when the render frame is NOT the
/// current selection — the per-param keyframe rows only appear under
/// the dedicated render-frame row when the user has it selected, so
/// non-RF-related layers never reserve vertical space for them.
fn render_frame_animated_params(state: &EditorState) -> Option<Vec<String>> {
    if state.selection != Selection::RenderFrame
        && !state.canvas_selection.contains(&Selection::RenderFrame)
    {
        return None;
    }
    let params = ordered_animated(&state.scene.render_frame.animated_params);
    if params.is_empty() {
        return None;
    }
    Some(params)
}

/// Vertical pixels needed for the render-frame row's per-param
/// keyframe strip, mirroring `selected_layer_expansion` for regular
/// tracks. Returns 0 when the render frame is unselected or has no
/// animated parameters, so the layout stays unchanged in the common
/// case.
fn render_frame_expansion(state: &EditorState, v_zoom: f32) -> f32 {
    let Some(params) = render_frame_animated_params(state) else {
        return 0.0;
    };
    (params.len() as f32) * PARAM_ROW_BASE * v_zoom + 4.0
}

// ─── MASK PARAM ROWS (above the clip) ────────────────────────────────
//
// The mask system treats each animated `Mask` / `ColorKey` parameter
// as its own diamond row, painted ABOVE the clip bar so the user
// reads them as "mask edits to the clip" — visually distinct from
// the transform rows below. The data model: every entry in
// `effect.param_kfs` whose key is also in `effect.animated_params`
// becomes a row with one diamond per keyframe time.

/// One diamond-row's worth of mask-related animation data attached
/// to the currently-selected layer.
#[derive(Clone)]
struct MaskParamRow {
    /// Index of the effect inside the layer's `effects` vec — used
    /// for the row's gutter colour key (multiple effects on the
    /// same layer get distinct colours so the user can tell them
    /// apart at a glance).
    effect_idx: usize,
    /// Param key inside the effect (e.g. "rect_left", "p0").
    param_key: String,
    /// Pre-formatted gutter label — "Mask · Left", "Color key · Blend"…
    label: String,
    /// Per-kf `(local_t, scene_t)` pairs. `local_t` matches the
    /// underlying `Keyframe.t`; `scene_t` is what the timeline ruler
    /// uses for its X math (= `local_t + t_in` for overlays,
    /// `= local_t` for actors).
    times: Vec<(f32, f32)>,
}

/// Walk the selected layer's effect stack and collect a `MaskParamRow`
/// for every animated parameter on every `Mask` / `ColorKey` entry.
/// Returns `None` when the selected layer doesn't live on
/// `track_idx`, or there is no selectable layer at all.
fn selected_layer_mask_param_rows(
    state: &EditorState,
    track_idx: usize,
) -> Option<(Selection, Vec<MaskParamRow>)> {
    let video_tracks: Vec<usize> = state.video_track_indices();
    let default_overlay_track = if video_tracks.len() >= 2 {
        video_tracks[1]
    } else {
        video_tracks.first().copied().unwrap_or(0)
    };

    let (effects, t_in): (&Vec<memstroy_core::Effect>, f32) = match state.selection {
        Selection::Actor(ai) => {
            let assigned = state
                .actor_track_assignments
                .get(&ai)
                .copied()
                .unwrap_or_else(|| video_tracks.first().copied().unwrap_or(0));
            if assigned != track_idx {
                return None;
            }
            let a = state.scene.actors.get(ai)?;
            (&a.effects, 0.0)
        }
        Selection::Overlay(oi) => {
            let assigned = state
                .overlay_track_assignments
                .get(&oi)
                .copied()
                .unwrap_or(default_overlay_track);
            if assigned != track_idx {
                return None;
            }
            let ov = state.scene.overlays.get(oi)?;
            match ov {
                Overlay::Text(t) => (&t.effects, t.t_in),
                Overlay::Image(im) => (&im.effects, im.t_in),
                Overlay::Video(v) => (&v.effects, v.t_in),
            }
        }
        _ => return None,
    };

    let mut rows: Vec<MaskParamRow> = Vec::new();
    for (ei, eff) in effects.iter().enumerate() {
        if !matches!(
            eff.kind,
            memstroy_core::EffectKind::Mask { .. } | memstroy_core::EffectKind::ColorKey { .. }
        ) {
            continue;
        }
        // Sort keys for stable ordering across frames so the rows
        // don't shuffle as the user toggles diamonds.
        let mut keys: Vec<String> = eff.animated_params.iter().cloned().collect();
        keys.sort();
        for key in keys {
            let kfs = match eff.param_kfs.get(&key) {
                Some(v) if !v.is_empty() => v,
                _ => continue,
            };
            let times: Vec<(f32, f32)> =
                kfs.iter().map(|kf| (kf.t, t_in + kf.t)).collect();
            let label = mask_row_label(&eff.kind, &key);
            rows.push(MaskParamRow {
                effect_idx: ei,
                param_key: key,
                label,
                times,
            });
        }
    }

    Some((state.selection, rows))
}

/// Pretty-print a mask param key into a gutter label. Falls back to
/// the raw key for unknown variants so the row stays readable.
fn mask_row_label(kind: &memstroy_core::EffectKind, key: &str) -> String {
    use memstroy_core::EffectKind as K;
    let prefix = match kind {
        K::Mask { .. } => "Mask",
        K::ColorKey { .. } => "Color key",
        _ => "Effect",
    };
    let suffix: &str = match key {
        "p0" if matches!(kind, K::Mask { .. }) => "Feather",
        "rect_left" => "Left",
        "rect_top" => "Top",
        "rect_right" => "Right",
        "rect_bottom" => "Bottom",
        "ellipse_cx" => "Center X",
        "ellipse_cy" => "Center Y",
        "ellipse_rx" => "Radius X",
        "ellipse_ry" => "Radius Y",
        "p0" => "Similarity",
        "p1" => "Blend",
        "p2" => "Spill",
        "intensity" => "Intensity",
        other => other,
    };
    format!("{prefix} · {suffix}")
}

/// Vertical space the mask-row strip needs above the clip on a track.
/// Returns 0.0 when the selected layer has no animated mask params,
/// so the existing layout is unchanged in the common case.
fn selected_layer_mask_above_height(
    state: &EditorState,
    track_idx: usize,
    v_zoom: f32,
) -> f32 {
    let Some((_, rows)) = selected_layer_mask_param_rows(state, track_idx) else {
        return 0.0;
    };
    if rows.is_empty() {
        return 0.0;
    }
    (rows.len() as f32) * PARAM_ROW_BASE * v_zoom + 4.0
}

/// Sample-and-extract the keyframe times for a given (layer, param) so
/// the timeline can render a diamond per kf without needing access to
/// the typed layout. We currently use the same `Vec<Keyframe<…>>` for
/// every param of the layer, so the times are shared — the per-param
/// row only differs in label / colour.
///
/// Currently unused: the inline kf strips inside the inspector took
/// over this responsibility. Kept around so the timeline-track-row
/// keyframe markers can be re-introduced without rediscovering the
/// per-layer time list math.
#[allow(dead_code)]
fn keyframe_times_for_layer(state: &EditorState, sel: Selection) -> Vec<f32> {
    match sel {
        Selection::Actor(ai) => state
            .scene
            .actors
            .get(ai)
            .map(|a| a.layout.iter().map(|kf| kf.t).collect())
            .unwrap_or_default(),
        Selection::Overlay(oi) => state
            .scene
            .overlays
            .get(oi)
            .map(|ov| match ov {
                Overlay::Text(t) => t.layout.iter().map(|kf| kf.t).collect(),
                Overlay::Image(im) => im.layout.iter().map(|kf| kf.t).collect(),
                Overlay::Video(v) => v.layout.iter().map(|kf| kf.t).collect(),
            })
            .unwrap_or_default(),
        Selection::RenderFrame => state
            .scene
            .render_frame
            .layout
            .iter()
            .map(|kf| kf.t)
            .collect(),
        _ => Vec::new(),
    }
}

/// For each animatable transform parameter on the selected layer,
/// return the list of `(local_t, scene_t)` keyframe pairs at which
/// **that specific parameter actually changes value** relative to the
/// previous kf in the same layer. The first kf of the layer is always
/// included (it sets the initial value). Layers with fewer than two
/// kfs return empty lists per parameter — there is nothing to animate
/// to display.
///
/// Heuristic, not schema-based: the layout vec carries one
/// `ActorState` / `OverlayState` per kf, so when the user edits only
/// `opacity` at the playhead the new kf still inherits the eased
/// values for every other field. Comparing each kf to its predecessor
/// per-field reconstructs which fields the user *meant* to author.
/// This avoids a schema migration while still giving the user the
/// "5 scale changes vs. 2 opacity changes" view they asked for.
fn compute_param_change_points(
    state: &EditorState,
    sel: Selection,
) -> std::collections::BTreeMap<String, Vec<(f32, f32)>> {
    use memstroy_core::param_ids as p;
    let mut out: std::collections::BTreeMap<String, Vec<(f32, f32)>> =
        std::collections::BTreeMap::new();
    const EPS: f32 = 1.0e-4;

    fn changed_actor<F>(layout: &[Keyframe<ActorState>], get: F) -> Vec<f32>
    where
        F: Fn(&ActorState) -> f32,
    {
        if layout.len() < 2 { return Vec::new(); }
        let mut times: Vec<f32> = Vec::new();
        // Always include the first kf — that's the initial value for
        // the parameter and the user wants to see/click it.
        times.push(layout[0].t);
        for win in layout.windows(2) {
            let (prev, cur) = (&win[0].value, &win[1].value);
            if (get(cur) - get(prev)).abs() > EPS {
                times.push(win[1].t);
            }
        }
        times
    }

    fn changed_overlay<F>(layout: &[Keyframe<OverlayState>], get: F) -> Vec<f32>
    where
        F: Fn(&OverlayState) -> f32,
    {
        if layout.len() < 2 { return Vec::new(); }
        let mut times: Vec<f32> = Vec::new();
        times.push(layout[0].t);
        for win in layout.windows(2) {
            let (prev, cur) = (&win[0].value, &win[1].value);
            if (get(cur) - get(prev)).abs() > EPS {
                times.push(win[1].t);
            }
        }
        times
    }

    let pairs = |times: Vec<f32>, sel: Selection| -> Vec<(f32, f32)> {
        times
            .into_iter()
            .map(|local_t| (local_t, kf_time_to_scene_time(state, sel, local_t)))
            .collect()
    };

    match sel {
        Selection::Actor(ai) => {
            if let Some(a) = state.scene.actors.get(ai) {
                out.insert(p::POS_X.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.pos[0]), sel));
                out.insert(p::POS_Y.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.pos[1]), sel));
                out.insert(p::SCALE.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.scale), sel));
                out.insert(p::SCALE_Y.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.scale_y), sel));
                out.insert(p::ROTATION.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.rotation_deg), sel));
                out.insert(p::OPACITY.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.opacity), sel));
                out.insert(p::FLIP_X.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.flip_x_anim), sel));
                out.insert(p::FLIP_Y.to_string(),
                    pairs(changed_actor(&a.layout, |s| s.flip_y_anim), sel));
            }
        }
        Selection::Overlay(oi) => {
            if let Some(ov) = state.scene.overlays.get(oi) {
                let layout: &[Keyframe<OverlayState>] = match ov {
                    Overlay::Text(t) => &t.layout,
                    Overlay::Image(im) => &im.layout,
                    Overlay::Video(v) => &v.layout,
                };
                out.insert(p::POS_X.to_string(),
                    pairs(changed_overlay(layout, |s| s.pos[0]), sel));
                out.insert(p::POS_Y.to_string(),
                    pairs(changed_overlay(layout, |s| s.pos[1]), sel));
                out.insert(p::SCALE.to_string(),
                    pairs(changed_overlay(layout, |s| s.scale), sel));
                out.insert(p::SCALE_Y.to_string(),
                    pairs(changed_overlay(layout, |s| s.scale_y), sel));
                out.insert(p::ROTATION.to_string(),
                    pairs(changed_overlay(layout, |s| s.rotation_deg), sel));
                out.insert(p::OPACITY.to_string(),
                    pairs(changed_overlay(layout, |s| s.opacity), sel));
                out.insert(p::FLIP_X.to_string(),
                    pairs(changed_overlay(layout, |s| s.flip_x_anim), sel));
                out.insert(p::FLIP_Y.to_string(),
                    pairs(changed_overlay(layout, |s| s.flip_y_anim), sel));
            }
        }
        _ => {
            // RenderFrame is handled below; any other selection (None,
            // Background, Audio, Camera) has no per-param strip in the
            // timeline expansion area, so it falls through with an
            // empty map.
        }
    }

    // Render frame keyframes are scene-time anchored (no t_in/t_out)
    // and the four animatable parameters mirror the inspector.
    if matches!(sel, Selection::RenderFrame) {
        let rf = &state.scene.render_frame;
        let layout: &[Keyframe<RenderFrameState>] = &rf.layout;

        fn changed_rf<F>(layout: &[Keyframe<RenderFrameState>], get: F) -> Vec<f32>
        where
            F: Fn(&RenderFrameState) -> f32,
        {
            const EPS: f32 = 1.0e-4;
            if layout.len() < 2 { return Vec::new(); }
            let mut times: Vec<f32> = Vec::new();
            times.push(layout[0].t);
            for win in layout.windows(2) {
                let (prev, cur) = (&win[0].value, &win[1].value);
                if (get(cur) - get(prev)).abs() > EPS {
                    times.push(win[1].t);
                }
            }
            times
        }

        out.insert(p::POS_X.to_string(),
            pairs(changed_rf(layout, |s| s.pos.x), sel));
        out.insert(p::POS_Y.to_string(),
            pairs(changed_rf(layout, |s| s.pos.y), sel));
        // The render frame uses `zoom` instead of `scale` internally;
        // expose it under the SCALE param id so existing inspector /
        // timeline pipes treat it like the actor / overlay scale row.
        out.insert(p::SCALE.to_string(),
            pairs(changed_rf(layout, |s| s.zoom), sel));
        out.insert(p::ROTATION.to_string(),
            pairs(changed_rf(layout, |s| s.rotation_deg), sel));
    }
    out
}

/// Translate kf times (LOCAL for overlays / scene-time for actors) to
/// scene-time absolute coords used by the timeline ruler.
fn kf_time_to_scene_time(state: &EditorState, sel: Selection, kf_t: f32) -> f32 {
    match sel {
        Selection::Actor(_) => kf_t,
        Selection::Overlay(oi) => {
            let t_in = state.scene.overlays.get(oi).map(|ov| match ov {
                Overlay::Text(t) => t.t_in,
                Overlay::Image(im) => im.t_in,
                Overlay::Video(v) => v.t_in,
            }).unwrap_or(0.0);
            t_in + kf_t
        }
        _ => kf_t,
    }
}

/// Inverse of `kf_time_to_scene_time` — used when seeking the playhead
/// from a clicked diamond. Actor kfs are stored in scene time so the
/// playhead just becomes `kf_t`; overlay kfs are local to the clip.
#[allow(dead_code)]
fn kf_scene_time(state: &EditorState, sel: Selection, kf_t: f32) -> f32 {
    kf_time_to_scene_time(state, sel, kf_t)
}

/// Apply `easing` to the keyframe at time `kf_t` in the given layer's
/// layout. Used by the per-param row context menu and the multi-edit
/// path (when a batch of kfs is selected, the easing is broadcast to
/// every entry). Time matching uses an ε of 1ms to absorb fp drift.
fn apply_easing_to_layer_kf(
    state: &mut EditorState,
    layer: &crate::kf_anim::SelectedLayer,
    kf_t: f32,
    easing: memstroy_core::Easing,
) {
    let eps = 1.0e-3;
    match layer {
        crate::kf_anim::SelectedLayer::Actor(ai) => {
            if let Some(a) = state.scene.actors.get_mut(*ai) {
                if let Some(kf) = a.layout.iter_mut().find(|k| (k.t - kf_t).abs() < eps) {
                    kf.easing = easing;
                }
            }
        }
        crate::kf_anim::SelectedLayer::Overlay(oi) => {
            if let Some(ov) = state.scene.overlays.get_mut(*oi) {
                let layout: &mut Vec<Keyframe<OverlayState>> = match ov {
                    Overlay::Text(t) => &mut t.layout,
                    Overlay::Image(im) => &mut im.layout,
                    Overlay::Video(v) => &mut v.layout,
                };
                if let Some(kf) = layout.iter_mut().find(|k| (k.t - kf_t).abs() < eps) {
                    kf.easing = easing;
                }
            }
        }
        crate::kf_anim::SelectedLayer::RenderFrame => {
            if let Some(kf) = state
                .scene
                .render_frame
                .layout
                .iter_mut()
                .find(|k| (k.t - kf_t).abs() < eps)
            {
                kf.easing = easing;
            }
        }
    }
}

/// Remove every keyframe currently flagged in `state.selected_keyframes`
/// from the matching layer's layout vec, then clear the selection. Used
/// by the Delete / Backspace key handler in the timeline. Time
/// comparison uses an ε of 1ms to absorb floating-point drift between
/// the stored kf time and the click hit-test value.
fn delete_selected_keyframes(state: &mut EditorState) {
    if state.selected_keyframes.is_empty() {
        return;
    }
    let to_delete = state.selected_keyframes.clone();
    let eps = 1.0e-3;
    let mut removed = 0usize;

    // Group deletions by layer so each layout vec is mutated once.
    let mut actor_kfs: std::collections::HashMap<usize, Vec<f32>> =
        std::collections::HashMap::new();
    let mut overlay_kfs: std::collections::HashMap<usize, Vec<f32>> =
        std::collections::HashMap::new();
    for kf in to_delete {
        match kf.layer {
            crate::kf_anim::SelectedLayer::Actor(ai) => {
                actor_kfs.entry(ai).or_default().push(kf.t);
            }
            crate::kf_anim::SelectedLayer::Overlay(oi) => {
                overlay_kfs.entry(oi).or_default().push(kf.t);
            }
            crate::kf_anim::SelectedLayer::RenderFrame => {
                state
                    .scene
                    .render_frame
                    .layout
                    .retain(|kfx| (kfx.t - kf.t).abs() > eps);
                removed += 1;
            }
        }
    }
    for (ai, ts) in actor_kfs {
        if let Some(a) = state.scene.actors.get_mut(ai) {
            let before = a.layout.len();
            a.layout
                .retain(|kfx| !ts.iter().any(|t| (kfx.t - t).abs() < eps));
            removed += before - a.layout.len();
        }
    }
    for (oi, ts) in overlay_kfs {
        if let Some(ov) = state.scene.overlays.get_mut(oi) {
            let layout: &mut Vec<Keyframe<OverlayState>> = match ov {
                Overlay::Text(t) => &mut t.layout,
                Overlay::Image(im) => &mut im.layout,
                Overlay::Video(v) => &mut v.layout,
            };
            let before = layout.len();
            layout
                .retain(|kfx| !ts.iter().any(|t| (kfx.t - t).abs() < eps));
            removed += before - layout.len();
        }
    }

    state.selected_keyframes.clear();
    if removed > 0 {
        state.status = format!("Deleted {} keyframe(s)", removed);
    }
}

/// Render the per-param keyframe rows in the bottom expansion area of
/// the selected layer's track row. Each row has a label + a horizontal
/// line of diamond markers — one per keyframe time on the layer.
///
/// `kf_pairs` carries `(local_t, scene_t)` pairs: `local_t` is what's
/// stored in the layer's layout vec (clip-local for overlays, scene-time
/// for actors); `scene_t` is the scene-time used to position the diamond
/// on the timeline ruler.
///
/// Diamonds are clickable (seek + select). Returns the click hits the
/// user produced this frame so the caller can fold them into the
/// keyframe-selection list without nested borrows.
#[allow(clippy::too_many_arguments)]
fn draw_mask_param_kf_rows(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    sel_layer_label: &crate::kf_anim::SelectedLayer,
    rows: &[MaskParamRow],
    strip_top: f32,
    strip_height: f32,
    track_left: f32,
    track_right: f32,
    pps: f32,
    scroll: f32,
    state_playhead: f32,
    selected_kfs: &[crate::kf_anim::SelectedKeyframe],
    clip_x_start: f32,
    clip_x_end: f32,
) -> ParamRowOutcome {
    let row_h = (strip_height / rows.len().max(1) as f32).max(10.0);
    let mut outcome = ParamRowOutcome::default();

    let strip_x_start = clip_x_start.max(track_left);
    let strip_x_end = clip_x_end.min(track_right);
    let strip_visible = strip_x_end - strip_x_start > 1.0;

    // Bottom separator so the mask strip reads as a "section above"
    // rather than blending into the clip's top edge.
    painter.line_segment(
        [
            egui::pos2(track_left, strip_top + strip_height),
            egui::pos2(track_right, strip_top + strip_height),
        ],
        Stroke::new(1.0, Color32::from_rgb(140, 90, 50)),
    );
    // Faint top edge as a visual cap.
    painter.line_segment(
        [
            egui::pos2(track_left, strip_top),
            egui::pos2(track_right, strip_top),
        ],
        Stroke::new(1.0, Color32::from_rgba_premultiplied(140, 90, 50, 120)),
    );

    let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
    let shift_held = ui.input(|i| i.modifiers.shift);

    for (ri, row) in rows.iter().enumerate() {
        let row_top = strip_top + (ri as f32) * row_h;
        let row_bot = row_top + row_h;

        // Distinct warm tint per effect index so the user can tell
        // multiple masks on the same layer apart at a glance. The
        // colour rotates through 5 hues so even crowded layers stay
        // readable.
        let palette = [
            Color32::from_rgba_premultiplied(255, 200, 80, 36),
            Color32::from_rgba_premultiplied(255, 130, 130, 36),
            Color32::from_rgba_premultiplied(160, 200, 255, 36),
            Color32::from_rgba_premultiplied(170, 230, 170, 36),
            Color32::from_rgba_premultiplied(220, 170, 255, 36),
        ];
        let bg = palette[row.effect_idx % palette.len()];
        if strip_visible {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(strip_x_start, row_top),
                    egui::pos2(strip_x_end, row_bot),
                ),
                Rounding::ZERO,
                bg,
            );
        }

        // Gutter label.
        painter.text(
            egui::pos2(track_left + 4.0, row_top + row_h * 0.5),
            egui::Align2::LEFT_CENTER,
            &row.label,
            egui::FontId::proportional(9.0),
            Color32::from_rgb(255, 220, 180),
        );

        // One diamond per keyframe time, clickable.
        let half = 4.5_f32;
        let synth_param_id = format!("fx_{}_{}", row.effect_idx, row.param_key);
        for &(local_t, scene_t) in row.times.iter() {
            let x = (scene_t - scroll) * pps + track_left;
            if !strip_visible {
                continue;
            }
            if x < strip_x_start - half || x > strip_x_end + half {
                continue;
            }
            let cy = row_top + row_h * 0.5;

            let is_selected = selected_kfs.iter().any(|sk| {
                &sk.layer == sel_layer_label
                    && sk.param_id == synth_param_id
                    && (sk.t - local_t).abs() < 1.0e-3
            });
            let at_playhead =
                (scene_t - state_playhead).abs() < (0.5 / pps.max(1.0)).max(0.005);

            // Slightly different fill so mask diamonds read as
            // "different concept" from transform-row diamonds below.
            let fill = if is_selected {
                Color32::from_rgb(255, 220, 80)
            } else if at_playhead {
                Color32::from_rgb(255, 160, 80)
            } else {
                Color32::from_rgb(255, 200, 120)
            };
            let pts = vec![
                egui::pos2(x, cy - half),
                egui::pos2(x + half, cy),
                egui::pos2(x, cy + half),
                egui::pos2(x - half, cy),
            ];
            painter.add(egui::Shape::convex_polygon(
                pts,
                fill,
                Stroke::new(1.0, Color32::from_rgb(20, 20, 30)),
            ));
            if at_playhead && !is_selected {
                painter.circle_stroke(
                    egui::pos2(x, cy),
                    half + 2.5,
                    Stroke::new(1.0, Color32::from_rgb(255, 160, 80)),
                );
            }

            let hit = egui::Rect::from_center_size(
                egui::pos2(x, cy),
                Vec2::new(half * 2.5, row_h.min(20.0)),
            );
            let id = ui.id().with((
                "mask_kf",
                sel_layer_label,
                &synth_param_id,
                local_t.to_bits(),
            ));
            let r = ui.interact(hit, id, Sense::click_and_drag());
            if r.clicked() {
                outcome.click_hits.push(ParamRowClick {
                    param_id: synth_param_id.clone(),
                    t: local_t,
                    extend: ctrl_held || shift_held,
                    seek_to: scene_t,
                });
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }

    outcome
}

/// Diamonds are clickable (seek + select). Returns the click hits the
/// user produced this frame so the caller can fold them into the
/// keyframe-selection list without nested borrows.
#[allow(clippy::too_many_arguments)]
fn draw_param_kf_rows(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    sel_layer_label: &crate::kf_anim::SelectedLayer,
    params: &[String],
    // Per-parameter list of `(local_t, scene_t)` pairs. The caller
    // computes change-points for each parameter from the layer's
    // typed layout so opacity-only edits don't pollute the scale row
    // (and vice versa). When a key is missing, that param's row is
    // drawn empty.
    param_kfs: &std::collections::BTreeMap<String, Vec<(f32, f32)>>,
    expansion_top: f32,
    expansion_height: f32,
    track_left: f32,
    track_right: f32,
    pps: f32,
    scroll: f32,
    state_playhead: f32,
    selected_kfs: &[crate::kf_anim::SelectedKeyframe],
    // On-screen X range that the OWNING CLIP occupies on the timeline.
    // The per-param rows are drawn ONLY within this range so they read
    // as visually attached to the clip — moving or trimming the clip
    // crops the keyframe area along with it. Labels still anchor at
    // `track_left` so the user can read them in the layer gutter.
    clip_x_start: f32,
    clip_x_end: f32,
) -> ParamRowOutcome {
    let row_h = (expansion_height / params.len().max(1) as f32).max(10.0);
    let mut outcome = ParamRowOutcome::default();

    // Visible attached strip — clamped to the timeline viewport AND to
    // the clip's bar. When the clip is scrolled out of view this comes
    // out empty and the function essentially no-ops.
    let strip_x_start = clip_x_start.max(track_left);
    let strip_x_end = clip_x_end.min(track_right);
    let strip_visible = strip_x_end - strip_x_start > 1.0;

    // Track separator above the param rows so they read as a separate
    // sub-section of the layer's row. Drawn over the full row width so
    // the gutter (label area) keeps its baseline.
    painter.line_segment(
        [
            egui::pos2(track_left, expansion_top),
            egui::pos2(track_right, expansion_top),
        ],
        Stroke::new(1.0, Color32::from_rgb(70, 70, 90)),
    );

    let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
    let shift_held = ui.input(|i| i.modifiers.shift);

    for (pi, param_id) in params.iter().enumerate() {
        let row_top = expansion_top + (pi as f32) * row_h;
        let row_bot = row_top + row_h;

        // Alternating background tint per param row — drawn ONLY inside
        // the clip's strip so the row visually attaches to the clip.
        let bg = if pi % 2 == 0 {
            Color32::from_rgba_premultiplied(255, 255, 255, 6)
        } else {
            Color32::from_rgba_premultiplied(255, 255, 255, 12)
        };
        if strip_visible {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(strip_x_start, row_top),
                    egui::pos2(strip_x_end, row_bot),
                ),
                Rounding::ZERO,
                bg,
            );
            // Strip side accents so the boundary with the timeline is
            // visible and the user reads the rows as "clip-bound".
            painter.line_segment(
                [
                    egui::pos2(strip_x_start, row_top),
                    egui::pos2(strip_x_start, row_bot),
                ],
                Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 255, 255, 30)),
            );
            painter.line_segment(
                [
                    egui::pos2(strip_x_end, row_top),
                    egui::pos2(strip_x_end, row_bot),
                ],
                Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 255, 255, 30)),
            );
        }

        // Label on the far-left of the row.
        let label = memstroy_core::param_ids::label(param_id);
        painter.text(
            egui::pos2(track_left + 4.0, row_top + row_h * 0.5),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(9.0),
            COL_TEXT_DIM,
        );

        // Diamond per kf, hit-tested. Diamonds outside the clip's strip
        // are skipped so the per-param row only carries keyframes that
        // belong to the clip's visible range. The list of kfs is
        // **per-parameter** (caller pre-computes change-points against
        // the layer's typed layout), so a kf that only changes opacity
        // never appears in the scale row, etc.
        let empty: Vec<(f32, f32)> = Vec::new();
        let kfs_for_param: &[(f32, f32)] = param_kfs
            .get(param_id)
            .map(|v| v.as_slice())
            .unwrap_or(&empty);
        let half = 4.5_f32;
        for &(local_t, scene_t) in kfs_for_param {
            let x = (scene_t - scroll) * pps + track_left;
            if !strip_visible {
                continue;
            }
            if x < strip_x_start - half || x > strip_x_end + half {
                continue;
            }
            let cy = row_top + row_h * 0.5;

            let is_selected = selected_kfs.iter().any(|sk| {
                &sk.layer == sel_layer_label
                    && sk.param_id == *param_id
                    && (sk.t - local_t).abs() < 1.0e-3
            });
            let at_playhead =
                (scene_t - state_playhead).abs() < (0.5 / pps.max(1.0)).max(0.005);

            let fill = if is_selected {
                Color32::from_rgb(255, 230, 80)
            } else if at_playhead {
                Color32::from_rgb(255, 180, 80)
            } else {
                Color32::from_rgb(160, 200, 255)
            };
            let pts = vec![
                egui::pos2(x, cy - half),
                egui::pos2(x + half, cy),
                egui::pos2(x, cy + half),
                egui::pos2(x - half, cy),
            ];
            painter.add(egui::Shape::convex_polygon(
                pts,
                fill,
                Stroke::new(1.0, Color32::from_rgb(20, 20, 30)),
            ));
            if at_playhead && !is_selected {
                painter.circle_stroke(
                    egui::pos2(x, cy),
                    half + 2.5,
                    Stroke::new(1.0, Color32::from_rgb(255, 180, 80)),
                );
            }

            // Click hit-test (small rect around the diamond).
            let hit = egui::Rect::from_center_size(
                egui::pos2(x, cy),
                Vec2::new(half * 2.5, row_h.min(20.0)),
            );
            let id = ui.id().with(("param_kf", sel_layer_label, param_id, local_t.to_bits()));
            let r = ui.interact(hit, id, Sense::click_and_drag());
            if r.clicked() {
                outcome.click_hits.push(ParamRowClick {
                    param_id: param_id.clone(),
                    t: local_t,
                    extend: ctrl_held || shift_held,
                    seek_to: scene_t,
                });
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // Right-click context menu — pick the interpolation curve
            // ("transition") used to ease INTO this keyframe. When the
            // user has multiple keyframes already selected (Ctrl+click
            // before right-click) the choice applies to every selected
            // kf so they can re-flag a whole batch of "make these
            // step-holds" or "linearise these" in one shot.
            let kf_in_selection = is_selected;
            r.context_menu(|ui| {
                ui.label(
                    egui::RichText::new(crate::i18n::t("Transition into kf"))
                        .size(10.0)
                        .strong(),
                );
                ui.separator();
                let easings: [(memstroy_core::Easing, &str); 6] = [
                    (memstroy_core::Easing::Linear, "Linear"),
                    (memstroy_core::Easing::Step, "Step (instant)"),
                    (memstroy_core::Easing::EaseIn, "Ease in"),
                    (memstroy_core::Easing::EaseOut, "Ease out"),
                    (memstroy_core::Easing::EaseInOut, "Ease in/out"),
                    (memstroy_core::Easing::Cubic, "Cubic"),
                ];
                for (e, label) in easings {
                    if ui
                        .selectable_label(false, crate::i18n::t(label))
                        .clicked()
                    {
                        outcome.easing_changes.push(ParamRowEasingChange {
                            t: local_t,
                            easing: e,
                            apply_to_selection: kf_in_selection,
                        });
                        ui.close_menu();
                    }
                }
            });
        }
    }

    outcome
}

/// Output of `draw_param_kf_rows` consumed by the caller to update
/// state without nested borrow scopes.
#[derive(Default)]
struct ParamRowOutcome {
    click_hits: Vec<ParamRowClick>,
    /// Easing changes requested via the right-click menu on a kf
    /// diamond. Each entry carries the local kf time, the new easing
    /// to apply, and whether the change should be replicated to every
    /// currently-selected keyframe of the same layer (so the user can
    /// "make these 5 step-holds" in one shot).
    easing_changes: Vec<ParamRowEasingChange>,
}

struct ParamRowClick {
    param_id: String,
    /// kf time as stored in the layer (scene-time for actors; clip-local
    /// for overlays). Used together with `Selection` when removing the
    /// kf from the layout vector.
    t: f32,
    /// Whether Ctrl/Shift was held — extends the kf selection rather
    /// than replacing it.
    extend: bool,
    /// Resolved scene-time for the playhead seek.
    seek_to: f32,
}

struct ParamRowEasingChange {
    /// Local kf time (the row coordinate space) of the kf the user
    /// right-clicked on.
    t: f32,
    /// Replacement easing.
    easing: memstroy_core::Easing,
    /// When true the change is broadcast to every currently-selected
    /// keyframe of the layer (multi-edit). Otherwise it only applies
    /// to the kf at `t`.
    apply_to_selection: bool,
}


/// Draw small diamond markers on a clip bar, one per layout keyframe.
/// `kf_t_is_scene_time` controls whether `kf.t` is interpreted as the
/// final scene-time (true — actors) or as a clip-local offset that
/// should be added to `clip_start` (false — overlays).
#[allow(clippy::too_many_arguments)]
fn draw_keyframe_diamonds<T>(
    painter: &egui::Painter,
    content_rect: egui::Rect,
    clip_start: f32,
    clip_end: f32,
    layout: &[Keyframe<T>],
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    selected: bool,
    kf_t_is_scene_time: bool,
) {
    if layout.is_empty() { return; }
    // If there's only a single static keyframe, no need to draw anything —
    // the clip is non-animated.
    if layout.len() == 1 { return; }

    let bar_y_top = content_rect.min.y + 2.0;
    let bar_y_bot = content_rect.max.y - 2.0;
    let cy = (bar_y_top + bar_y_bot) * 0.5;
    let half = 4.0_f32;
    let fill = if selected {
        Color32::from_rgb(255, 230, 80)
    } else {
        Color32::from_rgb(200, 200, 255)
    };
    let stroke = Color32::from_rgb(20, 20, 30);

    for kf in layout {
        let abs_t = if kf_t_is_scene_time { kf.t } else { clip_start + kf.t };
        if abs_t < clip_start - 0.001 || abs_t > clip_end + 0.001 { continue; }
        let x = (abs_t - scroll) * pps + track_left;
        if x < track_left - half || x > track_right + half { continue; }

        // Diamond shape (45-degree rotated square).
        let pts = vec![
            egui::pos2(x, cy - half),
            egui::pos2(x + half, cy),
            egui::pos2(x, cy + half),
            egui::pos2(x - half, cy),
        ];
        painter.add(egui::Shape::convex_polygon(pts, fill, Stroke::new(1.0, stroke)));
    }
}


/// Draw a small triangle marker + faded gradient overlay representing a
/// non-`Cut` transition at either edge of an actor clip on the timeline.
#[allow(clippy::too_many_arguments)]
fn draw_transition_indicators(
    painter: &egui::Painter,
    content_rect: egui::Rect,
    clip_start: f32,
    clip_end: f32,
    trans_in: Transition,
    trans_out: Transition,
    trans_dur: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
) {
    if trans_dur <= 0.0 {
        return;
    }

    let x_start = (clip_start - scroll) * pps + track_left;
    let x_end = (clip_end - scroll) * pps + track_left;
    if x_end < track_left || x_start > track_right {
        return;
    }

    let band_w = (trans_dur * pps).clamp(2.0, (clip_end - clip_start) * pps * 0.5);

    // In-edge band: from x_start..x_start+band_w
    if !matches!(trans_in, Transition::Cut) {
        let bx0 = x_start.max(track_left);
        let bx1 = (x_start + band_w).min(track_right);
        if bx1 > bx0 + 1.0 {
            let band = egui::Rect::from_min_max(
                egui::pos2(bx0, content_rect.min.y + 2.0),
                egui::pos2(bx1, content_rect.max.y - 2.0),
            );
            painter.rect_filled(
                band,
                Rounding::same(2.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 50),
            );
            // Triangle marker pointing right at the in-edge
            let tri = 4.0;
            let ty = content_rect.min.y + 4.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(bx0, ty),
                    egui::pos2(bx0 + tri * 1.4, ty + tri),
                    egui::pos2(bx0, ty + tri * 2.0),
                ],
                Color32::from_rgb(255, 220, 120),
                Stroke::NONE,
            ));
        }
    }

    // Out-edge band: from x_end-band_w..x_end
    if !matches!(trans_out, Transition::Cut) {
        let bx0 = (x_end - band_w).max(track_left);
        let bx1 = x_end.min(track_right);
        if bx1 > bx0 + 1.0 {
            let band = egui::Rect::from_min_max(
                egui::pos2(bx0, content_rect.min.y + 2.0),
                egui::pos2(bx1, content_rect.max.y - 2.0),
            );
            painter.rect_filled(
                band,
                Rounding::same(2.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 50),
            );
            let tri = 4.0;
            let ty = content_rect.min.y + 4.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(bx1, ty),
                    egui::pos2(bx1 - tri * 1.4, ty + tri),
                    egui::pos2(bx1, ty + tri * 2.0),
                ],
                Color32::from_rgb(255, 220, 120),
                Stroke::NONE,
            ));
        }
    }
}


/// Draw ruler time marks with proper spacing.
fn draw_ruler_marks(painter: &egui::Painter, rect: egui::Rect, scroll: f32, pps: f32, duration: f32) {
    // Choose step based on zoom level
    let step = choose_ruler_step_pps(pps);
    let start_t = (scroll / step).floor() * step;
    let end_t = scroll + rect.width() / pps;

    let mut t = start_t;
    while t <= end_t.min(duration) {
        let x = rect.min.x + (t - scroll) * pps;
        if x >= rect.min.x && x <= rect.max.x {
            let is_major = (t / step).round() as i32 % 5 == 0 || step >= duration;
            let tick_h = if is_major { rect.height() * 0.7 } else { rect.height() * 0.35 };
            painter.line_segment(
                [egui::pos2(x, rect.max.y - tick_h), egui::pos2(x, rect.max.y)],
                Stroke::new(1.0, Color32::from_rgb(80, 80, 100)));
            if is_major {
                painter.text(egui::pos2(x, rect.min.y + 2.0), egui::Align2::CENTER_TOP,
                    format_time(t), egui::FontId::proportional(9.0), COL_TEXT_DIM);
            }
        }
        t += step;
    }
}

/// Convert time to X pixel position. Returns None if off-screen.
fn time_to_x(t: f32, scroll: f32, pps: f32, track_left: f32, track_right: f32) -> Option<f32> {
    let x = track_left + (t - scroll) * pps;
    if x >= track_left && x <= track_right { Some(x) } else { None }
}

/// Convert X pixel position back to time.
fn x_to_time(x: f32, scroll: f32, pps: f32, track_left: f32) -> f32 {
    scroll + (x - track_left) / pps
}

/// Choose ruler step based on pixels-per-second zoom.
fn choose_ruler_step_pps(pps: f32) -> f32 {
    // Target ~60-100px between major marks (every 5 steps)
    let target_px = 80.0;
    let step_secs = target_px / pps / 5.0;
    // Round to nice values
    if step_secs < 0.02 { 0.01 }
    else if step_secs < 0.05 { 0.02 }
    else if step_secs < 0.1 { 0.05 }
    else if step_secs < 0.2 { 0.1 }
    else if step_secs < 0.5 { 0.2 }
    else if step_secs < 1.0 { 0.5 }
    else if step_secs < 2.0 { 1.0 }
    else if step_secs < 5.0 { 2.0 }
    else if step_secs < 10.0 { 5.0 }
    else { 10.0 }
}

fn format_time(t: f32) -> String {
    let mins = (t / 60.0).floor() as u32;
    let secs = t % 60.0;
    if mins > 0 { format!("{}:{:05.2}", mins, secs) }
    else { format!("{:.2}s", secs) }
}


// ─── HELPERS ─────────────────────────────────────────────────────────

fn color_edit_u8(ui: &mut egui::Ui, c: &mut [u8; 3]) -> bool {
    let mut rgb = [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0];
    if ui.color_edit_button_rgb(&mut rgb).changed() {
        c[0] = (rgb[0] * 255.0).round() as u8;
        c[1] = (rgb[1] * 255.0).round() as u8;
        c[2] = (rgb[2] * 255.0).round() as u8;
        true
    } else {
        false
    }
}

fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}...", s.chars().take(n).collect::<String>()) }
}

fn clean_clip_text(raw: &str) -> String {
    let noise: &[&str] = &["Имба", "Топ", "Херня", "имба", "топ", "херня", "—", "\u{2014}"];
    let mut s: String = raw.chars().filter(|c| {
        c.is_ascii() || ('\u{0400}'..='\u{04FF}').contains(c) || *c == ' ' || *c == '.' || *c == ',' || *c == '!' || *c == '?'
    }).collect();
    for n in noise { s = s.replace(n, ""); }
    while s.contains("  ") { s = s.replace("  ", " "); }
    s.trim_matches(|c: char| c == ' ' || c == '-').to_string()
}

pub fn add_actor_from_clip(state: &mut EditorState, path: &PathBuf) {
    let t = state.playhead;
    add_actor_from_clip_at_time(state, path, t);
}

/// Load chroma sidecar for `path`, falling back to default when absent.
fn load_chroma_for_clip(path: &PathBuf) -> ChromaKeyParams {
    ChromaKeyParams::load_for_clip(path).unwrap_or_default()
}

/// Push an `AudioTrack` matching `actor` so the embedded audio shows up as
/// its own row on the audio lanes. Returns the new index. The audio track is
/// linked to its parent actor via `parent_actor` so we can keep them in sync
/// (move / trim / delete together).
fn push_audio_track_for_actor(state: &mut EditorState, actor_id: &str, source: &PathBuf,
                              t_in: f32, t_out: Option<f32>, source_start: f32) -> usize {
    let id = format!("{}_audio", actor_id);
    state.scene.audio.push(AudioTrack {
        id,
        source: source.clone(),
        t_in,
        t_out,
        source_start,
        parent_actor: Some(actor_id.to_string()),
        ..Default::default()
    });
    state.scene.audio.len() - 1
}

/// Find the index of the audio track bound to a given actor id, if any.
fn find_audio_for_actor(state: &EditorState, actor_id: &str) -> Option<usize> {
    state.scene.audio.iter().position(|au| {
        au.parent_actor.as_deref() == Some(actor_id)
    })
}

/// Sync the bound audio track's timing with its parent actor. Call this after
/// every actor t_in/t_out/source_start change so the audio follows the clip.
pub(crate) fn sync_audio_to_actor(state: &mut EditorState, actor_idx: usize) {
    if actor_idx >= state.scene.actors.len() { return; }
    let (actor_id, t_in, t_out, source_start) = {
        let a = &state.scene.actors[actor_idx];
        (a.id.clone(), a.t_in.unwrap_or(0.0), a.t_out, a.source_start)
    };
    if let Some(au_idx) = find_audio_for_actor(state, &actor_id) {
        let au = &mut state.scene.audio[au_idx];
        au.t_in = t_in;
        au.t_out = t_out;
        au.source_start = source_start;
    }
}

/// Remove every audio track bound to the actor at `actor_idx` (called by the
/// actor delete path so we never leave orphaned audio).
pub(crate) fn remove_audio_bound_to_actor(state: &mut EditorState, actor_id: &str)
    -> Vec<usize>
{
    let mut removed = Vec::new();
    let mut i = 0;
    while i < state.scene.audio.len() {
        if state.scene.audio[i].parent_actor.as_deref() == Some(actor_id) {
            state.scene.audio.remove(i);
            // Same-index side-tables (waveforms, extract results) and
            // the {idx -> track} assignment map all need to slide down
            // by one or the next-actor's audio renders / draws under
            // the wrong row. Mirrors the bookkeeping `apply_remove`
            // does for timeline-driven removals.
            if i < state.audio_waveforms.len() {
                state.audio_waveforms.remove(i);
            }
            shift_assignments_after_remove(&mut state.audio_track_assignments, i);
            removed.push(i);
        } else {
            i += 1;
        }
    }
    removed
}

/// Auto-attach a skeleton template for `path` if a sidecar exists and we
/// haven't already loaded it into the scene.
fn ensure_skeleton_template_for_clip(state: &mut EditorState, path: &PathBuf) {
    let already = state.scene.skeleton_templates.iter()
        .any(|t| t.source_clip == *path);
    if already { return; }
    if let Some(template) = SkeletonTemplate::load_for_clip(path) {
        state.scene.skeleton_templates.push(template);
    }
}

/// Add a styled text overlay at the playhead and select it.
/// Returns the index of the new overlay.
pub fn add_text_overlay(state: &mut EditorState) -> usize {
    let counter = state.scene.overlays.len() + 1;
    let id = format!("text_{}", counter);
    let t_in = state.playhead;
    let t_out = (t_in + 3.0).min(state.scene.output.duration.max(t_in + 0.1));

    let max_z = state.scene.overlays.iter().filter_map(|o| match o {
        Overlay::Text(t) => Some(t.z_index),
        _ => None,
    }).max().unwrap_or(99);

    let style = TextStyle {
        font: "DejaVuSans".into(),
        font_size: 96.0,
        color: [255, 255, 255],
        box_color: Some([0, 0, 0]),
        box_padding: 24.0,
        box_extra_left: 0.0,
        box_extra_right: 0.0,
        bold: true,
        italic: false,
        outline: Some([0, 0, 0]),
        outline_width: 4.0,
        align: TextAlign::Center,
        box_kind: TextBoxKind::Solid,
        box_corner_radius: 12.0,
        box_opacity: 0.85,
        box_gradient_end: None,
        box_outline_color: None,
        box_outline_width: 0.0,
    };

    let overlay = Overlay::Text(TextOverlay {
        id: id.clone(),
        text: "Text".into(),
        t_in,
        t_out,
        style,
        layout: vec![Keyframe::new(0.0, OverlayState {
            pos: [0.5, 0.5],
            scale: 1.0,
            scale_y: 1.0,
            rotation_deg: 0.0,
            opacity: 1.0,
            flip_x_anim: 1.0,
            flip_y_anim: 1.0,
        })],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        z_index: max_z + 1,
        behind_actors: false,
        effects: Vec::new(),
        animated_params: Default::default(),
    });

    state.scene.overlays.push(overlay);
    let idx = state.scene.overlays.len() - 1;

    // Always create the new text on its own freshly-inserted layer. The
    // new layer goes at the top of the video stack (so the text is on top
    // of everything by default); the user can drag it to a different
    // layer afterwards.
    let new_track = state.insert_video_track_at_top();
    state.overlay_track_assignments.insert(idx, new_track);

    state.selection = Selection::Overlay(idx);
    state.status = format!("Added text: {} (new layer)", id);
    idx
}

/// Add an actor from a clip at a specific time (used by drag-to-track).
pub(crate) fn add_actor_from_clip_at_time(state: &mut EditorState, path: &PathBuf, t: f32) {
    let counter = state.scene.actors.len() + 1;
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("{}_{}", s, counter))
        .unwrap_or_else(|| format!("actor_{}", counter));

    let clip_duration = probe_video_duration(path);
    // The timeline auto-grows to fit content (see timeline()'s auto-length
    // pass), so don't clamp the right edge to the current `output.duration`.
    let t_in = t.max(0.0);
    let t_out = t_in + clip_duration.max(0.1);

    // Per-clip chroma settings live next to the source file (`<clip>.chroma.json`).
    // This is independent of the project, so re-using the same Mellstroy clip
    // in another scene starts pre-tuned.
    let chroma = load_chroma_for_clip(path);
    // Likewise, auto-attach a skeleton template if one was saved for this clip.
    ensure_skeleton_template_for_clip(state, path);

    let actor = Actor {
        id: id.clone(),
        source: path.clone(),
        anchors: None,
        chroma_key: chroma,
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: Some(t_in),
        t_out: Some(t_out),
        source_start: 0.0,
        loop_source: false,
        flip_horizontal: false,
        attachments: Vec::new(),
        skeleton_attachments: Vec::new(),
        modifiers: Vec::new(),
        visible: true,
        color_correction: ColorCorrection::default(),
        transition_in: Transition::Cut,
        transition_out: Transition::Cut,
        transition_duration: 0.3,
        effects: Vec::new(),
        speed: 1.0,
        animated_params: Default::default(),
    };
    state.scene.actors.push(actor);
    let new_actor_idx = state.scene.actors.len() - 1;

    // Also push an AudioTrack referencing the same source so the embedded
    // audio appears as its own row on the audio lanes (and gains a waveform,
    // volume slider, etc. on the inspector).
    push_audio_track_for_actor(state, &id, path, t_in, Some(t_out), 0.0);

    state.selection = Selection::Actor(new_actor_idx);
    state.status = format!("Dropped actor: {}", id);
}

/// Probe a media file and return its duration in seconds (5.0s fallback
/// when ffprobe isn't available or the file can't be opened).
fn probe_video_duration(path: &PathBuf) -> f32 {
    let ffprobe = {
        let mut p = memstroy_render::ffmpeg_binary();
        p.set_file_name("ffprobe");
        if !p.exists() { PathBuf::from("ffprobe") } else { p }
    };
    let mut cmd = std::process::Command::new(&ffprobe);
    cmd.args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path);
    match memstroy_render::hide_console_std(&mut cmd).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().unwrap_or(5.0),
        Err(_) => 5.0,
    }
}


/// Add an actor from a clip dropped onto the free canvas. Inserts an entry
/// in `canvas_layouts` so the clip is centred at the supplied world
/// position immediately, instead of falling back to the legacy normalised
/// layout (which would otherwise put it at the render frame centre).
pub(crate) fn add_actor_from_clip_at_canvas(
    state: &mut EditorState,
    path: &PathBuf,
    world_pos: [f32; 2],
) {
    let t = state.playhead;
    add_actor_from_clip_at_time(state, path, t);
    let new_actor_idx = match state.scene.actors.len().checked_sub(1) {
        Some(i) => i,
        None => return,
    };
    let actor_id = state.scene.actors[new_actor_idx].id.clone();

    // Drop the actor onto the first EMPTY video lane at the current
    // playhead — falls back to inserting a brand-new top lane when
    // every existing lane already has something on it. This makes
    // canvas drops always create a clean layer instead of stacking
    // on top of whatever's on V1.
    let assigned = state.pick_or_create_empty_video_lane_at(t);
    state.actor_track_assignments.insert(new_actor_idx, assigned);

    use memstroy_core::{CanvasLayout, Keyframe, CanvasTransform, WorldPos};
    let canvas_kf = Keyframe::new(
        0.0,
        CanvasTransform {
            pos: WorldPos { x: world_pos[0], y: world_pos[1] },
            ..Default::default()
        },
    );
    state.scene.canvas_layouts.push(CanvasLayout {
        element_id: actor_id,
        keyframes: vec![canvas_kf],
    });

    state.selection = Selection::Actor(new_actor_idx);
    state.status = format!(
        "Dropped on canvas at ({:.0}, {:.0})",
        world_pos[0], world_pos[1]
    );
}



/// Make sure every audio row that has a `parent_actor` lives on the audio
/// lane that mirrors its parent's video lane (actor on the i-th video lane
/// → bound audio on the i-th audio lane). Standalone audio rows keep the
/// user's own placement. New audio lanes are appended on demand if there
/// aren't enough to cover the deepest video lane in use.
pub(crate) fn sync_bound_audio_lanes(state: &mut EditorState) {
    let videos = state.video_track_indices();
    if videos.is_empty() {
        return;
    }

    // Resolve each actor's video-lane index, by id.
    let mut actor_track_for_id: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (ai, a) in state.scene.actors.iter().enumerate() {
        let track = state
            .actor_track_assignments
            .get(&ai)
            .copied()
            .unwrap_or_else(|| videos.first().copied().unwrap_or(0));
        actor_track_for_id.insert(a.id.clone(), track);
    }

    // Compute (audio_idx, vt_pos) targets for every bound audio row.
    let mut targets: Vec<(usize, usize)> = Vec::new();
    for (au_idx, au) in state.scene.audio.iter().enumerate() {
        let Some(parent_id) = au.parent_actor.as_deref() else { continue };
        let Some(&video_track) = actor_track_for_id.get(parent_id) else { continue };
        let Some(vt_pos) = videos.iter().position(|&t| t == video_track) else { continue };
        targets.push((au_idx, vt_pos));
    }

    // Append new audio lanes if any actor's video position exceeds the
    // current audio-lane count.
    if let Some(&max_pos) = targets.iter().map(|(_, p)| p).max() {
        while state.audio_track_indices().len() <= max_pos {
            state.add_audio_track();
        }
    }

    let audios = state.audio_track_indices();
    for (au_idx, vt_pos) in targets {
        if let Some(&target) = audios.get(vt_pos) {
            state.audio_track_assignments.insert(au_idx, target);
        }
    }
}
