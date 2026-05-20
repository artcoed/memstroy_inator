//! Curve editor panel — shows keyframe graphs for actor properties.
//!
//! Displays a time/value graph with draggable keyframe diamonds.
//! Currently supports linear interpolation only (no bezier handles).

use egui::{Color32, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
use memstroy_core::{ActorState, Keyframe};

/// Property indices for the curve editor.
pub const PROP_SCALE: usize = 0;
pub const PROP_POS_X: usize = 1;
pub const PROP_POS_Y: usize = 2;
pub const PROP_OPACITY: usize = 3;
pub const PROP_ROTATION: usize = 4;

const PROPERTY_NAMES: &[&str] = &["Scale", "Pos X", "Pos Y", "Opacity", "Rotation"];

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

/// Get the value of a property from an ActorState.
fn get_property(state: &ActorState, prop: usize) -> f32 {
    match prop {
        PROP_SCALE => state.scale,
        PROP_POS_X => state.pos[0],
        PROP_POS_Y => state.pos[1],
        PROP_OPACITY => state.opacity,
        PROP_ROTATION => state.rotation_deg,
        _ => 0.0,
    }
}

/// Set the value of a property on an ActorState.
fn set_property(state: &mut ActorState, prop: usize, value: f32) {
    match prop {
        PROP_SCALE => state.scale = value,
        PROP_POS_X => state.pos[0] = value,
        PROP_POS_Y => state.pos[1] = value,
        PROP_OPACITY => state.opacity = value,
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

/// Draw the curve editor panel.
///
/// `keyframes` is the actor's layout keyframe track.
/// `duration` is the scene duration.
/// `selected_property` is a mutable reference to which property is being edited.
/// `playhead` is the current playhead time for the time indicator.
pub fn curve_editor_panel(
    ui: &mut egui::Ui,
    keyframes: &mut Vec<Keyframe<ActorState>>,
    duration: f32,
    selected_property: &mut usize,
    playhead: f32,
) {
    // ── Property selector toolbar ──
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Curve Editor")
                .size(13.0)
                .strong()
                .color(Color32::from_rgb(200, 180, 255)),
        );
        ui.separator();
        for (i, name) in PROPERTY_NAMES.iter().enumerate() {
            let color = property_color(i);
            let selected = *selected_property == i;
            let text = egui::RichText::new(*name).size(11.0).color(if selected {
                color
            } else {
                Color32::from_rgb(140, 140, 160)
            });
            if ui.selectable_label(selected, text).clicked() {
                *selected_property = i;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("+ Key").on_hover_text("Add keyframe at playhead").clicked() {
                // Add a keyframe at the current playhead position
                let value = interpolate_at(keyframes, playhead, *selected_property);
                let mut new_state = keyframes
                    .last()
                    .map(|kf| kf.value)
                    .unwrap_or_default();
                set_property(&mut new_state, *selected_property, value);
                keyframes.push(Keyframe::new(playhead, new_state));
                keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
            }
        });
    });

    ui.add_space(4.0);

    // ── Graph area ──
    let available = ui.available_size();
    let graph_height = (available.y - 8.0).max(60.0);
    let graph_width = available.x;

    let (graph_rect, response) = ui.allocate_exact_size(
        Vec2::new(graph_width, graph_height),
        Sense::click_and_drag(),
    );

    let painter = ui.painter_at(graph_rect);

    // Dark background
    painter.rect_filled(graph_rect, Rounding::same(4.0), Color32::from_rgb(12, 12, 20));

    // Border
    painter.rect_stroke(
        graph_rect,
        Rounding::same(4.0),
        Stroke::new(1.0, Color32::from_rgb(40, 40, 60)),
    );

    let prop = *selected_property;
    let (val_min, val_max) = property_range(prop);
    let time_min = 0.0_f32;
    let time_max = duration.max(0.1);

    let margin = 4.0;
    let inner_rect = graph_rect.shrink(margin);

    // ── Grid lines ──
    draw_grid(&painter, inner_rect, time_min, time_max, val_min, val_max);

    // ── Playhead indicator ──
    let ph_x = time_to_graph_x(playhead, time_min, time_max, inner_rect);
    if ph_x >= inner_rect.min.x && ph_x <= inner_rect.max.x {
        painter.line_segment(
            [
                Pos2::new(ph_x, inner_rect.min.y),
                Pos2::new(ph_x, inner_rect.max.y),
            ],
            Stroke::new(1.0, Color32::from_rgb(255, 60, 60)),
        );
    }

    // ── Draw curve (linear segments between keyframes) ──
    let curve_color = property_color(prop);
    if keyframes.len() >= 2 {
        for pair in keyframes.windows(2) {
            let (kf_a, kf_b) = (&pair[0], &pair[1]);
            let va = get_property(&kf_a.value, prop);
            let vb = get_property(&kf_b.value, prop);
            let xa = time_to_graph_x(kf_a.t, time_min, time_max, inner_rect);
            let ya = value_to_graph_y(va, val_min, val_max, inner_rect);
            let xb = time_to_graph_x(kf_b.t, time_min, time_max, inner_rect);
            let yb = value_to_graph_y(vb, val_min, val_max, inner_rect);
            painter.line_segment(
                [Pos2::new(xa, ya), Pos2::new(xb, yb)],
                Stroke::new(1.5, curve_color),
            );
        }
    }

    // ── Draw keyframe diamonds (draggable) ──
    let diamond_size = 6.0;
    let mut drag_idx: Option<usize> = None;

    for (ki, kf) in keyframes.iter().enumerate() {
        let v = get_property(&kf.value, prop);
        let cx = time_to_graph_x(kf.t, time_min, time_max, inner_rect);
        let cy = value_to_graph_y(v, val_min, val_max, inner_rect);
        let center = Pos2::new(cx, cy);

        // Diamond shape (rotated square)
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

        // Check if this diamond is being dragged
        let diamond_rect = Rect::from_center_size(center, Vec2::splat(diamond_size * 2.5));
        let id = ui.make_persistent_id(("curve_kf", ki));
        let kf_resp = ui.interact(diamond_rect, id, Sense::click_and_drag());

        if kf_resp.dragged() {
            drag_idx = Some(ki);
        }
    }

    // Apply drag if any
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

    // Double-click to add a keyframe
    if response.double_clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let new_t = graph_x_to_time(pos.x, time_min, time_max, inner_rect)
                .clamp(0.0, time_max);
            let new_v = graph_y_to_value(pos.y, val_min, val_max, inner_rect)
                .clamp(val_min, val_max);
            let mut new_state = keyframes
                .last()
                .map(|kf| kf.value)
                .unwrap_or_default();
            set_property(&mut new_state, prop, new_v);
            keyframes.push(Keyframe::new(new_t, new_state));
            keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
        }
    }
}

/// Interpolate the current property value at a given time (linear).
fn interpolate_at(keyframes: &[Keyframe<ActorState>], t: f32, prop: usize) -> f32 {
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
            let frac = (t - a.t) / span;
            let va = get_property(&a.value, prop);
            let vb = get_property(&b.value, prop);
            return va + (vb - va) * frac;
        }
    }
    get_property(&last.value, prop)
}

/// Draw background grid lines.
fn draw_grid(painter: &egui::Painter, rect: Rect, t_min: f32, t_max: f32, v_min: f32, v_max: f32) {
    let grid_color = Color32::from_rgb(30, 30, 45);
    let text_color = Color32::from_rgb(80, 80, 100);

    // Horizontal lines (value axis)
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

    // Vertical lines (time axis)
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

// ─── Coordinate mapping helpers ──────────────────────────────────────

fn time_to_graph_x(t: f32, t_min: f32, t_max: f32, rect: Rect) -> f32 {
    let frac = (t - t_min) / (t_max - t_min).max(1e-6);
    rect.min.x + frac * rect.width()
}

fn value_to_graph_y(v: f32, v_min: f32, v_max: f32, rect: Rect) -> f32 {
    let frac = (v - v_min) / (v_max - v_min).max(1e-6);
    // Y is inverted (higher values = higher on screen = lower Y)
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
