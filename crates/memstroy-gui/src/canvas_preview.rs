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
const COL_ELEMENT_BORDER: Color32 = Color32::from_rgb(180, 180, 200);
const COL_SELECTED_BORDER: Color32 = Color32::from_rgb(255, 220, 80);
const COL_INACTIVE_TINT: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 100);
const COL_OVERLAY_TEXT: Color32 = Color32::from_rgb(80, 200, 120);
const COL_OVERLAY_IMAGE: Color32 = Color32::from_rgb(100, 180, 255);
const COL_OVERLAY_VIDEO: Color32 = Color32::from_rgb(200, 100, 255);
const COL_BACKGROUND: Color32 = Color32::from_rgb(60, 130, 220);
const COL_RENDER_FRAME_HANDLE: Color32 = Color32::from_rgb(255, 120, 120);
const COL_ROTATION_HANDLE: Color32 = Color32::from_rgb(120, 220, 255);

/// Distance from the bbox top edge at which the rotation handle floats
/// (in screen pixels). Bigger than the resize handle radius so the
/// hit-tests don't overlap.
const ROTATION_HANDLE_OFFSET: f32 = 28.0;
const ROTATION_HANDLE_RADIUS: f32 = 7.0;


// ─── LAYER ORDER HELPERS ─────────────────────────────────────────────
//
// The timeline panel's track row order is the single source of truth for
// stacking. Lower track index = higher up the panel = renders on TOP. To
// keep editor preview, click selection, and the (legacy) ffmpeg renderer
// in sync we expose helpers that derive z-order and "behind actors" from
// `overlay_track_assignments` instead of from the now-hidden
// `t.z_index` / `t.behind_actors` fields.

/// Default video-track index for an unassigned overlay. Mirrors the
/// fallback used by the timeline panel: prefer the second video track,
/// otherwise the first, otherwise 0.
fn default_overlay_track(state: &EditorState) -> usize {
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

/// Resolve the timeline track index for a given overlay (using the same
/// fallback rule as the timeline panel).
fn overlay_track_index(state: &EditorState, overlay_idx: usize) -> usize {
    state
        .overlay_track_assignments
        .get(&overlay_idx)
        .copied()
        .unwrap_or_else(|| default_overlay_track(state))
}

/// Resolve the timeline track index for a given actor (defaulting to the
/// first video track).
fn actor_track_index(state: &EditorState, actor_idx: usize) -> usize {
    state
        .actor_track_assignments
        .get(&actor_idx)
        .copied()
        .unwrap_or_else(|| {
            (0..state.tracks.len())
                .find(|i| state.tracks[*i].kind == TrackKind::Video)
                .unwrap_or(0)
        })
}

/// True if any visible actor sits on a track ABOVE this overlay's row
/// (i.e. with a smaller track index). When that is the case, the overlay
/// must be drawn before the actors so it ends up visually behind them.
fn overlay_is_behind_actors(state: &EditorState, overlay_idx: usize) -> bool {
    let overlay_track = overlay_track_index(state, overlay_idx);
    state.scene.actors.iter().enumerate().any(|(ai, actor)| {
        if !actor.visible {
            return false;
        }
        actor_track_index(state, ai) < overlay_track
    })
}


// ─── MAIN ENTRY POINT ────────────────────────────────────────────────

/// Render the free canvas preview panel.
pub fn canvas_preview(ui: &mut egui::Ui, state: &mut EditorState) {
    // Force a per-frame repaint while a canvas drag (move / resize /
    // rotate / pan / asset-drop) is in flight so motion stays smooth
    // — egui's reactive scheduler would otherwise wait for the next
    // input event and the dragged element would trail the cursor by a
    // frame or two ("застревание" feedback from users).
    let any_pointer_down = ui.input(|i| i.pointer.any_down());
    let canvas_drag_active = state.canvas_drag.mode != crate::state::CanvasDragMode::None
        || state.asset_drag.dragging.is_some();
    if any_pointer_down || canvas_drag_active {
        ui.ctx().request_repaint();
    }

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

    // ── Mask / crop drawing tools — when one of the tools is armed
    //    we hand the pointer gesture off to the mask painter BEFORE
    //    the regular transform pipeline so a click+drag never
    //    accidentally moves the element underneath the brush.
    if mask_tool_active(state) {
        handle_mask_draw_input(ui, &response, state, full_rect, viewport_size);
    }

    // ── Draw grid ──
    draw_canvas_grid(&painter, full_rect, &state.canvas_viewport, viewport_size);

    // ── Draw elements (actors, overlays) ──
    draw_canvas_elements(ui, &painter, full_rect, state, viewport_size);

    // ── Draw element gizmo for selected ──
    draw_selection_gizmo(ui, &painter, &response, full_rect, state, viewport_size);

    // ── Draw multi-select outlines (every secondary entry in canvas_selection) ──
    draw_multi_selection_borders(&painter, full_rect, state, viewport_size);

    // ── Draw render frame LAST so it sits on top of every layer ──
    //    The render frame is the output region marker — the user must
    //    always be able to see it, including its corner/edge handles,
    //    even when actors / overlays cover the whole canvas.
    draw_render_frame(&painter, full_rect, state, viewport_size);

    // ── Fit button overlay ──
    draw_viewport_controls(ui, full_rect, state, viewport_size);

    // ── Snap guidelines for the active drag (drawn last so they're on top) ──
    draw_snap_guides(&painter, full_rect, state, viewport_size);

    // ── Marquee (rubber-band) selection rectangle, drawn above
    //    everything else so the user can see what they're lassoing.
    draw_canvas_marquee(&painter, full_rect, state, viewport_size);

    // ── Mask / crop drawing preview overlay (drawn last so the
    //    in-progress shape sits on top of every gizmo). Visual only;
    //    the actual masking is applied through the image-effects
    //    pipeline once the gesture is committed.
    draw_mask_draft(&painter, full_rect, state, viewport_size);

    // ── Library drag-to-canvas: visual ghost + drop accept ──
    handle_canvas_asset_drag(ui, state, full_rect, viewport_size);
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
    // Match the actor's playback speed so the eyedropper picks the
    // same pixel the user sees on canvas at the current playhead.
    let speed = actor.speed.max(0.0001);
    let local_t = if t >= t_in && t <= t_out {
        (t - t_in) * speed + actor.source_start
    } else if t < t_in {
        actor.source_start
    } else {
        actor.source_start + (t_out - t_in) * speed
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


// ─── MARQUEE (RUBBER-BAND) HELPERS ───────────────────────────────────
//
// The marquee lives in WORLD pixel coords on `EditorState::canvas_marquee`
// (Option<CanvasMarquee>) so it stays anchored to the canvas region the
// user lassoed regardless of pan / zoom. These helpers are used by:
//   * `apply_drag` (Marquee arm) — updates the live rectangle.
//   * `draw_canvas_marquee` — paints the semi-transparent rect on top of
//     the scene.
//   * `commit_marquee_selection` — converts the rectangle into a list of
//     selected elements when the user releases the mouse.
//   * `draw_multi_selection_borders` — paints a slim coloured outline
//     around every element currently in `state.canvas_selection`, so
//     the user can see who's "in the bag" while they keep dragging.

const COL_MARQUEE_FILL: Color32 = Color32::from_rgba_premultiplied(255, 220, 80, 30);
const COL_MARQUEE_STROKE: Color32 = Color32::from_rgb(255, 220, 80);
const COL_MULTI_SELECT_BORDER: Color32 = Color32::from_rgb(255, 180, 60);

/// World-space AABB of an actor at the current playhead. Mirrors the
/// math in `actor_screen_rect` but stays in world pixels (no canvas
/// zoom multiplier, no full_rect offset). Returns `(min, max)` corners.
fn actor_world_aabb(state: &EditorState, idx: usize) -> Option<([f32; 2], [f32; 2])> {
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
    let half_w = elem_w * 0.5;
    let half_h = elem_h * 0.5;

    Some((
        [world_pos.x - half_w, world_pos.y - half_h],
        [world_pos.x + half_w, world_pos.y + half_h],
    ))
}

/// World-space AABB of an overlay at the current playhead. Mirrors the
/// math in `draw_canvas_overlays`. Returns `(min, max)` corners.
fn overlay_world_aabb(state: &EditorState, idx: usize) -> Option<([f32; 2], [f32; 2])> {
    let overlay = state.scene.overlays.get(idx)?;
    let t = state.playhead;
    let (t_in, t_out, layout) = match overlay {
        Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
        Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
        Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
    };
    let sample_t = if t >= t_in && t <= t_out { t - t_in }
        else if t < t_in { 0.0 } else { (t_out - t_in).max(0.0) };
    let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom.max(1e-6);
    let world_h = rh as f32 / rf_state.zoom.max(1e-6);
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;
    let center_x = frame_tl_x + ov_state.pos[0] * world_w;
    let center_y = frame_tl_y + ov_state.pos[1] * world_h;

    // Use the texture-aware bbox so image overlays use real PNG
    // dimensions (not the legacy 200×200 placeholder), keeping the
    // marquee hit-rect aligned with the visible picture.
    let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
    Some((
        [center_x - ew * 0.5, center_y - eh * 0.5],
        [center_x + ew * 0.5, center_y + eh * 0.5],
    ))
}

/// AABB ∩ AABB test (both expressed as `(min, max)` world-pixel pairs).
fn aabbs_overlap(a: ([f32; 2], [f32; 2]), b: ([f32; 2], [f32; 2])) -> bool {
    let (a_min, a_max) = a;
    let (b_min, b_max) = b;
    a_min[0] <= b_max[0] && a_max[0] >= b_min[0]
        && a_min[1] <= b_max[1] && a_max[1] >= b_min[1]
}

/// Convert the world-coord marquee rectangle into a list of selected
/// elements and commit them to `state.canvas_selection`. When `extend`
/// is set, the lasso adds to whatever was already selected; otherwise
/// it replaces the set.
///
/// The primary `state.selection` is updated to the topmost (lowest
/// track index) hit so the inspector still has a sensible "focused"
/// element — but the inspector itself short-circuits to a count when
/// the lasso captured more than one item.
fn commit_marquee_selection(state: &mut EditorState, extend: bool) {
    let Some(marquee) = state.canvas_marquee else { return; };
    let (mn, mx) = marquee.rect_world();
    // Reject zero-size lassos (≤ 2 world-pixels in either dimension).
    // Treat these as an empty-area click instead of a selection paint —
    // we just clear the existing selection (unless extend was held).
    let too_small = (mx[0] - mn[0]).abs() < 2.0 || (mx[1] - mn[1]).abs() < 2.0;

    if too_small {
        if !extend {
            state.canvas_selection.clear();
            state.selection = Selection::None;
        }
        return;
    }

    let marquee_box = (mn, mx);

    // Collect every actor / overlay whose world AABB intersects the
    // marquee. Backgrounds and the render frame stay out of the lasso —
    // they're full-canvas clips and would always match.
    let mut hits: Vec<Selection> = Vec::new();
    for idx in 0..state.scene.actors.len() {
        if !state.scene.actors[idx].visible { continue; }
        if let Some(aabb) = actor_world_aabb(state, idx) {
            if aabbs_overlap(aabb, marquee_box) {
                hits.push(Selection::Actor(idx));
            }
        }
    }
    for idx in 0..state.scene.overlays.len() {
        if let Some(aabb) = overlay_world_aabb(state, idx) {
            if aabbs_overlap(aabb, marquee_box) {
                hits.push(Selection::Overlay(idx));
            }
        }
    }

    if !extend {
        state.canvas_selection.clear();
    }
    for h in hits {
        if !state.canvas_selection.contains(&h) {
            state.canvas_selection.push(h);
        }
    }

    // Pick a primary so the inspector has something to anchor on.
    // Prefer the existing primary when it's still in the set;
    // otherwise fall back to the first entry. When the set is empty
    // the primary clears too.
    if state.canvas_selection.is_empty() {
        state.selection = Selection::None;
    } else if !state.canvas_selection.iter().any(|s| *s == state.selection) {
        state.selection = state.canvas_selection[0];
    }
}

/// Paint the live marquee rectangle on top of the canvas. Called once
/// per frame from the bottom of `canvas_preview()` so it sits above
/// every element / gizmo.
fn draw_canvas_marquee(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let Some(marquee) = state.canvas_marquee else { return; };
    let (mn, mx) = marquee.rect_world();
    let tl_screen = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos { x: mn[0], y: mn[1] },
        viewport_size,
    );
    let br_screen = state.canvas_viewport.world_to_screen(
        memstroy_core::WorldPos { x: mx[0], y: mx[1] },
        viewport_size,
    );
    let rect = Rect::from_min_max(
        Pos2::new(full_rect.min.x + tl_screen[0], full_rect.min.y + tl_screen[1]),
        Pos2::new(full_rect.min.x + br_screen[0], full_rect.min.y + br_screen[1]),
    );
    painter.rect_filled(rect, Rounding::same(1.0), COL_MARQUEE_FILL);
    painter.rect_stroke(rect, Rounding::same(1.0), Stroke::new(1.0, COL_MARQUEE_STROKE));
}

/// Paint a slim outline around every element in `state.canvas_selection`
/// that is NOT the primary selection (the primary already gets the gold
/// gizmo border drawn by `draw_selection_handles`). This way the user
/// can see the full lassoed set at a glance — useful both during the
/// marquee paint (live update of who's currently inside the box) and
/// after release (so the user knows what the next move/scale/rotate
/// will broadcast to).
fn draw_multi_selection_borders(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    if state.canvas_selection.len() < 2 {
        return;
    }
    for sel in &state.canvas_selection {
        // Skip the primary — its handles/border are drawn elsewhere.
        if *sel == state.selection { continue; }
        let aabb = match *sel {
            Selection::Actor(i) => actor_world_aabb(state, i),
            Selection::Overlay(i) => overlay_world_aabb(state, i),
            _ => None,
        };
        let Some((mn, mx)) = aabb else { continue; };
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
        painter.rect_stroke(rect, Rounding::same(2.0), Stroke::new(1.5, COL_MULTI_SELECT_BORDER));
    }
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

    // Scroll wheel → zoom viewport.
    //
    // The previous step was 1.05× per wheel notch which felt aggressive
    // for fine-grained editing on the canvas. We now scale the zoom by
    // an exponential curve based on the actual scroll magnitude so the
    // user gets:
    //   * very small steps (~1.5%) per detent for precise tweaks,
    //   * proportional behaviour when the OS sends large smooth-scroll
    //     deltas (e.g. continuous trackpad pinch),
    //   * Ctrl+Wheel = larger steps for fast traversal between zoom
    //     levels (mirrors Photoshop / Figma muscle memory).
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        let ctrl = ui.input(|i| i.modifiers.ctrl);

        if scroll.y.abs() > 0.1 {
            let base = if ctrl { 0.0040_f32 } else { 0.0015_f32 };
            // Cap |dy| so a runaway momentum scroll can't multiply the
            // zoom by an extreme factor in a single frame.
            let dy = scroll.y.clamp(-120.0, 120.0);
            let factor = (base * dy).exp();
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
    // Use the eased + modifier-aware state for display so any wobble /
    // pulse / spin modifiers added to the frame visibly shake the
    // outline and (transitively) the children that live inside it.
    let rf_state = sample_render_frame_eased(rf, state.playhead);
    let [rw, rh] = rf.resolution;

    // The render frame covers (rw/zoom) x (rh/zoom) world pixels
    let world_w = rw as f32 / rf_state.zoom;
    let world_h = rh as f32 / rf_state.zoom;

    // Compute the four corners in screen-space, applying the frame's
    // own rotation around its centre. We never bake the rotation into the
    // child elements — they keep their normalised positions and naturally
    // ride along inside the frame.
    let corners_screen = render_frame_corners_screen(state, full_rect, viewport_size);
    let [tl, tr, br, bl] = corners_screen;

    // Outline only — interior stays fully transparent so canvas content
    // (background, actors) is never tinted by the render-area marker.
    let rotation_rad = rf_state.rotation_deg.to_radians();
    let is_rotated = rotation_rad.abs() > 0.001;
    if !is_rotated {
        let aabb = Rect::from_min_max(tl, br);
        painter.rect_stroke(
            aabb,
            Rounding::ZERO,
            Stroke::new(2.0, COL_RENDER_FRAME),
        );
    } else {
        painter.add(egui::Shape::convex_polygon(
            vec![tl, tr, br, bl],
            Color32::TRANSPARENT,
            Stroke::new(2.0, COL_RENDER_FRAME),
        ));
    }

    // Corner resize handles for the render frame (rotated to follow the frame).
    let handle_size = 8.0;
    for corner in &corners_screen {
        let hr = Rect::from_center_size(*corner, Vec2::splat(handle_size));
        painter.rect_filled(hr, Rounding::same(2.0), COL_RENDER_FRAME_HANDLE);
        painter.rect_stroke(hr, Rounding::same(2.0), Stroke::new(1.0, Color32::WHITE));
    }

    // Edge midpoint handles for the render frame (also rotated).
    let midpoints = [
        Pos2::new((tl.x + tr.x) * 0.5, (tl.y + tr.y) * 0.5),
        Pos2::new((bl.x + br.x) * 0.5, (bl.y + br.y) * 0.5),
        Pos2::new((tl.x + bl.x) * 0.5, (tl.y + bl.y) * 0.5),
        Pos2::new((tr.x + br.x) * 0.5, (tr.y + br.y) * 0.5),
    ];
    for mp in &midpoints {
        let hr = Rect::from_center_size(*mp, Vec2::splat(handle_size * 0.9));
        painter.rect_filled(hr, Rounding::same(2.0), COL_RENDER_FRAME_HANDLE);
    }

    // Label anchored to the (rotated) top-left corner.
    let label_pos = Pos2::new(tl.x + 4.0, tl.y - 16.0);
    if label_pos.y > full_rect.min.y {
        let _ = world_w;
        let _ = world_h;
        painter.text(
            label_pos,
            egui::Align2::LEFT_BOTTOM,
            format!("{}x{}", rw, rh),
            egui::FontId::proportional(10.0),
            COL_RENDER_FRAME,
        );
    }
}

/// Compute the four screen-space corners of the render frame (TL, TR, BR, BL).
/// Applies the frame's `rotation_deg` around its centre.
fn render_frame_corners_screen(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> [Pos2; 4] {
    let rf = &state.scene.render_frame;
    // Modifier-aware state so the corner handles follow the visible
    // frame outline drawn by `draw_render_frame`.
    let rf_state = sample_render_frame_eased(rf, state.playhead);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom.max(1e-6);
    let world_h = rh as f32 / rf_state.zoom.max(1e-6);

    let cx = rf_state.pos.x;
    let cy = rf_state.pos.y;
    let half_w = world_w * 0.5;
    let half_h = world_h * 0.5;
    let rad = rf_state.rotation_deg.to_radians();
    let cs = rad.cos();
    let sn = rad.sin();

    let corner_world = |dx: f32, dy: f32| -> WorldPos {
        WorldPos {
            x: cx + dx * cs - dy * sn,
            y: cy + dx * sn + dy * cs,
        }
    };

    let tl_w = corner_world(-half_w, -half_h);
    let tr_w = corner_world( half_w, -half_h);
    let br_w = corner_world( half_w,  half_h);
    let bl_w = corner_world(-half_w,  half_h);

    let to_screen = |w: WorldPos| -> Pos2 {
        let s = state.canvas_viewport.world_to_screen(w, viewport_size);
        Pos2::new(full_rect.min.x + s[0], full_rect.min.y + s[1])
    };

    [to_screen(tl_w), to_screen(tr_w), to_screen(br_w), to_screen(bl_w)]
}

/// Sample the RenderFrame state at time t.
fn sample_render_frame(rf: &RenderFrame, t: f32) -> RenderFrameState {
    keyframe::sample(&rf.layout, t).unwrap_or_default()
}

/// Sample the RenderFrame state with its animation modifiers layered on
/// top — mirrors the additive treatment used for actors / overlays. Used
/// for *display* (drawing the frame outline + child positioning), NOT
/// for drag math: the user authors the underlying kf-eased value, and
/// modifiers visually perturb the rendered output on top of that.
fn sample_render_frame_eased(rf: &RenderFrame, t: f32) -> RenderFrameState {
    let mut s = sample_render_frame(rf, t);
    if rf.modifiers.is_empty() { return s; }
    let delta = keyframe::evaluate_modifiers(&rf.modifiers, t);
    s.pos.x += delta.dx;
    s.pos.y += delta.dy;
    s.rotation_deg += delta.d_rotation_deg;
    if delta.d_scale.abs() > 1e-4 {
        // d_scale is in linear units; the frame's "scale" is 1/zoom, so
        // bump the zoom inversely to match a Pulse-style scale shift.
        let scale_now = (1.0 / s.zoom.max(1e-4)) + delta.d_scale;
        s.zoom = (1.0 / scale_now.max(1e-4)).clamp(0.001, 1000.0);
    }
    s
}

/// Resolve a `SkeletonAttachment` on an overlay to a WORLD position by
/// finding the host actor and projecting the normalised point through
/// the actor's current bounding box. Returns `None` when the host or
/// point can't be resolved.
fn resolve_overlay_attachment_world(
    state: &EditorState,
    attachment: &SkeletonAttachment,
    t: f32,
) -> Option<WorldPos> {
    // Find the matching template (by name or source-clip filename).
    let template = state.scene.skeleton_templates.iter().find(|tmpl| {
        tmpl.name == attachment.skeleton_id
            || tmpl.source_clip.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == attachment.skeleton_id)
                .unwrap_or(false)
    })?;
    let point_state = template.sample_point(&attachment.point_name, t)?;

    // Find the host actor that uses this template's source clip.
    let (host_idx, host_actor) = state.scene.actors.iter().enumerate().find(|(_, a)| {
        a.source == template.source_clip
            || a.source.file_name() == template.source_clip.file_name()
    })?;
    if !host_actor.visible { return None; }

    // Host actor's world center + dimensions at `t`.
    let host_world = get_element_world_pos(state, &host_actor.id, &host_actor.layout, t);
    let host_state = keyframe::sample(&host_actor.layout, t).unwrap_or_default();
    let (src_w, src_h) = if let Some(fc) = state.frame_caches.get(host_idx) {
        if fc.is_ready() && fc.frame_count > 0 {
            (fc.source_width as f32, fc.source_height as f32)
        } else { (1080.0_f32, 1920.0) }
    } else { (1080.0_f32, 1920.0) };
    let elem_w = src_w * host_state.scale;
    let elem_h = src_h * host_state.scale * host_state.scale_y;

    // Map normalised clip coords [0,1] (with offset) to world space
    // relative to the host actor's centre. Honor host rotation so the
    // attached element follows when the host actor is rotated.
    let nx = (point_state.x + attachment.offset[0]) - 0.5;
    let ny = (point_state.y + attachment.offset[1]) - 0.5;
    let local_x = nx * elem_w;
    let local_y = ny * elem_h;
    let rot = host_state.rotation_deg.to_radians();
    let cs = rot.cos();
    let sn = rot.sin();
    Some(WorldPos {
        x: host_world.x + local_x * cs - local_y * sn,
        y: host_world.y + local_x * sn + local_y * cs,
    })
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

/// Pick at most one image / video overlay per track for the preview.
/// Mirrors `pick_actors_for_canvas` for overlays so two clips that
/// share a lane don't render simultaneously on the canvas (one as
/// "current" and the other as a FIRST/LAST preview).
///
/// Text overlays are NOT filtered here — they're treated as decorative
/// callouts that may legitimately overlap on a single lane, and unlike
/// video clips they don't carry the "show first frame" semantics.
fn pick_overlays_for_canvas(state: &EditorState, t: f32) -> HashSet<usize> {
    use std::collections::HashMap;
    let mut by_track: HashMap<usize, Vec<usize>> = HashMap::new();
    for oi in 0..state.scene.overlays.len() {
        // Skip text overlays — they may overlap freely on a lane.
        if matches!(state.scene.overlays[oi], Overlay::Text(_)) { continue; }
        let lane = overlay_track_index(state, oi);
        by_track.entry(lane).or_default().push(oi);
    }
    let mut keep = HashSet::new();
    for (_lane, indices) in by_track {
        let active = indices.iter().copied().find(|&oi| {
            let (t_in, t_out) = match &state.scene.overlays[oi] {
                Overlay::Text(o) => (o.t_in, o.t_out),
                Overlay::Image(o) => (o.t_in, o.t_out),
                Overlay::Video(o) => (o.t_in, o.t_out),
            };
            t >= t_in && t <= t_out
        });
        if let Some(oi) = active {
            keep.insert(oi);
            continue;
        }
        // No clip currently active on this lane — pick the closest one
        // (in time) so the user still has a visible preview of "what
        // this lane will show".
        let best = indices.iter().copied().min_by(|&a, &b| {
            let dist = |oi: usize| -> f32 {
                let (t_in, t_out) = match &state.scene.overlays[oi] {
                    Overlay::Text(o) => (o.t_in, o.t_out),
                    Overlay::Image(o) => (o.t_in, o.t_out),
                    Overlay::Video(o) => (o.t_in, o.t_out),
                };
                if t < t_in { (t_in - t).abs() } else { (t - t_out).abs() }
            };
            dist(a)
                .partial_cmp(&dist(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(oi) = best {
            keep.insert(oi);
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
        // Modifiers (wobble/shake/pulse/spin) layered on top of the eased
        // sample. They are additive and only run while the clip is active
        // — outside the clip's window we use the raw sample so the static
        // first/last preview doesn't shake.
        let mod_delta = if matches!(display_mode, DisplayMode::Active) {
            keyframe::evaluate_modifiers(&actor.modifiers, t - t_in)
        } else {
            keyframe::ModifierDelta::default()
        };
        let actor_rotation = actor_state.rotation_deg + mod_delta.d_rotation_deg;
        let actor_opacity = actor_state.opacity;
        let actor_flip_x = actor_state.flip_x_anim;
        let actor_flip_y = actor_state.flip_y_anim;
        // Apply per-axis scale: scale_y stretches Y on top of uniform scale.
        // Modifiers are added uniformly to keep the aspect ratio sensible.
        let scale_eff = (actor_scale + mod_delta.d_scale).max(0.001);
        let elem_width = src_w * scale_eff;
        let elem_height = src_h * scale_eff * actor_scale_y;

        // Convert to screen coordinates
        // Modifier (shake/wobble) offsets are added in world pixels.
        let world_pos_with_mod = WorldPos {
            x: world_pos.x + mod_delta.dx,
            y: world_pos.y + mod_delta.dy,
        };
        let center_screen = state.canvas_viewport.world_to_screen(world_pos_with_mod, viewport_size);
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

        // Try to show actual frame from cache. Apply `actor.speed` to
        // the source-time so a >1.0 multiplier actually plays the file
        // faster (consumes more source seconds per scene-second) — the
        // previous formula stretched the visible window without
        // touching the frame index, which made fast-mode look identical
        // to a regular trim.
        let speed = actor.speed.max(0.0001);
        let local_t = match display_mode {
            DisplayMode::Active => (t - t_in) * speed + actor.source_start,
            DisplayMode::BeforeStart => actor.source_start, // first frame
            DisplayMode::AfterEnd => {
                // last frame: source_start + visible_dur_in_source
                actor.source_start + (t_out - t_in) * speed
            }
        };

        let mut frame_shown = false;
        if let Some(fc) = state.frame_caches.get_mut(idx) {
            if fc.is_ready() {
                // Apply chromakey on the raw frame data if actor has non-default settings
                let actor_ck = &state.scene.actors[idx].chroma_key;
                // Sample CC + effect-stack params at the actor's local
                // playhead so animated diamonds materialise into the
                // running preview. Without this the inspector would
                // show kfs but the picture would stay frozen at the
                // static field values.
                let actor_t_in = state.scene.actors[idx].t_in.unwrap_or(0.0);
                let local_for_anim = (state.playhead - actor_t_in).max(0.0);
                let actor_cc_owned: memstroy_core::ColorCorrection =
                    state.scene.actors[idx].color_correction.sampled_at(local_for_anim);
                let actor_cc = &actor_cc_owned;
                let actor_fx_owned: Vec<memstroy_core::Effect> = state
                    .scene
                    .actors[idx]
                    .effects
                    .iter()
                    .map(|e| e.sampled_at(local_for_anim))
                    .collect();
                let actor_fx = &actor_fx_owned;
                // Bypass the (expensive) preview pipeline when chroma /
                // colour correction / and the effect stack are all
                // empty / identity. Otherwise route through the processed
                // path which layers the user's effect stack on top.
                let any_fx_active = actor_fx.iter().any(|e| e.enabled && e.intensity > 0.001);
                let has_effects = actor_ck.similarity > 0.01
                    || !actor_cc.is_identity()
                    || any_fx_active;

                let texture = if has_effects {
                    fc.processed_frame_at_time(local_t, actor_ck, actor_cc, actor_fx, ui.ctx())
                } else {
                    fc.frame_at_time(local_t, ui.ctx())
                };

                if let Some(tex) = texture {
                    let rotation_rad = actor_rotation.to_radians();
                    // Flip + 3D-fold: the local half-extents shrink with
                    // |flip_x_anim| / |flip_y_anim| so a value going from 1
                    // through 0 to −1 produces a "card-flip" silhouette.
                    // Static `flip_horizontal` still toggles UV mirroring.
                    let static_hflip = state.scene.actors[idx].flip_horizontal;
                    let combined_x = if static_hflip { -actor_flip_x } else { actor_flip_x };
                    let combined_y = actor_flip_y;
                    let abs_fx = combined_x.abs().max(0.02);
                    let abs_fy = combined_y.abs().max(0.02);
                    let center = elem_rect.center();
                    let hw = elem_rect.width() * 0.5 * abs_fx;
                    let hh = elem_rect.height() * 0.5 * abs_fy;
                    let cos_r = rotation_rad.cos();
                    let sin_r = rotation_rad.sin();
                    let corners_local = [[-hw, -hh], [hw, -hh], [hw, hh], [-hw, hh]];
                    // Build UV corners that respect the sign of the flip.
                    let (uv_l, uv_r) = if combined_x < 0.0 { (1.0, 0.0) } else { (0.0, 1.0) };
                    let (uv_t, uv_b) = if combined_y < 0.0 { (1.0, 0.0) } else { (0.0, 1.0) };
                    let uv_corners = [
                        Pos2::new(uv_l, uv_t), Pos2::new(uv_r, uv_t),
                        Pos2::new(uv_r, uv_b), Pos2::new(uv_l, uv_b),
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

        // Border. Primary selection gets the bright yellow gizmo
        // border; multi-selection (Ctrl+click) shows a slimmer dashed
        // border so the user can see every element in the set without
        // confusing it with the primary "edit me" target.
        let multi_selected = state
            .canvas_selection
            .iter()
            .any(|s| *s == Selection::Actor(idx));
        let border_col = if state.selection == Selection::Actor(idx) {
            COL_SELECTED_BORDER
        } else if multi_selected {
            Color32::from_rgb(255, 180, 60)
        } else {
            COL_ELEMENT_BORDER
        };
        let border_width = if state.selection == Selection::Actor(idx) {
            2.0
        } else if multi_selected {
            1.5
        } else {
            1.0
        };
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
    /// Overlays that should render UNDER the actors. An overlay is
    /// classified into this pass dynamically when its timeline row sits
    /// below at least one actor's row (smaller track index = higher on
    /// the panel = drawn on top).
    BehindActors,
    /// All remaining overlays — rendered after the actors so they end up
    /// on top of them.
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

    // ── Per-lane preview filter ──
    // For image / video overlays we only want to draw ONE per lane
    // (the active clip if any, or the temporally closest one as a
    // preview). Text overlays are exempt — see `pick_overlays_for_canvas`
    // for the rationale.
    let overlays_to_draw = pick_overlays_for_canvas(state, t);

    // Build a sorted list of overlay indices for this pass.
    // Sort key = track index, DESCENDING — i.e. the lowest row on the
    // panel renders FIRST so the topmost row ends up on top. Within the
    // same track, draw in scene order to keep behaviour stable.
    //
    // The pass split is also driven by the panel: any overlay (text or
    // media) sitting on a row BELOW one of the actor rows renders in the
    // BehindActors pass; everything else renders OnTop. That keeps the
    // "below actors" semantics auto-derived from the timeline layout
    // for every overlay kind, not just text.
    let mut order: Vec<(usize, usize)> = state.scene.overlays.iter().enumerate()
        .filter(|(idx, ov)| {
            let behind = overlay_is_behind_actors(state, *idx);
            let kind_ok = match pass {
                OverlayPass::BehindActors => behind,
                OverlayPass::OnTop => !behind,
            };
            if !kind_ok { return false; }
            // Image / Video overlays on the same lane: only the chosen
            // one. Text overlays bypass the filter (always allowed).
            match ov {
                Overlay::Text(_) => true,
                _ => overlays_to_draw.contains(idx),
            }
        })
        .map(|(idx, _)| (idx, overlay_track_index(state, idx)))
        .collect();
    // Larger track index = lower on the panel = drawn FIRST (so the
    // smaller-index rows end up painted last and on top). Within the
    // same row, scene order acts as the tie-breaker.
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

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
        let mut ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

        // Apply animation modifiers (additive on top of eased keyframe).
        let modifiers: &[TrackModifier] = match overlay {
            Overlay::Text(txt) => &txt.modifiers,
            Overlay::Image(img) => &img.modifiers,
            Overlay::Video(vid) => &vid.modifiers,
        };
        let mod_delta = if matches!(display_mode, DisplayMode::Active) {
            keyframe::evaluate_modifiers(modifiers, sample_t)
        } else {
            keyframe::ModifierDelta::default()
        };
        ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
        ov_state.rotation_deg += mod_delta.d_rotation_deg;

        let rf = &state.scene.render_frame;
        let rf_state = sample_render_frame(rf, t);
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32 / rf_state.zoom;
        let world_h = rh as f32 / rf_state.zoom;
        let frame_tl_x = rf_state.pos.x - world_w * 0.5;
        let frame_tl_y = rf_state.pos.y - world_h * 0.5;

        // Default world position from layout. Modifier offsets and any
        // skeleton attachment can override / shift it.
        let mut world_pos = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w + mod_delta.dx,
            y: frame_tl_y + ov_state.pos[1] * world_h + mod_delta.dy,
        };

        // Skeleton attachment: if this overlay is bound to a host actor's
        // skeleton point, compute the point's world position from the
        // host's current bounding box and override `world_pos`.
        let skel_att: Option<&SkeletonAttachment> = match overlay {
            Overlay::Text(txt) => txt.skeleton_attachment.as_ref(),
            Overlay::Image(img) => img.skeleton_attachment.as_ref(),
            Overlay::Video(vid) => vid.skeleton_attachment.as_ref(),
        };
        if let Some(att) = skel_att {
            if let Some(p) = resolve_overlay_attachment_world(state, att, t) {
                world_pos = p;
            }
        }
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
                let zoom = state.canvas_viewport.zoom;
                let real_size = ensure_image_loaded(state, &img.source, painter.ctx());
                // Fall back to a 200×200 logical box when the file
                // hasn't been decoded (yet or at all). Once the texture
                // is loaded, the real PNG dimensions drive the bbox so
                // resize handles snap to the picture's edges.
                let (sw, sh) = real_size.unwrap_or((200, 200));
                let elem_w = sw as f32 * ov_state.scale;
                let elem_h = sh as f32 * ov_state.scale * ov_state.scale_y;
                let half_w = elem_w * 0.5 * zoom;
                let half_h = elem_h * 0.5 * zoom;
                let elem_rect =
                    Rect::from_center_size(center_pos, Vec2::new(half_w * 2.0, half_h * 2.0));
                if !full_rect.intersects(elem_rect) { continue; }

                let tex_handle: Option<egui::TextureHandle> = state
                    .image_textures
                    .lock()
                    .ok()
                    .and_then(|map| match map.get(&img.source) {
                        Some(crate::state::ImageTextureSlot::Loaded { texture, .. }) => {
                            Some(texture.clone())
                        }
                        _ => None,
                    });

                // ── Effect-baked override ──
                // When the overlay carries any active effect entries,
                // build (or fetch from cache) a CPU-processed texture
                // and prefer it for drawing. Crop entries also return a
                // UV inset — applied below to shrink the visible
                // rectangle so the picture mirrors the FFmpeg export.
                let mut crop_inset = [0.0_f32; 4];
                let fx_tex_handle: Option<egui::TextureHandle> =
                    if !img.effects.is_empty() && tex_handle.is_some() {
                        match ensure_image_fx_loaded(state, &img.source, &img.effects, painter.ctx()) {
                            Some((tex, crop)) => {
                                crop_inset = crop;
                                Some(tex)
                            }
                            None => None,
                        }
                    } else {
                        None
                    };
                let tex_for_draw = fx_tex_handle.or(tex_handle);

                if let Some(tex) = tex_for_draw {
                    let rotation_rad = ov_state.rotation_deg.to_radians();
                    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
                    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
                    let cos_r = rotation_rad.cos();
                    let sin_r = rotation_rad.sin();
                    let center = elem_rect.center();
                    // Crop inset shrinks the picture's screen rect so
                    // the user sees the same crop rectangle the export
                    // will produce (the inset is in normalised 0..1).
                    let crop_w_factor = (1.0 - crop_inset[0] - crop_inset[2]).max(0.001);
                    let crop_h_factor = (1.0 - crop_inset[1] - crop_inset[3]).max(0.001);
                    let crop_dx = (crop_inset[0] - crop_inset[2]) * 0.5; // recentre after asymmetric crop
                    let crop_dy = (crop_inset[1] - crop_inset[3]) * 0.5;
                    let hw = elem_rect.width() * 0.5 * abs_fx * crop_w_factor;
                    let hh = elem_rect.height() * 0.5 * abs_fy * crop_h_factor;
                    let center_offset_x = elem_rect.width() * 0.5 * crop_dx;
                    let center_offset_y = elem_rect.height() * 0.5 * crop_dy;
                    let centre_offset = Vec2::new(
                        center_offset_x * cos_r - center_offset_y * sin_r,
                        center_offset_x * sin_r + center_offset_y * cos_r,
                    );
                    let center = center + centre_offset;
                    let corners_local = [[-hw, -hh], [hw, -hh], [hw, hh], [-hw, hh]];
                    // ── UV inset by crop ──
                    //
                    // Without the inset, the on-canvas rectangle was
                    // shrunk to the cropped extent BUT the full 0..1
                    // texture was still mapped onto it — so the picture
                    // got *squished* into a narrower box instead of
                    // being cropped (the user's "crop compresses
                    // instead of cuts" report). Mapping the uncropped
                    // sub-rectangle of the texture to the cropped on-
                    // canvas rectangle restores true crop semantics:
                    // pixels outside the crop region are simply not
                    // shown, the visible pixels keep their aspect ratio.
                    let uv_l_base = crop_inset[0].clamp(0.0, 0.99);
                    let uv_t_base = crop_inset[1].clamp(0.0, 0.99);
                    let uv_r_base = (1.0 - crop_inset[2]).clamp(uv_l_base + 1.0e-3, 1.0);
                    let uv_b_base = (1.0 - crop_inset[3]).clamp(uv_t_base + 1.0e-3, 1.0);
                    let (uv_l, uv_r) = if ov_state.flip_x_anim < 0.0 {
                        (uv_r_base, uv_l_base)
                    } else {
                        (uv_l_base, uv_r_base)
                    };
                    let (uv_t, uv_b) = if ov_state.flip_y_anim < 0.0 {
                        (uv_b_base, uv_t_base)
                    } else {
                        (uv_t_base, uv_b_base)
                    };
                    let uv_corners = [
                        Pos2::new(uv_l, uv_t), Pos2::new(uv_r, uv_t),
                        Pos2::new(uv_r, uv_b), Pos2::new(uv_l, uv_b),
                    ];
                    let alpha_factor = match display_mode {
                        DisplayMode::Active => 1.0,
                        _ => 0.5,
                    };
                    let a = (ov_state.opacity * alpha_factor * 255.0).clamp(0.0, 255.0) as u8;
                    let tint = Color32::from_rgba_unmultiplied(255, 255, 255, a);
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

                    // Selection / FIRST/LAST badges still useful even
                    // when the picture is fully drawn.
                    let is_selected = state.selection == Selection::Overlay(idx);
                    if is_selected {
                        painter.rect_stroke(
                            elem_rect,
                            Rounding::same(2.0),
                            Stroke::new(2.0, COL_SELECTED_BORDER),
                        );
                    }
                    if display_mode != DisplayMode::Active {
                        let badge = match display_mode {
                            DisplayMode::BeforeStart => "FIRST",
                            DisplayMode::AfterEnd => "LAST",
                            _ => "",
                        };
                        painter.text(
                            Pos2::new(elem_rect.min.x + 4.0, elem_rect.min.y + 4.0),
                            egui::Align2::LEFT_TOP,
                            badge,
                            egui::FontId::proportional(9.0),
                            Color32::from_rgb(255, 180, 80),
                        );
                    }
                } else {
                    // Decode failed or asset is genuinely missing.
                    // Fall back to the labelled placeholder so the user
                    // can still see WHERE the overlay is and remove it.
                    draw_overlay_placeholder(
                        painter, elem_rect, COL_OVERLAY_IMAGE, idx, state,
                        &format!(
                            "IMG (missing): {}",
                            img.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                        ),
                        display_mode,
                    );
                }
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
/// rounded corners, plate border, glyph stroke, alignment, opacity, italic,
/// and rotation around the plate centre.
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

    // Effective font size in screen pixels = font_size * canvas_zoom only.
    // The Scale slider on a Text overlay used to multiply the glyph size
    // too, which made the inspector confusing — `font_size` says "Size"
    // but the on-screen glyphs grew or shrank with `scale` as well.
    // Scale is now reserved for the **background plate** (padding,
    // corner radius, asymmetric extras): glyphs follow `font_size`
    // alone, the plate around them follows `scale`.
    let zoom = state.canvas_viewport.zoom;
    let effective_size = (style.font_size * zoom).clamp(4.0, 1024.0);
    // Italic is faked via a horizontal skew on each glyph row. Slightly
    // larger than the previous 0.18 so the slant reads at small sizes.
    let italic_skew = if style.italic { 0.22 } else { 0.0 };
    let rotation_rad = ov_state.rotation_deg.to_radians();
    let rotated = rotation_rad.abs() > 0.001;

    // Logical font family: "Monospace" → bundled mono font, anything
    // else → bundled proportional. Real custom-font loading still needs
    // bundled TTFs (deferred); the field is kept so existing scenes
    // round-trip unchanged.
    let family = if style.font.eq_ignore_ascii_case("Monospace")
        || style.font.eq_ignore_ascii_case("Courier")
        || style.font.eq_ignore_ascii_case("Hack")
    {
        egui::FontFamily::Monospace
    } else {
        egui::FontFamily::Proportional
    };
    let font_id = egui::FontId::new(effective_size, family);

    // Per-line layout: split text into lines and measure each in egui.
    let lines: Vec<&str> = if txt.text.is_empty() { vec![" "] } else { txt.text.lines().collect() };
    let line_h = effective_size * 1.2;

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

    // Padding/border in screen pixels (scaled by zoom for visual consistency).
    // `box_padding` is the symmetric padding around the text; `box_extra_left`
    // and `box_extra_right` widen the plate **outwards** (without changing
    // the text scale or its anchor) so the user can ask for "background a
    // bit wider on the left only" and combine it with TextAlign for proper
    // typography (left-aligned text on a left-wide plate looks like a
    // banner with right-padding, etc.). The text's natural anchor stays
    // on `center_pos`.
    let padding = (style.box_padding * zoom * ov_state.scale).max(0.0);
    let pad_extra_l = (style.box_extra_left * zoom * ov_state.scale).max(0.0);
    let pad_extra_r = (style.box_extra_right * zoom * ov_state.scale).max(0.0);
    let radius = (style.box_corner_radius * zoom * ov_state.scale).max(0.0);
    let plate_h = total_h + padding * 2.0;

    // Compose plate min/max around `center_pos` directly so the L/R
    // extras can extend asymmetrically without disturbing the rotation
    // pivot or the on-canvas drag anchor (both follow `center_pos`).
    let half_text = max_line_w * 0.5;
    let plate_min_x = center_pos.x - half_text - padding - pad_extra_l;
    let plate_max_x = center_pos.x + half_text + padding + pad_extra_r;
    let plate_min_y = center_pos.y - plate_h * 0.5;
    let plate_max_y = center_pos.y + plate_h * 0.5;
    let plate_rect = Rect::from_min_max(
        Pos2::new(plate_min_x, plate_min_y),
        Pos2::new(plate_max_x, plate_max_y),
    );
    let plate_w = plate_rect.width();

    // Skip if completely off-screen (use a generous rotation-aware margin)
    let bbox_margin = if rotated { plate_w.max(plate_h) } else { 50.0 };
    if !full_rect.intersects(plate_rect.expand(bbox_margin)) { return; }

    let alpha_factor = match display_mode { DisplayMode::Active => 1.0, _ => 0.5 };
    let plate_opacity = style.box_opacity.clamp(0.0, 1.0) * ov_state.opacity * alpha_factor;

    // Helper: rotate a point around `center_pos` by `rotation_rad`.
    let rotate = |p: Pos2| -> Pos2 {
        if !rotated { return p; }
        let dx = p.x - center_pos.x;
        let dy = p.y - center_pos.y;
        let c = rotation_rad.cos();
        let s = rotation_rad.sin();
        Pos2::new(center_pos.x + dx * c - dy * s, center_pos.y + dx * s + dy * c)
    };

    // ─── Plate background ────────────────────────────────────────
    if let Some(box_color) = style.box_color {
        let primary = Color32::from_rgba_unmultiplied(
            box_color[0], box_color[1], box_color[2],
            (plate_opacity * 255.0) as u8,
        );

        if rotated {
            // Rotated plate: draw as a 4-vertex convex polygon. Rounded
            // corners and gradients are deliberately dropped here — they
            // are not worth approximating in screen space when rotated.
            if !matches!(style.box_kind, TextBoxKind::None | TextBoxKind::OutlineOnly) {
                let pts = vec![
                    rotate(plate_rect.left_top()),
                    rotate(plate_rect.right_top()),
                    rotate(plate_rect.right_bottom()),
                    rotate(plate_rect.left_bottom()),
                ];
                painter.add(egui::Shape::convex_polygon(pts, primary, Stroke::NONE));
            }

            // Plate border (rotated)
            if style.box_outline_width > 0.0 || matches!(style.box_kind, TextBoxKind::OutlineOnly) {
                let border_color_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
                let border_color = Color32::from_rgba_unmultiplied(
                    border_color_rgb[0], border_color_rgb[1], border_color_rgb[2],
                    (plate_opacity * 255.0) as u8,
                );
                let bw = if style.box_outline_width > 0.0 {
                    style.box_outline_width * zoom
                } else {
                    2.0
                };
                let pts = vec![
                    rotate(plate_rect.left_top()),
                    rotate(plate_rect.right_top()),
                    rotate(plate_rect.right_bottom()),
                    rotate(plate_rect.left_bottom()),
                    rotate(plate_rect.left_top()),
                ];
                for w in pts.windows(2) {
                    painter.line_segment([w[0], w[1]], Stroke::new(bw, border_color));
                }
            }
        } else {
            match style.box_kind {
                TextBoxKind::None => {}
                TextBoxKind::Solid => {
                    painter.rect_filled(plate_rect, Rounding::same(radius), primary);
                }
                TextBoxKind::Gradient => {
                    let end_color_rgb = style.box_gradient_end.unwrap_or(box_color);
                    let end = Color32::from_rgba_unmultiplied(
                        end_color_rgb[0], end_color_rgb[1], end_color_rgb[2],
                        (plate_opacity * 255.0) as u8,
                    );
                    draw_vertical_gradient(painter, plate_rect, radius, primary, end);
                }
                TextBoxKind::OutlineOnly => {}
            }

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
    }

    // ─── Glyph stroke (poor man's outline) ───────────────────────
    let stroke_w = (style.outline_width * zoom * ov_state.scale).max(0.0);
    let glyph_color = apply_alpha(
        Color32::from_rgb(style.color[0], style.color[1], style.color[2]),
        ov_state.opacity, display_mode);

    // Compute starting Y for vertical centering
    let mut y = plate_rect.center().y - total_h * 0.5 + line_h * 0.5;
    let center_x = plate_rect.center().x;

    // Bold synthesis: with the bundled font there is no real bold variant,
    // so we emulate weight by repainting each line with a sub-pixel
    // horizontal offset. Two extra passes feel close enough to a bold
    // weight without ghosting.
    let bold_offsets: &[f32] = if style.bold {
        &[0.0, 0.7, 1.4]
    } else {
        &[0.0]
    };

    // Flip channels — passed through to `paint_text_line_flipped` so
    // each line's glyph quads get mirrored around the row centre when
    // the user pulls the Flip X / Flip Y slider negative. Mirrors the
    // semantics already in place for actor / image overlays.
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;

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
                    paint_text_line_flipped(
                        painter, pos + off, &lines[li], font_id.clone(),
                        stroke_color, rotation_rad, center_pos, flip_x, flip_y,
                    );
                }
            }
        }

        // Main glyph fill — repeated once per bold offset to synthesise
        // weight on the bundled font.
        for &dx in bold_offsets {
            let p = if dx > 0.0 { pos + Vec2::new(dx, 0.0) } else { pos };
            paint_text_line_flipped(
                painter, p, &lines[li], font_id.clone(), glyph_color,
                rotation_rad, center_pos, flip_x, flip_y,
            );
        }
        y += line_h;
    }

    // ─── Selection border ─────────────────────────────────────────
    let is_selected = state.selection == Selection::Overlay(idx);
    if is_selected {
        if rotated {
            let pts = vec![
                rotate(plate_rect.left_top()),
                rotate(plate_rect.right_top()),
                rotate(plate_rect.right_bottom()),
                rotate(plate_rect.left_bottom()),
                rotate(plate_rect.left_top()),
            ];
            for w in pts.windows(2) {
                painter.line_segment([w[0], w[1]], Stroke::new(2.0, COL_SELECTED_BORDER));
            }
        } else {
            painter.rect_stroke(plate_rect.expand(2.0), Rounding::same(radius + 2.0),
                Stroke::new(2.0, COL_SELECTED_BORDER));
        }
    } else if display_mode == DisplayMode::Active && !rotated {
        painter.rect_stroke(plate_rect, Rounding::same(radius),
            Stroke::new(0.5, Color32::from_rgba_unmultiplied(120, 200, 140, 60)));
    }

    // Display mode badge (kept axis-aligned for legibility)
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

/// Paint a single line of text, optionally rotated around `pivot` by
/// `flip_x` / `flip_y` mirror the rendered glyphs around the line's
/// own centre when the corresponding flip channel is < 0. egui's
/// `TextShape` has no per-axis flip field, so to actually mirror the
/// glyph shapes (not just the layout direction) we build a custom
/// `Mesh` from the galley's per-row tessellated mesh, then reflect
/// each vertex's screen position around `row.rect`. UVs are kept the
/// same — the position flip combined with unchanged texture sampling
/// is what gives a visually mirrored glyph.
///
/// When no flip is requested we fall back to the cheap painter.text
/// (no rotation) or to TextShape::with_angle (rotation only) paths
/// to avoid building the per-glyph mesh on every frame.
#[allow(clippy::too_many_arguments)]
fn paint_text_line_flipped(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font_id: egui::FontId,
    color: Color32,
    rotation_rad: f32,
    pivot: Pos2,
    flip_x: bool,
    flip_y: bool,
) {
    if !flip_x && !flip_y {
        if rotation_rad.abs() < 0.001 {
            painter.text(pos, egui::Align2::LEFT_TOP, text, font_id, color);
            return;
        }

        // Rotate the anchor `pos` around `pivot`, then ask egui to render the
        // glyphs themselves at that rotation via TextShape::with_angle.
        let dx = pos.x - pivot.x;
        let dy = pos.y - pivot.y;
        let c = rotation_rad.cos();
        let s = rotation_rad.sin();
        let rotated_pos = Pos2::new(pivot.x + dx * c - dy * s, pivot.y + dx * s + dy * c);

        let job = egui::text::LayoutJob::simple_singleline(text.to_string(), font_id, color);
        let galley = painter.layout_job(job);
        let mut shape = egui::epaint::TextShape::new(rotated_pos, galley, color);
        shape.angle = rotation_rad;
        painter.add(egui::Shape::Text(shape));
        return;
    }

    // Flip path: build a custom Mesh where each glyph quad is reflected
    // around the row's logical rectangle. This mirrors the GLYPH SHAPES
    // (not just their order) — that's what the user's "Flip" axis means
    // on actor / image layers, and we want consistent semantics for
    // text overlays too.
    let job = egui::text::LayoutJob::simple_singleline(text.to_string(), font_id, color);
    let galley = painter.layout_job(job);

    let font_tex_size: [usize; 2] = painter.ctx().fonts(|f| f.font_image_size());
    let uv_norm = Vec2::new(
        1.0 / font_tex_size[0].max(1) as f32,
        1.0 / font_tex_size[1].max(1) as f32,
    );

    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();

    let mut mesh = egui::Mesh::with_texture(egui::TextureId::default());
    for row in &galley.rows {
        if row.visuals.mesh.is_empty() {
            continue;
        }
        let row_rect = row.rect;
        let row_left = row_rect.min.x;
        let row_right = row_rect.max.x;
        let row_top = row_rect.min.y;
        let row_bottom = row_rect.max.y;

        let idx_offset = mesh.vertices.len() as u32;
        for &i in &row.visuals.mesh.indices {
            mesh.indices.push(i + idx_offset);
        }
        for vtx in &row.visuals.mesh.vertices {
            let mut local_x = vtx.pos.x;
            let mut local_y = vtx.pos.y;
            if flip_x {
                local_x = row_right - (local_x - row_left);
            }
            if flip_y {
                local_y = row_bottom - (local_y - row_top);
            }
            // Translate to absolute then rotate around pivot.
            let abs_x = pos.x + local_x;
            let abs_y = pos.y + local_y;
            let dx = abs_x - pivot.x;
            let dy = abs_y - pivot.y;
            let rx = pivot.x + dx * cos_r - dy * sin_r;
            let ry = pivot.y + dx * sin_r + dy * cos_r;

            let resolved_color = if vtx.color == Color32::PLACEHOLDER {
                color
            } else {
                vtx.color
            };

            mesh.vertices.push(egui::epaint::Vertex {
                pos: Pos2::new(rx, ry),
                uv: (vtx.uv.to_vec2() * uv_norm).to_pos2(),
                color: resolved_color,
            });
        }
    }
    if !mesh.indices.is_empty() {
        painter.add(egui::Shape::Mesh(mesh));
    }
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
    // ── Skeleton-attachment override (actors only) ──
    // When this element is an actor that has at least one
    // `skeleton_attachments` binding, snap its world centre to the
    // first bound point. Without this the actor was rendered at its
    // legacy `layout` position and IGNORED the attachment, which
    // surfaced as the user-visible "attached video clip just plays
    // without moving" bug. Only the first attachment is used as the
    // primary anchor; downstream code can still apply offsets / scale.
    if let Some(actor) = state.scene.actors.iter().find(|a| a.id == element_id) {
        if let Some(att) = actor.skeleton_attachments.first() {
            if let Some(p) = resolve_overlay_attachment_world(state, att, t) {
                return p;
            }
        }
    }
    // Same override path for any overlay that's bound to a skeleton
    // point — `draw_canvas_overlays` already calls
    // `resolve_overlay_attachment_world` directly, but auxiliary
    // codepaths (selection gizmos, snapping guides, drag origin, …)
    // call `get_element_world_pos` with the overlay id and need the
    // attachment to be honoured here too so the gizmo doesn't drift
    // off the picture's centre when an attachment is active.
    if let Some(att) = state.scene.overlays.iter().find_map(|ov| match ov {
        Overlay::Text(o)  if o.id == element_id => o.skeleton_attachment.as_ref(),
        Overlay::Image(o) if o.id == element_id => o.skeleton_attachment.as_ref(),
        Overlay::Video(o) if o.id == element_id => o.skeleton_attachment.as_ref(),
        _ => None,
    }) {
        if let Some(p) = resolve_overlay_attachment_world(state, att, t) {
            return p;
        }
    }

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
    // Mask / crop drawing tools own the pointer for the duration of
    // their gesture — render the gizmo handles for visual context but
    // skip the drag-state machine so the regular Move/Resize handlers
    // don't fight the mask painter.
    if mask_tool_active(state) {
        draw_selection_handles(painter, full_rect, state, viewport_size);
        return;
    }

    // Drag state machine
    if response.drag_started() {
        if let Some(start) = response.interact_pointer_pos() {
            let local = [start.x - full_rect.min.x, start.y - full_rect.min.y];
            let world = state.canvas_viewport.screen_to_world(local, viewport_size);
            let modifiers = ui.input(|i| i.modifiers);
            let extend_marquee = modifiers.ctrl || modifiers.shift || modifiers.command;
            state.canvas_drag.start_screen = local;
            state.canvas_drag.mode = decide_drag_mode(
                state,
                full_rect,
                viewport_size,
                start,
                world,
                extend_marquee,
            );

            // Freeze the playhead for the rest of the gesture so every
            // keyframe upsert lands on the same `t` (one kf per drag,
            // even when playback is running at drag start). This alone
            // prevents the "thousand kfs" bug — every write during the
            // drag re-anchors to `drag_start_playhead`, not the live
            // playhead — so we deliberately DO NOT pause playback when
            // the user grabs an element. Preview keeps running while
            // they reposition / resize / rotate.
            state.canvas_drag.drag_start_playhead = Some(state.playhead);
            state.canvas_drag.was_playing_at_drag_start = state.playing;

            // ── Render-frame drags must KEEP child elements visually
            // ── pinned to the canvas. ──
            // Overlays (and legacy actors without a canvas_layouts
            // entry) store their `pos` as a normalised [0..1] vector
            // relative to the render frame. If we leave that alone
            // while the user drags the frame, every child rides along
            // with the frame — which is NOT what users expect when
            // they "grab the output region rectangle". So at drag
            // start we snapshot every child's WORLD position and on
            // every drag tick we re-derive their normalised `pos` so
            // the world position survives. The compensation is
            // applied in `apply_drag`'s MoveRenderFrame / ResizeRenderFrame
            // arms after the frame's new state is written.
            use crate::state::CanvasDragMode;
            match state.canvas_drag.mode {
                CanvasDragMode::MoveRenderFrame { .. }
                | CanvasDragMode::ResizeRenderFrame { .. } => {
                    state.selection = Selection::RenderFrame;
                    state.canvas_drag.actor_legacy_snapshot =
                        snapshot_legacy_actor_world_positions(state);
                    state.canvas_drag.overlay_world_snapshot =
                        snapshot_overlay_world_positions(state);
                }
                CanvasDragMode::Marquee { start_world, .. } => {
                    // Initialise the live marquee at a zero-size box on
                    // the press point so the very first paint already
                    // shows the rectangle (it then grows with apply_drag).
                    state.canvas_marquee = Some(crate::state::CanvasMarquee {
                        start: start_world,
                        end: start_world,
                    });
                    state.canvas_drag.actor_legacy_snapshot.clear();
                    state.canvas_drag.overlay_world_snapshot.clear();
                    state.canvas_drag.multi_drag_snapshot.clear();
                }
                _ => {
                    state.canvas_drag.actor_legacy_snapshot.clear();
                    state.canvas_drag.overlay_world_snapshot.clear();
                    // Snapshot every entry of canvas_selection (when the
                    // user has lassoed more than one element) so the
                    // active transform mode broadcasts to all of them.
                    // The snapshot captures the current world centre,
                    // scale and rotation — apply_drag adds the primary's
                    // accumulated delta to each entry per frame so the
                    // group moves together without drift.
                    state.canvas_drag.multi_drag_snapshot =
                        snapshot_multi_drag(state);
                }
            }
        }
    } else if response.drag_stopped() || !response.dragged() {
        if !response.dragged() && state.canvas_drag.mode != crate::state::CanvasDragMode::None {
            // ── Marquee commit ──
            // The drag is ending (or the response says the pointer is
            // no longer down). When the active mode was a marquee, we
            // resolve which elements lie inside the rectangle and
            // populate `canvas_selection` accordingly. Done here, on
            // drag-end, so the user sees live preview of who's inside
            // while they drag (handled by per-element highlights), but
            // commit only happens once on release.
            if let crate::state::CanvasDragMode::Marquee { extend, .. } =
                state.canvas_drag.mode
            {
                commit_marquee_selection(state, extend);
                state.canvas_marquee = None;
            }
            state.canvas_drag.mode = crate::state::CanvasDragMode::None;
            state.canvas_drag.actor_legacy_snapshot.clear();
            state.canvas_drag.overlay_world_snapshot.clear();
            state.canvas_drag.multi_drag_snapshot.clear();
            state.canvas_drag.snap_guides.clear();
            // Release the frozen-playhead lock; subsequent inspector
            // edits go back to using the live playhead.
            state.canvas_drag.drag_start_playhead = None;
            state.canvas_drag.was_playing_at_drag_start = false;
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
            let modifiers = ui.input(|i| i.modifiers);
            let extend = modifiers.ctrl || modifiers.shift || modifiers.command;
            try_select_at(state, click_world);
            if extend {
                // Ctrl/Shift+click toggles the clicked element in the
                // canvas multi-selection. Empty hits (Selection::None)
                // are ignored so the modifier-click never accidentally
                // wipes the existing set.
                if state.selection != Selection::None {
                    let sel = state.selection;
                    if let Some(pos) = state.canvas_selection.iter().position(|s| *s == sel) {
                        state.canvas_selection.remove(pos);
                    } else {
                        state.canvas_selection.push(sel);
                    }
                }
            } else {
                // Plain click: replace the multi-selection with the
                // single hit (or clear it if nothing was hit).
                state.canvas_selection.clear();
                if state.selection != Selection::None {
                    state.canvas_selection.push(state.selection);
                }
            }
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
    extend_marquee: bool,
) -> crate::state::CanvasDragMode {
    use crate::state::CanvasDragMode;

    // 0. Rotation handle takes priority over corner/edge handles when the
    //    user clicks the small circle floating above the bbox.
    if let Some(elem_rect) = selected_element_screen_rect(state, full_rect, viewport_size) {
        let handle_pos = rotation_handle_screen_pos(elem_rect);
        if (start - handle_pos).length() < ROTATION_HANDLE_RADIUS * 2.5 {
            let center = elem_rect.center();
            let initial_rot_deg = current_selection_rotation(state).unwrap_or(0.0);
            let dx = start.x - center.x;
            let dy = start.y - center.y;
            return CanvasDragMode::RotateSelection {
                initial_rot_deg,
                center_screen: [center.x, center.y],
                start_angle_rad: dy.atan2(dx),
            };
        }
    }

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

    // 3b. When the render frame is the selection, dragging anywhere
    //     inside the (rotated) frame body also moves it. We project the
    //     click into the frame's local coordinates so the body-click
    //     hit-test follows the visible rotated outline rather than the
    //     un-rotated bbox.
    if state.selection == Selection::RenderFrame {
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32 / rf_state.zoom.max(1e-6);
        let world_h = rh as f32 / rf_state.zoom.max(1e-6);
        let dx = world.x - rf_state.pos.x;
        let dy = world.y - rf_state.pos.y;
        let rad = rf_state.rotation_deg.to_radians();
        let cs = rad.cos();
        let sn = rad.sin();
        let lx = dx * cs + dy * sn;
        let ly = -dx * sn + dy * cs;
        if lx.abs() <= world_w * 0.5 && ly.abs() <= world_h * 0.5 {
            return CanvasDragMode::MoveRenderFrame {
                initial_pos: [rf_state.pos.x, rf_state.pos.y],
            };
        }
    }

    // 4. Click on a render frame corner → ResizeRenderFrame.
    let rf_corners = render_frame_corners_screen(state, full_rect, viewport_size);
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

    // 6. Empty canvas drag → rubber-band marquee selection. Records the
    //    drag-origin in world coords so the live rectangle stays anchored
    //    to the start point regardless of pan / zoom while the user
    //    drags the opposite corner around.
    CanvasDragMode::Marquee {
        start_world: [world.x, world.y],
        extend: extend_marquee,
    }
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

        CanvasDragMode::Marquee { start_world, .. } => {
            // Live update of the rubber-band rectangle's far corner.
            // Both corners stay in world coords so panning / zooming
            // during the gesture leaves the box anchored to the same
            // canvas region the user lassoed at the start.
            let viewport = &state.canvas_viewport;
            let cur_world = viewport.screen_to_world(cur_local, viewport_size);
            state.canvas_marquee = Some(crate::state::CanvasMarquee {
                start: start_world,
                end: [cur_world.x, cur_world.y],
            });
        }

        CanvasDragMode::MoveActorWorld { actor_idx, initial_pos } => {
            if actor_idx < state.scene.actors.len() {
                let proposed_x = initial_pos[0] + world_dx;
                let proposed_y = initial_pos[1] + world_dy;
                let (snapped_x, snapped_y, guides) = snap_world_center(
                    state,
                    proposed_x,
                    proposed_y,
                    Some(SnapExclude::Actor(actor_idx)),
                );
                state.canvas_drag.snap_guides = guides;
                // Route through the unified setter so a non-zero playhead
                // auto-inserts a keyframe (canvas-first animation).
                set_selection_world_center(state, [snapped_x, snapped_y]);
                // Broadcast the primary's POST-SNAP world delta to every
                // other lassoed element so the whole canvas_selection
                // moves together, anchored relative to each element's
                // own drag-start world centre.
                let total_dx = snapped_x - initial_pos[0];
                let total_dy = snapped_y - initial_pos[1];
                broadcast_multi_translation(state, total_dx, total_dy);
            }
        }

        CanvasDragMode::MoveActorLegacy { actor_idx, initial_pos } => {
            if actor_idx < state.scene.actors.len() {
                let rf = &state.scene.render_frame;
                let rf_state = sample_render_frame(rf, state.playhead);
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32 / rf_state.zoom;
                let world_h = rh as f32 / rf_state.zoom;

                let proposed_norm_x = initial_pos[0] + world_dx / world_w;
                let proposed_norm_y = initial_pos[1] + world_dy / world_h;
                let world_x = rf_state.pos.x - world_w * 0.5 + proposed_norm_x * world_w;
                let world_y = rf_state.pos.y - world_h * 0.5 + proposed_norm_y * world_h;
                let (snapped_world_x, snapped_world_y, guides) = snap_world_center(
                    state,
                    world_x,
                    world_y,
                    Some(SnapExclude::Actor(actor_idx)),
                );
                state.canvas_drag.snap_guides = guides;
                set_selection_world_center(state, [snapped_world_x, snapped_world_y]);
                // Broadcast snapped delta in world coords so non-primary
                // elements track the primary's actual on-screen motion.
                let prim_initial_world_x =
                    rf_state.pos.x - world_w * 0.5 + initial_pos[0] * world_w;
                let prim_initial_world_y =
                    rf_state.pos.y - world_h * 0.5 + initial_pos[1] * world_h;
                let total_dx = snapped_world_x - prim_initial_world_x;
                let total_dy = snapped_world_y - prim_initial_world_y;
                broadcast_multi_translation(state, total_dx, total_dy);
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
                let proposed_norm_x = initial_pos[0] + dx_norm;
                let proposed_norm_y = initial_pos[1] + dy_norm;
                let world_x = rf_state.pos.x - world_w * 0.5 + proposed_norm_x * world_w;
                let world_y = rf_state.pos.y - world_h * 0.5 + proposed_norm_y * world_h;
                let (snapped_world_x, snapped_world_y, guides) = snap_world_center(
                    state,
                    world_x,
                    world_y,
                    Some(SnapExclude::Overlay(overlay_idx)),
                );
                state.canvas_drag.snap_guides = guides;
                set_selection_world_center(state, [snapped_world_x, snapped_world_y]);
                // Broadcast snapped world delta to other selected items.
                let prim_initial_world_x =
                    rf_state.pos.x - world_w * 0.5 + initial_pos[0] * world_w;
                let prim_initial_world_y =
                    rf_state.pos.y - world_h * 0.5 + initial_pos[1] * world_h;
                let total_dx = snapped_world_x - prim_initial_world_x;
                let total_dy = snapped_world_y - prim_initial_world_y;
                broadcast_multi_translation(state, total_dx, total_dy);
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

            // Broadcast the same scale ratio + translation delta to
            // every other lassoed element. We use ratios (not absolute
            // values) so each element scales relative to its own
            // drag-start size; translation is the primary's centre
            // delta, applied uniformly to keep the group cohesive.
            let scale_factor = new_scale / initial_scale.max(1e-3);
            let scale_y_factor = new_scale_y / initial_scale_y.max(1e-3);
            broadcast_multi_scale(state, scale_factor, scale_y_factor);
            let total_dx = cx - initial_pos_world[0];
            let total_dy = cy - initial_pos_world[1];
            broadcast_multi_translation(state, total_dx, total_dy);
        }

        CanvasDragMode::MoveRenderFrame { initial_pos } => {
            // Insert a keyframe at the drag-start playhead and write the
            // new position there — same canvas-first semantics as actors
            // and overlays. Re-using the cached drag-start playhead
            // means the entire drag produces a single kf rather than
            // one per frame while playback is running.
            let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
            let new_x = initial_pos[0] + world_dx;
            let new_y = initial_pos[1] + world_dy;
            ensure_render_frame_kf_at_playhead(&mut state.scene.render_frame.layout, t);
            apply_to_render_frame_kf(&mut state.scene.render_frame.layout, t, |v| {
                v.pos.x = new_x;
                v.pos.y = new_y;
            });
            // Keep every child element pinned to its drag-start world
            // position by re-deriving their normalised `pos` against
            // the new frame state.
            compensate_children_after_render_frame_change(state, t);
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
                let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
                ensure_render_frame_kf_at_playhead(&mut state.scene.render_frame.layout, t);
                apply_to_render_frame_kf(&mut state.scene.render_frame.layout, t, |v| {
                    v.zoom = new_zoom;
                });
                compensate_children_after_render_frame_change(state, t);
            }
        }

        CanvasDragMode::RotateSelection { initial_rot_deg, center_screen, start_angle_rad } => {
            // Map current pointer to angle around the element's centre,
            // subtract the start angle, and add to the initial rotation.
            // Hold Shift to snap to 15° increments.
            let dx = cur.x - center_screen[0];
            let dy = cur.y - center_screen[1];
            if dx.abs() < 0.001 && dy.abs() < 0.001 { return; }
            let cur_angle = dy.atan2(dx);
            let mut delta_deg = (cur_angle - start_angle_rad).to_degrees();
            // Wrap delta to (-180, 180] so spinning past the wrap stays smooth.
            while delta_deg > 180.0 { delta_deg -= 360.0; }
            while delta_deg < -180.0 { delta_deg += 360.0; }
            let mut new_rot = initial_rot_deg + delta_deg;
            if shift_held {
                new_rot = (new_rot / 15.0).round() * 15.0;
            }
            // Clamp to a reasonable range to avoid runaway values.
            new_rot = new_rot.clamp(-3600.0, 3600.0);
            set_selection_rotation(state, new_rot);
            // Broadcast the same rotation delta to every other lassoed
            // element. Each non-primary element rotates around its own
            // centre, so the group spins in unison without drifting.
            let total_delta = new_rot - initial_rot_deg;
            broadcast_multi_rotation(state, total_delta);
        }

        // Mask drawing modes are handled in `handle_mask_draw_input` —
        // they don't go through the same world-pixel transform pipeline
        // as the move / resize / rotate modes above.
        CanvasDragMode::DrawMask { .. } => {}
    }
}

fn move_selection_mode(state: &EditorState) -> crate::state::CanvasDragMode {
    use crate::state::CanvasDragMode;
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor_id = state.scene.actors[idx].id.clone();
            // Prefer the canvas-layout (world-pixel) track when the actor
            // has one. Sample at the *playhead* — using `.first()` here
            // caused the actor to snap to the first keyframe's position
            // on the very first frame of the drag whenever the playhead
            // sat between keyframes.
            if let Some(cl) = state.scene.canvas_layouts.iter().find(|cl| cl.element_id == actor_id) {
                if !cl.keyframes.is_empty() {
                    let sampled = keyframe::sample(&cl.keyframes, t).unwrap_or_default();
                    return CanvasDragMode::MoveActorWorld {
                        actor_idx: idx,
                        initial_pos: [sampled.pos.x, sampled.pos.y],
                    };
                }
            }
            // Legacy normalised layout — sample at the playhead, same fix.
            let initial_pos = keyframe::sample(&state.scene.actors[idx].layout, t)
                .map(|s| s.pos)
                .unwrap_or_else(|| state.scene.actors[idx].layout.first()
                    .map(|kf| kf.value.pos).unwrap_or([0.5, 0.5]));
            CanvasDragMode::MoveActorLegacy { actor_idx: idx, initial_pos }
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            // Overlays are sampled in CLIP-LOCAL time (`t - t_in`); the
            // drag origin must use the same time base so we read the
            // visible position the user is grabbing.
            let local_t = overlay_clip_local_time(state, idx);
            let layout: &Vec<Keyframe<OverlayState>> = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            let initial_pos = keyframe::sample(layout, local_t)
                .map(|s| s.pos)
                .unwrap_or_else(|| layout.first().map(|k| k.value.pos).unwrap_or([0.5, 0.5]));
            CanvasDragMode::MoveOverlay { overlay_idx: idx, initial_pos }
        }
        Selection::RenderFrame => {
            let rf_state = sample_render_frame(&state.scene.render_frame, t);
            CanvasDragMode::MoveRenderFrame {
                initial_pos: [rf_state.pos.x, rf_state.pos.y],
            }
        }
        _ => CanvasDragMode::None,
    }
}

fn current_selection_scale(state: &EditorState) -> Option<f32> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            keyframe::sample(&state.scene.actors[idx].layout, t)
                .map(|s| s.scale)
                .or_else(|| state.scene.actors[idx].layout.first().map(|k| k.value.scale))
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time(state, idx);
            let layout: &Vec<Keyframe<OverlayState>> = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            keyframe::sample(layout, local_t).map(|s| s.scale)
                .or_else(|| layout.first().map(|k| k.value.scale))
        }
        Selection::RenderFrame => {
            // Render frame uses inverse zoom as its "scale" — bigger value
            // = larger frame on the canvas.
            let rf_state = sample_render_frame(&state.scene.render_frame, t);
            Some((1.0 / rf_state.zoom.max(1e-4)).clamp(0.05, 20.0))
        }
        _ => None,
    }
}

/// Sample the current rotation (in degrees) of the selected element at
/// the playhead. Returns the eased value if the track has multiple
/// keyframes — so dragging the rotation gizmo at a non-zero playhead
/// starts from where the animation is right now.
fn current_selection_rotation(state: &EditorState) -> Option<f32> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            keyframe::sample(&state.scene.actors[idx].layout, t)
                .map(|s| s.rotation_deg)
                .or_else(|| state.scene.actors[idx].layout.first().map(|k| k.value.rotation_deg))
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time(state, idx);
            let layout: &Vec<Keyframe<OverlayState>> = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            keyframe::sample(layout, local_t)
                .map(|s| s.rotation_deg)
                .or_else(|| layout.first().map(|k| k.value.rotation_deg))
        }
        Selection::RenderFrame => {
            let rf_state = sample_render_frame(&state.scene.render_frame, t);
            Some(rf_state.rotation_deg)
        }
        _ => None,
    }
}

/// Where to draw the rotation gizmo in screen space, given the element's
/// bounding rect. It floats above the top-mid handle, attached by a stem
/// (drawn in `draw_selection_handles`).
fn rotation_handle_screen_pos(elem_rect: Rect) -> Pos2 {
    Pos2::new(elem_rect.center().x, elem_rect.min.y - ROTATION_HANDLE_OFFSET)
}

fn current_selection_scale_y(state: &EditorState) -> Option<f32> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            keyframe::sample(&state.scene.actors[idx].layout, t)
                .map(|s| s.scale_y)
                .or_else(|| state.scene.actors[idx].layout.first().map(|k| k.value.scale_y))
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time(state, idx);
            let layout: &Vec<Keyframe<OverlayState>> = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            keyframe::sample(layout, local_t).map(|s| s.scale_y)
                .or_else(|| layout.first().map(|k| k.value.scale_y))
        }
        // Render frame is locked to its output aspect ratio.
        Selection::RenderFrame => Some(1.0),
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
            // For image overlays, ask the texture cache for the real
            // PNG dimensions when they're loaded so resize handles snap
            // to the visible picture corners (not the legacy 200×200
            // placeholder).
            let neutral = OverlayState { pos: [0.0, 0.0], scale: 1.0, scale_y: 1.0, rotation_deg: 0.0, opacity: 1.0, flip_x_anim: 1.0, flip_y_anim: 1.0 };
            Some(overlay_bbox_with_state(ov, &neutral, state))
        }
        Selection::RenderFrame => {
            let [rw, rh] = state.scene.render_frame.resolution;
            Some((rw as f32, rh as f32))
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
            let local_t = overlay_clip_local_time(state, idx);
            let ov = &state.scene.overlays[idx];
            let layout: &Vec<Keyframe<OverlayState>> = match ov {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            let ov_state = keyframe::sample(layout, local_t).unwrap_or_default();
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
        Selection::RenderFrame => {
            let rf_state = sample_render_frame(&state.scene.render_frame, t);
            Some([rf_state.pos.x, rf_state.pos.y])
        }
        _ => None,
    }
}

/// Write the world-space center back to the selected element. When the
/// playhead is at a non-zero time and no keyframe exists at that time
/// yet, a new keyframe is inserted (seeded with the eased current value)
/// so dragging on the canvas at any time directly authors animation.
/// World-space center of the selected element is updated. Routes through
/// `kf_anim::write_*_param` so canvas drags respect the per-parameter
/// `animated_params` set:
///   - if the parameter is animated → the value is written to a kf at
///     the **drag-start playhead** (re-using the same kf across the
///     gesture, so a single drag produces ONE kf, not N);
///   - if the parameter is static → the new value is broadcast to every
///     existing kf and no auto-animation kicks in.
///
/// `auto_animate_on_canvas_drag = false` everywhere — canvas drags must
/// not silently mark a parameter as animated; the user explicitly
/// toggles that via the diamond next to the inspector control.
/// Stable category strings for canvas-drag undo grouping. Each setter
/// uses `mutate_drag` with a per-element token so ONE undo snapshot is
/// taken at the start of the gesture and the whole drag collapses into
/// a single Ctrl+Z step. Outside an active gesture (e.g. a single
/// slider tick from the inspector), `state.last_drag_group` is reset by
/// `app.rs::end_drag_group` so the next edit begins a fresh entry.
const CANVAS_TOKEN_POS: &str = "canvas_pos";
const CANVAS_TOKEN_SCALE: &str = "canvas_scale";
const CANVAS_TOKEN_SCALE_Y: &str = "canvas_scale_y";
const CANVAS_TOKEN_ROTATION: &str = "canvas_rotation";

/// Compute a per-element token salt for canvas drag undo. Passing 0 for
/// `RenderFrame` (the only "non-indexed" target) is safe because each
/// category string is namespaced.
fn canvas_drag_token(category: &'static str, sel: Selection) -> u64 {
    let salt = match sel {
        Selection::Actor(i) => 0x1000 + i,
        Selection::Overlay(i) => 0x2000 + i,
        Selection::Background(i) => 0x3000 + i,
        Selection::Audio(i) => 0x4000 + i,
        Selection::Camera(i) => 0x6000 + i,
        Selection::RenderFrame => 0x5000,
        Selection::None => 0xFFFF,
    };
    EditorState::drag_token(category, salt)
}

fn set_selection_world_center(state: &mut EditorState, center: [f32; 2]) {
    let token = canvas_drag_token(CANVAS_TOKEN_POS, state.selection);
    let sel = state.selection;
    write_selection_world_center(state, sel, center, token);
}

// ─── MULTI-DRAG BROADCAST HELPERS ────────────────────────────────────
//
// When `state.canvas_selection` holds more than one element, every
// canvas-side transform (move / scale / rotate) should be relative to
// each element's drag-start state, NOT the primary's. We snapshot
// every selected element's world-centre + scale + rotation when the
// gesture begins, then per-frame compute "where this element should
// be RIGHT NOW" from the primary's accumulated delta. Each frame's
// writes route through the same undo token as the primary's setter
// so the entire compound move is one Ctrl+Z.

/// Snapshot every entry in `state.canvas_selection` so the active
/// transform mode can later broadcast to all of them. Returns an empty
/// vec when fewer than 2 elements are selected.
fn snapshot_multi_drag(state: &EditorState) -> Vec<crate::state::MultiDragEntry> {
    if state.canvas_selection.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(state.canvas_selection.len());
    for sel in &state.canvas_selection {
        let pos = match *sel {
            Selection::Actor(i) => {
                actor_world_aabb(state, i).map(|(mn, mx)| {
                    [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5]
                })
            }
            Selection::Overlay(i) => {
                overlay_world_aabb(state, i).map(|(mn, mx)| {
                    [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5]
                })
            }
            _ => None,
        };
        let Some(initial_pos) = pos else { continue; };
        let (initial_scale, initial_scale_y, initial_rotation) =
            sample_selection_transform(state, *sel);
        out.push(crate::state::MultiDragEntry {
            selection: *sel,
            initial_pos,
            initial_scale,
            initial_scale_y,
            initial_rotation,
        });
    }
    out
}

/// Read scale, scale_y, rotation_deg for an arbitrary `Selection` at the
/// current playhead. Falls back to neutral defaults when the element
/// has no layout / is the render frame / etc.
fn sample_selection_transform(state: &EditorState, sel: Selection) -> (f32, f32, f32) {
    let t = state.playhead;
    match sel {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let st = keyframe::sample(&state.scene.actors[idx].layout, t).unwrap_or_default();
            (st.scale, st.scale_y, st.rotation_deg)
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time(state, idx);
            let layout: &Vec<Keyframe<OverlayState>> = match &state.scene.overlays[idx] {
                Overlay::Text(t) => &t.layout,
                Overlay::Image(im) => &im.layout,
                Overlay::Video(v) => &v.layout,
            };
            let st = keyframe::sample(layout, local_t).unwrap_or_default();
            (st.scale, st.scale_y, st.rotation_deg)
        }
        Selection::RenderFrame => {
            let rf_state = sample_render_frame(&state.scene.render_frame, t);
            (
                (1.0 / rf_state.zoom.max(1e-4)).clamp(0.05, 20.0),
                1.0,
                rf_state.rotation_deg,
            )
        }
        _ => (1.0, 1.0, 0.0),
    }
}

/// Apply the same total world translation to every snapshot entry that
/// is NOT the primary selection. Re-uses the primary's undo token so
/// the whole compound move is a single undo entry.
fn broadcast_multi_translation(state: &mut EditorState, total_dx: f32, total_dy: f32) {
    if state.canvas_drag.multi_drag_snapshot.is_empty() {
        return;
    }
    let token = canvas_drag_token(CANVAS_TOKEN_POS, state.selection);
    let primary = state.selection;
    let snapshot = state.canvas_drag.multi_drag_snapshot.clone();
    for entry in snapshot {
        if entry.selection == primary { continue; }
        let new_center = [
            entry.initial_pos[0] + total_dx,
            entry.initial_pos[1] + total_dy,
        ];
        write_selection_world_center(state, entry.selection, new_center, token);
    }
}

/// Apply the same scale ratio to every snapshot entry that is NOT the
/// primary selection. `scale_factor` is the multiplier between the
/// primary's drag-start scale and its current value.
fn broadcast_multi_scale(state: &mut EditorState, scale_factor: f32, scale_y_factor: f32) {
    if state.canvas_drag.multi_drag_snapshot.is_empty() {
        return;
    }
    let primary = state.selection;
    let token_x = canvas_drag_token(CANVAS_TOKEN_SCALE, state.selection);
    let token_y = canvas_drag_token(CANVAS_TOKEN_SCALE_Y, state.selection);
    let snapshot = state.canvas_drag.multi_drag_snapshot.clone();
    for entry in snapshot {
        if entry.selection == primary { continue; }
        let new_scale = (entry.initial_scale * scale_factor).clamp(0.05, 20.0);
        let new_scale_y = (entry.initial_scale_y * scale_y_factor).clamp(0.05, 20.0);
        write_selection_scale(state, entry.selection, new_scale, token_x);
        write_selection_scale_y(state, entry.selection, new_scale_y, token_y);
    }
}

/// Apply the same absolute rotation delta to every snapshot entry that
/// is NOT the primary selection. `delta_deg` is `current - initial`
/// for the primary; each non-primary element rotates around its own
/// centre by the same amount.
fn broadcast_multi_rotation(state: &mut EditorState, delta_deg: f32) {
    if state.canvas_drag.multi_drag_snapshot.is_empty() {
        return;
    }
    let primary = state.selection;
    let token = canvas_drag_token(CANVAS_TOKEN_ROTATION, state.selection);
    let snapshot = state.canvas_drag.multi_drag_snapshot.clone();
    for entry in snapshot {
        if entry.selection == primary { continue; }
        let new_rot = (entry.initial_rotation + delta_deg).clamp(-3600.0, 3600.0);
        write_selection_rotation(state, entry.selection, new_rot, token);
    }
}

/// Apply a new world-pixel centre to an arbitrary selection. Same
/// semantics as [`set_selection_world_center`] but takes the target
/// element AND the undo token explicitly so multi-element drags can
/// route every broadcast through the SAME token (one undo entry per
/// gesture, not N).
fn write_selection_world_center(
    state: &mut EditorState,
    sel: Selection,
    center: [f32; 2],
    token: u64,
) {
    // Re-anchor every keyframe write to the playhead captured at drag
    // start (frozen for the duration of the gesture). Outside an active
    // drag the live playhead is used so single inspector edits still
    // land at the visible time.
    let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
    // ── Undo grouping: one snapshot per drag gesture. ──
    if state.last_drag_group != Some(token) {
        state.undo.push(&state.scene);
        state.last_drag_group = Some(token);
    }
    match sel {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor_id = state.scene.actors[idx].id.clone();
            // Prefer canvas_layouts entry when present (free canvas v2).
            // The world-pixel canvas track honours the host actor's
            // `animated_params` for POS_X / POS_Y so toggling the
            // diamond OFF makes canvas drags broadcast (static) instead
            // of animating mid-track. Without this every drag while a
            // canvas_layouts entry existed silently authored animation,
            // even when the inspector clearly showed the param as
            // static.
            let animated_clone = state.scene.actors[idx].animated_params.clone();
            if let Some(cl) = state.scene.canvas_layouts.iter_mut()
                .find(|cl| cl.element_id == actor_id)
            {
                crate::kf_anim::write_canvas_param(
                    &mut cl.keyframes,
                    &animated_clone,
                    &[
                        memstroy_core::param_ids::POS_X,
                        memstroy_core::param_ids::POS_Y,
                    ],
                    t,
                    |v| {
                        v.pos.x = center[0];
                        v.pos.y = center[1];
                    },
                );
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
            let new_norm = [(center[0] - frame_tl_x) / world_w, (center[1] - frame_tl_y) / world_h];
            let actor = &mut state.scene.actors[idx];
            crate::kf_anim::write_actor_param(
                &mut actor.layout, &mut actor.animated_params, t,
                memstroy_core::param_ids::POS_X, false,
                |v| v.pos[0] = new_norm[0],
            );
            crate::kf_anim::write_actor_param(
                &mut actor.layout, &mut actor.animated_params, t,
                memstroy_core::param_ids::POS_Y, false,
                |v| v.pos[1] = new_norm[1],
            );
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time_at(state, idx, t);
            let rf = &state.scene.render_frame;
            let rf_state = sample_render_frame(rf, t);
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32 / rf_state.zoom;
            let world_h = rh as f32 / rf_state.zoom;
            let frame_tl_x = rf_state.pos.x - world_w * 0.5;
            let frame_tl_y = rf_state.pos.y - world_h * 0.5;
            if world_w <= 0.0 || world_h <= 0.0 { return; }
            let new_norm = [(center[0] - frame_tl_x) / world_w, (center[1] - frame_tl_y) / world_h];
            let (layout, animated_params) =
                overlay_layout_and_animated_mut(&mut state.scene.overlays[idx]);
            crate::kf_anim::write_overlay_param(
                layout, animated_params, local_t,
                memstroy_core::param_ids::POS_X, false,
                |v| v.pos[0] = new_norm[0],
            );
            let (layout, animated_params) =
                overlay_layout_and_animated_mut(&mut state.scene.overlays[idx]);
            crate::kf_anim::write_overlay_param(
                layout, animated_params, local_t,
                memstroy_core::param_ids::POS_Y, false,
                |v| v.pos[1] = new_norm[1],
            );
        }
        Selection::RenderFrame => {
            ensure_render_frame_kf_at_playhead(&mut state.scene.render_frame.layout, t);
            apply_to_render_frame_kf(&mut state.scene.render_frame.layout, t, |v| {
                v.pos.x = center[0];
                v.pos.y = center[1];
            });
        }
        _ => {}
    }
}

// `mark_actor_canvas_animated` / `mark_overlay_canvas_animated` were
// removed (and replaced by the gating inside `kf_anim::write_*_param`).
// Canvas drags no longer auto-mark a parameter as animated — the user
// explicitly toggles that with the diamond next to the inspector field.

fn set_selection_scale_y(state: &mut EditorState, new_scale_y: f32) {
    let token = canvas_drag_token(CANVAS_TOKEN_SCALE_Y, state.selection);
    let sel = state.selection;
    write_selection_scale_y(state, sel, new_scale_y, token);
}

fn write_selection_scale_y(
    state: &mut EditorState,
    sel: Selection,
    new_scale_y: f32,
    token: u64,
) {
    let s = new_scale_y.clamp(0.05, 20.0);
    let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
    if state.last_drag_group != Some(token) {
        state.undo.push(&state.scene);
        state.last_drag_group = Some(token);
    }
    match sel {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &mut state.scene.actors[idx];
            crate::kf_anim::write_actor_param(
                &mut actor.layout, &mut actor.animated_params, t,
                memstroy_core::param_ids::SCALE_Y, false,
                |v| v.scale_y = s,
            );
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time_at(state, idx, t);
            let (layout, animated_params) =
                overlay_layout_and_animated_mut(&mut state.scene.overlays[idx]);
            crate::kf_anim::write_overlay_param(
                layout, animated_params, local_t,
                memstroy_core::param_ids::SCALE_Y, false,
                |v| v.scale_y = s,
            );
        }
        // Render frame is locked to its output aspect ratio — scale_y is
        // ignored here, scale alone changes its size on the canvas.
        _ => {}
    }
}

fn set_selection_scale(state: &mut EditorState, new_scale: f32) {
    let token = canvas_drag_token(CANVAS_TOKEN_SCALE, state.selection);
    let sel = state.selection;
    write_selection_scale(state, sel, new_scale, token);
}

fn write_selection_scale(
    state: &mut EditorState,
    sel: Selection,
    new_scale: f32,
    token: u64,
) {
    let s = new_scale.clamp(0.05, 20.0);
    let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
    if state.last_drag_group != Some(token) {
        state.undo.push(&state.scene);
        state.last_drag_group = Some(token);
    }
    match sel {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &mut state.scene.actors[idx];
            crate::kf_anim::write_actor_param(
                &mut actor.layout, &mut actor.animated_params, t,
                memstroy_core::param_ids::SCALE, false,
                |v| v.scale = s,
            );
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time_at(state, idx, t);
            let (layout, animated_params) =
                overlay_layout_and_animated_mut(&mut state.scene.overlays[idx]);
            crate::kf_anim::write_overlay_param(
                layout, animated_params, local_t,
                memstroy_core::param_ids::SCALE, false,
                |v| v.scale = s,
            );
        }
        Selection::RenderFrame => {
            // Map scale → inverse zoom (bigger scale = bigger frame).
            ensure_render_frame_kf_at_playhead(&mut state.scene.render_frame.layout, t);
            apply_to_render_frame_kf(&mut state.scene.render_frame.layout, t, |v| {
                v.zoom = (1.0 / s.max(1e-4)).clamp(0.001, 1000.0);
            });
        }
        _ => {}
    }
}

fn apply_scale_delta(state: &mut EditorState, delta: f32) {
    if let Some(s) = current_selection_scale(state) {
        set_selection_scale(state, s + delta);
    }
}

/// Write the rotation back to the selected element. Like
/// `set_selection_world_center`, this auto-inserts a keyframe at the
/// current playhead time when one is missing — giving the user a
/// canvas-first animation workflow: drag the rotation gizmo at any
/// time and the system records a keyframe automatically.
fn set_selection_rotation(state: &mut EditorState, new_rot_deg: f32) {
    let token = canvas_drag_token(CANVAS_TOKEN_ROTATION, state.selection);
    let sel = state.selection;
    write_selection_rotation(state, sel, new_rot_deg, token);
}

fn write_selection_rotation(
    state: &mut EditorState,
    sel: Selection,
    new_rot_deg: f32,
    token: u64,
) {
    let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
    if state.last_drag_group != Some(token) {
        state.undo.push(&state.scene);
        state.last_drag_group = Some(token);
    }
    match sel {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &mut state.scene.actors[idx];
            crate::kf_anim::write_actor_param(
                &mut actor.layout, &mut actor.animated_params, t,
                memstroy_core::param_ids::ROTATION, false,
                |v| v.rotation_deg = new_rot_deg,
            );
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let local_t = overlay_clip_local_time_at(state, idx, t);
            let (layout, animated_params) =
                overlay_layout_and_animated_mut(&mut state.scene.overlays[idx]);
            crate::kf_anim::write_overlay_param(
                layout, animated_params, local_t,
                memstroy_core::param_ids::ROTATION, false,
                |v| v.rotation_deg = new_rot_deg,
            );
        }
        Selection::RenderFrame => {
            ensure_render_frame_kf_at_playhead(&mut state.scene.render_frame.layout, t);
            apply_to_render_frame_kf(&mut state.scene.render_frame.layout, t, |v| {
                v.rotation_deg = new_rot_deg;
            });
        }
        _ => {}
    }
}

/// Insert a keyframe within ε of `t` on an actor track, seeding it with
/// the eased current value so animation continues smoothly from the
/// visual state.
#[allow(dead_code)]
fn ensure_actor_kf_at_playhead(layout: &mut Vec<Keyframe<ActorState>>, t: f32) {
    if layout.is_empty() {
        layout.push(Keyframe::new(t, ActorState::default()));
        return;
    }
    let eps = 1.0e-3;
    if layout.iter().any(|kf| (kf.t - t).abs() < eps) {
        return;
    }
    let sampled = keyframe::sample(layout, t).unwrap_or_default();
    layout.push(Keyframe::new(t, sampled));
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

#[allow(dead_code)]
fn ensure_overlay_kf_at_playhead(layout: &mut Vec<Keyframe<OverlayState>>, t: f32) {
    if layout.is_empty() {
        layout.push(Keyframe::new(t, OverlayState::default()));
        return;
    }
    let eps = 1.0e-3;
    if layout.iter().any(|kf| (kf.t - t).abs() < eps) {
        return;
    }
    let sampled = keyframe::sample(layout, t).unwrap_or_default();
    layout.push(Keyframe::new(t, sampled));
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

#[allow(dead_code)]
fn ensure_canvas_kf_at_playhead(layout: &mut Vec<Keyframe<CanvasTransform>>, t: f32) {
    if layout.is_empty() {
        layout.push(Keyframe::new(t, CanvasTransform::default()));
        return;
    }
    let eps = 1.0e-3;
    if layout.iter().any(|kf| (kf.t - t).abs() < eps) {
        return;
    }
    let sampled = keyframe::sample(layout, t).unwrap_or_default();
    layout.push(Keyframe::new(t, sampled));
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

/// Insert a keyframe at `t` on the render-frame layout, seeded with the
/// eased current value. Used so that moving / resizing / rotating the
/// render frame on the canvas at any playhead authors animation
/// automatically (canvas-first workflow, same as actors / overlays).
fn ensure_render_frame_kf_at_playhead(layout: &mut Vec<Keyframe<RenderFrameState>>, t: f32) {
    if layout.is_empty() {
        layout.push(Keyframe::new(t, RenderFrameState::default()));
        return;
    }
    let eps = 1.0e-3;
    if layout.iter().any(|kf| (kf.t - t).abs() < eps) {
        return;
    }
    let sampled = keyframe::sample(layout, t).unwrap_or_default();
    layout.push(Keyframe::new(t, sampled));
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

#[allow(dead_code)]
fn apply_to_anim_kf<F: FnOnce(&mut ActorState)>(
    layout: &mut Vec<Keyframe<ActorState>>,
    t: f32,
    f: F,
) {
    let eps = 1.0e-3;
    if let Some(kf) = layout.iter_mut().find(|kf| (kf.t - t).abs() < eps) {
        f(&mut kf.value);
        return;
    }
    if let Some(kf) = layout.first_mut() {
        f(&mut kf.value);
    }
}

#[allow(dead_code)]
fn apply_to_overlay_kf<F: FnOnce(&mut OverlayState)>(
    layout: &mut Vec<Keyframe<OverlayState>>,
    t: f32,
    f: F,
) {
    let eps = 1.0e-3;
    if let Some(kf) = layout.iter_mut().find(|kf| (kf.t - t).abs() < eps) {
        f(&mut kf.value);
        return;
    }
    if let Some(kf) = layout.first_mut() {
        f(&mut kf.value);
    }
}

/// Apply a closure to the render-frame keyframe at `t`. Falls back to the
/// first keyframe when no exact match exists (mirrors the behaviour of
/// `apply_to_anim_kf`).
fn apply_to_render_frame_kf<F: FnOnce(&mut RenderFrameState)>(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    t: f32,
    f: F,
) {
    let eps = 1.0e-3;
    if let Some(kf) = layout.iter_mut().find(|kf| (kf.t - t).abs() < eps) {
        f(&mut kf.value);
        return;
    }
    if let Some(kf) = layout.first_mut() {
        f(&mut kf.value);
    }
}

// ─── RENDER-FRAME CHILD COMPENSATION ─────────────────────────────────
//
// When the user drags the render frame (move or resize), we want the
// elements that LIVE INSIDE the frame to stay visually pinned to their
// original world position — the user is reframing the output region, not
// re-arranging the scene contents. Overlays and legacy actors store
// their `pos` as a normalised [0..1] vector relative to the render
// frame, so a frame move would otherwise drag every child along.
//
// Approach:
//   1. At drag start, snapshot every child's CURRENT world position
//      (before the frame moves) into the canvas drag state.
//   2. After every drag tick that mutates the render frame, re-derive
//      each child's normalised `pos` from the snapshotted world point
//      against the NEW frame state — so the world point is preserved.

/// Snapshot the world position of every overlay relative to the render
/// frame at the drag-start playhead. Skipped for overlays attached to a
/// skeleton point (those are positioned by a different mechanism and
/// would fight the compensation).
fn snapshot_overlay_world_positions(state: &EditorState) -> Vec<(usize, [f32; 2])> {
    let mut out = Vec::new();
    let t = state.playhead;
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom.max(1e-6);
    let world_h = rh as f32 / rf_state.zoom.max(1e-6);
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;

    for (idx, overlay) in state.scene.overlays.iter().enumerate() {
        // Skeleton-attached overlays follow their host bone; leave them
        // alone or the compensation would fight the attachment.
        let attached = match overlay {
            Overlay::Text(t) => t.skeleton_attachment.is_some(),
            Overlay::Image(im) => im.skeleton_attachment.is_some(),
            Overlay::Video(v) => v.skeleton_attachment.is_some(),
        };
        if attached { continue; }
        let (t_in, t_out, layout) = match overlay {
            Overlay::Text(t) => (t.t_in, t.t_out, &t.layout),
            Overlay::Image(im) => (im.t_in, im.t_out, &im.layout),
            Overlay::Video(v) => (v.t_in, v.t_out, &v.layout),
        };
        let sample_t = if t >= t_in && t <= t_out { t - t_in }
            else if t < t_in { 0.0 } else { (t_out - t_in).max(0.0) };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
        let world_x = frame_tl_x + ov_state.pos[0] * world_w;
        let world_y = frame_tl_y + ov_state.pos[1] * world_h;
        out.push((idx, [world_x, world_y]));
    }
    out
}

/// Snapshot the world position of every actor that is using the LEGACY
/// normalised layout (i.e. has no entry in `canvas_layouts`). Actors
/// already pinned via canvas_layouts are world-pixel anchored and don't
/// need compensation.
fn snapshot_legacy_actor_world_positions(state: &EditorState) -> Vec<(usize, [f32; 2])> {
    let mut out = Vec::new();
    let t = state.playhead;
    for (idx, actor) in state.scene.actors.iter().enumerate() {
        if state.scene.canvas_layouts.iter().any(|cl| cl.element_id == actor.id) {
            continue;
        }
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        out.push((idx, [world_pos.x, world_pos.y]));
    }
    out
}

/// After the render-frame keyframe at `t` was just mutated, re-derive
/// the normalised `pos` of every snapshotted child so they land on the
/// SAME world coordinate as before the mutation. Called at the end of
/// MoveRenderFrame / ResizeRenderFrame `apply_drag` arms.
fn compensate_children_after_render_frame_change(state: &mut EditorState, t: f32) {
    let rf_state_new = sample_render_frame(&state.scene.render_frame, t);
    let [rw, rh] = state.scene.render_frame.resolution;
    let world_w = (rw as f32 / rf_state_new.zoom.max(1e-6)).max(1e-3);
    let world_h = (rh as f32 / rf_state_new.zoom.max(1e-6)).max(1e-3);
    let frame_tl_x = rf_state_new.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state_new.pos.y - world_h * 0.5;

    // Overlays — pos[0]/[1] are normalised [0..1] relative to the frame.
    let overlay_snap = state.canvas_drag.overlay_world_snapshot.clone();
    for (idx, world_xy) in overlay_snap {
        if let Some(overlay) = state.scene.overlays.get_mut(idx) {
            let (t_in, t_out, layout) = match overlay {
                Overlay::Text(o) => (o.t_in, o.t_out, &mut o.layout),
                Overlay::Image(o) => (o.t_in, o.t_out, &mut o.layout),
                Overlay::Video(o) => (o.t_in, o.t_out, &mut o.layout),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in }
                else if t < t_in { 0.0 } else { (t_out - t_in).max(0.0) };
            let new_norm_x = (world_xy[0] - frame_tl_x) / world_w;
            let new_norm_y = (world_xy[1] - frame_tl_y) / world_h;
            // Find or create the keyframe nearest sample_t (overlay
            // keyframes are clip-local). Mutate the closest one so the
            // compensation lands on the kf the user is currently
            // viewing rather than spawning a fresh kf per frame.
            let eps = 1.0e-3;
            if let Some(kf) = layout
                .iter_mut()
                .min_by(|a, b| {
                    (a.t - sample_t).abs()
                        .partial_cmp(&(b.t - sample_t).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                if (kf.t - sample_t).abs() < 0.5 + eps {
                    kf.value.pos[0] = new_norm_x;
                    kf.value.pos[1] = new_norm_y;
                }
            }
        }
    }

    // Legacy actors — same idea, but the layout is on the actor itself.
    let actor_snap = state.canvas_drag.actor_legacy_snapshot.clone();
    for (idx, world_xy) in actor_snap {
        if let Some(actor) = state.scene.actors.get_mut(idx) {
            let new_norm_x = (world_xy[0] - frame_tl_x) / world_w;
            let new_norm_y = (world_xy[1] - frame_tl_y) / world_h;
            // Actor legacy kfs are scene-time anchored; pick the
            // nearest one to the drag-start playhead.
            if let Some(kf) = actor
                .layout
                .iter_mut()
                .min_by(|a, b| {
                    (a.t - t).abs()
                        .partial_cmp(&(b.t - t).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                kf.value.pos[0] = new_norm_x;
                kf.value.pos[1] = new_norm_y;
            }
        }
    }
}

#[allow(dead_code)]
fn overlay_layout_mut(overlay: &mut Overlay) -> &mut Vec<Keyframe<OverlayState>> {
    match overlay {
        Overlay::Text(t) => &mut t.layout,
        Overlay::Image(im) => &mut im.layout,
        Overlay::Video(v) => &mut v.layout,
    }
}

/// Borrow both the layout vec AND the per-overlay `animated_params` set
/// at once. Used by canvas drag setters that route through
/// `kf_anim::write_overlay_param`, which needs both to gate per-param
/// keyframing vs. broadcasting.
fn overlay_layout_and_animated_mut<'a>(
    overlay: &'a mut Overlay,
) -> (
    &'a mut Vec<Keyframe<OverlayState>>,
    &'a mut std::collections::BTreeSet<String>,
) {
    match overlay {
        Overlay::Text(t) => (&mut t.layout, &mut t.animated_params),
        Overlay::Image(im) => (&mut im.layout, &mut im.animated_params),
        Overlay::Video(v) => (&mut v.layout, &mut v.animated_params),
    }
}

/// Time used to read/write keyframes on overlays. Overlays are sampled
/// in clip-local seconds (`t - t_in`) by `draw_canvas_overlays` and the
/// renderer, so any drag setter / inspector edit on the canvas must use
/// the same time base — otherwise a kf inserted at the global playhead
/// would never be visible.
fn overlay_clip_local_time(state: &EditorState, ov_idx: usize) -> f32 {
    overlay_clip_local_time_at(state, ov_idx, state.playhead)
}

/// Same as `overlay_clip_local_time` but lets the caller pin the scene
/// time explicitly. Used by the canvas drag setters so they can re-anchor
/// every kf write to the drag-start playhead instead of the live one.
fn overlay_clip_local_time_at(state: &EditorState, ov_idx: usize, scene_t: f32) -> f32 {
    if ov_idx >= state.scene.overlays.len() { return scene_t; }
    let t_in = match &state.scene.overlays[ov_idx] {
        Overlay::Text(t) => t.t_in,
        Overlay::Image(im) => im.t_in,
        Overlay::Video(v) => v.t_in,
    };
    (scene_t - t_in).max(0.0)
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

    // Overlays first (top of z-order). Sort by track index ascending — the
    // topmost row on the timeline panel hits first. Overlays drawn BEHIND
    // the actors are de-prioritised so a click on a stacked spot selects
    // the on-top element first.
    let mut order: Vec<(usize, i32)> = state.scene.overlays.iter().enumerate()
        .map(|(i, _)| {
            let bias = if overlay_is_behind_actors(state, i) {
                1_000_000
            } else {
                0
            };
            (i, overlay_track_index(state, i) as i32 + bias)
        })
        .collect();
    // Smaller key = higher in panel = drawn on top = checked first.
    order.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

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
        let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
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
            // Use the texture-aware bbox so resize handles snap to the
            // image's real dimensions instead of the legacy 200×200
            // placeholder square that used to bunch them in the centre.
            let (elem_w, elem_h) = overlay_bbox_with_state(overlay, &ov_state, state);
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let half_w = elem_w * 0.5 * state.canvas_viewport.zoom;
            let half_h = elem_h * 0.5 * state.canvas_viewport.zoom;
            Some(Rect::from_center_size(
                Pos2::new(full_rect.min.x + center_screen[0], full_rect.min.y + center_screen[1]),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            ))
        }
        Selection::RenderFrame => {
            // The render frame is selectable like any other element so the
            // user can rotate/resize/reposition it from the canvas. We
            // intentionally return `None` here so the generic AABB
            // handles drawn by `draw_selection_handles` don't appear on
            // top of the rotated handles drawn by `draw_render_frame`.
            // The resize / move drag modes for the render frame are
            // detected separately in `decide_drag_mode` using rotated
            // corner positions, and the rotated body hit-test uses the
            // OBB so a click inside the visible frame still selects it.
            None
        }
        _ => None,
    }
}

#[allow(dead_code)]
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
        // Rotation handle gets priority over the corners/edges so its
        // hit-box is checked first (it's drawn outside the bbox).
        let rot_pos = rotation_handle_screen_pos(rect);
        if (hover - rot_pos).length() < ROTATION_HANDLE_RADIUS * 2.0 {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            return;
        }
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
    let rf_corners = render_frame_corners_screen(state, full_rect, viewport_size);
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
/// Each keyframe is shown as a small numbered dot connected by a polyline,
/// so the user can see the motion path at a glance. Per-keyframe parameter
/// callouts (coordinates / scale / rotation / opacity) are intentionally
/// NOT drawn here — they cluttered the canvas during dragging. All these
/// values remain available in the inspector and timeline curves.
fn draw_selection_keyframe_trajectory(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let rf_resolution = rf.resolution;

    // Collect each keyframe's world position for the active selection.
    #[derive(Clone)]
    struct KfPoint { world: WorldPos }

    let points: Vec<KfPoint> = match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if actor.layout.len() < 2 { return; }
            actor.layout.iter().map(|kf| {
                let world = actor_kf_world_pos(state, &actor.id, &kf.value, &rf_state, rf_resolution);
                KfPoint { world }
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
            layout.iter().map(|kf| {
                let world = WorldPos {
                    x: frame_tl_x + kf.value.pos[0] * world_w,
                    y: frame_tl_y + kf.value.pos[1] * world_h,
                };
                KfPoint { world }
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

    // Compact numbered dots — no textual callouts.
    let dot_radius = 5.0;
    let dot_fill = Color32::from_rgb(255, 180, 60);
    let dot_stroke = Color32::from_rgb(40, 30, 0);
    for (i, pt) in screen_pts.iter().enumerate() {
        painter.circle_filled(*pt, dot_radius, dot_fill);
        painter.circle_stroke(*pt, dot_radius, Stroke::new(1.2, dot_stroke));
        // Number inside the dot — small enough to be unobtrusive.
        painter.text(
            *pt, egui::Align2::CENTER_CENTER,
            (i + 1).to_string(),
            egui::FontId::proportional(9.0),
            Color32::from_rgb(20, 20, 20),
        );
    }
    let _ = full_rect;
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

        // ── Rotation handle ───────────────────────────────────────
        // Floats above the top-mid handle on a short stem so it never
        // overlaps the resize affordances. Cyan colour distinguishes
        // it from yellow scale handles. Drag this handle to rotate the
        // element around its centre; hold Shift to snap to 15° steps.
        let rot_pos = rotation_handle_screen_pos(rect);
        let top_mid = Pos2::new(rect.center().x, rect.min.y);
        painter.line_segment(
            [top_mid, rot_pos],
            Stroke::new(1.5, Color32::from_rgb(80, 160, 200)),
        );
        painter.circle_filled(rot_pos, ROTATION_HANDLE_RADIUS, COL_ROTATION_HANDLE);
        painter.circle_stroke(rot_pos, ROTATION_HANDLE_RADIUS, Stroke::new(1.5, Color32::from_rgb(20, 40, 60)));
        // Tiny "↻" hint glyph.
        painter.text(
            rot_pos,
            egui::Align2::CENTER_CENTER,
            "\u{21BB}",
            egui::FontId::proportional(10.0),
            Color32::from_rgb(20, 40, 60),
        );
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
            let (elem_w, elem_h) = overlay_bbox_with_state(overlay, &ov_state, state);
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
    _ui: &mut egui::Ui,
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
            // Glyphs are sized by `font_size` alone — `scale` only
            // governs the background plate (padding / extras / corner
            // radius). See draw_text_overlay for the matching change.
            let text_w = (max_chars.max(1.0)) * font * 0.55;
            let text_h = (lines.len() as f32) * font * 1.2;
            // Symmetric padding + asymmetric extras on the horizontal axis.
            // Resize handles need the FULL plate size so they snap to the
            // visible edges; padding scales with `ov_state.scale` so the
            // plate itself can be enlarged without touching the glyphs.
            let pad_w = (style.box_padding * 2.0
                + style.box_extra_left
                + style.box_extra_right)
                * sx;
            let pad_h = style.box_padding * 2.0 * sy;
            (
                (text_w + pad_w).max(40.0),
                (text_h + pad_h).max(20.0),
            )
        }
        Overlay::Image(_) => (200.0 * sx, 200.0 * sy),
        Overlay::Video(_) => (300.0 * sx, 300.0 * 16.0 / 9.0 * sy),
    }
}

/// Same as `overlay_bbox` but returns the actual on-disk dimensions of
/// loaded image overlays when available. Used by the resize-handle math
/// so the gizmo snaps to the visible PNG corners (not the legacy 200×200
/// placeholder bbox). Falls back to `overlay_bbox` for the other variants.
fn overlay_bbox_with_state(
    overlay: &Overlay,
    ov_state: &OverlayState,
    state: &EditorState,
) -> (f32, f32) {
    if let Overlay::Image(img) = overlay {
        if let Ok(map) = state.image_textures.lock() {
            if let Some(crate::state::ImageTextureSlot::Loaded { size, .. }) =
                map.get(&img.source)
            {
                let sx = ov_state.scale;
                let sy = ov_state.scale * ov_state.scale_y;
                return (size[0] as f32 * sx, size[1] as f32 * sy);
            }
        }
    }
    overlay_bbox(overlay, ov_state)
}

/// Lazily decode `path` into a cached `egui::TextureHandle` and report
/// the source dimensions. Re-entry is cheap — once a slot is `Loaded`
/// or `Failed`, subsequent calls only do a hash-map lookup. Returns
/// `Some((w, h))` when a real texture is available, `None` while
/// loading or after a permanent failure.
fn ensure_image_loaded(
    state: &EditorState,
    path: &std::path::Path,
    ctx: &egui::Context,
) -> Option<(u32, u32)> {
    use crate::state::ImageTextureSlot;
    // Fast path: already in the map.
    if let Ok(map) = state.image_textures.lock() {
        if let Some(slot) = map.get(path) {
            return match slot {
                ImageTextureSlot::Loaded { size, .. } => Some((size[0], size[1])),
                ImageTextureSlot::Failed => None,
            };
        }
    }

    // Slow path: decode now. We do a synchronous decode because typical
    // sticker PNGs are small and the result is cached after the first
    // hit; bumping this to a background thread is a follow-up if very
    // large images become common.
    let decoded = image::open(path).map(|img| img.to_rgba8());
    let (handle, size) = match decoded {
        Ok(rgba) => {
            let w = rgba.width();
            let h = rgba.height();
            let pixels = rgba.into_raw();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                &pixels,
            );
            let name = format!(
                "img_overlay_{}",
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("anon")
            );
            let handle = ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR);
            (Some(handle), Some([w, h]))
        }
        Err(_) => (None, None),
    };

    if let Ok(mut map) = state.image_textures.lock() {
        match (handle, size) {
            (Some(texture), Some(sz)) => {
                map.insert(
                    path.to_path_buf(),
                    ImageTextureSlot::Loaded { texture, size: sz },
                );
                return Some((sz[0], sz[1]));
            }
            _ => {
                map.insert(path.to_path_buf(), ImageTextureSlot::Failed);
            }
        }
    }
    None
}

/// Build (or fetch from cache) an image texture with the supplied
/// effect stack baked in. Returns the `TextureHandle` plus the
/// (left, top, right, bottom) Crop UV inset accumulated by the stack
/// — the renderer uses the inset to shrink the picture's screen
/// rectangle so previews match the FFmpeg export.
fn ensure_image_fx_loaded(
    state: &EditorState,
    path: &std::path::Path,
    effects: &[memstroy_core::effects::Effect],
    ctx: &egui::Context,
) -> Option<(egui::TextureHandle, [f32; 4])> {
    let sig = crate::image_effects::signature(effects);
    let key = (path.to_path_buf(), sig);

    // Fast path: cached texture from a previous frame.
    if let Ok(map) = state.image_fx_textures.lock() {
        if let Some(slot) = map.get(&key) {
            return Some((slot.texture.clone(), slot.crop));
        }
    }

    // Slow path: decode the source PNG, run the CPU effect pipeline,
    // upload the result as a fresh texture, and cache it.
    let decoded = image::open(path).ok()?.to_rgba8();
    let w = decoded.width();
    let h = decoded.height();
    let mut buf = decoded.into_raw();
    let crop = crate::image_effects::apply_effect_stack(&mut buf, w, h, effects, 0.0);
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &buf,
    );
    let name = format!(
        "img_overlay_fx_{}_{:x}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("anon"),
        sig,
    );
    let texture = ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR);
    let crop_arr = [crop.0, crop.1, crop.2, crop.3];

    if let Ok(mut map) = state.image_fx_textures.lock() {
        // Evict every cached entry for the same source path that has a
        // different signature — keeps the cache from growing unbounded
        // as the user tweaks effect parameters frame-by-frame.
        map.retain(|(p, s), _| !(p == path && *s != sig));
        map.insert(
            key,
            crate::state::ImageFxSlot {
                texture: texture.clone(),
                size: [w, h],
                crop: crop_arr,
            },
        );
    }
    Some((texture, crop_arr))
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

    // Build hit-test order from track positions: topmost row first. Any
    // overlay on a row below the actor rows is biased down so a stacked
    // click first lands on the on-top element.
    let mut order: Vec<(usize, i32)> = state.scene.overlays.iter().enumerate()
        .map(|(i, _)| {
            let bias = if overlay_is_behind_actors(state, i) {
                1_000_000
            } else {
                0
            };
            (i, overlay_track_index(state, i) as i32 + bias)
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
        let sample_t = if t >= t_in && t <= t_out { t - t_in }
            else if t < t_in { 0.0 } else { t_out - t_in };
        let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

        let ov_world = WorldPos {
            x: frame_tl_x + ov_state.pos[0] * world_w,
            y: frame_tl_y + ov_state.pos[1] * world_h,
        };
        let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
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

    // Render frame fall-through: clicking inside the frame outline
    // (when nothing else matched) selects the render frame itself so
    // the inspector exposes its position / size / rotation. The
    // hit-test rotates the click position into the frame's local
    // coordinate frame so the collider follows the visible (rotated)
    // outline rather than the un-rotated bbox.
    let rad = rf_state.rotation_deg.to_radians();
    let cs = rad.cos();
    let sn = rad.sin();
    let dx = pos.x - rf_state.pos.x;
    let dy = pos.y - rf_state.pos.y;
    let lx = dx * cs + dy * sn;
    let ly = -dx * sn + dy * cs;
    let half_w = world_w * 0.5;
    let half_h = world_h * 0.5;
    if lx >= -half_w && lx <= half_w && ly >= -half_h && ly <= half_h {
        state.selection = Selection::RenderFrame;
        return;
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
            let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
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
        let alpha = if dist < threshold {
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
    // ── Mask / crop tool palette (top-left) ──
    draw_mask_toolbar(ui, full_rect, state);

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




// ─── MASK / CROP TOOLBAR ─────────────────────────────────────────────
//
// A small floating palette anchored to the top-left of the canvas.
// Toggling a tool sets `EditorState::mask_tool`; subsequent drag input
// is dispatched to `handle_mask_draw_input` instead of the regular
// transform pipeline. Pressing the same button again or pressing
// Escape disarms the tool.

/// Draw the mask / crop tool palette and forward clicks back to
/// `EditorState::mask_tool`. The buttons sit on top of every other
/// canvas element so they remain accessible even while a marquee /
/// drag is in flight. A short status badge in the top-centre tells
/// the user which tool is armed and how to commit / cancel.
fn draw_mask_toolbar(
    ui: &mut egui::Ui,
    full_rect: Rect,
    state: &mut EditorState,
) {
    use crate::state::MaskTool;
    let margin = 8.0;
    let btn_w = 28.0;
    let btn_h = 24.0;
    let gap = 4.0;
    let tools: [(MaskTool, &str, &str); 5] = [
        (MaskTool::None, "\u{2196}", "Select / transform (Esc)"),
        (MaskTool::Crop, "\u{2702}", "Crop tool — drag a rectangle to crop"),
        (MaskTool::RectMask, "\u{25A1}", "Rectangle mask — drag to mask outside the rect"),
        (MaskTool::EllipseMask, "\u{25CB}", "Ellipse mask — drag to mask outside the ellipse"),
        (MaskTool::FreehandMask, "\u{270F}", "Freehand mask — paint a closed polygon"),
    ];
    for (i, (tool, glyph, hint)) in tools.iter().enumerate() {
        let rect = Rect::from_min_size(
            Pos2::new(full_rect.min.x + margin + (btn_w + gap) * i as f32,
                     full_rect.min.y + margin),
            Vec2::new(btn_w, btn_h),
        );
        let active = state.mask_tool == *tool;
        let mut btn = egui::Button::new(
            RichText::new(*glyph)
                .size(14.0)
                .color(if active { Color32::BLACK } else { Color32::from_rgb(220, 220, 230) }),
        );
        if active {
            btn = btn.fill(Color32::from_rgb(255, 200, 50));
        } else {
            btn = btn.fill(Color32::from_rgba_premultiplied(30, 30, 40, 220));
        }
        let resp = ui.put(rect, btn).on_hover_text(*hint);
        if resp.clicked() {
            state.mask_tool = if active { MaskTool::None } else { *tool };
            state.mask_draft_points.clear();
        }
    }

    // ── Status badge ──
    if state.mask_tool != MaskTool::None {
        let label = format!(
            "{} active — drag inside the selected element. Esc to cancel.",
            state.mask_tool.label()
        );
        let pos = Pos2::new(
            full_rect.center().x,
            full_rect.min.y + margin + btn_h * 0.5,
        );
        let painter = ui.painter_at(full_rect);
        let galley = painter.layout_no_wrap(
            label,
            egui::FontId::proportional(11.0),
            Color32::from_rgb(20, 20, 30),
        );
        let pad = Vec2::new(8.0, 3.0);
        let bg_rect = Rect::from_center_size(
            pos,
            galley.size() + pad * 2.0,
        );
        painter.rect_filled(
            bg_rect,
            Rounding::same(4.0),
            Color32::from_rgb(255, 200, 50),
        );
        painter.galley(
            bg_rect.min + pad,
            galley,
            Color32::from_rgb(20, 20, 30),
        );
    }
}

// ─── MASK / CROP DRAWING INPUT ───────────────────────────────────────
//
// Uses the same world-pixel coordinate system as the rest of the
// canvas. The drag origin is captured when the pointer first goes
// down inside the selected element's bounding box, then on every
// frame while the button is held the cursor's UV position relative
// to that bounding box is stored / appended. On release the resulting
// shape is committed to the element's `effects` stack as the right
// `EffectKind` variant for the active tool.

use crate::state::MaskTool;

/// True when the mask drawing pipeline should consume this pointer
/// gesture instead of the default transform handler. Always returns
/// `false` when no tool is armed.
pub(crate) fn mask_tool_active(state: &EditorState) -> bool {
    state.mask_tool != MaskTool::None
}

/// Convert a screen-space pointer to UV (0..1) inside the selected
/// element's screen rect. Returns `None` when no element is selected
/// or the rect has zero size.
fn screen_to_element_uv(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    pointer: Pos2,
) -> Option<([f32; 2], Selection, Rect)> {
    let elem_rect = selected_element_screen_rect(state, full_rect, viewport_size)?;
    if elem_rect.width() <= 0.5 || elem_rect.height() <= 0.5 {
        return None;
    }
    let u = (pointer.x - elem_rect.min.x) / elem_rect.width();
    let v = (pointer.y - elem_rect.min.y) / elem_rect.height();
    Some(([u.clamp(-0.5, 1.5), v.clamp(-0.5, 1.5)], state.selection, elem_rect))
}

/// Run the mask drawing input handler. Must be called from
/// `canvas_preview` BEFORE the regular transform pipeline so the mask
/// gesture has priority. Returns `true` when the gesture has been
/// consumed (caller should skip the default handlers).
pub(crate) fn handle_mask_draw_input(
    ui: &egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> bool {
    if !mask_tool_active(state) {
        return false;
    }
    // The crop / rect / ellipse / freehand tools all dispatch through
    // the same code path; a single `CanvasDragMode::DrawMask` carries
    // the active tool tag.
    let pointer_pos = ui.input(|i| i.pointer.hover_pos());

    // Pointer-down inside the selected element starts the drag.
    if response.drag_started() || (response.clicked() && pointer_pos.is_some()) {
        if let Some(p) = pointer_pos {
            if let Some((uv, target, _rect)) =
                screen_to_element_uv(state, full_rect, viewport_size, p)
            {
                state.canvas_drag.mode = crate::state::CanvasDragMode::DrawMask {
                    tool: state.mask_tool,
                    start_uv: uv,
                    target,
                };
                state.mask_draft_points.clear();
                state.mask_draft_points.push(uv);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }
    }

    // While the drag is in flight, accumulate freehand vertices and
    // request a per-frame repaint so the preview line follows the
    // cursor smoothly.
    if let crate::state::CanvasDragMode::DrawMask { tool, .. } = state.canvas_drag.mode {
        ui.ctx().request_repaint();
        if let Some(p) = pointer_pos {
            if let Some((uv, _, _)) =
                screen_to_element_uv(state, full_rect, viewport_size, p)
            {
                if matches!(tool, MaskTool::FreehandMask) {
                    // Decimate so the path doesn't accumulate thousands
                    // of duplicate points when the cursor barely moves.
                    let push = match state.mask_draft_points.last() {
                        Some(prev) => {
                            let dx = uv[0] - prev[0];
                            let dy = uv[1] - prev[1];
                            (dx * dx + dy * dy).sqrt() > 0.005
                        }
                        None => true,
                    };
                    if push {
                        state.mask_draft_points.push(uv);
                    }
                } else if state.mask_draft_points.len() >= 2 {
                    state.mask_draft_points[1] = uv;
                } else {
                    state.mask_draft_points.push(uv);
                }
            }
        }
    }

    // Pointer-release commits the shape.
    let released = ui.input(|i| i.pointer.any_released());
    if released {
        if let crate::state::CanvasDragMode::DrawMask { tool, start_uv, target } =
            state.canvas_drag.mode
        {
            commit_mask_draft(state, tool, start_uv, target);
            state.canvas_drag.mode = crate::state::CanvasDragMode::None;
            state.mask_draft_points.clear();
        }
    }
    true
}

/// Push the painted shape onto the target element's `effects` stack.
/// Crop becomes `EffectKind::Crop`; the other tools build an
/// `EffectKind::Mask` carrying the matching shape.
fn commit_mask_draft(
    state: &mut EditorState,
    tool: MaskTool,
    start_uv: [f32; 2],
    target: Selection,
) {
    let pts = state.mask_draft_points.clone();
    if pts.is_empty() { return; }
    // Bound the rect / ellipse to the smallest enclosing axis-aligned
    // box of the drag (start, current).
    let last_uv = *pts.last().unwrap_or(&start_uv);
    let lx = start_uv[0].min(last_uv[0]).clamp(0.0, 1.0);
    let rx = start_uv[0].max(last_uv[0]).clamp(0.0, 1.0);
    let ty = start_uv[1].min(last_uv[1]).clamp(0.0, 1.0);
    let by = start_uv[1].max(last_uv[1]).clamp(0.0, 1.0);
    if (rx - lx).abs() < 0.005 && (by - ty).abs() < 0.005 && tool != MaskTool::FreehandMask {
        // Treat tiny gestures as a misclick — don't commit anything.
        return;
    }

    let new_effect = match tool {
        MaskTool::Crop => {
            // Crop insets are measured FROM each edge in 0..0.49.
            let l = lx.clamp(0.0, 0.49);
            let t = ty.clamp(0.0, 0.49);
            let r = (1.0 - rx).clamp(0.0, 0.49);
            let b = (1.0 - by).clamp(0.0, 0.49);
            Some(memstroy_core::Effect::new(
                memstroy_core::EffectKind::Crop {
                    left: l,
                    top: t,
                    right: r,
                    bottom: b,
                },
            ))
        }
        MaskTool::RectMask => Some(memstroy_core::Effect::new(
            memstroy_core::EffectKind::Mask {
                shape: memstroy_core::MaskShape::Rect {
                    left: lx,
                    top: ty,
                    right: rx,
                    bottom: by,
                },
                feather: 0.0,
                invert: false,
            },
        )),
        MaskTool::EllipseMask => {
            let cx = (lx + rx) * 0.5;
            let cy = (ty + by) * 0.5;
            let rxx = ((rx - lx) * 0.5).max(0.005);
            let ryy = ((by - ty) * 0.5).max(0.005);
            Some(memstroy_core::Effect::new(
                memstroy_core::EffectKind::Mask {
                    shape: memstroy_core::MaskShape::Ellipse {
                        cx,
                        cy,
                        rx: rxx,
                        ry: ryy,
                    },
                    feather: 0.0,
                    invert: false,
                },
            ))
        }
        MaskTool::FreehandMask => {
            if pts.len() < 3 { None } else {
                let clamped: Vec<[f32; 2]> = pts
                    .into_iter()
                    .map(|p| [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)])
                    .collect();
                Some(memstroy_core::Effect::new(
                    memstroy_core::EffectKind::Mask {
                        shape: memstroy_core::MaskShape::Polygon { points: clamped },
                        feather: 0.0,
                        invert: false,
                    },
                ))
            }
        }
        MaskTool::None => None,
    };

    let Some(effect) = new_effect else { return; };

    // Fold the new effect into the target element's stack inside a
    // single `mutate` call so it lands as one undo step.
    state.mutate(|scene| {
        match target {
            Selection::Actor(i) if i < scene.actors.len() => {
                scene.actors[i].effects.push(effect);
            }
            Selection::Overlay(i) if i < scene.overlays.len() => {
                let effects = match &mut scene.overlays[i] {
                    memstroy_core::Overlay::Text(t) => &mut t.effects,
                    memstroy_core::Overlay::Image(im) => &mut im.effects,
                    memstroy_core::Overlay::Video(v) => &mut v.effects,
                };
                effects.push(effect);
            }
            _ => {}
        }
    });
    state.status = format!("\u{2702} {} applied", tool.label());
}

/// Draw the in-progress mask shape on top of the canvas while the
/// user is dragging. Visual only — committed shapes render through
/// the live image-effects pipeline.
fn draw_mask_draft(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let crate::state::CanvasDragMode::DrawMask { tool, start_uv, .. } =
        state.canvas_drag.mode
    else {
        return;
    };
    let Some(elem_rect) =
        selected_element_screen_rect(state, full_rect, viewport_size)
    else {
        return;
    };
    let to_screen = |uv: [f32; 2]| {
        Pos2::new(
            elem_rect.min.x + uv[0] * elem_rect.width(),
            elem_rect.min.y + uv[1] * elem_rect.height(),
        )
    };
    let stroke_main = Stroke::new(1.5, Color32::from_rgb(255, 200, 50));
    let stroke_dash = Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 200, 50, 160));
    let last_uv = *state
        .mask_draft_points
        .last()
        .unwrap_or(&start_uv);
    match tool {
        MaskTool::Crop | MaskTool::RectMask => {
            let a = to_screen(start_uv);
            let b = to_screen(last_uv);
            let r = Rect::from_two_pos(a, b);
            painter.rect_stroke(r, Rounding::ZERO, stroke_main);
            // Dim the area being trimmed away so the user can preview
            // the cropped result without committing.
            if matches!(tool, MaskTool::Crop) {
                let dim = Color32::from_rgba_premultiplied(0, 0, 0, 110);
                let er = elem_rect;
                if r.min.y > er.min.y {
                    painter.rect_filled(
                        Rect::from_min_max(er.min, Pos2::new(er.max.x, r.min.y)),
                        Rounding::ZERO, dim);
                }
                if r.max.y < er.max.y {
                    painter.rect_filled(
                        Rect::from_min_max(Pos2::new(er.min.x, r.max.y), er.max),
                        Rounding::ZERO, dim);
                }
                if r.min.x > er.min.x {
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(er.min.x, r.min.y),
                            Pos2::new(r.min.x, r.max.y),
                        ),
                        Rounding::ZERO, dim);
                }
                if r.max.x < er.max.x {
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(r.max.x, r.min.y),
                            Pos2::new(er.max.x, r.max.y),
                        ),
                        Rounding::ZERO, dim);
                }
            }
        }
        MaskTool::EllipseMask => {
            let a = to_screen(start_uv);
            let b = to_screen(last_uv);
            let r = Rect::from_two_pos(a, b);
            // Approximate an ellipse with a dense polyline.
            let cx = r.center().x;
            let cy = r.center().y;
            let rx = r.width() * 0.5;
            let ry = r.height() * 0.5;
            let segments = 64;
            let mut prev = Pos2::new(cx + rx, cy);
            for s in 1..=segments {
                let theta = (s as f32 / segments as f32) * std::f32::consts::TAU;
                let p = Pos2::new(cx + rx * theta.cos(), cy + ry * theta.sin());
                painter.line_segment([prev, p], stroke_main);
                prev = p;
            }
        }
        MaskTool::FreehandMask => {
            if state.mask_draft_points.len() >= 2 {
                let mut prev = to_screen(state.mask_draft_points[0]);
                for &uv in state.mask_draft_points.iter().skip(1) {
                    let cur = to_screen(uv);
                    painter.line_segment([prev, cur], stroke_main);
                    prev = cur;
                }
                // Ghost line back to the start so the user knows the
                // path will be closed on release.
                if let Some(&first) = state.mask_draft_points.first() {
                    painter.line_segment([prev, to_screen(first)], stroke_dash);
                }
            }
        }
        MaskTool::None => {}
    }
}


// ─── SNAP HELPERS ────────────────────────────────────────────────────
/// element doesn't snap to itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapExclude {
    Actor(usize),
    Overlay(usize),
}

/// Snap a proposed world-space CENTER position to the nearest snap target on
/// each axis. Targets include:
///   - the render frame's left/center/right (X) and top/center/bottom (Y);
///   - every other element's centre (actors + overlays).
///
/// Threshold is fixed to ~6 screen pixels (converted to world space via the
/// current zoom) so the snap distance feels consistent regardless of zoom.
/// Returns the snapped (x, y) and any guides that activated for rendering.
fn snap_world_center(
    state: &EditorState,
    proposed_x: f32,
    proposed_y: f32,
    exclude: Option<SnapExclude>,
) -> (f32, f32, Vec<crate::state::SnapGuide>) {
    if !state.snap_enabled {
        return (proposed_x, proposed_y, Vec::new());
    }

    let zoom = state.canvas_viewport.zoom.max(0.0001);
    // 6 screen pixels worth of world-space distance.
    let thresh = 6.0 / zoom;

    let (xs, ys) = collect_snap_targets(state, exclude);

    let mut guides: Vec<crate::state::SnapGuide> = Vec::new();
    let mut snapped_x = proposed_x;
    let mut best_x = thresh;
    for &tx in &xs {
        let d = (proposed_x - tx).abs();
        if d < best_x {
            best_x = d;
            snapped_x = tx;
        }
    }
    if best_x < thresh {
        guides.push(crate::state::SnapGuide {
            axis: crate::state::SnapAxis::Vertical,
            world: snapped_x,
        });
    }

    let mut snapped_y = proposed_y;
    let mut best_y = thresh;
    for &ty in &ys {
        let d = (proposed_y - ty).abs();
        if d < best_y {
            best_y = d;
            snapped_y = ty;
        }
    }
    if best_y < thresh {
        guides.push(crate::state::SnapGuide {
            axis: crate::state::SnapAxis::Horizontal,
            world: snapped_y,
        });
    }

    (snapped_x, snapped_y, guides)
}

/// Collect every world-space X (vertical-line) and Y (horizontal-line) snap
/// target available in the current scene.
fn collect_snap_targets(
    state: &EditorState,
    exclude: Option<SnapExclude>,
) -> (Vec<f32>, Vec<f32>) {
    let mut xs = Vec::new();
    let mut ys = Vec::new();

    // Render frame edges + centre (the most commonly used alignment lines).
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, state.playhead);
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom.max(0.0001);
    let world_h = rh as f32 / rf_state.zoom.max(0.0001);
    let cx = rf_state.pos.x;
    let cy = rf_state.pos.y;
    xs.push(cx - world_w * 0.5);
    xs.push(cx);
    xs.push(cx + world_w * 0.5);
    ys.push(cy - world_h * 0.5);
    ys.push(cy);
    ys.push(cy + world_h * 0.5);

    // Other actors' centres.
    let t = state.playhead;
    for (i, actor) in state.scene.actors.iter().enumerate() {
        if exclude == Some(SnapExclude::Actor(i)) {
            continue;
        }
        let pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        xs.push(pos.x);
        ys.push(pos.y);
    }

    // Overlays' centres (convert from normalized to world).
    for (i, ov) in state.scene.overlays.iter().enumerate() {
        if exclude == Some(SnapExclude::Overlay(i)) {
            continue;
        }
        let layout_first = match ov {
            Overlay::Text(t) => t.layout.first().map(|kf| kf.value.pos),
            Overlay::Image(im) => im.layout.first().map(|kf| kf.value.pos),
            Overlay::Video(v) => v.layout.first().map(|kf| kf.value.pos),
        };
        if let Some([nx, ny]) = layout_first {
            let world_x = cx - world_w * 0.5 + nx * world_w;
            let world_y = cy - world_h * 0.5 + ny * world_h;
            xs.push(world_x);
            ys.push(world_y);
        }
    }

    (xs, ys)
}

/// Draw active snap guidelines as thin yellow lines spanning the full canvas
/// rect. Called at the end of `canvas_preview` so the lines sit on top of
/// every element.
fn draw_snap_guides(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    if state.canvas_drag.snap_guides.is_empty() {
        return;
    }
    let col = Color32::from_rgb(255, 220, 80);
    for guide in &state.canvas_drag.snap_guides {
        match guide.axis {
            crate::state::SnapAxis::Vertical => {
                let s = state
                    .canvas_viewport
                    .world_to_screen(WorldPos { x: guide.world, y: 0.0 }, viewport_size);
                let sx = full_rect.min.x + s[0];
                if sx >= full_rect.min.x && sx <= full_rect.max.x {
                    painter.line_segment(
                        [
                            Pos2::new(sx, full_rect.min.y),
                            Pos2::new(sx, full_rect.max.y),
                        ],
                        Stroke::new(1.0, col),
                    );
                }
            }
            crate::state::SnapAxis::Horizontal => {
                let s = state
                    .canvas_viewport
                    .world_to_screen(WorldPos { x: 0.0, y: guide.world }, viewport_size);
                let sy = full_rect.min.y + s[1];
                if sy >= full_rect.min.y && sy <= full_rect.max.y {
                    painter.line_segment(
                        [
                            Pos2::new(full_rect.min.x, sy),
                            Pos2::new(full_rect.max.x, sy),
                        ],
                        Stroke::new(1.0, col),
                    );
                }
            }
        }
    }
}


// ─── LIBRARY DRAG-TO-CANVAS ──────────────────────────────────────────

/// Render a floating preview card next to the cursor while a library clip
/// is being dragged over the canvas, plus accept the drop and add the actor
/// at the cursor's world position. Mirrors the timeline drag-ghost so the
/// drop position can be picked freely on either panel.
pub fn handle_canvas_asset_drag(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) {
    if state.asset_drag.dragging.is_none() {
        return;
    }

    let pointer_pos = ui.input(|i| i.pointer.hover_pos());
    let in_canvas = pointer_pos.map(|p| full_rect.contains(p)).unwrap_or(false);

    // Only render the ghost while the cursor is over the canvas (otherwise
    // the timeline is responsible for it).
    if !in_canvas {
        return;
    }

    let drag_pos = pointer_pos.unwrap();
    state.asset_drag.pos = [drag_pos.x, drag_pos.y];

    // Translucent crosshair at the proposed drop point.
    let painter = ui.painter_at(full_rect);
    painter.circle_stroke(
        drag_pos,
        18.0,
        Stroke::new(1.5, Color32::from_rgb(255, 220, 80)),
    );
    painter.line_segment(
        [
            Pos2::new(drag_pos.x - 24.0, drag_pos.y),
            Pos2::new(drag_pos.x + 24.0, drag_pos.y),
        ],
        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 220, 80, 180)),
    );
    painter.line_segment(
        [
            Pos2::new(drag_pos.x, drag_pos.y - 24.0),
            Pos2::new(drag_pos.x, drag_pos.y + 24.0),
        ],
        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 220, 80, 180)),
    );

    // Floating thumbnail card next to the cursor.
    let card_w = 180.0_f32;
    let card_h = 56.0_f32;
    let anchor = drag_pos + egui::vec2(20.0, 16.0);
    let card_rect = Rect::from_min_size(anchor, Vec2::new(card_w, card_h));
    painter.rect_filled(
        card_rect,
        Rounding::same(6.0),
        Color32::from_rgba_premultiplied(20, 20, 30, 230),
    );
    painter.rect_stroke(
        card_rect,
        Rounding::same(6.0),
        Stroke::new(1.5, Color32::from_rgb(255, 200, 50)),
    );
    let thumb_size = Vec2::splat(48.0);
    let thumb_rect = Rect::from_min_size(card_rect.min + egui::vec2(4.0, 4.0), thumb_size);
    if let Some(thumb) = &state.asset_drag.thumbnail {
        let uri = format!("file://{}", thumb.display());
        let img = egui::Image::from_uri(uri)
            .fit_to_exact_size(thumb_size)
            .maintain_aspect_ratio(false)
            .rounding(Rounding::same(3.0))
            .tint(Color32::from_white_alpha(220));
        img.paint_at(ui, thumb_rect);
    } else {
        painter.rect_filled(thumb_rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 60));
        painter.text(
            thumb_rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{1F3AC}",
            egui::FontId::proportional(24.0),
            Color32::from_rgb(255, 200, 50),
        );
    }
    let label = if state.asset_drag.label.is_empty() {
        "Drop on canvas".to_string()
    } else {
        state.asset_drag.label.clone()
    };
    let text_anchor = thumb_rect.right_top() + egui::vec2(6.0, 4.0);
    painter.text(
        text_anchor,
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(220, 220, 240),
    );
    painter.text(
        text_anchor + egui::vec2(0.0, 18.0),
        egui::Align2::LEFT_TOP,
        "drop here to place at cursor",
        egui::FontId::proportional(9.0),
        Color32::from_rgb(160, 160, 180),
    );

    // ── Accept drop on release ──
    let mouse_released = ui.input(|i| i.pointer.any_released());
    if mouse_released {
        let world = state
            .canvas_viewport
            .screen_to_world([drag_pos.x - full_rect.min.x, drag_pos.y - full_rect.min.y], viewport_size);
        let asset_path = state.asset_drag.dragging.clone().unwrap();
        let asset_label = state.asset_drag.label.clone();
        let kind = state.asset_drag.kind;
        match kind {
            crate::state::AssetDragKind::Clip | crate::state::AssetDragKind::Video => {
                crate::panels::add_actor_from_clip_at_canvas(state, &asset_path, [world.x, world.y]);
            }
            crate::state::AssetDragKind::Sound
            | crate::state::AssetDragKind::Image
            | crate::state::AssetDragKind::Particle => {
                // Build a temporary LibraryAsset out of the drag payload —
                // the asset card already populated the path / label /
                // thumbnail, and the helper picks the right scene element
                // for the drag kind.
                let id = asset_path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("asset")
                    .to_string();
                let asset = crate::state::LibraryAsset {
                    id: id.clone(),
                    path: asset_path.clone(),
                    label: if asset_label.is_empty() { id } else { asset_label },
                    thumbnail: state.asset_drag.thumbnail.clone(),
                };
                crate::panels::add_library_asset_at_playhead(state, &asset, kind);
                // For images / particles, snap their normalised position
                // so they spawn under the cursor rather than the frame
                // centre default.
                if matches!(kind, crate::state::AssetDragKind::Image
                    | crate::state::AssetDragKind::Particle) {
                    let rf = &state.scene.render_frame;
                    let rf_state = sample_render_frame(rf, state.playhead);
                    let [rw, rh] = rf.resolution;
                    let world_w = rw as f32 / rf_state.zoom.max(1e-4);
                    let world_h = rh as f32 / rf_state.zoom.max(1e-4);
                    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
                    let frame_tl_y = rf_state.pos.y - world_h * 0.5;
                    if let Some(last) = state.scene.overlays.last_mut() {
                        let layout = match last {
                            Overlay::Image(im) => &mut im.layout,
                            Overlay::Video(v) => &mut v.layout,
                            Overlay::Text(t) => &mut t.layout,
                        };
                        if let Some(kf) = layout.first_mut() {
                            kf.value.pos = [
                                ((world.x - frame_tl_x) / world_w).clamp(-2.0, 3.0),
                                ((world.y - frame_tl_y) / world_h).clamp(-2.0, 3.0),
                            ];
                        }
                    }
                }
            }
            crate::state::AssetDragKind::None => {}
        }

        state.asset_drag.dragging = None;
        state.asset_drag.kind = crate::state::AssetDragKind::None;
        state.asset_drag.label.clear();
        state.asset_drag.thumbnail = None;
    }
}
