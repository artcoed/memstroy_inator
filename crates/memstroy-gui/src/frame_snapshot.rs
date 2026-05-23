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
//!   selection): every visible layer at `t` is composited and the
//!   output is filled with `scene.output.background_color`.
//! - **Subset** (when `state.canvas_selection` is non-empty, or when
//!   `state.selection` points at a single element): only the listed
//!   layers are composited and the canvas starts transparent so the
//!   user gets a "copy these layers as if isolated" snapshot.
//!
//! In both modes the resulting RGBA is saved as
//! `assets/images/frame_<unix-millis>.png` and a fresh
//! `Overlay::Image` layer is added at the playhead pointing at it,
//! mirroring the existing Ctrl+V "paste image from clipboard" flow.
//!
//! ## What's covered (v1)
//!
//! - Image overlays — full effect stack (blur, hue shift, crop, mask,
//!   colour-key, …), opacity, scale, rotation, flip.
//! - Actors — chroma key + colour correction + effect stack, opacity,
//!   scale, rotation, flip (static `flip_horizontal` plus animated
//!   `flip_x_anim` / `flip_y_anim`).
//! - Backgrounds — solid colour fill across the render frame.
//!
//! ## What's deferred (v1)
//!
//! - Text overlays — `memstroy_render::rasterize_text_overlay` would
//!   slot in here but adds a font-loading dependency we're keeping
//!   out of the snapshot path for now. They are silently skipped
//!   when present in the subset (and a status message reports the
//!   skip count).
//! - Video overlays — preview frame caches are indexed per actor
//!   today; video overlays don't have one yet, so they're skipped
//!   the same way as text overlays.
//! - Image / video backgrounds — only solid-colour backgrounds are
//!   painted; image/video backgrounds are skipped.
//!
//! Both deferrals are documented in the status string the user sees
//! after extraction so they aren't surprised by a "missing" layer.

use std::path::Path;

use image::{Rgba, RgbaImage};
use memstroy_core::{
    canvas::WorldPos, effects::Effect, keyframe, ChromaKeyParams, ColorCorrection,
    ImageOverlay, MediaSource, Overlay, RenderFrame, RenderFrameState, Scene,
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
        return Err("render frame has zero resolution".into());
    }

    // ── Save into project library + spawn the new overlay ────────
    let asset = state.save_snapshot_image_to_library(img.as_raw(), w, h)?;
    let idx = state.add_image_overlay_at_playhead(&asset);

    // Status message — mention any layers we had to skip so the user
    // isn't surprised when they look at the resulting picture.
    let mut status = if full_frame {
        format!(
            "\u{1F4F8} Frame extracted as image layer '{}' ({}\u{00D7}{}).",
            asset.id, w, h
        )
    } else {
        format!(
            "\u{1F4F8} Selected layers extracted as image layer '{}' ({}\u{00D7}{}).",
            asset.id, w, h
        )
    };
    if summary.skipped_text > 0 || summary.skipped_video > 0 || summary.skipped_image_bg > 0 {
        status.push_str(" Skipped:");
        if summary.skipped_text > 0 {
            status.push_str(&format!(" {} text", summary.skipped_text));
        }
        if summary.skipped_video > 0 {
            status.push_str(&format!(" {} video overlay", summary.skipped_video));
        }
        if summary.skipped_image_bg > 0 {
            status.push_str(&format!(" {} image background", summary.skipped_image_bg));
        }
        status.push('.');
    }
    state.status = status;
    Ok(idx)
}

// ─── COMPOSITOR ────────────────────────────────────────────────────

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
    let [rw, rh] = rf.resolution;
    let rw = rw.max(1);
    let rh = rh.max(1);
    let rf_state = sample_render_frame_eased(rf, t);

    // Output canvas. Full-frame mode: pre-fill with scene background
    // colour at full alpha. Subset mode: stay transparent so the
    // copied layers can later be overlaid on something else.
    let mut canvas = if full_frame {
        let [r, g, b] = scene_clone.output.background_color;
        RgbaImage::from_pixel(rw, rh, Rgba([r, g, b, 255]))
    } else {
        RgbaImage::new(rw, rh)
    };

    let mut summary = CompositeSummary::default();

    // ── Pass 1: backgrounds ──────────────────────────────────────
    //
    // Only paint backgrounds in full-frame mode, OR when the user has
    // explicitly added a Background to the subset. In both cases we
    // render the same way the canvas does (solid colour fill across
    // the render frame; image/video bg skipped for v1).
    let bg_iter: Vec<usize> = if full_frame {
        (0..scene_clone.backgrounds.len()).collect()
    } else {
        subset
            .iter()
            .filter_map(|s| match s {
                Selection::Background(i) => Some(*i),
                _ => None,
            })
            .collect()
    };
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
                paint_solid_background(&mut canvas, *color, &rf_state, rw, rh);
            }
            MediaSource::Image { .. } | MediaSource::Video { .. } => {
                summary.skipped_image_bg += 1;
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
                            &mut canvas,
                            &img_ov,
                            &rf_state,
                            rw,
                            rh,
                            t,
                        );
                    }
                    Overlay::Text(_) => {
                        summary.skipped_text += 1;
                    }
                    Overlay::Video(_) => {
                        summary.skipped_video += 1;
                    }
                }
            }
            PaintOp::Actor(ai) => {
                paint_actor(state, &scene_clone, ai, &rf_state, rw, rh, t, &mut canvas);
            }
        }
    }

    summary.image = canvas;
    summary
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

    // Compute the layer's centre and effective output size.
    let world_w = (rw as f32) / rf_state.zoom.max(1.0e-3);
    let world_h = (rh as f32) / rf_state.zoom.max(1.0e-3);
    let frame_tl_x = rf_state.pos.x - world_w * 0.5;
    let frame_tl_y = rf_state.pos.y - world_h * 0.5;
    let world_pos = WorldPos {
        x: frame_tl_x + ov_state.pos[0] * world_w + mod_delta.dx,
        y: frame_tl_y + ov_state.pos[1] * world_h + mod_delta.dy,
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

    // World position from canvas_layouts (free canvas v2) or legacy
    // normalised layout. We mirror `canvas_preview::get_element_world_pos`
    // here, but skip the skeleton-attachment path — actors with
    // attachments are an advanced editor feature and the snapshot
    // can fall back to the keyframed centre.
    let world_pos = element_world_pos(scene, &actor.id, t).unwrap_or_else(|| {
        let world_w = (rw as f32) / rf_state.zoom.max(1.0e-3);
        let world_h = (rh as f32) / rf_state.zoom.max(1.0e-3);
        let frame_tl_x = rf_state.pos.x - world_w * 0.5;
        let frame_tl_y = rf_state.pos.y - world_h * 0.5;
        let layout_state = keyframe::sample(&actor.layout, t).unwrap_or_default();
        WorldPos {
            x: frame_tl_x + layout_state.pos[0] * world_w,
            y: frame_tl_y + layout_state.pos[1] * world_h,
        }
    });
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

/// Look up an actor's world position from `canvas_layouts` (free canvas
/// v2). Returns `None` when no entry exists — caller falls back to the
/// legacy normalised `layout`. Mirrors the relevant branch of
/// `canvas_preview::get_element_world_pos` minus the skeleton path.
fn element_world_pos(scene: &Scene, element_id: &str, t: f32) -> Option<WorldPos> {
    let cl = scene
        .canvas_layouts
        .iter()
        .find(|cl| cl.element_id == element_id)?;
    let transform = keyframe::sample(&cl.keyframes, t)?;
    Some(transform.pos)
}

/// Load the JPEG at the actor's frame-cache path corresponding to
/// `local_t`. Returns `(rgba, w, h)` or `None` when the cache isn't
/// ready (frames haven't finished extracting yet).
fn load_actor_frame_rgba(
    state: &EditorState,
    actor_idx: usize,
    local_t: f32,
) -> Option<(Vec<u8>, u32, u32)> {
    let fc = state.frame_caches.get(actor_idx)?;
    if !fc.is_ready() || fc.frame_count == 0 {
        return None;
    }
    let frame_index = ((local_t * fc.fps).floor() as usize)
        .clamp(0, fc.frame_count.saturating_sub(1));
    // FrameCache writes 1-based 6-digit frame names: `000001.jpg`.
    let path = fc.cache_dir.join(format!("{:06}.jpg", frame_index + 1));
    let img = image::open(&path).ok()?.to_rgba8();
    let w = img.width();
    let h = img.height();
    Some((img.into_raw(), w, h))
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

// Suppress unused-import warning when `Path` only appears in the
// public docs — keep the import explicit to make the file portable
// if more disk-touching helpers land here.
#[allow(dead_code)]
const _PATH: Option<&Path> = None;
