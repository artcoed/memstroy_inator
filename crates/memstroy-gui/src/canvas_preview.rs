//! Free Canvas preview panel — replaces the old fixed 9:16 preview.
//!
//! Renders an infinite 2D canvas with pan/zoom, the render frame
//! rectangle, and all scene elements positioned in world pixels.

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_CANVAS_BG: Color32 = Color32::from_rgb(12, 12, 18);
const COL_GRID_MINOR: Color32 = Color32::from_rgb(25, 25, 35);
const COL_GRID_MAJOR: Color32 = Color32::from_rgb(35, 35, 48);
const COL_RENDER_FRAME: Color32 = Color32::from_rgb(255, 80, 80);
const COL_RENDER_FRAME_FILL: Color32 = Color32::from_rgba_premultiplied(255, 80, 80, 8);
const COL_ELEMENT_BORDER: Color32 = Color32::from_rgb(180, 180, 200);
const COL_SELECTED_BORDER: Color32 = Color32::from_rgb(255, 220, 80);
const COL_INACTIVE_TINT: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 100);


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
    // Middle mouse drag or Space+drag → pan
    let middle_down = ui.input(|i| i.pointer.middle_down());
    let space_held = ui.input(|i| i.key_down(egui::Key::Space));

    if (middle_down || (space_held && response.dragged()))
        && response.hovered()
    {
        let delta = response.drag_delta();
        state.canvas_viewport.pan([delta.x, delta.y]);
        state.canvas_panning = true;
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else {
        state.canvas_panning = false;
    }

    // Scroll wheel → zoom (Ctrl+scroll) or pan (plain scroll)
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        let ctrl = ui.input(|i| i.modifiers.ctrl);

        if ctrl && scroll.y.abs() > 0.1 {
            // Zoom towards mouse position
            let factor = if scroll.y > 0.0 { 1.1 } else { 1.0 / 1.1 };
            if let Some(mouse) = ui.input(|i| i.pointer.hover_pos()) {
                let local = [mouse.x - _full_rect.min.x, mouse.y - _full_rect.min.y];
                state.canvas_viewport.zoom_at(local, viewport_size, factor);
            }
        } else if scroll.y.abs() > 0.1 || scroll.x.abs() > 0.1 {
            // Pan
            state.canvas_viewport.pan([scroll.x, scroll.y]);
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

    // Semi-transparent fill inside the frame
    painter.rect_filled(frame_rect, Rounding::ZERO, COL_RENDER_FRAME_FILL);

    // Dashed border (simulated with multiple short segments)
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
        let elem_width = 400.0; // default width in world pixels
        let elem_height = elem_width * 16.0 / 9.0; // assume 9:16 source aspect

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

    // Handle drag to move selected element on canvas
    if let Selection::Actor(idx) = state.selection {
        if idx < state.scene.actors.len() && response.dragged() {
            let delta = response.drag_delta();
            // Convert screen delta to world delta
            let world_dx = delta.x / state.canvas_viewport.zoom;
            let world_dy = delta.y / state.canvas_viewport.zoom;

            // Update canvas_layout or legacy layout
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

/// Try to select an element at the given world position.
fn try_select_at(state: &mut EditorState, pos: WorldPos) {
    let t = state.playhead;
    let duration = state.scene.output.duration;

    // Check actors (reverse order = top layer first)
    for (idx, actor) in state.scene.actors.iter().enumerate().rev() {
        if !actor.visible { continue; }
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        let elem_width = 400.0;
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
