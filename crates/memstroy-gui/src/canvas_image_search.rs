//! Canvas-anchored web image search (RMB selection frame + inline UI).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use image::{Rgba, RgbaImage};
use memstroy_core::{CanvasLayout, CanvasTransform, Effect, EffectKind, Keyframe, MaskShape, Overlay, WorldPos};

use crate::jobs::{
    spawn_ai_background_remove, spawn_web_image_download, spawn_web_image_search, JobEvent,
    WebImageSearchOptions, WebSearchTarget,
};
use crate::state::{CanvasMarquee, EditorState, LibraryAsset, LibraryTab, MaskTool, Selection};
use crate::web_image_search::WebImageHit;

/// How many preview cards the carousel shows at once.
pub const CAROUSEL_PAGE_SIZE: usize = 4;

/// Minimum world-pixel size for a committed selection frame.
pub const MIN_FRAME_WORLD_PX: f32 = 8.0;

/// Minimum pointer travel (screen px) before an RMB press becomes a
/// selection-frame drag instead of a no-op release.
pub const RMB_MIN_TRAVEL_PX: f32 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CanvasImageSearchMode {
    /// RMB drag — image is cover-fitted inside the rectangle.
    SelectionFrame {
        rect: ([f32; 2], [f32; 2]),
    },
    /// Single RMB click — search popup only, no selection frame.
    PopupOnly,
}

/// One active canvas search popup + selection frame.
#[derive(Clone, Debug)]
pub struct CanvasImageSearchSession {
    pub mode: CanvasImageSearchMode,
    pub ui_anchor_world: [f32; 2],
    pub query: String,
    pub last_sent_query: String,
    pub transparent_only: bool,
    /// Tunables for fake-checkerboard removal (transparent-only downloads).
    pub checker_color_tolerance: u8,
    pub checker_edge_flood: bool,
    pub checker_edge_brightness: u8,
    pub checker_neutral_tolerance: u8,
    /// Original downloaded file before checkerboard strip (for re-apply).
    pub strip_source_path: Option<PathBuf>,
    /// When set, popup is pinned at this screen offset (relative to canvas
    /// panel origin) instead of following the world anchor.
    pub ui_screen_pos: Option<[f32; 2]>,
    pub focus_input: bool,
    pub searching: bool,
    pub results: Vec<WebImageHit>,
    pub carousel_start: usize,
    pub next_offset: Option<u32>,
    pub page_count: u32,
    pub next_request_id: u64,
    pub placed_overlay: Option<usize>,
    pub status: String,
    /// When false the inline search UI is hidden (Apply / Esc) but the
    /// selection frame and placed image remain on canvas.
    pub ui_visible: bool,
    pub ai_removing: bool,
}

impl Default for CanvasImageSearchSession {
    fn default() -> Self {
        Self {
            mode: CanvasImageSearchMode::PopupOnly,
            ui_anchor_world: [0.0, 0.0],
            query: String::new(),
            last_sent_query: String::new(),
            transparent_only: false,
            checker_color_tolerance: 24,
            checker_edge_flood: false,
            checker_edge_brightness: 252,
            checker_neutral_tolerance: 18,
            strip_source_path: None,
            ui_screen_pos: None,
            focus_input: true,
            searching: false,
            results: Vec::new(),
            carousel_start: 0,
            next_offset: None,
            page_count: 0,
            next_request_id: 1,
            placed_overlay: None,
            status: String::new(),
            ui_visible: true,
            ai_removing: false,
        }
    }
}

/// Parameters for removing stock-photo checkerboard backgrounds.
#[derive(Clone, Copy, Debug)]
pub struct CheckerboardStripSettings {
    pub color_tolerance: u8,
    pub edge_flood: bool,
    pub edge_brightness: u8,
    pub neutral_tolerance: u8,
}

impl Default for CheckerboardStripSettings {
    fn default() -> Self {
        Self {
            color_tolerance: 24,
            edge_flood: false,
            edge_brightness: 252,
            neutral_tolerance: 18,
        }
    }
}

impl CheckerboardStripSettings {
    pub fn from_session(session: &CanvasImageSearchSession) -> Self {
        Self {
            color_tolerance: session.checker_color_tolerance,
            edge_flood: session.checker_edge_flood,
            edge_brightness: session.checker_edge_brightness,
            neutral_tolerance: session.checker_neutral_tolerance,
        }
    }
}

/// Brush select + AI cutout controls for a canvas-search image overlay.
pub fn inspector_web_image_tools(ui: &mut egui::Ui, state: &mut EditorState, overlay_idx: usize) {
    let is_placed = state
        .canvas_image_search
        .as_ref()
        .and_then(|s| s.placed_overlay)
        == Some(overlay_idx);
    if !is_placed {
        return;
    }

    let Some(tx) = state.image_fx_tx.clone() else {
        return;
    };

    let mut arm_brush = false;
    let mut apply_ai = false;
    let ai_busy = state
        .canvas_image_search
        .as_ref()
        .is_some_and(|s| s.ai_removing);
    let status = state
        .canvas_image_search
        .as_ref()
        .map(|s| s.status.clone())
        .unwrap_or_default();

    ui.label(
        RichText::new(crate::i18n::t("Web image cutout"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(100, 200, 255)),
    );
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        if ui
            .add_enabled(!ai_busy, egui::Button::new(crate::i18n::t("Brush select")))
            .on_hover_text(crate::i18n::t(
                "Draw around the subject with the brush, then apply AI cutout.",
            ))
            .clicked()
        {
            arm_brush = true;
        }
        if ui
            .add_enabled(!ai_busy, egui::Button::new(crate::i18n::t("Apply AI cutout")))
            .on_hover_text(crate::i18n::t(
                "Remove background using AI (draw a brush selection first).",
            ))
            .clicked()
        {
            apply_ai = true;
        }
    });

    if ai_busy {
        ui.label(RichText::new(crate::i18n::t("Removing background…")).small());
    } else if !status.is_empty() {
        ui.label(RichText::new(status).small().weak());
    }

    if arm_brush {
        state.selection = Selection::Overlay(overlay_idx);
        state.mask_tool = MaskTool::FreehandMask;
        state.mask_draft_points.clear();
        state.status = crate::i18n::t(
            "Draw around the subject, release to finish, then click Apply AI cutout.",
        )
        .into();
    }
    if apply_ai {
        kick_ai_background_remove(state, &tx, overlay_idx);
    }
}

impl CanvasImageSearchSession {
    pub fn target_aspect_ratio(&self) -> Option<f32> {
        let CanvasImageSearchMode::SelectionFrame { rect } = self.mode else {
            return None;
        };
        let (mn, mx) = rect;
        let w = (mx[0] - mn[0]).abs();
        let h = (mx[1] - mn[1]).abs();
        if w > 1.0 && h > 1.0 {
            Some(w / h)
        } else {
            None
        }
    }

    pub fn selection_rect(&self) -> Option<([f32; 2], [f32; 2])> {
        match self.mode {
            CanvasImageSearchMode::SelectionFrame { rect } => Some(rect),
            CanvasImageSearchMode::PopupOnly => None,
        }
    }
}

/// Paint the committed selection frame and the live RMB draft rectangle.
pub fn draw_selection_frames(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    const STROKE: Color32 = Color32::from_rgb(100, 180, 255);
    const DRAFT_STROKE: Color32 = Color32::from_rgba_premultiplied(100, 180, 255, 180);

    if let Some(m) = state.canvas_image_search_draft {
        paint_world_rect_outline(
            painter,
            full_rect,
            state,
            viewport_size,
            &m,
            DRAFT_STROKE,
            1.0,
        );
    }
    if let Some(session) = &state.canvas_image_search {
        if let Some(rect) = session.selection_rect() {
            let marquee = CanvasMarquee {
                start: rect.0,
                end: rect.1,
            };
            paint_world_rect_outline(painter, full_rect, state, viewport_size, &marquee, STROKE, 1.5);
        }
        paint_placement_marker(
            painter,
            full_rect,
            state,
            viewport_size,
            session.ui_anchor_world,
        );
    }
}

fn paint_placement_marker(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    world_pos: [f32; 2],
) {
    let screen = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos {
            x: world_pos[0],
            y: world_pos[1],
        },
        viewport_size,
    );
    let center = Pos2::new(full_rect.min.x + screen[0], full_rect.min.y + screen[1]);
    const R: f32 = 7.0;
    painter.circle_filled(
        center,
        R,
        Color32::from_rgba_premultiplied(100, 180, 255, 200),
    );
    painter.circle_stroke(center, R, Stroke::new(2.0, Color32::WHITE));
    painter.line_segment(
        [Pos2::new(center.x - R - 4.0, center.y), Pos2::new(center.x + R + 4.0, center.y)],
        Stroke::new(1.0, Color32::from_rgb(100, 180, 255)),
    );
    painter.line_segment(
        [Pos2::new(center.x, center.y - R - 4.0), Pos2::new(center.x, center.y + R + 4.0)],
        Stroke::new(1.0, Color32::from_rgb(100, 180, 255)),
    );
}

fn paint_world_rect_outline(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    marquee: &CanvasMarquee,
    stroke: Color32,
    width: f32,
) {
    let (mn, mx) = marquee.rect_world();
    let tl = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos { x: mn[0], y: mn[1] },
        viewport_size,
    );
    let br = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos { x: mx[0], y: mx[1] },
        viewport_size,
    );
    let rect = Rect::from_min_max(
        Pos2::new(full_rect.min.x + tl[0], full_rect.min.y + tl[1]),
        Pos2::new(full_rect.min.x + br[0], full_rect.min.y + br[1]),
    );
    painter.rect_stroke(rect, Rounding::same(2.0), Stroke::new(width, stroke));
}

pub fn open_search_popup_at(state: &mut EditorState, world_pos: [f32; 2]) {
    dismiss_session(state, false);
    let mut session = CanvasImageSearchSession::default();
    session.mode = CanvasImageSearchMode::PopupOnly;
    session.ui_anchor_world = world_pos;
    session.ui_screen_pos = None;
    session.focus_input = true;
    session.ui_visible = true;
    state.canvas_image_search = Some(session);
}

pub fn open_selection_frame_session(
    state: &mut EditorState,
    draft: CanvasMarquee,
    _release_world: [f32; 2],
) {
    dismiss_session(state, false);
    let (mn, mx) = draft.rect_world();
    let mut session = CanvasImageSearchSession::default();
    session.mode = CanvasImageSearchMode::SelectionFrame { rect: (mn, mx) };
    session.ui_anchor_world = [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5];
    session.ui_screen_pos = None;
    session.focus_input = true;
    session.ui_visible = true;
    state.canvas_image_search = Some(session);
}

/// Hide the search popup; keep the selection frame and placed image.
pub fn hide_canvas_search_ui(state: &mut EditorState) {
    if let Some(s) = state.canvas_image_search.as_mut() {
        s.ui_visible = false;
    }
}

/// True when the inline search panel should be painted this frame.
pub fn canvas_search_ui_visible(state: &EditorState) -> bool {
    state
        .canvas_image_search
        .as_ref()
        .is_some_and(|s| s.ui_visible)
}

/// Remove the session; when `remove_overlay` is true also delete any
/// image placed inside the selection frame.
pub fn dismiss_session(state: &mut EditorState, remove_overlay: bool) {
    let placed = state
        .canvas_image_search
        .as_ref()
        .and_then(|s| s.placed_overlay);
    if remove_overlay {
        if let Some(idx) = placed {
            remove_overlay_at(state, idx);
        }
    }
    state.canvas_image_search = None;
    state.canvas_image_search_draft = None;
    state.canvas_image_search_rmb_pending = None;
}

fn remove_overlay_at(state: &mut EditorState, idx: usize) {
    if idx >= state.scene.overlays.len() {
        return;
    }
    state.mutate_state(|s| {
        s.scene.overlays.remove(idx);
    });
    shift_overlay_assignments_after_remove(&mut state.overlay_track_assignments, idx);
    state.canvas_selection.retain(|sel| {
        !matches!(sel, Selection::Overlay(i) if *i == idx)
    });
    if matches!(state.selection, Selection::Overlay(i) if i == idx) {
        state.selection = Selection::None;
    } else if let Selection::Overlay(i) = state.selection {
        if i > idx {
            state.selection = Selection::Overlay(i - 1);
        }
    }
}

fn shift_overlay_assignments_after_remove(
    map: &mut std::collections::HashMap<usize, usize>,
    removed: usize,
) {
    let mut new_map = std::collections::HashMap::with_capacity(map.len());
    for (k, v) in map.iter() {
        if *k == removed {
            continue;
        }
        let nk = if *k > removed { *k - 1 } else { *k };
        new_map.insert(nk, *v);
    }
    *map = new_map;
}

/// Cancel the active canvas image-search session (popup + frame + placed image).
pub fn cancel_canvas_image_search(state: &mut EditorState) {
    dismiss_session(state, true);
}

/// Render the inline search UI anchored to the canvas.
pub fn show_canvas_search_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) {
    let Some(session) = state.canvas_image_search.as_ref().filter(|s| s.ui_visible) else {
        return;
    };
    let Some(tx) = state.image_fx_tx.clone() else {
        return;
    };

    let screen = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos {
            x: session.ui_anchor_world[0],
            y: session.ui_anchor_world[1],
        },
        viewport_size,
    );
    let anchor_screen = Pos2::new(
        full_rect.min.x + screen[0],
        full_rect.min.y + screen[1],
    );
    let default_screen = [screen[0], screen[1]];

    let window_id = egui::Id::new("canvas_image_search_popup");
    let mut window = egui::Window::new(crate::i18n::t("Image search"))
        .id(window_id)
        .collapsible(false)
        .resizable(false)
        .title_bar(true)
        .max_width(420.0);

    if let Some([x, y]) = state
        .canvas_image_search
        .as_ref()
        .and_then(|s| s.ui_screen_pos)
    {
        window = window.current_pos(Pos2::new(full_rect.min.x + x, full_rect.min.y + y));
    } else if let Some(pos) = ctx.memory(|m| m.area_rect(window_id).map(|r| r.left_top())) {
        window = window.current_pos(pos);
    } else {
        window = window.current_pos(anchor_screen);
    }

    if let Some(inner) = window.show(ctx, |ui| {
        ui.set_max_width(420.0);
        draw_popup_body(ui, state, &tx, default_screen);
    }) {
        let top_left = inner.response.rect.left_top();
        if let Some(s) = state.canvas_image_search.as_mut() {
            s.ui_screen_pos = Some([
                top_left.x - full_rect.min.x,
                top_left.y - full_rect.min.y,
            ]);
        }
    }
}

fn draw_popup_body(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    tx: &Sender<JobEvent>,
    default_screen: [f32; 2],
) {
    let mut dismiss_all = false;
    let mut apply_ui = false;
    let mut kick_search_now = false;
    let mut re_search_transparent = false;
    let mut strip_settings_changed = false;
    let mut placed_idx = None::<usize>;
    let mut carousel_prev = false;
    let mut carousel_next_local = false;
    let mut carousel_next_fetch = false;
    let mut download_idx = None::<usize>;

    {
        let session = match state.canvas_image_search.as_mut() {
            Some(s) => s,
            None => return,
        };

        egui::Frame::none()
            .fill(Color32::from_rgba_premultiplied(24, 24, 20, 240))
            .rounding(Rounding::same(6.0))
            .stroke(Stroke::new(1.0, Color32::from_rgb(100, 180, 255)))
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("\u{2716}")
                            .on_hover_text(crate::i18n::t("Delete frame and image"))
                            .clicked()
                        {
                            dismiss_all = true;
                            placed_idx = session.placed_overlay;
                        }
                        if ui
                            .button(crate::i18n::t("Apply"))
                            .on_hover_text(crate::i18n::t("Close search and keep the frame"))
                            .clicked()
                        {
                            apply_ui = true;
                        }
                    });
                });

                ui.horizontal(|ui| {
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut session.query)
                            .hint_text(crate::i18n::t("Search images…"))
                            .desired_width(220.0),
                    );
                    if session.focus_input {
                        te.request_focus();
                        session.focus_input = false;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        kick_search_now = true;
                    }
                    if ui.button("\u{1F50D}").clicked() {
                        kick_search_now = true;
                    }
                });

                let prev_transparent = session.transparent_only;
                ui.checkbox(
                    &mut session.transparent_only,
                    crate::i18n::t("Transparent background only"),
                );
                if prev_transparent != session.transparent_only
                    && !session.last_sent_query.is_empty()
                    && !session.searching
                {
                    re_search_transparent = true;
                }

                if session.transparent_only {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(crate::i18n::t("Checkerboard removal"))
                            .size(11.0)
                            .strong(),
                    );
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::t("Color tolerance"));
                        if ui
                            .add(
                                egui::Slider::new(&mut session.checker_color_tolerance, 8..=48)
                                    .show_value(true),
                            )
                            .changed()
                        {
                            strip_settings_changed = true;
                        }
                    });
                    if ui
                        .checkbox(
                            &mut session.checker_edge_flood,
                            crate::i18n::t("Also remove light edges"),
                        )
                        .changed()
                    {
                        strip_settings_changed = true;
                    }
                    if session.checker_edge_flood {
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Edge brightness"));
                            if ui
                                .add(
                                    egui::Slider::new(
                                        &mut session.checker_edge_brightness,
                                        235..=255,
                                    )
                                    .show_value(true),
                                )
                                .changed()
                            {
                                strip_settings_changed = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(crate::i18n::t("Neutral tolerance"));
                            if ui
                                .add(
                                    egui::Slider::new(
                                        &mut session.checker_neutral_tolerance,
                                        8..=40,
                                    )
                                    .show_value(true),
                                )
                                .changed()
                            {
                                strip_settings_changed = true;
                            }
                        });
                    }
                }

                if session.searching {
                    ui.label(RichText::new(crate::i18n::t("Searching…")).small());
                } else if !session.status.is_empty() {
                    ui.label(RichText::new(&session.status).small().weak());
                }

                let start = session.carousel_start;
                let total = session.results.len();
                let can_prev = start >= CAROUSEL_PAGE_SIZE;
                let end = (start + CAROUSEL_PAGE_SIZE).min(total);
                let has_next_local = end < total;
                let has_next_remote = session.next_offset.is_some();
                let need_fetch = !has_next_local && has_next_remote && !session.searching;

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_prev, egui::Button::new("\u{25C0}"))
                        .clicked()
                    {
                        carousel_prev = true;
                    }

                    for idx in start..end {
                        let hit = session.results.get(idx).cloned();
                        let Some(hit) = hit else { continue; };
                        let thumb = 72.0;
                        let size = Vec2::splat(thumb);
                        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                        ui.painter().rect_filled(
                            rect,
                            Rounding::same(4.0),
                            Color32::from_rgb(18, 18, 14),
                        );
                        let img = egui::Image::from_uri(hit.thumbnail_url.clone())
                            .fit_to_exact_size(size)
                            .maintain_aspect_ratio(true)
                            .rounding(Rounding::same(4.0));
                        img.paint_at(ui, rect);
                        if hit.downloading {
                            ui.painter().rect_filled(
                                rect,
                                Rounding::same(4.0),
                                Color32::from_rgba_premultiplied(0, 0, 0, 140),
                            );
                        }
                        if resp.clicked() && !hit.downloading {
                            download_idx = Some(idx);
                        }
                    }

                    if ui
                        .add_enabled(has_next_local || has_next_remote, egui::Button::new("\u{25B6}"))
                        .clicked()
                    {
                        if has_next_local {
                            carousel_next_local = true;
                        } else if need_fetch {
                            carousel_next_fetch = true;
                        }
                    }
                });
            });
    }

    let _ = default_screen;

    if dismiss_all {
        if let Some(idx) = placed_idx {
            remove_overlay_at(state, idx);
        }
        state.canvas_image_search = None;
        return;
    }
    if apply_ui {
        hide_canvas_search_ui(state);
    }
    if kick_search_now || re_search_transparent {
        kick_canvas_search(state, tx, false);
    }
    if carousel_next_fetch {
        kick_canvas_search(state, tx, true);
    }
    if carousel_prev {
        if let Some(s) = state.canvas_image_search.as_mut() {
            s.carousel_start = s.carousel_start.saturating_sub(CAROUSEL_PAGE_SIZE);
        }
    }
    if carousel_next_local {
        if let Some(s) = state.canvas_image_search.as_mut() {
            s.carousel_start += CAROUSEL_PAGE_SIZE;
        }
    }
    if strip_settings_changed {
        if let Some(s) = state.canvas_image_search.as_ref() {
            if s.transparent_only {
                placed_idx = s.placed_overlay;
            }
        }
    }
    if let Some(idx) = placed_idx {
        if state
            .canvas_image_search
            .as_ref()
            .is_some_and(|s| s.transparent_only && s.placed_overlay == Some(idx))
        {
            if let Err(e) = reapply_checkerboard_strip(state, idx) {
                if let Some(s) = state.canvas_image_search.as_mut() {
                    s.status = e;
                }
            }
        }
    }
    if let Some(idx) = download_idx {
        kick_canvas_download(state, tx, idx);
    }
}

pub fn kick_canvas_search(state: &mut EditorState, tx: &Sender<JobEvent>, append: bool) {
    let session = match state.canvas_image_search.as_mut() {
        Some(s) => s,
        None => return,
    };
    let q = session.query.trim().to_string();
    if q.is_empty() {
        session.status = crate::i18n::t("Type a search query first.").to_string();
        return;
    }
    if session.searching {
        return;
    }
    let Some(handle) = state.tokio_handle.clone() else {
        session.status = crate::i18n::t("Tokio runtime missing — search disabled.").to_string();
        return;
    };

    let offset = if append {
        match session.next_offset {
            Some(o) => o,
            None => return,
        }
    } else {
        0
    };

    let aspect = session.target_aspect_ratio();
    let transparent = session.transparent_only;

    if !append {
        session.last_sent_query = q.clone();
        session.results.clear();
        session.carousel_start = 0;
        session.next_offset = None;
        session.page_count = 0;
        session.status = format!(
            "{} \"{}\"…",
            crate::i18n::t("\u{1F50D} Searching for"),
            q
        );
    } else {
        session.status = format!(
            "{} {}…",
            crate::i18n::t("\u{2B07} Loading page"),
            session.page_count + 1
        );
    }
    session.searching = true;

    spawn_web_image_search(
        &handle,
        tx.clone(),
        q,
        offset,
        WebSearchTarget::Canvas,
        WebImageSearchOptions {
            aspect_ratio: aspect,
            transparent_only: transparent,
        },
    );
}

fn kick_canvas_download(state: &mut EditorState, tx: &Sender<JobEvent>, hit_idx: usize) {
    let dest = state.images_dir();
    let handle = state.tokio_handle.clone();
    let session = match state.canvas_image_search.as_mut() {
        Some(s) => s,
        None => return,
    };
    if hit_idx >= session.results.len() {
        return;
    }
    if session.results[hit_idx].downloading {
        return;
    }
    session.results[hit_idx].downloading = true;
    session.results[hit_idx].last_error = None;

    let hit = session.results[hit_idx].clone();
    state.library_tab = LibraryTab::Images;

    let Some(handle) = handle else {
        session.results[hit_idx].downloading = false;
        session.results[hit_idx].last_error =
            Some(crate::i18n::t("Tokio runtime missing").to_string());
        return;
    };

    let request_id = session.next_request_id;
    session.next_request_id = session.next_request_id.wrapping_add(1);

    spawn_web_image_download(
        &handle,
        tx.clone(),
        hit.image_url.clone(),
        hit.title.clone(),
        dest,
        request_id,
        false,
    );
}

/// Apply a finished web search to the active canvas session.
pub fn ingest_canvas_search_result(
    state: &mut EditorState,
    page_offset: u32,
    result: Result<(Vec<WebImageHit>, Option<u32>), String>,
) {
    let Some(session) = state.canvas_image_search.as_mut() else {
        return;
    };
    session.searching = false;
    match result {
        Ok((mut hits, next_offset)) => {
            let is_first = page_offset == 0;
            if is_first {
                if let Some(ar) = session.target_aspect_ratio() {
                    hits.sort_by(|a, b| {
                        let ra = a.width as f32 / a.height.max(1) as f32;
                        let rb = b.width as f32 / b.height.max(1) as f32;
                        let da = (ra - ar).abs();
                        let db = (rb - ar).abs();
                        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                session.results = hits;
            } else {
                session.results.extend(hits);
            }
            session.next_offset = next_offset.filter(|&o| o != page_offset);
            if !is_first {
                session.page_count += 1;
                session.carousel_start = session
                    .results
                    .len()
                    .saturating_sub(CAROUSEL_PAGE_SIZE);
            }
            let n = session.results.len();
            session.status = if n == 0 {
                crate::i18n::t("\u{1F50D} No results.").to_string()
            } else {
                format!("{} {}.", crate::i18n::t("\u{2705} Got"), n)
            };
        }
        Err(e) => {
            if page_offset == 0 {
                session.results.clear();
            }
            session.next_offset = None;
            session.status = format!("\u{274C} {}", e);
        }
    }
}

/// Mark download state on canvas session hits and place the image.
pub fn ingest_canvas_download_result(
    state: &mut EditorState,
    image_url: &str,
    result: &Result<LibraryAsset, String>,
) {
    let Some(session) = state.canvas_image_search.as_mut() else {
        return;
    };
    for hit in session.results.iter_mut() {
        if hit.image_url == image_url {
            hit.downloading = false;
            match result {
                Ok(asset) => {
                    hit.local_path = Some(asset.path.clone());
                    hit.last_error = None;
                }
                Err(e) => {
                    hit.last_error = Some(e.clone());
                }
            }
            break;
        }
    }

    let Ok(asset) = result else {
        if let Some(s) = state.canvas_image_search.as_mut() {
            if let Err(e) = result {
                s.status = e.clone();
            }
        }
        return;
    };

    let Some(session_ref) = state.canvas_image_search.as_ref() else {
        return;
    };
    let rect = session_ref.selection_rect();
    let popup_anchor = session_ref.ui_anchor_world;
    let popup_only = matches!(session_ref.mode, CanvasImageSearchMode::PopupOnly);
    let existing = session_ref.placed_overlay;
    let strip_checker = session_ref.transparent_only;
    let strip_settings = CheckerboardStripSettings::from_session(session_ref);

    let mut asset = asset.clone();
    if strip_checker {
        if let Some(session) = state.canvas_image_search.as_mut() {
            // Always track the latest download's raw file — re-strip must
            // not keep reusing the first image placed in this session.
            session.strip_source_path = Some(asset.path.clone());
        }
        match strip_fake_checkerboard(&asset.path, strip_settings) {
            Ok(out_path) => {
                asset.path = out_path;
            }
            Err(e) => {
                if let Some(s) = state.canvas_image_search.as_mut() {
                    s.status = format!("{} {}", crate::i18n::t("Checkerboard strip:"), e);
                }
                return;
            }
        }
    }

    if let Some(idx) = existing {
        replace_overlay_image(state, idx, asset.clone());
        if let (Some(rect), Some(dims)) = (rect, image_dimensions(&asset.path)) {
            fit_overlay_to_selection_rect(state, idx, rect, dims);
        } else if popup_only {
            if let Some(dims) = image_dimensions(&asset.path) {
                place_overlay_at_world_point(state, idx, popup_anchor, dims);
            }
        }
    } else {
        let idx = state.add_image_overlay_at_playhead(&asset);
        if let Some(session) = state.canvas_image_search.as_mut() {
            session.placed_overlay = Some(idx);
        }
        if let Some(rect) = rect {
            if let Some(dims) = image_dimensions(&asset.path) {
                fit_overlay_to_selection_rect(state, idx, rect, dims);
            }
        } else if popup_only {
            if let Some(dims) = image_dimensions(&asset.path) {
                place_overlay_at_world_point(state, idx, popup_anchor, dims);
            }
        }
    }
}

fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

fn replace_overlay_image(state: &mut EditorState, idx: usize, asset: LibraryAsset) {
    state.mutate_state(|s| {
        if let Some(Overlay::Image(im)) = s.scene.overlays.get_mut(idx) {
            im.source = asset.path.clone();
            im.effects.retain(|e| !matches!(e.kind, EffectKind::Crop { .. }));
        }
    });
    if let Ok(mut map) = state.image_textures.lock() {
        map.remove(&asset.path);
    }
}

/// Cover-fit an image overlay into a world-pixel selection rectangle.
pub fn fit_overlay_to_selection_rect(
    state: &mut EditorState,
    overlay_idx: usize,
    rect: ([f32; 2], [f32; 2]),
    img_dims: (u32, u32),
) {
    let (mn, mx) = rect;
    let rect_w = (mx[0] - mn[0]).abs().max(1.0);
    let rect_h = (mx[1] - mn[1]).abs().max(1.0);
    let (img_w, img_h) = (img_dims.0 as f32, img_dims.1 as f32);
    if img_w < 1.0 || img_h < 1.0 {
        return;
    }

    let scale = (rect_w / img_w).max(rect_h / img_h);
    let disp_w = img_w * scale;
    let disp_h = img_h * scale;

    let crop_l = ((disp_w - rect_w) / (2.0 * disp_w)).clamp(0.0, 0.49);
    let crop_r = crop_l;
    let crop_t = ((disp_h - rect_h) / (2.0 * disp_h)).clamp(0.0, 0.49);
    let crop_b = crop_t;

    let cx = (mn[0] + mx[0]) * 0.5;
    let cy = (mn[1] + mx[1]) * 0.5;
    let [rw, rh] = state.scene.render_frame.resolution;
    let norm_pos = [cx / rw as f32, cy / rh as f32];

    state.mutate_state(|s| {
        upsert_overlay_world_center(&mut s.scene, overlay_idx, [cx, cy], norm_pos);
        let Some(Overlay::Image(im)) = s.scene.overlays.get_mut(overlay_idx) else {
            return;
        };
        if let Some(kf) = im.layout.first_mut() {
            kf.value.scale = scale;
            kf.value.scale_y = 1.0;
        }
        im.effects.retain(|e| !matches!(e.kind, EffectKind::Crop { .. }));
        im.effects.push(Effect::new(EffectKind::Crop {
            left: crop_l,
            top: crop_t,
            right: crop_r,
            bottom: crop_b,
        }));
    });
    state.selection = Selection::Overlay(overlay_idx);
}

/// Write world-pixel centre into `canvas_layouts` (preferred by
/// `get_element_world_pos`) and keep the normalised layout track in sync.
fn upsert_overlay_world_center(
    scene: &mut memstroy_core::Scene,
    overlay_idx: usize,
    world_center: [f32; 2],
    norm_pos: [f32; 2],
) {
    let overlay_id = match scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) => im.id.clone(),
        _ => return,
    };
    let [rw, rh] = scene.render_frame.resolution;
    let world_w = rw as f32;
    let world_h = rh as f32;
    if world_w <= 0.0 || world_h <= 0.0 {
        return;
    }

    if let Some(Overlay::Image(im)) = scene.overlays.get_mut(overlay_idx) {
        if let Some(kf) = im.layout.first_mut() {
            kf.value.pos = norm_pos;
        }
    }

    let world_pos = WorldPos {
        x: world_center[0],
        y: world_center[1],
    };
    if let Some(cl) = scene
        .canvas_layouts
        .iter_mut()
        .find(|cl| cl.element_id == overlay_id)
    {
        if let Some(kf) = cl.keyframes.first_mut() {
            kf.value.pos = world_pos;
        } else {
            cl.keyframes.push(Keyframe::new(
                0.0,
                CanvasTransform {
                    pos: world_pos,
                    ..Default::default()
                },
            ));
        }
    } else {
        scene.canvas_layouts.push(CanvasLayout {
            element_id: overlay_id,
            keyframes: vec![Keyframe::new(
                0.0,
                CanvasTransform {
                    pos: world_pos,
                    ..Default::default()
                },
            )],
        });
    }
}

/// Place a newly added image overlay at a world-pixel point (PopupOnly RMB).
pub fn place_overlay_at_world_point(
    state: &mut EditorState,
    overlay_idx: usize,
    world_pos: [f32; 2],
    img_dims: (u32, u32),
) {
    let [rw, rh] = state.scene.render_frame.resolution;
    let norm_pos = [
        (world_pos[0] / rw as f32).clamp(0.0, 1.0),
        (world_pos[1] / rh as f32).clamp(0.0, 1.0),
    ];
    let _ = img_dims;
    state.mutate_state(|s| {
        upsert_overlay_world_center(&mut s.scene, overlay_idx, world_pos, norm_pos);
        if let Some(Overlay::Image(im)) = s.scene.overlays.get_mut(overlay_idx) {
            if let Some(kf) = im.layout.first_mut() {
                kf.value.scale = 1.0;
                kf.value.scale_y = 1.0;
            }
            im.effects.retain(|e| !matches!(e.kind, EffectKind::Crop { .. }));
        }
    });
    state.selection = Selection::Overlay(overlay_idx);
}

/// Re-run checkerboard removal on an already placed web-search image.
pub fn reapply_checkerboard_strip(state: &mut EditorState, overlay_idx: usize) -> Result<(), String> {
    let settings = state
        .canvas_image_search
        .as_ref()
        .map(CheckerboardStripSettings::from_session)
        .unwrap_or_default();
    let source = state
        .canvas_image_search
        .as_ref()
        .and_then(|s| s.strip_source_path.clone())
        .filter(|p| p.is_file())
        .ok_or_else(|| crate::i18n::t("Image file not ready yet.").to_string())?;

    let out_path = strip_fake_checkerboard(&source, settings)?;
    let asset = LibraryAsset {
        id: source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image")
            .to_string(),
        path: out_path.clone(),
        label: source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image")
            .to_string(),
        thumbnail: None,
    };
    replace_overlay_image(state, overlay_idx, asset);
    if let Some(session) = state.canvas_image_search.as_ref() {
        if let Some(rect) = session.selection_rect() {
            if let Some(dims) = image_dimensions(&out_path) {
                fit_overlay_to_selection_rect(state, overlay_idx, rect, dims);
            }
        } else if matches!(session.mode, CanvasImageSearchMode::PopupOnly) {
            if let Some(dims) = image_dimensions(&out_path) {
                place_overlay_at_world_point(state, overlay_idx, session.ui_anchor_world, dims);
            }
        }
    }
    if let Some(s) = state.canvas_image_search.as_mut() {
        s.status = crate::i18n::t("Checkerboard strip updated.").into();
    }
    Ok(())
}

fn kick_ai_background_remove(state: &mut EditorState, tx: &Sender<JobEvent>, overlay_idx: usize) {
    let set_status = |state: &mut EditorState, msg: String| {
        if let Some(s) = state.canvas_image_search.as_mut() {
            s.status = msg.clone();
        }
        state.status = msg;
    };

    let model = memstroy_vision::resolve_u2netp_model_path();
    if !model.exists() {
        set_status(state, memstroy_vision::u2netp_missing_message(&model));
        return;
    }
    let handle = state.tokio_handle.clone();
    let path = match state.scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) if !im.source.as_os_str().is_empty() => im.source.clone(),
        _ => {
            set_status(state, crate::i18n::t("Image file not ready yet.").into());
            return;
        }
    };
    let mask_polygon = state
        .scene
        .overlays
        .get(overlay_idx)
        .and_then(|o| match o {
            Overlay::Image(im) => extract_overlay_mask_polygon(im),
            _ => None,
        });
    if mask_polygon.as_ref().map(|p| p.len()).unwrap_or(0) < 3 {
        set_status(
            state,
            crate::i18n::t("Draw a brush selection around the subject first (Brush select).").into(),
        );
        return;
    }
    let ai_busy = state
        .canvas_image_search
        .as_ref()
        .is_some_and(|s| s.ai_removing);
    if ai_busy {
        return;
    }
    let Some(handle) = handle else {
        set_status(state, crate::i18n::t("Tokio runtime missing.").into());
        return;
    };
    if let Some(s) = state.canvas_image_search.as_mut() {
        s.ai_removing = true;
        s.status = crate::i18n::t("Removing background…").into();
    }
    state.status = crate::i18n::t("Removing background…").into();
    spawn_ai_background_remove(
        &handle,
        tx.clone(),
        overlay_idx,
        path,
        model,
        mask_polygon,
    );
}

pub fn ingest_ai_bg_remove_result(
    state: &mut EditorState,
    overlay_idx: usize,
    path: &Path,
    result: Result<(), String>,
) {
    let status = match &result {
        Ok(()) => crate::i18n::t("Background removed.").into(),
        Err(e) => format!("\u{274C} {}", e),
    };
    if let Some(session) = state.canvas_image_search.as_mut() {
        session.ai_removing = false;
        session.status = status.clone();
    }
    state.status = status;
    if result.is_ok() {
        if let Ok(mut map) = state.image_textures.lock() {
            map.remove(path);
        }
        state.mutate_state(|s| {
            if let Some(Overlay::Image(im)) = s.scene.overlays.get_mut(overlay_idx) {
                im.effects.retain(|e| !matches!(e.kind, EffectKind::Mask { .. }));
            }
        });
    }
}

fn extract_overlay_mask_polygon(im: &memstroy_core::ImageOverlay) -> Option<Vec<[f32; 2]>> {
    for eff in im.effects.iter().rev() {
        if !eff.enabled {
            continue;
        }
        if let EffectKind::Mask {
            shape: MaskShape::Polygon { points },
            invert: false,
            ..
        } = &eff.kind
        {
            if points.len() >= 3 {
                return Some(points.clone());
            }
        }
    }
    None
}

/// Remove stock-photo fake transparency (gray/white checkerboard).
/// JPEG sources are written to a sibling `.png` path because JPEG
/// cannot store an alpha channel.
pub fn strip_fake_checkerboard(
    path: &Path,
    settings: CheckerboardStripSettings,
) -> Result<PathBuf, String> {
    let mut rgba = image::open(path)
        .map_err(|e| format!("open: {e}"))?
        .to_rgba8();
    strip_checkerboard_in_place(&mut rgba, settings);

    let out_path = if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
    {
        path.with_extension("png")
    } else {
        path.to_path_buf()
    };
    rgba.save(&out_path).map_err(|e| format!("save: {e}"))?;
    Ok(out_path)
}

fn strip_checkerboard_in_place(rgba: &mut RgbaImage, settings: CheckerboardStripSettings) {
    let (w, h) = rgba.dimensions();
    if w < 4 || h < 4 {
        return;
    }
    let (c1, c2) = detect_checker_colors(rgba);
    let Some(c1) = c1 else { return; };

    // Only remove checker colours connected to the image border — avoids
    // punching holes through light subject pixels in the interior.
    flood_checker_colors_from_edges(rgba, c1, c2, settings.color_tolerance);

    if settings.edge_flood {
        flood_light_neutral_from_edges(
            rgba,
            settings.edge_brightness,
            settings.neutral_tolerance,
        );
    }
}

fn flood_checker_colors_from_edges(
    rgba: &mut RgbaImage,
    c1: [u8; 3],
    c2: Option<[u8; 3]>,
    tol: u8,
) {
    let (w, h) = rgba.dimensions();
    let mut queue: std::collections::VecDeque<(u32, u32)> = std::collections::VecDeque::new();
    let mut visited = vec![false; (w * h) as usize];

    let matches_checker = |rgb: [u8; 3]| -> bool {
        color_near(rgb, c1, tol) || c2.map(|c| color_near(rgb, c, tol)).unwrap_or(false)
    };

    let mut seed = |x: u32, y: u32| {
        let i = (y * w + x) as usize;
        if visited[i] {
            return;
        }
        let p = rgba.get_pixel(x, y);
        if p[3] < 8 {
            visited[i] = true;
            queue.push_back((x, y));
            return;
        }
        if matches_checker([p[0], p[1], p[2]]) {
            visited[i] = true;
            queue.push_back((x, y));
        }
    };

    for x in 0..w {
        seed(x, 0);
        seed(x, h - 1);
    }
    for y in 0..h {
        seed(0, y);
        seed(w - 1, y);
    }

    while let Some((x, y)) = queue.pop_front() {
        rgba.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        for (nx, ny) in [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ] {
            if nx >= w || ny >= h {
                continue;
            }
            let i = (ny * w + nx) as usize;
            if visited[i] {
                continue;
            }
            let p = rgba.get_pixel(nx, ny);
            if p[3] < 8 {
                visited[i] = true;
                queue.push_back((nx, ny));
                continue;
            }
            if matches_checker([p[0], p[1], p[2]]) {
                visited[i] = true;
                queue.push_back((nx, ny));
            }
        }
    }
}

fn detect_checker_colors(rgba: &RgbaImage) -> (Option<[u8; 3]>, Option<[u8; 3]>) {
    let (w, h) = rgba.dimensions();
    let strip = 12u32.min(w / 4).min(h / 4).max(2);
    let mut samples: Vec<[u8; 3]> = Vec::new();
    for y in 0..strip {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            if p[3] > 200 {
                samples.push([p[0], p[1], p[2]]);
            }
        }
    }
    for y in h.saturating_sub(strip)..h {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            if p[3] > 200 {
                samples.push([p[0], p[1], p[2]]);
            }
        }
    }
    if samples.len() < 8 {
        return (None, None);
    }
    samples.sort_by_key(|c| c[0] as u16 + c[1] as u16 + c[2] as u16);
    let mid = samples.len() / 2;
    let c1 = samples[mid];
    let mut c2 = None;
    for c in &samples {
        if color_dist(*c, c1) > 18.0 {
            c2 = Some(*c);
            break;
        }
    }
    let sum = c1[0] as u16 + c1[1] as u16 + c1[2] as u16;
    if sum < 380 {
        return (None, None);
    }
    (Some(c1), c2)
}

fn color_near(a: [u8; 3], b: [u8; 3], tol: u8) -> bool {
    a[0].abs_diff(b[0]) <= tol
        && a[1].abs_diff(b[1]) <= tol
        && a[2].abs_diff(b[2]) <= tol
}

fn color_dist(a: [u8; 3], b: [u8; 3]) -> f32 {
    let dr = a[0] as f32 - b[0] as f32;
    let dg = a[1] as f32 - b[1] as f32;
    let db = a[2] as f32 - b[2] as f32;
    (dr * dr + dg * dg + db * db).sqrt()
}

fn flood_light_neutral_from_edges(rgba: &mut RgbaImage, min_brightness: u8, tol: u8) {
    let (w, h) = rgba.dimensions();
    let mut queue: std::collections::VecDeque<(u32, u32)> = std::collections::VecDeque::new();
    let mut visited = vec![false; (w * h) as usize];

    let seed = |x: u32, y: u32, q: &mut std::collections::VecDeque<(u32, u32)>, vis: &mut [bool]| {
        let i = (y * w + x) as usize;
        if vis[i] {
            return;
        }
        let p = rgba.get_pixel(x, y);
        if p[3] < 8 {
            return;
        }
        let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
        let neutral = p[0].abs_diff(p[1]) <= tol
            && p[1].abs_diff(p[2]) <= tol
            && bright >= min_brightness as u16;
        if neutral || (p[0] > min_brightness && p[1] > min_brightness && p[2] > min_brightness) {
            vis[i] = true;
            q.push_back((x, y));
        }
    };

    for x in 0..w {
        seed(x, 0, &mut queue, &mut visited);
        seed(x, h - 1, &mut queue, &mut visited);
    }
    for y in 0..h {
        seed(0, y, &mut queue, &mut visited);
        seed(w - 1, y, &mut queue, &mut visited);
    }

    while let Some((x, y)) = queue.pop_front() {
        rgba.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        for (nx, ny) in [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ] {
            if nx >= w || ny >= h {
                continue;
            }
            let i = (ny * w + nx) as usize;
            if visited[i] {
                continue;
            }
            let p = rgba.get_pixel(nx, ny);
            if p[3] < 8 {
                continue;
            }
            let bright = (p[0] as u16 + p[1] as u16 + p[2] as u16) / 3;
            if bright >= min_brightness as u16 {
                visited[i] = true;
                queue.push_back((nx, ny));
            }
        }
    }
}

/// True when RMB selection-frame gesture is active (blocks canvas pan).
pub fn rmb_gesture_active(state: &EditorState) -> bool {
    state.canvas_image_search_draft.is_some()
        || state.canvas_image_search_rmb_pending.is_some()
}
