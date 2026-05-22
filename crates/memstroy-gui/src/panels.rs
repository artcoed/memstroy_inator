//! UI panels — Premiere Pro-style timeline, modern inspector, drag&drop.

use std::path::PathBuf;

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

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
pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    // Capture the panel rect so the OS-level file-drop handler in `app.rs`
    // can route drops onto this region into the Videos / Images / Sounds
    // sub-folder rather than dropping straight onto the timeline.
    state.library_panel_rect = Some(ui.max_rect());

    // Header
    ui.horizontal(|ui| {
        ui.label(RichText::new("Library").size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(RichText::new("Refresh").color(Color32::WHITE).size(12.0))
                .fill(Color32::from_rgb(80, 50, 180))
                .rounding(Rounding::same(6.0));
            if ui.add_enabled(!state.refreshing, btn).clicked() {
                state.status = "__REFRESH_REQUESTED__".into();
            }
        });
    });
    ui.add_space(4.0);

    // Tab bar — Clips / Videos / Sounds / Images / Particles / Shared.
    // Sticky across sessions via `state.library_tab`.
    ui.horizontal_wrapped(|ui| {
        let tabs = [
            (LibraryTab::Clips,    "\u{1F3AC} Clips"),
            (LibraryTab::Videos,   "\u{1F4FD} Videos"),
            (LibraryTab::Sounds,   "\u{1F50A} Sounds"),
            (LibraryTab::Images,   "\u{1F5BC} Images"),
            (LibraryTab::Particles, "\u{2728} Particles"),
            (LibraryTab::Shared,   "\u{2601} Shared"),
        ];
        for (tab, label) in tabs {
            if ui.selectable_label(state.library_tab == tab, label).clicked() {
                state.library_tab = tab;
            }
        }
    });
    ui.add_space(4.0);

    ui.add(
        egui::TextEdit::singleline(&mut state.library_search)
            .hint_text("Search...")
            .desired_width(ui.available_width()),
    );
    ui.add_space(2.0);

    let hint_text = match state.library_tab {
        LibraryTab::Clips => "Drag a clip onto the canvas or timeline. Drop video files here from your file manager to import them into the Videos tab.",
        LibraryTab::Videos => "User-imported videos. Drop a video file from your file manager into this panel to add it. Drag a row onto the canvas or timeline to spawn an actor.",
        LibraryTab::Sounds => "Drop a sound onto the timeline to add it as an audio track. Drop audio files from your file manager here to import.",
        LibraryTab::Images => "Drag a sticker onto the canvas to add it as an image overlay. Drop image files from your file manager here to import.",
        LibraryTab::Particles => "Drag a particle onto the canvas — it spawns with spin + pulse modifiers.",
        LibraryTab::Shared => "Lazily-fetched assets served by an external memstroy-assets-server. Search, preview, download — or pull a Telegram channel to populate it.",
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
        LibraryTab::Shared => {
            // Use the runtime handle that App stashed at startup.
            // Cloning a Handle is cheap; we clone here so the borrow
            // checker is happy while the closure mutably borrows state.
            if let Some(handle) = state.tokio_handle.clone() {
                crate::shared_library::shared_library_panel(ui, state, &handle);
            } else {
                ui.label(
                    RichText::new("No tokio runtime available — restart the editor.")
                        .color(COL_TEXT_DIM),
                );
            }
        }
    }
}

/// Render a "Local | Global" split inside the library panel. Both
/// sections live inside a single vertical column with a draggable 6 px
/// handle in the middle that adjusts `state.library_split` (0.05..=0.95).
/// The same split ratio is reused across every tab so the user's choice
/// persists when they hop between Clips / Videos / Sounds / etc.
///
/// Each section's header is rendered automatically — callers only need
/// to push the rows / cards into the closure.
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
        RichText::new("Local (your imports)")
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
        RichText::new("Global (built-in / browser)")
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

/// Render the original Mellstroy clip browser content (split out so the
/// new tabs can render their own variants of the list).
fn library_clips_tab(ui: &mut egui::Ui, state: &mut EditorState) {
    library_split_panel(
        ui,
        state,
        "library_split_clips",
        |_ui, _state| {
            // Local clips section: currently empty placeholder. Future
            // work could let users import their own video clips into
            // a "local clips" pool that lives outside `assets/videos/`.
        },
        |ui, state| {
            let search_lower = state.library_search.to_lowercase();
            let clip_count = state.library.mellstroy_clips.len();
            ui.label(
                RichText::new(format!("Clips ({})", clip_count))
                    .size(12.0)
                    .strong()
                    .color(Color32::from_rgb(220, 130, 50)),
            );
            ui.add_space(2.0);

            egui::ScrollArea::vertical()
                .id_source("library_clips_scroll")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if state.library.mellstroy_clips.is_empty() {
                        ui.label(
                            RichText::new("No clips. Hit Refresh to download.")
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
        },
    );
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

    let split_id = format!("library_split_{}", title.to_lowercase());

    library_split_panel(
        ui,
        state,
        &split_id,
        |ui, state| {
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
                        "Empty. Drop files into:\n  {}\nthen click Refresh.",
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
            let rows: Vec<crate::state::LibraryAsset> = assets
                .iter()
                .filter(|a| {
                    search_lower.is_empty()
                        || a.label.to_lowercase().contains(&search_lower)
                        || a.id.to_lowercase().contains(&search_lower)
                })
                .cloned()
                .collect();

            egui::ScrollArea::vertical()
                .id_source(scroll_id)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    for asset in &rows {
                        library_asset_card(ui, state, asset, kind, title_color);
                    }
                });
        },
        |_ui, _state| {
            // Global section is intentionally empty for non-Clips tabs
            // for now — the slot exists so the splitter layout matches
            // every tab and so future work can wire a remote/built-in
            // library here without changing the UI shape.
        },
    );
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
            state.scene.audio.push(memstroy_core::AudioTrack {
                id: asset.id.clone(),
                source: asset.path.clone(),
                t_in: t,
                t_out: None,
                source_start: 0.0,
                volume: 1.0,
                speed: 1.0,
                parent_actor: None,
                volume_kfs: Vec::new(),
                speed_kfs: Vec::new(),
                animated_params: Default::default(),
            });
            state.selection = Selection::Audio(state.scene.audio.len() - 1);
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
            });
            state.scene.overlays.push(overlay);
            state.selection = Selection::Overlay(state.scene.overlays.len() - 1);
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
            });
            state.scene.overlays.push(overlay);
            state.selection = Selection::Overlay(state.scene.overlays.len() - 1);
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
                let (rect, _) = ui.allocate_exact_size(thumb_size, Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 55));
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                    format!("{}", clip.id), egui::FontId::proportional(11.0), COL_TEXT_DIM);
            }

            // Vertical text column claims the rest of the available width
            // so that even short labels don't shrink the card.
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), thumb_size.y),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.set_min_width(ui.available_width());
                    let desc = clean_clip_text(&clip.description);
                    let display = if desc.is_empty() { format!("Clip #{}", clip.id) } else { desc };
                    ui.label(RichText::new(format!("#{}", clip.id)).size(9.0)
                        .color(Color32::from_rgb(120, 100, 200)));
                    ui.add(
                        egui::Label::new(RichText::new(display).size(11.0).color(COL_TEXT))
                            .truncate(),
                    );
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
        format!("Clip #{}", clip.id)
    } else if desc.chars().count() > 28 {
        format!("#{}  {}\u{2026}", clip.id, desc.chars().take(26).collect::<String>())
    } else {
        format!("#{}  {}", clip.id, desc)
    }
}


// ─── INSPECTOR ───────────────────────────────────────────────────────

pub fn inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Inspector").size(16.0).strong());
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
    // etc.). `auto_shrink([false; 2])` keeps the scrollbar pinned to the
    // panel's right edge regardless of content height.
    egui::ScrollArea::vertical()
        .id_source("inspector_scroll")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            inspector_body(ui, state);
        });
}

fn inspector_body(ui: &mut egui::Ui, state: &mut EditorState) {
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
            ui.label("Camera editing coming soon.");
        }
        Selection::RenderFrame => {
            inspector_render_frame(ui, state);
        }
    }
}


fn inspector_nothing(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.add_space(20.0);
    ui.label(RichText::new("Select a clip on the timeline").italics().color(COL_TEXT_DIM).size(13.0));
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);

    // Output settings — fixed 1080x1920 9:16 short format.
    // FPS and duration are intentionally not user-editable here: FPS is
    // pinned by the format and the scene's duration grows automatically
    // to fit whatever is on the timeline.
    ui.label(RichText::new("Output").size(14.0).strong().color(Color32::from_rgb(100, 200, 255)));
    ui.add_space(4.0);
    ui.label(RichText::new("1080x1920 (9:16)").size(12.0).color(COL_TEXT_DIM));
    ui.add_space(4.0);

    let _ = state; // currently unused beyond the labels
}

fn inspector_actor(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let actor_count = state.scene.actors.len();
    let cache_count = state.frame_caches.len();

    // Header with name (delete button removed — use Delete/Backspace shortcut
    // or right-click on the timeline clip instead).
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Actor: {}", state.scene.actors[i].id))
            .strong().size(14.0).color(COL_CLIP_ACTOR));
    });
    ui.add_space(2.0);
    ui.label(RichText::new(
        state.scene.actors[i].source.file_name().and_then(|s| s.to_str()).unwrap_or("(source)")
    ).size(10.0).color(COL_TEXT_DIM));
    ui.add_space(6.0);

    // Tab bar: Transform | Effects
    ui.horizontal(|ui| {
        if ui.selectable_label(state.inspector_tab == 0, "Transform").clicked() { state.inspector_tab = 0; }
        if ui.selectable_label(state.inspector_tab == 2, "Effects").clicked() { state.inspector_tab = 2; }
    });
    ui.separator();
    ui.add_space(4.0);

    match state.inspector_tab {
        0 => inspector_actor_transform(ui, state, i),
        2 => inspector_actor_effects(ui, state, i, actor_count, cache_count),
        _ => inspector_actor_transform(ui, state, i),
    }
}


fn inspector_actor_transform(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    use crate::kf_anim;
    use memstroy_core::param_ids;

    let playhead = state.playhead;
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Position & Scale").size(12.0).strong());
    ui.add_space(4.0);

    // Sample the eased current value at the playhead — this is read-only
    // and never mutates `layout`. The widget below is bound to a temp
    // copy, and only `.changed()` triggers a write through `kf_anim`.
    let cur = kf_anim::sample_actor(&a.layout, playhead);

    let kf_count = a.layout.len();
    if kf_count <= 1 {
        ui.label(
            RichText::new("Static value (no keyframes yet)")
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

    // ── Position X / Y ──
    let mut new_x = cur.pos[0];
    let mut new_y = cur.pos[1];
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::POS_X, ("act_pos_x", i));
        ui.label(param_label(highlight.is_active(param_ids::POS_X), "X:"));
        let r = ui.add(egui::DragValue::new(&mut new_x).range(-2.0..=3.0).speed(0.005));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::POS_X, false,
                |s| s.pos[0] = new_x);
        }
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::POS_Y, ("act_pos_y", i));
        ui.label(param_label(highlight.is_active(param_ids::POS_Y), "Y:"));
        let r = ui.add(egui::DragValue::new(&mut new_y).range(-2.0..=3.0).speed(0.005));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::POS_Y, false,
                |s| s.pos[1] = new_y);
        }
    });

    // ── Scale ──
    let mut new_scale = cur.scale;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::SCALE, ("act_scale", i));
        ui.label(param_label(highlight.is_active(param_ids::SCALE), "Scale:"));
        let r = ui.add(egui::Slider::new(&mut new_scale, 0.05..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::SCALE, false,
                |s| s.scale = new_scale);
        }
    });

    // ── Stretch Y ──
    let mut new_scale_y = cur.scale_y;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, &mut a.animated_params, param_ids::SCALE_Y, ("act_sy", i));
        ui.label(param_label(highlight.is_active(param_ids::SCALE_Y), "Stretch Y:"))
            .on_hover_text("Y-axis stretch on top of uniform scale (1.0 = proportional)");
        let r = ui.add(egui::Slider::new(&mut new_scale_y, 0.1..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_actor_param(
                &mut a.layout, &mut a.animated_params, playhead,
                param_ids::SCALE_Y, false,
                |s| s.scale_y = new_scale_y);
        }
        // Reset (\u{21BB}) button intentionally removed — set Stretch Y to 1.0 via the slider directly.
    });

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

    ui.add_space(8.0);
    ui.checkbox(&mut a.visible, "Visible");

    // Animation modifiers — wobble / shake / pulse / spin.
    ui.add_space(8.0);
    inspector_modifiers(ui, &mut a.modifiers, ("actor_mods", i));
}

/// Color a param label gold when its kf was just clicked from the timeline.
fn param_label(highlighted: bool, text: &str) -> RichText {
    if highlighted {
        RichText::new(text).size(11.0).strong()
            .color(Color32::from_rgb(255, 220, 80))
            .background_color(Color32::from_rgba_premultiplied(80, 60, 0, 80))
    } else {
        RichText::new(text).size(11.0)
    }
}

/// Inspector-side mirror of `ensure_actor_kf_at_playhead` from canvas_preview.rs.
/// **NOTE:** kept as a no-op stub for now (kf insertion was the source of
/// the infinite-keyframe bug during playback / timeline scrubbing). The
/// new authoring path is `kf_anim::write_actor_param`, which only runs
/// when the user actually edits a parameter. Left here so the existing
/// callers compile until they're migrated.
#[allow(dead_code)]
fn ensure_actor_kf_at_playhead_inspector(
    _layout: &mut Vec<Keyframe<ActorState>>,
    _t: f32,
) -> Option<usize> {
    None
}

#[allow(dead_code)]
fn ensure_overlay_kf_at_playhead_inspector(
    _layout: &mut Vec<Keyframe<OverlayState>>,
    _t: f32,
) -> Option<usize> {
    None
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
    let rad = (deg.to_radians() - std::f32::consts::FRAC_PI_2);
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
        RichText::new("Animation Modifiers").size(12.0).strong()
            .color(Color32::from_rgb(150, 200, 255)),
    )
    .id_source(("modifier_collapse", salt))
    .default_open(false)
    .show(ui, |ui| {
        if modifiers.is_empty() {
            ui.label(RichText::new(
                "No modifiers. Add one to perturb the animation \
                 (wobble/shake/pulse/spin).",
            ).size(10.0).color(COL_TEXT_DIM).italics());
        } else {
            let mut to_remove: Option<usize> = None;
            for (mi, m) in modifiers.iter_mut().enumerate() {
                let kind_label = m.kind_label();
                let header_color = match m.kind {
                    ModifierKind::Wobble { .. } => Color32::from_rgb(120, 200, 255),
                    ModifierKind::Shake { .. } => Color32::from_rgb(255, 160, 100),
                    ModifierKind::Pulse { .. } => Color32::from_rgb(255, 220, 100),
                    ModifierKind::Spin { .. } => Color32::from_rgb(180, 255, 150),
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
                                if ui.small_button("x").on_hover_text("Remove modifier").clicked() {
                                    to_remove = Some(mi);
                                }
                            });
                        });
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Range").size(10.0).color(COL_TEXT_DIM));
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
                                if ui.small_button("\u{221E}").on_hover_text("Always active").clicked() {
                                    m.t_end = f32::MAX;
                                }
                            }
                        });
                        ui.add_space(2.0);
                        match &mut m.kind {
                            ModifierKind::Wobble { freq_hz, amp_x, amp_y, amp_rot_deg, phase } => {
                                ui.add(egui::Slider::new(freq_hz, 0.1..=10.0).text("Freq Hz"));
                                ui.add(egui::Slider::new(amp_x, 0.0..=120.0).text("Amp X (px)"));
                                ui.add(egui::Slider::new(amp_y, 0.0..=120.0).text("Amp Y (px)"));
                                ui.add(egui::Slider::new(amp_rot_deg, 0.0..=45.0).text("Amp Rot \u{00B0}"));
                                ui.add(egui::Slider::new(phase, 0.0..=std::f32::consts::TAU).text("Phase"));
                            }
                            ModifierKind::Shake { freq_hz, amp_x, amp_y, seed } => {
                                ui.add(egui::Slider::new(freq_hz, 1.0..=40.0).text("Freq Hz"));
                                ui.add(egui::Slider::new(amp_x, 0.0..=80.0).text("Amp X (px)"));
                                ui.add(egui::Slider::new(amp_y, 0.0..=80.0).text("Amp Y (px)"));
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Seed").size(10.0));
                                    ui.add(egui::DragValue::new(seed).range(0..=u32::MAX).speed(1.0));
                                });
                            }
                            ModifierKind::Pulse { freq_hz, amp_scale } => {
                                ui.add(egui::Slider::new(freq_hz, 0.1..=10.0).text("Freq Hz"));
                                ui.add(egui::Slider::new(amp_scale, -0.5..=0.5).text("Amp Scale"));
                            }
                            ModifierKind::Spin { speed_dps } => {
                                ui.add(egui::Slider::new(speed_dps, -720.0..=720.0).text("Speed \u{00B0}/s"));
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
            if ui.button(RichText::new("+ Wobble").size(10.0)).on_hover_text(
                "Smooth sinusoidal sway"
            ).clicked() {
                modifiers.push(TrackModifier::wobble());
            }
            if ui.button(RichText::new("+ Shake").size(10.0)).on_hover_text(
                "High-frequency jitter"
            ).clicked() {
                modifiers.push(TrackModifier::shake());
            }
            if ui.button(RichText::new("+ Pulse").size(10.0)).on_hover_text(
                "Periodic scale breathing"
            ).clicked() {
                modifiers.push(TrackModifier::pulse());
            }
            if ui.button(RichText::new("+ Spin").size(10.0)).on_hover_text(
                "Continuous rotation"
            ).clicked() {
                modifiers.push(TrackModifier::spin());
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
                            // Master intensity with per-effect "Animated"
                            // toggle. The toggle marks `intensity` as
                            // animatable on this Effect; when ON future
                            // edits write into `eff.param_kfs["intensity"]`
                            // at the playhead. (Renderer wiring for
                            // animated effect params is incremental — see
                            // memstroy_core::Effect for the data model.)
                            ui.horizontal(|ui| {
                                crate::kf_anim::animated_toggle(
                                    ui,
                                    &mut eff.animated_params,
                                    "intensity",
                                    ("eff_int", ei),
                                );
                                ui.add(egui::Slider::new(&mut eff.intensity, 0.0..=1.0)
                                    .text("Intensity"));
                            });
                            ui.add_space(2.0);
                            inspector_effect_kind_params(ui, &mut eff.kind, salt, ei);
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
                let presets = memstroy_core::all_effect_presets();
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
fn inspector_effect_kind_params(
    ui: &mut egui::Ui,
    kind: &mut EffectKind,
    _salt: impl std::hash::Hash + Copy,
    _ei: usize,
) {
    use memstroy_core::EffectKind as K;
    match kind {
        K::Blur { radius } => { ui.add(egui::Slider::new(radius, 0.0..=80.0).text("Radius (px)")); }
        K::Sharpen { amount } => { ui.add(egui::Slider::new(amount, 0.0..=3.0).text("Amount")); }
        K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV
            | K::OldFilm | K::Vhs => {
            ui.label(RichText::new("No parameters.").size(10.0).color(COL_TEXT_DIM).italics());
        }
        K::HueShift { degrees } => { ui.add(egui::Slider::new(degrees, -180.0..=180.0).text("Hue \u{00B0}")); }
        K::Vignette { strength } => { ui.add(egui::Slider::new(strength, 0.0..=1.0).text("Strength")); }
        K::Pixelate { block_size } => { ui.add(egui::Slider::new(block_size, 2.0..=80.0).text("Block size (px)")); }
        K::Posterize { levels } => { ui.add(egui::Slider::new(levels, 2..=32).text("Levels")); }
        K::Glow { radius, intensity } => {
            ui.add(egui::Slider::new(radius, 0.0..=64.0).text("Radius (px)"));
            ui.add(egui::Slider::new(intensity, 0.0..=2.0).text("Intensity"));
        }
        K::Brightness { amount } => { ui.add(egui::Slider::new(amount, -1.0..=1.0).text("Amount")); }
        K::Contrast { amount } => { ui.add(egui::Slider::new(amount, -1.0..=1.0).text("Amount")); }
        K::Saturation { amount } => { ui.add(egui::Slider::new(amount, -1.0..=1.0).text("Amount")); }
        K::EdgeDetect { threshold } => { ui.add(egui::Slider::new(threshold, 0.0..=1.0).text("Threshold")); }
        K::ChromaticAberration { offset } => { ui.add(egui::Slider::new(offset, 0.0..=24.0).text("Offset (px)")); }
        K::Noise { amount } => { ui.add(egui::Slider::new(amount, 0.0..=1.0).text("Amount")); }
        K::Wave { amplitude, wavelength } => {
            ui.add(egui::Slider::new(amplitude, 0.0..=40.0).text("Amplitude (px)"));
            ui.add(egui::Slider::new(wavelength, 4.0..=400.0).text("Wavelength (px)"));
        }
        K::Glitch { strength } => { ui.add(egui::Slider::new(strength, 0.0..=1.0).text("Strength")); }
        K::Bloom { radius } => { ui.add(egui::Slider::new(radius, 0.0..=80.0).text("Radius (px)")); }
    }
}


fn inspector_actor_effects(ui: &mut egui::Ui, state: &mut EditorState, i: usize, _actor_count: usize, _cache_count: usize) {
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Chroma Key").size(12.0).strong().color(Color32::from_rgb(100, 255, 100)));
    ui.add_space(4.0);

    // Eyedropper
    let mut chroma_changed = false;
    ui.horizontal(|ui| {
        if state.eyedropper_active {
            ui.label(RichText::new("Click preview to pick color...").color(Color32::from_rgb(255, 200, 50)).size(11.0));
        } else if ui.button("Eyedropper").on_hover_text("Pick color from preview").clicked() {
            state.eyedropper_active = true;
        }
        ui.label("Key:");
        if color_edit_u8(ui, &mut a.chroma_key.key_color) {
            chroma_changed = true;
        }
    });

    ui.add_space(4.0);
    if ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text("Similarity")).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text("Blend")).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text("Spill")).changed() {
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
        RichText::new("Color Correction").size(12.0).strong().color(Color32::from_rgb(200, 180, 255))
    ).default_open(true).show(ui, |ui| {
        color_correction_inspector(ui, state, i);
    });

    ui.add_space(12.0);

    // Skeleton Attachments
    inspector_actor_skeleton_attachments(ui, state, i);

    ui.add_space(12.0);
    // Effect stack — generic post-process effects layered on top of CC.
    let a = &mut state.scene.actors[i];
    inspector_effect_stack(ui, &mut a.effects, ("actor_fx", i));
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
            .selectable_label(tab == CcTab::Basic, RichText::new("Basic").size(11.0))
            .clicked()
        {
            tab = CcTab::Basic;
        }
        if ui
            .selectable_label(tab == CcTab::Wheels, RichText::new("Wheels").size(11.0))
            .clicked()
        {
            tab = CcTab::Wheels;
        }
        if ui
            .selectable_label(tab == CcTab::Curves, RichText::new("Curves").size(11.0))
            .clicked()
        {
            tab = CcTab::Curves;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Reset all").clicked() {
                state.scene.actors[actor_idx].color_correction =
                    memstroy_core::ColorCorrection::default();
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(tab_id, tab));
    ui.add_space(4.0);

    let cc = &mut state.scene.actors[actor_idx].color_correction;
    match tab {
        CcTab::Basic => {
            ui.add(egui::Slider::new(&mut cc.brightness, -1.0..=1.0).text("Brightness"));
            ui.add(egui::Slider::new(&mut cc.contrast, 0.0..=3.0).text("Contrast"));
            ui.add(egui::Slider::new(&mut cc.saturation, 0.0..=3.0).text("Saturation"));
            ui.add(egui::Slider::new(&mut cc.temperature, -1.0..=1.0).text("Temperature"));
        }
        CcTab::Wheels => {
            // Lift wheel: neutral 0, range ±0.5
            color_wheel_widget(ui, "Lift",  &mut cc.lift,  [0.0; 3], 0.5, -0.5..=0.5);
            ui.add_space(6.0);
            color_wheel_widget(ui, "Gamma", &mut cc.gamma, [1.0; 3], 1.0,  0.2..=4.0);
            ui.add_space(6.0);
            color_wheel_widget(ui, "Gain",  &mut cc.gain,  [1.0; 3], 1.0,  0.0..=4.0);
        }
        CcTab::Curves => {
            curve_editor_widget(ui, "Master", &mut cc.curves.master, Color32::from_rgb(220, 220, 220));
            ui.add_space(4.0);
            curve_editor_widget(ui, "Red",    &mut cc.curves.red,    Color32::from_rgb(255, 100, 100));
            ui.add_space(4.0);
            curve_editor_widget(ui, "Green",  &mut cc.curves.green,  Color32::from_rgb(100, 220, 120));
            ui.add_space(4.0);
            curve_editor_widget(ui, "Blue",   &mut cc.curves.blue,   Color32::from_rgb(100, 160, 255));
            ui.add_space(2.0);
            ui.label(RichText::new("Click empty area: add point  •  Drag: move  •  Right-click: remove")
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
                        let xmin = points[idx - 1][0] + 0.001;
                        let xmax = points[idx + 1][0] - 0.001;
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
                 Open Tools \u{2192} Skeleton Constructor and save a \
                 sidecar next to the source file."
            ).size(10.0).italics().color(COL_TEXT_DIM));
            return;
        }

        // ── Drag sources: chips for every overlay and every OTHER actor.
        ui.label(RichText::new("Drag an element onto a point to attach:")
            .size(10.0).color(COL_TEXT_DIM));
        ui.horizontal_wrapped(|ui| {
            for oi in 0..state.scene.overlays.len() {
                let label = match &state.scene.overlays[oi] {
                    Overlay::Text(t) => format!("T:{}", ellipsis(&t.id, 12)),
                    Overlay::Image(im) => format!("I:{}", ellipsis(&im.id, 12)),
                    Overlay::Video(v) => format!("V:{}", ellipsis(&v.id, 12)),
                };
                let chip = element_drag_chip(
                    ui,
                    ("ovr_chip", oi),
                    &label,
                    Color32::from_rgb(80, 200, 120),
                );
                if chip.dragged() {
                    state.element_drag.source = Some(crate::state::AttachableElement::Overlay(oi));
                    state.element_drag.label = label.clone();
                    if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                        state.element_drag.pos = [p.x, p.y];
                    }
                }
            }
            for ai in 0..state.scene.actors.len() {
                if ai == i { continue; }
                let label = format!("A:{}", ellipsis(&state.scene.actors[ai].id, 12));
                let chip = element_drag_chip(
                    ui,
                    ("act_chip", ai),
                    &label,
                    Color32::from_rgb(220, 130, 50),
                );
                if chip.dragged() {
                    state.element_drag.source = Some(crate::state::AttachableElement::Actor(ai));
                    state.element_drag.label = label.clone();
                    if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                        state.element_drag.pos = [p.x, p.y];
                    }
                }
            }
        });
        ui.add_space(6.0);

        // ── Per-template point list with drop zones ──
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

/// Commit a drag-and-drop attach: write the binding into the source
/// element's `skeleton_attachment` field (overlays) or push into the
/// source actor's `skeleton_attachments` list.
fn attach_element_to_skeleton_point(
    state: &mut EditorState,
    src: crate::state::AttachableElement,
    skeleton_id: &str,
    point_name: &str,
) {
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
            // Avoid duplicates for the same skeleton + point.
            state.scene.actors[ai].skeleton_attachments.retain(|att| {
                !(att.skeleton_id == skeleton_id && att.point_name == point_name)
            });
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
            ui.label(RichText::new(format!("Image: {}", im.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("In:");
                ui.add(egui::DragValue::new(&mut im.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label("Out:");
                ui.add(egui::DragValue::new(&mut im.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });
            inspector_overlay_state_widgets(
                ui, &mut im.layout, &mut im.animated_params, playhead, i, "img",
                state.kf_highlight.clone());
            ui.add_space(8.0);
            inspector_modifiers(ui, &mut im.modifiers, ("img_mods", i));
            ui.add_space(8.0);
            inspector_effect_stack(ui, &mut im.effects, ("img_fx", i));
        }
        Overlay::Video(v) => {
            ui.label(RichText::new(format!("Video: {}", v.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("In:");
                ui.add(egui::DragValue::new(&mut v.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label("Out:");
                ui.add(egui::DragValue::new(&mut v.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });
            inspector_overlay_state_widgets(
                ui, &mut v.layout, &mut v.animated_params, playhead, i, "vid",
                state.kf_highlight.clone());
            ui.add_space(8.0);
            inspector_modifiers(ui, &mut v.modifiers, ("vid_mods", i));
            ui.add_space(8.0);
            inspector_effect_stack(ui, &mut v.effects, ("vid_fx", i));
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

    let mut new_scale = cur.scale;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::SCALE, (salt_kind, "sc", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::SCALE), "Scale:"));
        let r = ui.add(egui::Slider::new(&mut new_scale, 0.05..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::SCALE, false, |s| s.scale = new_scale);
        }
    });

    let mut new_sy = cur.scale_y;
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::SCALE_Y, (salt_kind, "sy", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::SCALE_Y), "Stretch Y:"));
        let r = ui.add(egui::Slider::new(&mut new_sy, 0.1..=5.0).logarithmic(true));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::SCALE_Y, false, |s| s.scale_y = new_sy);
        }
    });

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
    ui.horizontal(|ui| {
        kf_anim::animated_toggle(ui, animated_params, param_ids::FLIP_Y, (salt_kind, "fy", salt_idx));
        ui.label(param_label(highlight.is_active(param_ids::FLIP_Y), "Flip Y:"));
        let r = ui.add(egui::Slider::new(&mut new_fy, -1.0..=1.0));
        if r.changed() {
            kf_anim::write_overlay_param(layout, animated_params, playhead,
                param_ids::FLIP_Y, false, |s| s.flip_y_anim = new_fy);
        }
    });
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
    ui.label(RichText::new("Text:").size(11.0).strong());
    ui.add(
        egui::TextEdit::multiline(&mut t.text)
            .desired_rows(2)
            .desired_width(ui.available_width()),
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
        RichText::new("Font").size(12.0).strong().color(Color32::from_rgb(180, 220, 255)),
    ).default_open(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Family:");
            egui::ComboBox::from_id_source("text_font_family")
                .selected_text(t.style.font.clone())
                .show_ui(ui, |ui| {
                    for fam in COMMON_FONTS {
                        ui.selectable_value(&mut t.style.font, fam.to_string(), *fam);
                    }
                });
        });

        // ── Size: compact "−  [drag value]  +" trio with quick presets.
        // The user's #1 ask was "more intuitive sizing with fewer params"
        // — the wide 8..=512 slider felt finicky. The drag value still
        // covers the whole sane range, but the −/+ buttons step by a
        // visible amount and the preset row gets you to common sizes
        // in one click.
        ui.horizontal(|ui| {
            ui.label("Size:");
            if ui.small_button("\u{2212}").on_hover_text("Decrease (-4)").clicked() {
                t.style.font_size = (t.style.font_size - 4.0).max(8.0);
            }
            ui.add(
                egui::DragValue::new(&mut t.style.font_size)
                    .range(8.0..=512.0)
                    .speed(0.5)
                    .suffix(" px"),
            );
            if ui.small_button("+").on_hover_text("Increase (+4)").clicked() {
                t.style.font_size = (t.style.font_size + 4.0).min(512.0);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Preset:");
            for (label, sz) in [("S", 24.0_f32), ("M", 48.0), ("L", 96.0), ("XL", 144.0)] {
                if ui.small_button(label).on_hover_text(format!("{} px", sz as i32)).clicked() {
                    t.style.font_size = sz;
                }
            }
        });

        ui.horizontal(|ui| {
            ui.checkbox(&mut t.style.bold, "Bold");
            ui.checkbox(&mut t.style.italic, "Italic");
        });
        ui.horizontal(|ui| {
            ui.label("Color:");
            color_edit_u8(ui, &mut t.style.color);
        });
        ui.horizontal(|ui| {
            ui.label("Align:");
            ui.selectable_value(&mut t.style.align, TextAlign::Left, "Left");
            ui.selectable_value(&mut t.style.align, TextAlign::Center, "Center");
            ui.selectable_value(&mut t.style.align, TextAlign::Right, "Right");
        });
    });
    ui.add_space(4.0);

    // ─── Stroke (glyph outline) ───────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new("Stroke").size(12.0).strong().color(Color32::from_rgb(255, 200, 120)),
    ).default_open(true).show(ui, |ui| {
        let mut has_outline = t.style.outline.is_some();
        ui.checkbox(&mut has_outline, "Stroke text");
        if has_outline && t.style.outline.is_none() {
            t.style.outline = Some([0, 0, 0]);
            if t.style.outline_width <= 0.0 { t.style.outline_width = 4.0; }
        }
        if !has_outline {
            t.style.outline = None;
        }

        if let Some(oc) = t.style.outline.as_mut() {
            ui.horizontal(|ui| {
                ui.label("Color:");
                color_edit_u8(ui, oc);
                ui.label("Width:");
                ui.add(egui::DragValue::new(&mut t.style.outline_width)
                    .range(0.0..=20.0).speed(0.1));
            });
        }
    });
    ui.add_space(4.0);

    // ─── Background plate ─────────────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new("Background plate").size(12.0).strong().color(Color32::from_rgb(180, 255, 180)),
    ).default_open(true).show(ui, |ui| {
        let mut has_box = t.style.box_color.is_some();
        ui.checkbox(&mut has_box, "Enable plate");
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
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Solid, "Solid");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Gradient, "Gradient");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::OutlineOnly, "Outline only");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::None, "None (text only)");
                    });
            });

            if matches!(t.style.box_kind, TextBoxKind::Solid | TextBoxKind::Gradient) {
                if let Some(bc) = t.style.box_color.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label("Color:"); color_edit_u8(ui, bc);
                    });
                }
            }
            if matches!(t.style.box_kind, TextBoxKind::Gradient) {
                if t.style.box_gradient_end.is_none() {
                    t.style.box_gradient_end = Some([60, 60, 60]);
                }
                if let Some(end) = t.style.box_gradient_end.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label("Gradient end:"); color_edit_u8(ui, end);
                    });
                }
            }

            ui.add(egui::Slider::new(&mut t.style.box_opacity, 0.0..=1.0).text("Opacity"));
            ui.add(egui::Slider::new(&mut t.style.box_padding, 0.0..=80.0).text("Padding"));
            ui.add(egui::Slider::new(&mut t.style.box_corner_radius, 0.0..=80.0).text("Corner radius"));

            // Plate border (independent of glyph stroke)
            let mut has_border = t.style.box_outline_color.is_some() || t.style.box_outline_width > 0.0;
            ui.checkbox(&mut has_border, "Plate border");
            if has_border && t.style.box_outline_color.is_none() {
                t.style.box_outline_color = Some([0, 0, 0]);
            }
            if !has_border {
                t.style.box_outline_color = None;
                t.style.box_outline_width = 0.0;
            }
            if let Some(boc) = t.style.box_outline_color.as_mut() {
                ui.horizontal(|ui| {
                    ui.label("Color:"); color_edit_u8(ui, boc);
                    ui.label("Width:");
                    ui.add(egui::DragValue::new(&mut t.style.box_outline_width)
                        .range(0.0..=20.0).speed(0.1));
                });
            }
        }
    });

    ui.add_space(8.0);
    inspector_modifiers(ui, &mut t.modifiers, ("text_mods", idx));
    ui.add_space(8.0);
    inspector_effect_stack(ui, &mut t.effects, ("text_fx", idx));

    // Layer/z-index actions are no longer exposed from the inspector — the
    // timeline track row order alone determines stacking.
    None
}

const COMMON_FONTS: &[&str] = &[
    "DejaVuSans",
    "DejaVuSans-Bold",
    "Arial",
    "Helvetica",
    "Impact",
    "Roboto",
    "Times",
    "Courier",
    "Comic Sans MS",
    "Verdana",
    "Tahoma",
    "Georgia",
];


/// Inspector for the render frame (output area). Exposes position,
/// rotation, and size in world pixels just like any other element.
fn inspector_render_frame(ui: &mut egui::Ui, state: &mut EditorState) {
    let rf = &mut state.scene.render_frame;
    let [rw, rh] = rf.resolution;

    ui.label(
        RichText::new("Render Frame")
            .strong()
            .size(14.0)
            .color(Color32::from_rgb(255, 120, 120)),
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new("The output region. Move/resize/rotate it like any element.")
            .size(10.0)
            .color(COL_TEXT_DIM),
    );
    ui.add_space(8.0);

    if let Some(kf) = rf.layout.first_mut() {
        // ─── Position (world pixels) ────────────────────────────
        ui.horizontal(|ui| {
            ui.label("X:");
            ui.add(egui::DragValue::new(&mut kf.value.pos.x).speed(0.5));
            ui.label("Y:");
            ui.add(egui::DragValue::new(&mut kf.value.pos.y).speed(0.5));
        });
        ui.add_space(4.0);

        // ─── Size (width × height in world pixels) ──────────────
        // The frame's world extent = resolution * scale. Editing the
        // width here updates `scale` so the displayed resolution stays
        // fixed. The height field stays in lock-step with the aspect
        // ratio of the output resolution.
        let zoom_clamped = kf.value.zoom.max(0.0001);
        let mut world_w = rw as f32 / zoom_clamped;
        let mut world_h = rh as f32 / zoom_clamped;
        let aspect = (rw as f32 / rh.max(1) as f32).max(0.0001);

        ui.horizontal(|ui| {
            ui.label("W:");
            if ui
                .add(egui::DragValue::new(&mut world_w).range(8.0..=200_000.0).speed(0.5))
                .changed()
            {
                kf.value.zoom = (rw as f32 / world_w.max(1.0)).clamp(0.001, 1000.0);
            }
            ui.label("H:");
            if ui
                .add(egui::DragValue::new(&mut world_h).range(8.0..=200_000.0).speed(0.5))
                .changed()
            {
                let new_w = world_h * aspect;
                kf.value.zoom = (rw as f32 / new_w.max(1.0)).clamp(0.001, 1000.0);
            }
        });
        ui.label(
            RichText::new(format!("aspect locked to {}\u{00D7}{} output", rw, rh))
                .size(10.0)
                .color(COL_TEXT_DIM),
        );
        ui.add_space(4.0);

        // ─── Rotation ────────────────────────────────────────────
        ui.add(
            egui::Slider::new(&mut kf.value.rotation_deg, -180.0..=180.0)
                .text("Rotation")
                .step_by(0.1)
                .fixed_decimals(1)
                .smart_aim(false),
        );
        ui.add_space(4.0);

        // ─── Scale ──────────────────────────────────────────────
        // Scale here is the inverse of the legacy `zoom` field: scale = 1
        // means the frame's world size matches the output resolution 1:1;
        // scale > 1 enlarges the frame on the canvas; scale < 1 shrinks
        // it. We expose this as the user-facing concept because
        // "scale" reads more intuitively for an animatable element than
        // "zoom" did.
        let mut scale = 1.0 / kf.value.zoom.max(1e-4);
        if ui
            .add(
                egui::Slider::new(&mut scale, 0.1..=20.0)
                    .text("Scale")
                    .logarithmic(true),
            )
            .changed()
        {
            kf.value.zoom = (1.0 / scale.max(1e-4)).clamp(0.001, 1000.0);
        }
    } else {
        ui.label(
            RichText::new("Render frame has no keyframes.")
                .italics()
                .color(COL_TEXT_DIM),
        );
    }

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    ui.label(
        RichText::new("Output resolution")
            .size(11.0)
            .strong()
            .color(Color32::from_rgb(180, 180, 220)),
    );
    ui.horizontal(|ui| {
        let mut w = rw;
        let mut h = rh;
        ui.label("W:");
        let cw = ui
            .add(egui::DragValue::new(&mut w).range(64..=8192))
            .changed();
        ui.label("H:");
        let ch = ui
            .add(egui::DragValue::new(&mut h).range(64..=8192))
            .changed();
        if cw || ch {
            rf.resolution = [w, h];
        }
    });

    // Animation modifiers (wobble/shake/pulse/spin) — perturb the
    // render-frame's eased keyframe state at preview/export time so the
    // user can add live camera-style motion without authoring every kf.
    ui.add_space(10.0);
    inspector_modifiers(ui, &mut rf.modifiers, "rf_mods");
    ui.add_space(8.0);
    inspector_effect_stack(ui, &mut rf.effects, "rf_fx");
}

fn inspector_background(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let b = &mut state.scene.backgrounds[i];
    ui.label(RichText::new(format!("Background: {}", b.id)).strong().size(14.0).color(COL_CLIP_BG));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Start:");
        ui.add(egui::DragValue::new(&mut b.start).speed(0.02).suffix("s"));
        ui.label("Duration:");
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
    let _ = state.scene.output.duration;
    let audio = &mut state.scene.audio[i];
    ui.label(RichText::new(format!("Audio: {}", audio.id)).strong().size(14.0).color(COL_CLIP_AUDIO));
    ui.add_space(4.0);

    // Clip-local time at the playhead — keyframes for volume / speed
    // are stored in clip-local seconds so the same edits apply when the
    // user moves the audio along the timeline.
    let t_local = (state.playhead - audio.t_in).max(0.0);

    // ── Volume ───────────────────────────────────────────────────────
    inspector_audio_param(
        ui,
        "Volume",
        "volume",
        0.0..=2.0,
        false, // not logarithmic
        t_local,
        &mut audio.volume,
        &mut audio.volume_kfs,
        &mut audio.animated_params,
    );

    // ── Speed (logarithmic, 0.25× .. 4.0×, 1.0× neutral) ─────────────
    inspector_audio_param(
        ui,
        "Speed",
        "speed",
        0.25..=4.0,
        true, // logarithmic
        t_local,
        &mut audio.speed,
        &mut audio.speed_kfs,
        &mut audio.animated_params,
    );
    if audio.speed.abs() < 0.05 {
        audio.speed = 0.05;
    }

    ui.add_space(6.0);
    if audio.parent_actor.is_some() {
        ui.label(
            RichText::new("Bound to an actor — moves and trims with its parent clip.")
                .size(10.0)
                .italics()
                .color(COL_TEXT_DIM),
        );
    } else {
        ui.label(
            RichText::new("Standalone music — independent of any actor.")
                .size(10.0)
                .italics()
                .color(COL_TEXT_DIM),
        );
    }
}

/// Render one row of the audio inspector with an animation toggle.
/// Mirrors the behaviour of inspector_anim_row for transform params on
/// actors / overlays — when "Animated" is on, edits to the slider write
/// a keyframe at the playhead's clip-local time; otherwise edits change
/// the static value.
#[allow(clippy::too_many_arguments)]
fn inspector_audio_param(
    ui: &mut egui::Ui,
    label: &str,
    param_id: &str,
    range: std::ops::RangeInclusive<f32>,
    logarithmic: bool,
    t_local: f32,
    static_value: &mut f32,
    kfs: &mut Vec<memstroy_core::Keyframe<f32>>,
    animated: &mut std::collections::BTreeSet<String>,
) {
    let is_animated = animated.contains(param_id);

    // Display value: when animated, sample the kf track at the playhead
    // so the slider reflects the current animated value; otherwise the
    // static field.
    let mut display = if is_animated && !kfs.is_empty() {
        memstroy_core::keyframe::sample(kfs, t_local).unwrap_or(*static_value)
    } else {
        *static_value
    };

    ui.horizontal(|ui| {
        ui.label(label);
        let mut slider = egui::Slider::new(&mut display, range.clone());
        if logarithmic {
            slider = slider.logarithmic(true);
            if param_id == "speed" {
                slider = slider.suffix("x");
            }
        }
        let resp = ui.add(slider);

        if resp.changed() {
            if is_animated {
                // Insert / replace a keyframe at the current clip-local
                // playhead. Seed the track with the current static
                // value at t=0 if it's still empty so the timeline has
                // a stable starting point.
                if kfs.is_empty() {
                    kfs.push(memstroy_core::Keyframe::new(0.0, *static_value));
                }
                memstroy_core::upsert_keyframe(kfs, t_local, display);
            } else {
                *static_value = display;
            }
        }

        // Animation toggle on the right.
        let mut anim_on = is_animated;
        let toggle = ui
            .selectable_label(anim_on, "\u{29BF}") // bullseye glyph
            .on_hover_text("Toggle animation for this parameter");
        if toggle.clicked() {
            anim_on = !anim_on;
            if anim_on {
                animated.insert(param_id.to_string());
                if kfs.is_empty() {
                    kfs.push(memstroy_core::Keyframe::new(0.0, *static_value));
                }
            } else {
                animated.remove(param_id);
            }
        }
    });

    // Quick "+ kf" button row when animated, so the user can drop
    // keyframes without dragging the slider every time.
    if is_animated {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if ui
                .small_button("+ kf at playhead")
                .on_hover_text("Add a keyframe at the current playhead")
                .clicked()
            {
                if kfs.is_empty() {
                    kfs.push(memstroy_core::Keyframe::new(0.0, *static_value));
                }
                memstroy_core::upsert_keyframe(kfs, t_local, display);
            }
            if !kfs.is_empty() && ui.small_button("Clear kfs").clicked() {
                kfs.clear();
            }
            ui.label(
                RichText::new(format!("({} kf)", kfs.len()))
                    .size(9.0)
                    .color(COL_TEXT_DIM),
            );
        });
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
        // ── Play / Pause / Stop transport ──
        // Always-visible inline playback controls so the user doesn't have
        // to reach for the Space shortcut. Stop returns the playhead to 0
        // and pauses; the Play button toggles like Space does. The big
        // glyphs render via the default font and stay legible at any
        // zoom level.
        let play_glyph = if state.playing { "\u{23F8}" } else { "\u{25B6}" }; // ⏸ / ▶
        let play_label = if state.playing { "Pause (Space)" } else { "Play (Space)" };
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
                "\u{25B6} Playing".into()
            } else {
                "\u{23F8} Paused".into()
            };
        }

        let stop_btn = egui::Button::new(
            RichText::new("\u{23F9}")
                .size(14.0)
                .color(Color32::from_rgb(220, 120, 120)),
        )
        .min_size(Vec2::new(26.0, 22.0));
        if ui.add(stop_btn).on_hover_text("Stop & rewind to start").clicked() {
            state.playing = false;
            state.playhead = 0.0;
            state.status = "\u{23F9} Stopped".into();
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
            .on_hover_text("Split tool: click anywhere on a clip to cut it at that position")
            .clicked()
        {
            state.split_tool_active = !state.split_tool_active;
        }

        // Add Text tool
        if ui.button(RichText::new("\u{1F520} +T").color(Color32::from_rgb(140, 220, 255)))
            .on_hover_text("Add text overlay at playhead")
            .clicked()
        {
            add_text_overlay(state);
        }

        ui.separator();

        // Loop preview toggle
        let loop_color = if state.loop_mode { Color32::from_rgb(255, 180, 80) } else { COL_TEXT_DIM };
        if ui
            .button(RichText::new("\u{1F501} Loop").size(11.0).color(loop_color))
            .on_hover_text(
                "Loop preview: Shift+click on the ruler to set loop start, Shift+click again for end. \
                Shift+drag = define a region.",
            )
            .clicked()
        {
            state.loop_mode = !state.loop_mode;
            if !state.loop_mode {
                state.loop_pending_start = None;
            }
        }

        // Zoom display (read-only — adjust via scrollbar handles)
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{:.0}px/s", state.timeline_zoom)).size(10.0).color(COL_TEXT_DIM));
        });
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


    // ── Track rows ──
    let mut to_select: Option<Selection> = None;

    let v_zoom = state.timeline_v_zoom.max(0.1);
    let num_tracks = state.tracks.len();

    // ── Pre-compute per-track row rectangles for vertical drag-resolution ──
    // (used by clip-drag handlers below to figure out which track the pointer
    // currently hovers over, and whether the user is dragging above the
    // topmost video / below the bottommost audio so we can auto-create a new
    // layer in that direction). The "expansion" added by the per-param
    // keyframe rows of the currently-selected layer is included here so the
    // hit-test recognises the whole row as one lane.
    let mut track_rows: Vec<(f32, f32)> = Vec::with_capacity(num_tracks);
    {
        let mut acc = 0.0_f32;
        for (ti, tk) in state.tracks.iter().enumerate() {
            let h = tk.height * v_zoom + selected_layer_expansion(state, ti, v_zoom);
            let top = tracks_rect.min.y + acc - state.timeline_v_scroll;
            let bot = top + h;
            track_rows.push((top, bot));
            acc += h;
        }
    }
    let pointer_y: Option<f32> = ui.input(|i| i.pointer.hover_pos().map(|p| p.y));
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
    let total_tracks_h: f32 = (0..num_tracks)
        .map(|i| state.tracks[i].height * v_zoom + selected_layer_expansion(state, i, v_zoom))
        .sum();
    let max_v_scroll = (total_tracks_h - viewport_h).max(0.0);
    state.timeline_v_scroll = state.timeline_v_scroll.max(0.0).min(max_v_scroll);
    let v_scroll = state.timeline_v_scroll;

    // Aggregate the kf-row click hits into a single update step at the
    // end of the loop. Avoids interleaving mutable borrows of state with
    // the clip-draw / drag handlers above.
    let mut param_row_clicks: Vec<(crate::kf_anim::SelectedLayer, ParamRowClick)> = Vec::new();

    let mut acc_y = 0.0_f32;
    for track_idx in 0..num_tracks {
        let track = &state.tracks[track_idx];
        let track_h = track.height * v_zoom;
        let expansion = selected_layer_expansion(state, track_idx, v_zoom);
        let effective_track_h = track_h + expansion;
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

        // Clip area = top portion of the row (excludes the param-row
        // expansion below). draw_clip / draw_keyframe_diamonds only draw
        // inside this rect so the per-param sub-rows have a clean area
        // beneath them.
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(tracks_rect.min.x, row_top + 1.0),
            egui::pos2(tracks_rect.max.x, row_top + track_h - 1.0),
        );

        // Draw clips on this track
        match track_kind {
            TrackKind::Video => {
                // Draw backgrounds on track 0
                if track_idx == 0 {
                    for bi in 0..state.scene.backgrounds.len() {
                        let bg_elem = &state.scene.backgrounds[bi];
                        let clip_start = bg_elem.start;
                        let clip_end = bg_elem.start + bg_elem.duration;
                        // Cull off-screen background clips before any
                        // per-clip allocation / interaction work.
                        if !in_viewport(clip_start, clip_end) { continue; }
                        let sel = state.selection == Selection::Background(bi);
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
                                to_select = Some(Selection::Background(bi));
                            } else if clicked == f32::NEG_INFINITY {
                                // Trim right: stretch / shrink the duration.
                                let dx = ui.input(|i| i.pointer.delta().x);
                                let delta_t = dx / pps;
                                let new_dur = (clip_end - clip_start + delta_t).max(0.1);
                                let token = EditorState::drag_token("trim_bg_right", bi);
                                state.mutate_drag(token, |s| {
                                    s.backgrounds[bi].duration = new_dur;
                                });
                                to_select = Some(Selection::Background(bi));
                            } else if clicked < 0.0 {
                                let new_start = (-clicked).max(0.0);
                                let dur = clip_end - clip_start;
                                let token = EditorState::drag_token("move_bg", bi);
                                state.mutate_drag(token, |s| {
                                    s.backgrounds[bi].start = new_start;
                                    s.backgrounds[bi].duration = dur;
                                });
                                to_select = Some(Selection::Background(bi));
                            } else if state.split_tool_active {
                                to_select = Some(Selection::Background(bi));
                                state.playhead = clicked;
                                state.status = "__SPLIT_AT_PLAYHEAD__".into();
                            } else {
                                to_select = Some(Selection::Background(bi));
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
                    let sel = state.selection == Selection::Actor(ai);
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
                            // Bound audio: shift its in-edge by the same delta and
                            // advance source_start so the playback head doesn't slip.
                            sync_audio_to_actor(state, ai);
                            to_select = Some(Selection::Actor(ai));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right edge: adjust t_out
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_out = (clip_end + delta_t).max(clip_start + 0.1);
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
                            sync_audio_to_actor(state, ai);
                            to_select = Some(Selection::Actor(ai));
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
                            sync_audio_to_actor(state, ai);
                            to_select = Some(Selection::Actor(ai));
                        } else if state.split_tool_active {
                            to_select = Some(Selection::Actor(ai));
                            state.playhead = clicked;
                            state.status = "__SPLIT_AT_PLAYHEAD__".into();
                        } else {
                            // ── Ctrl+click multi-select ──
                            let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                            if ctrl_held {
                                // Toggle in multi_select
                                if let Some(pos) = state.multi_select.iter().position(|&x| x == ai) {
                                    state.multi_select.remove(pos);
                                } else {
                                    state.multi_select.push(ai);
                                }
                            } else {
                                state.multi_select.clear();
                            }
                            to_select = Some(Selection::Actor(ai));
                        }
                    }

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

                    // Keyframe diamonds on the clip bar — one per layout keyframe.
                    draw_keyframe_diamonds(
                        painter,
                        content_rect,
                        clip_start,
                        clip_end,
                        &state.scene.actors[ai].layout,
                        state.timeline_scroll,
                        pps,
                        track_left,
                        track_right,
                        sel,
                        true, // actor kfs are stored in scene-time
                    );
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
                    let sel = state.selection == Selection::Overlay(oi);
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
                            to_select = Some(Selection::Overlay(oi));
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
                            to_select = Some(Selection::Overlay(oi));
                        } else if clicked < 0.0 {
                            // Drag: move the overlay's time window.
                            let new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;
                            let new_end = new_start + dur;
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
                            // deferred to drag-end.
                            if let Some(py) = pointer_y {
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
                            to_select = Some(Selection::Overlay(oi));
                        } else if state.split_tool_active {
                            to_select = Some(Selection::Overlay(oi));
                            state.playhead = clicked;
                            state.status = "__SPLIT_AT_PLAYHEAD__".into();
                        } else {
                            to_select = Some(Selection::Overlay(oi));
                        }
                    }
                    // Keyframe diamonds for overlays too.
                    let layout_ref: &[Keyframe<OverlayState>] = match &state.scene.overlays[oi] {
                        Overlay::Text(t) => &t.layout,
                        Overlay::Image(im) => &im.layout,
                        Overlay::Video(v) => &v.layout,
                    };
                    draw_keyframe_diamonds(
                        painter,
                        content_rect,
                        clip_start,
                        clip_end,
                        layout_ref,
                        state.timeline_scroll,
                        pps,
                        track_left,
                        track_right,
                        sel,
                        false, // overlay kfs are clip-local
                    );
                }
            }
            TrackKind::Audio => {
                let audio_tracks: Vec<usize> = (0..num_tracks).filter(|ti| state.tracks[*ti].kind == TrackKind::Audio).collect();

                for aui in 0..state.scene.audio.len() {
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
                    if !in_viewport(clip_start, clip_end) { continue; }
                    let sel = state.selection == Selection::Audio(aui);
                    let audio_id = egui::Id::new(("timeline_clip", "audio", aui));
                    if let Some(clicked) = draw_audio_clip(ui, painter, content_rect, &audio.id, audio_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        sel, track_h, track_locked, state.split_tool_active,
                        state.audio_waveforms.get(aui),
                        audio_source_start)
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
                            to_select = Some(Selection::Audio(aui));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right: extend / shrink the audible window.
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_out = (clip_end + delta_t).max(clip_start + 0.1);
                            let token = EditorState::drag_token("trim_audio_right", aui);
                            state.mutate_drag(token, |s| {
                                s.audio[aui].t_out = Some(new_out);
                            });
                            to_select = Some(Selection::Audio(aui));
                        } else if clicked < 0.0 {
                            // Drag: move the audio clip horizontally.
                            let new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;
                            let token = EditorState::drag_token("move_audio", aui);
                            state.mutate_drag(token, |s| {
                                s.audio[aui].t_in = new_start;
                                s.audio[aui].t_out = Some(new_start + dur);
                            });

                            // Vertical: only allow audio to land on audio
                            // lanes. Lane creation is deferred to drag-end
                            // via state.timeline_drag.pending_new_lane.
                            if let Some(py) = pointer_y {
                                let cur = state.audio_track_assignments.get(&aui).copied();
                                match classify_pointer_y(py, cur) {
                                    DropIntent::ToAudioRow(idx) => {
                                        state.audio_track_assignments.insert(aui, idx);
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                    DropIntent::NewAudioTop => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::AudioTopForAudio(aui));
                                    }
                                    DropIntent::NewAudioBottom => {
                                        state.timeline_drag.pending_new_lane =
                                            Some(crate::state::NewLaneIntent::AudioBottomForAudio(aui));
                                    }
                                    _ => {
                                        state.timeline_drag.pending_new_lane = None;
                                    }
                                }
                            }
                            to_select = Some(Selection::Audio(aui));
                        } else {
                            to_select = Some(Selection::Audio(aui));
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
                let kf_pairs: Vec<(f32, f32)> = keyframe_times_for_layer(state, sel_layer)
                    .into_iter()
                    .map(|local_t| (local_t, kf_time_to_scene_time(state, sel_layer, local_t)))
                    .collect();
                let outcome = draw_param_kf_rows(
                    ui,
                    painter,
                    &layer_label,
                    &params,
                    &kf_pairs,
                    row_top + track_h,
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
    sync_bound_audio_lanes(state);

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
    let total_unscaled_h: f32 = state.tracks.iter().map(|t| t.height).sum::<f32>().max(1.0);
    let total_v = (total_unscaled_h * v_zoom).max(viewport_h);
    let view_a_v = (state.timeline_v_scroll / total_v).clamp(0.0, 1.0);
    let view_b_v = ((state.timeline_v_scroll + viewport_h) / total_v).clamp(view_a_v, 1.0);
    let (new_a_v, new_b_v) = stretchable_scrollbar(
        ui,
        v_sb_rect,
        false, // vertical
        view_a_v,
        view_b_v,
    );
    {
        let new_window_pixels = ((new_b_v - new_a_v) * total_v).max(20.0);
        // viewport_h must equal v_zoom * total_unscaled_h * (new_b_v - new_a_v)
        // → v_zoom = viewport_h / (total_unscaled_h * (new_b_v - new_a_v))
        let denom = (total_unscaled_h * (new_b_v - new_a_v)).max(0.0001);
        let new_v_zoom = (viewport_h / denom).clamp(0.25, 8.0);
        state.timeline_v_zoom = new_v_zoom;
        // Recompute v_scroll using the NEW total height (so position stays consistent).
        let new_total_v = (total_unscaled_h * new_v_zoom).max(viewport_h);
        state.timeline_v_scroll = (new_a_v * new_total_v).max(0.0);
        let _ = new_window_pixels; // silence unused
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
                //   1. The lane under the cursor when it's an unlocked video lane.
                //   2. Otherwise, create a new video lane just above the
                //      audio block so the dropped clip gets its own layer.
                let target = drop_track
                    .filter(|i| state.tracks[*i].kind == TrackKind::Video
                        && !state.tracks[*i].locked);
                let assigned = match target {
                    Some(t) => t,
                    None => state.insert_video_track_at_bottom(),
                };
                add_actor_from_clip_at_time(state, &asset_path, drop_time);
                if let Some(new_idx) = state.scene.actors.len().checked_sub(1) {
                    state.actor_track_assignments.insert(new_idx, assigned);
                    // The bound audio (added by add_actor_from_clip_at_time)
                    // mirrors the actor's lane via sync_bound_audio_lanes()
                    // at the end of the frame.
                }
            } else if matches!(kind, AssetDragKind::Sound | AssetDragKind::Image | AssetDragKind::Particle) {
                // Build a LibraryAsset proxy from the drag payload and
                // delegate to the per-kind spawner. The element lands
                // at the drop time on the playhead-default lane.
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
        state.timeline_drag.dragging_clip = None;
        state.timeline_drag.pending_new_lane = None;
        state.timeline_drag.start_pointer_y = None;
    }
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
    let edge_zone = (cross * 0.6).max(6.0);
    let min_window_frac = (10.0 / track_len).min(0.5);

    let a = view_a_frac.clamp(0.0, 1.0);
    let b = view_b_frac.clamp(a + min_window_frac.min(0.001), 1.0);

    let id = ui.make_persistent_id((
        "scrollbar",
        rect.min.x as i32,
        rect.min.y as i32,
        horizontal,
    ));
    let resp = ui.interact(rect, id, Sense::click_and_drag());

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

    // Edge detection for hover cursor (purely cosmetic; the actual drag mode
    // is captured once at drag_started below and locked for the rest of the
    // drag, so the cursor flicker doesn't affect behaviour).
    let hover_pos = ui.input(|i| i.pointer.hover_pos());

    let near_left_edge = hover_pos.map(|p| (p.x - bar_rect.min.x).abs() < 5.0).unwrap_or(false);
    let near_right_edge = hover_pos.map(|p| (p.x - bar_rect.max.x).abs() < 5.0).unwrap_or(false);

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
                let visible_offset_pix = x_start - x_start_full;
                let source_t_at_visible_start =
                    source_start + (visible_offset_pix / full_bar_w) * (clip_end - clip_start);
                let source_t_per_pixel = (clip_end - clip_start) / full_bar_w;
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

/// Sample-and-extract the keyframe times for a given (layer, param) so
/// the timeline can render a diamond per kf without needing access to
/// the typed layout. We currently use the same `Vec<Keyframe<…>>` for
/// every param of the layer, so the times are shared — the per-param
/// row only differs in label / colour.
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
        _ => Vec::new(),
    }
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
fn draw_param_kf_rows(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    sel_layer_label: &crate::kf_anim::SelectedLayer,
    params: &[String],
    kf_pairs: &[(f32, f32)],
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
        // belong to the clip's visible range.
        let half = 4.5_f32;
        for &(local_t, scene_t) in kf_pairs {
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
            let r = ui.interact(hit, id, Sense::click());
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
        }
    }

    outcome
}

/// Output of `draw_param_kf_rows` consumed by the caller to update
/// state without nested borrow scopes.
#[derive(Default)]
struct ParamRowOutcome {
    click_hits: Vec<ParamRowClick>,
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
        volume: 1.0,
        speed: 1.0,
        parent_actor: Some(actor_id.to_string()),
        volume_kfs: Vec::new(),
        speed_kfs: Vec::new(),
        animated_params: Default::default(),
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
    match std::process::Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path).output()
    {
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

    // Drop the actor onto the topmost video lane by default — same default
    // as the timeline drop-handler when no specific lane is targeted.
    let assigned = state
        .video_track_indices()
        .first()
        .copied()
        .unwrap_or_else(|| state.insert_video_track_at_bottom());
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
