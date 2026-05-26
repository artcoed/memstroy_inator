//! Curve editor panel — shows keyframe graphs for actor / overlay /
//! audio properties.
//!
//! Displays a time/value graph with draggable keyframe diamonds.
//! Supports bezier easing curves with visual control handles. Adding
//! a keyframe (via the + Key button or by double-clicking the graph)
//! also flags the underlying parameter as **animated** in the owning
//! element's `animated_params` set so subsequent edits in the
//! inspector consistently behave as keyframable.
//!
//! The panel is intentionally generic over the exact backing storage
//! — see `CurveEditorTarget` for the supported flavours. The host
//! window in `app.rs` decides which target to bind based on the
//! current `Selection` (with a dropdown picker when the user has
//! several elements multi-selected).

use std::collections::BTreeSet;

use egui::{Color32, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
use memstroy_core::{
    param_ids, ActorState, Easing, Keyframe, OverlayState, RenderFrameState,
};

/// Property indices for the curve editor (transform parameters).
pub const PROP_SCALE: usize = 0;
pub const PROP_POS_X: usize = 1;
pub const PROP_POS_Y: usize = 2;
pub const PROP_OPACITY: usize = 3;
pub const PROP_ROTATION: usize = 4;

/// Total number of transform-parameter slots.
const NUM_TRANSFORM_PROPS: usize = 5;

/// What kind of element the curve editor is currently bound to. The
/// caller hands one of these to [`curve_editor_panel`] together with
/// the playhead and clip duration; the panel takes care of mapping
/// `selected_property` to the right backing field.
pub enum CurveEditorTarget<'a> {
    /// Transform layout of an actor.
    Actor {
        layout: &'a mut Vec<Keyframe<ActorState>>,
        animated_params: &'a mut BTreeSet<String>,
    },
    /// Transform layout of a text / image / video overlay. `t_in`
    /// shifts the keyframe time-axis so the editor speaks scene-time
    /// (matching the actor track's frame-of-reference).
    Overlay {
        layout: &'a mut Vec<Keyframe<OverlayState>>,
        animated_params: &'a mut BTreeSet<String>,
        t_in: f32,
    },
    /// Audio layer: per-parameter scalar tracks for Volume / Speed /
    /// Pan. Only one parameter is shown at a time; the property
    /// selector switches between them.
    Audio {
        kfs: &'a mut Vec<Keyframe<f32>>,
        animated_params: &'a mut BTreeSet<String>,
        param_id: &'static str,
        param_label: &'static str,
        param_color: Color32,
        value_range: (f32, f32),
        /// Static value to seed a freshly inserted keyframe with when
        /// the track is empty.
        static_value: f32,
        /// Clip-local time of the playhead (audio kfs are stored in
        /// clip-local seconds).
        t_local: f32,
    },
    /// Transform layout of the render frame (output camera). The
    /// render frame is scene-time anchored (no `t_in` offset, like
    /// actors) and exposes Scale / Pos X / Pos Y / Rotation but no
    /// Opacity — the inspector's "Scale" slider maps to `1 / zoom`,
    /// so the curve editor mirrors that here for round-trip parity.
    RenderFrame {
        layout: &'a mut Vec<Keyframe<RenderFrameState>>,
        animated_params: &'a mut BTreeSet<String>,
    },
}

const PROPERTY_NAMES: &[&str] =
    &["Scale", "Pos X", "Pos Y", "Opacity", "Rotation"];

/// Map a transform-property slot to the param-id used in
/// `animated_params`.
fn prop_to_param_id(prop: usize) -> &'static str {
    match prop {
        PROP_SCALE => param_ids::SCALE,
        PROP_POS_X => param_ids::POS_X,
        PROP_POS_Y => param_ids::POS_Y,
        PROP_OPACITY => param_ids::OPACITY,
        PROP_ROTATION => param_ids::ROTATION,
        _ => param_ids::SCALE,
    }
}

/// Value range for each property (min, max).
fn property_range(prop: usize) -> (f32, f32) {
    match prop {
        PROP_SCALE => (0.0, 5.0),
        PROP_POS_X => (-1.0, 2.0),
        PROP_POS_Y => (-1.0, 2.0),
        PROP_OPACITY => (0.0, 1.0),
        PROP_ROTATION => (-360.0, 360.0),
        _ => (0.0, 1.0),
    }
}

fn get_actor_property(state: &ActorState, prop: usize) -> f32 {
    match prop {
        PROP_SCALE => state.scale,
        PROP_POS_X => state.pos[0],
        PROP_POS_Y => state.pos[1],
        PROP_OPACITY => state.opacity,
        PROP_ROTATION => state.rotation_deg,
        _ => 0.0,
    }
}

fn set_actor_property(state: &mut ActorState, prop: usize, value: f32) {
    match prop {
        PROP_SCALE => state.scale = value,
        PROP_POS_X => state.pos[0] = value,
        PROP_POS_Y => state.pos[1] = value,
        PROP_OPACITY => state.opacity = value,
        PROP_ROTATION => state.rotation_deg = value,
        _ => {}
    }
}

fn get_overlay_property(state: &OverlayState, prop: usize) -> f32 {
    match prop {
        PROP_SCALE => state.scale,
        PROP_POS_X => state.pos[0],
        PROP_POS_Y => state.pos[1],
        PROP_OPACITY => state.opacity,
        PROP_ROTATION => state.rotation_deg,
        _ => 0.0,
    }
}

fn set_overlay_property(state: &mut OverlayState, prop: usize, value: f32) {
    match prop {
        PROP_SCALE => state.scale = value,
        PROP_POS_X => state.pos[0] = value,
        PROP_POS_Y => state.pos[1] = value,
        PROP_OPACITY => state.opacity = value,
        PROP_ROTATION => state.rotation_deg = value,
        _ => {}
    }
}

/// Render-frame property accessor.
///
/// The inspector exposes `1 / zoom` as the user-facing "Scale"
/// concept (see `inspector_render_frame` in `panels.rs`). The
/// curve editor mirrors the same convention so the displayed
/// keyframe value matches what the inspector shows.
fn get_render_frame_property(state: &RenderFrameState, prop: usize) -> f32 {
    match prop {
        PROP_SCALE => 1.0 / state.zoom.max(1e-4),
        PROP_POS_X => state.pos.x,
        PROP_POS_Y => state.pos.y,
        // OPACITY isn't a render-frame concept; return 1.0 so any
        // accidental sample reads a no-op value rather than zero.
        PROP_OPACITY => 1.0,
        PROP_ROTATION => state.rotation_deg,
        _ => 0.0,
    }
}

fn set_render_frame_property(state: &mut RenderFrameState, prop: usize, value: f32) {
    match prop {
        PROP_SCALE => state.zoom = (1.0 / value.max(1e-4)).clamp(0.001, 1000.0),
        PROP_POS_X => state.pos.x = value,
        PROP_POS_Y => state.pos.y = value,
        // OPACITY isn't render-frame state; ignore writes so the
        // shared transform_curve_editor doesn't have to special-case
        // its diamond toggle for this target.
        PROP_OPACITY => {}
        PROP_ROTATION => state.rotation_deg = value,
        _ => {}
    }
}

/// Color for each property curve.
fn property_color(prop: usize) -> Color32 {
    match prop {
        PROP_SCALE => Color32::from_rgb(255, 180, 50),
        PROP_POS_X => Color32::from_rgb(255, 80, 80),
        PROP_POS_Y => Color32::from_rgb(80, 255, 80),
        PROP_OPACITY => Color32::from_rgb(200, 200, 255),
        PROP_ROTATION => Color32::from_rgb(255, 100, 255),
        _ => Color32::WHITE,
    }
}

// ─── Common geometry helpers ─────────────────────────────────────────

fn time_to_graph_x(t: f32, t_min: f32, t_max: f32, rect: Rect) -> f32 {
    let frac = (t - t_min) / (t_max - t_min).max(1e-6);
    rect.min.x + frac * rect.width()
}

fn value_to_graph_y(v: f32, v_min: f32, v_max: f32, rect: Rect) -> f32 {
    let frac = (v - v_min) / (v_max - v_min).max(1e-6);
    rect.max.y - frac * rect.height()
}

fn graph_x_to_time(x: f32, t_min: f32, t_max: f32, rect: Rect) -> f32 {
    let frac = (x - rect.min.x) / rect.width().max(1.0);
    t_min + frac * (t_max - t_min)
}

fn graph_y_to_value(y: f32, v_min: f32, v_max: f32, rect: Rect) -> f32 {
    let frac = (rect.max.y - y) / rect.height().max(1.0);
    v_min + frac * (v_max - v_min)
}

/// Cycle to the next easing in a fixed order. Used by the right-click
/// menu *and* the "next easing" toolbar button so users can quickly
/// preview every interpolation flavour without diving into the menu.
#[allow(dead_code)]
fn cycle_easing(e: Easing) -> Easing {
    match e {
        Easing::Linear => Easing::EaseIn,
        Easing::EaseIn => Easing::EaseOut,
        Easing::EaseOut => Easing::EaseInOut,
        Easing::EaseInOut => Easing::Cubic,
        Easing::Cubic => Easing::Step,
        Easing::Step => Easing::Linear,
    }
}

/// Render a small "Easing for selected kf" pair of menus that lets
/// the user pick interpolation curves on the *currently focused*
/// keyframe. Returns the new easing if the user picked one, or
/// `None` if the menu was closed without changes.
fn easing_picker(ui: &mut egui::Ui, current: Easing) -> Option<Easing> {
    let mut picked = None;
    egui::ComboBox::from_id_source("curve_editor_easing")
        .selected_text(easing_label(current))
        .show_ui(ui, |ui| {
            for e in [
                Easing::Linear,
                Easing::EaseIn,
                Easing::EaseOut,
                Easing::EaseInOut,
                Easing::Cubic,
                Easing::Step,
            ] {
                if ui
                    .selectable_label(current == e, easing_label(e))
                    .clicked()
                {
                    picked = Some(e);
                }
            }
        });
    picked
}

fn easing_label(e: Easing) -> &'static str {
    match e {
        Easing::Linear => crate::i18n::t("Linear"),
        Easing::EaseIn => crate::i18n::t("Ease in"),
        Easing::EaseOut => crate::i18n::t("Ease out"),
        Easing::EaseInOut => crate::i18n::t("Ease in/out"),
        Easing::Cubic => crate::i18n::t("Cubic"),
        Easing::Step => crate::i18n::t("Step (hold)"),
    }
}

/// Draw the curve editor panel for the given target.
///
/// `selected_property` is only meaningful for Actor / Overlay targets
/// (transform property index). For Audio targets the property is fixed
/// and the slot is ignored.
pub fn curve_editor_panel(
    ui: &mut egui::Ui,
    target: CurveEditorTarget<'_>,
    duration: f32,
    selected_property: &mut usize,
    playhead: f32,
) {
    use crate::i18n::t;

    match target {
        CurveEditorTarget::Actor { layout, animated_params } => {
            transform_curve_editor::<ActorState>(
                ui,
                layout,
                animated_params,
                duration,
                selected_property,
                playhead,
                /* time_offset */ 0.0,
                get_actor_property,
                set_actor_property,
                ActorState::default,
            );
        }
        CurveEditorTarget::Overlay { layout, animated_params, t_in } => {
            transform_curve_editor::<OverlayState>(
                ui,
                layout,
                animated_params,
                duration,
                selected_property,
                playhead,
                /* time_offset */ t_in,
                get_overlay_property,
                set_overlay_property,
                OverlayState::default,
            );
        }
        CurveEditorTarget::RenderFrame { layout, animated_params } => {
            // Render-frame kfs are stored in scene time, same as
            // actors — pass `time_offset = 0.0` so the editor's
            // X-axis is direct scene-time.
            transform_curve_editor::<RenderFrameState>(
                ui,
                layout,
                animated_params,
                duration,
                selected_property,
                playhead,
                /* time_offset */ 0.0,
                get_render_frame_property,
                set_render_frame_property,
                RenderFrameState::default,
            );
        }
        CurveEditorTarget::Audio {
            kfs,
            animated_params,
            param_id,
            param_label,
            param_color,
            value_range,
            static_value,
            t_local,
        } => {
            // Header for audio param.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(t("Curve Editor"))
                        .size(13.0)
                        .strong()
                        .color(Color32::WHITE),
                );
                ui.separator();
                ui.label(
                    egui::RichText::new(t(param_label))
                        .size(11.0)
                        .color(param_color),
                );
                let on = animated_params.contains(param_id);
                let toggle_label = if on { t("Animated") } else { t("Static") };
                if ui
                    .selectable_label(on, toggle_label)
                    .on_hover_text(t(
                        "Toggle whether this parameter is animatable (changes will create keyframes)",
                    ))
                    .clicked()
                {
                    if on {
                        animated_params.remove(param_id);
                    } else {
                        animated_params.insert(param_id.to_string());
                    }
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .small_button(t("+ Key"))
                            .on_hover_text(t("Add keyframe at playhead"))
                            .clicked()
                        {
                            let value = sample_f32_kfs(kfs, t_local, static_value);
                            kfs.push(Keyframe::new(t_local.max(0.0), value));
                            kfs.sort_by(|a, b| {
                                a.t.partial_cmp(&b.t).unwrap()
                            });
                            animated_params.insert(param_id.to_string());
                        }
                    },
                );
            });

            ui.add_space(4.0);
            scalar_curve_editor(
                ui,
                kfs,
                animated_params,
                param_id,
                param_color,
                duration,
                value_range,
                static_value,
                t_local,
            );
        }
    }
}

/// Sample a `Vec<Keyframe<f32>>` track at time `t`, falling back to
/// `default` for empty tracks. Used by the audio curve editor.
fn sample_f32_kfs(kfs: &[Keyframe<f32>], t: f32, default: f32) -> f32 {
    memstroy_core::keyframe::sample(kfs, t).unwrap_or(default)
}

/// Generic transform-curve editor used by both Actor and Overlay
/// targets. Templated over the keyframe value type `T`. The caller
/// supplies the property accessor / mutator and a default-state
/// constructor used to seed the very first keyframe.
#[allow(clippy::too_many_arguments)]
fn transform_curve_editor<T>(
    ui: &mut egui::Ui,
    keyframes: &mut Vec<Keyframe<T>>,
    animated_params: &mut BTreeSet<String>,
    duration: f32,
    selected_property: &mut usize,
    playhead: f32,
    time_offset: f32,
    get_property: fn(&T, usize) -> f32,
    set_property: fn(&mut T, usize, f32),
    default_value: fn() -> T,
) where
    T: Clone,
{
    use crate::i18n::t;
    // ── Property selector toolbar ──
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(t("Curve Editor"))
                .size(13.0)
                .strong()
                .color(Color32::WHITE),
        );
        ui.separator();
        for (i, name) in PROPERTY_NAMES.iter().enumerate() {
            let color = property_color(i);
            let selected = *selected_property == i;
            let param_id = prop_to_param_id(i);
            let is_animated = animated_params.contains(param_id);
            // Add a small diamond marker to the label when the
            // parameter is currently flagged as animated so the user
            // can tell at a glance which curve they're authoring.
            let prefix = if is_animated { "\u{25C6} " } else { "" };
            let display = format!("{}{}", prefix, t(*name));
            let text = egui::RichText::new(display).size(11.0).color(if selected {
                color
            } else if is_animated {
                Color32::from_rgb(200, 180, 120)
            } else {
                Color32::from_rgb(140, 140, 160)
            });
            if ui.selectable_label(selected, text).clicked() {
                *selected_property = i;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(t("+ Key"))
                .on_hover_text(t("Add keyframe at playhead"))
                .clicked()
            {
                // The keyframe time stored on the layout is "scene
                // time minus t_in" so it always matches the local
                // frame the rest of the editor uses for that flavour
                // of element.
                let local_t = (playhead - time_offset).max(0.0);
                let value = interpolate_at::<T>(
                    keyframes,
                    local_t,
                    *selected_property,
                    get_property,
                );
                let mut new_state = keyframes
                    .last()
                    .map(|kf| kf.value.clone())
                    .unwrap_or_else(default_value);
                set_property(&mut new_state, *selected_property, value);
                keyframes.push(Keyframe::new(local_t, new_state));
                keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
                // Mark the parameter as animated — adding a kf is a
                // strong signal of intent.
                animated_params.insert(prop_to_param_id(*selected_property).to_string());
            }
        });
    });

    ui.add_space(4.0);

    // ── Compute which keyframes are relevant for the selected property ──
    // A keyframe is "relevant" if it's the first kf, or the value of the
    // selected property differs from the previous kf. This prevents
    // position-only keyframes from cluttering the Scale view (the core
    // bug report: "position kfs showing in scale menu").
    let prop = *selected_property;
    let relevant_indices: Vec<usize> = {
        let mut indices = Vec::new();
        const EPS: f32 = 1.0e-4;
        for (ki, kf) in keyframes.iter().enumerate() {
            if ki == 0 {
                // Always include the first kf — it establishes the
                // initial value for the property.
                indices.push(ki);
            } else {
                let prev_val = get_property(&keyframes[ki - 1].value, prop);
                let cur_val = get_property(&kf.value, prop);
                if (cur_val - prev_val).abs() > EPS {
                    indices.push(ki);
                }
            }
        }
        indices
    };

    // ── Easing picker for the selected kf at the playhead ──
    // Only considers keyframes that are relevant to the selected property
    // so the user isn't editing easing on a position-only kf while viewing
    // the Scale curve.
    if !relevant_indices.is_empty() {
        let local_t = (playhead - time_offset).max(0.0);
        let nearest_idx = relevant_indices
            .iter()
            .copied()
            .min_by(|&a, &b| {
                let da = (keyframes[a].t - local_t).abs();
                let db = (keyframes[b].t - local_t).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t("Transition into kf:"))
                    .size(10.5)
                    .color(Color32::from_rgb(160, 160, 180)),
            );
            let cur_easing = keyframes[nearest_idx].easing;
            if let Some(next) = easing_picker(ui, cur_easing) {
                keyframes[nearest_idx].easing = next;
            }
            // Show the kf number relative to relevant-only count so
            // the user reads "kf #2 of 3" (only property-relevant kfs)
            // instead of "kf #5 of 12" (all kfs including other props).
            let display_num = relevant_indices
                .iter()
                .position(|&i| i == nearest_idx)
                .map(|p| p + 1)
                .unwrap_or(1);
            ui.label(
                egui::RichText::new(format!(
                    "kf #{} @ {:.2}s",
                    display_num,
                    keyframes[nearest_idx].t,
                ))
                .size(10.0)
                .color(Color32::from_rgb(120, 120, 140)),
            );
        });
        ui.add_space(2.0);
    }

    // ── Graph area ──
    let available = ui.available_size();
    let graph_height = (available.y - 8.0).max(60.0);
    let graph_width = available.x;

    let (graph_rect, response) = ui.allocate_exact_size(
        Vec2::new(graph_width, graph_height),
        Sense::click_and_drag(),
    );

    let painter = ui.painter_at(graph_rect);

    painter.rect_filled(graph_rect, Rounding::same(4.0), Color32::from_rgb(16, 15, 8));
    painter.rect_stroke(
        graph_rect,
        Rounding::same(4.0),
        Stroke::new(1.0, Color32::from_rgb(44, 42, 28)),
    );

    let (val_min, val_max) = property_range(prop);
    let time_min = 0.0_f32;
    let time_max = duration.max(0.1);

    let margin = 4.0;
    let inner_rect = graph_rect.shrink(margin);

    draw_grid(&painter, inner_rect, time_min, time_max, val_min, val_max);

    // Playhead indicator (rendered in local time so it lines up with
    // the kf positions).
    let local_playhead = (playhead - time_offset).max(0.0);
    let ph_x = time_to_graph_x(local_playhead, time_min, time_max, inner_rect);
    if ph_x >= inner_rect.min.x && ph_x <= inner_rect.max.x {
        painter.line_segment(
            [
                Pos2::new(ph_x, inner_rect.min.y),
                Pos2::new(ph_x, inner_rect.max.y),
            ],
            Stroke::new(1.0, Color32::from_rgb(255, 60, 60)),
        );
    }

    let curve_color = property_color(prop);

    // Draw the curve segments.
    if keyframes.len() >= 2 {
        for pair in keyframes.windows(2) {
            let (kf_a, kf_b) = (&pair[0], &pair[1]);
            let va = get_property(&kf_a.value, prop);
            let vb = get_property(&kf_b.value, prop);
            let xa = time_to_graph_x(kf_a.t, time_min, time_max, inner_rect);
            let ya = value_to_graph_y(va, val_min, val_max, inner_rect);
            let xb = time_to_graph_x(kf_b.t, time_min, time_max, inner_rect);
            let yb = value_to_graph_y(vb, val_min, val_max, inner_rect);

            let num_samples = 24;
            let mut prev_point = Pos2::new(xa, ya);
            for i in 1..=num_samples {
                let frac = i as f32 / num_samples as f32;
                let eased = kf_b.easing.apply(frac);
                let px = xa + frac * (xb - xa);
                let py = ya + eased * (yb - ya);
                let cur_point = Pos2::new(px, py);
                painter.line_segment(
                    [prev_point, cur_point],
                    Stroke::new(1.5, curve_color),
                );
                prev_point = cur_point;
            }
        }
    }

    // Draw keyframe diamonds (draggable).
    // Only keyframes RELEVANT to the selected property get full-size
    // interactive diamonds. Irrelevant kfs (where the selected property
    // didn't change) are shown as tiny dimmed dots so the user still
    // sees the overall timeline structure but can't confuse them with
    // actual property changes — this fixes the "position kfs showing
    // in scale menu" bug.
    let diamond_size = 6.0;
    let mut drag_idx: Option<usize> = None;
    let mut delete_idx: Option<usize> = None;
    let mut easing_change: Option<(usize, Easing)> = None;

    for (ki, kf) in keyframes.iter().enumerate() {
        let v = get_property(&kf.value, prop);
        let cx = time_to_graph_x(kf.t, time_min, time_max, inner_rect);
        let cy = value_to_graph_y(v, val_min, val_max, inner_rect);
        let center = Pos2::new(cx, cy);

        let is_relevant = relevant_indices.contains(&ki);

        if !is_relevant {
            // Irrelevant kf: draw a tiny dimmed circle as a hint, no
            // interaction. This prevents the user from accidentally
            // dragging/editing a kf that belongs to another property.
            let ghost_r = 2.5;
            painter.circle_filled(
                center,
                ghost_r,
                Color32::from_rgba_premultiplied(140, 140, 160, 60),
            );
            continue;
        }

        let diamond_points = vec![
            Pos2::new(center.x, center.y - diamond_size),
            Pos2::new(center.x + diamond_size, center.y),
            Pos2::new(center.x, center.y + diamond_size),
            Pos2::new(center.x - diamond_size, center.y),
        ];

        painter.add(egui::Shape::convex_polygon(
            diamond_points,
            curve_color,
            Stroke::new(1.0, Color32::WHITE),
        ));

        if keyframes.len() > 1 && ki > 0 {
            let easing_l = match kf.easing {
                Easing::Step => "Step",
                Easing::Linear => "",
                Easing::EaseIn => "In",
                Easing::EaseOut => "Out",
                Easing::EaseInOut => "InOut",
                Easing::Cubic => "Cubic",
            };
            if !easing_l.is_empty() {
                painter.text(
                    Pos2::new(center.x, center.y - diamond_size - 6.0),
                    egui::Align2::CENTER_BOTTOM,
                    easing_l,
                    egui::FontId::proportional(8.0),
                    Color32::WHITE,
                );
            }
        }

        let diamond_rect = Rect::from_center_size(center, Vec2::splat(diamond_size * 2.5));
        let id = ui.make_persistent_id(("curve_kf", ki));
        let kf_resp = ui.interact(diamond_rect, id, Sense::click_and_drag());

        if kf_resp.dragged() {
            drag_idx = Some(ki);
        }
        kf_resp.context_menu(|ui| {
            ui.label(egui::RichText::new(t("Interpolation")).size(10.0).strong());
            ui.separator();
            for (label, value) in [
                ("Linear", Easing::Linear),
                ("Ease in", Easing::EaseIn),
                ("Ease out", Easing::EaseOut),
                ("Ease in/out", Easing::EaseInOut),
                ("Step (hold)", Easing::Step),
                ("Cubic", Easing::Cubic),
            ] {
                let selected = kf.easing == value;
                if ui.selectable_label(selected, t(label)).clicked() {
                    easing_change = Some((ki, value));
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui.selectable_label(false, t("Delete keyframe")).clicked() {
                delete_idx = Some(ki);
                ui.close_menu();
            }
        });
    }

    if let Some((ki, e)) = easing_change {
        if let Some(kf) = keyframes.get_mut(ki) {
            kf.easing = e;
        }
    }
    if let Some(ki) = delete_idx {
        if ki < keyframes.len() {
            keyframes.remove(ki);
        }
    }

    if let Some(ki) = drag_idx {
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            let new_t = graph_x_to_time(pos.x, time_min, time_max, inner_rect)
                .clamp(0.0, time_max);
            let new_v = graph_y_to_value(pos.y, val_min, val_max, inner_rect)
                .clamp(val_min, val_max);
            keyframes[ki].t = new_t;
            set_property(&mut keyframes[ki].value, prop, new_v);
        }
    }

    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let new_t = graph_x_to_time(pos.x, time_min, time_max, inner_rect)
                .clamp(0.0, time_max);
            let new_v = graph_y_to_value(pos.y, val_min, val_max, inner_rect)
                .clamp(val_min, val_max);
            let mut new_state = keyframes
                .last()
                .map(|kf| kf.value.clone())
                .unwrap_or_else(default_value);
            set_property(&mut new_state, prop, new_v);
            keyframes.push(Keyframe::new(new_t, new_state));
            keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
            animated_params.insert(prop_to_param_id(prop).to_string());
        }
    }

    let _ = NUM_TRANSFORM_PROPS;
}

/// Audio scalar (Vec<Keyframe<f32>>) curve editor — single value over
/// time, no property selector. Drag to move kfs; right-click for the
/// easing menu; double-click empty to add. Adding marks the param as
/// animated.
#[allow(clippy::too_many_arguments)]
fn scalar_curve_editor(
    ui: &mut egui::Ui,
    kfs: &mut Vec<Keyframe<f32>>,
    animated_params: &mut BTreeSet<String>,
    param_id: &str,
    color: Color32,
    duration: f32,
    value_range: (f32, f32),
    static_value: f32,
    t_local: f32,
) {
    use crate::i18n::t;
    let available = ui.available_size();
    let graph_height = (available.y - 8.0).max(60.0);
    let graph_width = available.x;

    let (graph_rect, response) = ui.allocate_exact_size(
        Vec2::new(graph_width, graph_height),
        Sense::click_and_drag(),
    );

    let painter = ui.painter_at(graph_rect);
    painter.rect_filled(graph_rect, Rounding::same(4.0), Color32::from_rgb(16, 15, 8));
    painter.rect_stroke(
        graph_rect,
        Rounding::same(4.0),
        Stroke::new(1.0, Color32::from_rgb(44, 42, 28)),
    );

    let (val_min, val_max) = value_range;
    let time_min = 0.0_f32;
    let time_max = duration.max(0.1);
    let inner_rect = graph_rect.shrink(4.0);

    draw_grid(&painter, inner_rect, time_min, time_max, val_min, val_max);

    let ph_x = time_to_graph_x(t_local.max(0.0), time_min, time_max, inner_rect);
    if ph_x >= inner_rect.min.x && ph_x <= inner_rect.max.x {
        painter.line_segment(
            [
                Pos2::new(ph_x, inner_rect.min.y),
                Pos2::new(ph_x, inner_rect.max.y),
            ],
            Stroke::new(1.0, Color32::from_rgb(255, 60, 60)),
        );
    }

    if kfs.len() >= 2 {
        for pair in kfs.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let xa = time_to_graph_x(a.t, time_min, time_max, inner_rect);
            let ya = value_to_graph_y(a.value, val_min, val_max, inner_rect);
            let xb = time_to_graph_x(b.t, time_min, time_max, inner_rect);
            let yb = value_to_graph_y(b.value, val_min, val_max, inner_rect);

            let num_samples = 24;
            let mut prev_point = Pos2::new(xa, ya);
            for i in 1..=num_samples {
                let frac = i as f32 / num_samples as f32;
                let eased = b.easing.apply(frac);
                let px = xa + frac * (xb - xa);
                let py = ya + eased * (yb - ya);
                let cur_point = Pos2::new(px, py);
                painter.line_segment(
                    [prev_point, cur_point],
                    Stroke::new(1.5, color),
                );
                prev_point = cur_point;
            }
        }
    }

    let diamond_size = 6.0;
    let mut drag_idx: Option<usize> = None;
    let mut delete_idx: Option<usize> = None;
    let mut easing_change: Option<(usize, Easing)> = None;

    for (ki, kf) in kfs.iter().enumerate() {
        let cx = time_to_graph_x(kf.t, time_min, time_max, inner_rect);
        let cy = value_to_graph_y(kf.value, val_min, val_max, inner_rect);
        let center = Pos2::new(cx, cy);
        let pts = vec![
            Pos2::new(center.x, center.y - diamond_size),
            Pos2::new(center.x + diamond_size, center.y),
            Pos2::new(center.x, center.y + diamond_size),
            Pos2::new(center.x - diamond_size, center.y),
        ];
        painter.add(egui::Shape::convex_polygon(
            pts,
            color,
            Stroke::new(1.0, Color32::WHITE),
        ));

        let diamond_rect =
            Rect::from_center_size(center, Vec2::splat(diamond_size * 2.5));
        let id = ui.make_persistent_id(("audio_curve_kf", ki));
        let kf_resp = ui.interact(diamond_rect, id, Sense::click_and_drag());
        if kf_resp.dragged() {
            drag_idx = Some(ki);
        }
        kf_resp.context_menu(|ui| {
            ui.label(egui::RichText::new(t("Interpolation")).size(10.0).strong());
            ui.separator();
            for (label, value) in [
                ("Linear", Easing::Linear),
                ("Ease in", Easing::EaseIn),
                ("Ease out", Easing::EaseOut),
                ("Ease in/out", Easing::EaseInOut),
                ("Step (hold)", Easing::Step),
                ("Cubic", Easing::Cubic),
            ] {
                let selected = kf.easing == value;
                if ui.selectable_label(selected, t(label)).clicked() {
                    easing_change = Some((ki, value));
                    ui.close_menu();
                }
            }
            ui.separator();
            if ui.selectable_label(false, t("Delete keyframe")).clicked() {
                delete_idx = Some(ki);
                ui.close_menu();
            }
        });
    }

    if let Some((ki, e)) = easing_change {
        if let Some(kf) = kfs.get_mut(ki) {
            kf.easing = e;
        }
    }
    if let Some(ki) = delete_idx {
        if ki < kfs.len() {
            kfs.remove(ki);
        }
    }
    if let Some(ki) = drag_idx {
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            let new_t = graph_x_to_time(pos.x, time_min, time_max, inner_rect)
                .clamp(0.0, time_max);
            let new_v = graph_y_to_value(pos.y, val_min, val_max, inner_rect)
                .clamp(val_min, val_max);
            kfs[ki].t = new_t;
            kfs[ki].value = new_v;
        }
    }
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let new_t = graph_x_to_time(pos.x, time_min, time_max, inner_rect)
                .clamp(0.0, time_max);
            let new_v = graph_y_to_value(pos.y, val_min, val_max, inner_rect)
                .clamp(val_min, val_max);
            // Seed the very first kf with the static value at t=0 so
            // the curve has a sensible starting point.
            if kfs.is_empty() {
                kfs.push(Keyframe::new(0.0, static_value));
            }
            kfs.push(Keyframe::new(new_t, new_v));
            kfs.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
            animated_params.insert(param_id.to_string());
        }
    }
}

/// Interpolate the current property value at a given time using easing.
fn interpolate_at<T>(
    keyframes: &[Keyframe<T>],
    t: f32,
    prop: usize,
    get_property: fn(&T, usize) -> f32,
) -> f32 {
    if keyframes.is_empty() {
        return match prop {
            PROP_SCALE => 1.0,
            PROP_OPACITY => 1.0,
            _ => 0.0,
        };
    }
    if keyframes.len() == 1 || t <= keyframes[0].t {
        return get_property(&keyframes[0].value, prop);
    }
    let last = keyframes.last().unwrap();
    if t >= last.t {
        return get_property(&last.value, prop);
    }
    for pair in keyframes.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if t >= a.t && t <= b.t {
            let span = (b.t - a.t).max(1e-6);
            let raw_frac = (t - a.t) / span;
            let eased_frac = b.easing.apply(raw_frac);
            let va = get_property(&a.value, prop);
            let vb = get_property(&b.value, prop);
            return va + (vb - va) * eased_frac;
        }
    }
    get_property(&last.value, prop)
}

/// Draw background grid lines.
fn draw_grid(painter: &egui::Painter, rect: Rect, t_min: f32, t_max: f32, v_min: f32, v_max: f32) {
    let grid_color = Color32::from_rgb(32, 30, 20);
    let text_color = Color32::from_rgb(80, 80, 100);

    let num_h_lines = 5;
    for i in 0..=num_h_lines {
        let frac = i as f32 / num_h_lines as f32;
        let y = rect.max.y - frac * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5, grid_color),
        );
        let v = v_min + frac * (v_max - v_min);
        painter.text(
            Pos2::new(rect.min.x + 2.0, y - 8.0),
            egui::Align2::LEFT_TOP,
            format!("{:.1}", v),
            egui::FontId::proportional(8.0),
            text_color,
        );
    }

    let time_span = t_max - t_min;
    let step = if time_span > 20.0 {
        5.0
    } else if time_span > 5.0 {
        1.0
    } else {
        0.5
    };
    let mut t = (t_min / step).ceil() * step;
    while t <= t_max {
        let x = time_to_graph_x(t, t_min, t_max, rect);
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
            Stroke::new(0.5, grid_color),
        );
        painter.text(
            Pos2::new(x, rect.max.y - 10.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{:.1}s", t),
            egui::FontId::proportional(8.0),
            text_color,
        );
        t += step;
    }
}
