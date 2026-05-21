//! Per-parameter animation helpers.
//!
//! The inspector and the canvas drag handlers both author keyframes, but
//! the *intent* differs:
//!
//! - **Animated parameter** (param id is in `animated_params`):
//!   editing it inserts/updates a keyframe at the current playhead so the
//!   value can vary over time.
//!
//! - **Static parameter** (not in `animated_params`):
//!   editing it overwrites the same value in *every* keyframe of the
//!   layout vector — the param has a single static value for the whole
//!   clip, even when the layer carries other animated keyframes.
//!
//! These helpers are also the gate that **prevents infinite keyframes
//! during playback / timeline scrubbing**. The legacy inspector kept
//! `ensure_kf_at_playhead` calls running every frame, which inserted a
//! brand-new keyframe every paint as soon as the playhead drifted off
//! an existing one. The new flow only writes a keyframe when the user
//! actually edits the param (driven by `egui::Response::changed`).

use std::collections::BTreeSet;

use memstroy_core::{
    keyframe, ActorState, CanvasTransform, Easing, Keyframe, OverlayState,
};

// ─── Generic in-vec upsert ───────────────────────────────────────────

/// Find or insert (within ε) the index of a keyframe at time `t`. Seeds a
/// freshly inserted keyframe with `seed_value` and easing `Linear`. Returns
/// the index of the kf at `t`. Used by writers below — never call this
/// from rendering / hover paths.
fn upsert_index<T: Clone + keyframe::Lerp>(
    layout: &mut Vec<Keyframe<T>>,
    t: f32,
    seed_value: T,
) -> usize {
    let eps = 1.0e-3;
    if let Some(idx) = layout.iter().position(|kf| (kf.t - t).abs() < eps) {
        return idx;
    }
    layout.push(Keyframe {
        t,
        value: seed_value,
        easing: Easing::Linear,
    });
    layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    layout
        .iter()
        .position(|kf| (kf.t - t).abs() < eps)
        .unwrap_or(0)
}

// ─── Sampled "current value" reads ───────────────────────────────────

/// Sample an actor layout at time `t`, returning a default if the layout
/// is empty. Reading is read-only — never mutates the layout.
pub fn sample_actor(layout: &[Keyframe<ActorState>], t: f32) -> ActorState {
    keyframe::sample(layout, t).unwrap_or_default()
}

pub fn sample_overlay(layout: &[Keyframe<OverlayState>], t: f32) -> OverlayState {
    keyframe::sample(layout, t).unwrap_or_default()
}

// ─── Writers (called from inspector / canvas after user edit) ────────

/// Apply a per-param mutation `f` to an actor's layout. If the param is
/// in `animated_params`, the change is written to a keyframe at the
/// playhead time `t` (inserting one — seeded with the eased current
/// value — when none exists at that time). Otherwise the change is
/// broadcast to every existing keyframe so the param remains static.
///
/// The mutator receives `&mut ActorState` and is expected to update only
/// the field that corresponds to `param_id`.
pub fn write_actor_param<F>(
    layout: &mut Vec<Keyframe<ActorState>>,
    animated_params: &mut BTreeSet<String>,
    t: f32,
    param_id: &str,
    auto_animate_on_canvas_drag: bool,
    f: F,
) where
    F: Fn(&mut ActorState),
{
    // Seed with at least one kf so the rest of the system has a value to read.
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, ActorState::default()));
    }

    // Canvas drags optionally auto-mark a param as animated when the
    // playhead is past 0 — this preserves the existing canvas-first
    // workflow where dragging the rig at any later time records motion.
    if auto_animate_on_canvas_drag && t > 1.0e-3 {
        animated_params.insert(param_id.to_string());
    }

    let is_animated = animated_params.contains(param_id);
    if is_animated {
        let seed = sample_actor(layout, t);
        let idx = upsert_index(layout, t, seed);
        if let Some(kf) = layout.get_mut(idx) {
            f(&mut kf.value);
        }
    } else {
        for kf in layout.iter_mut() {
            f(&mut kf.value);
        }
    }
}

pub fn write_overlay_param<F>(
    layout: &mut Vec<Keyframe<OverlayState>>,
    animated_params: &mut BTreeSet<String>,
    t: f32,
    param_id: &str,
    auto_animate_on_canvas_drag: bool,
    f: F,
) where
    F: Fn(&mut OverlayState),
{
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, OverlayState::default()));
    }
    if auto_animate_on_canvas_drag && t > 1.0e-3 {
        animated_params.insert(param_id.to_string());
    }

    let is_animated = animated_params.contains(param_id);
    if is_animated {
        let seed = sample_overlay(layout, t);
        let idx = upsert_index(layout, t, seed);
        if let Some(kf) = layout.get_mut(idx) {
            f(&mut kf.value);
        }
    } else {
        for kf in layout.iter_mut() {
            f(&mut kf.value);
        }
    }
}

/// Canvas-layout (free canvas v2) variant. Always animates by definition
/// — the canvas_layouts entries are only created the first time a clip is
/// dragged on the free canvas, so every change should land as a kf at the
/// playhead.
pub fn write_canvas_param<F>(
    layout: &mut Vec<Keyframe<CanvasTransform>>,
    t: f32,
    f: F,
) where
    F: Fn(&mut CanvasTransform),
{
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, CanvasTransform::default()));
    }
    let seed = keyframe::sample(layout, t).unwrap_or_default();
    let idx = upsert_index(layout, t, seed);
    if let Some(kf) = layout.get_mut(idx) {
        f(&mut kf.value);
    }
}

// ─── "Animated" toggle widget (inspector) ────────────────────────────

/// Render a small clickable diamond indicating whether `param_id` is in
/// `animated_params`, and toggle membership on click. Returns `true` when
/// the toggle changed this frame so the caller can refresh dependent UI.
///
/// Visual:
/// - filled gold diamond  ⬥  → param is animated (changes will create kfs)
/// - hollow gray diamond  ⬦  → param is static (single value across track)
pub fn animated_toggle(
    ui: &mut egui::Ui,
    animated_params: &mut BTreeSet<String>,
    param_id: &str,
    salt: impl std::hash::Hash + Copy,
) -> bool {
    let _ = salt; // reserved for future per-instance ids
    let on = animated_params.contains(param_id);
    let glyph = if on { "\u{2B25}" } else { "\u{2B26}" }; // filled vs hollow diamond
    let color = if on {
        egui::Color32::from_rgb(255, 220, 80)
    } else {
        egui::Color32::from_rgb(120, 120, 140)
    };
    let btn = egui::Button::new(
        egui::RichText::new(glyph).size(11.0).color(color),
    )
    .frame(false)
    .min_size(egui::Vec2::new(14.0, 14.0));
    let resp = ui.add(btn);
    let resp = resp.on_hover_text(if on {
        "Animated — click to lock to a single static value"
    } else {
        "Static — click to make this parameter animatable (changes will create keyframes)"
    });
    if resp.clicked() {
        if on {
            animated_params.remove(param_id);
        } else {
            animated_params.insert(param_id.to_string());
        }
        return true;
    }
    false
}

// ─── Keyframe selection in the timeline ──────────────────────────────

/// One entry in the layer's keyframe-selection list.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedKeyframe {
    /// Layer kind + index. Encoded as (layer_kind, layer_idx).
    pub layer: SelectedLayer,
    /// Param id this keyframe row represents.
    pub param_id: String,
    /// Approximate time of the keyframe (seconds, in clip-local frame for
    /// overlays / scene-time for actors).
    pub t: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SelectedLayer {
    Actor(usize),
    Overlay(usize),
    RenderFrame,
}

/// Highlight target for the inspector when a kf was just clicked. The
/// inspector flashes the matching param row for ~1 second so the user can
/// trace which control belongs to the kf they hit.
#[derive(Clone, Debug, Default)]
pub struct KfHighlight {
    pub param_id: String,
    pub started_at: Option<std::time::Instant>,
}

impl KfHighlight {
    pub fn set(&mut self, param_id: impl Into<String>) {
        self.param_id = param_id.into();
        self.started_at = Some(std::time::Instant::now());
    }
    pub fn is_active(&self, param_id: &str) -> bool {
        if self.param_id != param_id {
            return false;
        }
        match self.started_at {
            Some(t) => t.elapsed() < std::time::Duration::from_millis(1500),
            None => false,
        }
    }
}
