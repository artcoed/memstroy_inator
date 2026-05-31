//! Shared helpers for timeline / layer razor splits: trim clip windows and
//! crop every keyframe track that uses scene-time or clip-local time.

use std::collections::BTreeMap;

use memstroy_core::{
    keyframe::{self, Keyframe, TrackModifier},
    Actor, ActorState, Overlay, Scene, Transition,
};

const EPS: f32 = 1.0e-3;

/// Default timeline span before ffprobe / frame extraction report a real
/// duration (`clip_duration_for_placement` in the timeline UI).
pub const CLIP_DURATION_PLACEHOLDER_SEC: f32 = 60.0;

/// Maximum scene `t_out` reachable from `source_duration` given the actor's
/// in-point, `source_start`, and playback speed.
pub fn actor_source_window_cap(
    t_in: f32,
    source_start: f32,
    speed: f32,
    source_duration: f32,
) -> f32 {
    let speed = speed.max(0.0001);
    t_in + ((source_duration - source_start).max(0.0) / speed).max(0.05)
}

/// After source duration becomes known, widen clips that still carry the
/// drop placeholder length, clamp overstretched bars, and leave razor splits /
/// manual trims untouched.
pub fn reconcile_actor_t_out_for_source(actor: &mut Actor, source_duration: f32) {
    let t_in = actor.t_in.unwrap_or(0.0);
    let cap = actor_source_window_cap(
        t_in,
        actor.source_start,
        actor.speed,
        source_duration,
    );
    let Some(cur) = actor.t_out else {
        actor.t_out = Some(cap);
        return;
    };
    if cur > cap + 0.05 {
        actor.t_out = Some(cap);
        return;
    }
    let visible = (cur - t_in).max(0.0);
    if visible + 0.05 >= CLIP_DURATION_PLACEHOLDER_SEC {
        actor.t_out = Some(cap);
    }
}

/// Crop every track in a `param_kfs`-shaped clip-local map.
///
/// `local_cut` is the cut time expressed in the PRE-SPLIT clip-local
/// frame (i.e. `cut_t_scene - original_t_in`).
///
/// * **Left half** (`right_half = false`): keep `kf.t <= local_cut`.
///   No rebase — the left half's `t_in` didn't move, so the local
///   origin is unchanged.
/// * **Right half** (`right_half = true`): keep `kf.t >= local_cut`
///   and rebase to `kf.t -= local_cut`. After the split the right
///   half's `t_in` is `cut_t_scene`, so its local origin is shifted
///   forward by `local_cut`; subtracting that puts surviving kfs
///   back onto the new local timeline.
fn crop_param_kfs_local(
    map: &mut BTreeMap<String, Vec<Keyframe<f32>>>,
    local_cut: f32,
    right_half: bool,
) {
    for kfs in map.values_mut() {
        if right_half {
            kfs.retain(|kf| kf.t >= local_cut - EPS);
            for kf in kfs.iter_mut() {
                kf.t = (kf.t - local_cut).max(0.0);
            }
        } else {
            kfs.retain(|kf| kf.t <= local_cut + EPS);
        }
    }
}

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

pub fn crop_actor_timeline(
    actor: &mut Actor,
    cut_t: f32,
    original_t_in: f32,
    right_half: bool,
) {
    // Actor `layout` is sampled at scene time, so its split partitions
    // keyframes by absolute `kf.t` with no rebase.
    if right_half {
        crop_scene_time_kfs(&mut actor.layout, |t| t >= cut_t - EPS, None);
    } else {
        crop_scene_time_kfs(&mut actor.layout, |t| t <= cut_t + EPS, None);
    }

    // Everything else on the actor (`modifiers`, `effects[].param_kfs`,
    // `color_correction.kfs`) is sampled at clip-local time
    // (`t - t_in`
    crop_modifiers_local(&mut actor.modifiers, cut_t - original_t_in, right_half);

    // ── Animated effect parameters and color-correction tracks live
    //    in CLIP-LOCAL time. Without this they're duplicated verbatim
    //    on both halves and replay in full on each, which is what
    //    used to look like two clips overlapping in one layer.
    let local_cut = (cut_t - original_t_in).max(0.0);
    for eff in actor.effects.iter_mut() {
        crop_param_kfs_local(&mut eff.param_kfs, local_cut, right_half);
    }
    crop_param_kfs_local(&mut actor.color_correction.kfs, local_cut, right_half);

    // ── Hard cut at the split point. Whatever transition was
    //    configured at the now-internal seam would have played
    //    halfway through the original clip, so drop it on the
    //    cut-facing side of each half.
    if right_half {
        actor.transition_in = Transition::Cut;
    } else {
        actor.transition_out = Transition::Cut;
    }
}

pub fn crop_overlay_timeline(
    overlay: &mut Overlay,
    cut_t: f32,
    original_t_in: f32,
    right_half: bool,
) {
    let t_in = original_t_in;
    let t_out = match overlay {
        Overlay::Text(t) => t.t_out,
        Overlay::Image(im) => im.t_out,
        Overlay::Video(v) => v.t_out,
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
            for eff in t.effects.iter_mut() {
                crop_param_kfs_local(&mut eff.param_kfs, local_cut, right_half);
            }
        }
        Overlay::Image(im) => {
            if right_half {
                im.t_in = cut_t;
            } else {
                im.t_out = cut_t;
            }
            crop_layout(&mut im.layout);
            crop_modifiers_local(&mut im.modifiers, local_cut, right_half);
            for eff in im.effects.iter_mut() {
                crop_param_kfs_local(&mut eff.param_kfs, local_cut, right_half);
            }
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
            for eff in v.effects.iter_mut() {
                crop_param_kfs_local(&mut eff.param_kfs, local_cut, right_half);
            }
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
    // The right half's `t_in` was already mutated to `cut_right` by
    // the caller, so read the pre-split `t_in` from the LEFT (which
    // is unchanged) and use it for both halves' clip-local cropping.
    let original_t_in = scene.actors[left_idx].t_in.unwrap_or(0.0);

    // Snapshot layout at the cut point BEFORE we crop, so a half that
    // ends up with zero keyframes can still inherit the transform.
    let left_layout_sample = scene.actors.get(left_idx)
        .and_then(|a| keyframe::sample(&a.layout, cut_left));
    let right_layout_sample = scene.actors.get(right_idx)
        .and_then(|a| keyframe::sample(&a.layout, cut_right));

    if let Some(a) = scene.actors.get_mut(left_idx) {
        crop_actor_timeline(a, cut_left, original_t_in, false);
    }
    if let Some(a) = scene.actors.get_mut(right_idx) {
        crop_actor_timeline(a, cut_right, original_t_in, true);
    }
    for idx in [left_idx, right_idx] {
        if let Some(a) = scene.actors.get_mut(idx) {
            if a.layout.is_empty() {
                let seed_t = a.t_in.unwrap_or(0.0);
                let fallback = if idx == left_idx {
                    left_layout_sample
                } else {
                    right_layout_sample
                };
                a.layout.push(Keyframe::new(
                    seed_t,
                    fallback.unwrap_or_else(ActorState::default),
                ));
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
    // Pre-split `t_in` lives on the left half (the caller only
    // mutated `t_out` on the left and the new clone's `t_in` after
    // we get here, but `split_overlay_at` only changes `t_out` on
    // the left at mutate time, so this read is robust either way).
    let original_t_in = match &scene.overlays[left_idx] {
        Overlay::Text(t) => t.t_in,
        Overlay::Image(im) => im.t_in,
        Overlay::Video(v) => v.t_in,
    };

    // Snapshot overlay layout (clip-local time) at the cut point
    // before we crop, so a half that loses every keyframe can
    // still inherit the transform / scale / rotation.
    let local_left = (cut_left - original_t_in).max(0.0);
    let local_right = (cut_right - original_t_in).max(0.0);
    let left_layout_sample = match &scene.overlays[left_idx] {
        Overlay::Text(t) => keyframe::sample(&t.layout, local_left),
        Overlay::Image(im) => keyframe::sample(&im.layout, local_left),
        Overlay::Video(v) => keyframe::sample(&v.layout, local_left),
    };
    let right_layout_sample = match &scene.overlays[right_idx] {
        Overlay::Text(t) => keyframe::sample(&t.layout, local_right),
        Overlay::Image(im) => keyframe::sample(&im.layout, local_right),
        Overlay::Video(v) => keyframe::sample(&v.layout, local_right),
    };

    if let Some(ov) = scene.overlays.get_mut(left_idx) {
        crop_overlay_timeline(ov, cut_left, original_t_in, false);
    }
    if let Some(ov) = scene.overlays.get_mut(right_idx) {
        crop_overlay_timeline(ov, cut_right, original_t_in, true);
    }

    // Fallback: if the crop wiped every layout keyframe, restore
    // the sampled state so the element keeps its position / scale.
    if let Some(ov) = scene.overlays.get_mut(left_idx) {
        let layout = match ov {
            Overlay::Text(t) => &mut t.layout,
            Overlay::Image(im) => &mut im.layout,
            Overlay::Video(v) => &mut v.layout,
        };
        if layout.is_empty() {
            layout.push(Keyframe::new(
                0.0,
                left_layout_sample.unwrap_or_default(),
            ));
        }
    }
    if let Some(ov) = scene.overlays.get_mut(right_idx) {
        let layout = match ov {
            Overlay::Text(t) => &mut t.layout,
            Overlay::Image(im) => &mut im.layout,
            Overlay::Video(v) => &mut v.layout,
        };
        if layout.is_empty() {
            layout.push(Keyframe::new(
                0.0,
                right_layout_sample.unwrap_or_default(),
            ));
        }
    }

    crop_canvas_layouts_left(scene, &left_id, cut_left);
    duplicate_canvas_layout_for_split(scene, &left_id, &right_id, cut_right);
}

#[cfg(test)]
mod tests {
    use super::*;
    use memstroy_core::{
        keyframe::TrackModifier,
        ActorState, ImageOverlay, Overlay, OverlayState, Transition, VideoOverlay,
    };
    use serde_json::json;

    const TEPS: f32 = 1.0e-3;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < TEPS
    }

    fn assert_kf_times(kfs: &[Keyframe<f32>], expected: &[f32]) {
        assert_eq!(
            kfs.len(),
            expected.len(),
            "kf count mismatch: got {:?}, want times {:?}",
            kfs.iter().map(|k| k.t).collect::<Vec<_>>(),
            expected,
        );
        for (i, kf) in kfs.iter().enumerate() {
            assert!(
                approx(kf.t, expected[i]),
                "kf[{}].t = {}, expected {}",
                i,
                kf.t,
                expected[i]
            );
        }
    }

    fn mk_modifier(t_start: f32, t_end: f32) -> TrackModifier {
        let mut m = TrackModifier::wobble();
        m.t_start = t_start;
        m.t_end = t_end;
        m
    }

    fn mk_actor(t_in: Option<f32>, t_out: Option<f32>) -> Actor {
        let mut a: Actor = serde_json::from_value(serde_json::json!({
            "id": "t",
            "source": "",
        }))
        .expect("Actor has serde defaults");
        a.t_in = t_in;
        a.t_out = t_out;
        a
    }

    fn mk_image_overlay(t_in: f32, t_out: f32) -> Overlay {
        let mut im: ImageOverlay = serde_json::from_value(serde_json::json!({
            "id": "t",
            "source": "",
            "t_in": t_in,
            "t_out": t_out,
            "layout": [],
        }))
        .expect("ImageOverlay has serde defaults");
        im.t_in = t_in;
        im.t_out = t_out;
        Overlay::Image(im)
    }

    fn mk_video_overlay(t_in: f32, t_out: f32, source_start: f32) -> Overlay {
        let mut v: VideoOverlay = serde_json::from_value(serde_json::json!({
            "id": "t",
            "source": "",
            "t_in": t_in,
            "t_out": t_out,
            "source_start": source_start,
            "layout": [],
        }))
        .expect("VideoOverlay has serde defaults");
        v.t_in = t_in;
        v.t_out = t_out;
        v.source_start = source_start;
        Overlay::Video(v)
    }

    fn kfs_f32(times: &[f32]) -> Vec<Keyframe<f32>> {
        times
            .iter()
            .enumerate()
            .map(|(i, &t)| Keyframe::new(t, i as f32))
            .collect()
    }

    // ── crop_param_kfs_local ────────────────────────────────────────

    #[test]
    fn crop_param_kfs_local_left_half_keeps_before_cut_no_rebase() {
        let mut map = BTreeMap::new();
        map.insert("p".to_string(), kfs_f32(&[0.0, 1.0, 2.0, 4.0]));
        crop_param_kfs_local(&mut map, 2.0, false);
        assert_kf_times(map.get("p").unwrap(), &[0.0, 1.0, 2.0]);
    }

    #[test]
    fn crop_param_kfs_local_right_half_keeps_after_cut_and_rebases() {
        let mut map = BTreeMap::new();
        map.insert("p".to_string(), kfs_f32(&[0.0, 1.0, 2.0, 4.0]));
        crop_param_kfs_local(&mut map, 2.0, true);
        // {2, 4} survive, rebased to {0, 2}.
        assert_kf_times(map.get("p").unwrap(), &[0.0, 2.0]);
    }

    #[test]
    fn crop_param_kfs_local_kf_at_boundary_belongs_to_both_halves() {
        let mut left = BTreeMap::new();
        left.insert("p".to_string(), kfs_f32(&[0.0, 2.0, 4.0]));
        let mut right = left.clone();
        crop_param_kfs_local(&mut left, 2.0, false);
        crop_param_kfs_local(&mut right, 2.0, true);
        assert_kf_times(left.get("p").unwrap(), &[0.0, 2.0]);
        // Right half: kf at the cut stays and lands at t=0 after rebase.
        assert_kf_times(right.get("p").unwrap(), &[0.0, 2.0]);
    }

    #[test]
    fn crop_param_kfs_local_empty_track_stays_empty() {
        let mut map = BTreeMap::new();
        map.insert("p".to_string(), Vec::<Keyframe<f32>>::new());
        crop_param_kfs_local(&mut map, 5.0, false);
        assert!(map.get("p").unwrap().is_empty());
        crop_param_kfs_local(&mut map, 5.0, true);
        assert!(map.get("p").unwrap().is_empty());
    }

    // ── crop_actor_timeline ─────────────────────────────────────────

    #[test]
    fn crop_actor_timeline_left_drops_effects_and_cc_after_cut() {
        // Actor t_in=0, effect kfs at clip-local {0, 3, 8}, CC kfs at {0, 4, 8}.
        let mut a = mk_actor(Some(0.0), Some(10.0));
        let mut eff = memstroy_core::Effect::default();
        eff.param_kfs
            .insert("amount".to_string(), kfs_f32(&[0.0, 3.0, 8.0]));
        a.effects.push(eff);
        a.color_correction
            .kfs
            .insert("brightness".to_string(), kfs_f32(&[0.0, 4.0, 8.0]));
        // Scene-time layout kfs at {0, 4, 8} — cut at scene-time 5 → keep {0, 4}.
        a.layout = vec![
            Keyframe::new(0.0, ActorState::default()),
            Keyframe::new(4.0, ActorState::default()),
            Keyframe::new(8.0, ActorState::default()),
        ];

        crop_actor_timeline(&mut a, 5.0, 0.0, false);

        assert_kf_times(
            a.effects[0].param_kfs.get("amount").unwrap(),
            &[0.0, 3.0],
        );
        assert_kf_times(
            a.color_correction.kfs.get("brightness").unwrap(),
            &[0.0, 4.0],
        );
        assert_eq!(a.layout.len(), 2);
        assert!(approx(a.layout[0].t, 0.0));
        assert!(approx(a.layout[1].t, 4.0));
    }

    #[test]
    fn crop_actor_timeline_right_rebases_effects_and_cc() {
        let mut a = mk_actor(Some(0.0), Some(10.0));
        let mut eff = memstroy_core::Effect::default();
        eff.param_kfs
            .insert("amount".to_string(), kfs_f32(&[0.0, 3.0, 5.0, 8.0]));
        a.effects.push(eff);
        a.color_correction
            .kfs
            .insert("brightness".to_string(), kfs_f32(&[0.0, 4.0, 8.0]));
        a.layout = vec![
            Keyframe::new(0.0, ActorState::default()),
            Keyframe::new(4.0, ActorState::default()),
            Keyframe::new(8.0, ActorState::default()),
        ];

        crop_actor_timeline(&mut a, 5.0, 0.0, true);

        // {5, 8} survive (3 dropped), rebased to {0, 3}.
        assert_kf_times(
            a.effects[0].param_kfs.get("amount").unwrap(),
            &[0.0, 3.0],
        );
        // CC: {8} survives, rebased to {3}.
        assert_kf_times(
            a.color_correction.kfs.get("brightness").unwrap(),
            &[3.0],
        );
        // Layout is scene-time so no rebase: keep {8}.
        assert_eq!(a.layout.len(), 1);
        assert!(approx(a.layout[0].t, 8.0));
    }

    #[test]
    fn crop_actor_timeline_hard_cuts_transitions() {
        let mut a = mk_actor(Some(0.0), Some(10.0));
        a.transition_in = Transition::Fade;
        a.transition_out = Transition::SlideLeft;

        let mut left = a.clone();
        crop_actor_timeline(&mut left, 5.0, 0.0, false);
        assert_eq!(left.transition_in, Transition::Fade);
        assert_eq!(left.transition_out, Transition::Cut);

        let mut right = a;
        crop_actor_timeline(&mut right, 5.0, 0.0, true);
        assert_eq!(right.transition_in, Transition::Cut);
        assert_eq!(right.transition_out, Transition::SlideLeft);
    }

    #[test]
    fn crop_actor_timeline_local_cut_when_t_in_nonzero() {
        // Actor lives on [2, 10] in scene time. Cut at scene-time 5
        // → local_cut = 3. Effect kfs at clip-local {0, 3, 6, 9}.
        let mut a = mk_actor(Some(2.0), Some(10.0));
        let mut eff = memstroy_core::Effect::default();
        eff.param_kfs
            .insert("amount".to_string(), kfs_f32(&[0.0, 3.0, 6.0, 9.0]));
        a.effects.push(eff);

        let mut left = a.clone();
        crop_actor_timeline(&mut left, 5.0, 2.0, false);
        // local_cut = 3 → keep {0, 3}.
        assert_kf_times(left.effects[0].param_kfs.get("amount").unwrap(), &[0.0, 3.0]);

        let mut right = a;
        crop_actor_timeline(&mut right, 5.0, 2.0, true);
        // local_cut = 3 → keep {3, 6, 9} rebased to {0, 3, 6}.
        assert_kf_times(
            right.effects[0].param_kfs.get("amount").unwrap(),
            &[0.0, 3.0, 6.0],
        );
    }

    #[test]
    fn crop_actor_timeline_modifiers_split_in_clip_local_time() {
        // Actor t_in=2, t_out=12. Cut at scene-time 7 → local_cut=5.
        // Modifiers in clip-local time:
        //   [1,3]  → before cut, left only, unchanged.
        //   [3,7]  → straddles cut, both halves get a partial.
        //   [6,9]  → starts after cut, right only, rebased.
        let mut a = mk_actor(Some(2.0), Some(12.0));
        a.modifiers.push(mk_modifier(1.0, 3.0));
        a.modifiers.push(mk_modifier(3.0, 7.0));
        a.modifiers.push(mk_modifier(6.0, 9.0));

        let mut left = a.clone();
        crop_actor_timeline(&mut left, 7.0, 2.0, false);
        // Left: keep modifiers that started before local_cut=5, clamp t_end.
        assert_eq!(left.modifiers.len(), 2);
        assert!(approx(left.modifiers[0].t_start, 1.0));
        assert!(approx(left.modifiers[0].t_end, 3.0));
        assert!(approx(left.modifiers[1].t_start, 3.0));
        assert!(approx(left.modifiers[1].t_end, 5.0));

        let mut right = a;
        crop_actor_timeline(&mut right, 7.0, 2.0, true);
        // Right: keep modifiers whose t_end >= local_cut=5,
        // rebase so the new clip-local origin is 0.
        assert_eq!(right.modifiers.len(), 2);
        // Was [3,7] → rebased to [(3-5).max(0), (7-5)] = [0, 2].
        assert!(approx(right.modifiers[0].t_start, 0.0));
        assert!(approx(right.modifiers[0].t_end, 2.0));
        // Was [6,9] → rebased to [1, 4].
        assert!(approx(right.modifiers[1].t_start, 1.0));
        assert!(approx(right.modifiers[1].t_end, 4.0));
    }

    // ── crop_overlay_timeline ───────────────────────────────────────

    #[test]
    fn crop_overlay_timeline_image_left_half_trims_t_out_and_drops_kfs_after_cut() {
        let mut ov = mk_image_overlay(0.0, 10.0);
        if let Overlay::Image(im) = &mut ov {
            im.layout = vec![
                Keyframe::new(0.0, OverlayState::default()),
                Keyframe::new(4.0, OverlayState::default()),
                Keyframe::new(8.0, OverlayState::default()),
            ];
            let mut eff = memstroy_core::Effect::default();
            eff.param_kfs.insert("op".to_string(), kfs_f32(&[0.0, 4.0, 8.0]));
            im.effects.push(eff);
        }

        crop_overlay_timeline(&mut ov, 5.0, 0.0, false);

        match ov {
            Overlay::Image(im) => {
                assert!(approx(im.t_in, 0.0));
                assert!(approx(im.t_out, 5.0));
                assert_eq!(im.layout.len(), 2);
                assert!(approx(im.layout[0].t, 0.0));
                assert!(approx(im.layout[1].t, 4.0));
                assert_kf_times(
                    im.effects[0].param_kfs.get("op").unwrap(),
                    &[0.0, 4.0],
                );
            }
            _ => panic!("expected Image overlay"),
        }
    }

    #[test]
    fn crop_overlay_timeline_image_right_half_rebases_layout_and_effects() {
        let mut ov = mk_image_overlay(0.0, 10.0);
        if let Overlay::Image(im) = &mut ov {
            im.layout = vec![
                Keyframe::new(0.0, OverlayState::default()),
                Keyframe::new(4.0, OverlayState::default()),
                Keyframe::new(8.0, OverlayState::default()),
            ];
            let mut eff = memstroy_core::Effect::default();
            eff.param_kfs.insert("op".to_string(), kfs_f32(&[0.0, 4.0, 8.0]));
            im.effects.push(eff);
        }

        crop_overlay_timeline(&mut ov, 5.0, 0.0, true);

        match ov {
            Overlay::Image(im) => {
                assert!(approx(im.t_in, 5.0));
                assert!(approx(im.t_out, 10.0));
                // Layout in clip-local time: {8} survives, rebased to {3}.
                assert_eq!(im.layout.len(), 1);
                assert!(approx(im.layout[0].t, 3.0));
                // Effect param: same logic.
                assert_kf_times(
                    im.effects[0].param_kfs.get("op").unwrap(),
                    &[3.0],
                );
            }
            _ => panic!("expected Image overlay"),
        }
    }

    #[test]
    fn crop_overlay_timeline_video_right_half_bumps_source_start() {
        // Video overlay window [4, 14], source_start = 10. Cut at
        // scene-time 10 → local_cut = 6.
        let mut ov = mk_video_overlay(4.0, 14.0, 10.0);
        if let Overlay::Video(v) = &mut ov {
            let mut eff = memstroy_core::Effect::default();
            eff.param_kfs.insert("amount".to_string(), kfs_f32(&[0.0, 5.0, 9.0]));
            v.effects.push(eff);
        }

        crop_overlay_timeline(&mut ov, 10.0, 4.0, true);

        match ov {
            Overlay::Video(v) => {
                assert!(approx(v.t_in, 10.0));
                assert!(approx(v.t_out, 14.0));
                assert!(
                    approx(v.source_start, 16.0),
                    "source_start should advance by local_cut (6), got {}",
                    v.source_start,
                );
                // Effect kfs: {9} survives (5 dropped, 0 dropped),
                // rebased to {3}.
                assert_kf_times(
                    v.effects[0].param_kfs.get("amount").unwrap(),
                    &[3.0],
                );
            }
            _ => panic!("expected Video overlay"),
        }
    }

    // ── finish_actor_split preserves transform when single kf at t=0 ──

    #[test]
    fn finish_actor_split_preserves_transform() {
        let mut scene = Scene::default();
        let mut a = mk_actor(Some(0.0), Some(10.0));
        a.id = "actor_1".to_string();
        a.layout = vec![Keyframe::new(
            0.0,
            ActorState {
                pos: [1.0, 2.0],
                scale: 3.0,
                scale_y: 4.0,
                rotation_deg: 45.0,
                opacity: 0.5,
                flip_x_anim: 0.5,
                ..ActorState::default()
            },
        )];
        scene.actors.push(a);
        // Simulate the split: clone the actor and insert it at index 1.
        let right = scene.actors[0].clone();
        scene.actors.insert(1, right);
        scene.actors[0].t_out = Some(5.0);
        scene.actors[1].t_in = Some(5.0);

        finish_actor_split(&mut scene, 0, 1, 5.0, 5.0);

        // Left half should keep the sampled transform.
        let left = &scene.actors[0];
        assert_eq!(left.layout.len(), 1);
        assert!(approx(left.layout[0].t, 0.0));
        assert!(approx(left.layout[0].value.pos[0], 1.0));
        assert!(approx(left.layout[0].value.pos[1], 2.0));
        assert!(approx(left.layout[0].value.scale, 3.0));
        assert!(approx(left.layout[0].value.rotation_deg, 45.0));

        // Right half should also keep the sampled transform.
        let right = &scene.actors[1];
        assert_eq!(right.layout.len(), 1);
        assert!(approx(right.layout[0].t, 5.0));
        assert!(approx(right.layout[0].value.pos[0], 1.0));
        assert!(approx(right.layout[0].value.pos[1], 2.0));
        assert!(approx(right.layout[0].value.scale, 3.0));
        assert!(approx(right.layout[0].value.rotation_deg, 45.0));
    }

    #[test]
    fn finish_overlay_split_preserves_transform() {
        let mut scene = Scene::default();
        let mut ov = mk_image_overlay(0.0, 10.0);
        if let Overlay::Image(im) = &mut ov {
            im.id = "ov_1".to_string();
            im.layout = vec![Keyframe::new(
                0.0,
                OverlayState {
                    pos: [1.0, 2.0],
                    scale: 3.0,
                    scale_y: 4.0,
                    rotation_deg: 45.0,
                    opacity: 0.5,
                    flip_x_anim: 0.5,
                    ..OverlayState::default()
                },
            )];
        }
        scene.overlays.push(ov);
        let right = scene.overlays[0].clone();
        scene.overlays.insert(1, right);
        if let Overlay::Image(im) = &mut scene.overlays[0] {
            im.t_out = 5.0;
        }
        if let Overlay::Image(im) = &mut scene.overlays[1] {
            im.t_in = 5.0;
        }

        finish_overlay_split(&mut scene, 0, 1, 5.0, 5.0);

        // Left half
        match &scene.overlays[0] {
            Overlay::Image(im) => {
                assert_eq!(im.layout.len(), 1);
                assert!(approx(im.layout[0].t, 0.0));
                assert!(approx(im.layout[0].value.pos[0], 1.0));
                assert!(approx(im.layout[0].value.pos[1], 2.0));
                assert!(approx(im.layout[0].value.scale, 3.0));
                assert!(approx(im.layout[0].value.rotation_deg, 45.0));
            }
            _ => panic!("expected Image"),
        }
        // Right half
        match &scene.overlays[1] {
            Overlay::Image(im) => {
                assert_eq!(im.layout.len(), 1);
                assert!(approx(im.layout[0].t, 0.0));
                assert!(approx(im.layout[0].value.pos[0], 1.0));
                assert!(approx(im.layout[0].value.pos[1], 2.0));
                assert!(approx(im.layout[0].value.scale, 3.0));
                assert!(approx(im.layout[0].value.rotation_deg, 45.0));
            }
            _ => panic!("expected Image"),
        }
    }

    // ── reconcile_actor_t_out_for_source (timeline window after probe) ──

    #[test]
    fn reconcile_actor_t_out_preserves_razor_left_half() {
        let mut a = mk_actor(Some(0.0), Some(5.0));
        a.source_start = 0.0;
        super::reconcile_actor_t_out_for_source(&mut a, 100.0);
        assert_eq!(a.t_out, Some(5.0));
    }

    #[test]
    fn reconcile_actor_t_out_preserves_razor_right_half() {
        let mut a = mk_actor(Some(5.0), Some(10.0));
        a.source_start = 5.0;
        super::reconcile_actor_t_out_for_source(&mut a, 100.0);
        assert_eq!(a.t_out, Some(10.0));
    }

    #[test]
    fn reconcile_actor_t_out_expands_placeholder_drop() {
        let mut a = mk_actor(Some(0.0), Some(super::CLIP_DURATION_PLACEHOLDER_SEC));
        a.source_start = 0.0;
        super::reconcile_actor_t_out_for_source(&mut a, 30.0);
        assert_eq!(a.t_out, Some(30.0));
    }
}
