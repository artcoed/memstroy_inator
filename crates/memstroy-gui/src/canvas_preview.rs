//! Free Canvas preview panel — replaces the old fixed 9:16 preview.
//!
//! Renders an infinite 2D canvas with pan/zoom, the render frame
//! rectangle, and all scene elements positioned in world pixels.

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_CANVAS_BG: Color32 = Color32::from_rgb(28, 28, 24);
const COL_GRID_MINOR: Color32 = Color32::from_rgb(58, 56, 40);
const COL_GRID_MAJOR: Color32 = Color32::from_rgb(90, 85, 50);
const COL_RENDER_FRAME: Color32 = Color32::from_rgb(255, 80, 80);
const COL_RENDER_FRAME_FILL: Color32 = Color32::from_rgba_premultiplied(255, 80, 80, 8);
const COL_ELEMENT_BORDER: Color32 = Color32::from_rgb(180, 180, 200);
const COL_SELECTED_BORDER: Color32 = Color32::from_rgb(255, 220, 80);
const COL_INACTIVE_TINT: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 100);
const COL_OVERLAY_TEXT: Color32 = Color32::from_rgb(80, 200, 120);
const COL_OVERLAY_IMAGE: Color32 = Color32::from_rgb(100, 180, 255);
const COL_OVERLAY_VIDEO: Color32 = Color32::from_rgb(200, 100, 255);
const COL_BACKGROUND: Color32 = Color32::from_rgb(60, 130, 220);
const COL_RENDER_FRAME_HANDLE: Color32 = Color32::from_rgb(255, 120, 120);


// ─── MAIN ENTRY POINT ────────────────────────────────────────────────

/// Render the free canvas preview panel. Replaces `panels::preview()`.
pub fn canvas_preview(ui: &mut egui::Ui, state: &mut EditorState) {
    let avail = ui.available_size_before_wrap();
    let (full_rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());

    let painter = ui.painter_at(full_rect);
    let viewport_size = [full_rect.width(), full_rect.height()];

    // ── Background ──
    painter.rect_filled(full_rect, Rounding::ZERO, COL_CANVAS_BG);

    // ── Handle pan/zoom input ──
    handle_canvas_input(ui, &response, state, viewport_size, full_rect);

    // ── Draw grid ──
    draw_canvas_grid(&painter, full_rect, &state.canvas_viewport, viewport_size);

    // ── Draw render frame ──
    draw_render_frame(&painter, full_rect, state, viewport_size);

    // ── Draw elements (actors, overlays) ──
    draw_canvas_elements(ui, &painter, full_rect, state, viewport_size);

    // ── Draw element gizmo for selected ──
    draw_selection_gizmo(ui, &painter, &response, full_rect, state, viewport_size);

    // ── Fit button overlay ──
    draw_viewport_controls(ui, full_rect, state, viewport_size);
}


// ─── INPUT HANDLING ──────────────────────────────────────────────────

fn handle_canvas_input(
    ui: &mut egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    viewport_size: [f32; 2],
    _full_rect: Rect,
) {
    // Any drag (left, middle, or right mouse button) → pan the canvas
    // unless an element is selected and being moved (handled in gizmo)
    let middle_down = ui.input(|i| i.pointer.middle_down());
    let right_down = ui.input(|i| i.pointer.secondary_down());

    // Pan: middle mouse drag, right mouse drag, or left drag when nothing is selected
    let should_pan = middle_down
        || right_down
        || (response.dragged() && state.selection == Selection::None);

    if should_pan && response.hovered() {
        let delta = response.drag_delta();
        if delta.length_sq() > 0.0 {
            state.canvas_viewport.pan([delta.x, delta.y]);
            state.canvas_panning = true;
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    } else {
        state.canvas_panning = false;
    }

    // Scroll wheel → zoom only (always, Ctrl not required for scroll-to-zoom)
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);

        if scroll.y.abs() > 0.1 {
            // Zoom towards mouse position
            let factor = if scroll.y > 0.0 { 1.03 } else { 1.0 / 1.03 };
            if let Some(mouse) = ui.input(|i| i.pointer.hover_pos()) {
                let local = [mouse.x - _full_rect.min.x, mouse.y - _full_rect.min.y];
                state.canvas_viewport.zoom_at(local, viewport_size, factor);
            }
        }
    }
}


// ─── GRID ────────────────────────────────────────────────────────────

fn draw_canvas_grid(
    painter: &egui::Painter,
    full_rect: Rect,
    viewport: &EditorViewport,
    viewport_size: [f32; 2],
) {
    // Determine grid spacing based on zoom level
    let base_spacing = choose_grid_spacing(viewport.zoom);
    let major_every = 5;

    let world_tl = viewport.screen_to_world([0.0, 0.0], viewport_size);
    let world_br = viewport.screen_to_world(viewport_size, viewport_size);

    // Vertical lines
    let x_start = (world_tl.x / base_spacing).floor() as i32;
    let x_end = (world_br.x / base_spacing).ceil() as i32;
    for ix in x_start..=x_end {
        let wx = ix as f32 * base_spacing;
        let screen = viewport.world_to_screen(WorldPos { x: wx, y: 0.0 }, viewport_size);
        let sx = full_rect.min.x + screen[0];
        if sx < full_rect.min.x || sx > full_rect.max.x { continue; }
        let col = if ix % major_every == 0 { COL_GRID_MAJOR } else { COL_GRID_MINOR };
        painter.line_segment(
            [Pos2::new(sx, full_rect.min.y), Pos2::new(sx, full_rect.max.y)],
            Stroke::new(0.5, col),
        );
    }

    // Horizontal lines
    let y_start = (world_tl.y / base_spacing).floor() as i32;
    let y_end = (world_br.y / base_spacing).ceil() as i32;
    for iy in y_start..=y_end {
        let wy = iy as f32 * base_spacing;
        let screen = viewport.world_to_screen(WorldPos { x: 0.0, y: wy }, viewport_size);
        let sy = full_rect.min.y + screen[1];
        if sy < full_rect.min.y || sy > full_rect.max.y { continue; }
        let col = if iy % major_every == 0 { COL_GRID_MAJOR } else { COL_GRID_MINOR };
        painter.line_segment(
            [Pos2::new(full_rect.min.x, sy), Pos2::new(full_rect.max.x, sy)],
            Stroke::new(0.5, col),
        );
    }
}

fn choose_grid_spacing(zoom: f32) -> f32 {
    // Target ~40-80 screen pixels between grid lines
    let target_screen_px = 60.0;
    let raw = target_screen_px / zoom;
    // Snap to nice round values
    if raw < 25.0 { 20.0 }
    else if raw < 60.0 { 50.0 }
    else if raw < 125.0 { 100.0 }
    else if raw < 300.0 { 200.0 }
    else if raw < 600.0 { 500.0 }
    else { 1000.0 }
}


// ─── RENDER FRAME RECTANGLE ──────────────────────────────────────────

fn draw_render_frame(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let [rw, rh] = rf.resolution;

    // The render frame covers (rw/zoom) x (rh/zoom) world pixels
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;

    // Top-left and bottom-right in world coords
    let tl_world = WorldPos {
        x: rf_state.pos.x - world_w * 0.5,
        y: rf_state.pos.y - world_h * 0.5,
    };
    let br_world = WorldPos {
        x: rf_state.pos.x + world_w * 0.5,
        y: rf_state.pos.y + world_h * 0.5,
    };

    let tl_screen = state.canvas_viewport.world_to_screen(tl_world, viewport_size);
    let br_screen = state.canvas_viewport.world_to_screen(br_world, viewport_size);

    let frame_rect = Rect::from_min_max(
        Pos2::new(full_rect.min.x + tl_screen[0], full_rect.min.y + tl_screen[1]),
        Pos2::new(full_rect.min.x + br_screen[0], full_rect.min.y + br_screen[1]),
    );

    // Border only (no fill inside the render frame)
    painter.rect_stroke(frame_rect, Rounding::ZERO, Stroke::new(2.0, COL_RENDER_FRAME));

    // Dim area outside the render frame (vignette effect)
    let dim = Color32::from_rgba_premultiplied(0, 0, 0, 120);
    // Top band
    if frame_rect.min.y > full_rect.min.y {
        painter.rect_filled(
            Rect::from_min_max(full_rect.min, Pos2::new(full_rect.max.x, frame_rect.min.y)),
            Rounding::ZERO, dim,
        );
    }
    // Bottom band
    if frame_rect.max.y < full_rect.max.y {
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(full_rect.min.x, frame_rect.max.y), full_rect.max),
            Rounding::ZERO, dim,
        );
    }
    // Left band (between top and bottom bands)
    if frame_rect.min.x > full_rect.min.x {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(full_rect.min.x, frame_rect.min.y),
                Pos2::new(frame_rect.min.x, frame_rect.max.y),
            ),
            Rounding::ZERO, dim,
        );
    }
    // Right band
    if frame_rect.max.x < full_rect.max.x {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(frame_rect.max.x, frame_rect.min.y),
                Pos2::new(full_rect.max.x, frame_rect.max.y),
            ),
            Rounding::ZERO, dim,
        );
    }

    // Label
    let label_pos = Pos2::new(frame_rect.min.x + 4.0, frame_rect.min.y - 16.0);
    if label_pos.y > full_rect.min.y {
        painter.text(
            label_pos, egui::Align2::LEFT_BOTTOM,
            format!("{}x{}", rw, rh),
            egui::FontId::proportional(10.0),
            COL_RENDER_FRAME,
        );
    }
}

/// Sample the RenderFrame state at time t.
fn sample_render_frame(rf: &RenderFrame, t: f32) -> RenderFrameState {
    keyframe::sample(&rf.layout, t).unwrap_or_default()
}


// ─── ELEMENTS ON CANVAS ──────────────────────────────────────────────

fn draw_canvas_elements(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    let t = state.playhead;
    let duration = state.scene.output.duration;

    // Draw backgrounds first (bottom layer)
    draw_canvas_backgrounds(painter, full_rect, state, viewport_size);

    // Draw actors
    for (idx, actor) in state.scene.actors.iter().enumerate() {
        if !actor.visible { continue; }

        let t_in = actor.t_in.unwrap_or(0.0);
        let t_out = actor.t_out.unwrap_or(duration);

        // Determine display mode: active, before-start (first frame), after-end (last frame)
        let display_mode = if t >= t_in && t <= t_out {
            DisplayMode::Active
        } else if t < t_in {
            DisplayMode::BeforeStart // show first frame
        } else {
            DisplayMode::AfterEnd // show last frame
        };

        // Get world position from canvas_layouts or legacy layout
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        // Use actual source dimensions from frame cache, or default 480x270 (16:9)
        let (elem_width, elem_height) = if let Some(fc) = state.frame_caches.get(idx) {
            if fc.is_ready() && fc.frame_count > 0 {
                // Frame cache extracts at 480px width, source aspect preserved
                (480.0_f32, 480.0 * 16.0 / 9.0) // vertical video default
            } else {
                (400.0, 400.0 * 16.0 / 9.0)
            }
        } else {
            (400.0, 400.0 * 16.0 / 9.0)
        };
        // Apply actor scale from layout
        let actor_scale = keyframe::sample(&actor.layout, t)
            .map(|s| s.scale).unwrap_or(1.0);
        let elem_width = elem_width * actor_scale;
        let elem_height = elem_height * actor_scale;

        // Convert to screen coordinates
        let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
        let half_w = elem_width * 0.5 * state.canvas_viewport.zoom;
        let half_h = elem_height * 0.5 * state.canvas_viewport.zoom;

        let elem_rect = Rect::from_center_size(
            Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
            Vec2::new(half_w * 2.0, half_h * 2.0),
        );

        // Skip if fully offscreen
        if !full_rect.intersects(elem_rect) { continue; }

        // Draw the element placeholder (frame from cache if available)
        let tint = match display_mode {
            DisplayMode::Active => Color32::WHITE,
            _ => COL_INACTIVE_TINT,
        };

        // Try to show actual frame from cache
        let local_t = match display_mode {
            DisplayMode::Active => t - t_in + actor.source_start,
            DisplayMode::BeforeStart => actor.source_start, // first frame
            DisplayMode::AfterEnd => {
                // last frame: source_start + (t_out - t_in)
                actor.source_start + (t_out - t_in)
            }
        };

        let mut frame_shown = false;
        if let Some(fc) = state.frame_caches.get_mut(idx) {
            if fc.is_ready() {
                if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                    painter.image(tex.id(), elem_rect, uv, tint);
                    frame_shown = true;
                }
            }
        }

        if !frame_shown {
            // Placeholder rectangle
            let fill = match display_mode {
                DisplayMode::Active => Color32::from_rgb(40, 40, 55),
                _ => Color32::from_rgb(30, 30, 40),
            };
            painter.rect_filled(elem_rect, Rounding::same(3.0), fill);
            painter.text(
                elem_rect.center(), egui::Align2::CENTER_CENTER,
                &actor.id, egui::FontId::proportional(10.0),
                Color32::from_rgb(140, 140, 160),
            );
        }

        // Border
        let border_col = if state.selection == Selection::Actor(idx) {
            COL_SELECTED_BORDER
        } else {
            COL_ELEMENT_BORDER
        };
        let border_width = if state.selection == Selection::Actor(idx) { 2.0 } else { 1.0 };
        painter.rect_stroke(elem_rect, Rounding::same(3.0), Stroke::new(border_width, border_col));

        // Display mode indicator
        if display_mode != DisplayMode::Active {
            let badge = match display_mode {
                DisplayMode::BeforeStart => "FIRST",
                DisplayMode::AfterEnd => "LAST",
                _ => "",
            };
            painter.text(
                Pos2::new(elem_rect.min.x + 4.0, elem_rect.min.y + 4.0),
                egui::Align2::LEFT_TOP, badge,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(255, 180, 80),
            );
        }
    }

    // Draw overlays on top of actors
    draw_canvas_overlays(painter, full_rect, state, viewport_size);
}

// ─── BACKGROUNDS ON CANVAS ───────────────────────────────────────────

fn draw_canvas_backgrounds(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let t = state.playhead;
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;

    for (idx, bg) in state.scene.backgrounds.iter().enumerate() {
        let bg_end = bg.start + bg.duration;
        let display_mode = if t >= bg.start && t <= bg_end {
            DisplayMode::Active
        } else if t < bg.start {
            DisplayMode::BeforeStart
        } else {
            DisplayMode::AfterEnd
        };

        // Backgrounds fill the render frame area
        let tl_world = WorldPos {
            x: rf_state.pos.x - world_w * 0.5,
            y: rf_state.pos.y - world_h * 0.5,
        };
        let br_world = WorldPos {
            x: rf_state.pos.x + world_w * 0.5,
            y: rf_state.pos.y + world_h * 0.5,
        };

        let tl_screen = state.canvas_viewport.world_to_screen(tl_world, viewport_size);
        let br_screen = state.canvas_viewport.world_to_screen(br_world, viewport_size);

        let bg_rect = Rect::from_min_max(
            Pos2::new(full_rect.min.x + tl_screen[0], full_rect.min.y + tl_screen[1]),
            Pos2::new(full_rect.min.x + br_screen[0], full_rect.min.y + br_screen[1]),
        );

        if !full_rect.intersects(bg_rect) { continue; }

        // Draw background representation
        let (fill_color, label) = match &bg.source {
            MediaSource::SolidColor { color } => {
                let c = Color32::from_rgb(color[0], color[1], color[2]);
                (c, format!("Solid #{}", bg.id))
            }
            MediaSource::Image { path } => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("img");
                (Color32::from_rgb(30, 50, 70), format!("BG: {}", name))
            }
            MediaSource::Video { path, .. } => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("vid");
                (Color32::from_rgb(25, 40, 60), format!("BG: {}", name))
            }
        };

        let alpha = match display_mode {
            DisplayMode::Active => 180u8,
            _ => 80u8,
        };
        let fill_with_alpha = Color32::from_rgba_premultiplied(
            (fill_color.r() as u16 * alpha as u16 / 255) as u8,
            (fill_color.g() as u16 * alpha as u16 / 255) as u8,
            (fill_color.b() as u16 * alpha as u16 / 255) as u8,
            alpha,
        );

        painter.rect_filled(bg_rect, Rounding::ZERO, fill_with_alpha);

        // Border if selected
        let is_selected = state.selection == Selection::Background(idx);
        if is_selected {
            painter.rect_stroke(bg_rect, Rounding::ZERO, Stroke::new(2.0, COL_SELECTED_BORDER));
        }

        // Label
        painter.text(
            Pos2::new(bg_rect.min.x + 6.0, bg_rect.max.y - 6.0),
            egui::Align2::LEFT_BOTTOM,
            &label,
            egui::FontId::proportional(9.0),
            COL_BACKGROUND,
        );

        // Display mode badge
        if display_mode != DisplayMode::Active {
            let badge = match display_mode {
                DisplayMode::BeforeStart => "FIRST",
                DisplayMode::AfterEnd => "LAST",
                _ => "",
            };
            painter.text(
                Pos2::new(bg_rect.min.x + 6.0, bg_rect.min.y + 6.0),
                egui::Align2::LEFT_TOP, badge,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(100, 180, 255),
            );
        }
    }
}

// ─── OVERLAYS ON CANVAS ──────────────────────────────────────────────

fn draw_canvas_overlays(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let t = state.playhead;

    for (idx, overlay) in state.scene.overlays.iter().enumerate() {
        let (ov_id, t_in, t_out, layout, color, type_label) = match overlay {
            Overlay::Text(txt) => (
                &txt.id, txt.t_in, txt.t_out, &txt.layout,
                COL_OVERLAY_TEXT, format!("T: {}", truncate_str(&txt.text, 20)),
            ),
            Overlay::Image(img) => (
                &img.id, img.t_in, img.t_out, &img.layout,
                COL_OVERLAY_IMAGE,
                format!("IMG: {}", img.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")),
            ),
            Overlay::Video(vid) => (
                &vid.id, vid.t_in, vid.t_out, &vid.layout,
                COL_OVERLAY_VIDEO,
                format!("VID: {}", vid.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")),
            ),
        };

        // Determine display mode
        let display_mode = if t >= t_in && t <= t_out {
            DisplayMode::Active
        } else if t < t_in {
            DisplayMode::BeforeStart
        } else {
            DisplayMode::AfterEnd
        };

        // Get position — use overlay layout (OverlayState has normalised coords)
        let sample_t = match display_mode {
            DisplayMode::Active => t - t_in,
            DisplayMode::BeforeStart => 0.0,
            DisplayMode::AfterEnd => t_out - t_in,
        };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

        // Convert normalised coords to world pixels
        let rf = &state.scene.render_frame;
        let rf_state = sample_render_frame(rf, t);
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32 / rf_state.zoom;
        let world_h = rh as f32 / rf_state.zoom;
        let frame_tl_x = rf_state.pos.x - world_w * 0.5;
        let frame_tl_y = rf_state.pos.y - world_h * 0.5;

        let world_pos = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w,
            y: frame_tl_y + ov_state.pos[1] * world_h,
        };

        // Size depends on overlay type
        let (elem_w, elem_h) = match overlay {
            Overlay::Text(_) => (300.0 * ov_state.scale, 60.0 * ov_state.scale),
            Overlay::Image(_) => (200.0 * ov_state.scale, 200.0 * ov_state.scale),
            Overlay::Video(_) => (300.0 * ov_state.scale, 300.0 * 16.0 / 9.0 * ov_state.scale),
        };

        let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
        let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
        let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;

        let elem_rect = Rect::from_center_size(
            Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
            Vec2::new(half_w * 2.0, half_h * 2.0),
        );

        if !full_rect.intersects(elem_rect) { continue; }

        // Draw overlay
        let alpha = match display_mode {
            DisplayMode::Active => 200u8,
            _ => 100u8,
        };
        let fill = Color32::from_rgba_premultiplied(
            (color.r() as u16 * 40 / 255) as u8,
            (color.g() as u16 * 40 / 255) as u8,
            (color.b() as u16 * 40 / 255) as u8,
            alpha / 3,
        );
        painter.rect_filled(elem_rect, Rounding::same(4.0), fill);

        // For text overlays, show the text content
        if let Overlay::Text(txt) = overlay {
            let font_size = (12.0 * state.canvas_viewport.zoom * ov_state.scale).clamp(6.0, 48.0);
            painter.text(
                elem_rect.center(),
                egui::Align2::CENTER_CENTER,
                truncate_str(&txt.text, 30),
                egui::FontId::proportional(font_size),
                if display_mode == DisplayMode::Active {
                    Color32::WHITE
                } else {
                    Color32::from_rgb(180, 180, 180)
                },
            );
        } else {
            painter.text(
                elem_rect.center(), egui::Align2::CENTER_CENTER,
                &type_label, egui::FontId::proportional(10.0),
                Color32::from_rgb(160, 160, 180),
            );
        }

        // Border
        let is_selected = state.selection == Selection::Overlay(idx);
        let border_col = if is_selected { COL_SELECTED_BORDER } else { color };
        let border_width = if is_selected { 2.0 } else { 1.0 };
        painter.rect_stroke(elem_rect, Rounding::same(4.0), Stroke::new(border_width, border_col));

        // Display mode badge
        if display_mode != DisplayMode::Active {
            let badge = match display_mode {
                DisplayMode::BeforeStart => "FIRST",
                DisplayMode::AfterEnd => "LAST",
                _ => "",
            };
            painter.text(
                Pos2::new(elem_rect.min.x + 4.0, elem_rect.min.y + 4.0),
                egui::Align2::LEFT_TOP, badge,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(255, 180, 80),
            );
        }
    }
}

/// Helper: truncate a string to max_chars.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars { return s; }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Active,
    BeforeStart,
    AfterEnd,
}


// ─── ELEMENT POSITION RESOLUTION ─────────────────────────────────────

/// Get the world-pixel position of an element, checking canvas_layouts
/// first, then falling back to legacy normalised layout converted to
/// world coords relative to the render frame.
fn get_element_world_pos(
    state: &EditorState,
    element_id: &str,
    legacy_layout: &[Keyframe<ActorState>],
    t: f32,
) -> WorldPos {
    // Check canvas_layouts for this element
    if let Some(cl) = state.scene.canvas_layouts.iter().find(|cl| cl.element_id == element_id) {
        if let Some(transform) = keyframe::sample(&cl.keyframes, t) {
            return transform.pos;
        }
    }

    // Fallback: convert legacy normalised coords to world pixels
    // Legacy coords are [0,1] relative to the output resolution
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;

    if let Some(actor_state) = keyframe::sample(legacy_layout, t) {
        // Convert normalised [0,1] to world pixels relative to render frame
        let frame_tl_x = rf_state.pos.x - world_w * 0.5;
        let frame_tl_y = rf_state.pos.y - world_h * 0.5;
        WorldPos {
            x: frame_tl_x + actor_state.pos[0] * world_w,
            y: frame_tl_y + actor_state.pos[1] * world_h,
        }
    } else {
        rf_state.pos // default to center of render frame
    }
}


// ─── SELECTION GIZMO ─────────────────────────────────────────────────

fn draw_selection_gizmo(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    response: &egui::Response,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    if state.canvas_panning { return; }

    // ── Render frame drag handles ──
    draw_render_frame_handles(ui, painter, response, full_rect, state, viewport_size);

    // Handle drag to move selected element on canvas
    if response.dragged() && !state.canvas_panning {
        let delta = response.drag_delta();
        let world_dx = delta.x / state.canvas_viewport.zoom;
        let world_dy = delta.y / state.canvas_viewport.zoom;

        match state.selection {
            Selection::Actor(idx) if idx < state.scene.actors.len() => {
                let actor_id = state.scene.actors[idx].id.clone();
                if let Some(cl) = state.scene.canvas_layouts.iter_mut()
                    .find(|cl| cl.element_id == actor_id)
                {
                    if let Some(kf) = cl.keyframes.first_mut() {
                        kf.value.pos.x += world_dx;
                        kf.value.pos.y += world_dy;
                    }
                } else {
                    // Move legacy layout
                    if let Some(kf) = state.scene.actors[idx].layout.first_mut() {
                        let rf = &state.scene.render_frame;
                        let rf_state = sample_render_frame(rf, state.playhead);
                        let [rw, rh] = rf.resolution;
                        let world_w = rw as f32 / rf_state.zoom;
                        let world_h = rh as f32 / rf_state.zoom;
                        kf.value.pos[0] += world_dx / world_w;
                        kf.value.pos[1] += world_dy / world_h;
                    }
                }
            }
            Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
                // Move overlay in normalised coords
                let rf = &state.scene.render_frame;
                let rf_state = sample_render_frame(rf, state.playhead);
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32 / rf_state.zoom;
                let world_h = rh as f32 / rf_state.zoom;
                let dx_norm = world_dx / world_w;
                let dy_norm = world_dy / world_h;

                match &mut state.scene.overlays[idx] {
                    Overlay::Text(txt) => {
                        if let Some(kf) = txt.layout.first_mut() {
                            kf.value.pos[0] += dx_norm;
                            kf.value.pos[1] += dy_norm;
                        }
                    }
                    Overlay::Image(img) => {
                        if let Some(kf) = img.layout.first_mut() {
                            kf.value.pos[0] += dx_norm;
                            kf.value.pos[1] += dy_norm;
                        }
                    }
                    Overlay::Video(vid) => {
                        if let Some(kf) = vid.layout.first_mut() {
                            kf.value.pos[0] += dx_norm;
                            kf.value.pos[1] += dy_norm;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Click to select element
    if response.clicked() && !state.canvas_panning {
        if let Some(mouse) = response.interact_pointer_pos() {
            let local = [mouse.x - full_rect.min.x, mouse.y - full_rect.min.y];
            let click_world = state.canvas_viewport.screen_to_world(local, viewport_size);
            try_select_at(state, click_world);
        }
    }
}

// ─── RENDER FRAME HANDLES ────────────────────────────────────────────

fn draw_render_frame_handles(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    response: &egui::Response,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;

    // Compute corners of render frame in screen space
    let tl_world = WorldPos { x: rf_state.pos.x - world_w * 0.5, y: rf_state.pos.y - world_h * 0.5 };
    let br_world = WorldPos { x: rf_state.pos.x + world_w * 0.5, y: rf_state.pos.y + world_h * 0.5 };
    let center_world = rf_state.pos;

    let center_screen = state.canvas_viewport.world_to_screen(center_world, viewport_size);
    let center_pos = Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]);

    // Draw center handle (for moving the render frame)
    let handle_radius = 8.0;
    painter.circle_filled(center_pos, handle_radius, COL_RENDER_FRAME_HANDLE);
    painter.circle_stroke(center_pos, handle_radius, Stroke::new(1.5, Color32::WHITE));
    painter.text(
        Pos2::new(center_pos.x, center_pos.y - handle_radius - 4.0),
        egui::Align2::CENTER_BOTTOM,
        "\u{2316}", // crosshair symbol
        egui::FontId::proportional(10.0),
        COL_RENDER_FRAME,
    );

    // Allow dragging the render frame center with left drag when nothing else is selected
    if response.dragged() && !state.canvas_panning {
        if let Some(origin) = response.interact_pointer_pos() {
            let ox = origin.x - response.drag_delta().x;
            let oy = origin.y - response.drag_delta().y;
            let dist = ((ox - center_pos.x).powi(2) + (oy - center_pos.y).powi(2)).sqrt();
            if dist < handle_radius * 4.0 && state.selection == Selection::None {
                let delta = response.drag_delta();
                let world_dx = delta.x / state.canvas_viewport.zoom;
                let world_dy = delta.y / state.canvas_viewport.zoom;

                // Move the render frame
                if let Some(kf) = state.scene.render_frame.layout.first_mut() {
                    kf.value.pos.x += world_dx;
                    kf.value.pos.y += world_dy;
                }
            }
        }
    }
}

/// Try to select an element at the given world position.
fn try_select_at(state: &mut EditorState, pos: WorldPos) {
    let t = state.playhead;
    let duration = state.scene.output.duration;

    // Check overlays first (top layer)
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;

    for (idx, overlay) in state.scene.overlays.iter().enumerate().rev() {
        let (t_in, t_out, layout, scale_factor) = match overlay {
            Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout, 1.0_f32),
            Overlay::Image(img) => (img.t_in, img.t_out, &img.layout, 1.0_f32),
            Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout, 1.0_f32),
        };

        let sample_t = if t >= t_in && t <= t_out { t - t_in }
            else if t < t_in { 0.0 }
            else { t_out - t_in };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

        let ov_world = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w,
            y: frame_tl_y + ov_state.pos[1] * world_h,
        };

        let (ew, eh) = match overlay {
            Overlay::Text(_) => (300.0 * ov_state.scale, 60.0 * ov_state.scale),
            Overlay::Image(_) => (200.0 * ov_state.scale, 200.0 * ov_state.scale),
            Overlay::Video(_) => (300.0 * ov_state.scale, 300.0 * 16.0 / 9.0 * ov_state.scale),
        };

        if pos.x >= ov_world.x - ew * 0.5 && pos.x <= ov_world.x + ew * 0.5
            && pos.y >= ov_world.y - eh * 0.5 && pos.y <= ov_world.y + eh * 0.5
        {
            state.selection = Selection::Overlay(idx);
            return;
        }
    }

    // Check actors (reverse order = top layer first)
    for (idx, actor) in state.scene.actors.iter().enumerate().rev() {
        if !actor.visible { continue; }
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        let actor_scale = keyframe::sample(&actor.layout, t)
            .map(|s| s.scale).unwrap_or(1.0);
        let elem_width = 400.0 * actor_scale;
        let elem_height = elem_width * 16.0 / 9.0;

        let half_w = elem_width * 0.5;
        let half_h = elem_height * 0.5;

        if pos.x >= world_pos.x - half_w && pos.x <= world_pos.x + half_w
            && pos.y >= world_pos.y - half_h && pos.y <= world_pos.y + half_h
        {
            state.selection = Selection::Actor(idx);
            return;
        }
    }

    // Check backgrounds (click inside render frame area)
    for (idx, bg) in state.scene.backgrounds.iter().enumerate().rev() {
        let bg_end = bg.start + bg.duration;
        // Background occupies the render frame area
        if pos.x >= frame_tl_x && pos.x <= frame_tl_x + world_w
            && pos.y >= frame_tl_y && pos.y <= frame_tl_y + world_h
        {
            // Only select if this bg is active or closest to current time
            if t >= bg.start && t <= bg_end {
                state.selection = Selection::Background(idx);
                return;
            }
        }
    }

    // Nothing hit — deselect
    state.selection = Selection::None;
}


// ─── VIEWPORT CONTROLS ───────────────────────────────────────────────

fn draw_viewport_controls(
    ui: &mut egui::Ui,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    // Floating controls in the bottom-right corner
    let btn_size = Vec2::new(24.0, 24.0);
    let margin = 8.0;

    // Fit button
    let fit_rect = Rect::from_min_size(
        Pos2::new(full_rect.max.x - margin - btn_size.x * 3.0 - 8.0, full_rect.max.y - margin - btn_size.y),
        btn_size,
    );
    let fit_resp = ui.put(fit_rect, egui::Button::new("F").small());
    if fit_resp.on_hover_text("Fit render frame in view").clicked() {
        let rf = &state.scene.render_frame;
        let rf_state = sample_render_frame(rf, state.playhead);
        state.canvas_viewport.fit_render_frame(
            rf_state.pos,
            rf.resolution,
            viewport_size,
        );
    }

    // Zoom in
    let zin_rect = Rect::from_min_size(
        Pos2::new(fit_rect.max.x + 4.0, fit_rect.min.y),
        btn_size,
    );
    let zin_resp = ui.put(zin_rect, egui::Button::new("+").small());
    if zin_resp.on_hover_text("Zoom in").clicked() {
        state.canvas_viewport.zoom = (state.canvas_viewport.zoom * 1.3).min(50.0);
    }

    // Zoom out
    let zout_rect = Rect::from_min_size(
        Pos2::new(zin_rect.max.x + 4.0, fit_rect.min.y),
        btn_size,
    );
    let zout_resp = ui.put(zout_rect, egui::Button::new("-").small());
    if zout_resp.on_hover_text("Zoom out").clicked() {
        state.canvas_viewport.zoom = (state.canvas_viewport.zoom / 1.3).max(0.01);
    }

    // Zoom level indicator
    let zoom_text = format!("{:.0}%", state.canvas_viewport.zoom * 100.0);
    let text_pos = Pos2::new(full_rect.max.x - margin - 50.0, full_rect.max.y - margin - btn_size.y - 16.0);
    ui.painter().text(
        text_pos, egui::Align2::RIGHT_BOTTOM,
        zoom_text, egui::FontId::proportional(9.0),
        Color32::from_rgb(100, 100, 120),
    );
}
