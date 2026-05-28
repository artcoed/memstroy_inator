//! Free Canvas preview panel.
//!
//! Renders an infinite 2D canvas with pan/zoom, the render frame
//! rectangle, and all scene elements positioned in world pixels.

use egui::{Color32, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
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
const COL_OVERLAY_IMAGE: Color32 = Color32::from_rgb(100, 180, 255);
const COL_OVERLAY_VIDEO: Color32 = Color32::from_rgb(255, 200, 80);
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

    // ── Skeleton authoring — when the inspector's Skeleton tab is
    //    active and the user has the matching video-layer element
    //    selected, dragging on top of an existing skeleton point on
    //    the canvas writes a keyframe at the current playhead. The
    //    handler runs BEFORE `draw_canvas_elements` so it can short-
    //    circuit `decide_drag_mode` and stop the click from also
    //    moving the host actor.
    let skeleton_drag_active =
        handle_canvas_skeleton_input(ui, &response, state, full_rect, viewport_size);

    // ── Draw grid ──
    draw_canvas_grid(&painter, full_rect, &state.canvas_viewport, viewport_size);

    // ── Draw elements (actors, overlays) ──
    draw_canvas_elements(ui, &painter, full_rect, state, viewport_size);

    // ── Draw skeleton-point overlay for the active video-layer
    //    selection (when the inspector's Skeleton tab / section is
    //    visible). Drawn AFTER elements so the markers sit on top of
    //    the actor preview but BEFORE the gizmo so the rotation /
    //    resize handles still appear above them.
    draw_canvas_skeleton_overlay(ui, &painter, full_rect, state, viewport_size);

    // ── Draw element gizmo for selected ──
    // Skip the regular drag state machine while a skeleton-point drag
    // owns the gesture so corner / resize handles don't fight the
    // point drag.
    if !skeleton_drag_active {
        draw_selection_gizmo(ui, &painter, &response, full_rect, state, viewport_size);
    } else {
        draw_selection_handles(&painter, full_rect, state, viewport_size);
    }

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
/// the selected element's source frame at the click's UV coordinate and
/// store the colour as the element's chroma key. The chroma sidecar is
/// updated so the change persists across projects. Works for actors AND
/// image overlays — the user explicitly asked for the latter so picture
/// stickers can use the same green-screen workflow.
fn handle_eyedropper_click(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    click_pos: Pos2,
) {
    match state.selection {
        Selection::Actor(idx) => {
            handle_eyedropper_click_actor(ui, state, full_rect, viewport_size, click_pos, idx);
        }
        Selection::Overlay(idx) => {
            handle_eyedropper_click_overlay(ui, state, full_rect, viewport_size, click_pos, idx);
        }
        _ => {
            state.status = crate::i18n::t("Eyedropper: select an actor or image overlay first.").into();
        }
    }
}

/// Eyedropper handler for actors — picks a pixel from the decoded
/// frame cache.
fn handle_eyedropper_click_actor(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    click_pos: Pos2,
    idx: usize,
) {
    if idx >= state.scene.actors.len() {
        return;
    }

    // Compute the actor's on-screen rectangle (mirror of the math used in
    // draw_canvas_elements).
    let elem_rect = match actor_screen_rect(state, full_rect, viewport_size, idx) {
        Some(r) => r,
        None => {
            state.status = crate::i18n::t("Eyedropper: cannot resolve actor rect.").into();
            return;
        }
    };

    // Click must be inside the actor's rect — otherwise we have no UV.
    if !elem_rect.contains(click_pos) {
        state.status = crate::i18n::t("Eyedropper: click on the actor's image.").into();
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
            state.status = format!("{} #{:02X}{:02X}{:02X}", crate::i18n::t("Picked chroma key"), key[0], key[1], key[2]);
            ui.ctx().request_repaint();
            return;
        }
    }
    state.status = crate::i18n::t("Eyedropper: frame not yet decoded — try again in a moment.").into();
}

/// Eyedropper handler for image overlays. Loads the source image
/// directly (small sticker PNGs decode in milliseconds) and writes the
/// sampled colour into the overlay's `chroma_key.key_color`. If the
/// overlay didn't already have a `chroma_key` configured, one is
/// initialised with default similarity / blend / spill so the keying
/// kicks in immediately.
fn handle_eyedropper_click_overlay(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    click_pos: Pos2,
    idx: usize,
) {
    if idx >= state.scene.overlays.len() {
        return;
    }
    // Only image overlays support the chroma_key field — text doesn't
    // make sense and video overlays go through the actor pipeline.
    let path = match &state.scene.overlays[idx] {
        Overlay::Image(im) => im.source.clone(),
        _ => {
            state.status =
                crate::i18n::t("Eyedropper: overlay must be an image (text / video not supported here).")
                    .into();
            return;
        }
    };
    let elem_rect = selected_element_screen_rect(state, full_rect, viewport_size);
    let Some(elem_rect) = elem_rect else {
        state.status = crate::i18n::t("Eyedropper: cannot resolve overlay rect.").into();
        return;
    };
    if !elem_rect.contains(click_pos) {
        state.status = crate::i18n::t("Eyedropper: click on the overlay's image.").into();
        return;
    }
    // Account for rotation just like the mask painter does — without
    // this an eyedropper click on a rotated sticker would sample the
    // wrong pixel.
    let rotation_deg = match &state.scene.overlays[idx] {
        Overlay::Image(im) => {
            let local_t = (state.playhead - im.t_in).max(0.0);
            keyframe::sample(&im.layout, local_t)
                .map(|s| s.rotation_deg)
                .unwrap_or(0.0)
        }
        _ => 0.0,
    };
    let center = elem_rect.center();
    let theta = (-rotation_deg).to_radians();
    let (s, c) = (theta.sin(), theta.cos());
    let dx = click_pos.x - center.x;
    let dy = click_pos.y - center.y;
    let local_x = dx * c - dy * s;
    let local_y = dx * s + dy * c;
    let u = ((local_x / elem_rect.width().max(0.001)) + 0.5).clamp(0.0, 1.0);
    let v = ((local_y / elem_rect.height().max(0.001)) + 0.5).clamp(0.0, 1.0);

    let path_buf = if path.is_absolute() {
        path
    } else {
        state.assets_root.join(path)
    };
    match image::open(&path_buf).map(|i| i.to_rgba8()) {
        Ok(rgba) => {
            let w = rgba.width() as usize;
            let h = rgba.height() as usize;
            if w == 0 || h == 0 {
                state.status = crate::i18n::t("Eyedropper: overlay image has zero size.").into();
                return;
            }
            let px = ((u * w as f32) as usize).min(w - 1);
            let py = ((v * h as f32) as usize).min(h - 1);
            let pixel = rgba.get_pixel(px as u32, py as u32);
            let key = [pixel[0], pixel[1], pixel[2]];
            if let Overlay::Image(im) = &mut state.scene.overlays[idx] {
                let mut ck = im
                    .chroma_key
                    .clone()
                    .unwrap_or_default();
                ck.key_color = key;
                // Default similarity is 0 in `ChromaKeyParams::default`;
                // bump it to a sensible starting value when the user
                // first picks a colour so the key actually does
                // something visible. They can dial it back from the
                // inspector later.
                if ck.similarity < 1.0e-3 {
                    ck.similarity = 0.18;
                }
                im.chroma_key = Some(ck);
            }
            state.status = format!(
                "{} #{:02X}{:02X}{:02X}",
                crate::i18n::t("Picked overlay key"),
                key[0], key[1], key[2]
            );
            ui.ctx().request_repaint();
        }
        Err(e) => {
            state.status = format!("{} {}", crate::i18n::t("Eyedropper: failed to read overlay image —"), e);
        }
    }
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
    let mut effective_scale = actor_state.scale;
    let mut effective_scale_y = actor_state.scale_y;
    // Apply parent's scale if this actor has a parent (mirrors the
    // draw path's `effective_scale *= parent_xform.scale`).
    if let Some(ref pid) = actor.parent_id {
        let mut visited = vec![actor.id.clone()];
        if let Some(pxf) = resolve_parent_transform(state, pid, t, &mut visited) {
            effective_scale *= pxf.scale_x;
            effective_scale_y *= safe_div(pxf.scale_y, pxf.scale_x);
        }
    }
    let elem_w = src_w * effective_scale;
    let elem_h = src_h * effective_scale * effective_scale_y;
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
    let mut ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();

    let [out_w, out_h] = state.scene.render_frame.resolution;
    let world_w = out_w as f32;
    let world_h = out_h as f32;
    let mut center_x = ov_state.pos[0] * world_w;
    let mut center_y = ov_state.pos[1] * world_h;

    // Apply parent transform (position + scale) so marquee-select and
    // hit-testing match what the user actually sees on canvas.
    let parent_id = match overlay {
        Overlay::Text(o) => o.parent_id.clone(),
        Overlay::Image(o) => o.parent_id.clone(),
        Overlay::Video(o) => o.parent_id.clone(),
    };
    let element_id = match overlay {
        Overlay::Text(o) => o.id.clone(),
        Overlay::Image(o) => o.id.clone(),
        Overlay::Video(o) => o.id.clone(),
    };
    if let Some(pid) = parent_id {
        let mut visited = vec![element_id];
        if let Some(pxf) = resolve_parent_transform(state, &pid, t, &mut visited) {
            // Apply parent translation + scale to the local position
            let world_pos = apply_parent_transform(
                WorldPos { x: center_x, y: center_y }, &pxf,
            );
            center_x = world_pos.x;
            center_y = world_pos.y;
            ov_state.scale *= pxf.scale_x;
            ov_state.scale_y *= safe_div(pxf.scale_y, pxf.scale_x);
        }
    }

    // Use the texture-aware bbox so image overlays use real PNG
    // dimensions (not the legacy 200×200 placeholder), keeping the
    // marquee hit-rect aligned with the visible picture.
    let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
    Some((
        [center_x - ew * 0.5, center_y - eh * 0.5],
        [center_x + ew * 0.5, center_y + eh * 0.5],
    ))
}

/// Pick a tint color for an FX zone based on the dominant active
/// effect. Returns `None` when there are no active effects (empty
/// or all-disabled stack) — caller draws just the outline.
///
/// The tint is intentionally translucent (~30% alpha) so the user
/// can still see lower layers through the FX zone while getting
/// quick visual feedback about which effect is active.
fn fx_zone_tint_for_effects(effects: &[memstroy_core::effects::Effect]) -> Option<Color32> {
    use memstroy_core::effects::EffectKind as K;
    // Use the first ENABLED effect with intensity > 0 to drive the tint.
    let dominant = effects.iter().find(|e| e.enabled && e.intensity > 0.001)?;
    let alpha: u8 = 70; // ~27% — visible but not blocking
    let c = match &dominant.kind {
        K::Blur { .. } | K::Bloom { .. } | K::Glow { .. } => {
            Color32::from_rgba_unmultiplied(180, 200, 255, alpha)
        }
        K::Grayscale => Color32::from_rgba_unmultiplied(160, 160, 170, alpha),
        K::Sepia => Color32::from_rgba_unmultiplied(220, 190, 140, alpha),
        K::Invert => Color32::from_rgba_unmultiplied(80, 60, 220, alpha),
        K::HueShift { .. } => Color32::from_rgba_unmultiplied(255, 100, 200, alpha),
        K::Vignette { .. } => Color32::from_rgba_unmultiplied(40, 30, 60, alpha + 20),
        K::Pixelate { .. } => Color32::from_rgba_unmultiplied(220, 220, 100, alpha),
        K::Posterize { .. } => Color32::from_rgba_unmultiplied(180, 200, 80, alpha),
        K::Brightness { amount } => {
            if *amount > 0.0 {
                Color32::from_rgba_unmultiplied(255, 240, 180, alpha)
            } else {
                Color32::from_rgba_unmultiplied(40, 50, 80, alpha)
            }
        }
        K::Contrast { .. } => Color32::from_rgba_unmultiplied(220, 100, 100, alpha),
        K::Saturation { amount } => {
            if *amount > 0.0 {
                Color32::from_rgba_unmultiplied(255, 130, 200, alpha)
            } else {
                Color32::from_rgba_unmultiplied(180, 180, 180, alpha)
            }
        }
        K::EdgeDetect { .. } => Color32::from_rgba_unmultiplied(40, 220, 220, alpha),
        K::MirrorH | K::MirrorV => {
            Color32::from_rgba_unmultiplied(140, 200, 140, alpha)
        }
        K::ChromaticAberration { .. } => {
            Color32::from_rgba_unmultiplied(255, 80, 220, alpha)
        }
        K::Noise { .. } => Color32::from_rgba_unmultiplied(180, 180, 180, alpha),
        K::Wave { .. } => Color32::from_rgba_unmultiplied(80, 200, 220, alpha),
        K::OldFilm => Color32::from_rgba_unmultiplied(180, 150, 100, alpha),
        K::Vhs => Color32::from_rgba_unmultiplied(200, 100, 220, alpha),
        K::Glitch { .. } => Color32::from_rgba_unmultiplied(255, 80, 80, alpha),
        K::Sharpen { .. } => Color32::from_rgba_unmultiplied(220, 220, 100, alpha),
        K::Crop { .. } => Color32::from_rgba_unmultiplied(255, 140, 220, alpha),
        K::Mask { .. } => Color32::from_rgba_unmultiplied(255, 200, 80, alpha),
        K::ColorKey { color, .. } => Color32::from_rgba_unmultiplied(
            color[0], color[1], color[2], alpha,
        ),
    };
    Some(c)
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

    // Draw backgrounds first (bottom layer)
    draw_canvas_backgrounds(painter, full_rect, state, viewport_size);

    // ── Unified z-ordered pass ──
    //
    // Actors and overlays are interleaved by their timeline track index
    // so the canvas stacking matches what the user sees in the layer
    // panel. Lower track index = higher on the panel = drawn LAST (on
    // top). This replaces the old three-phase approach (overlays-behind
    // → all actors → overlays-on-top) which couldn't handle an overlay
    // sitting between two actors on different tracks.
    //
    // We build a unified list of (track_index, element_kind, scene_index)
    // tuples, sort by track_index DESCENDING (so the bottom-of-panel
    // elements are drawn first and end up visually behind), then draw
    // each element in order.

    #[derive(Clone, Copy)]
    enum ElementKind { Actor, Overlay }

    let actors_to_draw = pick_actors_for_canvas(state, t);
    let overlays_to_draw = pick_overlays_for_canvas(state, t);

    let mut elements: Vec<(usize, ElementKind, usize)> = Vec::new();

    // Collect actors.
    for (idx, actor) in state.scene.actors.iter().enumerate() {
        if !actor.visible { continue; }
        if !actors_to_draw.contains(&idx) { continue; }
        let track = actor_track_index(state, idx);
        elements.push((track, ElementKind::Actor, idx));
    }

    // Collect overlays (image/video filtered by pick, text always included).
    for (idx, ov) in state.scene.overlays.iter().enumerate() {
        let dominated = match ov {
            Overlay::Text(_) => false, // text always drawn
            _ => !overlays_to_draw.contains(&idx),
        };
        if dominated { continue; }
        let track = overlay_track_index(state, idx);
        elements.push((track, ElementKind::Overlay, idx));
    }

    // Sort: larger track index drawn FIRST (visually behind), smaller
    // track index drawn LAST (visually on top). Within the same track,
    // preserve scene order (stable sort).
    elements.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));

    // Draw each element in z-order.
    for &(_, kind, idx) in &elements {
        match kind {
            ElementKind::Actor => {
                draw_single_actor(ui, painter, full_rect, state, viewport_size, idx);
            }
            ElementKind::Overlay => {
                draw_single_overlay(painter, full_rect, state, viewport_size, idx);
            }
        }
    }

    // Draw the keyframe trajectory for the selected element on top of
    // everything else, so the user can see the animation path with numbered
    // points and per-keyframe parameter callouts.
    draw_selection_keyframe_trajectory(painter, full_rect, state, viewport_size);
}

// ─── SINGLE-ELEMENT DRAWING (used by unified z-order pass) ──────────

/// Draw a single actor on the canvas. Extracted from the old per-actor
/// loop so the unified z-order pass can interleave actors with overlays.
fn draw_single_actor(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    full_rect: Rect,
    state: &mut EditorState,
    viewport_size: [f32; 2],
    idx: usize,
) {
    let t = state.playhead;
    let duration = state.scene.output.duration;
    let actor = &state.scene.actors[idx];

    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(duration);

    let display_mode = if t >= t_in && t <= t_out {
        DisplayMode::Active
    } else if t < t_in {
        DisplayMode::BeforeStart
    } else {
        DisplayMode::AfterEnd
    };

    let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
    let (src_w, src_h) = if let Some(fc) = state.frame_caches.get(idx) {
        if fc.is_ready() && fc.frame_count > 0 {
            (fc.source_width as f32, fc.source_height as f32)
        } else {
            (1080.0_f32, 1920.0)
        }
    } else {
        (1080.0_f32, 1920.0)
    };
    let actor_state = keyframe::sample(&actor.layout, t).unwrap_or_default();
    let mut actor_scale = actor_state.scale;
    let mut actor_scale_y = actor_state.scale_y;
    let mut actor_rotation = actor_state.rotation_deg;
    apply_canvas_transform_preview(state, &actor.id, &mut actor_rotation, &mut actor_scale, &mut actor_scale_y);
    let mod_delta = if matches!(display_mode, DisplayMode::Active) {
        keyframe::evaluate_modifiers(&actor.modifiers, t - t_in)
    } else {
        keyframe::ModifierDelta::default()
    };
    actor_rotation += mod_delta.d_rotation_deg;
    let actor_opacity = actor_state.opacity;
    let actor_flip_x = actor_state.flip_x_anim;
    let actor_flip_y = actor_state.flip_y_anim;
    let mut scale_eff = (actor_scale + mod_delta.d_scale).max(0.001);

    // ── Parent transform inheritance (rotation + scale) ──
    if let Some(ref pid) = actor.parent_id {
        let mut visited = vec![actor.id.clone()];
        if let Some(parent_xform) = resolve_parent_transform(state, pid, t, &mut visited) {
            actor_rotation += parent_xform.rotation_deg;
            scale_eff *= parent_xform.scale_x;
            actor_scale_y *= safe_div(parent_xform.scale_y, parent_xform.scale_x);
        }
    }

    let elem_width = src_w * scale_eff;
    let elem_height = src_h * scale_eff * actor_scale_y;

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

    if !full_rect.intersects(elem_rect) { return; }

    let base_tint = match display_mode {
        DisplayMode::Active => Color32::WHITE,
        _ => COL_INACTIVE_TINT,
    };
    let alpha = (actor_opacity * (base_tint.a() as f32 / 255.0)).clamp(0.0, 1.0);
    let tint = Color32::from_rgba_unmultiplied(base_tint.r(), base_tint.g(), base_tint.b(), (alpha * 255.0) as u8);

    let speed = actor.speed.max(0.0001);
    let local_t = match display_mode {
        DisplayMode::Active => (t - t_in) * speed + actor.source_start,
        DisplayMode::BeforeStart => actor.source_start,
        DisplayMode::AfterEnd => actor.source_start + (t_out - t_in) * speed,
    };

    let mut frame_shown = false;
    if let Some(fc) = state.frame_caches.get_mut(idx) {
        if fc.is_ready() && fc.frame_count > 0 {
            let actor_ck = &state.scene.actors[idx].chroma_key;
            let actor_t_in = state.scene.actors[idx].t_in.unwrap_or(0.0);
            let local_for_anim = (state.playhead - actor_t_in).max(0.0);
            let actor_cc_owned: memstroy_core::ColorCorrection =
                state.scene.actors[idx].color_correction.sampled_at(local_for_anim);
            let actor_cc = &actor_cc_owned;
            let actor_fx_owned: Vec<memstroy_core::Effect> = state
                .scene.actors[idx].effects
                .iter()
                .map(|e| e.sampled_at(local_for_anim))
                .collect();
            let actor_fx = &actor_fx_owned;
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
        let (extracting, preview_failed, cache_ready) = state
            .frame_caches
            .get(idx)
            .map(|fc| (fc.extracting, fc.failed, fc.is_ready()))
            .unwrap_or((false, false, false));
        let preview_pending =
            !preview_failed && (extracting || !cache_ready);

        // While ffmpeg extracts preview frames, show the library
        // thumbnail so the user sees the clip land on canvas; once
        // extraction finishes `frame_shown` flips to the live video.
        if preview_pending {
            if let Some(thumb) =
                crate::panels::clip_thumbnail_for_source(state, &state.scene.actors[idx].source)
            {
                let uri = format!("file://{}", thumb.display());
                let img = egui::Image::from_uri(uri)
                    .fit_to_exact_size(elem_rect.size())
                    .maintain_aspect_ratio(true)
                    .rounding(Rounding::same(3.0));
                img.paint_at(ui, elem_rect);
                frame_shown = true;
            }
        }

        if !frame_shown {
            let fill = match display_mode {
                DisplayMode::Active => Color32::from_rgb(44, 42, 28),
                _ => Color32::from_rgb(32, 30, 20),
            };
            painter.rect_filled(elem_rect, Rounding::same(3.0), fill);
            let label = if preview_failed {
                crate::i18n::t("Video preview failed")
            } else if extracting {
                crate::i18n::t("Loading video...")
            } else {
                state.scene.actors[idx].id.as_str()
            };
            painter.text(
                elem_rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(10.0),
                Color32::from_rgb(140, 140, 160),
            );
        }
    }

    let multi_selected = state.canvas_selection.iter().any(|s| *s == Selection::Actor(idx));
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

    if display_mode != DisplayMode::Active {
        let badge = match display_mode {
            DisplayMode::BeforeStart => crate::i18n::t("FIRST"),
            DisplayMode::AfterEnd => crate::i18n::t("LAST"),
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

/// Draw a single overlay on the canvas. Delegates to the existing
/// `draw_canvas_overlays` infrastructure but for a single element.
fn draw_single_overlay(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    idx: usize,
) {
    // Draw this single overlay using the shared overlay drawing path.
    // Pass `Some(idx)` to restrict drawing to just this one element.
    draw_canvas_overlays_impl(painter, full_rect, state, viewport_size, None, Some(idx));
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
                (c, format!("{} #{}", crate::i18n::t("Solid"), bg.id))
            }
            MediaSource::Image { path } => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("img");
                (Color32::from_rgb(30, 50, 70), format!("{} {}", crate::i18n::t("BG:"), name))
            }
            MediaSource::Video { path, .. } => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("vid");
                (Color32::from_rgb(25, 40, 60), format!("{} {}", crate::i18n::t("BG:"), name))
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
                DisplayMode::BeforeStart => crate::i18n::t("FIRST"),
                DisplayMode::AfterEnd => crate::i18n::t("LAST"),
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
#[allow(dead_code)]
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

#[allow(dead_code)]
fn draw_canvas_overlays(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    pass: OverlayPass,
) {
    draw_canvas_overlays_impl(painter, full_rect, state, viewport_size, Some(pass), None);
}

/// Core overlay drawing implementation.
/// - `pass`: if `Some`, filter overlays by BehindActors/OnTop classification.
///   If `None`, draw regardless of pass (used by the unified z-order path).
/// - `only_idx`: if `Some(idx)`, draw only the overlay at that index.
///   If `None`, draw all overlays that pass the filter.
fn draw_canvas_overlays_impl(
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
    pass: Option<OverlayPass>,
    only_idx: Option<usize>,
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
            // Single-element mode: only draw the requested overlay.
            if let Some(only) = only_idx {
                return *idx == only;
            }
            // Pass filter (legacy two-phase path).
            if let Some(p) = pass {
                let behind = overlay_is_behind_actors(state, *idx);
                let kind_ok = match p {
                    OverlayPass::BehindActors => behind,
                    OverlayPass::OnTop => !behind,
                };
                if !kind_ok { return false; }
            }
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
        let overlay_id: &str = match overlay {
            Overlay::Text(txt) => &txt.id,
            Overlay::Image(img) => &img.id,
            Overlay::Video(vid) => &vid.id,
        };
        apply_canvas_transform_preview(
            state,
            overlay_id,
            &mut ov_state.rotation_deg,
            &mut ov_state.scale,
            &mut ov_state.scale_y,
        );

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

        // ── Parent transform inheritance (rotation + scale + position) ──
        let overlay_parent_id: Option<&String> = match overlay {
            Overlay::Text(txt) => txt.parent_id.as_ref(),
            Overlay::Image(img) => img.parent_id.as_ref(),
            Overlay::Video(vid) => vid.parent_id.as_ref(),
        };
        let parent_xform_resolved = overlay_parent_id.and_then(|pid| {
            let mut visited = vec![overlay_id.to_string()];
            resolve_parent_transform(state, pid, t, &mut visited)
        });
        if let Some(ref pxf) = parent_xform_resolved {
            ov_state.rotation_deg += pxf.rotation_deg;
            ov_state.scale *= pxf.scale_x;
            ov_state.scale_y *= safe_div(pxf.scale_y, pxf.scale_x);
        }

        let rf = &state.scene.render_frame;
        // World-space size of the FIXED reference rectangle that the
        // legacy normalised `pos` is interpreted against. Decoupled
        // from the live render frame: editing rf.pos / rf.zoom no
        // longer drags this overlay along on the canvas. The rf is a
        // pure camera viewport — its parameters change WHAT gets
        // captured to the output, not WHERE world-space layers sit.
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32;
        let world_h = rh as f32;

        // Default world position from layout. Modifier offsets and any
        // skeleton attachment can override / shift it.
        let mut world_pos = WorldPos {
            x: ov_state.pos[0] * world_w + mod_delta.dx,
            y: ov_state.pos[1] * world_h + mod_delta.dy,
        };

        // ── Apply parent position transform ──
        if let Some(ref pxf) = parent_xform_resolved {
            world_pos = apply_parent_transform(world_pos, pxf);
        }

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
                // ── FX element fast path ──
                // When the source path is empty, this overlay is an
                // FX zone — it has no image content, only effects that
                // (at export time) apply to layers below it. On the
                // canvas preview we just draw the bounding box outline
                // and a small "FX" label so the user can see / move /
                // resize the zone. The actual effect-on-lower-layers
                // compositing only happens during export.
                let is_fx_zone = img.source.as_os_str().is_empty();
                if is_fx_zone {
                    // FX zones default to a larger box so they're
                    // immediately useful as effect regions.
                    let fx_w = 600.0_f32 * ov_state.scale;
                    let fx_h = 400.0_f32 * ov_state.scale * ov_state.scale_y;
                    let half_w = fx_w * 0.5 * zoom;
                    let half_h = fx_h * 0.5 * zoom;
                    let elem_rect = Rect::from_center_size(
                        center_pos,
                        Vec2::new(half_w * 2.0, half_h * 2.0),
                    );
                    if !full_rect.intersects(elem_rect) { continue; }

                    // ── Live FX preview ──
                    // Try to bake/fetch a preview that shows the FX
                    // zone's effect stack applied to the composite of
                    // image overlays drawn BEFORE this one within its
                    // bbox. Falls back to the simple tint when no
                    // image overlays contribute (no preview possible)
                    // OR when the cache miss can't decode the source
                    // image yet.
                    //
                    // Compute world-space bbox of the FX zone.
                    let fx_bbox_min = [
                        world_pos.x - fx_w * 0.5,
                        world_pos.y - fx_h * 0.5,
                    ];
                    let fx_bbox_max = [
                        world_pos.x + fx_w * 0.5,
                        world_pos.y + fx_h * 0.5,
                    ];
                    let fx_preview_tex = crate::fx_preview::ensure_fx_preview(
                        state,
                        &state.fx_preview_cache,
                        idx,
                        &img.id,
                        fx_bbox_min,
                        fx_bbox_max,
                        &img.effects,
                        t,
                        painter.ctx(),
                    );

                    // Rotated rect corners
                    let rotation_rad = ov_state.rotation_deg.to_radians();
                    let cos_r = rotation_rad.cos();
                    let sin_r = rotation_rad.sin();
                    let center = elem_rect.center();
                    let corners_local = [[-half_w, -half_h], [half_w, -half_h], [half_w, half_h], [-half_w, half_h]];
                    let corners: [Pos2; 4] = std::array::from_fn(|i| {
                        let lx = corners_local[i][0];
                        let ly = corners_local[i][1];
                        Pos2::new(
                            center.x + lx * cos_r - ly * sin_r,
                            center.y + lx * sin_r + ly * cos_r,
                        )
                    });

                    if let Some(tex) = fx_preview_tex {
                        // Draw the baked preview as a textured quad
                        // matching the FX bbox. Egui's Mesh handles
                        // rotation via per-vertex positions.
                        let mut mesh = egui::Mesh::with_texture(tex.id());
                        let uvs = [
                            egui::pos2(0.0, 0.0),
                            egui::pos2(1.0, 0.0),
                            egui::pos2(1.0, 1.0),
                            egui::pos2(0.0, 1.0),
                        ];
                        for (i, c) in corners.iter().enumerate() {
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos: *c,
                                uv: uvs[i],
                                color: Color32::WHITE,
                            });
                        }
                        mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                        painter.add(egui::Shape::mesh(mesh));
                    } else {
                        // Fallback: tint based on dominant effect
                        let tint = fx_zone_tint_for_effects(&img.effects);
                        if let Some(fill) = tint {
                            if ov_state.rotation_deg.abs() > 0.5 {
                                painter.add(egui::Shape::convex_polygon(
                                    corners.to_vec(),
                                    fill,
                                    Stroke::NONE,
                                ));
                            } else {
                                painter.rect_filled(elem_rect, Rounding::ZERO, fill);
                            }
                        }
                    }

                    // Pink dashed outline always
                    let stroke = Stroke::new(2.0, Color32::from_rgb(255, 140, 220));
                    for i in 0..4 {
                        painter.line_segment([corners[i], corners[(i + 1) % 4]], stroke);
                    }

                    // Label in top-left with effect summary
                    let label_pos = Pos2::new(
                        center.x - half_w * cos_r + half_h * sin_r + 8.0,
                        center.y - half_w * sin_r - half_h * cos_r + 12.0,
                    );
                    let active_count = img.effects.iter().filter(|e| e.enabled).count();
                    let label_text = if active_count == 0 {
                        format!("FX: {}", img.id)
                    } else if active_count == 1 {
                        let kind_label = img.effects.iter()
                            .find(|e| e.enabled)
                            .map(|e| e.kind.label())
                            .unwrap_or("FX");
                        format!("FX [{}]", kind_label)
                    } else {
                        format!("FX [{}× effects]", active_count)
                    };
                    painter.text(
                        label_pos,
                        egui::Align2::LEFT_TOP,
                        label_text,
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(255, 140, 220),
                    );
                    // Skip the rest of the image-overlay rendering for FX zones
                    continue;
                }
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
                            DisplayMode::BeforeStart => crate::i18n::t("FIRST"),
                            DisplayMode::AfterEnd => crate::i18n::t("LAST"),
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
                            "{} {}",
                            crate::i18n::t("IMG (missing):"),
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
                    &format!("{} {}", crate::i18n::t("VID:"), vid.source.file_name().and_then(|s| s.to_str()).unwrap_or("?")),
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
            DisplayMode::BeforeStart => crate::i18n::t("FIRST"),
            DisplayMode::AfterEnd => crate::i18n::t("LAST"),
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
    // ── Effective font size ──
    //
    // The previous build pinned glyph size to `font_size * zoom`
    // alone, so on-canvas resize handles (which write to
    // `ov_state.scale`) only changed the plate padding and never the
    // visible text — the user reported it as "текст при изменении
    // размеров на холсте ведёт себя неадекватно". Image overlays
    // scale their pixel rect by the same `scale` channel, so we now
    // mirror that behaviour for text too: dragging a corner handle
    // resizes both the glyphs and the surrounding plate uniformly.
    //
    // Anisotropic Y stretch (driven by `scale_y` independently of
    // `scale`) is folded into the per-glyph mesh transform inside
    // `paint_text_line_flipped`, so we can author it without
    // requesting an anisotropic font from egui (egui's FontId only
    // exposes a single size).
    let zoom = state.canvas_viewport.zoom;
    let elem_scale = ov_state.scale.max(0.001);
    let elem_scale_y = ov_state.scale_y.max(0.001);
    // ── Continuous flip channels (match image-overlay semantics) ──
    //
    // For images, `flip_x_anim` doubles as a signed scale factor
    // around the X axis — its absolute value shrinks the picture as
    // it crosses 0, while its sign mirrors the texture. Text used to
    // collapse these into a boolean (`< 0.0 ⇒ mirrored, otherwise
    // full size`), giving only two visual states and breaking
    // mid-flight animations. We now treat them the same way: the
    // sign drives the mirror, and the magnitude scales the text /
    // plate along that axis. `min(0.02)` keeps the plate from
    // disappearing exactly at the crossover point.
    let flip_x_factor = if ov_state.flip_x_anim.abs() < 1.0e-3 {
        if ov_state.flip_x_anim < 0.0 { -0.02 } else { 0.02 }
    } else {
        ov_state.flip_x_anim
    };
    let flip_y_factor = if ov_state.flip_y_anim.abs() < 1.0e-3 {
        if ov_state.flip_y_anim < 0.0 { -0.02 } else { 0.02 }
    } else {
        ov_state.flip_y_anim
    };
    // Glyph base size — uniform via `elem_scale`. Anisotropic stretch
    // (and the flip mirror itself) is added in the per-glyph mesh.
    let effective_size = (style.font_size * zoom * elem_scale).clamp(4.0, 1024.0);
    // Italic is faked via a horizontal skew on each glyph row. Slightly
    // larger than the previous 0.18 so the slant reads at small sizes.
    let italic_skew = if style.italic { 0.22 } else { 0.0 };
    let rotation_rad = ov_state.rotation_deg.to_radians();
    let rotated = rotation_rad.abs() > 0.001;

    // Logical font family resolution:
    //   * "Monospace" / "Courier" / "Hack" → bundled monospace family.
    //   * "Default" / "Proportional"      → bundled proportional family.
    //   * Anything else                    → ensure the matching system
    //     TTF is loaded (lazy, idempotent) and reference it via
    //     `FontFamily::Name(<family>)`. If the load fails (no TTF on
    //     disk or parse error) we fall back to Proportional so the
    //     glyphs still render in *some* font instead of disappearing.
    let family = if style.font.eq_ignore_ascii_case("Monospace")
        || style.font.eq_ignore_ascii_case("Courier")
        || style.font.eq_ignore_ascii_case("Hack")
    {
        egui::FontFamily::Monospace
    } else if style.font.eq_ignore_ascii_case("Default")
        || style.font.eq_ignore_ascii_case("Proportional")
        || style.font.is_empty()
    {
        egui::FontFamily::Proportional
    } else if crate::system_fonts::ensure_font_loaded(painter.ctx(), &style.font) {
        egui::FontFamily::Name(style.font.clone().into())
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

    // Flip channels — `flip_x_factor` and `flip_y_factor` carry both
    // sign (mirror) and magnitude (continuous scale) so the on-canvas
    // text behaves the same way image overlays do when the user drags
    // the Flip X / Flip Y slider through 0 (squash → mirror →
    // un-squash) instead of a binary "either upright or fully
    // mirrored" jump. The same factors are passed to the per-line
    // glyph mesh below so the plate and the text deform together —
    // this is what the user reported as "у текста отражение должно
    // работать как у изображений".
    //
    // `elem_scale_y` is folded into the Y stretch only (image
    // overlays already scale their height by `scale_y`); the X axis
    // is already covered by `effective_size` having `elem_scale`
    // baked in.
    let stretch_x = flip_x_factor;
    let stretch_y = flip_y_factor * elem_scale_y;
    // Combined screen-space transform for plate corners: scale around
    // `center_pos` first (handles both the flip mirror and the
    // continuous-scale magnitude), then rotate around the same pivot.
    // Glyph text uses the *same* pivot + factor pair inside
    // `paint_text_line_flipped`, so plate and text stay glued
    // together through any transform.
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let xform_pt = |p: Pos2| -> Pos2 {
        let dx = (p.x - center_pos.x) * stretch_x;
        let dy = (p.y - center_pos.y) * stretch_y;
        if !rotated {
            return Pos2::new(center_pos.x + dx, center_pos.y + dy);
        }
        Pos2::new(
            center_pos.x + dx * cos_r - dy * sin_r,
            center_pos.y + dx * sin_r + dy * cos_r,
        )
    };
    // True when *anything* (rotation or anisotropic scale or flip)
    // means we must draw the plate as a polygon rather than as an
    // axis-aligned `rect_filled`. Allow a tiny epsilon either side of
    // 1.0 so floating-point round-off in the default state still
    // takes the cheap rect path.
    let stretch_x_neutral = (stretch_x - 1.0).abs() < 1.0e-3;
    let stretch_y_neutral = (stretch_y - 1.0).abs() < 1.0e-3;
    let needs_polygon_plate = rotated || !stretch_x_neutral || !stretch_y_neutral;

    // Per-line plate rects (used for `TextBoxKind::Wrap`). Each rect
    // hugs its own line's width while sharing the block's vertical
    // padding, so consecutive lines get touching plates with subtly
    // different widths — the look the user requested in
    // "неравномерный фон должен в другом режиме".
    let line_plate_rects: Vec<Rect> = if matches!(style.box_kind, TextBoxKind::Wrap) {
        let mut out = Vec::with_capacity(galleys.len());
        // Start at the text's actual top (plate_rect already includes
        // padding, so the text content begins at min.y + padding).
        let mut y_top = plate_rect.min.y + padding;
        for galley in &galleys {
            let line_w = galley.size().x;
            let line_total_w = line_w + padding * 2.0;
            let half = line_total_w * 0.5;
            // Per-line horizontal anchor follows the text alignment so
            // the plate sits under the line wherever the line itself
            // is drawn (Left → flush left of the block, Right → flush
            // right, Center → centred). Asymmetric L/R extras still
            // bleed onto the line's left/right edge so the user can
            // make all per-line plates wider on one side at once.
            let line_center_x = match style.align {
                TextAlign::Left => plate_rect.min.x + padding + line_w * 0.5,
                TextAlign::Right => plate_rect.max.x - padding - line_w * 0.5,
                TextAlign::Center => center_pos.x,
            };
            let lp = Rect::from_min_max(
                Pos2::new(line_center_x - half - pad_extra_l, y_top),
                Pos2::new(line_center_x + half + pad_extra_r, y_top + line_h),
            );
            out.push(lp);
            y_top += line_h;
        }
        // Apply the block's top/bottom padding symmetrically to the
        // first and last plate so the block as a whole looks padded
        // equally on both sides.
        if !out.is_empty() {
            let first_idx = 0;
            let last_idx = out.len() - 1;
            out[first_idx].min.y -= padding;
            out[last_idx].max.y += padding;
        }
        out
    } else {
        Vec::new()
    };

    // ─── Plate background ────────────────────────────────────────
    if let Some(box_color) = style.box_color {
        let primary = Color32::from_rgba_unmultiplied(
            box_color[0], box_color[1], box_color[2],
            (plate_opacity * 255.0) as u8,
        );

        // Helper closure: paint one rect's plate fill+border, applying
        // the rotation+flip transform when needed. Used for both the
        // single-block plate and (in Wrap mode) each per-line plate.
        let paint_one_plate = |rect: Rect, kind: TextBoxKind| {
            if needs_polygon_plate {
                if !matches!(kind, TextBoxKind::None | TextBoxKind::OutlineOnly) {
                    let pts = vec![
                        xform_pt(rect.left_top()),
                        xform_pt(rect.right_top()),
                        xform_pt(rect.right_bottom()),
                        xform_pt(rect.left_bottom()),
                    ];
                    painter.add(egui::Shape::convex_polygon(pts, primary, Stroke::NONE));
                }
                if style.box_outline_width > 0.0
                    || matches!(kind, TextBoxKind::OutlineOnly)
                {
                    let border_color_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
                    let border_color = Color32::from_rgba_unmultiplied(
                        border_color_rgb[0],
                        border_color_rgb[1],
                        border_color_rgb[2],
                        (plate_opacity * 255.0) as u8,
                    );
                    let bw = if style.box_outline_width > 0.0 {
                        style.box_outline_width * zoom
                    } else {
                        2.0
                    };
                    let pts = vec![
                        xform_pt(rect.left_top()),
                        xform_pt(rect.right_top()),
                        xform_pt(rect.right_bottom()),
                        xform_pt(rect.left_bottom()),
                        xform_pt(rect.left_top()),
                    ];
                    for w in pts.windows(2) {
                        painter.line_segment([w[0], w[1]], Stroke::new(bw, border_color));
                    }
                }
            } else {
                match kind {
                    TextBoxKind::None => {}
                    TextBoxKind::Solid | TextBoxKind::Wrap | TextBoxKind::FitText => {
                        painter.rect_filled(rect, Rounding::same(radius), primary);
                    }
                    TextBoxKind::Gradient => {
                        let end_color_rgb = style.box_gradient_end.unwrap_or(box_color);
                        let end = Color32::from_rgba_unmultiplied(
                            end_color_rgb[0],
                            end_color_rgb[1],
                            end_color_rgb[2],
                            (plate_opacity * 255.0) as u8,
                        );
                        draw_vertical_gradient(painter, rect, radius, primary, end);
                    }
                    TextBoxKind::OutlineOnly => {}
                }
                if style.box_outline_width > 0.0 {
                    let border_color_rgb = style.box_outline_color.unwrap_or([0, 0, 0]);
                    let border_color = Color32::from_rgba_unmultiplied(
                        border_color_rgb[0],
                        border_color_rgb[1],
                        border_color_rgb[2],
                        (plate_opacity * 255.0) as u8,
                    );
                    painter.rect_stroke(
                        rect,
                        Rounding::same(radius),
                        Stroke::new(style.box_outline_width * zoom, border_color),
                    );
                } else if matches!(kind, TextBoxKind::OutlineOnly) {
                    painter.rect_stroke(rect, Rounding::same(radius), Stroke::new(2.0, primary));
                }
            }
        };

        if matches!(style.box_kind, TextBoxKind::Wrap) {
            // Per-line plates. Each behaves like a `Solid` plate at the
            // line's width.
            for r in &line_plate_rects {
                paint_one_plate(*r, TextBoxKind::Wrap);
            }
        } else if matches!(style.box_kind, TextBoxKind::FitText) {
            // Tight halo around the text glyphs only — no padding /
            // extras. We synthesise a plate rect that hugs the text
            // block exactly and paint it as a regular `Solid`-like
            // plate. Using `Solid` here (rather than passing
            // `FitText` through) keeps the polygon-vs-rect dispatch
            // inside `paint_one_plate` straightforward; the *kind*
            // field only controls rendering mode, not geometry.
            let half_text = max_line_w * 0.5;
            let text_min_y = plate_rect.center().y - total_h * 0.5;
            let text_max_y = plate_rect.center().y + total_h * 0.5;
            let text_plate = Rect::from_min_max(
                Pos2::new(center_pos.x - half_text, text_min_y),
                Pos2::new(center_pos.x + half_text, text_max_y),
            );
            paint_one_plate(text_plate, TextBoxKind::Solid);
        } else {
            paint_one_plate(plate_rect, style.box_kind);
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

    // Flip channels — same continuous factors used for the plate
    // above. The per-line glyph mesh in `paint_text_line_flipped`
    // applies them around `center_pos`, mirroring the plate's
    // `xform_pt` so the text and its background stay glued through
    // any flip / scale / rotation animation. The previous build
    // used `flip_x_anim < 0.0` here as a boolean toggle, which is
    // the "two visual states" bug the user reported.
    let line_flip_x = stretch_x;
    let line_flip_y = stretch_y;

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
                        stroke_color, rotation_rad, center_pos, line_flip_x, line_flip_y,
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
                rotation_rad, center_pos, line_flip_x, line_flip_y,
            );
        }
        y += line_h;
    }

    // ─── Selection border ─────────────────────────────────────────
    let is_selected = state.selection == Selection::Overlay(idx);
    if is_selected {
        if needs_polygon_plate {
            let pts = vec![
                xform_pt(plate_rect.left_top()),
                xform_pt(plate_rect.right_top()),
                xform_pt(plate_rect.right_bottom()),
                xform_pt(plate_rect.left_bottom()),
                xform_pt(plate_rect.left_top()),
            ];
            for w in pts.windows(2) {
                painter.line_segment([w[0], w[1]], Stroke::new(2.0, COL_SELECTED_BORDER));
            }
        } else {
            painter.rect_stroke(plate_rect.expand(2.0), Rounding::same(radius + 2.0),
                Stroke::new(2.0, COL_SELECTED_BORDER));
        }
    } else if display_mode == DisplayMode::Active && !needs_polygon_plate {
        painter.rect_stroke(plate_rect, Rounding::same(radius),
            Stroke::new(0.5, Color32::from_rgba_unmultiplied(120, 200, 140, 60)));
    }

    // Display mode badge (kept axis-aligned for legibility)
    if display_mode != DisplayMode::Active {
        let badge = match display_mode {
            DisplayMode::BeforeStart => crate::i18n::t("FIRST"),
            DisplayMode::AfterEnd => crate::i18n::t("LAST"),
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
/// `rotation_rad` and scaled around the same pivot by `flip_x_factor`
/// / `flip_y_factor`.
///
/// The factors carry both **sign** (mirror, when negative) and
/// **magnitude** (continuous scale around the pivot). 1.0 means no
/// transform along that axis; -1.0 is a pure mirror; values between
/// 0 and ±1 squash the glyphs as the slider crosses zero — this
/// matches what image overlays do with `flip_x_anim` / `flip_y_anim`
/// and replaces the previous boolean "either upright or fully
/// mirrored" semantics that the user reported as broken for text.
///
/// Egui's `TextShape` has no per-axis flip / scale field, so to
/// actually deform the glyph shapes (not just the layout direction)
/// we build a custom `Mesh` from the galley's per-row tessellated
/// mesh, then transform each vertex's screen position around `pivot`.
/// UVs are kept the same — the position transform combined with
/// unchanged texture sampling is what gives a visually mirrored /
/// stretched glyph.
///
/// When the requested transform is identity (rotation ≈ 0, factors ≈
/// 1) we fall back to the cheap `painter.text` path so the typical
/// case stays cheap.
#[allow(clippy::too_many_arguments)]
fn paint_text_line_flipped(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font_id: egui::FontId,
    color: Color32,
    rotation_rad: f32,
    pivot: Pos2,
    flip_x_factor: f32,
    flip_y_factor: f32,
) {
    let identity_x = (flip_x_factor - 1.0).abs() < 1.0e-3;
    let identity_y = (flip_y_factor - 1.0).abs() < 1.0e-3;
    if identity_x && identity_y {
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

    // Transform path: build a custom Mesh where each glyph quad is
    // scaled (possibly with a mirror) around `pivot` along each axis.
    // Both plate and glyphs use the same pivot + factor pair, so the
    // background and the text deform together.
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

        let idx_offset = mesh.vertices.len() as u32;
        for &i in &row.visuals.mesh.indices {
            mesh.indices.push(i + idx_offset);
        }
        for vtx in &row.visuals.mesh.vertices {
            // Translate to absolute position then scale around pivot.
            let abs_x = pos.x + vtx.pos.x;
            let abs_y = pos.y + vtx.pos.y;
            let dx = (abs_x - pivot.x) * flip_x_factor;
            let dy = (abs_y - pivot.y) * flip_y_factor;
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
/// Resolved parent transform: position, rotation, and scale accumulated
/// from the parent chain. Used to transform child elements relative to
/// their parent.
#[derive(Clone, Copy, Debug)]
pub struct ParentTransform {
    /// Parent's world position (centre).
    pub pos: WorldPos,
    /// Parent's accumulated rotation in degrees.
    pub rotation_deg: f32,
    /// Parent's accumulated X scale.
    pub scale_x: f32,
    /// Parent's accumulated Y scale.
    pub scale_y: f32,
}

impl Default for ParentTransform {
    fn default() -> Self {
        Self {
            pos: WorldPos { x: 0.0, y: 0.0 },
            rotation_deg: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }
}

fn safe_div(num: f32, den: f32) -> f32 {
    if den.abs() > 1.0e-6 { num / den } else { 1.0 }
}

/// Resolve the full parent transform chain for an element identified by
/// `parent_id`. Walks up the parent hierarchy (with cycle detection) and
/// accumulates position, rotation, and scale. Returns `None` when the
/// element has no parent or the parent can't be resolved.
pub fn resolve_parent_transform(
    state: &EditorState,
    parent_id: &str,
    t: f32,
    visited: &mut Vec<String>,
) -> Option<ParentTransform> {
    // Cycle detection: if we've already visited this id, bail out.
    if visited.contains(&parent_id.to_string()) {
        return None;
    }
    visited.push(parent_id.to_string());

    // Special case: render frame as parent
    if parent_id == "__render_frame__" {
        let rf = &state.scene.render_frame;
        let rf_state = keyframe::sample(&rf.layout, t).unwrap_or_default();
        let mod_delta = keyframe::evaluate_modifiers(&rf.modifiers, t);
        return Some(ParentTransform {
            pos: WorldPos {
                x: rf_state.pos.x + mod_delta.dx,
                y: rf_state.pos.y + mod_delta.dy,
            },
            rotation_deg: rf_state.rotation_deg + mod_delta.d_rotation_deg,
            // Render frame zoom is inverse scale (zoom > 1 = zoomed in = smaller world area)
            // For parenting purposes, we treat it as scale = 1 (the rf doesn't scale children)
            scale_x: 1.0,
            scale_y: 1.0,
        });
    }

    // Try to find the parent as an actor
    if let Some(actor) = state.scene.actors.iter().find(|a| a.id == parent_id) {
        let actor_t_in = actor.t_in.unwrap_or(0.0);
        let actor_state = keyframe::sample(&actor.layout, t).unwrap_or_default();
        let mod_delta = keyframe::evaluate_modifiers(&actor.modifiers, (t - actor_t_in).max(0.0));
        let canvas_transform = state.scene.canvas_layouts.iter()
            .find(|cl| cl.element_id == actor.id)
            .and_then(|cl| keyframe::sample(&cl.keyframes, t));

        let rf = &state.scene.render_frame;
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32;
        let world_h = rh as f32;

        let mut pos = if let Some(transform) = canvas_transform {
            WorldPos { x: transform.pos.x + mod_delta.dx, y: transform.pos.y + mod_delta.dy }
        } else {
            WorldPos {
                x: actor_state.pos[0] * world_w + mod_delta.dx,
                y: actor_state.pos[1] * world_h + mod_delta.dy,
            }
        };
        let mut rotation = canvas_transform.map(|transform| transform.rotation_deg).unwrap_or(actor_state.rotation_deg) + mod_delta.d_rotation_deg;
        let mut scale_x = (canvas_transform.map(|transform| transform.scale).unwrap_or(actor_state.scale) + mod_delta.d_scale).max(0.001);
        let mut scale_y = scale_x * actor_state.scale_y;

        // Recursively resolve this element's own parent
        if let Some(ref grandparent_id) = actor.parent_id {
            if let Some(gp) = resolve_parent_transform(state, grandparent_id, t, visited) {
                // Apply grandparent transform to this parent's local transform.
                pos = apply_parent_transform(pos, &gp);
                rotation += gp.rotation_deg;
                scale_x *= gp.scale_x;
                scale_y *= gp.scale_y;
            }
        }

        return Some(ParentTransform { pos, rotation_deg: rotation, scale_x, scale_y });
    }

    // Try to find the parent as an overlay
    if let Some(ov) = state.scene.overlays.iter().find(|ov| {
        match ov {
            Overlay::Text(o) => o.id == parent_id,
            Overlay::Image(o) => o.id == parent_id,
            Overlay::Video(o) => o.id == parent_id,
        }
    }) {
        let (t_in, layout, parent_pid, modifiers): (f32, &[Keyframe<OverlayState>], Option<&String>, &[keyframe::TrackModifier]) = match ov {
            Overlay::Text(o) => (o.t_in, &o.layout, o.parent_id.as_ref(), &o.modifiers),
            Overlay::Image(o) => (o.t_in, &o.layout, o.parent_id.as_ref(), &o.modifiers),
            Overlay::Video(o) => (o.t_in, &o.layout, o.parent_id.as_ref(), &o.modifiers),
        };
        let local_t = (t - t_in).max(0.0);
        let ov_state = keyframe::sample(layout, local_t).unwrap_or_default();
        let mod_delta = keyframe::evaluate_modifiers(modifiers, local_t);

        let rf = &state.scene.render_frame;
        let [rw, rh] = rf.resolution;
        let world_w = rw as f32;
        let world_h = rh as f32;

        let overlay_id = match ov {
            Overlay::Text(o) => &o.id,
            Overlay::Image(o) => &o.id,
            Overlay::Video(o) => &o.id,
        };
        let canvas_transform = state.scene.canvas_layouts.iter()
            .find(|cl| cl.element_id == *overlay_id)
            .and_then(|cl| keyframe::sample(&cl.keyframes, t));

        let mut pos = if let Some(transform) = canvas_transform {
            WorldPos { x: transform.pos.x + mod_delta.dx, y: transform.pos.y + mod_delta.dy }
        } else {
            WorldPos {
                x: ov_state.pos[0] * world_w + mod_delta.dx,
                y: ov_state.pos[1] * world_h + mod_delta.dy,
            }
        };
        let mut rotation = canvas_transform.map(|transform| transform.rotation_deg).unwrap_or(ov_state.rotation_deg) + mod_delta.d_rotation_deg;
        let mut scale_x = (canvas_transform.map(|transform| transform.scale).unwrap_or(ov_state.scale) + mod_delta.d_scale).max(0.001);
        let mut scale_y = scale_x * ov_state.scale_y;

        // Recursively resolve this overlay's own parent
        if let Some(grandparent_id) = parent_pid {
            if let Some(gp) = resolve_parent_transform(state, grandparent_id, t, visited) {
                pos = apply_parent_transform(pos, &gp);
                rotation += gp.rotation_deg;
                scale_x *= gp.scale_x;
                scale_y *= gp.scale_y;
            }
        }

        return Some(ParentTransform { pos, rotation_deg: rotation, scale_x, scale_y });
    }

    None
}

/// Apply a parent transform to a child's local world position. The child's
/// position is treated as an offset from the parent's centre, rotated by
/// the parent's rotation and scaled by the parent's scale.
fn apply_parent_transform(child_pos: WorldPos, parent: &ParentTransform) -> WorldPos {
    let rad = parent.rotation_deg.to_radians();
    let cos_r = rad.cos();
    let sin_r = rad.sin();
    let dx = child_pos.x * parent.scale_x;
    let dy = child_pos.y * parent.scale_y;
    WorldPos {
        x: parent.pos.x + dx * cos_r - dy * sin_r,
        y: parent.pos.y + dx * sin_r + dy * cos_r,
    }
}

/// Inverse of [`apply_parent_transform`]: map a world centre back to the
/// stored pre-parent layout value used in `canvas_layouts` / legacy tracks.
fn inverse_parent_transform(world: WorldPos, parent: &ParentTransform) -> WorldPos {
    let rad = parent.rotation_deg.to_radians();
    let cos_r = rad.cos();
    let sin_r = rad.sin();
    let dx = world.x - parent.pos.x;
    let dy = world.y - parent.pos.y;
    WorldPos {
        x: safe_div(dx * cos_r + dy * sin_r, parent.scale_x),
        y: safe_div(-dx * sin_r + dy * cos_r, parent.scale_y),
    }
}

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

    if state.canvas_drag.preview_element_id.as_deref() == Some(element_id) {
        if let Some(c) = state.canvas_drag.preview_world_center {
            // Preview stores the element's world centre while dragging;
            // parent propagation is applied only when sampling stored data.
            return WorldPos { x: c[0], y: c[1] };
        }
    }

    let sample_t = if state.canvas_drag.preview_element_id.as_deref() == Some(element_id) {
        state.canvas_drag.drag_start_playhead.unwrap_or(t)
    } else {
        t
    };

    // Check canvas_layouts for this element (scene-time keyframes).
    let base_pos = if let Some(cl) = state.scene.canvas_layouts.iter().find(|cl| cl.element_id == element_id) {
        keyframe::sample(&cl.keyframes, sample_t)
            .map(|transform| transform.pos)
    } else {
        None
    };

    // Fallback: convert legacy normalised coords to world pixels.
    //
    // ── Render-frame is decoupled from the world ──
    //
    // `pos` is interpreted against a FIXED reference rectangle of
    // size `render_frame.resolution` anchored at world (0, 0). It
    // does NOT depend on the live `rf.pos` / `rf.zoom` / `rf.rotation`
    // — moving / scaling / rotating the rf no longer drags this
    // element on the canvas. The rf is a pure camera viewport: its
    // state determines what region of the world gets captured into
    // the output, never where world-space elements sit.
    let rf = &state.scene.render_frame;
    let [rw, rh] = rf.resolution;
    let world_w = rw as f32;
    let world_h = rh as f32;

    let base_pos = base_pos.unwrap_or_else(|| {
        if let Some(actor_state) = keyframe::sample(legacy_layout, sample_t) {
            WorldPos {
                x: actor_state.pos[0] * world_w,
                y: actor_state.pos[1] * world_h,
            }
        } else if let Some(ov_state) = overlay_state_at_scene_time(state, element_id, sample_t) {
            WorldPos {
                x: ov_state.pos[0] * world_w,
                y: ov_state.pos[1] * world_h,
            }
        } else {
            WorldPos {
                x: world_w * 0.5,
                y: world_h * 0.5,
            }
        }
    });

    // ── Parent transform propagation ──
    // If this element has a parent, apply the parent's accumulated
    // transform (position, rotation, scale) to the child's local pos.
    let parent_id = state.scene.actors.iter()
        .find(|a| a.id == element_id)
        .and_then(|a| a.parent_id.clone())
        .or_else(|| {
            state.scene.overlays.iter().find_map(|ov| match ov {
                Overlay::Text(o) if o.id == element_id => o.parent_id.clone(),
                Overlay::Image(o) if o.id == element_id => o.parent_id.clone(),
                Overlay::Video(o) if o.id == element_id => o.parent_id.clone(),
                _ => None,
            })
        });

    if let Some(pid) = parent_id {
        let mut visited = vec![element_id.to_string()];
        if let Some(parent_xform) = resolve_parent_transform(state, &pid, sample_t, &mut visited) {
            return apply_parent_transform(base_pos, &parent_xform);
        }
    }

    base_pos
}


/// Sample an overlay's layout at scene time `t` (clip-local keyframes).
fn overlay_state_at_scene_time(
    state: &EditorState,
    element_id: &str,
    scene_t: f32,
) -> Option<OverlayState> {
    state.scene.overlays.iter().find_map(|ov| {
        let (t_in, t_out, layout) = match ov {
            Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
            Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
            Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
        };
        let id = match ov {
            Overlay::Text(txt) => &txt.id,
            Overlay::Image(img) => &img.id,
            Overlay::Video(vid) => &vid.id,
        };
        if id != element_id {
            return None;
        }
        let local_t = if scene_t >= t_in && scene_t <= t_out {
            scene_t - t_in
        } else if scene_t < t_in {
            0.0
        } else {
            t_out - t_in
        };
        Some(keyframe::sample(layout, local_t).unwrap_or_default())
    })
}

/// While an animated param is dragged on canvas, layout sampling is frozen
/// but rotation / scale can still be previewed from `canvas_drag`.
fn apply_canvas_transform_preview(
    state: &EditorState,
    element_id: &str,
    rotation_deg: &mut f32,
    scale: &mut f32,
    scale_y: &mut f32,
) {
    if state.canvas_drag.preview_element_id.as_deref() != Some(element_id) {
        return;
    }
    if let Some(r) = state.canvas_drag.preview_rotation_deg {
        *rotation_deg = r;
    }
    if let Some(s) = state.canvas_drag.preview_scale {
        *scale = s;
    }
    if let Some(sy) = state.canvas_drag.preview_scale_y {
        *scale_y = sy;
    }
}

fn selection_element_id(state: &EditorState, sel: Selection) -> Option<String> {
    match sel {
        Selection::Actor(i) => state.scene.actors.get(i).map(|a| a.id.clone()),
        Selection::Overlay(i) => state.scene.overlays.get(i).and_then(|ov| match ov {
            Overlay::Text(t) => Some(t.id.clone()),
            Overlay::Image(im) => Some(im.id.clone()),
            Overlay::Video(v) => Some(v.id.clone()),
        }),
        _ => None,
    }
}

fn selection_position_animated(state: &EditorState, sel: Selection) -> bool {
    match sel {
        Selection::Actor(i) => state.scene.actors.get(i).map(|a| {
            a.animated_params.contains(memstroy_core::param_ids::POS_X)
                || a.animated_params.contains(memstroy_core::param_ids::POS_Y)
        }).unwrap_or(false),
        Selection::Overlay(i) => state.scene.overlays.get(i).map(|ov| {
            let ap = match ov {
                Overlay::Text(t) => &t.animated_params,
                Overlay::Image(im) => &im.animated_params,
                Overlay::Video(v) => &v.animated_params,
            };
            ap.contains(memstroy_core::param_ids::POS_X)
                || ap.contains(memstroy_core::param_ids::POS_Y)
        }).unwrap_or(false),
        _ => false,
    }
}

fn selection_rotation_animated(state: &EditorState, sel: Selection) -> bool {
    match sel {
        Selection::Actor(i) => state
            .scene
            .actors
            .get(i)
            .map(|a| a.animated_params.contains(memstroy_core::param_ids::ROTATION))
            .unwrap_or(false),
        Selection::Overlay(i) => state.scene.overlays.get(i).map(|ov| {
            let ap = match ov {
                Overlay::Text(t) => &t.animated_params,
                Overlay::Image(im) => &im.animated_params,
                Overlay::Video(v) => &v.animated_params,
            };
            ap.contains(memstroy_core::param_ids::ROTATION)
        }).unwrap_or(false),
        _ => false,
    }
}

fn selection_scale_animated(state: &EditorState, sel: Selection) -> bool {
    match sel {
        Selection::Actor(i) => state.scene.actors.get(i).map(|a| {
            a.animated_params.contains(memstroy_core::param_ids::SCALE)
                || a.animated_params.contains(memstroy_core::param_ids::SCALE_Y)
        }).unwrap_or(false),
        Selection::Overlay(i) => state.scene.overlays.get(i).map(|ov| {
            let ap = match ov {
                Overlay::Text(t) => &t.animated_params,
                Overlay::Image(im) => &im.animated_params,
                Overlay::Video(v) => &v.animated_params,
            };
            ap.contains(memstroy_core::param_ids::SCALE)
                || ap.contains(memstroy_core::param_ids::SCALE_Y)
        }).unwrap_or(false),
        _ => false,
    }
}

fn clear_canvas_drag_preview(state: &mut EditorState) {
    state.canvas_drag.preview_element_id = None;
    state.canvas_drag.preview_world_center = None;
    state.canvas_drag.preview_rotation_deg = None;
    state.canvas_drag.preview_scale = None;
    state.canvas_drag.preview_scale_y = None;
}

fn commit_canvas_drag_preview(state: &mut EditorState) {
    let sel = state.selection;
    if sel == Selection::None {
        clear_canvas_drag_preview(state);
        return;
    }
    if state.canvas_drag.preview_world_center.is_none()
        && state.canvas_drag.preview_rotation_deg.is_none()
        && state.canvas_drag.preview_scale.is_none()
        && state.canvas_drag.preview_scale_y.is_none()
    {
        return;
    }
    state.canvas_drag.drag_start_playhead = None;
    if let Some(center) = state.canvas_drag.preview_world_center.take() {
        let token = canvas_drag_token(CANVAS_TOKEN_POS, sel);
        write_selection_world_center(state, sel, center, token, false);
    }
    if let Some(rot) = state.canvas_drag.preview_rotation_deg.take() {
        let token = canvas_drag_token(CANVAS_TOKEN_ROTATION, sel);
        write_selection_rotation(state, sel, rot, token, false);
    }
    if let Some(scale) = state.canvas_drag.preview_scale.take() {
        let token = canvas_drag_token(CANVAS_TOKEN_SCALE, sel);
        write_selection_scale(state, sel, scale, token, false);
    }
    if let Some(scale_y) = state.canvas_drag.preview_scale_y.take() {
        let token = canvas_drag_token(CANVAS_TOKEN_SCALE_Y, sel);
        write_selection_scale_y(state, sel, scale_y, token, false);
    }
    state.canvas_drag.preview_element_id = None;
}


// ─── SELECTION & DRAG STATE MACHINE ──────────────────────────────────
//
// All canvas interaction is routed through this single handler. The active
// drag mode is captured *once* at `drag_started()` (so the origin stays
// stable) and applied incrementally on every frame the pointer is held.

const ELEM_HANDLE_SIZE: f32 = 7.0;
const RF_HANDLE_SIZE: f32 = 8.0;
const RF_CENTER_RADIUS: f32 = 8.0;

/// Screen-space gizmo for the selected actor / overlay (rotation-aware).
struct ElementGizmo {
    center: Pos2,
    half_w: f32,
    half_h: f32,
    rotation_deg: f32,
}

impl ElementGizmo {
    fn local_to_screen(&self, lx: f32, ly: f32) -> Pos2 {
        let theta = self.rotation_deg.to_radians();
        let (s, c) = (theta.sin(), theta.cos());
        let rx = lx * c - ly * s;
        let ry = lx * s + ly * c;
        Pos2::new(self.center.x + rx, self.center.y + ry)
    }

    fn screen_to_local(&self, p: Pos2) -> (f32, f32) {
        let theta = (-self.rotation_deg).to_radians();
        let (s, c) = (theta.sin(), theta.cos());
        let dx = p.x - self.center.x;
        let dy = p.y - self.center.y;
        (dx * c - dy * s, dx * s + dy * c)
    }

    fn contains_screen(&self, p: Pos2) -> bool {
        let (lx, ly) = self.screen_to_local(p);
        lx.abs() <= self.half_w && ly.abs() <= self.half_h
    }
}

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
    if response.drag_started() && response.hovered() {
        if let Some(start_resp) = response.interact_pointer_pos() {
            // ── Use the EXACT press location, not the drift-after-egui-
            // ── decided-it's-a-drag location.
            //
            // egui only flips `drag_started()` once the pointer has
            // moved past its drag-threshold (~6 px) from the press
            // origin. By that point `response.interact_pointer_pos()`
            // returns the *current* pointer position, which can be
            // several pixels away from where the user actually
            // clicked. For a press right at the edge of a resize
            // handle (where the visible hover cursor already showed
            // "resize"), the post-drift position can fall outside our
            // hit-test radius — so the drag mode resolved to a
            // marquee instead of a handle resize. The user reported
            // this as: "hovering shows the resize cursor but pressing
            // starts a multi-select; only pressing exactly on the
            // edge works".
            //
            // `i.pointer.press_origin()` carries the press location
            // we want; we still fall back to the post-drift position
            // when the pointer state is somehow unavailable.
            let start = ui
                .input(|i| i.pointer.press_origin())
                .unwrap_or(start_resp);
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

            // ── Render-frame drags are self-contained ──
            //
            // The render frame became a fully independent element in
            // scene v2 (see `Scene::migrate_decouple_render_frame`):
            // its position / zoom / rotation no longer feed into any
            // child's world-pixel layout. So a drag on the rf no
            // longer needs the elaborate "snapshot every child's
            // world pos so we can re-derive their normalised pos
            // against the new frame state" dance — we just write the
            // new rf state and let the canvas redraw.
            use crate::state::CanvasDragMode;
            match state.canvas_drag.mode {
                CanvasDragMode::MoveRenderFrame { .. }
                | CanvasDragMode::ResizeRenderFrame { .. } => {
                    state.selection = Selection::RenderFrame;
                    state.canvas_drag.actor_legacy_snapshot.clear();
                    state.canvas_drag.overlay_world_snapshot.clear();
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
        if response.drag_stopped() {
            commit_canvas_drag_preview(state);
        }
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
            clear_canvas_drag_preview(state);
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
            // ── Render-frame center handle has top priority ──
            //
            // Clicking the central marker of the render frame should
            // always select the render frame itself, regardless of
            // any actor / overlay that happens to be drawn underneath
            // the same point. The handle is painted on top of every
            // other layer (`draw_render_frame` runs last), so the
            // selection hit-test follows the same z-order — without
            // this priority the click fell through to whichever
            // actor / overlay covered the centre, and the user
            // reported "нажатие на центральную точку рендера должно
            // выделять элемент рендера".
            let rf_state = sample_render_frame(&state.scene.render_frame, state.playhead);
            let center_screen = state
                .canvas_viewport
                .world_to_screen(rf_state.pos, viewport_size);
            let rf_center = Pos2::new(
                full_rect.min.x + center_screen[0],
                full_rect.min.y + center_screen[1],
            );
            let rf_center_hit = (mouse - rf_center).length() < RF_CENTER_RADIUS * 2.5;
            if rf_center_hit {
                state.selection = Selection::RenderFrame;
            } else {
                try_select_at(state, click_world);
            }
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
    if let Some(gizmo) = selected_element_gizmo(state, full_rect, viewport_size) {
        let handle_pos = rotation_handle_screen_pos_gizmo(&gizmo);
        if (start - handle_pos).length() < ROTATION_HANDLE_RADIUS * 3.0 {
            let center = gizmo.center;
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
    if let Some(gizmo) = selected_element_gizmo(state, full_rect, viewport_size) {
        let zoom = state.canvas_viewport.zoom.max(0.0001);
        let screen_to_world_xy = |p: Pos2| -> [f32; 2] {
            let w = state.canvas_viewport.screen_to_world(
                [p.x - full_rect.min.x, p.y - full_rect.min.y],
                viewport_size,
            );
            [w.x, w.y]
        };
        let hw = gizmo.half_w;
        let hh = gizmo.half_h;
        let tl = screen_to_world_xy(gizmo.local_to_screen(-hw, -hh));
        let tr = screen_to_world_xy(gizmo.local_to_screen(hw, -hh));
        let br = screen_to_world_xy(gizmo.local_to_screen(hw, hh));
        let bl = screen_to_world_xy(gizmo.local_to_screen(-hw, hh));
        let world_cx = screen_to_world_xy(gizmo.center)[0];
        let world_cy = screen_to_world_xy(gizmo.center)[1];

        let handle_specs: [(Pos2, u8, [f32; 2]); 8] = [
            (gizmo.local_to_screen(-hw, -hh), 0, br),
            (gizmo.local_to_screen(hw, -hh), 1, bl),
            (gizmo.local_to_screen(hw, hh), 2, tl),
            (gizmo.local_to_screen(-hw, hh), 3, tr),
            (gizmo.local_to_screen(0.0, -hh), 4, screen_to_world_xy(gizmo.local_to_screen(0.0, hh))),
            (gizmo.local_to_screen(hw, 0.0), 5, screen_to_world_xy(gizmo.local_to_screen(-hw, 0.0))),
            (gizmo.local_to_screen(0.0, hh), 6, screen_to_world_xy(gizmo.local_to_screen(0.0, -hh))),
            (gizmo.local_to_screen(-hw, 0.0), 7, screen_to_world_xy(gizmo.local_to_screen(hw, 0.0))),
        ];
        let _ = (zoom, world_cx, world_cy, tl, tr);

        for (handle_pos, handle_id, anchor_world) in handle_specs.iter() {
            // Hit radius is intentionally a touch larger than the
            // visible handle so users don't have to land exactly on
            // the dot — and crucially, larger than `update_hover_cursor`'s
            // 2.0× radius so anywhere the cursor *advertised* a
            // resize affordance also commits as a resize on press
            // (3.0× = 21 px > the 14 px hover radius + egui's ~6 px
            // drag-start drift). This is what closes the "hover
            // shows resize but press triggers multi-select" gap.
            if (start - *handle_pos).length() < ELEM_HANDLE_SIZE * 3.0 {
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
        if gizmo.contains_screen(start) {
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
        // Same 3.0× hit-radius rationale as the regular element
        // resize handles above — keep it strictly larger than the
        // hover advertise threshold (`update_hover_cursor` uses
        // 2.0×) so a press anywhere the cursor showed "resize"
        // commits as ResizeRenderFrame.
        if (start - *corner).length() < RF_HANDLE_SIZE * 3.0 {
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
                // World-fixed dims — `pos` is interpreted against the
                // resolution rectangle anchored at world (0, 0). The
                // render frame is a pure CAMERA: editing rf.pos /
                // rf.zoom never drags world-space elements (per
                // `draw_canvas_overlays` / `get_element_world_pos`),
                // so the drag math must match. Without this, dragging
                // an element after moving the rf produced a snap-back
                // jump because the inverse formula assumed a different
                // anchor than the renderer.
                let rf = &state.scene.render_frame;
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32;
                let world_h = rh as f32;

                let proposed_norm_x = initial_pos[0] + world_dx / world_w;
                let proposed_norm_y = initial_pos[1] + world_dy / world_h;
                let world_x = proposed_norm_x * world_w;
                let world_y = proposed_norm_y * world_h;
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
                let prim_initial_world_x = initial_pos[0] * world_w;
                let prim_initial_world_y = initial_pos[1] * world_h;
                let total_dx = snapped_world_x - prim_initial_world_x;
                let total_dy = snapped_world_y - prim_initial_world_y;
                broadcast_multi_translation(state, total_dx, total_dy);
            }
        }

        CanvasDragMode::MoveOverlay { overlay_idx, initial_pos } => {
            if overlay_idx < state.scene.overlays.len() {
                // World-fixed dims — see `MoveActorLegacy` above.
                let rf = &state.scene.render_frame;
                let [rw, rh] = rf.resolution;
                let world_w = rw as f32;
                let world_h = rh as f32;
                let dx_norm = world_dx / world_w;
                let dy_norm = world_dy / world_h;
                let proposed_norm_x = initial_pos[0] + dx_norm;
                let proposed_norm_y = initial_pos[1] + dy_norm;
                let world_x = proposed_norm_x * world_w;
                let world_y = proposed_norm_y * world_h;
                let (snapped_world_x, snapped_world_y, guides) = snap_world_center(
                    state,
                    world_x,
                    world_y,
                    Some(SnapExclude::Overlay(overlay_idx)),
                );
                state.canvas_drag.snap_guides = guides;
                set_selection_world_center(state, [snapped_world_x, snapped_world_y]);
                // Broadcast snapped world delta to other selected items.
                let prim_initial_world_x = initial_pos[0] * world_w;
                let prim_initial_world_y = initial_pos[1] * world_h;
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

            // ── Edge / centre snap during resize ──
            //
            // The user explicitly asked for "помощь в точечном
            // позиционировании" while resizing — i.e. the same kind
            // of magnetic alignment the move arms get when an
            // element's centre approaches the render-frame's centre
            // or edges. We feed the dragged corner / edge through
            // the same `snap_world_center` helper used by the move
            // arms; it already covers RF centre, RF axis-aligned
            // edges, RF rotated edges (via `snap_to_render_frame_rotated_edges`),
            // and every other actor / overlay centre. We only honour
            // the snap on the axes the active handle actually
            // changes — for a horizontal-edge resize it would feel
            // wrong to silently shift the element vertically, so we
            // also drop any guide pointing along an axis that isn't
            // moving.
            let (snapped_cur_x, snapped_cur_y, snap_guides) = if state.snap_enabled {
                let exclude = match state.selection {
                    Selection::Actor(i) => Some(SnapExclude::Actor(i)),
                    Selection::Overlay(i) => Some(SnapExclude::Overlay(i)),
                    _ => None,
                };
                let (sx, sy, all_guides) =
                    snap_world_center(state, cur_world_x, cur_world_y, exclude);
                let final_x = if changes_x { sx } else { cur_world_x };
                let final_y = if changes_y { sy } else { cur_world_y };
                let filtered: Vec<crate::state::SnapGuide> = all_guides
                    .into_iter()
                    .filter(|g| match g.axis {
                        crate::state::SnapAxis::Vertical => changes_x,
                        crate::state::SnapAxis::Horizontal => changes_y,
                        // Free-orientation guides (rotated RF edges)
                        // are 2-D — only relevant when both axes can
                        // move, i.e. corner handles.
                        crate::state::SnapAxis::Line => changes_x && changes_y,
                    })
                    .collect();
                (final_x, final_y, filtered)
            } else {
                (cur_world_x, cur_world_y, Vec::new())
            };
            state.canvas_drag.snap_guides = snap_guides;

            let init_w = base_w * initial_scale;
            let init_h = base_h * initial_scale * initial_scale_y;

            let new_w = if changes_x {
                ((snapped_cur_x - anchor_world[0]).abs()).max(base_w * 0.05)
            } else { init_w };
            let new_h = if changes_y {
                ((snapped_cur_y - anchor_world[1]).abs()).max(base_h * 0.05)
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
                let signed = if snapped_cur_x >= anchor_world[0] { 1.0 } else { -1.0 };
                anchor_world[0] + signed * final_w * 0.5
            } else { initial_pos_world[0] };
            let cy = if changes_y {
                let signed = if snapped_cur_y >= anchor_world[1] { 1.0 } else { -1.0 };
                anchor_world[1] + signed * final_h * 0.5
            } else { initial_pos_world[1] };

            // Derive scale, scale_y from final dims.
            let new_scale = (final_w / base_w.max(1e-3)).clamp(0.05, 100.0);
            let new_scale_y_total = (final_h / base_h.max(1e-3)).clamp(0.05, 100.0);
            let new_scale_y = (new_scale_y_total / new_scale.max(1e-3)).clamp(0.05, 100.0);

            set_selection_scale(state, new_scale);
            set_selection_scale_y(state, new_scale_y);
            set_selection_world_center(state, [cx, cy]);

            // Broadcast the same scale ratio to every other lassoed
            // element. We use ratios (not absolute values) so each
            // element scales relative to its own drag-start size,
            // anchored at its OWN centre.
            //
            // The previous version also broadcast the primary's
            // centre delta as a translation — but during a corner /
            // edge resize the primary's centre moves only because
            // the opposite corner is anchored, NOT because the user
            // is moving the group. Re-applying that displacement to
            // every other selected element produced the "parallel
            // motion on the canvas while resizing" the user
            // reported. Resize is a pure scale broadcast; movement
            // is the dedicated MoveSelection / MoveActor / MoveOverlay
            // modes' job.
            let scale_factor = new_scale / initial_scale.max(1e-3);
            let scale_y_factor = new_scale_y / initial_scale_y.max(1e-3);
            broadcast_multi_scale(state, scale_factor, scale_y_factor);
        }

        CanvasDragMode::MoveRenderFrame { initial_pos } => {
            // Insert a keyframe at the drag-start playhead and write the
            // new position there — same canvas-first semantics as actors
            // and overlays. Re-using the cached drag-start playhead
            // means the entire drag produces a single kf rather than
            // one per frame while playback is running.
            //
            // Routed through `kf_anim::write_render_frame_param` so the
            // write is GATED on `render_frame.animated_params`. When
            // POS_X / POS_Y aren't toggled animated, the new value is
            // broadcast to every existing kf (static) instead of
            // silently spawning a mid-track keyframe — that's what
            // produced "parameter-less" diamonds during playback drags
            // before this fix.
            let t = state.canvas_drag.drag_start_playhead.unwrap_or(state.playhead);
            let new_x = initial_pos[0] + world_dx;
            let new_y = initial_pos[1] + world_dy;
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::POS_X,
                false,
                |v| v.pos.x = new_x,
            );
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::POS_Y,
                false,
                |v| v.pos.y = new_y,
            );
            // ── No child-compensation needed in v2 ──
            //
            // Element world positions are decoupled from the render
            // frame (see `migrate_decouple_render_frame` in
            // memstroy-core/scene.rs and the formula in
            // `get_element_world_pos`): they're a fixed multiple of
            // `render_frame.resolution` and do not depend on
            // `rf.pos` / `rf.zoom`. So moving the render frame on
            // canvas no longer drags any child elements with it —
            // the rf is a self-contained camera viewport now and the
            // explicit "snapshot world pos / re-derive norm" pass
            // that v1 needed is gone with it.
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
                // Same gating as MoveRenderFrame above — the resize
                // becomes static when SCALE isn't in
                // `animated_params`, otherwise upserts a kf at the
                // drag-start playhead.
                crate::kf_anim::write_render_frame_param(
                    &mut state.scene.render_frame.layout,
                    &mut state.scene.render_frame.animated_params,
                    t,
                    memstroy_core::param_ids::SCALE,
                    false,
                    |v| v.zoom = new_zoom,
                );
                // Same as MoveRenderFrame above: no child compensation
                // is needed in v2 because element world positions are
                // independent of `rf.zoom`.
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
            Some((1.0 / rf_state.zoom.max(1e-4)).clamp(0.05, 100.0))
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

fn rotation_handle_screen_pos_gizmo(g: &ElementGizmo) -> Pos2 {
    g.local_to_screen(0.0, -g.half_h - ROTATION_HANDLE_OFFSET)
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
            let ov_id = match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.id.as_str(),
                Overlay::Image(im) => im.id.as_str(),
                Overlay::Video(v) => v.id.as_str(),
            };
            let wp = get_element_world_pos(state, ov_id, &[], t);
            Some([wp.x, wp.y])
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
    let defer = state.canvas_drag.drag_start_playhead.is_some();
    write_selection_world_center(state, sel, center, token, defer);
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
                (1.0 / rf_state.zoom.max(1e-4)).clamp(0.05, 100.0),
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
        write_selection_world_center(state, entry.selection, new_center, token, true);
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
        let new_scale = (entry.initial_scale * scale_factor).clamp(0.05, 100.0);
        let new_scale_y = (entry.initial_scale_y * scale_y_factor).clamp(0.05, 100.0);
        write_selection_scale(state, entry.selection, new_scale, token_x, true);
        write_selection_scale_y(state, entry.selection, new_scale_y, token_y, true);
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
        write_selection_rotation(state, entry.selection, new_rot, token, true);
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
    defer_animated: bool,
) {
    if defer_animated
        && state.canvas_drag.drag_start_playhead.is_some()
        && selection_position_animated(state, sel)
    {
        state.canvas_drag.preview_element_id = selection_element_id(state, sel);
        state.canvas_drag.preview_world_center = Some(center);
        return;
    }
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
            let parent_id = state.scene.actors[idx].parent_id.clone();
            let mut stored_center = WorldPos { x: center[0], y: center[1] };
            if let Some(pid) = parent_id {
                let mut visited = vec![actor_id.clone()];
                if let Some(pxf) = resolve_parent_transform(state, &pid, t, &mut visited) {
                    stored_center = inverse_parent_transform(stored_center, &pxf);
                }
            }
            if let Some(cl) = state.scene.canvas_layouts.iter_mut()
                .find(|cl| cl.element_id == actor_id)
            {
                let sx = stored_center.x;
                let sy = stored_center.y;
                crate::kf_anim::write_canvas_param(
                    &mut cl.keyframes,
                    &animated_clone,
                    &[
                        memstroy_core::param_ids::POS_X,
                        memstroy_core::param_ids::POS_Y,
                    ],
                    t,
                    |v| {
                        v.pos.x = sx;
                        v.pos.y = sy;
                    },
                );
                return;
            }
            // Legacy normalised: convert world centre to a normalised
            // pos against the FIXED reference rectangle (size =
            // `render_frame.resolution`). Decoupled from the live rf
            // — moving / resizing / rotating the rf no longer
            // perturbs this actor's authored position.
            let [rw, rh] = state.scene.render_frame.resolution;
            let world_w = rw as f32;
            let world_h = rh as f32;
            if world_w <= 0.0 || world_h <= 0.0 { return; }
            let new_norm = [stored_center.x / world_w, stored_center.y / world_h];
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
            let ov_id = match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.id.clone(),
                Overlay::Image(im) => im.id.clone(),
                Overlay::Video(v) => v.id.clone(),
            };
            let parent_id = match &state.scene.overlays[idx] {
                Overlay::Text(t) => t.parent_id.clone(),
                Overlay::Image(im) => im.parent_id.clone(),
                Overlay::Video(v) => v.parent_id.clone(),
            };
            let mut stored_center = WorldPos { x: center[0], y: center[1] };
            if let Some(pid) = parent_id {
                let mut visited = vec![ov_id];
                if let Some(pxf) = resolve_parent_transform(state, &pid, t, &mut visited) {
                    stored_center = inverse_parent_transform(stored_center, &pxf);
                }
            }
            // Same decoupled-from-rf world→norm conversion as the
            // actor branch above.
            let [rw, rh] = state.scene.render_frame.resolution;
            let world_w = rw as f32;
            let world_h = rh as f32;
            if world_w <= 0.0 || world_h <= 0.0 { return; }
            let new_norm = [stored_center.x / world_w, stored_center.y / world_h];
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
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::POS_X,
                false,
                |v| v.pos.x = center[0],
            );
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::POS_Y,
                false,
                |v| v.pos.y = center[1],
            );
        }
        _ => {}
    }
}



pub fn set_element_parent_preserve_world(
    state: &mut EditorState,
    element_id: &str,
    new_parent_id: Option<String>,
) -> bool {
    let t = state.playhead;
    let sel = if let Some(idx) = state.scene.actors.iter().position(|a| a.id == element_id) {
        Selection::Actor(idx)
    } else if let Some(idx) = state.scene.overlays.iter().position(|ov| match ov {
        Overlay::Text(o) => o.id == element_id,
        Overlay::Image(o) => o.id == element_id,
        Overlay::Video(o) => o.id == element_id,
    }) {
        Selection::Overlay(idx)
    } else {
        return false;
    };
    let current_world = match sel {
        Selection::Actor(idx) => get_element_world_pos(state, element_id, &state.scene.actors[idx].layout, t),
        Selection::Overlay(idx) => {
            let dummy: &[Keyframe<ActorState>] = &[];
            let sample_t = overlay_clip_local_time_at(state, idx, t);
            get_element_world_pos(state, element_id, dummy, sample_t)
        }
        _ => return false,
    };
    let token = canvas_drag_token(CANVAS_TOKEN_POS, sel);
    if state.last_drag_group != Some(token) {
        state.undo.push(&state.scene);
        state.last_drag_group = Some(token);
    }
    if !state.scene.set_element_parent_id(element_id, new_parent_id) {
        return false;
    }
    write_selection_world_center(state, sel, [current_world.x, current_world.y], token, false);
    true
}

// `mark_actor_canvas_animated` / `mark_overlay_canvas_animated` were
// removed (and replaced by the gating inside `kf_anim::write_*_param`).
// Canvas drags no longer auto-mark a parameter as animated — the user
// explicitly toggles that with the diamond next to the inspector field.

fn set_selection_scale_y(state: &mut EditorState, new_scale_y: f32) {
    let token = canvas_drag_token(CANVAS_TOKEN_SCALE_Y, state.selection);
    let sel = state.selection;
    let defer = state.canvas_drag.drag_start_playhead.is_some();
    write_selection_scale_y(state, sel, new_scale_y, token, defer);
}

fn write_selection_scale_y(
    state: &mut EditorState,
    sel: Selection,
    new_scale_y: f32,
    token: u64,
    defer_animated: bool,
) {
    let s = new_scale_y.clamp(0.05, 100.0);
    if defer_animated
        && state.canvas_drag.drag_start_playhead.is_some()
        && selection_scale_animated(state, sel)
    {
        state.canvas_drag.preview_element_id = selection_element_id(state, sel);
        state.canvas_drag.preview_scale_y = Some(s);
        return;
    }
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
    let defer = state.canvas_drag.drag_start_playhead.is_some();
    write_selection_scale(state, sel, new_scale, token, defer);
}

fn write_selection_scale(
    state: &mut EditorState,
    sel: Selection,
    new_scale: f32,
    token: u64,
    defer_animated: bool,
) {
    let s = new_scale.clamp(0.05, 100.0);
    if defer_animated
        && state.canvas_drag.drag_start_playhead.is_some()
        && selection_scale_animated(state, sel)
    {
        state.canvas_drag.preview_element_id = selection_element_id(state, sel);
        state.canvas_drag.preview_scale = Some(s);
        return;
    }
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
            let new_zoom = (1.0 / s.max(1e-4)).clamp(0.001, 1000.0);
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::SCALE,
                false,
                |v| v.zoom = new_zoom,
            );
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
    let defer = state.canvas_drag.drag_start_playhead.is_some();
    write_selection_rotation(state, sel, new_rot_deg, token, defer);
}

fn write_selection_rotation(
    state: &mut EditorState,
    sel: Selection,
    new_rot_deg: f32,
    token: u64,
    defer_animated: bool,
) {
    if defer_animated
        && state.canvas_drag.drag_start_playhead.is_some()
        && selection_rotation_animated(state, sel)
    {
        state.canvas_drag.preview_element_id = selection_element_id(state, sel);
        state.canvas_drag.preview_rotation_deg = Some(new_rot_deg);
        return;
    }
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
            crate::kf_anim::write_render_frame_param(
                &mut state.scene.render_frame.layout,
                &mut state.scene.render_frame.animated_params,
                t,
                memstroy_core::param_ids::ROTATION,
                false,
                |v| v.rotation_deg = new_rot_deg,
            );
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
///
/// **Unused** since the render-frame canvas drag handlers were routed
/// through the gated [`crate::kf_anim::write_render_frame_param`] —
/// kept here as a breadcrumb so a future contributor doesn't
/// reintroduce parameter-less keyframe authoring (the bug we
/// removed in this refactor).
#[allow(dead_code)]
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
///
/// **Unused** — see [`ensure_render_frame_kf_at_playhead`] for context.
#[allow(dead_code)]
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

// ─── RENDER-FRAME CHILD COMPENSATION (REMOVED IN SCENE V2) ───────────
//
// Scene v1 stored every overlay / legacy actor's `pos` as a normalised
// `[0..1]` vector relative to the live render frame. Dragging or
// resizing the rf therefore shifted every child along with it, so the
// editor needed an explicit "snapshot world pos at drag start →
// re-derive normalised pos against the new frame state" compensation
// pass to keep children visually pinned.
//
// In scene v2 (`Scene::migrate_decouple_render_frame`) the same `pos`
// channel is interpreted against a FIXED reference rectangle of size
// `render_frame.resolution` anchored at world (0, 0). Element world
// positions no longer depend on `rf.pos` / `rf.zoom` / `rf.rotation`,
// so child compensation is a no-op by construction — and the helpers
// that drove it (`snapshot_overlay_world_positions`,
// `snapshot_legacy_actor_world_positions`,
// `compensate_children_after_render_frame_change`) have been removed.

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

/// Axis-aligned hit test in an oriented rectangle (world space).
fn world_point_in_oriented_rect(
    pos: WorldPos,
    center: WorldPos,
    half_w: f32,
    half_h: f32,
    rotation_deg: f32,
) -> bool {
    let rad = (-rotation_deg).to_radians();
    let cos_r = rad.cos();
    let sin_r = rad.sin();
    let dx = pos.x - center.x;
    let dy = pos.y - center.y;
    let lx = dx * cos_r - dy * sin_r;
    let ly = dx * sin_r + dy * cos_r;
    lx.abs() <= half_w && ly.abs() <= half_h
}

/// Lightweight hit sniffer (read-only) — returns what would be selected at a
/// given world position without mutating the editor state.
fn sniff_hit(state: &EditorState, pos: WorldPos) -> Option<Selection> {
    let t = state.playhead;
    let rf = &state.scene.render_frame;
    let [rw, rh] = rf.resolution;
    // ── World-fixed dims for overlay / actor hit-tests ──
    //
    // Mirrors `draw_canvas_overlays` and `get_element_world_pos`:
    // the legacy normalised `pos` is interpreted against a FIXED
    // reference rectangle of size `rf.resolution`, anchored at world
    // (0, 0). Without this fix, moving the render frame shifted
    // every collider away from its visible element — the user's
    // "позиция рендера... коллайдеры в неверных местах" report.
    //
    // (sniff_hit doesn't test backgrounds, so the camera-relative
    // dims aren't needed here.)
    let _world_w = rw as f32;
    let _world_h = rh as f32;

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
        let ov_id = match overlay {
            Overlay::Text(o) => o.id.as_str(),
            Overlay::Image(o) => o.id.as_str(),
            Overlay::Video(o) => o.id.as_str(),
        };
        let ov_world = get_element_world_pos(state, ov_id, &[], t);
        let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
        let mut rotation_deg = ov_state.rotation_deg;
        let mut ov_scale = ov_state.scale;
        let mut ov_scale_y = ov_state.scale_y;
        apply_canvas_transform_preview(
            state,
            ov_id,
            &mut rotation_deg,
            &mut ov_scale,
            &mut ov_scale_y,
        );
        let mut parent_scale_x = 1.0_f32;
        let mut parent_scale_y = 1.0_f32;
        let parent_id = match overlay {
            Overlay::Text(o) => o.parent_id.as_ref(),
            Overlay::Image(o) => o.parent_id.as_ref(),
            Overlay::Video(o) => o.parent_id.as_ref(),
        };
        if let Some(pid) = parent_id {
            let mut visited = vec![ov_id.to_string()];
            if let Some(pxf) = resolve_parent_transform(state, pid, t, &mut visited) {
                rotation_deg += pxf.rotation_deg;
                parent_scale_x *= pxf.scale_x;
                parent_scale_y *= pxf.scale_y;
            }
        }
        let half_w = ew * parent_scale_x * 0.5;
        let half_h = eh * parent_scale_y * 0.5;
        if world_point_in_oriented_rect(pos, ov_world, half_w, half_h, rotation_deg) {
            return Some(Selection::Overlay(idx));
        }
    }

    for (idx, actor) in state.scene.actors.iter().enumerate().rev() {
        if !actor.visible { continue; }
        let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
        let actor_st = keyframe::sample(&actor.layout, t).unwrap_or_default();
        let mut actor_scale = actor_st.scale;
        let mut actor_scale_y = actor_st.scale_y;
        let mut rotation_deg = actor_st.rotation_deg;
        if let Some(ref pid) = actor.parent_id {
            let mut visited = vec![actor.id.clone()];
            if let Some(pxf) = resolve_parent_transform(state, pid, t, &mut visited) {
                rotation_deg += pxf.rotation_deg;
                actor_scale *= pxf.scale_x;
                actor_scale_y *= safe_div(pxf.scale_y, pxf.scale_x);
            }
        }
        let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
            if fc.is_ready() && fc.frame_count > 0 {
                (fc.source_width as f32, fc.source_height as f32)
            } else { (1080.0, 1920.0) }
        } else { (1080.0, 1920.0) };
        let half_w = base_w * actor_scale * 0.5;
        let half_h = base_h * actor_scale * actor_scale_y * 0.5;
        if world_point_in_oriented_rect(pos, world_pos, half_w, half_h, rotation_deg) {
            return Some(Selection::Actor(idx));
        }
    }
    None
}

/// Rotation-aware screen gizmo for the current selection.
fn selected_element_gizmo(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> Option<ElementGizmo> {
    let t = state.playhead;
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let actor = &state.scene.actors[idx];
            if !actor.visible {
                return None;
            }
            let sample_t = state
                .canvas_drag
                .drag_start_playhead
                .filter(|_| state.canvas_drag.preview_element_id.as_deref() == Some(actor.id.as_str()))
                .unwrap_or(t);
            let world_pos = get_element_world_pos(state, &actor.id, &actor.layout, t);
            let actor_st = keyframe::sample(&actor.layout, sample_t).unwrap_or_default();
            let mut actor_scale = actor_st.scale;
            let mut actor_scale_y = actor_st.scale_y;
            let mut rotation_deg = actor_st.rotation_deg;
            apply_canvas_transform_preview(
                state,
                &actor.id,
                &mut rotation_deg,
                &mut actor_scale,
                &mut actor_scale_y,
            );
            if let Some(ref pid) = actor.parent_id {
                let mut visited = vec![actor.id.clone()];
                if let Some(pxf) = resolve_parent_transform(state, pid, sample_t, &mut visited) {
                    rotation_deg += pxf.rotation_deg;
                    actor_scale *= pxf.scale_x;
                actor_scale_y *= safe_div(pxf.scale_y, pxf.scale_x);
                }
            }
            let (base_w, base_h) = if let Some(fc) = state.frame_caches.get(idx) {
                if fc.is_ready() && fc.frame_count > 0 {
                    (fc.source_width as f32, fc.source_height as f32)
                } else {
                    (1080.0, 1920.0)
                }
            } else {
                (1080.0, 1920.0)
            };
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let zoom = state.canvas_viewport.zoom;
            Some(ElementGizmo {
                center: Pos2::new(
                    full_rect.min.x + center_screen[0],
                    full_rect.min.y + center_screen[1],
                ),
                half_w: base_w * actor_scale * 0.5 * zoom,
                half_h: base_h * actor_scale * actor_scale_y * 0.5 * zoom,
                rotation_deg,
            })
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let overlay = &state.scene.overlays[idx];
            let (ov_id, t_in, t_out, layout, parent_id) = match overlay {
                Overlay::Text(txt) => (
                    txt.id.as_str(),
                    txt.t_in,
                    txt.t_out,
                    &txt.layout,
                    txt.parent_id.as_ref(),
                ),
                Overlay::Image(img) => (
                    img.id.as_str(),
                    img.t_in,
                    img.t_out,
                    &img.layout,
                    img.parent_id.as_ref(),
                ),
                Overlay::Video(vid) => (
                    vid.id.as_str(),
                    vid.t_in,
                    vid.t_out,
                    &vid.layout,
                    vid.parent_id.as_ref(),
                ),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in } else { 0.0 };
            let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
            let world_pos = get_element_world_pos(state, ov_id, &[], t);
            let (elem_w, elem_h) = overlay_bbox_with_state(overlay, &ov_state, state);
            let mut rotation_deg = ov_state.rotation_deg;
            let mut ov_scale = ov_state.scale;
            let mut ov_scale_y = ov_state.scale_y;
            apply_canvas_transform_preview(
                state,
                ov_id,
                &mut rotation_deg,
                &mut ov_scale,
                &mut ov_scale_y,
            );
            let mut parent_scale_x = 1.0_f32;
            let mut parent_scale_y = 1.0_f32;
            if let Some(pid) = parent_id {
                let mut visited = vec![ov_id.to_string()];
                if let Some(pxf) = resolve_parent_transform(state, pid, t, &mut visited) {
                    rotation_deg += pxf.rotation_deg;
                    parent_scale_x *= pxf.scale_x;
                    parent_scale_y *= pxf.scale_y;
                }
            }
            let center_screen = state.canvas_viewport.world_to_screen(world_pos, viewport_size);
            let zoom = state.canvas_viewport.zoom;
            Some(ElementGizmo {
                center: Pos2::new(
                    full_rect.min.x + center_screen[0],
                    full_rect.min.y + center_screen[1],
                ),
                half_w: elem_w * parent_scale_x * 0.5 * zoom,
                half_h: elem_h * parent_scale_y * 0.5 * zoom,
                rotation_deg,
            })
        }
        _ => None,
    }
}

/// Axis-aligned bounding rect of the selected element (for legacy callers).
fn selected_element_screen_rect(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> Option<Rect> {
    let g = selected_element_gizmo(state, full_rect, viewport_size)?;
    let corners = [
        g.local_to_screen(-g.half_w, -g.half_h),
        g.local_to_screen(g.half_w, -g.half_h),
        g.local_to_screen(g.half_w, g.half_h),
        g.local_to_screen(-g.half_w, g.half_h),
    ];
    let mut rect = Rect::NOTHING;
    for c in corners {
        rect = rect.union(Rect::from_center_size(c, Vec2::ZERO));
    }
    Some(rect)
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
    if let Some(gizmo) = selected_element_gizmo(state, full_rect, viewport_size) {
        let rot_pos = rotation_handle_screen_pos_gizmo(&gizmo);
        if (hover - rot_pos).length() < ROTATION_HANDLE_RADIUS * 2.0 {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            return;
        }
        let hw = gizmo.half_w;
        let hh = gizmo.half_h;
        let handle_specs: [(Pos2, egui::CursorIcon); 8] = [
            (gizmo.local_to_screen(-hw, -hh), egui::CursorIcon::ResizeNwSe),
            (gizmo.local_to_screen(hw, -hh), egui::CursorIcon::ResizeNeSw),
            (gizmo.local_to_screen(hw, hh), egui::CursorIcon::ResizeNwSe),
            (gizmo.local_to_screen(-hw, hh), egui::CursorIcon::ResizeNeSw),
            (gizmo.local_to_screen(0.0, -hh), egui::CursorIcon::ResizeVertical),
            (gizmo.local_to_screen(hw, 0.0), egui::CursorIcon::ResizeHorizontal),
            (gizmo.local_to_screen(0.0, hh), egui::CursorIcon::ResizeVertical),
            (gizmo.local_to_screen(-hw, 0.0), egui::CursorIcon::ResizeHorizontal),
        ];
        for (handle_pos, cursor) in handle_specs {
            if (hover - handle_pos).length() < ELEM_HANDLE_SIZE * 2.0 {
                ui.ctx().set_cursor_icon(cursor);
                return;
            }
        }
        if gizmo.contains_screen(hover) {
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
    // Legacy normalised → world-pixel.
    //
    // The render frame is now a pure CAMERA: its pos / zoom never
    // shifts world-space elements (per `get_element_world_pos` and
    // `draw_canvas_overlays`). The trajectory drawing must follow
    // the same convention so the dots line up with the actor's
    // visible centre on the canvas. Without this fix, moving the
    // render frame dragged the trajectory away from the actor.
    let _ = rf_state;
    let world_w = rf_resolution[0] as f32;
    let world_h = rf_resolution[1] as f32;
    WorldPos {
        x: actor_state.pos[0] * world_w,
        y: actor_state.pos[1] * world_h,
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
            // World-fixed dims — `pos` is interpreted against the
            // resolution rectangle anchored at world (0, 0). Mirrors
            // the renderer in `draw_canvas_overlays`. Without this
            // the trajectory drifts off the visible overlay path
            // whenever the user repositions the render frame.
            let world_w = rf_resolution[0] as f32;
            let world_h = rf_resolution[1] as f32;
            layout.iter().map(|kf| {
                let world = WorldPos {
                    x: kf.value.pos[0] * world_w,
                    y: kf.value.pos[1] * world_h,
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
    // Selected element handles (rotation-aware OBB)
    if let Some(gizmo) = selected_element_gizmo(state, full_rect, viewport_size) {
        let handle_color = COL_SELECTED_BORDER;
        let corners = [
            gizmo.local_to_screen(-gizmo.half_w, -gizmo.half_h),
            gizmo.local_to_screen(gizmo.half_w, -gizmo.half_h),
            gizmo.local_to_screen(gizmo.half_w, gizmo.half_h),
            gizmo.local_to_screen(-gizmo.half_w, gizmo.half_h),
        ];
        for corner in &corners {
            let hr = Rect::from_center_size(*corner, Vec2::splat(ELEM_HANDLE_SIZE));
            painter.rect_filled(hr, Rounding::same(2.0), handle_color);
            painter.rect_stroke(hr, Rounding::same(2.0), Stroke::new(1.0, Color32::from_rgb(40, 40, 40)));
        }
        let midpoints = [
            gizmo.local_to_screen(0.0, -gizmo.half_h),
            gizmo.local_to_screen(0.0, gizmo.half_h),
            gizmo.local_to_screen(-gizmo.half_w, 0.0),
            gizmo.local_to_screen(gizmo.half_w, 0.0),
        ];
        for mp in &midpoints {
            let hr = Rect::from_center_size(*mp, Vec2::new(ELEM_HANDLE_SIZE * 1.4, ELEM_HANDLE_SIZE * 0.7));
            painter.rect_filled(hr, Rounding::same(2.0), handle_color);
        }
        for i in 0..4 {
            painter.line_segment(
                [corners[i], corners[(i + 1) % 4]],
                Stroke::new(1.5, handle_color),
            );
        }

        let rot_pos = rotation_handle_screen_pos_gizmo(&gizmo);
        let top_mid = gizmo.local_to_screen(0.0, -gizmo.half_h);
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
            // World-fixed dims — see the matching block above.
            let rf = &state.scene.render_frame;
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32;
            let world_h = rh as f32;
            let world_pos = WorldPos {
                x: ov_state.pos[0] * world_w,
                y: ov_state.pos[1] * world_h,
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
                                kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 100.0);
                            }
                        }
                        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
                            match &mut state.scene.overlays[idx] {
                                Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 100.0); } }
                                Overlay::Image(i) => { if let Some(kf) = i.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 100.0); } }
                                Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.scale = (kf.value.scale * scale_factor).clamp(0.05, 100.0); } }
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
            // Glyphs are now scaled by `ov_state.scale` (and `scale_y`
            // along the vertical axis) so the resize handles snap to
            // the visibly-stretched plate edges. The previous build
            // pinned glyph width to `font_size * 0.55` regardless of
            // scale, which is what made on-canvas text resize feel
            // sticky — handles drifted with `scale` while glyphs
            // stayed the same size, so the bbox the gizmos used and
            // the visible plate rapidly diverged.
            let text_w = (max_chars.max(1.0)) * font * 0.55 * sx;
            let text_h = (lines.len() as f32) * font * 1.2 * sy;
            // Plate padding scales uniformly with `ov_state.scale`
            // (matches `draw_text_overlay`).
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
        // FX zone (empty source): use a fixed default bbox so resize
        // handles and hit-testing work.
        if img.source.as_os_str().is_empty() {
            let sx = ov_state.scale;
            let sy = ov_state.scale * ov_state.scale_y;
            return (600.0 * sx, 400.0 * sy);
        }
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
    // Empty path = FX zone with no source. Skip the load entirely.
    if path.as_os_str().is_empty() {
        return None;
    }
    // Cool-down between repeat decode attempts on a previously-failed
    // path. 500 ms is short enough that the user perceives the file
    // appearing "as soon as it's written", but long enough that we
    // don't burn CPU when the file genuinely won't decode.
    const FAILED_RETRY_COOLDOWN: std::time::Duration =
        std::time::Duration::from_millis(500);

    // Fast path: already in the map.
    let mut should_retry = false;
    if let Ok(map) = state.image_textures.lock() {
        if let Some(slot) = map.get(path) {
            match slot {
                ImageTextureSlot::Loaded { size, .. } => return Some((size[0], size[1])),
                ImageTextureSlot::Failed { last_attempt } => {
                    if last_attempt.elapsed() < FAILED_RETRY_COOLDOWN {
                        return None;
                    }
                    should_retry = true;
                    // Fall through to decode again below.
                }
            }
        }
    }

    // Slow path: decode now. We do a synchronous decode because typical
    // sticker PNGs are small and the result is cached after the first
    // hit; bumping this to a background thread is a follow-up if very
    // large images become common.
    //
    // Path resolution: when the scene saves a RELATIVE path
    // (e.g. `assets/images/foo.png`) we have to anchor it against
    // `state.assets_root` before opening, otherwise `image::open`
    // resolves it relative to the process CWD and silently fails on
    // any project loaded from a path different from the cwd. The
    // failure cached the slot as `Failed`, so the bbox stayed at
    // the 200×200 placeholder forever — the user's "при сохранении
    // и открытии слетают коллайдеры у изображений" report.
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        state.assets_root.join(path)
    };
    let decoded = image::open(&resolved).map(|img| img.to_rgba8());
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
                if should_retry {
                    // Nudge the next paint so the freshly-decoded image
                    // shows up without waiting for an unrelated input
                    // event.
                    ctx.request_repaint();
                }
                return Some((sz[0], sz[1]));
            }
            _ => {
                map.insert(
                    path.to_path_buf(),
                    ImageTextureSlot::Failed {
                        last_attempt: std::time::Instant::now(),
                    },
                );
                // Schedule a repaint after the cool-down so the canvas
                // re-checks the file (e.g. once an in-flight download
                // finishes writing it). Without this, a failed-then-
                // succeeded sequence only retries when the user moves
                // the mouse over the canvas.
                ctx.request_repaint_after(FAILED_RETRY_COOLDOWN);
            }
        }
    }
    None
}

/// Look up the effect-baked image texture for `(path, effects)` from
/// the L2 cache. Non-blocking: when the entry is missing or stale,
/// dispatches a background bake job and returns `None` so the caller
/// falls back to drawing the unprocessed image until the worker
/// finishes. The previous synchronous decode + effect pipeline ran on
/// the UI thread and stalled paint for tens to hundreds of ms on a
/// typical 4K overlay; the worker (`crate::image_fx_worker`) now
/// handles the heavy lifting and posts the result back via
/// `JobEvent::ImageFxReady`.
///
/// Returns `(TextureHandle, crop_inset)` only when the cache holds a
/// `Ready` slot. `Pending` and `Failed` both return `None` — the draw
/// path then uses the unprocessed `image_textures` slot, so the
/// picture stays visible while the bake is in flight (or after a
/// failed bake).
fn ensure_image_fx_loaded(
    state: &EditorState,
    path: &std::path::Path,
    effects: &[memstroy_core::effects::Effect],
    ctx: &egui::Context,
) -> Option<(egui::TextureHandle, [f32; 4])> {
    use crate::image_fx_cache::LookupOutcome;

    // Empty path = FX zone, no source to bake.
    if path.as_os_str().is_empty() {
        return None;
    }

    let sig = crate::image_effects::signature(effects);

    match state.image_fx_cache.lookup(path, sig) {
        LookupOutcome::Ready(slot) => {
            // While any bake is still pending, keep the egui frame
            // loop alive so the eventual upload paints without a
            // user-input nudge. (Once everything is Ready / Failed
            // we go reactive again.)
            return Some((slot.texture, slot.crop));
        }
        LookupOutcome::Pending => {
            // A worker is already baking this exact (path, sig).
            // Caller will fall back to the unprocessed image.
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            return None;
        }
        LookupOutcome::Failed => {
            // Bake previously failed; don't keep retrying every frame.
            // The unprocessed image is shown as a fallback.
            return None;
        }
        LookupOutcome::Miss => {
            // Fall through and dispatch a fresh bake.
        }
    }

    // Cache miss. Dispatch a background bake — the worker will dedup
    // concurrent submissions for the same (path, sig) so it's safe to
    // call this every frame while the entry stays in `Pending`.
    if let (Some(handle), Some(tx)) =
        (state.tokio_handle.as_ref(), state.image_fx_tx.as_ref())
    {
        crate::image_fx_worker::submit_image_fx_job(
            handle,
            tx,
            &state.image_fx_cache,
            ctx,
            path.to_path_buf(),
            effects.to_vec(),
            sig,
        );
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    } else {
        // Tokio handle / channel not wired up — only happens in
        // tests that construct an EditorState without an App.
        // Treat as "no fx available" so draws fall back cleanly.
    }
    None
}

/// Try to select an element at the given world position.
///
/// Hit-test priority follows the **timeline / layer panel order**: the
/// row that sits highest on the panel (smallest track index = drawn
/// on top) is checked first, so the layer the user visually sees on
/// top is the one a click selects. Actors and overlays are merged
/// into a single ordering so a click on an actor that visually covers
/// an overlay no longer "falls through" to the hidden overlay
/// underneath.
fn try_select_at(state: &mut EditorState, pos: WorldPos) {
    let t = state.playhead;

    // Render-frame anchor for converting overlay normalised coords.
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame(rf, t);
    let [rw, rh] = rf.resolution;
    // ── Camera-relative dims (used by background + render-frame
    //    hit-tests only). The render frame IS a camera, so its
    //    visible rectangle moves / scales with rf.pos / rf.zoom.
    let cam_world_w = rw as f32 / rf_state.zoom.max(1e-6);
    let cam_world_h = rh as f32 / rf_state.zoom.max(1e-6);
    let frame_tl_x = rf_state.pos.x - cam_world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - cam_world_h * 0.5;
    // ── World-fixed dims (used by every overlay / actor hit-test).
    //    Mirrors the renderer in `draw_canvas_overlays` /
    //    `get_element_world_pos`: the legacy normalised `pos` is
    //    interpreted against a FIXED reference rectangle of size
    //    `rf.resolution` anchored at world (0, 0). Without this fix
    //    moving / zooming the render frame shifted every collider
    //    away from its visible element — the user's "коллайдеры
    //    в неверных местах" report.
    let world_w = rw as f32;
    let world_h = rh as f32;

    // ── Build a unified hit-test order across actors + overlays ──
    //
    // Z-rank mirrors the canvas render z-order so that whatever the
    // user sees on top is what gets selected first:
    //
    //   pass 1: overlays classified as "behind actors"
    //           (any actor sits on a higher row than the overlay)
    //   pass 2: actors
    //   pass 3: overlays "on top" of actors
    //
    // Within each pass the layer-panel order wins: the row that sits
    // higher on the panel (smaller `track_index`) gets a higher z and
    // therefore a higher selection priority. Within the same track,
    // the later-added element (larger scene index) wins, matching the
    // draw-in-scene-order tie-breaker used by `draw_canvas_overlays`
    // and `draw_canvas_elements`.
    enum HitCand {
        Actor(usize),
        Overlay(usize),
    }
    // Multiplier large enough to dominate any plausible track / scene
    // index combination so passes never bleed into each other.
    const PASS_STRIDE: i64 = 10_000_000_000;
    const TRACK_STRIDE: i64 = 100_000;
    let mut cands: Vec<(i64, HitCand)> = Vec::new();
    for (idx, _) in state.scene.overlays.iter().enumerate() {
        let track = overlay_track_index(state, idx) as i64;
        let pass: i64 = if overlay_is_behind_actors(state, idx) { 1 } else { 3 };
        // Smaller track => higher panel row => higher z (on top).
        // Larger scene idx => drawn later within track => higher z.
        let within = -track * TRACK_STRIDE + idx as i64;
        cands.push((pass * PASS_STRIDE + within, HitCand::Overlay(idx)));
    }
    for (idx, actor) in state.scene.actors.iter().enumerate() {
        if !actor.visible { continue; }
        let track = actor_track_index(state, idx) as i64;
        // Same within-pass tie-breaker as overlays so two actors that
        // overlap on the canvas are picked by panel row first, then by
        // scene order.
        let within = -track * TRACK_STRIDE + idx as i64;
        cands.push((2 * PASS_STRIDE + within, HitCand::Actor(idx)));
    }
    // Highest z (drawn last / on top) checked first.
    cands.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, cand) in cands {
        match cand {
            HitCand::Overlay(idx) => {
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
                    x: ov_state.pos[0] * world_w,
                    y: ov_state.pos[1] * world_h,
                };
                let (ew, eh) = overlay_bbox_with_state(overlay, &ov_state, state);
                if pos.x >= ov_world.x - ew * 0.5 && pos.x <= ov_world.x + ew * 0.5
                    && pos.y >= ov_world.y - eh * 0.5 && pos.y <= ov_world.y + eh * 0.5
                {
                    state.selection = Selection::Overlay(idx);
                    return;
                }
            }
            HitCand::Actor(idx) => {
                let actor = &state.scene.actors[idx];
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
        }
    }

    // Check backgrounds (click inside render frame area)
    for (idx, bg) in state.scene.backgrounds.iter().enumerate().rev() {
        let bg_end = bg.start + bg.duration;
        if pos.x >= frame_tl_x && pos.x <= frame_tl_x + cam_world_w
            && pos.y >= frame_tl_y && pos.y <= frame_tl_y + cam_world_h
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
    let half_w = cam_world_w * 0.5;
    let half_h = cam_world_h * 0.5;
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
            // World-fixed dims — `pos` is anchored at world (0, 0)
            // so the render frame's pos / zoom never shifts the
            // collider off the visible image.
            let rf = &state.scene.render_frame;
            let [rw, rh] = rf.resolution;
            let world_w = rw as f32;
            let world_h = rh as f32;
            let (t_in, t_out, layout) = match overlay {
                Overlay::Text(txt) => (txt.t_in, txt.t_out, &txt.layout),
                Overlay::Image(img) => (img.t_in, img.t_out, &img.layout),
                Overlay::Video(vid) => (vid.t_in, vid.t_out, &vid.layout),
            };
            let sample_t = if t >= t_in && t <= t_out { t - t_in } else { 0.0 };
            let ov_state = keyframe::sample(layout, sample_t).unwrap_or_default();
            let ov_world = WorldPos {
                x: ov_state.pos[0] * world_w,
                y: ov_state.pos[1] * world_h,
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
    if fit_resp.on_hover_text(crate::i18n::t("Fit render frame in view")).clicked() {
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
    if zin_resp.on_hover_text(crate::i18n::t("Zoom in")).clicked() {
        state.canvas_viewport.zoom = (state.canvas_viewport.zoom * 1.3).min(50.0);
    }

    // Zoom out
    let zout_rect = Rect::from_min_size(
        Pos2::new(zin_rect.max.x + 4.0, fit_rect.min.y),
        btn_size,
    );
    let zout_resp = ui.put(zout_rect, egui::Button::new("-").small());
    if zout_resp.on_hover_text(crate::i18n::t("Zoom out")).clicked() {
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
// Removed: mask tools now live exclusively in the inspector "Masks"
// panel. The floating canvas toolbar was causing confusion (two places
// to arm the same tool) and the inspector provides the full per-mask
// controls (feather, invert, repaint) alongside the arm button.

/// No-op — mask toolbar removed from canvas. Tools are armed from the
/// inspector panel only.
fn draw_mask_toolbar(
    _ui: &mut egui::Ui,
    _full_rect: Rect,
    state: &mut EditorState,
) {
    // Show a minimal status badge when a mask tool is armed (so the
    // user knows the canvas is in mask-draw mode and can press Esc).
    if state.mask_tool != crate::state::MaskTool::None {
        // The badge is drawn by the existing status-bar mechanism in
        // the inspector — no floating canvas chrome needed.
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
/// element's image, **accounting for any rotation/scale applied to the
/// element**. Returns `None` when no element is selected or its
/// bounding box is degenerate.
///
/// The element's visible rectangle on the canvas is the axis-aligned
/// bounding box of its (possibly rotated) image. Painting masks in
/// raw screen UV would skew the resulting shape relative to the
/// picture content — a rectangle drawn over a 45°-rotated sprite
/// would persist in image space as a parallelogram-ish region. To
/// keep the painted mask glued to the picture content we inverse-
/// rotate the click point around the element centre by the same
/// `rotation_deg` the renderer applies, then compute UV in the
/// element's *unrotated* image-local frame. The downstream filters
/// (`apply_mask_alpha`, FFmpeg mask export) consume image-local UVs,
/// so this matches the way the mask is sampled at render time.
fn screen_to_element_uv(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    pointer: Pos2,
) -> Option<([f32; 2], Selection, Rect)> {
    let gizmo = selected_element_gizmo(state, full_rect, viewport_size)?;
    if gizmo.half_w <= 0.5 || gizmo.half_h <= 0.5 {
        return None;
    }
    let (lx, ly) = gizmo.screen_to_local(pointer);
    let u = (lx / (gizmo.half_w * 2.0)) + 0.5;
    let v = (ly / (gizmo.half_h * 2.0)) + 0.5;
    let elem_rect = selected_element_screen_rect(state, full_rect, viewport_size)?;
    Some(([u.clamp(-0.5, 1.5), v.clamp(-0.5, 1.5)], state.selection, elem_rect))
}

/// Sample the rotation (in degrees, CW positive) of the currently-
/// selected actor / overlay at the playhead. Used by both
/// `screen_to_element_uv` (for input) and `draw_mask_draft` (for the
/// preview overlay) so the two stay in lock-step. Returns 0.0 for
/// any non-rotatable selection (RenderFrame, Audio, none).
fn selected_element_rotation_deg(state: &EditorState) -> f32 {
    match state.selection {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            let layout = &state.scene.actors[idx].layout;
            keyframe::sample(layout, state.playhead)
                .map(|s| s.rotation_deg)
                .unwrap_or(0.0)
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            let (t_in, t_out, layout) = match &state.scene.overlays[idx] {
                Overlay::Text(t) => (t.t_in, t.t_out, &t.layout),
                Overlay::Image(im) => (im.t_in, im.t_out, &im.layout),
                Overlay::Video(v) => (v.t_in, v.t_out, &v.layout),
            };
            let local_t = if state.playhead >= t_in && state.playhead <= t_out {
                state.playhead - t_in
            } else {
                0.0
            };
            keyframe::sample(layout, local_t)
                .map(|s| s.rotation_deg)
                .unwrap_or(0.0)
        }
        _ => 0.0,
    }
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

    // Eyedropper is single-click — there is no draft drag to commit.
    // Pre-empt the rest of the input pipeline so we don't accidentally
    // start a DrawMask drag for what's meant to be a one-shot click.
    if state.mask_tool == MaskTool::Eyedropper {
        let pointer_pos = ui.input(|i| i.pointer.hover_pos());
        if response.clicked() || response.drag_started() {
            if let Some(p) = pointer_pos {
                handle_eyedropper_mask_click(ui, state, full_rect, viewport_size, p);
            }
        }
        // Always keep the cursor as a crosshair so the user knows the
        // tool is armed even between clicks.
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        return true;
    }

    // Segment selection mask — multi-click polygon construction.
    // Like the eyedropper it lives outside the drag commit pipeline:
    // each click appends a vertex (or closes the shape) and the
    // commit fires when the user clicks near the first vertex,
    // double-clicks, or presses Enter. Routing this BEFORE the
    // generic drag handlers keeps a vertex-click from being mistaken
    // for a marquee or transform drag.
    if state.mask_tool == MaskTool::SegmentMask {
        handle_segment_mask_input(ui, response, state, full_rect, viewport_size);
        return true;
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

/// Closure-detection radius for the segment mask, expressed in UV
/// (image-local, 0..1) coordinates. ~2.5 % of the element extent
/// translates to roughly 10 px on a 400-px-wide layer — comfortably
/// large enough that the user doesn't have to pixel-hunt the first
/// vertex, but tight enough that a deliberate vertex placed nearby
/// won't accidentally close the polygon. Tuned by feel with a 1080×
/// reference frame; if users complain we can promote it to a setting.
const SEGMENT_MASK_CLOSE_RADIUS_UV: f32 = 0.025;

/// Click-by-click polygon construction for `MaskTool::SegmentMask`.
///
/// The handler keeps polygon state in `EditorState::mask_draft_points`
/// (the in-progress vertex list) and `EditorState::canvas_drag.mode`
/// (the `DrawMask { tool: SegmentMask, .. }` carrier that captures
/// the target element + the very first vertex). The carrier doubles
/// as a sentinel: while it sits in `DrawMask` mode, every subsequent
/// click is interpreted as a polygon vertex, never as a marquee /
/// transform start. The carrier is cleared on commit / cancel so the
/// regular transform pipeline can take over again.
///
/// Closure rules — any of these commits the polygon:
///   * left-click within `SEGMENT_MASK_CLOSE_RADIUS_UV` of the first
///     vertex, with at least three vertices already placed,
///   * double-click anywhere (≥ 3 vertices required),
///   * Enter / Return key (≥ 3 vertices required).
///
/// Right-click pops the most-recently placed vertex; popping the
/// last remaining vertex resets the drag carrier so the next
/// left-click starts a fresh polygon. Esc is wired up in `app.rs`
/// alongside the other mask tools.
///
/// `mask_segment_cursor_uv` is updated every frame to the cursor's
/// element-local UV (or `None` when the cursor leaves the element).
/// `draw_mask_draft` reads it to render the rubber-band line from
/// the last placed vertex to the live cursor and the dashed close
/// preview from the cursor back to the first vertex — without that
/// the user has no way to see where the next segment will land.
fn handle_segment_mask_input(
    ui: &egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) {
    use crate::state::CanvasDragMode;

    // Crosshair cursor + per-frame repaint so the rubber-band tracks
    // smoothly even between input events.
    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    ui.ctx().request_repaint();

    let pointer = ui.input(|i| i.pointer.hover_pos());

    // Maintain the live cursor UV. We resolve it through
    // `screen_to_element_uv` so it inherits the same rotation /
    // bounding-box math the eventual click would. When the cursor is
    // off-element (no UV) we clear the field — the renderer then
    // skips the rubber-band so it doesn't visually "stick" to the
    // last in-bounds position. Computed in a separate `let` so the
    // immutable borrow of `state` ends before we assign back.
    let new_cursor_uv = pointer.and_then(|p| {
        screen_to_element_uv(state, full_rect, viewport_size, p).map(|(uv, _, _)| uv)
    });
    state.mask_segment_cursor_uv = new_cursor_uv;

    // Right-click → pop the last vertex (undo single segment). When
    // the polygon empties out we drop the drag carrier so the next
    // primary click starts a brand-new polygon.
    if response.secondary_clicked() {
        if state.mask_draft_points.pop().is_some() {
            state.status = crate::i18n::t("Segment mask: removed last vertex").into();
        }
        if state.mask_draft_points.is_empty() {
            state.canvas_drag.mode = CanvasDragMode::None;
        }
        return;
    }

    // Helper: commit the in-flight polygon via `commit_mask_draft`,
    // then reset the carrier + draft.
    fn commit_segment(state: &mut EditorState) {
        if let CanvasDragMode::DrawMask { tool, start_uv, target } = state.canvas_drag.mode {
            commit_mask_draft(state, tool, start_uv, target);
        }
        state.canvas_drag.mode = CanvasDragMode::None;
        state.mask_draft_points.clear();
        state.mask_segment_cursor_uv = None;
    }

    // Double-click → commit (≥ 3 vertices). Checked BEFORE the click
    // branch so the second click of the double doesn't first land
    // as an extra vertex.
    if response.double_clicked() {
        if state.mask_draft_points.len() >= 3 {
            commit_segment(state);
        }
        return;
    }

    // Enter / Return → commit (≥ 3 vertices). Lets the user finish
    // the polygon from the keyboard if a clean closure click is
    // awkward (e.g. when the first vertex is occluded by other UI).
    let enter_pressed = ui.input(|i| {
        i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space)
    });
    if enter_pressed && state.mask_draft_points.len() >= 3 {
        commit_segment(state);
        return;
    }

    // Left-click → place a vertex (or close the polygon).
    if response.clicked() {
        let Some(p) = pointer else { return; };
        let Some((uv, target, _rect)) =
            screen_to_element_uv(state, full_rect, viewport_size, p)
        else {
            // No element selected (or click was outside its bounding
            // box). Surface a hint instead of silently swallowing the
            // click — the user otherwise gets no feedback for why
            // their vertex didn't appear.
            state.status =
                crate::i18n::t("Segment mask: select an actor or image overlay first, then click on it.").into();
            return;
        };

        // First click — arm the carrier and seed the vertex list.
        if state.mask_draft_points.is_empty() {
            state.canvas_drag.mode = CanvasDragMode::DrawMask {
                tool: MaskTool::SegmentMask,
                start_uv: uv,
                target,
            };
            state.mask_draft_points.push(uv);
            state.status = crate::i18n::t("Segment mask: vertex 1 placed — keep clicking, double-click or click the first point to close.").into();
            return;
        }

        // Subsequent clicks — check for closure first so the user
        // doesn't have to be perfectly on the first vertex (a small
        // hand wobble would otherwise plant a duplicate vertex on
        // top of the start).
        if state.mask_draft_points.len() >= 3 {
            let first = state.mask_draft_points[0];
            let dx = uv[0] - first[0];
            let dy = uv[1] - first[1];
            if (dx * dx + dy * dy).sqrt() < SEGMENT_MASK_CLOSE_RADIUS_UV {
                commit_segment(state);
                return;
            }
        }

        // Defensive: don't accumulate duplicate vertices when the
        // user clicks the same spot twice (would create zero-length
        // segments that polygon-fill rasterisers cope with poorly).
        if let Some(last) = state.mask_draft_points.last() {
            let dx = uv[0] - last[0];
            let dy = uv[1] - last[1];
            if (dx * dx + dy * dy).sqrt() < 0.001 {
                return;
            }
        }

        state.mask_draft_points.push(uv);
        state.status = format!(
            "{} {}{}",
            crate::i18n::t("Segment mask: vertex"),
            state.mask_draft_points.len(),
            crate::i18n::t(" placed — click first point or double-click to close."),
        );
    }
}

/// Push the painted shape onto the target element's `effects` stack.
/// Builds an `EffectKind::Mask` carrying the matching shape; the
/// eyedropper tool short-circuits before reaching this commit and
/// uses `handle_eyedropper_mask_click` instead. The segment-mask
/// tool also routes through here — its commit shape is identical
/// to a freehand polygon so the FFmpeg / preview sampler already
/// handles it without a new code path.
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
    if (rx - lx).abs() < 0.005
        && (by - ty).abs() < 0.005
        && tool != MaskTool::FreehandMask
        && tool != MaskTool::SegmentMask
    {
        // Treat tiny gestures as a misclick — don't commit anything.
        // Polygon-style tools (freehand and segment) skip this guard
        // because they're inherently multi-point: the start/last UV
        // pair only describes the carrier's first/most-recent click,
        // which may legitimately be close together while the polygon
        // body is large (e.g. lassoing a thin diagonal strip).
        return;
    }

    let new_effect = match tool {
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
        MaskTool::SegmentMask => {
            // Same polygon shape as freehand — the difference is
            // purely how the points were authored. Reuse the same
            // clamp + minimum-vertex check so the resulting mask is
            // guaranteed to fall inside the source UV (matches what
            // the renderer / FFmpeg expects).
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
        MaskTool::Eyedropper => {
            // Eyedropper commits via a separate single-click handler
            // (`handle_eyedropper_mask_click`) that knows how to read
            // the underlying pixel colour for actors and image
            // overlays. The drag pipeline never reaches commit for
            // this tool — the click consumes the input on press, so
            // arriving here means a stray drag finish that we can
            // safely ignore.
            None
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
    state.status = format!("\u{2702} {} {}", tool.label(), crate::i18n::t("applied"));
}

/// Eyedropper colour-key mask — the click handler for
/// `MaskTool::Eyedropper`. Resolves the click into the selected
/// element's UV space, samples the underlying pixel from the frame
/// cache (actor) or the source PNG (image overlay), and either:
///
///   - updates the most-recent `EffectKind::ColorKey` entry on the
///     layer's effect stack with the picked colour, OR
///   - pushes a fresh `EffectKind::ColorKey` if the layer doesn't
///     have one yet.
///
/// The choice between "update existing" and "push new" matters for
/// the user's workflow: clicking a fresh colour while a colour-key
/// mask is already armed should refine the same effect rather than
/// stack a new translucent layer of keys on top. The decision is
/// taken by inspecting the latest existing entry and matching its
/// kind. The picked colour overwrites whatever was on that entry,
/// so the inspector "Re-pick" button (which arms this same tool
/// without pushing a new effect) feels self-explanatory.
fn handle_eyedropper_mask_click(
    ui: &egui::Ui,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
    pointer: Pos2,
) {
    let Some((uv, target, _rect)) =
        screen_to_element_uv(state, full_rect, viewport_size, pointer)
    else {
        state.status = crate::i18n::t("Eyedropper mask: select an element first.").into();
        return;
    };
    // The UV math returns a clamped value in [-0.5, 1.5] so the user
    // can paint near the edges; for sampling we need a real pixel
    // inside the source so a click outside the picture is rejected.
    if uv[0] < 0.0 || uv[0] > 1.0 || uv[1] < 0.0 || uv[1] > 1.0 {
        state.status = crate::i18n::t("Eyedropper mask: click on the picture itself.").into();
        return;
    }

    let picked: Option<[u8; 3]> = match target {
        Selection::Actor(idx) if idx < state.scene.actors.len() => {
            sample_actor_pixel(state, idx, uv)
        }
        Selection::Overlay(idx) if idx < state.scene.overlays.len() => {
            sample_overlay_pixel(state, idx, uv)
        }
        _ => None,
    };
    let Some(rgb) = picked else {
        state.status =
            crate::i18n::t("Eyedropper mask: source frame not yet decoded — try again in a moment.").into();
        return;
    };

    // Apply the picked colour: overwrite the latest ColorKey entry on
    // the layer if there is one, otherwise push a fresh one.
    let label = format!(
        "\u{1F4A7} {} #{:02X}{:02X}{:02X}",
        crate::i18n::t("Color key:"),
        rgb[0], rgb[1], rgb[2],
    );
    state.mutate(|scene| {
        let effects: &mut Vec<memstroy_core::Effect> = match target {
            Selection::Actor(i) => &mut scene.actors[i].effects,
            Selection::Overlay(i) => match &mut scene.overlays[i] {
                memstroy_core::Overlay::Text(t) => &mut t.effects,
                memstroy_core::Overlay::Image(im) => &mut im.effects,
                memstroy_core::Overlay::Video(v) => &mut v.effects,
            },
            _ => return,
        };
        // Prefer the rightmost existing ColorKey so the user's most
        // recently-added entry is the one that updates.
        let existing = effects
            .iter_mut()
            .rev()
            .find(|e| matches!(e.kind, memstroy_core::EffectKind::ColorKey { .. }));
        if let Some(eff) = existing {
            if let memstroy_core::EffectKind::ColorKey { color, .. } = &mut eff.kind {
                *color = rgb;
            }
        } else {
            // Construct via the canonical preset so we inherit the
            // default similarity / blend / spill values defined by
            // the core `Effect::color_key()` helper.
            let mut eff = memstroy_core::Effect::color_key();
            if let memstroy_core::EffectKind::ColorKey { color, .. } = &mut eff.kind {
                *color = rgb;
            }
            effects.push(eff);
        }
    });
    state.status = label;
    ui.ctx().request_repaint();
}

/// Sample a single pixel from the actor's decoded frame cache at the
/// given UV. Returns `None` when the cache hasn't decoded the frame
/// yet (the user can retry once the frame lands). Mirrors the source-
/// time math used by `handle_eyedropper_click_actor` so the picked
/// pixel matches what the user sees on canvas. Takes `&mut` because
/// the underlying frame-cache fetch lazily promotes decoded frames
/// onto the recently-used list.
fn sample_actor_pixel(state: &mut EditorState, idx: usize, uv: [f32; 2]) -> Option<[u8; 3]> {
    let actor = state.scene.actors.get(idx)?;
    let t = state.playhead;
    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(state.scene.output.duration);
    let speed = actor.speed.max(0.0001);
    let local_t = if t >= t_in && t <= t_out {
        (t - t_in) * speed + actor.source_start
    } else if t < t_in {
        actor.source_start
    } else {
        actor.source_start + (t_out - t_in) * speed
    };
    let fc = state.frame_caches.get_mut(idx)?;
    let img = fc.raw_frame_at_time(local_t)?;
    let px = ((uv[0] * img.size[0] as f32) as usize).min(img.size[0].saturating_sub(1));
    let py = ((uv[1] * img.size[1] as f32) as usize).min(img.size[1].saturating_sub(1));
    let pixel = img.pixels[py * img.size[0] + px];
    Some([pixel.r(), pixel.g(), pixel.b()])
}

/// Sample a single pixel from an image overlay's source PNG at the
/// given UV. Loads the file directly (small sticker PNGs decode in
/// milliseconds — same trade-off the existing
/// `handle_eyedropper_click_overlay` makes). Text / video overlays
/// don't have a single source colour so they short-circuit to
/// `None`; the click-handler then surfaces a useful status line.
fn sample_overlay_pixel(state: &EditorState, idx: usize, uv: [f32; 2]) -> Option<[u8; 3]> {
    let overlay = state.scene.overlays.get(idx)?;
    let path = match overlay {
        memstroy_core::Overlay::Image(im) => im.source.clone(),
        _ => return None,
    };
    let path_buf = if path.is_absolute() {
        path
    } else {
        state.assets_root.join(path)
    };
    let rgba = image::open(&path_buf).ok()?.to_rgba8();
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    if w == 0 || h == 0 { return None; }
    let px = ((uv[0] * w as f32) as usize).min(w - 1);
    let py = ((uv[1] * h as f32) as usize).min(h - 1);
    let raw = rgba.as_raw();
    let i = (py * w + px) * 4;
    Some([raw[i], raw[i + 1], raw[i + 2]])
}

/// Draw the in-progress mask shape on top of the canvas while the
/// user is dragging. Visual only — committed shapes render through
/// the live image-effects pipeline.
///
/// The UV→screen mapping rotates each point around the element's
/// centre so the dashed preview tracks the *rotated* picture pixel
/// for pixel. Without this the preview rectangle / ellipse / polyline
/// floats axis-aligned over a rotated layer, which the user reported
/// as "masks work poorly on rotated elements". The render side and
/// the input side already operate in image-local UV (the input pipe
/// inverse-rotates the pointer in `screen_to_element_uv`) so this
/// preview alignment is the only piece that was missing.
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
    let rotation_deg = selected_element_rotation_deg(state);
    let center = elem_rect.center();
    let half_w = elem_rect.width() * 0.5;
    let half_h = elem_rect.height() * 0.5;
    let theta = rotation_deg.to_radians();
    let (s, c) = (theta.sin(), theta.cos());
    // UV (0..1, image-local) → screen, applying the element's rotation
    // around the centre. Identical to the inverse of
    // `screen_to_element_uv` modulo the clamp.
    let to_screen = |uv: [f32; 2]| {
        let lx = (uv[0] - 0.5) * 2.0 * half_w;
        let ly = (uv[1] - 0.5) * 2.0 * half_h;
        let rx = lx * c - ly * s;
        let ry = lx * s + ly * c;
        Pos2::new(center.x + rx, center.y + ry)
    };
    let stroke_main = Stroke::new(1.5, Color32::from_rgb(255, 200, 50));
    let stroke_dash = Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 200, 50, 160));
    let last_uv = *state
        .mask_draft_points
        .last()
        .unwrap_or(&start_uv);
    match tool {
        MaskTool::RectMask => {
            // Draw the rectangle as a closed polyline in element-local
            // UV so it rotates with the image. `Rect::from_two_pos`
            // would axis-align it on screen; we want the four corners
            // mapped through `to_screen` instead.
            let lx = start_uv[0].min(last_uv[0]);
            let rx = start_uv[0].max(last_uv[0]);
            let ty = start_uv[1].min(last_uv[1]);
            let by = start_uv[1].max(last_uv[1]);
            let p00 = to_screen([lx, ty]);
            let p10 = to_screen([rx, ty]);
            let p11 = to_screen([rx, by]);
            let p01 = to_screen([lx, by]);
            painter.line_segment([p00, p10], stroke_main);
            painter.line_segment([p10, p11], stroke_main);
            painter.line_segment([p11, p01], stroke_main);
            painter.line_segment([p01, p00], stroke_main);
        }
        MaskTool::EllipseMask => {
            // Approximate an ellipse with a dense polyline IN UV
            // space, then project each vertex through `to_screen` so
            // the curve rotates with the picture.
            let cx_uv = (start_uv[0] + last_uv[0]) * 0.5;
            let cy_uv = (start_uv[1] + last_uv[1]) * 0.5;
            let rx_uv = ((last_uv[0] - start_uv[0]).abs() * 0.5).max(0.001);
            let ry_uv = ((last_uv[1] - start_uv[1]).abs() * 0.5).max(0.001);
            let segments = 64;
            let mut prev = to_screen([cx_uv + rx_uv, cy_uv]);
            for s in 1..=segments {
                let theta = (s as f32 / segments as f32) * std::f32::consts::TAU;
                let cur = to_screen([
                    cx_uv + rx_uv * theta.cos(),
                    cy_uv + ry_uv * theta.sin(),
                ]);
                painter.line_segment([prev, cur], stroke_main);
                prev = cur;
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
        MaskTool::Eyedropper => {
            // The eyedropper commits on click via
            // `handle_eyedropper_mask_click` — there is no drag draft
            // to draw. The crosshair cursor (set by
            // `handle_mask_draw_input`) is the only visual hint, and
            // the inspector swatch reflects the picked colour as
            // soon as the click lands.
        }
        MaskTool::SegmentMask => {
            // Solid lines between every consecutive committed vertex
            // pair. Drawn first so the vertex dots paint over any
            // segment endpoints (which they share).
            if state.mask_draft_points.len() >= 2 {
                let mut prev = to_screen(state.mask_draft_points[0]);
                for &uv in state.mask_draft_points.iter().skip(1) {
                    let cur = to_screen(uv);
                    painter.line_segment([prev, cur], stroke_main);
                    prev = cur;
                }
            }
            // Vertex dots — small filled circles with a dark outline
            // so they read on light AND dark layers. The first
            // vertex gets a larger blue halo so the user can see the
            // closure target at a glance.
            for (i, &uv) in state.mask_draft_points.iter().enumerate() {
                let p = to_screen(uv);
                if i == 0 {
                    painter.circle_stroke(
                        p,
                        8.0,
                        Stroke::new(1.5, Color32::from_rgb(120, 220, 255)),
                    );
                }
                painter.circle_filled(p, 4.0, Color32::from_rgb(255, 200, 50));
                painter.circle_stroke(p, 4.0, Stroke::new(1.0, Color32::from_rgb(40, 30, 0)));
            }
            // Rubber-band line from the last placed vertex to the
            // current cursor, plus a dashed close-preview from the
            // cursor back to the first vertex (only once we have
            // ≥ 2 vertices placed — before that there's nothing to
            // close back to). The cursor UV is updated every frame
            // by `handle_segment_mask_input` and stays `None` while
            // the cursor is off the element so the rubber-band
            // doesn't visually lock onto the last in-bounds spot.
            if let (Some(&first), Some(&last), Some(cursor_uv)) = (
                state.mask_draft_points.first(),
                state.mask_draft_points.last(),
                state.mask_segment_cursor_uv,
            ) {
                let cursor_screen = to_screen(cursor_uv);
                let last_screen = to_screen(last);
                painter.line_segment([last_screen, cursor_screen], stroke_dash);
                if state.mask_draft_points.len() >= 2 {
                    let first_screen = to_screen(first);
                    painter.line_segment([cursor_screen, first_screen], stroke_dash);
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
        guides.push(crate::state::SnapGuide::axis_aligned(
            crate::state::SnapAxis::Vertical,
            snapped_x,
        ));
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
        guides.push(crate::state::SnapGuide::axis_aligned(
            crate::state::SnapAxis::Horizontal,
            snapped_y,
        ));
    }

    // ── Rotated render-frame edge snap ──
    //
    // When the render frame is rotated, its left/right edges are no
    // longer vertical lines and its top/bottom edges are no longer
    // horizontal lines, so the axis-aligned `xs` / `ys` collection
    // above silently stops covering them. The user reported this as
    // "только центр области рендера снапит, к краям тоже надо, и не
    // забывай про rotation". Below we additionally snap to each of
    // the four edges as actual oriented lines: we project the
    // (already axis-snapped) candidate onto each line, accept the
    // closest hit as the new snapped point, and emit a `Line` guide
    // so the user sees a guideline that actually follows the rotated
    // edge instead of an axis-aligned approximation.
    let (snapped_x, snapped_y, line_guide) =
        snap_to_render_frame_rotated_edges(state, snapped_x, snapped_y, thresh);
    if let Some(g) = line_guide {
        guides.push(g);
    }

    (snapped_x, snapped_y, guides)
}

/// Snap a proposed point to the closest rotated edge of the render
/// frame, when the frame's rotation makes axis-aligned edge snap
/// inadequate. Returns `(x, y, Some(guide))` when an edge was within
/// `thresh` (world units) of the proposed point — caller pushes the
/// guide onto the active list so it's drawn on top of the canvas.
///
/// At rotation 0 this is a no-op: the axis-aligned x/y snap above
/// already covers vertical / horizontal edges, and we'd just emit a
/// duplicate guide otherwise.
fn snap_to_render_frame_rotated_edges(
    state: &EditorState,
    proposed_x: f32,
    proposed_y: f32,
    thresh: f32,
) -> (f32, f32, Option<crate::state::SnapGuide>) {
    let rf = &state.scene.render_frame;
    let rf_state = sample_render_frame_eased(rf, state.playhead);
    let rad = rf_state.rotation_deg.to_radians();
    if rad.abs() < 1.0e-3 {
        // Frame is axis-aligned — caller's xs/ys snap already covers it.
        return (proposed_x, proposed_y, None);
    }

    let [rw, rh] = rf.resolution;
    let world_w = rw as f32 / rf_state.zoom.max(1e-6);
    let world_h = rh as f32 / rf_state.zoom.max(1e-6);
    let cx = rf_state.pos.x;
    let cy = rf_state.pos.y;

    // Project (proposed - centre) into the frame's local axes.
    // `lx` is along the rotated horizontal axis, `ly` along the
    // rotated vertical axis. Distance to each edge is just the local
    // coordinate offset from ±half_extent.
    let cs = rad.cos();
    let sn = rad.sin();
    let dx = proposed_x - cx;
    let dy = proposed_y - cy;
    let lx = dx * cs + dy * sn;
    let ly = -dx * sn + dy * cs;
    let half_w = world_w * 0.5;
    let half_h = world_h * 0.5;

    // For each of the four edges, perpendicular distance in the
    // frame's local frame is just |lx ± half_w| or |ly ± half_h|.
    let candidates: [(f32, f32, [f32; 2], f32); 4] = [
        // (signed local target axis, distance, line origin in local,
        //  line angle_rad in world)
        // Left edge:   local x = -half_w, line direction = local Y axis
        ( -half_w - lx,                      (-half_w - lx).abs(),  [-half_w, 0.0], rad + std::f32::consts::FRAC_PI_2 ),
        // Right edge:  local x = +half_w, line direction = local Y axis
        ( half_w  - lx,                      (half_w  - lx).abs(),  [ half_w, 0.0], rad + std::f32::consts::FRAC_PI_2 ),
        // Top edge:    local y = -half_h, line direction = local X axis
        ( -half_h - ly,                      (-half_h - ly).abs(),  [0.0, -half_h], rad ),
        // Bottom edge: local y = +half_h, line direction = local X axis
        ( half_h  - ly,                      (half_h  - ly).abs(),  [0.0,  half_h], rad ),
    ];

    let mut best_idx: Option<usize> = None;
    let mut best_dist = thresh;
    for (i, (_, dist, _, _)) in candidates.iter().enumerate() {
        if *dist < best_dist {
            best_dist = *dist;
            best_idx = Some(i);
        }
    }

    let Some(idx) = best_idx else {
        return (proposed_x, proposed_y, None);
    };
    let (signed, _, local_origin, line_angle) = candidates[idx];

    // Apply the snap by moving along the perpendicular of the edge
    // (i.e. along the local X axis for left/right edges; along the
    // local Y axis for top/bottom). Convert that delta back to world.
    let (dlx, dly) = if idx < 2 {
        (signed, 0.0)
    } else {
        (0.0, signed)
    };
    let snapped_dx = dlx * cs - dly * sn;
    let snapped_dy = dlx * sn + dly * cs;
    let snapped_x = proposed_x + snapped_dx;
    let snapped_y = proposed_y + snapped_dy;

    // Convert the line origin from local to world for the guide.
    let world_origin_x = cx + local_origin[0] * cs - local_origin[1] * sn;
    let world_origin_y = cy + local_origin[0] * sn + local_origin[1] * cs;

    let guide = crate::state::SnapGuide::line([world_origin_x, world_origin_y], line_angle);
    (snapped_x, snapped_y, Some(guide))
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
    // Centre snaps regardless of rotation — the centre point of a
    // rectangle is invariant under rotation.
    xs.push(cx);
    ys.push(cy);
    // Edges only contribute axis-aligned snap targets when the frame
    // itself is axis-aligned (rotation ≈ 0). When the frame is
    // rotated, `cx ± world_w/2` no longer corresponds to a visible
    // edge, so we skip these and let `snap_to_render_frame_rotated_edges`
    // emit proper rotated-line guides instead.
    let rf_rad = rf_state.rotation_deg.to_radians();
    if rf_rad.abs() < 1.0e-3 {
        xs.push(cx - world_w * 0.5);
        xs.push(cx + world_w * 0.5);
        ys.push(cy - world_h * 0.5);
        ys.push(cy + world_h * 0.5);
    }

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
            crate::state::SnapAxis::Line => {
                // Free-orientation guide (rotated render-frame edge).
                // Compute two world-space points far enough apart in
                // either direction along the line that the resulting
                // segment certainly crosses the entire visible
                // canvas, then convert to screen coords. egui's
                // painter clips against `full_rect` for us, so we
                // can be generous with the segment length without
                // worrying about overdraw.
                let len: f32 = 1.0e6;
                let dir_x = guide.line_angle_rad.cos();
                let dir_y = guide.line_angle_rad.sin();
                let p1_world = WorldPos {
                    x: guide.line_origin[0] - dir_x * len,
                    y: guide.line_origin[1] - dir_y * len,
                };
                let p2_world = WorldPos {
                    x: guide.line_origin[0] + dir_x * len,
                    y: guide.line_origin[1] + dir_y * len,
                };
                let p1 = state.canvas_viewport.world_to_screen(p1_world, viewport_size);
                let p2 = state.canvas_viewport.world_to_screen(p2_world, viewport_size);
                let p1s = Pos2::new(full_rect.min.x + p1[0], full_rect.min.y + p1[1]);
                let p2s = Pos2::new(full_rect.min.x + p2[0], full_rect.min.y + p2[1]);
                painter.line_segment([p1s, p2s], Stroke::new(1.0, col));
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

    let pointer_pos = ui.input(|i| i.pointer.hover_pos().or(i.pointer.interact_pos()));
    let drag_pos = pointer_pos.unwrap_or_else(|| {
        egui::pos2(state.asset_drag.pos[0], state.asset_drag.pos[1])
    });
    state.asset_drag.pos = [drag_pos.x, drag_pos.y];
    let in_canvas = full_rect.contains(drag_pos);

    // Ghost preview only while the cursor is over the canvas; drop handling
    // below still uses `drag_pos` (with the last known position as fallback).
    if !in_canvas {
        let mouse_released = ui.input(|i| i.pointer.any_released());
        if !mouse_released {
            return;
        }
    } else {

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
        painter.rect_filled(thumb_rect, Rounding::same(3.0), Color32::from_rgb(44, 42, 28));
        painter.text(
            thumb_rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{1F3AC}",
            egui::FontId::proportional(24.0),
            Color32::from_rgb(255, 200, 50),
        );
    }
    let label = if state.asset_drag.label.is_empty() {
        crate::i18n::t("Drop on canvas").to_string()
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
        crate::i18n::t("drop here to place at cursor"),
        egui::FontId::proportional(9.0),
        Color32::from_rgb(160, 160, 180),
    );
    }

    // ── Accept drop on release ──
    let mouse_released = ui.input(|i| i.pointer.any_released());
    if mouse_released && in_canvas {
        let world = state
            .canvas_viewport
            .screen_to_world([drag_pos.x - full_rect.min.x, drag_pos.y - full_rect.min.y], viewport_size);
        let asset_path = state.asset_drag.dragging.clone().unwrap();
        let asset_label = state.asset_drag.label.clone();
        let kind = state.asset_drag.kind;
        match kind {
            crate::state::AssetDragKind::Clip | crate::state::AssetDragKind::Video => {
                // Server-only stubs: download the full `.mp4` first;
                // `ClipDownloaded` places the actor at the drop point.
                if crate::panels::try_spawn_lazy_clip_download(
                    state,
                    &asset_path,
                    crate::jobs::ClipDropTarget::CanvasAt {
                        world_x: world.x,
                        world_y: world.y,
                    },
                ) {
                    state.asset_drag.dragging = None;
                    state.asset_drag.kind = crate::state::AssetDragKind::None;
                    state.asset_drag.label.clear();
                    state.asset_drag.thumbnail = None;
                    return;
                }
                crate::panels::add_actor_from_clip_at_canvas(
                    state,
                    &asset_path,
                    [world.x, world.y],
                );
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
                    // Convert the world-pixel drop point back to
                    // normalised `pos` coords. Mirrors the inverse of
                    // `draw_canvas_overlays`'s
                    // `world_pos = ov_state.pos * world_size` so the
                    // dropped overlay materialises EXACTLY under the
                    // cursor regardless of where the render frame is
                    // / how it's zoomed.
                    let rf = &state.scene.render_frame;
                    let [rw, rh] = rf.resolution;
                    let world_w = rw as f32;
                    let world_h = rh as f32;
                    if let Some(last) = state.scene.overlays.last_mut() {
                        let layout = match last {
                            Overlay::Image(im) => &mut im.layout,
                            Overlay::Video(v) => &mut v.layout,
                            Overlay::Text(t) => &mut t.layout,
                        };
                        if let Some(kf) = layout.first_mut() {
                            kf.value.pos = [
                                (world.x / world_w).clamp(-2.0, 3.0),
                                (world.y / world_h).clamp(-2.0, 3.0),
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


// ─── SKELETON-POINT CANVAS OVERLAY ───────────────────────────────────
//
// The standalone "Skeleton Editor" floating window has been retired —
// every piece of skeleton authoring lives in the inspector now. Point
// PLACEMENT is the one piece that didn't fit comfortably in the
// inspector (the user needs to see the actor full-size to align a
// point against a real feature), so it now happens directly on the
// main canvas: the markers are drawn over the host actor / video
// overlay, and dragging one writes a keyframe at the current playhead
// in clip-local time.

/// Hint at which clip the inspector is currently editing the skeleton
/// for. Returns the source-clip context for the active selection
/// (actor or video overlay) so the canvas overlay can resolve / draw
/// the matching template.
fn active_skeleton_ctx(
    state: &EditorState,
) -> Option<crate::skeleton_editor::SourceClipCtx> {
    match state.selection {
        Selection::Actor(i) => crate::skeleton_editor::SourceClipCtx::from_actor(state, i),
        Selection::Overlay(i) => {
            crate::skeleton_editor::SourceClipCtx::from_video_overlay(state, i)
        }
        _ => None,
    }
}

/// Compute the on-canvas screen rectangle that the skeleton points are
/// projected through. Mirrors the per-element projection used by the
/// renderer for `SkeletonAttachment` so dragging a point on the canvas
/// directly correlates with the source-clip's normalised [0,1] coords.
fn skeleton_host_screen_rect(
    state: &EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> Option<Rect> {
    match state.selection {
        Selection::Actor(i) => actor_screen_rect(state, full_rect, viewport_size, i),
        Selection::Overlay(i) => {
            // Reuse the overlay AABB and convert it to screen.
            let (mn, mx) = overlay_world_aabb(state, i)?;
            let center = [(mn[0] + mx[0]) * 0.5, (mn[1] + mx[1]) * 0.5];
            let center_screen = state
                .canvas_viewport
                .world_to_screen(WorldPos { x: center[0], y: center[1] }, viewport_size);
            let zoom = state.canvas_viewport.zoom.max(0.0001);
            let half_w = (mx[0] - mn[0]) * 0.5 * zoom;
            let half_h = (mx[1] - mn[1]) * 0.5 * zoom;
            Some(Rect::from_center_size(
                Pos2::new(
                    full_rect.min.x + center_screen[0],
                    full_rect.min.y + center_screen[1],
                ),
                Vec2::new(half_w * 2.0, half_h * 2.0),
            ))
        }
        _ => None,
    }
}

/// Draw skeleton-point markers over the selected video-layer element
/// (and the per-point guide images, if any).
fn draw_canvas_skeleton_overlay(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    full_rect: Rect,
    state: &EditorState,
    viewport_size: [f32; 2],
) {
    let Some(ctx) = active_skeleton_ctx(state) else { return; };
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else { return; };
    if state
        .skeleton_editor
        .clip_path
        .as_deref()
        .map(|p| p != ctx.source && p.file_name() != ctx.source.file_name())
        .unwrap_or(true)
    {
        // Selection is a video-layer element but the inspector is
        // tracking a different clip's template (stale state from the
        // previous selection). Skip until the next inspector paint
        // syncs the template index.
        return;
    }
    let Some(host_rect) = skeleton_host_screen_rect(state, full_rect, viewport_size) else {
        return;
    };
    let Some(template) = state.scene.skeleton_templates.get(tmpl_idx) else { return; };

    let clip_t = ctx.clip_local_time(state.playhead);
    let dragging = state.skeleton_editor.dragging_point.clone();
    let selected_name = state.skeleton_editor.selected_point.clone();

    // Optional per-point guide images, drawn first so the markers sit
    // on top.
    for (name, point) in &template.points {
        let Some(img_path) = state.skeleton_editor.point_guide_images.get(name) else {
            continue;
        };
        let ps = crate::skeleton_editor::sample_point_at(point, clip_t);
        let cx = host_rect.min.x + ps.x * host_rect.width();
        let cy = host_rect.min.y + ps.y * host_rect.height();
        let size = (host_rect.width() * 0.22).clamp(40.0, 240.0);
        let img_rect =
            egui::Rect::from_center_size(Pos2::new(cx, cy), Vec2::splat(size));
        let uri = format!("file://{}", img_path.display());
        let img = egui::Image::from_uri(uri)
            .fit_to_exact_size(Vec2::splat(size))
            .maintain_aspect_ratio(true)
            .tint(Color32::from_rgba_unmultiplied(255, 255, 255, 140));
        img.paint_at(ui, img_rect);
    }

    // Marker stroke / fill.
    for (name, point) in &template.points {
        let ps = crate::skeleton_editor::sample_point_at(point, clip_t);
        let sx = host_rect.min.x + ps.x * host_rect.width();
        let sy = host_rect.min.y + ps.y * host_rect.height();
        let pos = Pos2::new(sx, sy);

        let is_selected = selected_name.as_deref() == Some(name);
        let is_dragging = dragging.as_deref() == Some(name);
        let color = if is_selected || is_dragging {
            Color32::from_rgb(255, 220, 80)
        } else {
            Color32::from_rgb(point.color[0], point.color[1], point.color[2])
        };
        let radius = if is_selected || is_dragging { 9.0 } else { 6.5 };

        painter.circle_filled(
            pos + Vec2::new(0.0, 1.0),
            radius + 0.5,
            Color32::from_black_alpha(180),
        );
        painter.circle_filled(pos, radius, color);
        painter.circle_stroke(pos, radius, Stroke::new(1.5, Color32::WHITE));
        painter.text(
            Pos2::new(pos.x + 11.0, pos.y - 6.0),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(11.0),
            color,
        );

        // Diamond indicator if there's a kf within ~one frame of `t`.
        let fps = state.skeleton_editor.fps.max(1.0);
        let kf_proximity = 1.0 / fps * 0.6;
        let has_kf = point.track.iter().any(|kf| (kf.t - clip_t).abs() < kf_proximity);
        if has_kf {
            painter.text(
                Pos2::new(pos.x, pos.y - radius - 4.0),
                egui::Align2::CENTER_BOTTOM,
                "\u{25C6}",
                egui::FontId::proportional(9.0),
                Color32::from_rgb(255, 200, 50),
            );
        }
    }
}

/// Pointer interaction over the canvas skeleton-point overlay.
///
/// Returns `true` when the gesture is owned by skeleton authoring
/// (hover near a marker, active drag on a marker, or click placement
/// of a freshly-selected point) so the caller short-circuits the
/// regular drag pipeline. Public draw + interact split mirrors the
/// `mask_tool_active` short-circuit pattern in `canvas_preview()`.
fn handle_canvas_skeleton_input(
    ui: &mut egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    full_rect: Rect,
    viewport_size: [f32; 2],
) -> bool {
    let Some(ctx) = active_skeleton_ctx(state) else {
        // Cancel any in-flight drag if the user changed selection
        // mid-gesture.
        state.skeleton_editor.dragging_point = None;
        return false;
    };
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        state.skeleton_editor.dragging_point = None;
        return false;
    };
    if state
        .skeleton_editor
        .clip_path
        .as_deref()
        .map(|p| p != ctx.source && p.file_name() != ctx.source.file_name())
        .unwrap_or(true)
    {
        state.skeleton_editor.dragging_point = None;
        return false;
    }
    let Some(host_rect) = skeleton_host_screen_rect(state, full_rect, viewport_size) else {
        state.skeleton_editor.dragging_point = None;
        return false;
    };

    let pointer_pos = response.interact_pointer_pos();
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let primary_released = ui.input(|i| i.pointer.any_released());

    let clip_t = ctx.clip_local_time(state.playhead);

    // Hit-test: closest marker to the cursor (within ~14 px).
    let mut hovered_point: Option<String> = None;
    if state.skeleton_editor.dragging_point.is_none() {
        if let Some(pos) = pointer_pos {
            if host_rect.contains(pos) {
                let template = &state.scene.skeleton_templates[tmpl_idx];
                let mut best: Option<(String, f32)> = None;
                for (name, point) in &template.points {
                    let ps = crate::skeleton_editor::sample_point_at(point, clip_t);
                    let sx = host_rect.min.x + ps.x * host_rect.width();
                    let sy = host_rect.min.y + ps.y * host_rect.height();
                    let dist = ((pos.x - sx).powi(2) + (pos.y - sy).powi(2)).sqrt();
                    if dist < 14.0 && best.as_ref().map(|b| dist < b.1).unwrap_or(true) {
                        best = Some((name.clone(), dist));
                    }
                }
                if let Some((name, _)) = best {
                    hovered_point = Some(name);
                }
            }
        }
    }

    // Continue or finish an in-flight drag.
    if let Some(name) = state.skeleton_editor.dragging_point.clone() {
        if primary_down {
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - host_rect.min.x) / host_rect.width().max(1.0))
                    .clamp(0.0, 1.0);
                let ny = ((pos.y - host_rect.min.y) / host_rect.height().max(1.0))
                    .clamp(0.0, 1.0);
                crate::skeleton_editor::place_point_at_clip_time(
                    state, &name, nx, ny, clip_t,
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            return true;
        }
        if primary_released || !primary_down {
            state.skeleton_editor.dragging_point = None;
        }
        return false;
    }

    // Begin a drag when the user presses on (or near) a marker.
    if response.drag_started() {
        if let Some(name) = hovered_point.clone() {
            state.skeleton_editor.selected_point = Some(name.clone());
            state.skeleton_editor.dragging_point = Some(name.clone());
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - host_rect.min.x) / host_rect.width().max(1.0))
                    .clamp(0.0, 1.0);
                let ny = ((pos.y - host_rect.min.y) / host_rect.height().max(1.0))
                    .clamp(0.0, 1.0);
                crate::skeleton_editor::place_point_at_clip_time(
                    state, &name, nx, ny, clip_t,
                );
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            return true;
        }
        // No marker under the cursor — but a point is selected and the
        // user grabbed the host rect → start placing the selected
        // point at the cursor (matches the old preview-window
        // "place-by-drag" behaviour).
        if let Some(name) = state.skeleton_editor.selected_point.clone() {
            if let Some(pos) = pointer_pos {
                if host_rect.contains(pos) {
                    state.skeleton_editor.dragging_point = Some(name.clone());
                    let nx = ((pos.x - host_rect.min.x) / host_rect.width().max(1.0))
                        .clamp(0.0, 1.0);
                    let ny = ((pos.y - host_rect.min.y) / host_rect.height().max(1.0))
                        .clamp(0.0, 1.0);
                    crate::skeleton_editor::place_point_at_clip_time(
                        state, &name, nx, ny, clip_t,
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    return true;
                }
            }
        }
    }

    // Click on a marker → just select it (no kf write).
    if response.clicked() {
        if let Some(name) = hovered_point.clone() {
            state.skeleton_editor.selected_point = Some(name);
            return true;
        }
        // Click on empty host area with a selected point → drop a kf.
        if let Some(name) = state.skeleton_editor.selected_point.clone() {
            if let Some(pos) = pointer_pos {
                if host_rect.contains(pos) {
                    let nx = ((pos.x - host_rect.min.x) / host_rect.width().max(1.0))
                        .clamp(0.0, 1.0);
                    let ny = ((pos.y - host_rect.min.y) / host_rect.height().max(1.0))
                        .clamp(0.0, 1.0);
                    crate::skeleton_editor::place_point_at_clip_time(
                        state, &name, nx, ny, clip_t,
                    );
                    return true;
                }
            }
        }
    }

    // Hover cursor hint.
    if hovered_point.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        // Hovering over a marker is enough to reserve the gesture for
        // the skeleton overlay so the user doesn't accidentally start
        // moving the host actor on the next press.
        return true;
    }

    false
}

#[cfg(test)]
mod transform_hierarchy_tests {
    use super::*;

    fn actor(id: &str, parent_id: Option<&str>) -> Actor {
        Actor {
            id: id.to_string(),
            source: std::path::PathBuf::new(),
            anchors: None,
            chroma_key: ChromaKeyParams::default(),
            layout: vec![Keyframe::new(0.0, ActorState::default())],
            t_in: None,
            t_out: None,
            visible: true,
            z_order: 0,
            source_start: 0.0,
            loop_source: false,
            flip_horizontal: false,
            attachments: Vec::new(),
            skeleton_attachments: Vec::new(),
            modifiers: Vec::new(),
            effects: Vec::new(),
            parent_id: parent_id.map(str::to_string),
            color_correction: ColorCorrection::default(),
            transition_in: Transition::default(),
            transition_out: Transition::default(),
            transition_duration: 0.35,
            speed: 1.0,
            animated_params: std::collections::BTreeSet::new(),
            mute_audio: false,
        }
    }

    fn canvas_layout(id: &str, pos: WorldPos, scale: f32, rotation_deg: f32) -> CanvasLayout {
        CanvasLayout {
            element_id: id.to_string(),
            keyframes: vec![Keyframe::new(0.0, CanvasTransform { pos, width: 500.0, scale, rotation_deg, opacity: 1.0 })],
        }
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1.0e-3, "actual={actual}, expected={expected}");
    }

    #[test]
    fn child_position_follows_parent_position_rotation_and_non_uniform_scale() {
        let mut state = EditorState::new();
        state.scene.actors = vec![actor("parent", None), actor("child", Some("parent"))];
        state.scene.canvas_layouts = vec![
            canvas_layout("parent", WorldPos { x: 100.0, y: 100.0 }, 2.0, 90.0),
            canvas_layout("child", WorldPos { x: 10.0, y: 20.0 }, 1.0, 0.0),
        ];

        let pos = get_element_world_pos(&state, "child", &state.scene.actors[1].layout, 0.0);
        assert_close(pos.x, 60.0);
        assert_close(pos.y, 120.0);

        let mut visited = vec!["child".to_string()];
        let parent = resolve_parent_transform(&state, "parent", 0.0, &mut visited).unwrap();
        assert_close(parent.scale_x, 2.0);
        assert_close(parent.scale_y, 2.0);
    }

    #[test]
    fn inverse_parent_transform_preserves_world_when_reparenting() {
        let parent = ParentTransform {
            pos: WorldPos { x: 100.0, y: 100.0 },
            rotation_deg: 90.0,
            scale_x: 2.0,
            scale_y: 2.0,
        };
        let world = WorldPos { x: 60.0, y: 120.0 };
        let local = inverse_parent_transform(world, &parent);
        assert_close(local.x, 10.0);
        assert_close(local.y, 20.0);
        let roundtrip = apply_parent_transform(local, &parent);
        assert_close(roundtrip.x, world.x);
        assert_close(roundtrip.y, world.y);
    }
}
