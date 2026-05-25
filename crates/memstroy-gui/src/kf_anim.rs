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

/// Same as [`write_overlay_param`] but for a [`RenderFrame`]'s
/// `Vec<Keyframe<RenderFrameState>>` plus its `animated_params` set.
/// Mirrors the semantics: when the requested param is animated the
/// edit upserts a kf at `t`, otherwise the value is broadcast to
/// every kf so the param stays static across the whole scene.
///
/// The render frame uses the same `param_ids::POS_X` / `POS_Y` /
/// `SCALE` / `ROTATION` namespace as actor / overlay layouts so the
/// diamond toggle widget (`animated_toggle`) reads / writes the same
/// well-known strings without any per-element re-mapping. `zoom` is
/// what gets keyframed (the inspector's "scale" slider is just
/// `1.0 / zoom`).
pub fn write_render_frame_param<F>(
    layout: &mut Vec<Keyframe<memstroy_core::RenderFrameState>>,
    animated_params: &mut BTreeSet<String>,
    t: f32,
    param_id: &str,
    auto_animate_on_canvas_drag: bool,
    f: F,
) where
    F: Fn(&mut memstroy_core::RenderFrameState),
{
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, memstroy_core::RenderFrameState::default()));
    }
    if auto_animate_on_canvas_drag && t > 1.0e-3 {
        animated_params.insert(param_id.to_string());
    }
    let is_animated = animated_params.contains(param_id);
    if is_animated {
        let seed = keyframe::sample(layout, t)
            .unwrap_or(memstroy_core::RenderFrameState::default());
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

/// Canvas-layout (free canvas v2) variant. Honours the host element's
/// `animated_params` set — if no relevant param id is animated, the new
/// value is broadcast to every kf in the layout (single static value);
/// otherwise it's upserted at the playhead like the legacy paths.
///
/// `relevant_param_ids` lists the param ids whose animation flag should
/// gate this write. Most callers pass the position pair (`POS_X`, `POS_Y`)
/// for moves, or the scale/rotation single for resize / rotate. As long
/// as **any** of the listed ids is in `animated_params`, the write
/// upserts at `t`; otherwise it broadcasts.
pub fn write_canvas_param<F>(
    layout: &mut Vec<Keyframe<CanvasTransform>>,
    animated_params: &BTreeSet<String>,
    relevant_param_ids: &[&str],
    t: f32,
    f: F,
) where
    F: Fn(&mut CanvasTransform),
{
    if layout.is_empty() {
        layout.push(Keyframe::new(0.0, CanvasTransform::default()));
    }
    let any_animated = relevant_param_ids
        .iter()
        .any(|id| animated_params.contains(*id));
    if any_animated {
        let seed = keyframe::sample(layout, t).unwrap_or_default();
        let idx = upsert_index(layout, t, seed);
        if let Some(kf) = layout.get_mut(idx) {
            f(&mut kf.value);
        }
    } else {
        // Static: broadcast to every keyframe so the value stays
        // constant across the entire track. This matches what the
        // user expects when the per-param diamond is OFF — canvas
        // drags during playback should not silently spawn a
        // mid-track keyframe.
        for kf in layout.iter_mut() {
            f(&mut kf.value);
        }
    }
}

// ─── "Animated" toggle widget (inspector) ────────────────────────────

/// Render a small clickable diamond indicating whether `param_id` is in
/// `animated_params`, and toggle membership on click. Returns `true` when
/// the toggle changed this frame so the caller can refresh dependent UI.
///
/// The diamond is **painted directly** rather than rendered as a Unicode
/// glyph because egui's default font doesn't include U+2B25 / U+2B26 and
/// they showed up as empty squares — see the bug report from 2026‑05.
///
/// Visual:
/// - filled gold diamond  → param is animated (changes will create kfs)
/// - hollow gray diamond  → param is static (single value across track)
pub fn animated_toggle(
    ui: &mut egui::Ui,
    animated_params: &mut BTreeSet<String>,
    param_id: &str,
    salt: impl std::hash::Hash + Copy,
) -> bool {
    let _ = salt; // reserved for future per-instance ids
    let on = animated_params.contains(param_id);

    let (rect, resp) =
        ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::click());
    let center = rect.center();
    let half = 5.0_f32;
    let pts = vec![
        egui::pos2(center.x, center.y - half),
        egui::pos2(center.x + half, center.y),
        egui::pos2(center.x, center.y + half),
        egui::pos2(center.x - half, center.y),
    ];

    let hovered = resp.hovered();
    let (fill, stroke_col) = if on {
        let base = egui::Color32::from_rgb(255, 220, 80);
        let hov = egui::Color32::from_rgb(255, 240, 120);
        (
            if hovered { hov } else { base },
            egui::Color32::from_rgb(120, 90, 0),
        )
    } else {
        let base = egui::Color32::TRANSPARENT;
        let hov = egui::Color32::from_rgba_premultiplied(180, 180, 200, 40);
        (
            if hovered { hov } else { base },
            egui::Color32::from_rgb(140, 140, 160),
        )
    };
    ui.painter().add(egui::Shape::convex_polygon(
        pts,
        fill,
        egui::Stroke::new(1.4, stroke_col),
    ));

    let resp = resp.on_hover_text(if on {
        crate::i18n::t("Animated — click to lock to a single static value")
    } else {
        crate::i18n::t("Static — click to make this parameter animatable (changes will create keyframes)")
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
    /// Audio track at the given index in `Scene::audio`. Audio kfs
    /// live in dedicated per-param `Vec<Keyframe<f32>>` (volume_kfs,
    /// speed_kfs, …) rather than a shared `layout`, so the
    /// timeline / multi-select / delete / easing-change paths use
    /// [`audio_param_kfs_mut`] to resolve the right vec from a
    /// param id.
    Audio(usize),
}

/// Recognised audio param ids. The strings match
/// `AudioTrack::animated_params` membership.
pub mod audio_param_ids {
    pub const VOLUME: &str    = "volume";
    pub const SPEED: &str     = "speed";
    pub const PITCH: &str     = "pitch";
    pub const PAN: &str       = "pan";
    pub const LOW_PASS: &str  = "low_pass";
    pub const HIGH_PASS: &str = "high_pass";
    pub const REVERB: &str    = "reverb";

    /// All audio params in inspector / timeline display order.
    pub const ALL: &[&str] = &[
        VOLUME, SPEED, PITCH, PAN, LOW_PASS, HIGH_PASS, REVERB,
    ];

    /// Human-readable label for an audio param id. Returns the id back
    /// as a fallback for unknown / future ids.
    pub fn label(id: &str) -> &'static str {
        match id {
            VOLUME    => crate::i18n::t("Volume"),
            SPEED     => crate::i18n::t("Speed"),
            PITCH     => crate::i18n::t("Pitch"),
            PAN       => crate::i18n::t("Pan"),
            LOW_PASS  => crate::i18n::t("Low-pass"),
            HIGH_PASS => crate::i18n::t("High-pass"),
            REVERB    => crate::i18n::t("Reverb"),
            _         => crate::i18n::t("param"),
        }
    }
}

/// Resolve the `Vec<Keyframe<f32>>` track for one audio param. Returns
/// `None` for unknown ids so callers can transparently skip the
/// rendering / mutation step instead of panicking. Used by the
/// timeline param-row pipeline for click / drag / easing / delete on
/// audio keyframes.
pub fn audio_param_kfs_mut<'a>(
    audio: &'a mut memstroy_core::AudioTrack,
    param_id: &str,
) -> Option<&'a mut Vec<memstroy_core::Keyframe<f32>>> {
    use audio_param_ids as p;
    match param_id {
        p::VOLUME    => Some(&mut audio.volume_kfs),
        p::SPEED     => Some(&mut audio.speed_kfs),
        p::PITCH     => Some(&mut audio.pitch_kfs),
        p::PAN       => Some(&mut audio.pan_kfs),
        p::LOW_PASS  => Some(&mut audio.low_pass_kfs),
        p::HIGH_PASS => Some(&mut audio.high_pass_kfs),
        p::REVERB    => Some(&mut audio.reverb_kfs),
        _ => None,
    }
}

/// Read-only counterpart to [`audio_param_kfs_mut`]. Used by the
/// timeline param-row pipeline when building the
/// `(local_t, scene_t)` change-point lists without mutating the
/// scene.
///
/// **Currently unused** — the timeline path reads kfs through the
/// `Scene` borrow directly (see `compute_param_change_points`). Kept
/// public as a symmetric counterpart to `audio_param_kfs_mut` for
/// future read-side callers.
#[allow(dead_code)]
pub fn audio_param_kfs<'a>(
    audio: &'a memstroy_core::AudioTrack,
    param_id: &str,
) -> Option<&'a Vec<memstroy_core::Keyframe<f32>>> {
    use audio_param_ids as p;
    match param_id {
        p::VOLUME    => Some(&audio.volume_kfs),
        p::SPEED     => Some(&audio.speed_kfs),
        p::PITCH     => Some(&audio.pitch_kfs),
        p::PAN       => Some(&audio.pan_kfs),
        p::LOW_PASS  => Some(&audio.low_pass_kfs),
        p::HIGH_PASS => Some(&audio.high_pass_kfs),
        p::REVERB    => Some(&audio.reverb_kfs),
        _ => None,
    }
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

// ─── Per-parameter keyframe strip (inspector) — UNUSED SINCE 2026-05 ─
//
// A thin horizontal "ruler" used to be drawn directly under each
// parameter widget in the inspector. The widget rendered every
// keyframe as a diamond and supported click/drag/right-click for
// time and easing edits.
//
// Per the 2026-05 keyframe refactor, the inspector no longer
// displays keyframes at all — they live exclusively in the
// per-parameter rows under each element on the timeline (see
// `panels.rs::draw_param_kf_rows`). The widget below is kept here
// as a breadcrumb so a future contributor reaching for an in-line
// keyframe ruler has a starting point, but it is currently **not
// referenced** by any production code path. Hence the
// `#[allow(dead_code)]` on the type and the function.

/// Per-keyframe interaction reported back to the caller. Indices map
/// 1:1 to the `(time, easing)` pairs the caller passed in.
#[allow(dead_code)]
#[derive(Default, Debug, Clone)]
pub struct KfStripInteraction {
    /// Kf at this index was clicked (no drag, no modifier). Caller
    /// usually translates this into a playhead-seek.
    pub clicked_idx: Option<usize>,
    /// Kf at this index was dragged; the new clip-local time is in
    /// the second tuple slot. Already clamped to `[0, duration]`.
    pub dragged_idx_to: Option<(usize, f32)>,
    /// User selected a new easing for the kf at this index from the
    /// context menu. Caller writes this onto the matching `Keyframe<T>`.
    pub easing_changed: Option<(usize, memstroy_core::Easing)>,
}

/// Draw a horizontal keyframe strip and collect pointer interactions.
///
/// `times` and `easings` must have the same length. `duration` is the
/// clip-local span the strip represents (left edge = 0s, right edge =
/// duration). Pass `Some(playhead)` to draw a thin marker at the
/// current clip-local playhead.
#[allow(dead_code)]
pub fn keyframe_strip(
    ui: &mut egui::Ui,
    times: &[f32],
    easings: &[memstroy_core::Easing],
    duration: f32,
    playhead_local: Option<f32>,
    salt: impl std::hash::Hash + Copy,
) -> KfStripInteraction {
    debug_assert_eq!(times.len(), easings.len());

    let mut out = KfStripInteraction::default();
    let row_h = 16.0_f32;
    let avail_w = ui.available_width().max(40.0);
    let (rect, _bg_resp) =
        ui.allocate_exact_size(egui::Vec2::new(avail_w, row_h), egui::Sense::hover());

    // Subtle background ruler so the strip reads as "the time axis of
    // this parameter".
    ui.painter().rect_filled(
        rect,
        egui::Rounding::same(2.0),
        egui::Color32::from_rgba_premultiplied(255, 255, 255, 12),
    );
    // Horizontal centerline.
    let cy = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(rect.min.x, cy), egui::pos2(rect.max.x, cy)],
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 30),
        ),
    );

    let dur = duration.max(1.0e-3);
    let time_to_x = |t: f32| -> f32 {
        rect.min.x + (t / dur).clamp(0.0, 1.0) * rect.width()
    };
    let x_to_time = |x: f32| -> f32 {
        ((x - rect.min.x) / rect.width()).clamp(0.0, 1.0) * dur
    };

    // Playhead tick.
    if let Some(ph) = playhead_local {
        if ph >= 0.0 && ph <= dur {
            let x = time_to_x(ph);
            ui.painter().line_segment(
                [
                    egui::pos2(x, rect.min.y + 2.0),
                    egui::pos2(x, rect.max.y - 2.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 180, 80)),
            );
        }
    }

    let half = 5.0_f32;
    for (i, (t, easing)) in times.iter().zip(easings.iter()).enumerate() {
        let x = time_to_x(*t);
        let pts = vec![
            egui::pos2(x, cy - half),
            egui::pos2(x + half, cy),
            egui::pos2(x, cy + half),
            egui::pos2(x - half, cy),
        ];
        // Each easing flavour gets a slightly different fill so the
        // user can read "this is a Step" at a glance once they learn
        // the colour code.
        let fill = match *easing {
            memstroy_core::Easing::Step => egui::Color32::from_rgb(180, 180, 200),
            memstroy_core::Easing::Linear => egui::Color32::from_rgb(160, 200, 255),
            memstroy_core::Easing::EaseIn => egui::Color32::from_rgb(255, 200, 120),
            memstroy_core::Easing::EaseOut => egui::Color32::from_rgb(120, 220, 200),
            memstroy_core::Easing::EaseInOut => egui::Color32::from_rgb(255, 242, 0),
            memstroy_core::Easing::Cubic => egui::Color32::from_rgb(255, 160, 200),
        };

        let hit = egui::Rect::from_center_size(
            egui::pos2(x, cy),
            egui::Vec2::new(half * 2.5, row_h),
        );
        let id = ui.id().with(("kf_strip", &salt, i));
        let resp = ui.interact(hit, id, egui::Sense::click_and_drag());
        ui.painter().add(egui::Shape::convex_polygon(
            pts,
            fill,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(24, 22, 12)),
        ));

        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            // Tooltip: time + easing name so the user can read the kf
            // without having to open the context menu.
            let easing_name = match *easing {
                memstroy_core::Easing::Step => "Step",
                memstroy_core::Easing::Linear => "Linear",
                memstroy_core::Easing::EaseIn => "Ease in",
                memstroy_core::Easing::EaseOut => "Ease out",
                memstroy_core::Easing::EaseInOut => "Ease in/out",
                memstroy_core::Easing::Cubic => "Cubic",
            };
            egui::show_tooltip_text(
                ui.ctx(),
                ui.layer_id(),
                id.with("tooltip"),
                format!("kf {:.3}s · {}", t, easing_name),
            );
        }

        if resp.dragged() {
            let dx = resp.drag_delta().x;
            if dx.abs() > 0.0 {
                let new_x = x + dx;
                let new_t = x_to_time(new_x);
                out.dragged_idx_to = Some((i, new_t));
            }
        } else if resp.clicked() {
            out.clicked_idx = Some(i);
        }

        // Right-click context menu for picking the interpolation curve.
        // Each entry calls back into the outcome struct so the caller
        // can mutate its own kf vector.
        resp.context_menu(|ui| {
            ui.label(egui::RichText::new("Interpolation").size(10.0).strong());
            ui.separator();
            for (label, value) in [
                ("Linear", memstroy_core::Easing::Linear),
                ("Ease in", memstroy_core::Easing::EaseIn),
                ("Ease out", memstroy_core::Easing::EaseOut),
                ("Ease in/out", memstroy_core::Easing::EaseInOut),
                ("Step (hold)", memstroy_core::Easing::Step),
                ("Cubic", memstroy_core::Easing::Cubic),
            ] {
                let selected = *easing == value;
                if ui
                    .selectable_label(selected, label)
                    .clicked()
                {
                    out.easing_changed = Some((i, value));
                    ui.close_menu();
                }
            }
        });
    }

    out
}

// ─── Convenience writers tied to the strip outcome ───────────────────

/// Apply a [`KfStripInteraction`] to a `Vec<Keyframe<f32>>`. The drag
/// is clamped, sorted, and de-duplicated; any easing change is
/// persisted. Returns the new (still-valid) index of the kf the user
/// interacted with — useful when the caller wants to keep tracking it
/// after a sort. `None` when no interaction touched a kf.
///
/// Currently unused since the inspector strip helpers were retired in
/// 2026-05. Retained alongside `keyframe_strip` as a self-contained
/// reference implementation of "consume interaction outcome → mutate
/// kfs" for any future consumer.
#[allow(dead_code)]
pub fn apply_strip_to_f32_kfs(
    kfs: &mut Vec<memstroy_core::Keyframe<f32>>,
    interaction: &KfStripInteraction,
) -> Option<usize> {
    let mut acted_t: Option<f32> = None;
    if let Some((idx, new_t)) = interaction.dragged_idx_to {
        if idx < kfs.len() {
            kfs[idx].t = new_t.max(0.0);
            acted_t = Some(kfs[idx].t);
            kfs.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    if let Some((idx, easing)) = interaction.easing_changed {
        if idx < kfs.len() {
            kfs[idx].easing = easing;
            acted_t.get_or_insert(kfs[idx].t);
        }
    }
    acted_t.and_then(|t| {
        kfs.iter()
            .position(|k| (k.t - t).abs() < 1.0e-3)
    })
}

/// Same as [`apply_strip_to_f32_kfs`] but for any `Vec<Keyframe<T>>`.
/// Used by actor / overlay layouts where `T` is `ActorState` or
/// `OverlayState`. Easing change is per-kf; drag moves the kf time.
/// Note: when multiple parameters are co-animated at the same kf,
/// dragging shifts all of them together — that's a pragmatic
/// compromise for the shared-layout schema.
///
/// Currently unused: every actor / overlay strip in the inspector
/// uses the inline path inside `inspector_actor_param_strip` /
/// `inspector_overlay_param_strip` instead. Kept public so a future
/// "param-bag-style" strip helper that doesn't need typed access can
/// reuse it without re-deriving the sort / index dance.
#[allow(dead_code)]
pub fn apply_strip_to_kfs<T>(
    kfs: &mut Vec<memstroy_core::Keyframe<T>>,
    interaction: &KfStripInteraction,
) where
    T: Clone,
{
    if let Some((idx, new_t)) = interaction.dragged_idx_to {
        if idx < kfs.len() {
            kfs[idx].t = new_t.max(0.0);
            kfs.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        }
    }
    if let Some((idx, easing)) = interaction.easing_changed {
        if idx < kfs.len() {
            kfs[idx].easing = easing;
        }
    }
}
