//! Frame snapshot — bake the current canvas at the playhead into a
//! standalone image asset.
//!
//! The button in the timeline toolbar (`📸 Extract frame`) calls
//! [`extract_frame_to_image_layer`]. The compositor walks the scene
//! the same way `canvas_preview::draw_canvas_elements` does — sample
//! each layer's animated state at `state.playhead`, apply the user's
//! CPU effect stack (`image_effects::apply_effect_stack` for image
//! overlays, `video_cache::apply_effects_cpu` + `apply_effect_stack_cpu`
//! for actors), then place the result onto an `image::RgbaImage` of
//! `scene.render_frame.resolution` using inverse-mapping affine
//! sampling so rotation / scale / flip survive.
//!
//! Two modes:
//!
//! - **Full frame** (default — no canvas selection, no primary
//!   selection): every visible layer at `t` is composited onto a
//!   transparent canvas (no scene background fill).
//! - **Subset** (when `state.canvas_selection` is non-empty, or when
//!   `state.selection` points at a single element): only the listed
//!   layers are composited and the canvas is sized to their world
//!   bounding box, also transparent.
//!
//! In both modes the resulting RGBA is saved as
//! `assets/images/frame_<unix-millis>.png` and a fresh
//! `Overlay::Image` layer is added at the playhead pointing at it,
//! mirroring the existing Ctrl+V "paste image from clipboard" flow.
//!
//! ## What's covered
//!
//! - **Image overlays** — full effect stack (blur, hue shift, crop,
//!   mask, colour-key, …), opacity, scale, rotation, flip.
//! - **Actors** — chroma key + colour correction + effect stack,
//!   opacity, scale, rotation, flip (static `flip_horizontal` plus
//!   animated `flip_x_anim` / `flip_y_anim`).
//! - **Text overlays** — full plate (solid / gradient / outline-only
//!   / per-line wrap), corner radius, asymmetric padding, glyph
//!   stroke, alignment, italic, opacity, scale, rotation and flip.
//!   Routed through `memstroy_render::rasterize_text_overlay` so the
//!   snapshot is glyph-for-glyph identical to the MP4 export.
//! - **Video overlays** — single-frame extraction via ffmpeg at
//!   `(t - t_in) * speed + source_start` (with `loop_source` wrap),
//!   followed by chroma key + effect stack, then placed exactly like
//!   an image overlay.
//! - **Backgrounds** — solid colour fills the render frame; image
//!   and video backgrounds are loaded / extracted and composited
//!   under the supplied `Fit` mode (Cover / Contain / Original /
//!   Stretch), matching what the export filter graph produces.
//!
//! ## Skip cases (reflected in the status string)
//!
//! Each background / overlay paint reports success or failure; only
//! genuine failures bump the skip counters. Typical causes:
//!
//! - missing or undecodable image / font file,
//! - ffmpeg / ffprobe not on `PATH` (so video frame extraction can't
//!   run),
//! - empty text body (silently treated as success — there's nothing
//!   to draw).

use std::path::Path;

use image::{Rgba, RgbaImage};
use memstroy_core::{
    canvas::WorldPos, effects::Effect, keyframe, ChromaKeyParams, ColorCorrection,
    Fit, ImageOverlay, MediaSource, Overlay, RenderFrame, RenderFrameState, Scene,
    TextOverlay, VideoOverlay,
};

use crate::image_effects;
use crate::state::{EditorState, Selection};
use crate::video_cache;

/// Public entry point — composes the canvas at the playhead, saves
/// the result into the project's image library, and adds a new
/// image-overlay layer pointing at it. Returns the index of the new
/// overlay on success.
pub fn extract_frame_to_image_layer(state: &mut EditorState) -> Result<usize, String> {
    let t = state.playhead;

    // ── Decide which layers to capture ───────────────────────────
    //
    // 1. Multi-selection on the canvas wins (Ctrl+click / marquee).
    // 2. Otherwise the primary selection (single element) is the
    //    subset — handy for "copy this one layer as a flat image".
    // 3. Otherwise the WHOLE frame is captured (background + every
    //    active layer at the playhead).
    let subset: Vec<Selection> = if !state.canvas_selection.is_empty() {
        state.canvas_selection.clone()
    } else {
        match state.selection {
            Selection::None | Selection::RenderFrame => Vec::new(),
            other => vec![other],
        }
    };
    let full_frame = subset.is_empty();

    // ── Compose ──────────────────────────────────────────────────
    let summary = compose_frame(state, &subset, full_frame, t);
    let img = summary.image;
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return Err(crate::i18n::t("render frame has zero resolution").into());
    }

    // ── Save into project library + spawn the new overlay ────────
    let asset = state.save_snapshot_image_to_library(img.as_raw(), w, h)?;
    let idx = state.add_image_overlay_at_playhead(&asset);

    // Status message — mention any layers we had to skip so the user
    // isn't surprised when they look at the resulting picture.
    let mut status = if full_frame {
        format!(
            "{} '{}' ({}\u{00D7}{}).",
            crate::i18n::t("\u{1F4F8} Frame extracted as image layer"),
            asset.id, w, h
        )
    } else {
        format!(
            "{} '{}' ({}\u{00D7}{}).",
            crate::i18n::t("\u{1F4F8} Selected layers extracted as image layer"),
            asset.id, w, h
        )
    };
    if summary.skipped_text > 0 || summary.skipped_video > 0 || summary.skipped_image_bg > 0 {
        status.push_str(crate::i18n::t(" Skipped:"));
        if summary.skipped_text > 0 {
            status.push_str(&format!(" {}{}", summary.skipped_text, crate::i18n::t(" text")));
        }
        if summary.skipped_video > 0 {
            status.push_str(&format!(" {}{}", summary.skipped_video, crate::i18n::t(" video overlay")));
        }
        if summary.skipped_image_bg > 0 {
            status.push_str(&format!(" {}{}", summary.skipped_image_bg, crate::i18n::t(" image background")));
        }
        status.push('.');
    }
    state.status = status;
    Ok(idx)
}

// ─── COMPOSITOR ────────────────────────────────────────────────────

/// World-space bounding box of every actor / overlay / background in
/// `subset` at time `t`. Returns `None` when nothing in the subset
/// resolves to a valid bbox. Used by the subset-mode extract to size
/// the output canvas exactly to the union of selected element bounds
/// (no render-frame clipping).
fn compute_subset_world_bbox(
    state: &EditorState,
    _scene: &Scene,
    subset: &[Selection],
    t: f32,
) -> Option<([f32; 2], [f32; 2])> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut any = false;
    for sel in subset {
        let bbox = match *sel {
            Selection::Actor(ai) => crate::canvas_preview::actor_world_aabb(state, ai, t),
            Selection::Overlay(oi) => crate::canvas_preview::overlay_world_aabb(state, oi, t),
            _ => None,
        };
        if let Some((mn, mx)) = bbox {
            min_x = min_x.min(mn[0]);
            min_y = min_y.min(mn[1]);
            max_x = max_x.max(mx[0]);
            max_y = max_y.max(mx[1]);
            any = true;
        }
    }
    if any {
        Some(([min_x, min_y], [max_x, max_y]))
    } else {
        None
    }
}

#[derive(Default)]
struct CompositeSummary {
    image: RgbaImage,
    skipped_text: u32,
    skipped_video: u32,
    skipped_image_bg: u32,
}

fn compose_frame(
    state: &mut EditorState,
    subset: &[Selection],
    full_frame: bool,
    t: f32,
) -> CompositeSummary {
    let scene_clone = state.scene.clone();
    let rf = &scene_clone.render_frame;
    let [rw_full, rh_full] = rf.resolution;
    let rw_full = rw_full.max(1);
    let rh_full = rh_full.max(1);
    let rf_state = sample_render_frame_eased(rf, t);

    // Always composite at full render-frame resolution with the real
    // camera transform, then crop to the selection bbox in output
    // space. The old "synthetic camera centred on the bbox" path
    // mis-aligned subset extracts when elements used canvas_layouts
    // or parent transforms — the saved region looked like a random
    // crop of the frame instead of the selected layer.
    let subset_crop_bbox = if full_frame {
        None
    } else {
        compute_subset_world_bbox(state, &scene_clone, subset, t)
    };

    // Transparent output — extracted images are meant to be composited
    // elsewhere, not pasted with an opaque scene background.
    let mut canvas = RgbaImage::new(rw_full, rh_full);

    let mut summary = CompositeSummary::default();

    // ── Pass 1: backgrounds ──────────────────────────────────────
    //
    // Only paint a background when the user explicitly selected it.
    // Full-frame extract stays transparent so "Extract as image"
    // produces a PNG with alpha, not the scene's solid backdrop.
    let bg_iter: Vec<usize> = subset
        .iter()
        .filter_map(|s| match s {
            Selection::Background(i) => Some(*i),
            _ => None,
        })
        .collect();
    for i in bg_iter {
        let bg = match scene_clone.backgrounds.get(i) {
            Some(b) => b,
            None => continue,
        };
        if t < bg.start || t > bg.start + bg.duration {
            continue;
        }
        match &bg.source {
            MediaSource::SolidColor { color } => {
                paint_solid_background(&mut canvas, *color, &rf_state, rw_full, rh_full);
            }
            MediaSource::Image { path } => {
                if !paint_image_background(&mut canvas, path, bg.fit, rw_full, rh_full) {
                    summary.skipped_image_bg += 1;
                }
            }
            MediaSource::Video { path, r#loop, start_at } => {
                let local = (t - bg.start).max(0.0) + *start_at;
                if !paint_video_background(
                    &mut canvas, path, local, *r#loop, bg.fit, rw_full, rh_full,
                ) {
                    summary.skipped_image_bg += 1;
                }
            }
        }
    }

    // ── Pass 2: actors and overlays interleaved by z-order ───────
    //
    // Z-order rule (matches `canvas_preview` semantics):
    //   - text overlays with `behind_actors=true` go below actors;
    //   - everything else goes above actors;
    //   - within each band, lower track index = higher Z (drawn last).
    //
    // We build a flat list of paint ops with sort keys, then iterate.

    #[derive(Clone)]
    enum PaintOp {
        Actor(usize),
        Overlay(usize),
    }

    let mut ops: Vec<(i32, i32, usize, PaintOp)> = Vec::new();

    // Track index → "z bucket" sort key. Lower track index in the
    // timeline UI = higher Z (= painted last). We negate to get the
    // ascending sort to put bottom layers first.
    let track_key = |track_idx: usize| -> i32 { -(track_idx as i32) };

    // Actors.
    let actor_indices: Vec<usize> = if full_frame {
        (0..scene_clone.actors.len()).collect()
    } else {
        subset
            .iter()
            .filter_map(|s| match s {
                Selection::Actor(i) => Some(*i),
                _ => None,
            })
            .collect()
    };
    for ai in actor_indices {
        let actor = match scene_clone.actors.get(ai) {
            Some(a) => a,
            None => continue,
        };
        if !actor.visible {
            continue;
        }
        let track = state
            .actor_track_assignments
            .get(&ai)
            .copied()
            .unwrap_or(0);
        // Actors live in the "above behind-actors-text" band. We
        // bucket them at 1 so behind-actors texts stay below.
        ops.push((1, track_key(track), ai, PaintOp::Actor(ai)));
    }

    // Overlays.
    let overlay_indices: Vec<usize> = if full_frame {
        (0..scene_clone.overlays.len()).collect()
    } else {
        subset
            .iter()
            .filter_map(|s| match s {
                Selection::Overlay(i) => Some(*i),
                _ => None,
            })
            .collect()
    };
    for oi in overlay_indices {
        let overlay = match scene_clone.overlays.get(oi) {
            Some(o) => o,
            None => continue,
        };
        let track = state
            .overlay_track_assignments
            .get(&oi)
            .copied()
            .unwrap_or(0);
        let bucket = match overlay {
            Overlay::Text(txt) if txt.behind_actors => 0, // below actors
            _ => 2,                                       // above actors
        };
        ops.push((bucket, track_key(track), oi, PaintOp::Overlay(oi)));
    }

    // Sort ascending: bucket asc, track_key asc (more negative first
    // = higher track index first = bottom layer first), then index asc
    // for stable ordering within ties.
    ops.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    for (_b, _k, _i, op) in ops {
        match op {
            PaintOp::Overlay(oi) => {
                let overlay_owned = scene_clone.overlays[oi].clone();
                match overlay_owned {
                    Overlay::Image(img_ov) => {
                        paint_image_overlay(
                            state,
                            &mut canvas,
                            &img_ov,
                            &rf_state,
                            rw_full,
                            rh_full,
                            t,
                        );
                    }
                    Overlay::Text(txt) => {
                        if !paint_text_overlay(state, &mut canvas, &txt, &rf_state, rw_full, rh_full, t)
                        {
                            summary.skipped_text += 1;
                        }
                    }
                    Overlay::Video(vid) => {
                        if !paint_video_overlay(
                            state, &mut canvas, &vid, &rf_state, rw_full, rh_full, t,
                        ) {
                            summary.skipped_video += 1;
                        }
                    }
                }
            }
            PaintOp::Actor(ai) => {
                paint_actor(state, &scene_clone, ai, &rf_state, rw_full, rh_full, t, &mut canvas);
            }
        }
    }

    // ── Pass 3: effect layers ────────────────────────────────────
    //
    // Effect layers operate on the already-composited canvas within
    // their bounding box. Applied after all content layers.
    if !scene_clone.effect_layers.is_empty() {
        apply_snapshot_effect_layers(state, &scene_clone, &rf_state, rw_full, rh_full, t, &mut canvas);
    }

    summary.image = if let Some((mn, mx)) = subset_crop_bbox {
        crop_canvas_to_world_bbox(&canvas, mn, mx, &rf_state, rw_full, rh_full)
    } else {
        canvas
    };
    summary
}

/// Crop a full-frame composite to the output-pixel bounds of a
/// world-space rectangle (with a little padding).
fn crop_canvas_to_world_bbox(
    canvas: &RgbaImage,
    world_min: [f32; 2],
    world_max: [f32; 2],
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
) -> RgbaImage {
    let pad_world = 8.0_f32;
    let corners = [
        [world_min[0] - pad_world, world_min[1] - pad_world],
        [world_max[0] + pad_world, world_min[1] - pad_world],
        [world_max[0] + pad_world, world_max[1] + pad_world],
        [world_min[0] - pad_world, world_max[1] + pad_world],
    ];
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for c in corners {
        let (ox, oy) = world_to_output(
            WorldPos { x: c[0], y: c[1] },
            rf_state,
            rw,
            rh,
        );
        min_x = min_x.min(ox);
        min_y = min_y.min(oy);
        max_x = max_x.max(ox);
        max_y = max_y.max(oy);
    }
    if !min_x.is_finite() || !min_y.is_finite() {
        return canvas.clone();
    }
    let x0 = (min_x.floor() as i32).max(0).min(rw as i32 - 1) as u32;
    let y0 = (min_y.floor() as i32).max(0).min(rh as i32 - 1) as u32;
    let x1 = (max_x.ceil() as i32).max(x0 as i32 + 1).min(rw as i32) as u32;
    let y1 = (max_y.ceil() as i32).max(y0 as i32 + 1).min(rh as i32) as u32;
    let cw = (x1 - x0).max(1);
    let ch = (y1 - y0).max(1);
    let mut out = RgbaImage::new(cw, ch);
    for y in 0..ch {
        for x in 0..cw {
            let p = *canvas.get_pixel(x0 + x, y0 + y);
            out.put_pixel(x, y, p);
        }
    }
    out
}

// ─── BACKGROUND ────────────────────────────────────────────────────

fn paint_solid_background(
    canvas: &mut RgbaImage,
    color: [u8; 3],
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
) {
    // The render-frame fills the entire output by definition, so a
    // solid background simply fills every pixel. The render-frame
    // rotation doesn't matter for a solid colour. Honour the alpha
    // semantics of the canvas: backgrounds fully cover everything
    // below them.
    let _ = rf_state; // unused but kept for parity with image/video bg
    for y in 0..rh {
        for x in 0..rw {
            let p = canvas.get_pixel_mut(x, y);
            *p = Rgba([color[0], color[1], color[2], 255]);
        }
    }
}

// ─── IMAGE OVERLAY ─────────────────────────────────────────────────

fn paint_image_overlay(
    state: &EditorState,
    canvas: &mut RgbaImage,
    img_ov: &ImageOverlay,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
) {
    // Skip when outside its visible window — we don't render
    // first/last fallback frames into the snapshot (the user
    // explicitly placed the playhead, so they expect "what's
    // showing right now").
    if t < img_ov.t_in || t > img_ov.t_out {
        return;
    }
    let sample_t = t - img_ov.t_in;
    let mut ov_state = keyframe::sample(&img_ov.layout, sample_t).unwrap_or_default();

    // Animation modifiers (wobble / shake / pulse / spin) — additive
    // on top of the eased keyframe sample, exactly like the canvas.
    let mod_delta = keyframe::evaluate_modifiers(&img_ov.modifiers, sample_t);
    ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
    ov_state.rotation_deg += mod_delta.d_rotation_deg;

    // Sample animated effect-stack params at the overlay-local time so
    // animated effect parameters (intensity etc.) also bake in.
    let baked_effects: Vec<Effect> = img_ov
        .effects
        .iter()
        .map(|e| e.sampled_at(sample_t))
        .collect();

    // Decode the source picture.
    let raw = match image::open(&img_ov.source) {
        Ok(img) => img.to_rgba8(),
        Err(_) => return, // missing / undecodable file → skip silently
    };
    let src_w = raw.width();
    let src_h = raw.height();
    if src_w == 0 || src_h == 0 {
        return;
    }
    let mut buf: Vec<u8> = raw.into_raw();

    // Effect stack (returns crop inset in normalised units).
    let crop = image_effects::apply_effect_stack(&mut buf, src_w, src_h, &baked_effects, sample_t);

    // Apply crop by extracting a sub-image. This gives the snapshot
    // a true cropped picture (mirroring the export path), instead of
    // the canvas trick of shrinking the screen rect with UV insets.
    let (crop_left, crop_top, crop_right, crop_bottom) =
        (crop.0.max(0.0), crop.1.max(0.0), crop.2.max(0.0), crop.3.max(0.0));
    let l_px = (crop_left * src_w as f32).round() as u32;
    let t_px = (crop_top * src_h as f32).round() as u32;
    let r_px = ((1.0 - crop_right) * src_w as f32).round() as u32;
    let b_px = ((1.0 - crop_bottom) * src_h as f32).round() as u32;
    let r_px = r_px.max(l_px + 1).min(src_w);
    let b_px = b_px.max(t_px + 1).min(src_h);
    let cw = r_px - l_px;
    let ch = b_px - t_px;
    let layer = if cw == src_w && ch == src_h && (l_px, t_px) == (0, 0) {
        RgbaImage::from_raw(src_w, src_h, buf).expect("rgba buffer matches dims")
    } else {
        let full = RgbaImage::from_raw(src_w, src_h, buf).expect("rgba buffer matches dims");
        let mut sub = RgbaImage::new(cw, ch);
        for y in 0..ch {
            for x in 0..cw {
                let p = full.get_pixel(l_px + x, t_px + y);
                sub.put_pixel(x, y, *p);
            }
        }
        sub
    };

    // World position matches `canvas_preview::get_element_world_pos`.
    let world_pos = crate::canvas_preview::get_element_world_pos(
        state,
        &img_ov.id,
        &[],
        t,
    );
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    // Output dimensions: source-pixel size × scale × zoom (= world→
    // output pixels-per-unit). The canvas uses src_w as the picture's
    // intrinsic world-pixel size, then multiplies by ov_state.scale.
    // Crop shrinks the visible region — apply that to the output box
    // so the snapshot mirrors the canvas's UV inset behaviour exactly.
    let crop_w_factor = (1.0 - crop_left - crop_right).max(1.0e-3);
    let crop_h_factor = (1.0 - crop_top - crop_bottom).max(1.0e-3);
    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let out_w = (src_w as f32) * ov_state.scale * crop_w_factor * abs_fx * rf_state.zoom;
    let out_h =
        (src_h as f32) * ov_state.scale * ov_state.scale_y * crop_h_factor * abs_fy * rf_state.zoom;

    // Recentre after asymmetric crop — same offset the canvas applies
    // so the cropped sub-rect stays anchored to its untrimmed centre.
    let crop_dx_norm = (crop_left - crop_right) * 0.5;
    let crop_dy_norm = (crop_top - crop_bottom) * 0.5;
    let centre_offset_x = (src_w as f32) * ov_state.scale * abs_fx * rf_state.zoom * crop_dx_norm;
    let centre_offset_y =
        (src_h as f32) * ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom * crop_dy_norm;
    // Output rotation: layer rotation minus render-frame rotation
    // (because the camera/frame is itself tilted).
    let rotation_deg = ov_state.rotation_deg - rf_state.rotation_deg;
    let rotation_rad = rotation_deg.to_radians();
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let cx_final = cx + centre_offset_x * cos_r - centre_offset_y * sin_r;
    let cy_final = cy + centre_offset_x * sin_r + centre_offset_y * cos_r;

    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;

    paint_layer_rgba(
        canvas,
        &layer,
        cx_final,
        cy_final,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );
}

// ─── ACTOR ─────────────────────────────────────────────────────────

fn paint_actor(
    state: &mut EditorState,
    scene: &Scene,
    actor_idx: usize,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let actor = match scene.actors.get(actor_idx) {
        Some(a) => a,
        None => return,
    };
    if !actor.visible {
        return;
    }
    let scene_dur = scene.output.duration;
    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(scene_dur);
    if t < t_in || t > t_out {
        return;
    }

    // Sample the layout state and animation modifiers — same recipe
    // as canvas_preview's actor pass.
    let mut actor_state = keyframe::sample(&actor.layout, t).unwrap_or_default();
    let mod_delta = keyframe::evaluate_modifiers(&actor.modifiers, t - t_in);
    actor_state.scale = (actor_state.scale + mod_delta.d_scale).max(0.001);
    actor_state.rotation_deg += mod_delta.d_rotation_deg;

    // Pull the source-clip frame for this scene-time. We bypass the
    // egui texture cache and re-decode the JPEG directly so the
    // snapshot path doesn't need a `&Context` and doesn't fight the
    // live preview's texture handle.
    let speed = actor.speed.max(1.0e-4);
    let local_t = (t - t_in) * speed + actor.source_start;
    let (mut frame_buf, src_w, src_h) =
        match load_actor_frame_rgba(state, actor_idx, local_t) {
            Some(t) => t,
            None => return,
        };

    // Apply chroma key + colour correction + effect stack at the
    // actor's local-for-anim time (same time canvas_preview uses).
    let local_for_anim = (state.playhead - t_in).max(0.0);
    let cc = actor.color_correction.sampled_at(local_for_anim);
    let baked_effects: Vec<Effect> = actor
        .effects
        .iter()
        .map(|e| e.sampled_at(local_for_anim))
        .collect();
    apply_actor_processing(
        &mut frame_buf,
        src_w,
        src_h,
        &actor.chroma_key,
        &cc,
        &baked_effects,
    );

    let layer = match RgbaImage::from_raw(src_w, src_h, frame_buf) {
        Some(img) => img,
        None => return,
    };

    let world_pos = crate::canvas_preview::get_element_world_pos(
        state,
        &actor.id,
        &actor.layout,
        t,
    );
    let world_pos_with_mod = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos_with_mod, rf_state, rw, rh);

    // Static + animated flips combined the same way the canvas does.
    let combined_x = if actor.flip_horizontal {
        -actor_state.flip_x_anim
    } else {
        actor_state.flip_x_anim
    };
    let combined_y = actor_state.flip_y_anim;
    let flip_x = combined_x < 0.0;
    let flip_y = combined_y < 0.0;
    let abs_fx = combined_x.abs().max(0.02);
    let abs_fy = combined_y.abs().max(0.02);

    // Output dims: source-pixel × scale × zoom.
    let out_w = (src_w as f32) * actor_state.scale * abs_fx * rf_state.zoom;
    let out_h =
        (src_h as f32) * actor_state.scale * actor_state.scale_y * abs_fy * rf_state.zoom;
    let rotation_rad = (actor_state.rotation_deg - rf_state.rotation_deg).to_radians();

    paint_layer_rgba(
        canvas,
        &layer,
        cx,
        cy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        actor_state.opacity,
    );
}

/// Load the RGBA frame for an actor at clip-local time `local_t`.
///
/// Primary path: read from the pre-extracted frame cache (fast, no
/// subprocess spawn). Fallback: when the cache isn't ready yet (e.g.
/// extraction still in progress, or the actor was just added), we
/// invoke ffmpeg synchronously to pull a single frame — the same
/// approach `paint_video_overlay` uses. This ensures the "extract
/// selected layer" button always renders actors regardless of cache
/// state.
fn load_actor_frame_rgba(
    state: &EditorState,
    actor_idx: usize,
    local_t: f32,
) -> Option<(Vec<u8>, u32, u32)> {
    // Try the fast path first: pre-extracted frame cache.
    if let Some(fc) = state.frame_caches.get(actor_idx) {
        if fc.is_ready() && fc.frame_count > 0 {
            let frame_index = ((local_t * fc.fps).floor() as usize)
                .clamp(0, fc.frame_count.saturating_sub(1));
            // FrameCache writes 1-based 6-digit frame names: `000001.jpg`.
            let path = fc.cache_dir.join(format!("{:06}.jpg", frame_index + 1));
            if let Ok(img) = image::open(&path) {
                let rgba = img.to_rgba8();
                let w = rgba.width();
                let h = rgba.height();
                return Some((rgba.into_raw(), w, h));
            }
        }
    }

    // Fallback: extract a single frame via ffmpeg synchronously.
    // This covers the case where the frame cache hasn't finished
    // extracting (or doesn't exist at all for this actor).
    let actor = state.scene.actors.get(actor_idx)?;
    let source = &actor.source;
    // Handle looping: wrap seek time around source duration.
    let seek = if actor.loop_source {
        match probe_video_duration_sync(source) {
            Some(dur) if dur > 0.001 => local_t.rem_euclid(dur),
            _ => local_t.max(0.0),
        }
    } else {
        local_t.max(0.0)
    };
    extract_video_frame_sync(source, seek)
}

/// Apply chroma-key + colour-correction + effect-stack to an RGBA8
/// buffer in place. Routes through `video_cache::apply_effects_cpu` /
/// `apply_effect_stack_cpu` so the result matches the live preview
/// pixel-for-pixel.
fn apply_actor_processing(
    rgba: &mut Vec<u8>,
    w: u32,
    h: u32,
    ck: &ChromaKeyParams,
    cc: &ColorCorrection,
    effects: &[Effect],
) {
    use egui::ColorImage;
    let ci = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba);
    let mut processed = video_cache::apply_effects_cpu(&ci, ck, cc);
    if !effects.is_empty() {
        processed = video_cache::apply_effect_stack_cpu(&processed, effects);
    }
    // ColorImage stores premultiplied Color32. Round-trip back to
    // unmultiplied RGBA8 for `image::RgbaImage` — we lose a tiny bit
    // of precision on partial-alpha pixels, but actor frames are
    // either fully opaque (pre-keying) or fully transparent (post-
    // keying) the vast majority of the time so the visible impact
    // is negligible.
    rgba.clear();
    rgba.reserve(processed.pixels.len() * 4);
    for c in &processed.pixels {
        let a = c.a();
        if a == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
        } else {
            let scale = 255.0 / a as f32;
            let r = (c.r() as f32 * scale).min(255.0) as u8;
            let g = (c.g() as f32 * scale).min(255.0) as u8;
            let b = (c.b() as f32 * scale).min(255.0) as u8;
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
}

// ─── EFFECT LAYERS ─────────────────────────────────────────────────

/// Apply all active effect layers to the snapshot canvas. Mirrors the
/// CPU compositor's `apply_effect_layers` logic.
fn apply_snapshot_effect_layers(
    state: &EditorState,
    scene: &Scene,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let mut sorted: Vec<(i32, usize)> = scene
        .effect_layers
        .iter()
        .enumerate()
        .map(|(i, e)| (e.z_order, i))
        .collect();
    sorted.sort_by_key(|&(z, i)| (z, i));

    for (_, idx) in sorted {
        let fx_ov = &scene.effect_layers[idx];
        if t < fx_ov.t_in || t > fx_ov.t_out {
            continue;
        }
        if fx_ov.effects.is_empty() {
            continue;
        }
        let sample_t = t - fx_ov.t_in;
        let mut ov_state = keyframe::sample(&fx_ov.layout, sample_t).unwrap_or_default();
        let mod_delta = keyframe::evaluate_modifiers(&fx_ov.modifiers, sample_t);
        ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
        ov_state.rotation_deg += mod_delta.d_rotation_deg;

        let base_size = 200.0_f32;
        let world_w = base_size * ov_state.scale;
        let world_h = base_size * ov_state.scale * ov_state.scale_y;

        let world_pos = crate::canvas_preview::get_element_world_pos(state, &fx_ov.id, &[], t);
        let world_pos = WorldPos {
            x: world_pos.x + mod_delta.dx,
            y: world_pos.y + mod_delta.dy,
        };
        let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

        let half_w = world_w * rf_state.zoom * 0.5;
        let half_h = world_h * rf_state.zoom * 0.5;
        let x_min = ((cx - half_w).floor() as i32).max(0) as u32;
        let y_min = ((cy - half_h).floor() as i32).max(0) as u32;
        let x_max = ((cx + half_w).ceil() as i32).min(rw as i32 - 1).max(0) as u32;
        let y_max = ((cy + half_h).ceil() as i32).min(rh as i32 - 1).max(0) as u32;

        let region_w = x_max.saturating_sub(x_min) + 1;
        let region_h = y_max.saturating_sub(y_min) + 1;
        if region_w == 0 || region_h == 0 {
            continue;
        }

        // Extract region.
        let mut region = RgbaImage::new(region_w, region_h);
        for y in 0..region_h {
            for x in 0..region_w {
                let px = canvas.get_pixel(x_min + x, y_min + y);
                region.put_pixel(x, y, *px);
            }
        }

        // Apply effects via the image_effects module (same path as
        // image overlays use in the snapshot).
        let baked: Vec<Effect> = fx_ov
            .effects
            .iter()
            .map(|e| e.sampled_at(sample_t))
            .collect();
        let mut buf = region.into_raw();
        image_effects::apply_effect_stack(&mut buf, region_w, region_h, &baked, sample_t);
        let region = RgbaImage::from_raw(region_w, region_h, buf)
            .expect("effect layer region buffer matches dims");

        // Write back.
        for y in 0..region_h {
            for x in 0..region_w {
                let px = region.get_pixel(x, y);
                canvas.put_pixel(x_min + x, y_min + y, *px);
            }
        }
    }
}

// ─── COMMON HELPERS ────────────────────────────────────────────────

/// Sample the render-frame state with animation modifiers layered on
/// top — direct port of `canvas_preview::sample_render_frame_eased`.
fn sample_render_frame_eased(rf: &RenderFrame, t: f32) -> RenderFrameState {
    let mut s = keyframe::sample(&rf.layout, t).unwrap_or_default();
    if rf.modifiers.is_empty() {
        return s;
    }
    let delta = keyframe::evaluate_modifiers(&rf.modifiers, t);
    s.pos.x += delta.dx;
    s.pos.y += delta.dy;
    s.rotation_deg += delta.d_rotation_deg;
    if delta.d_scale.abs() > 1e-4 {
        // d_scale is linear; the frame's "zoom" behaves inversely so
        // a positive d_scale should *zoom out* (cover more area).
        let mult = (1.0 + delta.d_scale).max(1.0e-3);
        s.zoom = (s.zoom / mult).max(1.0e-3);
    }
    s
}

/// Map a world-pixel position to output-pixel coordinates inside the
/// render frame (top-left = (0,0), bottom-right = (rw, rh)). Honours
/// the render frame's rotation around its centre.
fn world_to_output(world: WorldPos, rf_state: &RenderFrameState, rw: u32, rh: u32) -> (f32, f32) {
    let dx = world.x - rf_state.pos.x;
    let dy = world.y - rf_state.pos.y;
    let theta = -rf_state.rotation_deg.to_radians();
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    let rx = dx * cos_t - dy * sin_t;
    let ry = dx * sin_t + dy * cos_t;
    let zoom = rf_state.zoom.max(1.0e-3);
    let ox = (rw as f32) * 0.5 + rx * zoom;
    let oy = (rh as f32) * 0.5 + ry * zoom;
    (ox, oy)
}

// ─── LAYER → CANVAS COMPOSITOR ─────────────────────────────────────
//
// Inverse-mapping affine sampler: for every output pixel inside the
// rotated bounding box we compute the corresponding (u, v) inside
// the layer image and bilinearly sample. Faster than forward-
// mapping every layer pixel (which would either leave gaps or need
// supersampling) and dead-simple.

fn paint_layer_rgba(
    canvas: &mut RgbaImage,
    layer: &RgbaImage,
    cx: f32,
    cy: f32,
    out_w: f32,
    out_h: f32,
    rotation_rad: f32,
    flip_x: bool,
    flip_y: bool,
    opacity: f32,
) {
    let lw = layer.width();
    let lh = layer.height();
    if lw == 0 || lh == 0 || out_w < 0.5 || out_h < 0.5 || opacity <= 1.0e-3 {
        return;
    }
    let cw = canvas.width() as i32;
    let ch = canvas.height() as i32;
    if cw == 0 || ch == 0 {
        return;
    }

    let half_w = out_w * 0.5;
    let half_h = out_h * 0.5;
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();

    // Axis-aligned bbox of the rotated rectangle.
    let extent_x = half_w * cos_r.abs() + half_h * sin_r.abs();
    let extent_y = half_w * sin_r.abs() + half_h * cos_r.abs();
    let x_min = ((cx - extent_x).floor() as i32).max(0);
    let x_max = ((cx + extent_x).ceil() as i32).min(cw - 1);
    let y_min = ((cy - extent_y).floor() as i32).max(0);
    let y_max = ((cy + extent_y).ceil() as i32).min(ch - 1);
    if x_min > x_max || y_min > y_max {
        return;
    }

    let inv_half_w = 1.0 / half_w;
    let inv_half_h = 1.0 / half_h;
    let lw_f = lw as f32;
    let lh_f = lh as f32;
    let opacity = opacity.clamp(0.0, 1.0);

    for y in y_min..=y_max {
        for x in x_min..=x_max {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            // Rotate by -rotation to land in the layer's local frame.
            let lx = dx * cos_r + dy * sin_r;
            let ly = -dx * sin_r + dy * cos_r;
            let mut nx = lx * inv_half_w;
            let mut ny = ly * inv_half_h;
            if flip_x {
                nx = -nx;
            }
            if flip_y {
                ny = -ny;
            }
            if nx < -1.0 || nx > 1.0 || ny < -1.0 || ny > 1.0 {
                continue;
            }
            let u = (nx + 1.0) * 0.5;
            let v = (ny + 1.0) * 0.5;
            let src_x = u * lw_f - 0.5;
            let src_y = v * lh_f - 0.5;
            let sample = sample_bilinear(layer, src_x, src_y);
            let src_a = (sample[3] as f32) * opacity;
            if src_a < 0.5 {
                continue;
            }
            // src-over with unpremultiplied colour.
            let p = canvas.get_pixel_mut(x as u32, y as u32);
            let dst_a = p[3] as f32;
            let src_a_n = src_a / 255.0;
            let inv_n = 1.0 - src_a_n;
            let out_a = src_a + dst_a * inv_n;
            if out_a < 1.0e-3 {
                continue;
            }
            let out_r =
                (sample[0] as f32 * src_a_n * 255.0 + p[0] as f32 * dst_a * inv_n) / out_a;
            let out_g =
                (sample[1] as f32 * src_a_n * 255.0 + p[1] as f32 * dst_a * inv_n) / out_a;
            let out_b =
                (sample[2] as f32 * src_a_n * 255.0 + p[2] as f32 * dst_a * inv_n) / out_a;
            *p = Rgba([
                out_r.clamp(0.0, 255.0) as u8,
                out_g.clamp(0.0, 255.0) as u8,
                out_b.clamp(0.0, 255.0) as u8,
                out_a.clamp(0.0, 255.0) as u8,
            ]);
        }
    }
}

/// Bilinear sample of an RGBA image at fractional `(x, y)` (0-based,
/// half-pixel-centred convention — `(0.0, 0.0)` is the centre of the
/// top-left pixel). Out-of-bounds coordinates clamp.
fn sample_bilinear(img: &RgbaImage, x: f32, y: f32) -> [u8; 4] {
    let w = img.width() as i32;
    let h = img.height() as i32;
    if w == 0 || h == 0 {
        return [0, 0, 0, 0];
    }
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = x0 + 1;
    let y1 = y0 + 1;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let get = |xx: i32, yy: i32| -> [f32; 4] {
        let cx = xx.clamp(0, w - 1) as u32;
        let cy = yy.clamp(0, h - 1) as u32;
        let p = img.get_pixel(cx, cy).0;
        [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
    };
    let p00 = get(x0, y0);
    let p10 = get(x1, y0);
    let p01 = get(x0, y1);
    let p11 = get(x1, y1);
    let w00 = (1.0 - fx) * (1.0 - fy);
    let w10 = fx * (1.0 - fy);
    let w01 = (1.0 - fx) * fy;
    let w11 = fx * fy;
    let blend = |i: usize| -> u8 {
        (p00[i] * w00 + p10[i] * w10 + p01[i] * w01 + p11[i] * w11)
            .clamp(0.0, 255.0) as u8
    };
    [blend(0), blend(1), blend(2), blend(3)]
}

// ─── IMAGE BACKGROUND ──────────────────────────────────────────────

/// Decode the still at `path` and composite it onto the full output
/// canvas using the supplied `fit` mode. Returns `false` when the
/// file can't be decoded (so the caller can record the skip in the
/// status message).
fn paint_image_background(
    canvas: &mut RgbaImage,
    path: &Path,
    fit: Fit,
    rw: u32,
    rh: u32,
) -> bool {
    let layer = match image::open(path) {
        Ok(img) => img.to_rgba8(),
        Err(_) => return false,
    };
    if layer.width() == 0 || layer.height() == 0 {
        return false;
    }
    composite_fit_image(canvas, &layer, fit, rw, rh);
    true
}

// ─── VIDEO BACKGROUND ──────────────────────────────────────────────

/// Pull a single frame from `path` at clip-local `t_local` (already
/// includes the `start_at` offset) and composite it onto the full
/// output canvas under `fit`. When `looping` is set the seek time
/// wraps around the source's probed duration so the snapshot
/// matches the export's loop semantics; when off, a seek past EOF
/// is treated as "skipped" instead of holding the last frame
/// (the export's `r#loop=false` clamps too).
fn paint_video_background(
    canvas: &mut RgbaImage,
    path: &Path,
    t_local: f32,
    looping: bool,
    fit: Fit,
    rw: u32,
    rh: u32,
) -> bool {
    let seek = if looping {
        match probe_video_duration_sync(path) {
            Some(dur) if dur > 0.001 => t_local.rem_euclid(dur),
            _ => t_local.max(0.0),
        }
    } else {
        t_local.max(0.0)
    };
    let (rgba, w, h) = match extract_video_frame_sync(path, seek) {
        Some(f) => f,
        None => return false,
    };
    let layer = match RgbaImage::from_raw(w, h, rgba) {
        Some(img) => img,
        None => return false,
    };
    composite_fit_image(canvas, &layer, fit, rw, rh);
    true
}

// ─── TEXT OVERLAY ──────────────────────────────────────────────────

/// Compose a text overlay at the playhead. Returns `true` when the
/// glyphs were drawn (or the layer was outside its visible window /
/// the body was empty — both are considered "successfully handled");
/// `false` when rasterisation failed (missing font, write error, …).
///
/// Text scale + flip + rotation come from `ov_state`. The PNG
/// produced by the rasterizer is anchored on the **text-block**
/// centre, not the plate centre — that anchor's offset relative to
/// the PNG's geometric centre is honoured here so an asymmetric
/// `box_extra_left/right` plate doesn't shift the visible text away
/// from the user's authored position.
fn paint_text_overlay(
    state: &EditorState,
    canvas: &mut RgbaImage,
    txt: &TextOverlay,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
) -> bool {
    if t < txt.t_in || t > txt.t_out {
        return true;
    }
    let sample_t = t - txt.t_in;
    let mut ov_state = keyframe::sample(&txt.layout, sample_t).unwrap_or_default();
    let mod_delta = keyframe::evaluate_modifiers(&txt.modifiers, sample_t);
    ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
    ov_state.rotation_deg += mod_delta.d_rotation_deg;

    // Route through the same rasteriser the MP4 export uses so the
    // snapshot matches the rendered video glyph-for-glyph.
    let raster = match memstroy_render::rasterize_text_overlay(txt, rw, rh) {
        Ok(Some(r)) => r,
        Ok(None) => return true, // empty body — nothing to draw
        Err(_) => return false,
    };
    let layer = match image::open(&raster.png_path).map(|i| i.to_rgba8()) {
        Ok(img) => img,
        Err(_) => {
            let _ = std::fs::remove_file(&raster.png_path);
            return false;
        }
    };
    let png_w = raster.width as f32;
    let png_h = raster.height as f32;
    if png_w < 1.0 || png_h < 1.0 {
        let _ = std::fs::remove_file(&raster.png_path);
        return true;
    }

    let world_pos = crate::canvas_preview::get_element_world_pos(
        state,
        &txt.id,
        &[],
        t,
    );
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let scale_x = ov_state.scale * abs_fx * rf_state.zoom;
    let scale_y = ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom;
    let out_w = png_w * scale_x;
    let out_h = png_h * scale_y;

    // Anchor offset inside the PNG, signed from the PNG centre.
    // Flipping the layer mirrors the visible content, so the anchor
    // moves with it — flip the offset sign before transforming.
    let mut local_dx = raster.anchor_dx_from_left - png_w * 0.5;
    let mut local_dy = raster.anchor_dy_from_top - png_h * 0.5;
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;
    if flip_x {
        local_dx = -local_dx;
    }
    if flip_y {
        local_dy = -local_dy;
    }
    let out_dx = local_dx * scale_x;
    let out_dy = local_dy * scale_y;

    let rotation_rad = (ov_state.rotation_deg - rf_state.rotation_deg).to_radians();
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let rot_dx = out_dx * cos_r - out_dy * sin_r;
    let rot_dy = out_dx * sin_r + out_dy * cos_r;

    paint_layer_rgba(
        canvas,
        &layer,
        cx - rot_dx,
        cy - rot_dy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );

    // Clean up the temp file the rasteriser dropped in /tmp.
    let _ = std::fs::remove_file(&raster.png_path);
    true
}

// ─── VIDEO OVERLAY ─────────────────────────────────────────────────

/// Compose a video overlay at the playhead — extract a single frame
/// from the source via ffmpeg, optionally apply chroma key + the
/// effect stack, then composite under the layer's keyframed
/// transform. Returns `false` when ffmpeg can't deliver a frame.
fn paint_video_overlay(
    state: &EditorState,
    canvas: &mut RgbaImage,
    vid: &VideoOverlay,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
) -> bool {
    if t < vid.t_in || t > vid.t_out {
        return true;
    }
    let sample_t = t - vid.t_in;
    let mut ov_state = keyframe::sample(&vid.layout, sample_t).unwrap_or_default();
    let mod_delta = keyframe::evaluate_modifiers(&vid.modifiers, sample_t);
    ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
    ov_state.rotation_deg += mod_delta.d_rotation_deg;

    // Source-clip seek time. `speed > 1.0` advances the source faster
    // than the timeline (matching `Actor::speed` semantics); looping
    // wraps the seek modulo the source duration so the user gets the
    // same frame the export would produce.
    let speed = vid.speed.max(1.0e-4);
    let raw_seek = sample_t * speed + vid.source_start;
    let seek = if vid.loop_source {
        match probe_video_duration_sync(&vid.source) {
            Some(dur) if dur > 0.001 => raw_seek.rem_euclid(dur),
            _ => raw_seek.max(0.0),
        }
    } else {
        raw_seek.max(0.0)
    };
    let (mut frame_buf, src_w, src_h) =
        match extract_video_frame_sync(&vid.source, seek) {
            Some(f) => f,
            None => return false,
        };

    // Apply chroma key + effect stack only when the user has actually
    // set them — bypassing the per-pixel CPU loop entirely on a plain
    // overlay matches the image-overlay path's "no-op when None" rule
    // and avoids a needless premultiplied round-trip.
    let baked_effects: Vec<Effect> =
        vid.effects.iter().map(|e| e.sampled_at(sample_t)).collect();
    if vid.chroma_key.is_some() || !baked_effects.is_empty() {
        let ck = vid.chroma_key.clone().unwrap_or_else(ChromaKeyParams::default);
        let cc = ColorCorrection::default();
        apply_actor_processing(
            &mut frame_buf, src_w, src_h, &ck, &cc, &baked_effects,
        );
    }

    let layer = match RgbaImage::from_raw(src_w, src_h, frame_buf) {
        Some(img) => img,
        None => return false,
    };

    let world_pos = crate::canvas_preview::get_element_world_pos(
        state,
        &vid.id,
        &[],
        t,
    );
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let out_w = (src_w as f32) * ov_state.scale * abs_fx * rf_state.zoom;
    let out_h =
        (src_h as f32) * ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom;
    let rotation_rad = (ov_state.rotation_deg - rf_state.rotation_deg).to_radians();
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;

    paint_layer_rgba(
        canvas,
        &layer,
        cx,
        cy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );
    true
}

// ─── FFMPEG HELPERS ────────────────────────────────────────────────

/// Probe the duration (in seconds) of the video at `path` via
/// ffprobe. Returns `None` when ffprobe isn't on `PATH` or the
/// probe fails.
fn probe_video_duration_sync(path: &Path) -> Option<f32> {
    let ffmpeg = memstroy_render::ffmpeg_binary();
    let mut ffprobe = ffmpeg.clone();
    ffprobe.set_file_name("ffprobe");
    if !ffprobe.exists() {
        ffprobe = std::path::PathBuf::from("ffprobe");
    }
    let out = {
        let mut cmd = std::process::Command::new(&ffprobe);
        cmd.args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path);
        memstroy_render::hide_console_std(&mut cmd).output().ok()?
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<f32>().ok()
}

/// Extract a single PNG frame at `time_secs` from `path` via ffmpeg
/// and return it as raw RGBA8 bytes. Returns `None` on any failure
/// — caller folds that into the skip counter so the user gets a
/// clean status message instead of a hard error.
///
/// The temp PNG written by ffmpeg is removed before returning,
/// regardless of success.
fn extract_video_frame_sync(path: &Path, time_secs: f32) -> Option<(Vec<u8>, u32, u32)> {
    let ffmpeg = memstroy_render::ffmpeg_binary();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let out_path = std::env::temp_dir()
        .join(format!("memstroy_snapshot_frame_{}_{}.png", pid, stamp));

    // `-ss` AFTER `-i` performs an accurate decode-up-to seek instead
    // of the demuxer's keyframe-based fast seek — we want the exact
    // frame the user is parked on, not the closest preceding keyframe.
    let status = {
        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(path)
            .args(["-ss", &format!("{:.3}", time_secs.max(0.0))])
            .args(["-frames:v", "1", "-vcodec", "png"])
            .arg(&out_path);
        memstroy_render::hide_console_std(&mut cmd).status()
    };

    let ok = matches!(status, Ok(s) if s.success());
    let result = if ok {
        image::open(&out_path).ok().map(|img| {
            let rgba = img.to_rgba8();
            let w = rgba.width();
            let h = rgba.height();
            (rgba.into_raw(), w, h)
        })
    } else {
        None
    };
    let _ = std::fs::remove_file(&out_path);
    result
}

// ─── FIT-COMPOSITE ─────────────────────────────────────────────────

/// Composite `layer` onto the full `rw × rh` output canvas applying
/// the supplied `Fit` mode. Mirrors the export filter graph's
/// `scale` + `crop`/`pad` pipeline:
///
/// - **Cover** — scale up to cover the output, crop overflow.
/// - **Contain** — scale down to fit, leave the canvas's existing
///   pixels untouched outside the picture (= letterboxing using
///   the scene's `background_color` in full-frame mode, or a
///   transparent margin in subset mode).
/// - **Stretch** — scale to exactly the output dimensions.
/// - **Original** — keep the source resolution and centre it.
///
/// The render frame's own rotation is intentionally not applied —
/// backgrounds fill the rendered output uniformly the same way the
/// MP4 export pipeline produces them.
fn composite_fit_image(canvas: &mut RgbaImage, layer: &RgbaImage, fit: Fit, rw: u32, rh: u32) {
    let lw = layer.width() as f32;
    let lh = layer.height() as f32;
    if lw < 1.0 || lh < 1.0 || rw == 0 || rh == 0 {
        return;
    }
    let (out_w, out_h) = match fit {
        Fit::Stretch => (rw as f32, rh as f32),
        Fit::Original => (lw, lh),
        Fit::Cover => {
            let s = (rw as f32 / lw).max(rh as f32 / lh);
            (lw * s, lh * s)
        }
        Fit::Contain => {
            let s = (rw as f32 / lw).min(rh as f32 / lh);
            (lw * s, lh * s)
        }
    };
    let cx = rw as f32 * 0.5;
    let cy = rh as f32 * 0.5;
    paint_layer_rgba(
        canvas, layer, cx, cy, out_w, out_h, 0.0, false, false, 1.0,
    );
}

// Suppress unused-import warning when `Path` only appears in the
// public docs — keep the import explicit to make the file portable
// if more disk-touching helpers land here.
#[allow(dead_code)]
const _PATH: Option<&Path> = None;
