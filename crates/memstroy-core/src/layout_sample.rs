//! Per-parameter layout sampling.
//!
//! Actor / overlay / render-frame layouts store `Vec<Keyframe<State>>`
//! where each keyframe carries the full state snapshot. Naïvely
//! interpolating the whole struct ties every parameter to the same
//! keyframe times. These helpers sample each scalar field on its own
//! time track (like audio `volume_kfs` or effect `param_kfs`).

use std::collections::BTreeSet;

use crate::keyframe::{self, Keyframe};
use crate::param_ids;
use crate::{ActorState, CanvasTransform, OverlayState, RenderFrameState};

const EPS: f32 = 1.0e-4;
const TIME_EPS: f32 = 1.0 / 120.0;

/// Keyframe times that matter for one scalar field (timeline diamonds,
/// curve editor points). Omits `layout[0]` unless it starts a change
/// segment or is the only keyframe — avoids a spurious diamond at
/// `t = 0` when the user authored a single keyframe at the playhead.
pub fn param_change_times<T, F>(layout: &[Keyframe<T>], get: F) -> Vec<f32>
where
    F: Fn(&T) -> f32,
{
    param_change_times_with_owners(layout, get, |_| false)
}

fn param_change_times_with_owners<T, F>(
    layout: &[Keyframe<T>],
    get: F,
    owns_keyframe: impl Fn(usize) -> bool,
) -> Vec<f32>
where
    F: Fn(&T) -> f32,
{
    if layout.is_empty() {
        return Vec::new();
    }
    if layout.len() == 1 {
        return if layout[0].t.abs() < TIME_EPS {
            Vec::new()
        } else {
            vec![layout[0].t]
        };
    }
    let mut times = Vec::new();
    for i in 1..layout.len() {
        if (get(&layout[i].value) - get(&layout[i - 1].value)).abs() > EPS {
            if times.is_empty() {
                if i == 1 || owns_keyframe(i - 1) {
                    times.push(layout[i - 1].t);
                }
            }
            times.push(layout[i].t);
        }
    }
    if times.is_empty() {
        return if layout[0].t.abs() < TIME_EPS {
            Vec::new()
        } else {
            vec![layout[0].t]
        };
    }
    times
}

/// Like [`param_change_times`], but also includes layout breakpoints where
/// this field is flat yet no *other* animated parameter changed at the
/// same layout keyframe — e.g. "enable animation" at the playhead.
pub fn param_timeline_times<T, F>(
    layout: &[Keyframe<T>],
    get: F,
    other_fields_changed_at: impl Fn(usize) -> bool,
) -> Vec<f32>
where
    F: Fn(&T) -> f32 + Copy,
{
    param_timeline_times_with_owners(layout, get, other_fields_changed_at, |_| true)
}

pub fn param_timeline_times_with_owners<T, F>(
    layout: &[Keyframe<T>],
    get: F,
    other_fields_changed_at: impl Fn(usize) -> bool,
    owns_flat_keyframe: impl Fn(usize) -> bool,
) -> Vec<f32>
where
    F: Fn(&T) -> f32 + Copy,
{
    if layout.is_empty() {
        return Vec::new();
    }
    if layout.len() == 1 {
        return if owns_flat_keyframe(0) {
            vec![layout[0].t]
        } else {
            Vec::new()
        };
    }
    let param_has_value_change = layout
        .windows(2)
        .any(|w| (get(&w[1].value) - get(&w[0].value)).abs() > EPS);
    let mut times = Vec::new();
    for i in 1..layout.len() {
        let t = layout[i].t;
        let v_prev = get(&layout[i - 1].value);
        let v_cur = get(&layout[i].value);
        if (v_cur - v_prev).abs() > EPS {
            if times.is_empty() {
                times.push(layout[i - 1].t);
            }
            times.push(t);
        } else if !other_fields_changed_at(i) && !param_has_value_change && owns_flat_keyframe(i) {
            // Constant param newly enabled at the playhead — show a
            // diamond even though the value matches the previous kf. Full-state
            // layout snapshots share values across every param, so only the
            // param that explicitly authored this flat keyframe may claim it.
            times.push(t);
        }
    }
    if times.is_empty() {
        return if owns_flat_keyframe(0) {
            vec![layout[0].t]
        } else {
            Vec::new()
        };
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < TIME_EPS);
    times
}

pub fn actor_param_timeline_times(
    layout: &[Keyframe<ActorState>],
    _animated_params: &BTreeSet<String>,
    param_id: &str,
) -> Vec<f32> {
    owned_param_times(layout, |s| actor_get(s, param_id), param_id)
}

pub fn overlay_param_timeline_times(
    layout: &[Keyframe<OverlayState>],
    _animated_params: &BTreeSet<String>,
    param_id: &str,
) -> Vec<f32> {
    owned_param_times(layout, |s| overlay_get(s, param_id), param_id)
}

pub fn render_frame_param_timeline_times(
    layout: &[Keyframe<RenderFrameState>],
    _animated_params: &BTreeSet<String>,
    param_id: &str,
) -> Vec<f32> {
    owned_param_times(layout, |s| rf_get(s, param_id), param_id)
}

fn layout_index_at_time<T>(layout: &[Keyframe<T>], t: f32) -> usize {
    layout
        .iter()
        .position(|kf| (kf.t - t).abs() < TIME_EPS)
        .unwrap_or(0)
}

fn keyframe_relevant_to_param<T>(kf: &Keyframe<T>, param_id: &str) -> bool {
    kf.param_owners.is_empty() || kf.param_owners.contains(param_id)
}

fn owned_param_times<T, F>(layout: &[Keyframe<T>], get: F, param_id: &str) -> Vec<f32>
where
    F: Fn(&T) -> f32 + Copy,
{
    if layout.is_empty() {
        return Vec::new();
    }

    let mut explicit: Vec<f32> = layout
        .iter()
        .filter_map(|kf| kf.param_owners.contains(param_id).then_some(kf.t))
        .collect();
    if !explicit.is_empty() {
        explicit.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        explicit.dedup_by(|a, b| (*a - *b).abs() < TIME_EPS);
        return explicit;
    }

    let relevant: Vec<usize> = layout
        .iter()
        .enumerate()
        .filter_map(|(i, kf)| keyframe_relevant_to_param(kf, param_id).then_some(i))
        .collect();
    if relevant.is_empty() {
        return Vec::new();
    }

    if relevant.len() == 1 {
        let kf = &layout[relevant[0]];
        return if kf.param_owners.contains(param_id) || kf.t.abs() >= TIME_EPS {
            vec![kf.t]
        } else {
            Vec::new()
        };
    }

    let param_has_value_change = relevant.windows(2).any(|pair| {
        let a = pair[0];
        let b = pair[1];
        (get(&layout[b].value) - get(&layout[a].value)).abs() > EPS
    });

    let mut times = Vec::new();
    for pair in relevant.windows(2) {
        let prev_i = pair[0];
        let cur_i = pair[1];
        let prev = &layout[prev_i];
        let cur = &layout[cur_i];
        if (get(&cur.value) - get(&prev.value)).abs() > EPS {
            if times.is_empty() && prev_i == relevant[0] {
                times.push(prev.t);
            }
            times.push(cur.t);
        } else if !param_has_value_change && cur.param_owners.contains(param_id) {
            times.push(cur.t);
        }
    }

    if times.is_empty() {
        let first = &layout[relevant[0]];
        if first.param_owners.contains(param_id) || first.t.abs() >= TIME_EPS {
            times.push(first.t);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < TIME_EPS);
    times
}

/// Build a scalar track for one field. A single keyframe ⇒ constant value.
fn sample_scalar_track<T, F>(
    layout: &[Keyframe<T>],
    times: &[f32],
    t: f32,
    get: F,
    fallback: f32,
) -> f32
where
    F: Fn(&T) -> f32,
{
    if layout.is_empty() {
        return fallback;
    }
    if times.is_empty() {
        return get(&layout[0].value);
    }
    if times.len() == 1 {
        let idx = layout_index_at_time(layout, times[0]);
        if t < times[0] - TIME_EPS {
            return get(&layout[0].value);
        }
        return get(&layout[idx].value);
    }
    let track: Vec<Keyframe<f32>> = times
        .iter()
        .map(|&tm| {
            let idx = layout_index_at_time(layout, tm);
            Keyframe {
                t: tm,
                value: get(&layout[idx].value),
                easing: layout[idx].easing,
                param_owners: Default::default(),
            }
        })
        .collect();
    keyframe::sample(&track, t).unwrap_or(fallback)
}

fn sample_actor_field(
    layout: &[Keyframe<ActorState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    get: impl Fn(&ActorState) -> f32,
    default: f32,
) -> f32 {
    if animated_params.contains(param_id) {
        let times = owned_param_times(layout, &get, param_id);
        sample_scalar_track(layout, &times, t, &get, default)
    } else {
        layout.first().map(|kf| get(&kf.value)).unwrap_or(default)
    }
}

/// Sample an actor layout with independent per-parameter interpolation.
pub fn sample_actor_layout(
    layout: &[Keyframe<ActorState>],
    animated_params: &BTreeSet<String>,
    t: f32,
) -> ActorState {
    if layout.is_empty() {
        return ActorState::default();
    }
    ActorState {
        pos: [
            sample_actor_field(
                layout,
                animated_params,
                param_ids::POS_X,
                t,
                |s| s.pos[0],
                layout[0].value.pos[0],
            ),
            sample_actor_field(
                layout,
                animated_params,
                param_ids::POS_Y,
                t,
                |s| s.pos[1],
                layout[0].value.pos[1],
            ),
        ],
        scale: sample_actor_field(
            layout,
            animated_params,
            param_ids::SCALE,
            t,
            |s| s.scale,
            layout[0].value.scale,
        ),
        scale_y: sample_actor_field(
            layout,
            animated_params,
            param_ids::SCALE_Y,
            t,
            |s| s.scale_y,
            layout[0].value.scale_y,
        ),
        rotation_deg: sample_actor_field(
            layout,
            animated_params,
            param_ids::ROTATION,
            t,
            |s| s.rotation_deg,
            layout[0].value.rotation_deg,
        ),
        opacity: sample_actor_field(
            layout,
            animated_params,
            param_ids::OPACITY,
            t,
            |s| s.opacity,
            layout[0].value.opacity,
        ),
        flip_x_anim: sample_actor_field(
            layout,
            animated_params,
            param_ids::FLIP_X,
            t,
            |s| s.flip_x_anim,
            layout[0].value.flip_x_anim,
        ),
        flip_y_anim: sample_actor_field(
            layout,
            animated_params,
            param_ids::FLIP_Y,
            t,
            |s| s.flip_y_anim,
            layout[0].value.flip_y_anim,
        ),
    }
}

fn sample_overlay_field(
    layout: &[Keyframe<OverlayState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    get: impl Fn(&OverlayState) -> f32,
    default: f32,
) -> f32 {
    if animated_params.contains(param_id) {
        let times = owned_param_times(layout, &get, param_id);
        sample_scalar_track(layout, &times, t, &get, default)
    } else {
        layout.first().map(|kf| get(&kf.value)).unwrap_or(default)
    }
}

/// Sample an overlay layout with independent per-parameter interpolation.
pub fn sample_overlay_layout(
    layout: &[Keyframe<OverlayState>],
    animated_params: &BTreeSet<String>,
    t: f32,
) -> OverlayState {
    if layout.is_empty() {
        return OverlayState::default();
    }
    OverlayState {
        pos: [
            sample_overlay_field(
                layout,
                animated_params,
                param_ids::POS_X,
                t,
                |s| s.pos[0],
                layout[0].value.pos[0],
            ),
            sample_overlay_field(
                layout,
                animated_params,
                param_ids::POS_Y,
                t,
                |s| s.pos[1],
                layout[0].value.pos[1],
            ),
        ],
        scale: sample_overlay_field(
            layout,
            animated_params,
            param_ids::SCALE,
            t,
            |s| s.scale,
            layout[0].value.scale,
        ),
        scale_y: sample_overlay_field(
            layout,
            animated_params,
            param_ids::SCALE_Y,
            t,
            |s| s.scale_y,
            layout[0].value.scale_y,
        ),
        rotation_deg: sample_overlay_field(
            layout,
            animated_params,
            param_ids::ROTATION,
            t,
            |s| s.rotation_deg,
            layout[0].value.rotation_deg,
        ),
        opacity: sample_overlay_field(
            layout,
            animated_params,
            param_ids::OPACITY,
            t,
            |s| s.opacity,
            layout[0].value.opacity,
        ),
        flip_x_anim: sample_overlay_field(
            layout,
            animated_params,
            param_ids::FLIP_X,
            t,
            |s| s.flip_x_anim,
            layout[0].value.flip_x_anim,
        ),
        flip_y_anim: sample_overlay_field(
            layout,
            animated_params,
            param_ids::FLIP_Y,
            t,
            |s| s.flip_y_anim,
            layout[0].value.flip_y_anim,
        ),
    }
}

fn sample_rf_field(
    layout: &[Keyframe<RenderFrameState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    get: impl Fn(&RenderFrameState) -> f32,
    default: f32,
) -> f32 {
    if animated_params.contains(param_id) {
        let times = owned_param_times(layout, &get, param_id);
        sample_scalar_track(layout, &times, t, &get, default)
    } else {
        layout.first().map(|kf| get(&kf.value)).unwrap_or(default)
    }
}

/// Sample a render-frame layout with independent per-parameter interpolation.
pub fn sample_render_frame_layout(
    layout: &[Keyframe<RenderFrameState>],
    animated_params: &BTreeSet<String>,
    t: f32,
) -> RenderFrameState {
    if layout.is_empty() {
        return RenderFrameState::default();
    }
    let base = &layout[0].value;
    RenderFrameState {
        pos: crate::canvas::WorldPos {
            x: sample_rf_field(
                layout,
                animated_params,
                param_ids::POS_X,
                t,
                |s| s.pos.x,
                base.pos.x,
            ),
            y: sample_rf_field(
                layout,
                animated_params,
                param_ids::POS_Y,
                t,
                |s| s.pos.y,
                base.pos.y,
            ),
        },
        zoom: sample_rf_field(
            layout,
            animated_params,
            param_ids::SCALE,
            t,
            |s| s.zoom,
            base.zoom,
        ),
        rotation_deg: sample_rf_field(
            layout,
            animated_params,
            param_ids::ROTATION,
            t,
            |s| s.rotation_deg,
            base.rotation_deg,
        ),
    }
}

pub fn canvas_param_timeline_times(
    layout: &[Keyframe<CanvasTransform>],
    param_id: &str,
) -> Vec<f32> {
    owned_param_times(layout, |s| canvas_get(s, param_id), param_id)
}

fn sample_canvas_field(
    layout: &[Keyframe<CanvasTransform>],
    param_id: &str,
    t: f32,
    get: impl Fn(&CanvasTransform) -> f32,
    default: f32,
) -> f32 {
    let times = owned_param_times(layout, &get, param_id);
    sample_scalar_track(layout, &times, t, &get, default)
}

pub fn sample_canvas_layout(layout: &[Keyframe<CanvasTransform>], t: f32) -> CanvasTransform {
    if layout.is_empty() {
        return CanvasTransform::default();
    }
    let base = &layout[0].value;
    CanvasTransform {
        pos: crate::canvas::WorldPos {
            x: sample_canvas_field(layout, param_ids::POS_X, t, |s| s.pos.x, base.pos.x),
            y: sample_canvas_field(layout, param_ids::POS_Y, t, |s| s.pos.y, base.pos.y),
        },
        width: base.width,
        scale: sample_canvas_field(layout, param_ids::SCALE, t, |s| s.scale, base.scale),
        rotation_deg: sample_canvas_field(
            layout,
            param_ids::ROTATION,
            t,
            |s| s.rotation_deg,
            base.rotation_deg,
        ),
        opacity: sample_canvas_field(layout, param_ids::OPACITY, t, |s| s.opacity, base.opacity),
    }
}

pub fn sample_canvas_param_at(layout: &[Keyframe<CanvasTransform>], param_id: &str, t: f32) -> f32 {
    canvas_get(&sample_canvas_layout(layout, t), param_id)
}

fn remove_layout_times<T: Clone>(layout: &mut Vec<Keyframe<T>>, times: &[f32]) {
    layout.retain(|kf| !times.iter().any(|t| (kf.t - *t).abs() < TIME_EPS));
}

fn prune_empty_layout<T: Clone + Default>(layout: &mut Vec<Keyframe<T>>) {
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, T::default()));
    }
}

/// Drop the auto-seeded anchor at `t = 0` when the first real keyframe
/// is authored elsewhere on the timeline.
fn remove_redundant_anchor_keyframe<T>(layout: &mut Vec<Keyframe<T>>, seed_t: f32) {
    if layout.len() == 1 && layout[0].t.abs() < TIME_EPS && seed_t > TIME_EPS {
        layout.clear();
    }
}

fn actor_get(s: &ActorState, param_id: &str) -> f32 {
    match param_id {
        param_ids::POS_X => s.pos[0],
        param_ids::POS_Y => s.pos[1],
        param_ids::SCALE => s.scale,
        param_ids::SCALE_Y => s.scale_y,
        param_ids::ROTATION => s.rotation_deg,
        param_ids::OPACITY => s.opacity,
        param_ids::FLIP_X => s.flip_x_anim,
        param_ids::FLIP_Y => s.flip_y_anim,
        _ => 0.0,
    }
}

fn actor_set(s: &mut ActorState, param_id: &str, v: f32) {
    match param_id {
        param_ids::POS_X => s.pos[0] = v,
        param_ids::POS_Y => s.pos[1] = v,
        param_ids::SCALE => s.scale = v,
        param_ids::SCALE_Y => s.scale_y = v,
        param_ids::ROTATION => s.rotation_deg = v,
        param_ids::OPACITY => s.opacity = v,
        param_ids::FLIP_X => s.flip_x_anim = v,
        param_ids::FLIP_Y => s.flip_y_anim = v,
        _ => {}
    }
}

fn overlay_get(s: &OverlayState, param_id: &str) -> f32 {
    match param_id {
        param_ids::POS_X => s.pos[0],
        param_ids::POS_Y => s.pos[1],
        param_ids::SCALE => s.scale,
        param_ids::SCALE_Y => s.scale_y,
        param_ids::ROTATION => s.rotation_deg,
        param_ids::OPACITY => s.opacity,
        param_ids::FLIP_X => s.flip_x_anim,
        param_ids::FLIP_Y => s.flip_y_anim,
        _ => 0.0,
    }
}

fn overlay_set(s: &mut OverlayState, param_id: &str, v: f32) {
    match param_id {
        param_ids::POS_X => s.pos[0] = v,
        param_ids::POS_Y => s.pos[1] = v,
        param_ids::SCALE => s.scale = v,
        param_ids::SCALE_Y => s.scale_y = v,
        param_ids::ROTATION => s.rotation_deg = v,
        param_ids::OPACITY => s.opacity = v,
        param_ids::FLIP_X => s.flip_x_anim = v,
        param_ids::FLIP_Y => s.flip_y_anim = v,
        _ => {}
    }
}

fn rf_get(s: &RenderFrameState, param_id: &str) -> f32 {
    match param_id {
        param_ids::POS_X => s.pos.x,
        param_ids::POS_Y => s.pos.y,
        param_ids::SCALE => s.zoom,
        param_ids::ROTATION => s.rotation_deg,
        _ => 0.0,
    }
}

fn rf_set(s: &mut RenderFrameState, param_id: &str, v: f32) {
    match param_id {
        param_ids::POS_X => s.pos.x = v,
        param_ids::POS_Y => s.pos.y = v,
        param_ids::SCALE => s.zoom = v,
        param_ids::ROTATION => s.rotation_deg = v,
        _ => {}
    }
}

fn canvas_get(s: &CanvasTransform, param_id: &str) -> f32 {
    match param_id {
        param_ids::POS_X => s.pos.x,
        param_ids::POS_Y => s.pos.y,
        param_ids::SCALE => s.scale,
        param_ids::ROTATION => s.rotation_deg,
        param_ids::OPACITY => s.opacity,
        _ => 0.0,
    }
}

#[allow(dead_code)]
fn canvas_set(s: &mut CanvasTransform, param_id: &str, v: f32) {
    match param_id {
        param_ids::POS_X => s.pos.x = v,
        param_ids::POS_Y => s.pos.y = v,
        param_ids::SCALE => s.scale = v,
        param_ids::ROTATION => s.rotation_deg = v,
        param_ids::OPACITY => s.opacity = v,
        _ => {}
    }
}

fn other_actor_fields_changed_at(
    layout: &[Keyframe<ActorState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    i: usize,
) -> bool {
    if i == 0 {
        return false;
    }
    for pid in animated_params {
        if pid == except {
            continue;
        }
        if (actor_get(&layout[i].value, pid) - actor_get(&layout[i - 1].value, pid)).abs() > EPS {
            return true;
        }
    }
    false
}

fn other_overlay_fields_changed_at(
    layout: &[Keyframe<OverlayState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    i: usize,
) -> bool {
    if i == 0 {
        return false;
    }
    for pid in animated_params {
        if pid == except {
            continue;
        }
        if (overlay_get(&layout[i].value, pid) - overlay_get(&layout[i - 1].value, pid)).abs() > EPS
        {
            return true;
        }
    }
    false
}

fn other_rf_fields_changed_at(
    layout: &[Keyframe<RenderFrameState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    i: usize,
) -> bool {
    if i == 0 {
        return false;
    }
    for pid in animated_params {
        if pid == except {
            continue;
        }
        if (rf_get(&layout[i].value, pid) - rf_get(&layout[i - 1].value, pid)).abs() > EPS {
            return true;
        }
    }
    false
}

fn other_actor_param_changes_at(
    layout: &[Keyframe<ActorState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    at_t: f32,
) -> bool {
    for pid in animated_params {
        if pid == except {
            continue;
        }
        let times = actor_param_timeline_times(layout, animated_params, pid);
        if times.iter().any(|t| (*t - at_t).abs() < TIME_EPS) {
            return true;
        }
    }
    false
}

fn other_overlay_param_changes_at(
    layout: &[Keyframe<OverlayState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    at_t: f32,
) -> bool {
    for pid in animated_params {
        if pid == except {
            continue;
        }
        let times = overlay_param_timeline_times(layout, animated_params, pid);
        if times.iter().any(|t| (*t - at_t).abs() < TIME_EPS) {
            return true;
        }
    }
    false
}

fn other_rf_param_changes_at(
    layout: &[Keyframe<RenderFrameState>],
    animated_params: &BTreeSet<String>,
    except: &str,
    at_t: f32,
) -> bool {
    for pid in animated_params {
        if pid == except {
            continue;
        }
        let times = render_frame_param_timeline_times(layout, animated_params, pid);
        if times.iter().any(|t| (*t - at_t).abs() < TIME_EPS) {
            return true;
        }
    }
    false
}

/// Remove layout keyframes that only existed for `param_id`, then
/// flatten the parameter to a constant on every surviving keyframe.
pub fn clear_actor_param_animation(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    reference_t: f32,
) {
    let mut with_param = animated_params.clone();
    with_param.insert(param_id.to_string());
    let static_val = actor_get(
        &sample_actor_layout(layout, &with_param, reference_t),
        param_id,
    );

    let times = actor_param_timeline_times(layout, animated_params, param_id);
    let mut remove = Vec::new();
    for t in times {
        if !other_actor_param_changes_at(layout, animated_params, param_id, t) {
            remove.push(t);
        }
    }
    remove_layout_times(layout, &remove);
    prune_empty_layout(layout);
    for kf in layout.iter_mut() {
        actor_set(&mut kf.value, param_id, static_val);
        kf.clear_param_owner(param_id);
    }
}

pub fn clear_overlay_param_animation(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    reference_t: f32,
) {
    let mut with_param = animated_params.clone();
    with_param.insert(param_id.to_string());
    let static_val = overlay_get(
        &sample_overlay_layout(layout, &with_param, reference_t),
        param_id,
    );

    let times = overlay_param_timeline_times(layout, animated_params, param_id);
    let mut remove = Vec::new();
    for t in times {
        if !other_overlay_param_changes_at(layout, animated_params, param_id, t) {
            remove.push(t);
        }
    }
    remove_layout_times(layout, &remove);
    prune_empty_layout(layout);
    for kf in layout.iter_mut() {
        overlay_set(&mut kf.value, param_id, static_val);
        kf.clear_param_owner(param_id);
    }
}

pub fn clear_render_frame_param_animation(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    reference_t: f32,
) {
    let mut with_param = animated_params.clone();
    with_param.insert(param_id.to_string());
    let static_val = rf_get(
        &sample_render_frame_layout(layout, &with_param, reference_t),
        param_id,
    );

    let times = render_frame_param_timeline_times(layout, animated_params, param_id);
    let mut remove = Vec::new();
    for t in times {
        if !other_rf_param_changes_at(layout, animated_params, param_id, t) {
            remove.push(t);
        }
    }
    remove_layout_times(layout, &remove);
    prune_empty_layout(layout);
    for kf in layout.iter_mut() {
        rf_set(&mut kf.value, param_id, static_val);
        kf.clear_param_owner(param_id);
    }
}

fn layout_state_at_or_before<T: Clone>(layout: &[Keyframe<T>], t: f32, fallback: T) -> T {
    layout
        .iter()
        .filter(|k| k.t <= t + TIME_EPS)
        .last()
        .map(|k| k.value.clone())
        .unwrap_or(fallback)
}

fn upsert_actor_param_keyframe(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
    value: f32,
) {
    remove_redundant_anchor_keyframe(layout, seed_t);
    let eps = TIME_EPS;
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - seed_t).abs() < eps) {
        actor_set(&mut kf.value, param_id, value);
        kf.mark_param_owner(param_id);
        return;
    }
    let fallback = sample_actor_layout(layout, animated_params, seed_t);
    let mut state = layout_state_at_or_before(layout, seed_t, fallback);
    actor_set(&mut state, param_id, value);
    layout.push(Keyframe {
        t: seed_t,
        value: state,
        easing: crate::easing::Easing::Linear,
        param_owners: std::iter::once(param_id.to_string()).collect(),
    });
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

fn upsert_overlay_param_keyframe(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
    value: f32,
) {
    remove_redundant_anchor_keyframe(layout, seed_t);
    let eps = TIME_EPS;
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - seed_t).abs() < eps) {
        overlay_set(&mut kf.value, param_id, value);
        kf.mark_param_owner(param_id);
        return;
    }
    let fallback = sample_overlay_layout(layout, animated_params, seed_t);
    let mut state = layout_state_at_or_before(layout, seed_t, fallback);
    overlay_set(&mut state, param_id, value);
    layout.push(Keyframe {
        t: seed_t,
        value: state,
        easing: crate::easing::Easing::Linear,
        param_owners: std::iter::once(param_id.to_string()).collect(),
    });
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

fn upsert_render_frame_param_keyframe(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
    value: f32,
) {
    remove_redundant_anchor_keyframe(layout, seed_t);
    let eps = TIME_EPS;
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - seed_t).abs() < eps) {
        rf_set(&mut kf.value, param_id, value);
        kf.mark_param_owner(param_id);
        return;
    }
    let fallback = sample_render_frame_layout(layout, animated_params, seed_t);
    let mut state = layout_state_at_or_before(layout, seed_t, fallback);
    rf_set(&mut state, param_id, value);
    layout.push(Keyframe {
        t: seed_t,
        value: state,
        easing: crate::Easing::Linear,
        param_owners: std::iter::once(param_id.to_string()).collect(),
    });
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
}

/// Sample one animated scalar field at `t` (per-parameter track).
pub fn sample_actor_param_at(
    layout: &[Keyframe<ActorState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
) -> f32 {
    let mut track = animated_params.clone();
    track.insert(param_id.to_string());
    actor_get(&sample_actor_layout(layout, &track, t), param_id)
}

pub fn sample_overlay_param_at(
    layout: &[Keyframe<OverlayState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
) -> f32 {
    let mut track = animated_params.clone();
    track.insert(param_id.to_string());
    overlay_get(&sample_overlay_layout(layout, &track, t), param_id)
}

pub fn sample_render_frame_param_at(
    layout: &[Keyframe<RenderFrameState>],
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
) -> f32 {
    let mut track = animated_params.clone();
    track.insert(param_id.to_string());
    rf_get(&sample_render_frame_layout(layout, &track, t), param_id)
}

/// Write one scalar parameter at `t` without disturbing other animated
/// fields (curve editor "+ Key", drag, double-click).
pub fn author_actor_param_keyframe(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    value: f32,
) {
    upsert_actor_param_keyframe(layout, animated_params, param_id, t, value);
}

pub fn author_overlay_param_keyframe(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    value: f32,
) {
    upsert_overlay_param_keyframe(layout, animated_params, param_id, t, value);
}

pub fn author_render_frame_param_keyframe(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    t: f32,
    value: f32,
) {
    upsert_render_frame_param_keyframe(layout, animated_params, param_id, t, value);
}

/// Remove a per-parameter breakpoint at `at_t`. Shared layout keyframes
/// (other params also change there) are kept — only this field is flattened.
pub fn delete_actor_param_keyframe_at(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    at_t: f32,
) {
    let Some(i) = layout.iter().position(|k| (k.t - at_t).abs() < TIME_EPS) else {
        return;
    };
    if other_actor_fields_changed_at(layout, animated_params, param_id, i) {
        if i > 0 {
            let prev = actor_get(&layout[i - 1].value, param_id);
            actor_set(&mut layout[i].value, param_id, prev);
        }
        layout[i].clear_param_owner(param_id);
        return;
    }
    remove_layout_times(layout, &[at_t]);
    prune_empty_layout(layout);
}

pub fn delete_overlay_param_keyframe_at(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    at_t: f32,
) {
    let Some(i) = layout.iter().position(|k| (k.t - at_t).abs() < TIME_EPS) else {
        return;
    };
    if other_overlay_fields_changed_at(layout, animated_params, param_id, i) {
        if i > 0 {
            let prev = overlay_get(&layout[i - 1].value, param_id);
            overlay_set(&mut layout[i].value, param_id, prev);
        }
        layout[i].clear_param_owner(param_id);
        return;
    }
    remove_layout_times(layout, &[at_t]);
    prune_empty_layout(layout);
}

pub fn delete_render_frame_param_keyframe_at(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    at_t: f32,
) {
    let Some(i) = layout.iter().position(|k| (k.t - at_t).abs() < TIME_EPS) else {
        return;
    };
    if other_rf_fields_changed_at(layout, animated_params, param_id, i) {
        if i > 0 {
            let prev = rf_get(&layout[i - 1].value, param_id);
            rf_set(&mut layout[i].value, param_id, prev);
        }
        layout[i].clear_param_owner(param_id);
        return;
    }
    remove_layout_times(layout, &[at_t]);
    prune_empty_layout(layout);
}

/// Move a per-parameter breakpoint (time and/or value) for curve-editor drags.
pub fn rekey_actor_param_keyframe(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    old_t: f32,
    new_t: f32,
    value: f32,
) {
    let old_easing = layout
        .iter()
        .find(|k| (k.t - old_t).abs() < TIME_EPS)
        .map(|k| k.easing)
        .unwrap_or(crate::easing::Easing::Linear);
    if (old_t - new_t).abs() > TIME_EPS {
        delete_actor_param_keyframe_at(layout, animated_params, param_id, old_t);
    }
    author_actor_param_keyframe(layout, animated_params, param_id, new_t, value);
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - new_t).abs() < TIME_EPS) {
        kf.easing = old_easing;
    }
}

pub fn rekey_overlay_param_keyframe(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    old_t: f32,
    new_t: f32,
    value: f32,
) {
    let old_easing = layout
        .iter()
        .find(|k| (k.t - old_t).abs() < TIME_EPS)
        .map(|k| k.easing)
        .unwrap_or(crate::easing::Easing::Linear);
    if (old_t - new_t).abs() > TIME_EPS {
        delete_overlay_param_keyframe_at(layout, animated_params, param_id, old_t);
    }
    author_overlay_param_keyframe(layout, animated_params, param_id, new_t, value);
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - new_t).abs() < TIME_EPS) {
        kf.easing = old_easing;
    }
}

pub fn rekey_render_frame_param_keyframe(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    old_t: f32,
    new_t: f32,
    value: f32,
) {
    let old_easing = layout
        .iter()
        .find(|k| (k.t - old_t).abs() < TIME_EPS)
        .map(|k| k.easing)
        .unwrap_or(crate::easing::Easing::Linear);
    if (old_t - new_t).abs() > TIME_EPS {
        delete_render_frame_param_keyframe_at(layout, animated_params, param_id, old_t);
    }
    author_render_frame_param_keyframe(layout, animated_params, param_id, new_t, value);
    if let Some(kf) = layout.iter_mut().find(|k| (k.t - new_t).abs() < TIME_EPS) {
        kf.easing = old_easing;
    }
}

/// Enable animation for one actor parameter: place the first keyframe
/// at `seed_t` (already clamped to the element's visible range).
pub fn enable_actor_param_animation(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
) {
    let mut without_param = animated_params.clone();
    without_param.remove(param_id);
    let value = actor_get(
        &sample_actor_layout(layout, &without_param, seed_t),
        param_id,
    );
    for kf in layout.iter_mut() {
        actor_set(&mut kf.value, param_id, value);
    }
    upsert_actor_param_keyframe(layout, animated_params, param_id, seed_t, value);
}

pub fn enable_overlay_param_animation(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
) {
    let mut without_param = animated_params.clone();
    without_param.remove(param_id);
    let value = overlay_get(
        &sample_overlay_layout(layout, &without_param, seed_t),
        param_id,
    );
    for kf in layout.iter_mut() {
        overlay_set(&mut kf.value, param_id, value);
    }
    upsert_overlay_param_keyframe(layout, animated_params, param_id, seed_t, value);
}

pub fn enable_render_frame_param_animation(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    param_id: &str,
    seed_t: f32,
) {
    let mut without_param = animated_params.clone();
    without_param.remove(param_id);
    let value = rf_get(
        &sample_render_frame_layout(layout, &without_param, seed_t),
        param_id,
    );
    for kf in layout.iter_mut() {
        rf_set(&mut kf.value, param_id, value);
    }
    upsert_render_frame_param_keyframe(layout, animated_params, param_id, seed_t, value);
}

/// Apply enable/disable side effects after the inspector toggles
/// `animated_params`.
pub fn reconcile_actor_animated_params(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &BTreeSet<String>,
    animated_before: &BTreeSet<String>,
    scene_t: f32,
    clip_start: f32,
    clip_end: f32,
) {
    let seed_t = scene_t.clamp(clip_start, clip_end.max(clip_start));
    for pid in animated_before.iter() {
        if !animated_params.contains(pid) {
            clear_actor_param_animation(layout, animated_params, pid, seed_t);
        }
    }
    for pid in animated_params.iter() {
        if !animated_before.contains(pid) {
            enable_actor_param_animation(layout, animated_params, pid, seed_t);
        }
    }
}

pub fn reconcile_overlay_animated_params(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &BTreeSet<String>,
    animated_before: &BTreeSet<String>,
    local_t: f32,
    clip_duration: f32,
) {
    let seed_t = local_t.clamp(0.0, clip_duration.max(0.0));
    for pid in animated_before.iter() {
        if !animated_params.contains(pid) {
            clear_overlay_param_animation(layout, animated_params, pid, seed_t);
        }
    }
    for pid in animated_params.iter() {
        if !animated_before.contains(pid) {
            enable_overlay_param_animation(layout, animated_params, pid, seed_t);
        }
    }
}

pub fn reconcile_render_frame_animated_params(
    layout: &mut Vec<Keyframe<RenderFrameState>>,
    animated_params: &BTreeSet<String>,
    animated_before: &BTreeSet<String>,
    scene_t: f32,
    scene_duration: f32,
) {
    let seed_t = scene_t.clamp(0.0, scene_duration.max(0.0));
    for pid in animated_before.iter() {
        if !animated_params.contains(pid) {
            clear_render_frame_param_animation(layout, animated_params, pid, seed_t);
        }
    }
    for pid in animated_params.iter() {
        if !animated_before.contains(pid) {
            enable_render_frame_param_animation(layout, animated_params, pid, seed_t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Keyframe;

    #[test]
    fn opacity_two_kfs_independent_of_scale_times() {
        let mut animated = BTreeSet::new();
        animated.insert(param_ids::OPACITY.to_string());
        animated.insert(param_ids::SCALE.to_string());

        let layout = vec![
            Keyframe::new(
                0.0,
                ActorState {
                    opacity: 1.0,
                    scale: 1.0,
                    ..ActorState::default()
                },
            ),
            Keyframe::new(
                1.0,
                ActorState {
                    opacity: 0.0,
                    scale: 1.0,
                    ..ActorState::default()
                },
            ),
            Keyframe::new(
                5.0,
                ActorState {
                    opacity: 0.0,
                    scale: 2.0,
                    ..ActorState::default()
                },
            ),
        ];

        // Between opacity kfs (0..1) opacity eases; scale stays 1 until t=5.
        let mid = sample_actor_layout(&layout, &animated, 0.5);
        assert!((mid.opacity - 0.5).abs() < 0.05, "opacity should ease 1→0");
        assert!(
            (mid.scale - 1.0).abs() < EPS,
            "scale should stay 1 until its kf at 5"
        );

        // After opacity's last change (t=1) opacity holds; scale still 1 before t=5.
        let at3 = sample_actor_layout(&layout, &animated, 3.0);
        assert!((at3.opacity - 0.0).abs() < EPS);
        assert!((at3.scale - 1.0).abs() < EPS);

        // Scale only moves between its own kfs (1 and 5).
        let at3_scale_only = sample_actor_layout(
            &layout,
            &std::iter::once(param_ids::SCALE.to_string()).collect(),
            3.0,
        );
        assert!((at3_scale_only.scale - 1.0).abs() < EPS);
    }

    #[test]
    fn enable_at_playhead_drops_anchor_at_zero() {
        let mut animated = BTreeSet::new();
        let mut layout = vec![Keyframe::new(
            0.0,
            ActorState {
                opacity: 1.0,
                ..ActorState::default()
            },
        )];
        animated.insert(param_ids::OPACITY.to_string());
        enable_actor_param_animation(&mut layout, &animated, param_ids::OPACITY, 3.0);
        let times = param_change_times(&layout, |s| s.opacity);
        assert_eq!(times.len(), 1);
        assert!((times[0] - 3.0).abs() < EPS);
    }

    #[test]
    fn enable_scale_shows_timeline_diamond_without_value_change() {
        let mut animated = BTreeSet::new();
        animated.insert(param_ids::SCALE.to_string());
        let mut layout = vec![
            Keyframe::new(
                0.0,
                ActorState {
                    scale: 1.0,
                    ..ActorState::default()
                },
            ),
            Keyframe::new(
                2.0,
                ActorState {
                    opacity: 0.5,
                    scale: 1.0,
                    ..ActorState::default()
                },
            ),
        ];
        enable_actor_param_animation(&mut layout, &animated, param_ids::SCALE, 1.5);
        let scale_times = actor_param_timeline_times(&layout, &animated, param_ids::SCALE);
        assert!(
            scale_times.iter().any(|t| (*t - 1.5).abs() < EPS),
            "scale row should show playhead diamond, got {scale_times:?}"
        );
    }

    #[test]
    fn enabling_second_flat_param_does_not_create_extra_diamond_for_first() {
        let mut animated = BTreeSet::new();
        let mut layout = vec![Keyframe::new(0.0, ActorState::default())];

        animated.insert(param_ids::OPACITY.to_string());
        enable_actor_param_animation(&mut layout, &animated, param_ids::OPACITY, 1.0);

        animated.insert(param_ids::SCALE.to_string());
        enable_actor_param_animation(&mut layout, &animated, param_ids::SCALE, 2.0);

        let opacity_times = actor_param_timeline_times(&layout, &animated, param_ids::OPACITY);
        assert_eq!(
            opacity_times,
            vec![1.0],
            "opacity row must not claim scale's flat seed keyframe: {opacity_times:?}"
        );

        let scale_times = actor_param_timeline_times(&layout, &animated, param_ids::SCALE);
        assert_eq!(
            scale_times,
            vec![2.0],
            "scale row should own only its playhead seed keyframe: {scale_times:?}"
        );
    }

    #[test]
    fn enabling_rotation_does_not_reuse_position_keyframe_times() {
        let mut animated = BTreeSet::new();
        animated.insert(param_ids::POS_X.to_string());
        animated.insert(param_ids::POS_Y.to_string());
        animated.insert(param_ids::ROTATION.to_string());
        let mut layout = vec![
            Keyframe::new(
                2.0,
                ActorState {
                    pos: [0.2, 0.4],
                    rotation_deg: 0.0,
                    ..ActorState::default()
                },
            ),
            Keyframe::new(
                5.0,
                ActorState {
                    pos: [0.8, 0.4],
                    rotation_deg: 25.0,
                    ..ActorState::default()
                },
            ),
        ];

        enable_actor_param_animation(&mut layout, &animated, param_ids::ROTATION, 8.0);

        let pos_x_times = actor_param_timeline_times(&layout, &animated, param_ids::POS_X);
        assert_eq!(pos_x_times, vec![2.0, 5.0]);

        let rot_times = actor_param_timeline_times(&layout, &animated, param_ids::ROTATION);
        assert_eq!(
            rot_times,
            vec![8.0],
            "rotation row must not inherit position times: {rot_times:?}"
        );
    }

    #[test]
    fn owned_keyframes_keep_animated_params_independent() {
        let mut animated = BTreeSet::new();
        animated.insert(param_ids::POS_X.to_string());
        animated.insert(param_ids::SCALE.to_string());
        animated.insert(param_ids::ROTATION.to_string());

        let layout = vec![
            Keyframe::new(
                4.0,
                ActorState {
                    pos: [0.5, 0.7],
                    scale: 1.0,
                    rotation_deg: 0.0,
                    ..ActorState::default()
                },
            )
            .with_param_owner(param_ids::POS_X),
            Keyframe::new(
                5.5,
                ActorState {
                    pos: [0.1, 0.7],
                    scale: 0.11,
                    rotation_deg: 0.0,
                    ..ActorState::default()
                },
            )
            .with_param_owner(param_ids::SCALE),
            Keyframe::new(
                6.0,
                ActorState {
                    pos: [0.9, 0.7],
                    scale: 0.11,
                    rotation_deg: 30.0,
                    ..ActorState::default()
                },
            )
            .with_param_owner(param_ids::ROTATION),
        ];

        assert_eq!(
            actor_param_timeline_times(&layout, &animated, param_ids::POS_X),
            vec![4.0]
        );
        assert_eq!(
            actor_param_timeline_times(&layout, &animated, param_ids::SCALE),
            vec![5.5]
        );
        assert_eq!(
            actor_param_timeline_times(&layout, &animated, param_ids::ROTATION),
            vec![6.0]
        );

        let sampled = sample_actor_layout(&layout, &animated, 6.0);
        assert!(
            (sampled.pos[0] - 0.5).abs() < EPS,
            "scale/rotation snapshots must not move position X"
        );
        assert!((sampled.scale - 0.11).abs() < EPS);
        assert!((sampled.rotation_deg - 30.0).abs() < EPS);
    }

    #[test]
    fn canvas_sampling_keeps_position_scale_rotation_independent() {
        let layout = vec![
            Keyframe::new(
                1.0,
                CanvasTransform {
                    pos: crate::canvas::WorldPos { x: 100.0, y: 200.0 },
                    scale: 1.0,
                    rotation_deg: 0.0,
                    opacity: 1.0,
                    ..CanvasTransform::default()
                },
            )
            .with_param_owner(param_ids::POS_X),
            Keyframe::new(
                2.0,
                CanvasTransform {
                    pos: crate::canvas::WorldPos { x: 999.0, y: 200.0 },
                    scale: 0.25,
                    rotation_deg: 0.0,
                    opacity: 1.0,
                    ..CanvasTransform::default()
                },
            )
            .with_param_owner(param_ids::SCALE),
            Keyframe::new(
                3.0,
                CanvasTransform {
                    pos: crate::canvas::WorldPos { x: 444.0, y: 200.0 },
                    scale: 0.25,
                    rotation_deg: 45.0,
                    opacity: 1.0,
                    ..CanvasTransform::default()
                },
            )
            .with_param_owner(param_ids::ROTATION),
        ];

        assert_eq!(
            canvas_param_timeline_times(&layout, param_ids::POS_X),
            vec![1.0]
        );
        assert_eq!(
            canvas_param_timeline_times(&layout, param_ids::SCALE),
            vec![2.0]
        );
        assert_eq!(
            canvas_param_timeline_times(&layout, param_ids::ROTATION),
            vec![3.0]
        );

        let sampled = sample_canvas_layout(&layout, 3.0);
        assert!(
            (sampled.pos.x - 100.0).abs() < EPS,
            "unowned scale/rotation snapshots must not move canvas X"
        );
        assert!((sampled.scale - 0.25).abs() < EPS);
        assert!((sampled.rotation_deg - 45.0).abs() < EPS);
    }

    #[test]
    fn disable_removes_param_keyframes() {
        let mut layout = vec![
            Keyframe::new(
                1.0,
                ActorState {
                    opacity: 1.0,
                    ..ActorState::default()
                },
            ),
            Keyframe::new(
                4.0,
                ActorState {
                    opacity: 0.2,
                    ..ActorState::default()
                },
            ),
        ];
        clear_actor_param_animation(&mut layout, &BTreeSet::new(), param_ids::OPACITY, 2.0);
        assert!(param_change_times(&layout, |s| s.opacity).is_empty());
        let flat = sample_actor_layout(&layout, &BTreeSet::new(), 2.0).opacity;
        assert!(
            (flat - 0.73333335).abs() < 0.01,
            "flattened to value at t=2, got {flat}"
        );
    }

    #[test]
    fn single_kf_behaves_as_constant() {
        let mut animated = BTreeSet::new();
        animated.insert(param_ids::OPACITY.to_string());
        let layout = vec![Keyframe::new(
            2.0,
            ActorState {
                opacity: 0.4,
                ..ActorState::default()
            },
        )];
        for t in [0.0, 2.0, 10.0] {
            let s = sample_actor_layout(&layout, &animated, t);
            assert!((s.opacity - 0.4).abs() < EPS, "at t={t}");
        }
    }
}
