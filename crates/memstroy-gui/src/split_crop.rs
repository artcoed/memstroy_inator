//! Shared helpers for timeline / layer razor splits: trim clip windows and
//! crop every keyframe track that uses scene-time or clip-local time.

use memstroy_core::{
    keyframe::{self, Keyframe, TrackModifier},
    Actor, ActorState, Overlay, Scene,
};

const EPS: f32 = 1.0e-3;

/// Keep keyframes on one side of a scene-time cut and optionally rebase times.
pub fn crop_scene_time_kfs<T: Clone>(
    kfs: &mut Vec<Keyframe<T>>,
    keep: impl Fn(f32) -> bool,
    rebase_sub: Option<f32>,
) {
    kfs.retain(|kf| keep(kf.t));
    if let Some(sub) = rebase_sub {
        for kf in kfs.iter_mut() {
            kf.t = (kf.t - sub).max(0.0);
        }
    }
    if kfs.is_empty() {
        return;
    }
    kfs.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

/// Trim the left half's `canvas_layouts` keyframes at scene-time `cut_t`.
pub fn crop_canvas_layouts_left(scene: &mut Scene, element_id: &str, cut_t: f32) {
    let Some(cl) = scene
        .canvas_layouts
        .iter_mut()
        .find(|cl| cl.element_id == element_id)
    else {
        return;
    };
    cl.keyframes.retain(|kf| kf.t <= cut_t + EPS);
}

/// Duplicate canvas layout for a new element id (right half of a split).
pub fn duplicate_canvas_layout_for_split(
    scene: &mut Scene,
    from_id: &str,
    to_id: &str,
    cut_t: f32,
) {
    let Some(src) = scene
        .canvas_layouts
        .iter()
        .find(|cl| cl.element_id == from_id)
        .cloned()
    else {
        return;
    };
    let sample_at_cut = keyframe::sample(&src.keyframes, cut_t);
    let mut right_cl = src;
    right_cl.element_id = to_id.to_string();
    // Canvas layout keyframes use scene time — keep absolute `t`, only trim.
    right_cl.keyframes.retain(|kf| kf.t >= cut_t - EPS);
    if right_cl.keyframes.is_empty() {
        if let Some(v) = sample_at_cut {
            right_cl.keyframes.push(Keyframe::new(cut_t, v));
        }
    }
    scene.canvas_layouts.push(right_cl);
    if let Some(left) = scene.canvas_layouts.iter_mut().find(|cl| cl.element_id == from_id) {
        left.keyframes.retain(|kf| kf.t <= cut_t + EPS);
    }
}

fn crop_modifiers_scene(modifiers: &mut Vec<TrackModifier>, cut_t: f32, right_half: bool) {
    if right_half {
        modifiers.retain(|m| m.t_end >= cut_t - EPS);
        for m in modifiers.iter_mut() {
            m.t_start = (m.t_start - cut_t).max(0.0);
            if m.t_end.is_finite() {
                m.t_end = (m.t_end - cut_t).max(m.t_start);
            }
        }
    } else {
        modifiers.retain(|m| m.t_start <= cut_t + EPS);
        for m in modifiers.iter_mut() {
            if m.t_end.is_finite() {
                m.t_end = m.t_end.min(cut_t);
            }
        }
    }
}

fn crop_modifiers_local(modifiers: &mut Vec<TrackModifier>, local_cut: f32, right_half: bool) {
    if right_half {
        modifiers.retain(|m| m.t_end >= local_cut - EPS);
        for m in modifiers.iter_mut() {
            m.t_start = (m.t_start - local_cut).max(0.0);
            if m.t_end.is_finite() {
                m.t_end = (m.t_end - local_cut).max(m.t_start);
            }
        }
    } else {
        modifiers.retain(|m| m.t_start <= local_cut + EPS);
        for m in modifiers.iter_mut() {
            if m.t_end.is_finite() {
                m.t_end = m.t_end.min(local_cut);
            }
        }
    }
}

pub fn crop_actor_timeline(actor: &mut Actor, cut_t: f32, right_half: bool) {
    if right_half {
        crop_scene_time_kfs(&mut actor.layout, |t| t >= cut_t - EPS, None);
    } else {
        crop_scene_time_kfs(&mut actor.layout, |t| t <= cut_t + EPS, None);
    }
    crop_modifiers_scene(&mut actor.modifiers, cut_t, right_half);
}

pub fn crop_overlay_timeline(overlay: &mut Overlay, cut_t: f32, right_half: bool) {
    let (t_in, t_out) = match overlay {
        Overlay::Text(t) => (t.t_in, t.t_out),
        Overlay::Image(im) => (im.t_in, im.t_out),
        Overlay::Video(v) => (v.t_in, v.t_out),
    };
    let local_cut = (cut_t - t_in).max(0.0);
    let local_end = (t_out - t_in).max(0.0);

    let crop_layout = |layout: &mut Vec<Keyframe<memstroy_core::OverlayState>>| {
        if right_half {
            layout.retain(|kf| kf.t >= local_cut - EPS);
            for kf in layout.iter_mut() {
                kf.t = (kf.t - local_cut).max(0.0);
            }
        } else {
            layout.retain(|kf| kf.t <= local_cut + EPS);
        }
    };

    match overlay {
        Overlay::Text(t) => {
            if right_half {
                t.t_in = cut_t;
                t.t_out = t_out;
            } else {
                t.t_out = cut_t;
            }
            crop_layout(&mut t.layout);
            crop_modifiers_local(&mut t.modifiers, local_cut, right_half);
        }
        Overlay::Image(im) => {
            if right_half {
                im.t_in = cut_t;
            } else {
                im.t_out = cut_t;
            }
            crop_layout(&mut im.layout);
            crop_modifiers_local(&mut im.modifiers, local_cut, right_half);
        }
        Overlay::Video(v) => {
            if right_half {
                v.t_in = cut_t;
                v.source_start = v.source_start + local_cut;
            } else {
                v.t_out = cut_t;
            }
            crop_layout(&mut v.layout);
            crop_modifiers_local(&mut v.modifiers, local_cut, right_half);
        }
    }
    let _ = local_end;
}

/// After inserting the right-half actor next to the left half, trim both
/// timeline tracks and split any shared `canvas_layouts` row.
pub fn finish_actor_split(
    scene: &mut Scene,
    left_idx: usize,
    right_idx: usize,
    cut_left: f32,
    cut_right: f32,
) {
    if left_idx >= scene.actors.len() || right_idx >= scene.actors.len() {
        return;
    }
    let left_id = scene.actors[left_idx].id.clone();
    let right_id = scene.actors[right_idx].id.clone();
    if let Some(a) = scene.actors.get_mut(left_idx) {
        crop_actor_timeline(a, cut_left, false);
    }
    if let Some(a) = scene.actors.get_mut(right_idx) {
        crop_actor_timeline(a, cut_right, true);
    }
    for idx in [left_idx, right_idx] {
        if let Some(a) = scene.actors.get_mut(idx) {
            if a.layout.is_empty() {
                let seed_t = a.t_in.unwrap_or(0.0);
                a.layout
                    .push(Keyframe::new(seed_t, ActorState::default()));
            }
        }
    }
    crop_canvas_layouts_left(scene, &left_id, cut_left);
    duplicate_canvas_layout_for_split(scene, &left_id, &right_id, cut_right);
}

/// Trim overlay halves after a razor split (clip windows + layout + modifiers).
pub fn finish_overlay_split(
    scene: &mut Scene,
    left_idx: usize,
    right_idx: usize,
    cut_left: f32,
    cut_right: f32,
) {
    if left_idx >= scene.overlays.len() || right_idx >= scene.overlays.len() {
        return;
    }
    let left_id = match &scene.overlays[left_idx] {
        Overlay::Text(t) => t.id.clone(),
        Overlay::Image(im) => im.id.clone(),
        Overlay::Video(v) => v.id.clone(),
    };
    let right_id = match &scene.overlays[right_idx] {
        Overlay::Text(t) => t.id.clone(),
        Overlay::Image(im) => im.id.clone(),
        Overlay::Video(v) => v.id.clone(),
    };
    if let Some(ov) = scene.overlays.get_mut(left_idx) {
        crop_overlay_timeline(ov, cut_left, false);
    }
    if let Some(ov) = scene.overlays.get_mut(right_idx) {
        crop_overlay_timeline(ov, cut_right, true);
    }
    crop_canvas_layouts_left(scene, &left_id, cut_left);
    duplicate_canvas_layout_for_split(scene, &left_id, &right_id, cut_right);
}
