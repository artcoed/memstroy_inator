//! Free Canvas preview panel.
//!
//! Renders an infinite 2D canvas with pan/zoom, the render frame
//! rectangle, and all scene elements positioned in world pixels.

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;
use std::collections::HashSet;

use crate::state::{EditorState, Selection, TrackKind};

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

/// Render the free canvas preview panel.
pub fn canvas_preview(ui: &mut egui::Ui, state: &mut EditorState) {
    let avail = ui.available_size_before_wrap();
    let (full_rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());

    let painter = ui.painter_at(full_rect);
    let viewport_size = [full_rect.width(), full_rect.height()];

    // ── Background ──
    painter.rect_filled(full_rect, Rounding::ZERO, COL_CANVAS_BG);

    // ── Eyedropper: when armed, the next click picks a pixel from the
    //    selected actor's preview frame and writes it to the actor's
    //    chroma_key.key_color. We handle this BEFORE the regular drag/click
    //    pipeline so it pre-empts selection and gizmo interactions.
    if state.eyedropper_active && response.clicked() {
        if let Some(click_pos) = response.interact_pointer_pos() {
            handle_eyedropper_click(ui, state, full_rect, viewport_size, click_pos);
        }
        // The click is consumed regardless of success — the user expects
        // one click to either commit or exit eyedropper mode.
        state.eyedropper_active = false;
        return;
    }

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

/// When the eyedropper is armed and the user clicks on the preview, sample
/// the selected actor's source frame at the click's UV coordinate and store
/// the colour as the actor's chroma key. The chroma sidecar is updated so
/// the change persists across projects.
fn handle_eyedropper_click(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    click_pos: Pos2,
) {
    let Selection::Actor(idx) = state.selection else {
        state.status = "Eyedropper: no actor selected.".into();
        return;
    };
    if idx >= state.scene.actors.len() { return; }

    // Compute the actor's on-screen rectangle (mirror of the math used in
    // draw_canvas_elements).
    let elem_rect = match actor_screen_rect(state, full_rect, viewport_size, idx) {
        Some(r) => r,
        None => {
            state.status = "Eyedropper: cannot resolve actor rect.".into();
            return;
        }
    };

    // Click must be inside the actor's rect — otherwise we have no UV.
    if !elem_rect.contains(click_pos) {
        state.status = "Eyedropper: click on the actor's image.".into();
        return;
    }

    let u = ((click_pos.x - elem_rect.min.x) / elem_rect.width().max(0.001)).clamp(0.0, 1.0);
    let v = ((click_pos.y - elem_rect.min.y) / elem_rect.height().max(0.001)).clamp(0.0, 1.0);

    // Time inside the source clip, mirroring draw_canvas_elements.
    let actor = &state.scene.actors[idx];
    let t = state.playhead;
    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(state.scene.output.duration);
    let local_t = if t >= t_in && t <= t_out {
        t - t_in + actor.source_start
    } else if t < t_in {
        actor.source_start
    } else {
        actor.source_start + (t_out - t_in)
    };

    if let Some(fc) = state.frame_caches.get_mut(idx) {
        if let Some(img) = fc.raw_frame_at_time(local_t) {
            let px = ((u * img.size[0] as f32) as usize).min(img.size[0].saturating_sub(1));
            let py = ((v * img.size[1] as f32) as usize).min(img.size[1].saturating_sub(1));
            let pixel = img.pixels[py * img.size[0] + px];
            let key = [pixel.r(), pixel.g(), pixel.b()];

            state.scene.actors[idx].chroma_key.key_color = key;
            // Persist the new key colour as part of the per-clip sidecar.
            let src = state.scene.actors[idx].source.clone();
            let chroma = state.scene.actors[idx].chroma_key.clone();
            let _ = chroma.save_alongside_clip(&src);
            state.status = format!("Picked chroma key #{:02X}{:02X}{:02X}", key[0], key[1], key[2]);
            ui.ctx().request_repaint();
            return;
        }
    }
    state.status = "Eyedropper: frame not yet decoded — try again in a moment.".into();
}

/// Compute the screen-space rectangle of an actor on the canvas, replicating
/// the math in `draw_canvas_elements`. Returns `None` when the actor has no
/// usable layout/size info yet.
fn actor_screen_rect(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    idx: usize,
) -> Option<Rect> {
    let actor = state.scene.actors.get(idx)?;
    let t = state.playhead;

    let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
    let (src_w, src_h) = if let Some(fc) = state.frame_caches.get(idx) {
        if fc.is_ready() && fc.frame_count > 0 {
            (fc.source_width as f32, fc.source_height as f32)
        } else { (1080.0_f32, 1920.0) }
    } else { (1080.0_f32, 1920.0) };

    let actor_state = keyframe::sample(&actor.layout, t).unwrap_or_default();
    let elem_w = src_w * actor_state.scale;
    let elem_h = src_h * actor_state.scale * actor_state.scale_y;

    let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
    let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
    let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;

    Some(Rect::from_center_size(
        Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
        Vec2::new(half_w * 2.0, half_h * 2.0),
    ))
}


// ─── INPUT HANDLING ──────────────────────────────────────────────────

fn handle_canvas_input(
    ui: &mut egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    viewport_size: [f32; 2],
    _full_rect: Rect,
) {
    // Pan with middle mouse button OR Space+left-drag OR right-drag.
    // Left click/drag is ONLY for element interaction (select/move).
    let middle_down = ui.input(|i| i.pointer.middle_down());
    let space_held = ui.input(|i| i.key_down(egui::Key::Space));
    let right_down = ui.input(|i| i.pointer.secondary_down());

    let should_pan = middle_down || right_down || (space_held && response.dragged());

    if should_pan && response.hovered() {
        let delta = response.drag_delta();
        if delta.length_sq() > 0.0 {
            state.canvas_viewport.pan([delta.x, delta.y]);
            state.canvas_panning = true;
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    } else if !ui.input(|i| i.pointer.primary_down()) {
        // Only clear panning state when primary isn't held (avoid conflict)
        state.canvas_panning = false;
    }

    // Scroll wheel → zoom viewport
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);

        if scroll.y.abs() > 0.1 {
            // Zoom towards mouse position
            let factor = if scroll.y > 0.0 { 1.05 } else { 1.0 / 1.05 };
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

    // Border only (no fill, no dimming outside)
    painter.rect_stroke(frame_rect, Rounding::ZERO, Stroke::new(2.0, COL_RENDER_FRAME));

    // Corner resize handles for the render frame
    let handle_size = 8.0;
    let corners = [
        frame_rect.left_top(),
        frame_rect.right_top(),
        frame_rect.left_bottom(),
        frame_rect.right_bottom(),
    ];
    for corner in &corners {
        let hr = Rect::from_center_size(*corner, Vec2::splat(handle_size));
        painter.rect_filled(hr, Rounding::same(2.0), COL_RENDER_FRAME_HANDLE);
        painter.rect_stroke(hr, Rounding::same(2.0), Stroke::new(1.0, Color32::WHITE));
    }

    // Edge midpoint handles for the render frame
    let midpoints = [
        Pos2::new(frame_rect.center().x, frame_rect.min.y), // top mid
        Pos2::new(frame_rect.center().x, frame_rect.max.y), // bottom mid
        Pos2::new(frame_rect.min.x, frame_rect.center().y), // left mid
        Pos2::new(frame_rect.max.x, frame_rect.center().y), // right mid
    ];
    for mp in &midpoints {
        let hr = Rect::from_center_size(*mp, Vec2::new(handle_size * 1.2, handle_size * 0.6));
        painter.rect_filled(hr, Rounding::same(2.0), COL_RENDER_FRAME_HANDLE);
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

/// Pick at most one actor per video track for the preview. When multiple
/// trimmed clips of the same source live on the same track at different
/// times, we only show ONE on the canvas — preferring the active one whose
/// window contains `t`, otherwise the one whose [t_in, t_out] is closest
/// to `t`. The chosen actor is drawn with its first/last/current frame
/// according to existing `display_mode` rules.
fn pick_actors_for_canvas(state: &EditorState, t: f32) -> HashSet<usize> {
    use std::collections::HashMap;
    let mut by_track: HashMap<usize, Vec<usize>> = HashMap::new();
    let first_video_track = state
        .tracks
        .iter()
        .enumerate()
        .find(|(_, tk)| tk.kind == TrackKind::Video)
        .map(|(i, _)| i)
        .unwrap_or(0);
    for ai in 0..state.scene.actors.len() {
        let assigned = state
            .actor_track_assignments
            .get(&ai)
            .copied()
            .unwrap_or(first_video_track);
        by_track.entry(assigned).or_default().push(ai);
    }
    let duration = state.scene.output.duration;
    let mut keep = HashSet::new();
    for (_track, indices) in by_track {
        let active = indices.iter().copied().find(|&ai| {
            let a = &state.scene.actors[ai];
            if !a.visible { return false; }
            let t_in = a.t_in.unwrap_or(0.0);
            let t_out = a.t_out.unwrap_or(duration);
            t >= t_in && t <= t_out
        });
        if let Some(ai) = active {
            keep.insert(ai);
            continue;
        }
        // No active clip on this track — pick the one closest to t.
        let best = indices.iter().copied().min_by(|&a, &b| {
            let dist = |ai: usize| -> f32 {
                let actor = &state.scene.actors[ai];
                let t_in = actor.t_in.unwrap_or(0.0);
                let t_out = actor.t_out.unwrap_or(duration);
                if t < t_in { (t_in - t).abs() } else { (t - t_out).abs() }
            };
            dist(a)
                .partial_cmp(&dist(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(ai) = best {
            keep.insert(ai);
        }
    }
    keep
}

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

    // Draw text overlays explicitly placed below the actors.
    draw_canvas_overlays(painter, full_rect, state, viewport_size, OverlayPass::BehindActors);

    // Pick one actor per track to actually draw (closest-to-playhead rule).
    let actors_to_draw = pick_actors_for_canvas(state, t);

    // Draw actors
    for (idx, actor) in state.scene.actors.iter().enumerate() {
        if !actor.visible { continue; }
        if !actors_to_draw.contains(&idx) { continue; }

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
        // Use actual source dimensions from frame cache (native aspect ratio preserved)
        let (src_w, src_h) = if let Some(fc) = state.frame_caches.get(idx) {
            if fc.is_ready() && fc.frame_count > 0 {
                (fc.source_width as f32, fc.source_height as f32)
            } else {
                // Default to 9:16 vertical video (common for mellstroy clips)
                (1080.0_f32, 1920.0)
            }
        } else {
            (1080.0_f32, 1920.0)
        };
        // Apply actor scale from layout
        let actor_state = keyframe::sample(&actor.layout, t)
            .unwrap_or_default();
        let actor_scale = actor_state.scale;
        let actor_scale_y = actor_state.scale_y;
        let actor_rotation = actor_state.rotation_deg;
        let actor_opacity = actor_state.opacity;
        // Apply per-axis scale: scale_y stretches Y on top of uniform scale.
        let elem_width = src_w * actor_scale;
        let elem_height = src_h * actor_scale * actor_scale_y;

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
        let base_tint = match display_mode {
            DisplayMode::Active => Color32::WHITE,
            _ => COL_INACTIVE_TINT,
        };
        // Apply opacity from keyframe
        let alpha = (actor_opacity * (base_tint.a() as f32 / 255.0)).clamp(0.0, 1.0);
        let tint = Color32::from_rgba_unmultiplied(base_tint.r(), base_tint.g(), base_tint.b(), (alpha * 255.0) as u8);

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
                // Apply chromakey on the raw frame data if actor has non-default settings
                let actor_ck = &state.scene.actors[idx].chroma_key;
                let actor_cc = &state.scene.actors[idx].color_correction;
                let has_effects = actor_ck.similarity > 0.01 || actor_cc.brightness.abs() > 0.01
                    || (actor_cc.contrast - 1.0).abs() > 0.01
                    || (actor_cc.saturation - 1.0).abs() > 0.01;

                let texture = if has_effects {
                    fc.processed_frame_at_time(local_t, actor_ck, actor_cc, ui.ctx())
                } else {
                    fc.frame_at_time(local_t, ui.ctx())
                };

                if let Some(tex) = texture {
                    let rotation_rad = actor_rotation.to_radians();
                    if rotation_rad.abs() > 0.001 {
                        let center = elem_rect.center();
                        let hw = elem_rect.width() * 0.5;
                        let hh = elem_rect.height() * 0.5;
                        let cos_r = rotation_rad.cos();
                        let sin_r = rotation_rad.sin();
                        let corners_local = [[-hw, -hh], [hw, -hh], [hw, hh], [-hw, hh]];
                        let uv_corners = [
                            Pos2::new(0.0, 0.0), Pos2::new(1.0, 0.0),
                            Pos2::new(1.0, 1.0), Pos2::new(0.0, 1.0),
                        ];
                        let mut mesh = egui::Mesh::with_texture(tex.id());
                        for ci in 0..4 {
                            let [lx, ly] = corners_local[ci];
                            let rx = lx * cos_r - ly * sin_r + center.x;
                            let ry = lx * sin_r + ly * cos_r + center.y;
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos: Pos2::new(rx, ry),
                                uv: uv_corners[ci],
                                color: tint,
                            });
                        }
                        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                        painter.add(egui::Shape::mesh(mesh));
                    } else {
                        let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                        painter.image(tex.id(), elem_rect, uv, tint);
                    }
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
    draw_canvas_overlays(painter, full_rect, state, viewport_size, OverlayPass::OnTop);

    // Draw the keyframe trajectory for the selected element on top of
    // everything else, so the user can see the animation path with numbered
    // points and per-keyframe parameter callouts.
    draw_selection_keyframe_trajectory(painter, full_rect, state, viewport_size);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayPass {
    /// Only text overlays flagged behind_actors=true.
    BehindActors,
    /// Image/video overlays + text overlays with behind_actors=false.
    OnTop,
}

fn draw_canvas_overlays(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    pass: OverlayPass,
) {
    let t = state.playhead;

    // Build a sorted list of overlay indices for this pass.
    // Sort by z_index (asc), so higher z draws on top.
    let mut order: Vec<(usize, i32)> = state.scene.overlays.iter().enumerate()
        .filter(|(_, ov)| match (ov, pass) {
            (Overlay::Text(t), OverlayPass::BehindActors) => t.behind_actors,
            (Overlay::Text(t), OverlayPass::OnTop) => !t.behind_actors,
            (_, OverlayPass::BehindActors) => false,
            (_, OverlayPass::OnTop) => true,
        })
        .map(|(idx, ov)| {
            let z = match ov {
                Overlay::Text(t) => t.z_index,
                _ => 100,
            };
            (idx, z)
        })
        .collect();
    order.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

    for (idx, _) in order {
        let overlay = &state.scene.overlays[idx];
        let (t_in, t_out, layout) = match overlay {
            Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
            Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
            Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
        };

        let display_mode = if t >= t_in && t <= t_out {
            DisplayMode::Active
        } else if t < t_in {
            DisplayMode::BeforeStart
        } else {
            DisplayMode::AfterEnd
        };

        let sample_t = match display_mode {
            DisplayMode::Active => t - t_in,
            DisplayMode::BeforeStart => 0.0,
            DisplayMode::AfterEnd => t_out - t_in,
        };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

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
        let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
        let center_pos = Pos2::new(
            full_rect.min.x + center_screen[0],
            full_rect.min.y + center_screen[1],
        );

        match overlay {
            Overlay::Text(txt) => {
                draw_text_overlay(painter, full_rect, state, idx, txt, &ov_state, center_pos, display_mode);
            }
            Overlay::Image(img) => {
                let elem_w = 200.0 * ov_state.scale;
                let elem_h = 200.0 * ov_state.scale * ov_state.scale_y;
                let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
                let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;
                let elem_rect = Rect::from_center_size(center_pos, Vec2::new(half_w * 2.0, half_h * 2.0));
                if !full_rect.intersects(elem_rect) { continue; }
                draw_overlay_placeholder(painter, elem_rect, COL_OVERLAY_IMAGE, idx, state,
                    &format!("IMG: {}", img.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")),
                    display_mode);
            }
            Overlay::Video(vid) => {
                let elem_w = 300.0 * ov_state.scale;
                let elem_h = 300.0 * 16.0 / 9.0 * ov_state.scale * ov_state.scale_y;
                let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
                let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;
                let elem_rect = Rect::from_center_size(center_pos, Vec2::new(half_w * 2.0, half_h * 2.0));
                if !full_rect.intersects(elem_rect) { continue; }
                draw_overlay_placeholder(painter, elem_rect, COL_OVERLAY_VIDEO, idx, state,
                    &format!("VID: {}", vid.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")),
                    display_mode);
            }
        }
    }
}

/// Draw the placeholder rectangle (used for image/video overlays in preview).
fn draw_overlay_placeholder(
    painter: &egui::Painter,
    elem_rect: Rect,
    color: Color32,
    idx: usize,
    state: &EditorState,
    label: &str,
    display_mode: DisplayMode,
) {
    let alpha = match display_mode { DisplayMode::Active => 200u8, _ => 100u8 };
    let fill = Color32::from_rgba_premultiplied(
        (color.r() as u16 * 40 / 255) as u8,
        (color.g() as u16 * 40 / 255) as u8,
        (color.b() as u16 * 40 / 255) as u8,
        alpha / 3,
    );
    painter.rect_filled(elem_rect, Rounding::same(4.0), fill);
    painter.text(
        elem_rect.center(), egui::Align2::CENTER_CENTER,
        label, egui::FontId::proportional(10.0),
        Color32::from_rgb(160, 160, 180),
    );
    let is_selected = state.selection == Selection::Overlay(idx);
    let border_col = if is_selected { COL_SELECTED_BORDER } else { color };
    let border_width = if is_selected { 2.0 } else { 1.0 };
    painter.rect_stroke(elem_rect, Rounding::same(4.0), Stroke::new(border_width, border_col));
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

/// Render a TextOverlay on the canvas with full styling: gradient/solid plate,
/// rounded corners, plate border, glyph stroke, alignment, opacity, italic.
fn draw_text_overlay(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    idx: usize,
    txt: &TextOverlay,
    ov_state: &OverlayState,
    center_pos: Pos2,
    display_mode: DisplayMode,
) {
    let style = &txt.style;

    // Effective font size in screen pixels = font_size * scale * canvas_zoom
    let zoom = state.canvas_viewport.zoom;
    let effective_size = (style.font_size * ov_state.scale * zoom).clamp(4.0, 1024.0);
    let italic_skew = if style.italic { 0.18 } else { 0.0 };

    let font_id = egui::FontId::new(effective_size,
        if style.bold { egui::FontFamily::Proportional } else { egui::FontFamily::Proportional });

    // Per-line layout: split text into lines and measure each in egui.
    let lines: Vec<&str> = if txt.text.is_empty() { vec![" "] } else { txt.text.lines().collect() };
    let line_h = effective_size * 1.2;

    // Measure widths via font height heuristic; egui::TextStyle layout would be more
    // exact, but for live preview the bounding box from `painter.text` is sufficient.
    // Build galleys for each line so we can size the plate accurately.
    let galleys: Vec<std::sync::Arc<egui::epaint::text::Galley>> = lines.iter().map(|l| {
        let job = egui::text::LayoutJob::simple_singleline(
            l.to_string(),
            font_id.clone(),
            apply_alpha(Color32::from_rgb(style.color[0], style.color[1], style.color[2]),
                        ov_state.opacity, display_mode),
        );
        painter.layout_job(job)
    }).collect();

    let max_line_w = galleys.iter().map(|g| g.size().x).fold(0.0_f32, f32::max);
    let total_h = galleys.len() as f32 * line_h;

    // Padding/border in screen pixels (scaled by zoom for visual consistency)
    let padding = (style.box_padding * zoom * ov_state.scale).max(0.0);
    let radius = (style.box_corner_radius * zoom * ov_state.scale).max(0.0);
    let plate_w = max_line_w + padding * 2.0;
    let plate_h = total_h + padding * 2.0;

    let plate_rect = Rect::from_center_size(center_pos, Vec2::new(plate_w, plate_h));

    // Skip if completely off-screen
    if !full_rect.intersects(plate_rect.expand(50.0)) { return; }

    let alpha_factor = match display_mode { DisplayMode::Active => 1.0, _ => 0.5 };
    let plate_opacity = style.box_opacity.clamp(0.0, 1.0) * ov_state.opacity * alpha_factor;

    // ─── Plate background ────────────────────────────────────────
    if let Some(box_color) = style.box_color {
        let primary = Color32::from_rgba_unmultiplied(
            box_color[0], box_color[1], box_color[2],
            (plate_opacity * 255.0) as u8,
        );

        match style.box_kind {
            TextBoxKind::None => {}
            TextBoxKind::Solid => {
                painter.rect_filled(plate_rect, Rounding::same(radius), primary);
            }
            TextBoxKind::Gradient => {
                // Vertical gradient via a strip of horizontal lines.
                let end_color_rgb = style.box_gradient_end.unwrap_or(box_color);
                let end = Color32::from_rgba_unmultiplied(
                    end_color_rgb[0], end_color_rgb[1], end_color_rgb[2],
                    (plate_opacity * 255.0) as u8,
                );
                draw_vertical_gradient(painter, plate_rect, radius, primary, end);
            }
            TextBoxKind::OutlineOnly => {
                // No fill, only border drawn below.
            }
        }

        // Plate border
        if style.box_outline_width > 0.0 {
            let border_color_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
            let border_color = Color32::from_rgba_unmultiplied(
                border_color_rgb[0], border_color_rgb[1], border_color_rgb[2],
                (plate_opacity * 255.0) as u8,
            );
            painter.rect_stroke(plate_rect, Rounding::same(radius),
                Stroke::new(style.box_outline_width * zoom, border_color));
        } else if matches!(style.box_kind, TextBoxKind::OutlineOnly) {
            painter.rect_stroke(plate_rect, Rounding::same(radius),
                Stroke::new(2.0, primary));
        }
    }

    // ─── Glyph stroke (poor man's outline) ───────────────────────
    let stroke_w = (style.outline_width * zoom * ov_state.scale).max(0.0);
    let glyph_color = apply_alpha(
        Color32::from_rgb(style.color[0], style.color[1], style.color[2]),
        ov_state.opacity, display_mode);

    // Compute starting Y for vertical centering
    let mut y = plate_rect.center().y - total_h * 0.5 + line_h * 0.5;
    let center_x = plate_rect.center().x;

    for (li, galley) in galleys.iter().enumerate() {
        let line_w = galley.size().x;
        let line_x_left = match style.align {
            TextAlign::Left => plate_rect.min.x + padding,
            TextAlign::Right => plate_rect.max.x - padding - line_w,
            TextAlign::Center => center_x - line_w * 0.5,
        };
        let pos = Pos2::new(line_x_left + italic_skew * line_h * 0.5, y - line_h * 0.5);

        // Glyph outline: draw text repeatedly offset around the position.
        if stroke_w > 0.5 {
            if let Some(stroke_rgb) = style.outline {
                let stroke_color = apply_alpha(
                    Color32::from_rgb(stroke_rgb[0], stroke_rgb[1], stroke_rgb[2]),
                    ov_state.opacity, display_mode);
                let n_steps = 8;
                for step in 0..n_steps {
                    let theta = (step as f32) / (n_steps as f32) * std::f32::consts::TAU;
                    let off = Vec2::new(theta.cos() * stroke_w, theta.sin() * stroke_w);
                    paint_text_line(painter, pos + off, &lines[li], font_id.clone(), stroke_color, italic_skew);
                }
            }
        }

        paint_text_line(painter, pos, &lines[li], font_id.clone(), glyph_color, italic_skew);
        y += line_h;
    }

    // ─── Selection border ─────────────────────────────────────────
    let is_selected = state.selection == Selection::Overlay(idx);
    if is_selected {
        painter.rect_stroke(plate_rect.expand(2.0), Rounding::same(radius + 2.0),
            Stroke::new(2.0, COL_SELECTED_BORDER));
    } else if display_mode == DisplayMode::Active {
        painter.rect_stroke(plate_rect, Rounding::same(radius),
            Stroke::new(0.5, Color32::from_rgba_unmultiplied(120, 200, 140, 60)));
    }

    // Display mode badge
    if display_mode != DisplayMode::Active {
        let badge = match display_mode {
            DisplayMode::BeforeStart => "FIRST",
            DisplayMode::AfterEnd => "LAST",
            _ => "",
        };
        painter.text(
            Pos2::new(plate_rect.min.x + 4.0, plate_rect.min.y + 4.0),
            egui::Align2::LEFT_TOP, badge,
            egui::FontId::proportional(9.0),
            Color32::from_rgb(255, 180, 80),
        );
    }
}

/// Paint a single line of text (helper that ignores italic skew for now —
/// egui can't shear text natively, so italic_skew is unused but we keep the
/// hook for future improvement).
fn paint_text_line(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font_id: egui::FontId,
    color: Color32,
    _italic_skew: f32,
) {
    painter.text(pos, egui::Align2::LEFT_TOP, text, font_id, color);
}

/// Apply a per-state opacity factor to a base color.
fn apply_alpha(base: Color32, ov_opacity: f32, mode: DisplayMode) -> Color32 {
    let mode_factor = match mode { DisplayMode::Active => 1.0, _ => 0.5 };
    let a = ((base.a() as f32) * ov_opacity * mode_factor).clamp(0.0, 255.0) as u8;
    Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), a)
}

/// Draw a vertical gradient inside a rectangle using a strip of thin rects.
fn draw_vertical_gradient(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    top: Color32,
    bottom: Color32,
) {
    let n_strips = 24usize;
    let h = rect.height();
    if h <= 0.0 { return; }
    let strip_h = h / n_strips as f32;
    for i in 0..n_strips {
        let t = (i as f32 + 0.5) / n_strips as f32;
        let r = lerp_u8(top.r(), bottom.r(), t);
        let g = lerp_u8(top.g(), bottom.g(), t);
        let b = lerp_u8(top.b(), bottom.b(), t);
        let a = lerp_u8(top.a(), bottom.a(), t);
        let c = Color32::from_rgba_unmultiplied(r, g, b, a);
        let strip = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.min.y + i as f32 * strip_h),
            Pos2::new(rect.max.x, rect.min.y + (i as f32 + 1.0) * strip_h),
        );
        // Apply rounding only to top and bottom strips.
        let rounding = if i == 0 {
            Rounding { nw: radius, ne: radius, sw: 0.0, se: 0.0 }
        } else if i == n_strips - 1 {
            Rounding { nw: 0.0, ne: 0.0, sw: radius, se: radius }
        } else {
            Rounding::ZERO
        };
        painter.rect_filled(strip, rounding, c);
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    ((a as f32) * (1.0 - t) + (b as f32) * t).clamp(0.0, 255.0) as u8
}

/// Helper: truncate a string to max_chars.
#[allow(dead_code)]
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


// ─── SELECTION & DRAG STATE MACHINE ──────────────────────────────────
//
// All canvas interaction is routed through this single handler. The active
// drag mode is captured *once* at `drag_started()` (so the origin stays
// stable) and applied incrementally on every frame the pointer is held.

const ELEM_HANDLE_SIZE: f32 = 7.0;
const RF_HANDLE_SIZE: f32 = 8.0;
const RF_CENTER_RADIUS: f32 = 8.0;

fn draw_selection_gizmo(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    response: &egui::Response,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    if state.canvas_panning {
        // While panning, only draw selection visuals (no input).
        draw_selection_handles(painter, full_rect, state, viewport_size);
        return;
    }

    // Drag state machine
    if response.drag_started() {
        if let Some(start) = response.interact_pointer_pos() {
            let local = [start.x - full_rect.min.x, start.y - full_rect.min.y];
            let world = state.canvas_viewport.screen_to_world(local, viewport_size);
            state.canvas_drag.start_screen = local;
            state.canvas_drag.mode = decide_drag_mode(state, full_rect, viewport_size, start, world);

            // ── Snapshot world positions for render-frame drag ──
            // The render frame must move/resize independently, so we record the
            // current world positions of every legacy-positioned actor & overlay
            // and recompute their normalised coords on each drag step to keep
            // them visually fixed.
            use crate::state::CanvasDragMode;
            match state.canvas_drag.mode {
                CanvasDragMode::MoveRenderFrame { .. }
                | CanvasDragMode::ResizeRenderFrame { .. } => {
                    let t = state.playhead;
                    state.canvas_drag.actor_legacy_snapshot = state
                        .scene
                        .actors
                        .iter()
                        .enumerate()
                        .filter_map(|(i, a)| {
                            // Skip actors that have a canvas_layouts entry — their
                            // position is already in world coords.
                            if state.scene.canvas_layouts.iter().any(|cl| cl.element_id == a.id) {
                                return None;
                            }
                            let wp = get_element_world_pos(state, &a.id, &a.layout, t);
                            Some((i, [wp.x, wp.y]))
                        })
                        .collect();

                    let rf_state = sample_render_frame(&state.scene.render_frame, t);
                    let [rw, rh] = state.scene.render_frame.resolution;
                    let world_w = rw as f32 / rf_state.zoom;
                    let world_h = rh as f32 / rf_state.zoom;
                    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
                    let frame_tl_y = rf_state.pos.y - world_h * 0.5;

                    state.canvas_drag.overlay_world_snapshot = state
                        .scene
                        .overlays
                        .iter()
                        .enumerate()
                        .filter_map(|(i, ov)| {
                            let layout = match ov {
                                Overlay::Text(t) => &t.layout,
                                Overlay::Image(im) => &im.layout,
                                Overlay::Video(v) => &v.layout,
                            };
                            let ov_state = keyframe::sample(layout, t).unwrap_or_default();
                            let wx = frame_tl_x + ov_state.pos[0] * world_w;
                            let wy = frame_tl_y + ov_state.pos[1] * world_h;
                            Some((i, [wx, wy]))
                        })
                        .collect();
                }
                _ => {
                    state.canvas_drag.actor_legacy_snapshot.clear();
                    state.canvas_drag.overlay_world_snapshot.clear();
                }
            }
        }
    } else if response.drag_stopped() || !response.dragged() {
        if !response.dragged() && state.canvas_drag.mode != crate::state::CanvasDragMode::None {
            state.canvas_drag.mode = crate::state::CanvasDragMode::None;
            state.canvas_drag.actor_legacy_snapshot.clear();
            state.canvas_drag.overlay_world_snapshot.clear();
        }
    }

    if response.dragged() {
        if let Some(cur) = response.interact_pointer_pos() {
            let local = [cur.x - full_rect.min.x, cur.y - full_rect.min.y];
            let shift_held = ui.input(|i| i.modifiers.shift);
            apply_drag(state, full_rect, viewport_size, cur, local, shift_held);
        }
    }

    // ── Click to select (egui distinguishes click from drag automatically) ──
    if response.clicked() && state.canvas_drag.mode == crate::state::CanvasDragMode::None {
        if let Some(mouse) = response.interact_pointer_pos() {
            let local = [mouse.x - full_rect.min.x, mouse.y - full_rect.min.y];
            let click_world = state.canvas_viewport.screen_to_world(local, viewport_size);
            try_select_at(state, click_world);
        }
    }

    // ── Ctrl+scroll to scale the selected element ──
    if response.hovered() && !state.canvas_panning {
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if ctrl && scroll_y.abs() > 0.1 {
            let scale_delta = scroll_y * 0.002;
            apply_scale_delta(state, scale_delta);
        }
    }

    // Update cursor based on what's under the pointer (only when not dragging).
    if response.hovered() && !response.dragged() {
        if let Some(hover) = ui.input(|i| i.pointer.hover_pos()) {
            update_hover_cursor(ui, state, full_rect, viewport_size, hover);
        }
    }

    // ── Draw handles (visual only) ──
    draw_selection_handles(painter, full_rect, state, viewport_size);
}

/// Decide which drag mode to enter based on what was clicked.
fn decide_drag_mode(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    start: Pos2,
    world: WorldPos,
) -> crate::state::CanvasDragMode {
    use crate::state::CanvasDragMode;

    // 1. If a selected element is present and the click is on one of its
    //    8 resize handles → ResizeSelection.
    if let Some(elem_rect) = selected_element_screen_rect(state, full_rect, viewport_size) {
        // Convert screen rect to world rect for anchor computation.
        let zoom = state.canvas_viewport.zoom.max(0.0001);
        let world_min_x = state.canvas_viewport.center.x
            + (elem_rect.min.x - full_rect.min.x - viewport_size[0] * 0.5) / zoom;
        let world_min_y = state.canvas_viewport.center.y
            + (elem_rect.min.y - full_rect.min.y - viewport_size[1] * 0.5) / zoom;
        let world_max_x = state.canvas_viewport.center.x
            + (elem_rect.max.x - full_rect.min.x - viewport_size[0] * 0.5) / zoom;
        let world_max_y = state.canvas_viewport.center.y
            + (elem_rect.max.y - full_rect.min.y - viewport_size[1] * 0.5) / zoom;
        let world_cx = (world_min_x + world_max_x) * 0.5;
        let world_cy = (world_min_y + world_max_y) * 0.5;

        // Handle screen positions paired with handle ID and opposite-anchor
        // world positions:
        //   0..3 = corners (TL, TR, BR, BL)
        //   4..7 = edge midpoints (Top, Right, Bottom, Left)
        let handle_specs: [(Pos2, u8, [f32; 2]); 8] = [
            // Corner: TL → anchor BR
            (elem_rect.left_top(),         0, [world_max_x, world_max_y]),
            // Corner: TR → anchor BL
            (elem_rect.right_top(),        1, [world_min_x, world_max_y]),
            // Corner: BR → anchor TL
            (elem_rect.right_bottom(),     2, [world_min_x, world_min_y]),
            // Corner: BL → anchor TR
            (elem_rect.left_bottom(),      3, [world_max_x, world_min_y]),
            // Edge top → anchor bottom-mid
            (Pos2::new(elem_rect.center().x, elem_rect.min.y), 4, [world_cx, world_max_y]),
            // Edge right → anchor left-mid
            (Pos2::new(elem_rect.max.x, elem_rect.center().y), 5, [world_min_x, world_cy]),
            // Edge bottom → anchor top-mid
            (Pos2::new(elem_rect.center().x, elem_rect.max.y), 6, [world_cx, world_min_y]),
            // Edge left → anchor right-mid
            (Pos2::new(elem_rect.min.x, elem_rect.center().y), 7, [world_max_x, world_cy]),
        ];

        for (handle_pos, handle_id, anchor_world) in handle_specs.iter() {
            if (start - *handle_pos).length() < ELEM_HANDLE_SIZE * 2.5 {
                let initial_scale = current_selection_scale(state).unwrap_or(1.0);
                let initial_scale_y = current_selection_scale_y(state).unwrap_or(1.0);
                let initial_pos_world = current_selection_world_center(state)
                    .unwrap_or([world_cx, world_cy]);
                let (base_w, base_h) = current_selection_base_dims(state)
                    .unwrap_or((1080.0, 1920.0));
                return CanvasDragMode::ResizeSelection {
                    handle: *handle_id,
                    initial_scale,
                    initial_scale_y,
                    initial_pos_world,
                    anchor_world: *anchor_world,
                    base_w,
                    base_h,
                };
            }
        }
        // 2. Click inside the selected element body → MoveSelection.
        if elem_rect.contains(start) {
            return move_selection_mode(state);
        }
    }

    // 3. Click on the render frame's center handle → MoveRenderFrame.
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let center_screen = state.canvas_viewport.world_to_screen(rf_state.pos, viewport_size);
    let rf_center = Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]);
    if (start - rf_center).length() < RF_CENTER_RADIUS * 2.0 {
        return CanvasDragMode::MoveRenderFrame { initial_pos: [rf_state.pos.x, rf_state.pos.y] };
    }

    // 4. Click on a render frame corner → ResizeRenderFrame.
    let rf_rect = render_frame_screen_rect(state, full_rect, viewport_size);
    let rf_corners = [
        rf_rect.left_top(), rf_rect.right_top(),
        rf_rect.left_bottom(), rf_rect.right_bottom(),
    ];
    for corner in &rf_corners {
        if (start - *corner).length() < RF_HANDLE_SIZE * 2.5 {
            let anchor_distance = (start - rf_center).length().max(1.0);
            return CanvasDragMode::ResizeRenderFrame {
                initial_zoom: rf_state.zoom,
                anchor_distance,
            };
        }
    }

    // 5. Otherwise: try to select what's under the cursor and start moving it.
    //    We can't mutate state here, so we just sniff: is anything hit?
    if let Some(hit) = sniff_hit(state, world) {
        match hit {
            Selection::Actor(_) | Selection::Overlay(_) => {
                // The click handler will switch selection on click; but here we
                // don't know the click vs drag yet, so we just leave the mode
                // None and let the click handler do the selection. Movement
                // will start on the next press.
                return CanvasDragMode::None;
            }
            _ => {}
        }
    }

    CanvasDragMode::None
}

/// Apply the active drag mode using the current pointer position.
fn apply_drag(
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    cur: Pos2,
    cur_local: [f32; 2],
    shift_held: bool,
) {
    use crate::state::CanvasDragMode;

    let mode = state.canvas_drag.mode;
    let start = state.canvas_drag.start_screen;
    let dx_screen = cur_local[0] - start[0];
    let dy_screen = cur_local[1] - start[1];
    let zoom = state.canvas_viewport.zoom.max(0.0001);
    let world_dx = dx_screen / zoom;
    let world_dy = dy_screen / zoom;

    match mode {
        CanvasDragMode::None => {}

        CanvasDragMode::MoveActorWorld { actor_idx, initial_pos } => {
            if actor_idx < state.scene.actors.len() {
                let actor_id = state.scene.actors[actor_idx].id.clone();
                if let Some(cl) = state.scene.canvas_layouts.iter_mut()
                    .find(|cl| cl.element_id == actor_id)
                {
                    if let Some(kf) = cl.keyframes.first_mut() {
                        kf.value.pos.x = initial_pos[0] + world_dx;
                        kf.value.pos.y = initial_pos[1] + world_dy;
                    }
                }
            }
        }

        CanvasDragMode::MoveActorLegacy { actor_idx, initial_pos } => {
            if actor_idx < state.scene.actors.len() {
                let rf = &state.scene.render_frame;
                let rf_state = sample_render_frame(rf, state.playhead);
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32 / rf_state.zoom;
                let world_h = rh as f32 / rf_state.zoom;
                if let Some(kf) = state.scene.actors[actor_idx].layout.first_mut() {
                    kf.value.pos[0] = initial_pos[0] + world_dx / world_w;
                    kf.value.pos[1] = initial_pos[1] + world_dy / world_h;
                }
            }
        }

        CanvasDragMode::MoveOverlay { overlay_idx, initial_pos } => {
            if overlay_idx < state.scene.overlays.len() {
                let rf = &state.scene.render_frame;
                let rf_state = sample_render_frame(rf, state.playhead);
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32 / rf_state.zoom;
                let world_h = rh as f32 / rf_state.zoom;
                let dx_norm = world_dx / world_w;
                let dy_norm = world_dy / world_h;
                let new_pos = [initial_pos[0] + dx_norm, initial_pos[1] + dy_norm];
                match &mut state.scene.overlays[overlay_idx] {
                    Overlay::Text(t) => {
                        if let Some(kf) = t.layout.first_mut() { kf.value.pos = new_pos; }
                    }
                    Overlay::Image(im) => {
                        if let Some(kf) = im.layout.first_mut() { kf.value.pos = new_pos; }
                    }
                    Overlay::Video(v) => {
                        if let Some(kf) = v.layout.first_mut() { kf.value.pos = new_pos; }
                    }
                }
            }
        }

        CanvasDragMode::ResizeSelection {
            handle, initial_scale, initial_scale_y, initial_pos_world,
            anchor_world, base_w, base_h,
        } => {
            // Convert pointer to world space.
            let viewport = &state.canvas_viewport;
            let cur_world = viewport.screen_to_world(cur_local, viewport_size);
            let cur_world_x = cur_world.x;
            let cur_world_y = cur_world.y;

            // For each handle, decide which axes change, and what the new
            // dragged-edge world position is.
            let (changes_x, changes_y) = match handle {
                0 | 1 | 2 | 3 => (true, true),     // corners
                4 | 6 => (false, true),            // top / bottom edges
                5 | 7 => (true, false),            // right / left edges
                _ => (true, true),
            };

            let init_w = base_w * initial_scale;
            let init_h = base_h * initial_scale * initial_scale_y;

            let new_w = if changes_x {
                ((cur_world_x - anchor_world[0]).abs()).max(base_w * 0.05)
            } else { init_w };
            let new_h = if changes_y {
                ((cur_world_y - anchor_world[1]).abs()).max(base_h * 0.05)
            } else { init_h };

            // Shift = uniform scale (lock aspect ratio).
            let (final_w, final_h) = if shift_held && changes_x && changes_y {
                // Use the larger relative change to drive both axes.
                let rx = new_w / init_w.max(1e-3);
                let ry = new_h / init_h.max(1e-3);
                let r = if rx.abs() >= ry.abs() { rx } else { ry };
                (init_w * r, init_h * r)
            } else { (new_w, new_h) };

            // Compute new center in world space:
            //   - For axes that change: midpoint between pointer and anchor.
            //   - For axes that don't change: keep initial center.
            let cx = if changes_x {
                let signed = if cur_world_x >= anchor_world[0] { 1.0 } else { -1.0 };
                anchor_world[0] + signed * final_w * 0.5
            } else { initial_pos_world[0] };
            let cy = if changes_y {
                let signed = if cur_world_y >= anchor_world[1] { 1.0 } else { -1.0 };
                anchor_world[1] + signed * final_h * 0.5
            } else { initial_pos_world[1] };

            // Derive scale, scale_y from final dims.
            let new_scale = (final_w / base_w.max(1e-3)).clamp(0.05, 20.0);
            let new_scale_y_total = (final_h / base_h.max(1e-3)).clamp(0.05, 20.0);
            let new_scale_y = (new_scale_y_total / new_scale.max(1e-3)).clamp(0.05, 20.0);

            set_selection_scale(state, new_scale);
            set_selection_scale_y(state, new_scale_y);
            set_selection_world_center(state, [cx, cy]);
        }

        CanvasDragMode::MoveRenderFrame { initial_pos } => {
            // Move the render frame; recompute legacy actor & overlay
            // positions so they stay visually fixed in world space.
            if let Some(kf) = state.scene.render_frame.layout.first_mut() {
                kf.value.pos.x = initial_pos[0] + world_dx;
                kf.value.pos.y = initial_pos[1] + world_dy;
            }

            let rf_state_after = sample_render_frame(&state.scene.render_frame, state.playhead);
            let [rw, rh] = state.scene.render_frame.resolution;
            let world_w = rw as f32 / rf_state_after.zoom;
            let world_h = rh as f32 / rf_state_after.zoom;
            let frame_tl_x = rf_state_after.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state_after.pos.y - world_h * 0.5;

            // Legacy actors: rewrite normalised pos to keep world pos.
            for (idx, world_p) in &state.canvas_drag.actor_legacy_snapshot {
                if *idx >= state.scene.actors.len() { continue; }
                let actor_id = state.scene.actors[*idx].id.clone();
                if state.scene.canvas_layouts.iter().any(|cl| cl.element_id == actor_id) {
                    continue;
                }
                if let Some(kf) = state.scene.actors[*idx].layout.first_mut() {
                    if world_w > 0.0 && world_h > 0.0 {
                        kf.value.pos[0] = (world_p[0] - frame_tl_x) / world_w;
                        kf.value.pos[1] = (world_p[1] - frame_tl_y) / world_h;
                    }
                }
            }
            // Overlays: same recomputation.
            for (idx, world_p) in &state.canvas_drag.overlay_world_snapshot {
                if *idx >= state.scene.overlays.len() { continue; }
                let new_norm = if world_w > 0.0 && world_h > 0.0 {
                    [(world_p[0] - frame_tl_x) / world_w, (world_p[1] - frame_tl_y) / world_h]
                } else { continue; };
                match &mut state.scene.overlays[*idx] {
                    Overlay::Text(t) => {
                        if let Some(kf) = t.layout.first_mut() { kf.value.pos = new_norm; }
                    }
                    Overlay::Image(im) => {
                        if let Some(kf) = im.layout.first_mut() { kf.value.pos = new_norm; }
                    }
                    Overlay::Video(v) => {
                        if let Some(kf) = v.layout.first_mut() { kf.value.pos = new_norm; }
                    }
                }
            }
        }

        CanvasDragMode::ResizeRenderFrame { initial_zoom, anchor_distance } => {
            let rf_state = sample_render_frame(&state.scene.render_frame, state.playhead);
            let center_screen = state.canvas_viewport.world_to_screen(rf_state.pos, viewport_size);
            let rf_center = Pos2::new(
                full_rect.min.x + center_screen[0],
                full_rect.min.y + center_screen[1],
            );
            let cur_dist = (cur - rf_center).length();
            if anchor_distance > 1.0 {
                let factor = (cur_dist / anchor_distance).max(0.05);
                let new_zoom = (initial_zoom / factor).clamp(0.1, 10.0);
                if let Some(kf) = state.scene.render_frame.layout.first_mut() {
                    kf.value.zoom = new_zoom;
                }

                // Recompute legacy positions to preserve world coordinates.
                let rf_state_after = sample_render_frame(&state.scene.render_frame, state.playhead);
                let [rw, rh] = state.scene.render_frame.resolution;
                let world_w = rw as f32 / rf_state_after.zoom;
                let world_h = rh as f32 / rf_state_after.zoom;
                let frame_tl_x = rf_state_after.pos.x - world_w * 0.5;
                let frame_tl_y = rf_state_after.pos.y - world_h * 0.5;

                for (idx, world_p) in &state.canvas_drag.actor_legacy_snapshot {
                    if *idx >= state.scene.actors.len() { continue; }
                    let actor_id = state.scene.actors[*idx].id.clone();
                    if state.scene.canvas_layouts.iter().any(|cl| cl.element_id == actor_id) {
                        continue;
                    }
                    if let Some(kf) = state.scene.actors[*idx].layout.first_mut() {
                        if world_w > 0.0 && world_h > 0.0 {
                            kf.value.pos[0] = (world_p[0] - frame_tl_x) / world_w;
                            kf.value.pos[1] = (world_p[1] - frame_tl_y) / world_h;
                        }
                    }
                }
                for (idx, world_p) in &state.canvas_drag.overlay_world_snapshot {
                    if *idx >= state.scene.overlays.len() { continue; }
                    let new_norm = if world_w > 0.0 && world_h > 0.0 {
                        [(world_p[0] - frame_tl_x) / world_w, (world_p[1] - frame_tl_y) / world_h]
                    } else { continue; };
                    match &mut state.scene.overlays[*idx] {
                        Overlay::Text(t) => {
                            if let Some(kf) = t.layout.first_mut() { kf.value.pos = new_norm; }
                        }
                        Overlay::Image(im) => {
                            if let Some(kf) = im.layout.first_mut() { kf.value.pos = new_norm; }
                        }
                        Overlay::Video(v) => {
                            if let Some(kf) = v.layout.first_mut() { kf.value.pos = new_norm; }
                        }
                    }
                }
            }
        }
    }
}

fn move_selection_mode(state: &EditorState) -> crate::state::CanvasDragMode {
    use crate::state::CanvasDragMode;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor_id = state.scene.actors[idx].id.clone();
            if let Some(cl) = state.scene.canvas_layouts.iter().find(|cl| cl.element_id == actor_id) {
                if let Some(kf) = cl.keyframes.first() {
                    return CanvasDragMode::MoveActorWorld {
                        actor_idx: idx,
                        initial_pos: [kf.value.pos.x, kf.value.pos.y],
                    };
                }
            }
            // Fall back to legacy normalised layout.
            let initial_pos = state.scene.actors[idx].layout.first()
                .map(|kf| kf.value.pos).unwrap_or([0.5, 0.5]);
            CanvasDragMode::MoveActorLegacy { actor_idx: idx, initial_pos }
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let initial_pos = match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.layout.first().map(|k| k.value.pos).unwrap_or([0.5, 0.5]),
                Overlay::Image(im) => im.layout.first().map(|k| k.value.pos).unwrap_or([0.5, 0.5]),
                Overlay::Video(v) => v.layout.first().map(|k| k.value.pos).unwrap_or([0.5, 0.5]),
            };
            CanvasDragMode::MoveOverlay { overlay_idx: idx, initial_pos }
        }
        _ => CanvasDragMode::None,
    }
}

fn current_selection_scale(state: &EditorState) -> Option<f32> {
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            state.scene.actors[idx].layout.first().map(|k| k.value.scale)
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.layout.first().map(|k| k.value.scale),
                Overlay::Image(im) => im.layout.first().map(|k| k.value.scale),
                Overlay::Video(v) => v.layout.first().map(|k| k.value.scale),
            }
        }
        _ => None,
    }
}

fn current_selection_scale_y(state: &EditorState) -> Option<f32> {
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            state.scene.actors[idx].layout.first().map(|k| k.value.scale_y)
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.layout.first().map(|k| k.value.scale_y),
                Overlay::Image(im) => im.layout.first().map(|k| k.value.scale_y),
                Overlay::Video(v) => v.layout.first().map(|k| k.value.scale_y),
            }
        }
        _ => None,
    }
}

/// Return the unscaled (base) world-pixel dimensions of the selected element.
/// Used as the divisor when converting back to scale factors during a free
/// transform.
fn current_selection_base_dims(state: &EditorState) -> Option<(f32, f32)> {
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
                if fc.is_ready() && fc.frame_count > 0 {
                    (fc.source_width as f32, fc.source_height as f32)
                } else { (1080.0, 1920.0) }
            } else { (1080.0, 1920.0) };
            Some((base_w, base_h))
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let ov = &state.scene.overlays[idx];
            // Use the bbox at scale=1, scale_y=1 for the base dimensions.
            let neutral = OverlayState { pos: [0.0, 0.0], scale: 1.0, scale_y: 1.0, rotation_deg: 0.0, opacity: 1.0 };
            Some(overlay_bbox(ov, &neutral))
        }
        _ => None,
    }
}

/// Get the current world-space center position of the selected element.
fn current_selection_world_center(state: &EditorState) -> Option<[f32; 2]> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            let wp = get_element_world_pos(state, &actor.id, &actor.layout, t);
            Some([wp.x, wp.y])
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let ov = &state.scene.overlays[idx];
            let layout = match ov {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            let ov_state = keyframe::sample(layout, t).unwrap_or_default();
            let rf = &state.scene.render_frame;
            let rf_state = sample_render_frame(rf, t);
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32 / rf_state.zoom;
            let world_h = rh as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            Some([
                frame_tl_x + ov_state.pos[0] * world_w,
                frame_tl_y + ov_state.pos[1] * world_h,
            ])
        }
        _ => None,
    }
}

/// Write the world-space center back to the selected element's first
/// keyframe (handles both world-coord canvas_layouts and legacy normalised
/// pos for actors and overlays).
fn set_selection_world_center(state: &mut EditorState, center: [f32; 2]) {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor_id = state.scene.actors[idx].id.clone();
            // Prefer canvas_layouts entry when present.
            if let Some(cl) = state.scene.canvas_layouts.iter_mut()
                .find(|cl| cl.element_id == actor_id)
            {
                if let Some(kf) = cl.keyframes.first_mut() {
                    kf.value.pos.x = center[0];
                    kf.value.pos.y = center[1];
                }
                return;
            }
            // Legacy normalised: convert to render-frame-relative.
            let rf = &state.scene.render_frame;
            let rf_state = sample_render_frame(rf, t);
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32 / rf_state.zoom;
            let world_h = rh as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            if world_w <= 0.0 || world_h <= 0.0 { return; }
            if let Some(kf) = state.scene.actors[idx].layout.first_mut() {
                kf.value.pos[0] = (center[0] - frame_tl_x) / world_w;
                kf.value.pos[1] = (center[1] - frame_tl_y) / world_h;
            }
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let rf = &state.scene.render_frame;
            let rf_state = sample_render_frame(rf, t);
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32 / rf_state.zoom;
            let world_h = rh as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            if world_w <= 0.0 || world_h <= 0.0 { return; }
            let new_norm = [(center[0] - frame_tl_x) / world_w, (center[1] - frame_tl_y) / world_h];
            match &mut state.scene.overlays[idx] {
                Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.pos = new_norm; } }
                Overlay::Image(im) => { if let Some(kf) = im.layout.first_mut() { kf.value.pos = new_norm; } }
                Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.pos = new_norm; } }
            }
        }
        _ => {}
    }
}

fn set_selection_scale_y(state: &mut EditorState, new_scale_y: f32) {
    let s = new_scale_y.clamp(0.05, 20.0);
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            if let Some(kf) = state.scene.actors[idx].layout.first_mut() {
                kf.value.scale_y = s;
            }
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            match &mut state.scene.overlays[idx] {
                Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.scale_y = s; } }
                Overlay::Image(im) => { if let Some(kf) = im.layout.first_mut() { kf.value.scale_y = s; } }
                Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.scale_y = s; } }
            }
        }
        _ => {}
    }
}

fn set_selection_scale(state: &mut EditorState, new_scale: f32) {
    let s = new_scale.clamp(0.05, 20.0);
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            if let Some(kf) = state.scene.actors[idx].layout.first_mut() {
                kf.value.scale = s;
            }
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            match &mut state.scene.overlays[idx] {
                Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.scale = s; } }
                Overlay::Image(im) => { if let Some(kf) = im.layout.first_mut() { kf.value.scale = s; } }
                Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.scale = s; } }
            }
        }
        _ => {}
    }
}

fn apply_scale_delta(state: &mut EditorState, delta: f32) {
    if let Some(s) = current_selection_scale(state) {
        set_selection_scale(state, s + delta);
    }
}

/// Lightweight hit sniffer (read-only) — returns what would be selected at a
/// given world position without mutating the editor state.
fn sniff_hit(state: &EditorState, pos: WorldPos) -> Option<Selection> {
    let t = state.playhead;
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;

    // Overlays first (top of z-order)
    let mut order: Vec<(usize, i32)> = state.scene.overlays.iter().enumerate()
        .map(|(i, ov)| {
            let z = match ov {
                Overlay::Text(t) => {
                    let bias = if t.behind_actors { -1000 } else { 0 };
                    t.z_index + bias
                }
                _ => 100,
            };
            (i, z)
        })
        .collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (idx, _) in order {
        let overlay = &state.scene.overlays[idx];
        let (t_in, t_out, layout) = match overlay {
            Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
            Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
            Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
        };
        let sample_t = if t >= t_in && t <= t_out { t - t_in }
            else if t < t_in { 0.0 } else { t_out - t_in };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
        let ov_world = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w,
            y: frame_tl_y + ov_state.pos[1] * world_h,
        };
        let (ew, eh) = overlay_bbox(overlay, &ov_state);
        if pos.x >= ov_world.x - ew * 0.5 && pos.x <= ov_world.x + ew * 0.5
            && pos.y >= ov_world.y - eh * 0.5 && pos.y <= ov_world.y + eh * 0.5
        {
            return Some(Selection::Overlay(idx));
        }
    }

    for (idx, actor) in state.scene.actors.iter().enumerate().rev() {
        if !actor.visible { continue; }
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        let actor_st = keyframe::sample(&actor.layout, t).unwrap_or_default();
        let actor_scale = actor_st.scale;
        let actor_scale_y = actor_st.scale_y;
        let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
            if fc.is_ready() && fc.frame_count > 0 {
                (fc.source_width as f32, fc.source_height as f32)
            } else { (1080.0, 1920.0) }
        } else { (1080.0, 1920.0) };
        let half_w = base_w * actor_scale * 0.5;
        let half_h = base_h * actor_scale * actor_scale_y * 0.5;
        if pos.x >= world_pos.x - half_w && pos.x <= world_pos.x + half_w
            && pos.y >= world_pos.y - half_h && pos.y <= world_pos.y + half_h
        {
            return Some(Selection::Actor(idx));
        }
    }
    None
}

/// Compute the screen-space rectangle of the selected element (actor/overlay).
fn selected_element_screen_rect(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> Option<Rect> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if !actor.visible { return None; }
            let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
            let actor_st = keyframe::sample(&actor.layout, t).unwrap_or_default();
            let actor_scale = actor_st.scale;
            let actor_scale_y = actor_st.scale_y;
            let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
                if fc.is_ready() && fc.frame_count > 0 {
                    (fc.source_width as f32, fc.source_height as f32)
                } else { (1080.0, 1920.0) }
            } else { (1080.0, 1920.0) };
            let elem_width = base_w * actor_scale;
            let elem_height = base_h * actor_scale * actor_scale_y;
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let half_w = elem_width * 0.5 * state.canvas_viewport.zoom;
            let half_h = elem_height * 0.5 * state.canvas_viewport.zoom;
            Some(Rect::from_center_size(
                Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            ))
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let overlay = &state.scene.overlays[idx];
            let (t_in, t_out, layout) = match overlay {
                Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
                Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
                Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in } else { 0.0 };
            let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
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
            let (elem_w, elem_h) = overlay_bbox(overlay, &ov_state);
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
            let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;
            Some(Rect::from_center_size(
                Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            ))
        }
        _ => None,
    }
}

fn render_frame_screen_rect(state: &EditorState, full_rect: Rect, viewport_size: [f32; 2]) -> Rect {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;
    let tl_world = WorldPos { x: rf_state.pos.x - world_w * 0.5, y: rf_state.pos.y - world_h * 0.5 };
    let br_world = WorldPos { x: rf_state.pos.x + world_w * 0.5, y: rf_state.pos.y + world_h * 0.5 };
    let tl = state.canvas_viewport.world_to_screen(tl_world, viewport_size);
    let br = state.canvas_viewport.world_to_screen(br_world, viewport_size);
    Rect::from_min_max(
        Pos2::new(full_rect.min.x + tl[0], full_rect.min.y + tl[1]),
        Pos2::new(full_rect.min.x + br[0], full_rect.min.y + br[1]),
    )
}

/// Update the cursor icon based on whatever is under the pointer.
fn update_hover_cursor(
    ui: &egui::Ui,
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    hover: Pos2,
) {
    if let Some(rect) = selected_element_screen_rect(state, full_rect, viewport_size) {
        let corners = [
            (rect.left_top(), egui::CursorIcon::ResizeNwSe),
            (rect.right_top(), egui::CursorIcon::ResizeNeSw),
            (rect.left_bottom(), egui::CursorIcon::ResizeNeSw),
            (rect.right_bottom(), egui::CursorIcon::ResizeNwSe),
        ];
        for (corner, cursor) in &corners {
            if (hover - *corner).length() < ELEM_HANDLE_SIZE * 2.0 {
                ui.ctx().set_cursor_icon(*cursor);
                return;
            }
        }
        // Edge midpoint handles
        let edges = [
            (Pos2::new(rect.center().x, rect.min.y), egui::CursorIcon::ResizeVertical),
            (Pos2::new(rect.max.x, rect.center().y), egui::CursorIcon::ResizeHorizontal),
            (Pos2::new(rect.center().x, rect.max.y), egui::CursorIcon::ResizeVertical),
            (Pos2::new(rect.min.x, rect.center().y), egui::CursorIcon::ResizeHorizontal),
        ];
        for (mp, cursor) in &edges {
            if (hover - *mp).length() < ELEM_HANDLE_SIZE * 2.0 {
                ui.ctx().set_cursor_icon(*cursor);
                return;
            }
        }
        if rect.contains(hover) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
            return;
        }
    }

    // Render frame center / corners
    let rf_rect = render_frame_screen_rect(state, full_rect, viewport_size);
    let rf_corners = [
        rf_rect.left_top(), rf_rect.right_top(),
        rf_rect.left_bottom(), rf_rect.right_bottom(),
    ];
    for c in &rf_corners {
        if (hover - *c).length() < RF_HANDLE_SIZE * 2.0 {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
            return;
        }
    }
    let rf_state = sample_render_frame(&state.scene.render_frame, state.playhead);
    let center_screen = state.canvas_viewport.world_to_screen(rf_state.pos, viewport_size);
    let rf_center = Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]);
    if (hover - rf_center).length() < RF_CENTER_RADIUS * 2.0 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
    }
}

/// Compute the world-pixel position for an actor at a given LOCAL keyframe time.
/// Mirrors `get_element_world_pos` but operates on a single ActorState
/// rather than sampling the layout (since for trajectory drawing we want
/// each keyframe's exact value).
fn actor_kf_world_pos(
    state: &EditorState,
    actor_id: &str,
    actor_state: &ActorState,
    rf_state: &RenderFrameState,
    rf_resolution: [u32; 2],
) -> WorldPos {
    // Prefer canvas_layouts world coords if present; pick the kf nearest in t.
    if let Some(cl) = state.scene.canvas_layouts.iter().find(|cl| cl.element_id == actor_id) {
        if !cl.keyframes.is_empty() {
            // Approximation: sample at center of clip — simple and stable
            if let Some(kf) = cl.keyframes.first() {
                return kf.value.pos;
            }
        }
    }
    // Legacy normalised → world-pixel relative to the render frame.
    let world_w = rf_resolution[0] as f32 / rf_state.zoom;
    let world_h = rf_resolution[1] as f32 / rf_state.zoom;
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;
    WorldPos {
        x: frame_tl_x + actor_state.pos[0] * world_w,
        y: frame_tl_y + actor_state.pos[1] * world_h,
    }
}

/// Draw the keyframe trajectory for the currently selected actor or overlay.
/// Each keyframe is shown as a numbered dot, all dots are connected by a
/// dashed polyline, and a small callout shows the parameter values at each
/// keyframe (position, scale, rotation, opacity). This is the on-canvas
/// half of the "visual animation constructor".
fn draw_selection_keyframe_trajectory(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let rf_resolution = rf.resolution;

    // Collect (world_pos, label) per keyframe for the active selection.
    #[derive(Clone)]
    struct KfPoint { world: WorldPos, t: f32, label: String }

    let points: Vec<KfPoint> = match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if actor.layout.len() < 2 { return; }
            actor.layout.iter().enumerate().map(|(i, kf)| {
                let world = actor_kf_world_pos(state, &actor.id, &kf.value, &rf_state, rf_resolution);
                KfPoint {
                    world,
                    t: kf.t,
                    label: format!(
                        "#{}: t={:.2}s\np=({:.0},{:.0})\ns={:.2} sy={:.2}\nr={:.0}\u{00B0}\nα={:.2}",
                        i + 1, kf.t,
                        world.x, world.y,
                        kf.value.scale, kf.value.scale_y,
                        kf.value.rotation_deg, kf.value.opacity,
                    ),
                }
            }).collect()
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let layout: &[Keyframe<OverlayState>] = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            if layout.len() < 2 { return; }
            let world_w = rf_resolution[0] as f32 / rf_state.zoom;
            let world_h = rf_resolution[1] as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            layout.iter().enumerate().map(|(i, kf)| {
                let world = WorldPos {
                    x: frame_tl_x + kf.value.pos[0] * world_w,
                    y: frame_tl_y + kf.value.pos[1] * world_h,
                };
                KfPoint {
                    world,
                    t: kf.t,
                    label: format!(
                        "#{}: t={:.2}s\np=({:.0},{:.0})\ns={:.2} sy={:.2}\nr={:.0}\u{00B0}\nα={:.2}",
                        i + 1, kf.t,
                        world.x, world.y,
                        kf.value.scale, kf.value.scale_y,
                        kf.value.rotation_deg, kf.value.opacity,
                    ),
                }
            }).collect()
        }
        _ => return,
    };

    if points.len() < 2 { return; }

    // Convert each kf world position to screen pos.
    let screen_pts: Vec<Pos2> = points.iter().map(|p| {
        let s = state.canvas_viewport.world_to_screen(p.world, viewport_size);
        Pos2::new(full_rect.min.x + s[0], full_rect.min.y + s[1])
    }).collect();

    // Polyline connecting consecutive keyframes.
    let path_color = Color32::from_rgba_premultiplied(255, 220, 80, 200);
    for win in screen_pts.windows(2) {
        painter.line_segment([win[0], win[1]], Stroke::new(2.0, path_color));
    }

    // Numbered dots + value callouts.
    let dot_radius = 6.0;
    let dot_fill = Color32::from_rgb(255, 180, 60);
    let dot_stroke = Color32::from_rgb(40, 30, 0);
    for (i, (pt, kp)) in screen_pts.iter().zip(points.iter()).enumerate() {
        painter.circle_filled(*pt, dot_radius, dot_fill);
        painter.circle_stroke(*pt, dot_radius, Stroke::new(1.5, dot_stroke));
        // Number inside the dot
        painter.text(
            *pt, egui::Align2::CENTER_CENTER,
            (i + 1).to_string(),
            egui::FontId::proportional(10.0),
            Color32::from_rgb(20, 20, 20),
        );
        // Callout label with parameter values
        let label_pos = Pos2::new(pt.x + dot_radius + 4.0, pt.y - 4.0);
        if full_rect.contains(label_pos) {
            painter.text(
                label_pos,
                egui::Align2::LEFT_BOTTOM,
                &kp.label,
                egui::FontId::proportional(9.5),
                Color32::from_rgb(255, 230, 130),
            );
        }
        let _ = kp; // explicit use
    }
}

/// Render-only: draw selection border, corner handles, and render-frame
/// center handle. Does NOT consume any input.
fn draw_selection_handles(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    // Selected element handles
    if let Some(rect) = selected_element_screen_rect(state, full_rect, viewport_size) {
        let handle_color = COL_SELECTED_BORDER;
        let corners = [
            rect.left_top(), rect.right_top(),
            rect.left_bottom(), rect.right_bottom(),
        ];
        for corner in &corners {
            let hr = Rect::from_center_size(*corner, Vec2::splat(ELEM_HANDLE_SIZE));
            painter.rect_filled(hr, Rounding::same(2.0), handle_color);
            painter.rect_stroke(hr, Rounding::same(2.0), Stroke::new(1.0, Color32::from_rgb(40, 40, 40)));
        }
        let midpoints = [
            Pos2::new(rect.center().x, rect.min.y),
            Pos2::new(rect.center().x, rect.max.y),
            Pos2::new(rect.min.x, rect.center().y),
            Pos2::new(rect.max.x, rect.center().y),
        ];
        for mp in &midpoints {
            let hr = Rect::from_center_size(*mp, Vec2::new(ELEM_HANDLE_SIZE * 1.4, ELEM_HANDLE_SIZE * 0.7));
            painter.rect_filled(hr, Rounding::same(2.0), handle_color);
        }
        painter.rect_stroke(rect, Rounding::same(2.0), Stroke::new(1.5, handle_color));
    }

    // Render frame center handle
    let rf_state = sample_render_frame(&state.scene.render_frame, state.playhead);
    let center_screen = state.canvas_viewport.world_to_screen(rf_state.pos, viewport_size);
    let rf_center = Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]);
    painter.circle_filled(rf_center, RF_CENTER_RADIUS, COL_RENDER_FRAME_HANDLE);
    painter.circle_stroke(rf_center, RF_CENTER_RADIUS, Stroke::new(1.5, Color32::WHITE));
}

// ─── ELEMENT RESIZE HANDLES ──────────────────────────────────────────

/// Draw resize handles (corners + edges) on the currently selected element.
/// Dragging a handle scales the element proportionally.
#[allow(dead_code)]
fn draw_element_resize_handles(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    response: &egui::Response,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
) {
    let t = state.playhead;
    let handle_size = 7.0;
    let handle_color = COL_SELECTED_BORDER;

    // Get the bounding rect of the selected element in screen space
    let elem_rect = match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if !actor.visible { return; }
            let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
            let actor_scale = keyframe::sample(&actor.layout, t)
                .map(|s| s.scale).unwrap_or(1.0);
            // Use real source dimensions from frame cache
            let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
                if fc.is_ready() && fc.frame_count > 0 {
                    (fc.source_width as f32, fc.source_height as f32)
                } else { (1080.0, 1920.0) }
            } else { (1080.0, 1920.0) };
            let elem_width = base_w * actor_scale;
            let elem_height = base_h * actor_scale;
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let half_w = elem_width * 0.5 * state.canvas_viewport.zoom;
            let half_h = elem_height * 0.5 * state.canvas_viewport.zoom;
            Rect::from_center_size(
                Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            )
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let overlay = &state.scene.overlays[idx];
            let (t_in, t_out, layout) = match overlay {
                Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
                Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
                Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in } else { 0.0 };
            let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
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
            let (elem_w, elem_h) = overlay_bbox(overlay, &ov_state);
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
            let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;
            Rect::from_center_size(
                Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            )
        }
        _ => return,
    };

    // Draw corner handles
    let corners = [
        (elem_rect.left_top(), egui::CursorIcon::ResizeNwSe),
        (elem_rect.right_top(), egui::CursorIcon::ResizeNeSw),
        (elem_rect.left_bottom(), egui::CursorIcon::ResizeNeSw),
        (elem_rect.right_bottom(), egui::CursorIcon::ResizeNwSe),
    ];
    for (corner, _cursor) in &corners {
        let hr = Rect::from_center_size(*corner, Vec2::splat(handle_size));
        painter.rect_filled(hr, Rounding::same(2.0), handle_color);
        painter.rect_stroke(hr, Rounding::same(2.0), Stroke::new(1.0, Color32::from_rgb(40, 40, 40)));
    }

    // Draw edge midpoint handles
    let midpoints = [
        Pos2::new(elem_rect.center().x, elem_rect.min.y),
        Pos2::new(elem_rect.center().x, elem_rect.max.y),
        Pos2::new(elem_rect.min.x, elem_rect.center().y),
        Pos2::new(elem_rect.max.x, elem_rect.center().y),
    ];
    for mp in &midpoints {
        let hr = Rect::from_center_size(*mp, Vec2::new(handle_size * 1.4, handle_size * 0.7));
        painter.rect_filled(hr, Rounding::same(2.0), handle_color);
    }

    // Draw selection border
    painter.rect_stroke(elem_rect, Rounding::same(2.0), Stroke::new(1.5, handle_color));

    // Handle corner drag for scaling
    if response.dragged() && !state.canvas_panning {
        if let Some(origin) = response.interact_pointer_pos() {
            let ox = origin.x - response.drag_delta().x;
            let oy = origin.y - response.drag_delta().y;

            // Check if drag originated near a corner handle
            let mut near_corner = false;
            for (corner, _) in &corners {
                let dist = ((ox - corner.x).powi(2) + (oy - corner.y).powi(2)).sqrt();
                if dist < handle_size * 2.5 {
                    near_corner = true;
                    break;
                }
            }

            if near_corner {
                // Scale based on drag distance from center
                let center = elem_rect.center();
                let prev_dist = ((ox - center.x).powi(2) + (oy - center.y).powi(2)).sqrt();
                let curr_dist = ((origin.x - center.x).powi(2) + (origin.y - center.y).powi(2)).sqrt();
                if prev_dist > 1.0 {
                    let scale_factor = curr_dist / prev_dist;
                    match state.selection {
                        Selection::Actor(idx) if idx < state.scene.actors.len() => {
                            if let Some(kf) = state.scene.actors[idx].layout.first_mut() {
                                kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 10.0);
                            }
                        }
                        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
                            match &mut state.scene.overlays[idx] {
                                Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 10.0); } }
                                Overlay::Image(i) => { if let Some(kf) = i.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 10.0); } }
                                Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 10.0); } }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Show resize cursor when hovering handles
    if response.hovered() && !state.canvas_panning {
        if let Some(hover) = ui.input(|i| i.pointer.hover_pos()) {
            for (corner, cursor) in &corners {
                let dist = ((hover.x - corner.x).powi(2) + (hover.y - corner.y).powi(2)).sqrt();
                if dist < handle_size * 2.0 {
                    ui.ctx().set_cursor_icon(*cursor);
                    break;
                }
            }
        }
    }
}

// ─── RENDER FRAME HANDLES ────────────────────────────────────────────

#[allow(dead_code)]
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

    let tl_screen = state.canvas_viewport.world_to_screen(tl_world, viewport_size);
    let br_screen = state.canvas_viewport.world_to_screen(br_world, viewport_size);
    let center_screen = state.canvas_viewport.world_to_screen(center_world, viewport_size);

    let frame_rect = Rect::from_min_max(
        Pos2::new(full_rect.min.x + tl_screen[0], full_rect.min.y + tl_screen[1]),
        Pos2::new(full_rect.min.x + br_screen[0], full_rect.min.y + br_screen[1]),
    );
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

    // Allow dragging the render frame center or body
    if response.dragged() && !state.canvas_panning {
        if let Some(origin) = response.interact_pointer_pos() {
            let ox = origin.x - response.drag_delta().x;
            let oy = origin.y - response.drag_delta().y;

            // Check if near center handle → move
            let dist_center = ((ox - center_pos.x).powi(2) + (oy - center_pos.y).powi(2)).sqrt();
            if dist_center < handle_radius * 4.0 && state.selection == Selection::None {
                let delta = response.drag_delta();
                let world_dx = delta.x / state.canvas_viewport.zoom;
                let world_dy = delta.y / state.canvas_viewport.zoom;

                // Move the render frame
                if let Some(kf) = state.scene.render_frame.layout.first_mut() {
                    kf.value.pos.x += world_dx;
                    kf.value.pos.y += world_dy;
                }
            }

            // Check if near a corner → resize (zoom)
            let corners = [
                frame_rect.left_top(),
                frame_rect.right_top(),
                frame_rect.left_bottom(),
                frame_rect.right_bottom(),
            ];
            let mut near_rf_corner = false;
            for corner in &corners {
                let dist = ((ox - corner.x).powi(2) + (oy - corner.y).powi(2)).sqrt();
                if dist < 14.0 {
                    near_rf_corner = true;
                    break;
                }
            }
            if near_rf_corner && state.selection == Selection::None {
                // Resize = change zoom of the render frame
                let prev_dist = ((ox - center_pos.x).powi(2) + (oy - center_pos.y).powi(2)).sqrt();
                let curr_dist = ((origin.x - center_pos.x).powi(2) + (origin.y - center_pos.y).powi(2)).sqrt();
                if prev_dist > 1.0 {
                    let scale_factor = curr_dist / prev_dist;
                    if let Some(kf) = state.scene.render_frame.layout.first_mut() {
                        // Increasing corner distance = zoom out (show more), decreasing = zoom in
                        kf.value.zoom = (kf.value.zoom / scale_factor).clamp(0.1, 10.0);
                    }
                }
            }
        }
    }
}

/// Compute the world-pixel bounding box (width, height) of an overlay.
/// For text this approximates the dynamic plate size from text content + style.
fn overlay_bbox(overlay: &Overlay, ov_state: &OverlayState) -> (f32, f32) {
    let sx = ov_state.scale;
    let sy = ov_state.scale * ov_state.scale_y;
    match overlay {
        Overlay::Text(txt) => {
            let style = &txt.style;
            let lines: Vec<&str> = if txt.text.is_empty() { vec![" "] } else { txt.text.lines().collect() };
            let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
            let font = style.font_size;
            // ~0.55 per glyph is a reasonable heuristic for proportional fonts.
            let text_w = (max_chars.max(1.0)) * font * 0.55;
            let text_h = (lines.len() as f32) * font * 1.2;
            let pad = style.box_padding * 2.0;
            ((text_w + pad).max(40.0) * sx,
             (text_h + pad).max(20.0) * sy)
        }
        Overlay::Image(_) => (200.0 * sx, 200.0 * sy),
        Overlay::Video(_) => (300.0 * sx, 300.0 * 16.0 / 9.0 * sy),
    }
}

/// Try to select an element at the given world position.
fn try_select_at(state: &mut EditorState, pos: WorldPos) {
    let t = state.playhead;

    // Render-frame anchor for converting overlay normalised coords.
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;

    // Build z-sorted overlay list and check from top to bottom (highest z first).
    let mut order: Vec<(usize, i32)> = state.scene.overlays.iter().enumerate()
        .map(|(i, ov)| {
            let z = match ov {
                Overlay::Text(t) => {
                    let bias = if t.behind_actors { -1000 } else { 0 };
                    t.z_index + bias
                }
                _ => 100,
            };
            (i, z)
        })
        .collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (idx, _) in order {
        let overlay = &state.scene.overlays[idx];
        let (t_in, t_out, layout) = match overlay {
            Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
            Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
            Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
        };
        let sample_t = if t >= t_in && t <= t_out { t - t_in }
            else if t < t_in { 0.0 } else { t_out - t_in };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

        let ov_world = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w,
            y: frame_tl_y + ov_state.pos[1] * world_h,
        };
        let (ew, eh) = overlay_bbox(overlay, &ov_state);
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
        let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
            if fc.is_ready() && fc.frame_count > 0 {
                (fc.source_width as f32, fc.source_height as f32)
            } else { (1080.0, 1920.0) }
        } else { (1080.0, 1920.0) };
        let elem_width = base_w * actor_scale;
        let elem_height = base_h * actor_scale;
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
        if pos.x >= frame_tl_x && pos.x <= frame_tl_x + world_w
            && pos.y >= frame_tl_y && pos.y <= frame_tl_y + world_h
        {
            if t >= bg.start && t <= bg_end {
                state.selection = Selection::Background(idx);
                return;
            }
        }
    }

    state.selection = Selection::None;
}

/// Check if a world-pixel position hits the currently selected element.
#[allow(dead_code)]
fn is_point_on_selection(state: &EditorState, pos: WorldPos) -> bool {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if !actor.visible { return false; }
            let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
            let actor_scale = keyframe::sample(&actor.layout, t)
                .map(|s| s.scale).unwrap_or(1.0);
            let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
                if fc.is_ready() && fc.frame_count > 0 {
                    (fc.source_width as f32, fc.source_height as f32)
                } else { (1080.0, 1920.0) }
            } else { (1080.0, 1920.0) };
            let half_w = base_w * actor_scale * 0.5;
            let half_h = base_h * actor_scale * 0.5;
            pos.x >= world_pos.x - half_w && pos.x <= world_pos.x + half_w
                && pos.y >= world_pos.y - half_h && pos.y <= world_pos.y + half_h
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let overlay = &state.scene.overlays[idx];
            let rf = &state.scene.render_frame;
            let rf_state = sample_render_frame(rf, t);
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32 / rf_state.zoom;
            let world_h = rh as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            let (t_in, t_out, layout) = match overlay {
                Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
                Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
                Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in } else { 0.0 };
            let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
            let ov_world = WorldPos {
                x: frame_tl_x + ov_state.pos[0] * world_w,
                y: frame_tl_y + ov_state.pos[1] * world_h,
            };
            let (ew, eh) = overlay_bbox(overlay, &ov_state);
            pos.x >= ov_world.x - ew * 0.5 && pos.x <= ov_world.x + ew * 0.5
                && pos.y >= ov_world.y - eh * 0.5 && pos.y <= ov_world.y + eh * 0.5
        }
        _ => false,
    }
}


// ─── PREVIEW EFFECTS (CHROMAKEY + COLOR CORRECTION) ──────────────────

/// Apply chromakey and color correction to a raw frame for preview display.
/// This is a simplified CPU-based version for real-time preview.
#[allow(dead_code)]
fn apply_preview_effects(
    img: &egui::ColorImage,
    ck: &memstroy_core::ChromaKeyParams,
    cc: &memstroy_core::ColorCorrection,
) -> egui::ColorImage {
    let mut out = egui::ColorImage::new(img.size, Color32::TRANSPARENT);
    let key_r = ck.key_color[0] as f32;
    let key_g = ck.key_color[1] as f32;
    let key_b = ck.key_color[2] as f32;
    let similarity = ck.similarity.clamp(0.0, 1.0);
    let blend = ck.blend.clamp(0.0, 1.0);
    let spill = ck.spill.clamp(0.0, 1.0);
    // Color distance threshold
    let threshold = similarity * 441.0; // max RGB distance = sqrt(3*255^2) ≈ 441
    let blend_range = blend * 200.0;

    for (i, pixel) in img.pixels.iter().enumerate() {
        let r = pixel.r() as f32;
        let g = pixel.g() as f32;
        let b = pixel.b() as f32;

        // Chromakey: compute color distance to key
        let dist = ((r - key_r).powi(2) + (g - key_g).powi(2) + (b - key_b).powi(2)).sqrt();
        let mut alpha = if dist < threshold {
            0.0
        } else if dist < threshold + blend_range {
            (dist - threshold) / blend_range.max(0.01)
        } else {
            1.0
        };

        // Spill suppression
        let (mut out_r, mut out_g, mut out_b) = (r, g, b);
        if alpha > 0.0 && spill > 0.0 && g > ((r + b) * 0.5) as f32 {
            let avg_rb = (r + b) * 0.5;
            out_g = g - (g - avg_rb) * spill;
        }

        // Color correction
        // Brightness
        out_r = (out_r + cc.brightness * 255.0).clamp(0.0, 255.0);
        out_g = (out_g + cc.brightness * 255.0).clamp(0.0, 255.0);
        out_b = (out_b + cc.brightness * 255.0).clamp(0.0, 255.0);
        // Contrast
        out_r = ((out_r - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        out_g = ((out_g - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        out_b = ((out_b - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
        // Saturation
        let gray = 0.299 * out_r + 0.587 * out_g + 0.114 * out_b;
        out_r = (gray + (out_r - gray) * cc.saturation).clamp(0.0, 255.0);
        out_g = (gray + (out_g - gray) * cc.saturation).clamp(0.0, 255.0);
        out_b = (gray + (out_b - gray) * cc.saturation).clamp(0.0, 255.0);
        // Temperature (warm/cool shift)
        if cc.temperature > 0.0 {
            out_r = (out_r + cc.temperature * 30.0).clamp(0.0, 255.0);
            out_b = (out_b - cc.temperature * 30.0).clamp(0.0, 255.0);
        } else if cc.temperature < 0.0 {
            out_r = (out_r + cc.temperature * 30.0).clamp(0.0, 255.0);
            out_b = (out_b - cc.temperature * 30.0).clamp(0.0, 255.0);
        }

        let a = (alpha * 255.0) as u8;
        out.pixels[i] = Color32::from_rgba_unmultiplied(out_r as u8, out_g as u8, out_b as u8, a);
    }
    out
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
