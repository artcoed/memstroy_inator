//! Skeleton Editor window — frame-by-frame point placement UI.
//!
//! Opens as a floating egui::Window. The user selects a clip (actor),
//! navigates frame-by-frame, and places/moves named anchor points.
//! Points are stored as keyframes in a SkeletonTemplate.

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_POINT_DEFAULT: Color32 = Color32::from_rgb(255, 100, 100);
const COL_POINT_SELECTED: Color32 = Color32::from_rgb(255, 220, 80);
const COL_POINT_INACTIVE: Color32 = Color32::from_rgb(100, 100, 140);
const COL_FRAME_BG: Color32 = Color32::from_rgb(20, 20, 30);
const COL_TOOLBAR_BG: Color32 = Color32::from_rgb(28, 28, 40);

/// Persistent state for the skeleton editor (lives in EditorState).
#[derive(Default)]
pub struct SkeletonEditorState {
    /// Whether the skeleton editor window is open.
    pub open: bool,
    /// Index of the actor whose skeleton is being edited.
    pub actor_idx: Option<usize>,
    /// Index of the skeleton template being edited (in scene.skeleton_templates).
    pub template_idx: Option<usize>,
    /// Currently selected point name.
    pub selected_point: Option<String>,
    /// Current frame index for navigation.
    pub current_frame: u32,
    /// Total frame count (from frame cache or clip duration * fps).
    pub total_frames: u32,
    /// FPS used for frame navigation.
    pub fps: f32,
    /// New point name input buffer.
    pub new_point_name: String,
    /// Whether we're in "place mode" (next click places/moves the selected point).
    pub place_mode: bool,
}

impl SkeletonEditorState {
    pub fn current_time(&self) -> f32 {
        self.current_frame as f32 / self.fps.max(1.0)
    }
}

// ─── MAIN WINDOW ─────────────────────────────────────────────────────

/// Render the skeleton editor window. Returns whether it remains open.
pub fn skeleton_editor_window(ctx: &egui::Context, state: &mut EditorState) -> bool {
    if !state.skeleton_editor.open {
        return false;
    }

    let mut open = state.skeleton_editor.open;

    egui::Window::new("Skeleton Constructor")
        .open(&mut open)
        .default_size([700.0, 550.0])
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            skeleton_editor_content(ui, state);
        });

    state.skeleton_editor.open = open;
    open
}

fn skeleton_editor_content(ui: &mut egui::Ui, state: &mut EditorState) {
    // ── Top toolbar: actor selection + template management ──
    skeleton_toolbar(ui, state);
    ui.separator();

    // If no actor/template selected, show help
    if state.skeleton_editor.template_idx.is_none() {
        ui.add_space(20.0);
        ui.label(
            RichText::new("Select an actor above and create or load a skeleton template.")
                .italics()
                .color(Color32::from_rgb(140, 140, 160)),
        );
        return;
    }

    // ── Main content: frame preview + point list ──
    ui.horizontal(|ui| {
        // Left: frame preview with point overlay
        let preview_width = (ui.available_width() - 200.0).max(300.0);
        ui.vertical(|ui| {
            frame_preview(ui, state, preview_width);
            ui.add_space(4.0);
            frame_navigation(ui, state);
        });

        ui.separator();

        // Right: point list + properties
        ui.vertical(|ui| {
            ui.set_min_width(180.0);
            point_list_panel(ui, state);
        });
    });
}

// ─── TOOLBAR ─────────────────────────────────────────────────────────

fn skeleton_toolbar(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Actor:").size(12.0).strong());

        // Actor selector combo
        let actor_names: Vec<String> = state.scene.actors.iter()
            .map(|a| a.id.clone())
            .collect();
        let current_label = state.skeleton_editor.actor_idx
            .and_then(|i| actor_names.get(i).cloned())
            .unwrap_or_else(|| "(none)".into());

        egui::ComboBox::from_id_source("skel_actor_select")
            .selected_text(&current_label)
            .show_ui(ui, |ui| {
                for (i, name) in actor_names.iter().enumerate() {
                    if ui.selectable_value(
                        &mut state.skeleton_editor.actor_idx,
                        Some(i),
                        name,
                    ).clicked() {
                        // When actor changes, try to find/create matching template
                        on_actor_changed(state, i);
                    }
                }
            });

        ui.separator();

        // Template actions
        if state.skeleton_editor.actor_idx.is_some() {
            if state.skeleton_editor.template_idx.is_none() {
                if ui.button(RichText::new("+ Create Skeleton").color(Color32::from_rgb(80, 200, 120)))
                    .clicked()
                {
                    create_template_for_current_actor(state);
                }
                if ui.button("Load from file").clicked() {
                    load_template_for_current_actor(state);
                }
            } else {
                if ui.button("Save").on_hover_text("Save skeleton to .skeleton.json").clicked() {
                    save_current_template(state);
                }
            }
        }
    });
}

fn on_actor_changed(state: &mut EditorState, actor_idx: usize) {
    state.skeleton_editor.actor_idx = Some(actor_idx);
    state.skeleton_editor.selected_point = None;
    state.skeleton_editor.current_frame = 0;

    let actor = &state.scene.actors[actor_idx];

    // Calculate total frames
    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(state.scene.output.duration);
    let duration = t_out - t_in;
    state.skeleton_editor.fps = 30.0;
    state.skeleton_editor.total_frames = (duration * 30.0).ceil() as u32;

    // Try to find existing template for this actor's source
    let source = actor.source.clone();
    state.skeleton_editor.template_idx = state.scene.skeleton_templates.iter()
        .position(|t| t.source_clip == source);
}

fn create_template_for_current_actor(state: &mut EditorState) {
    let Some(actor_idx) = state.skeleton_editor.actor_idx else { return };
    let actor = &state.scene.actors[actor_idx];

    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(state.scene.output.duration);

    let template = SkeletonTemplate {
        name: format!("{}_skeleton", actor.id),
        source_clip: actor.source.clone(),
        fps: 30.0,
        clip_duration: t_out - t_in,
        points: Default::default(),
    };

    state.scene.skeleton_templates.push(template);
    state.skeleton_editor.template_idx = Some(state.scene.skeleton_templates.len() - 1);
    state.status = "Skeleton template created.".into();
}

fn load_template_for_current_actor(state: &mut EditorState) {
    let Some(actor_idx) = state.skeleton_editor.actor_idx else { return };
    let actor = &state.scene.actors[actor_idx];

    if let Some(template) = SkeletonTemplate::load_for_clip(&actor.source) {
        state.scene.skeleton_templates.push(template);
        state.skeleton_editor.template_idx = Some(state.scene.skeleton_templates.len() - 1);
        state.status = "Skeleton loaded from file.".into();
    } else {
        state.status = "No .skeleton.json file found for this clip.".into();
    }
}

fn save_current_template(state: &mut EditorState) {
    let Some(idx) = state.skeleton_editor.template_idx else { return };
    let template = &state.scene.skeleton_templates[idx];
    match template.save_alongside_clip() {
        Ok(path) => state.status = format!("Skeleton saved: {}", path.display()),
        Err(e) => state.status = format!("Save failed: {}", e),
    }
}

// ─── FRAME PREVIEW ───────────────────────────────────────────────────

fn frame_preview(ui: &mut egui::Ui, state: &mut EditorState, width: f32) {
    let aspect = 9.0 / 16.0; // vertical video
    let height = width / aspect;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let painter = ui.painter_at(rect);

    // Background
    painter.rect_filled(rect, Rounding::same(4.0), COL_FRAME_BG);

    // Try to show the actual frame from the actor's cache
    let actor_idx = state.skeleton_editor.actor_idx.unwrap_or(0);
    let t = state.skeleton_editor.current_time();
    let mut frame_shown = false;

    if let Some(fc) = state.frame_caches.get_mut(actor_idx) {
        if fc.is_ready() {
            let actor = &state.scene.actors[actor_idx];
            let local_t = t + actor.source_start;
            if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                painter.image(tex.id(), rect, uv, Color32::WHITE);
                frame_shown = true;
            }
        }
    }

    if !frame_shown {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("Frame {}", state.skeleton_editor.current_frame),
            egui::FontId::proportional(14.0),
            Color32::from_rgb(80, 80, 100),
        );
    }

    // Draw skeleton points on the frame
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        let template = &state.scene.skeleton_templates[tmpl_idx];

        for (name, point) in &template.points {
            // Sample position at current time
            let point_state = crate::skeleton_editor::sample_point_at(point, t);

            let screen_x = rect.min.x + point_state.x * rect.width();
            let screen_y = rect.min.y + point_state.y * rect.height();
            let pos = Pos2::new(screen_x, screen_y);

            let is_selected = state.skeleton_editor.selected_point.as_deref() == Some(name);
            let color = if is_selected {
                COL_POINT_SELECTED
            } else {
                Color32::from_rgb(point.color[0], point.color[1], point.color[2])
            };

            // Draw point marker
            let radius = if is_selected { 8.0 } else { 6.0 };
            painter.circle_filled(pos, radius, color);
            painter.circle_stroke(pos, radius, Stroke::new(1.5, Color32::WHITE));

            // Label
            painter.text(
                Pos2::new(pos.x + 10.0, pos.y - 6.0),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(10.0),
                color,
            );

            // Has keyframe at this time? Show diamond
            let has_kf = point.track.iter().any(|kf| (kf.t - t).abs() < 0.02);
            if has_kf {
                painter.text(
                    Pos2::new(pos.x, pos.y - radius - 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    "\u{25C6}",
                    egui::FontId::proportional(8.0),
                    Color32::from_rgb(255, 200, 50),
                );
            }
        }
    }

    // Handle click to place/move selected point
    if response.clicked() && state.skeleton_editor.place_mode {
        if let Some(click_pos) = response.interact_pointer_pos() {
            let nx = ((click_pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let ny = ((click_pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
            place_point_at(state, nx, ny);
        }
    }

    // Click on a point to select it (when not in place mode)
    if response.clicked() && !state.skeleton_editor.place_mode {
        if let (Some(click_pos), Some(tmpl_idx)) = (response.interact_pointer_pos(), state.skeleton_editor.template_idx) {
            let template = &state.scene.skeleton_templates[tmpl_idx];
            let mut best: Option<(&str, f32)> = None;

            for (name, point) in &template.points {
                let ps = sample_point_at(point, t);
                let sx = rect.min.x + ps.x * rect.width();
                let sy = rect.min.y + ps.y * rect.height();
                let dist = ((click_pos.x - sx).powi(2) + (click_pos.y - sy).powi(2)).sqrt();
                if dist < 15.0 {
                    if best.is_none() || dist < best.unwrap().1 {
                        best = Some((name, dist));
                    }
                }
            }

            if let Some((name, _)) = best {
                state.skeleton_editor.selected_point = Some(name.to_string());
            } else {
                state.skeleton_editor.selected_point = None;
            }
        }
    }
}

/// Helper: sample a SkeletonPoint at time t (without needing &SkeletonTemplate).
pub fn sample_point_at(point: &SkeletonPoint, t: f32) -> PointState {
    keyframe::sample(&point.track, t).unwrap_or_default()
}

fn place_point_at(state: &mut EditorState, nx: f32, ny: f32) {
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else { return };
    let Some(ref point_name) = state.skeleton_editor.selected_point.clone() else { return };
    let t = state.skeleton_editor.current_time();

    let ps = PointState { x: nx, y: ny, scale: 1.0, rotation_deg: 0.0 };
    state.scene.skeleton_templates[tmpl_idx].set_point_keyframe(
        point_name, t, ps, Easing::Linear,
    );
    state.status = format!("Point '{}' set at ({:.2}, {:.2}) t={:.2}s", point_name, nx, ny, t);
}

// ─── FRAME NAVIGATION ────────────────────────────────────────────────

fn frame_navigation(ui: &mut egui::Ui, state: &mut EditorState) {
    let total = state.skeleton_editor.total_frames.max(1);

    ui.horizontal(|ui| {
        // Previous frame
        if ui.button("\u{23EE}").on_hover_text("First frame").clicked() {
            state.skeleton_editor.current_frame = 0;
        }
        if ui.button("\u{25C0}").on_hover_text("Previous frame").clicked() {
            if state.skeleton_editor.current_frame > 0 {
                state.skeleton_editor.current_frame -= 1;
            }
        }

        // Frame slider
        let mut frame = state.skeleton_editor.current_frame;
        ui.add(
            egui::Slider::new(&mut frame, 0..=total.saturating_sub(1))
                .show_value(false)
                .clamp_to_range(true),
        );
        state.skeleton_editor.current_frame = frame;

        // Next frame
        if ui.button("\u{25B6}").on_hover_text("Next frame").clicked() {
            if state.skeleton_editor.current_frame < total - 1 {
                state.skeleton_editor.current_frame += 1;
            }
        }
        if ui.button("\u{23ED}").on_hover_text("Last frame").clicked() {
            state.skeleton_editor.current_frame = total - 1;
        }

        // Frame number / time display
        let t = state.skeleton_editor.current_time();
        ui.label(
            RichText::new(format!("{}/{} ({:.2}s)", frame + 1, total, t))
                .size(11.0)
                .color(Color32::from_rgb(160, 160, 180)),
        );
    });
}

// ─── POINT LIST PANEL ────────────────────────────────────────────────

fn point_list_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.label(RichText::new("Points").size(14.0).strong());
    ui.add_space(4.0);

    // Add new point
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.skeleton_editor.new_point_name)
                .hint_text("Point name...")
                .desired_width(100.0),
        );
        if ui.button("+").on_hover_text("Add point").clicked() {
            let name = state.skeleton_editor.new_point_name.trim().to_string();
            if !name.is_empty() {
                if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
                    state.scene.skeleton_templates[tmpl_idx].add_point(&name);
                    state.skeleton_editor.selected_point = Some(name.clone());
                    state.skeleton_editor.new_point_name.clear();
                    state.status = format!("Added point: {}", name);
                }
            }
        }
    });

    ui.add_space(8.0);

    // Point list
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else { return };
    let t = state.skeleton_editor.current_time();

    let point_names: Vec<String> = state.scene.skeleton_templates[tmpl_idx]
        .points.keys().cloned().collect();

    let mut to_remove: Option<String> = None;

    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
        for name in &point_names {
            let is_selected = state.skeleton_editor.selected_point.as_deref() == Some(name);
            let point = &state.scene.skeleton_templates[tmpl_idx].points[name];
            let color = Color32::from_rgb(point.color[0], point.color[1], point.color[2]);
            let has_kf = point.track.iter().any(|kf| (kf.t - t).abs() < 0.02);
            let num_kf = point.track.len();

            let frame = egui::Frame::none()
                .fill(if is_selected { Color32::from_rgb(40, 40, 60) } else { Color32::TRANSPARENT })
                .rounding(Rounding::same(4.0))
                .inner_margin(egui::Margin::same(4.0));

            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Color dot
                    let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 5.0, color);

                    // Name (click to select)
                    let resp = ui.selectable_label(is_selected, RichText::new(name).size(12.0));
                    if resp.clicked() {
                        state.skeleton_editor.selected_point = Some(name.clone());
                    }

                    // Keyframe indicator
                    if has_kf {
                        ui.label(RichText::new("\u{25C6}").size(10.0).color(Color32::from_rgb(255, 200, 50)));
                    }

                    // Keyframe count
                    ui.label(
                        RichText::new(format!("{}kf", num_kf))
                            .size(9.0)
                            .color(Color32::from_rgb(100, 100, 120)),
                    );

                    // Delete button
                    if ui.small_button("\u{1F5D1}").on_hover_text("Remove point").clicked() {
                        to_remove = Some(name.clone());
                    }
                });
            });
        }
    });

    // Process removal
    if let Some(name) = to_remove {
        state.scene.skeleton_templates[tmpl_idx].remove_point(&name);
        if state.skeleton_editor.selected_point.as_deref() == Some(&name) {
            state.skeleton_editor.selected_point = None;
        }
        state.status = format!("Removed point: {}", name);
    }

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // ── Selected point actions ──
    if let Some(ref sel_name) = state.skeleton_editor.selected_point.clone() {
        ui.label(RichText::new(format!("Selected: {}", sel_name)).size(12.0).strong());
        ui.add_space(4.0);

        // Place mode toggle
        let place_color = if state.skeleton_editor.place_mode {
            Color32::from_rgb(255, 80, 80)
        } else {
            Color32::from_rgb(80, 200, 120)
        };
        let place_text = if state.skeleton_editor.place_mode { "Placing... (click frame)" } else { "Place Point" };
        if ui.button(RichText::new(place_text).color(place_color)).clicked() {
            state.skeleton_editor.place_mode = !state.skeleton_editor.place_mode;
        }

        ui.add_space(4.0);

        // Add keyframe at current time
        if ui.button("+ Keyframe here").on_hover_text("Add/update keyframe at current frame").clicked() {
            // Use current interpolated position or default center
            let current_state = state.scene.skeleton_templates[tmpl_idx]
                .sample_point(sel_name, t)
                .unwrap_or_default();
            state.scene.skeleton_templates[tmpl_idx].set_point_keyframe(
                sel_name, t, current_state, Easing::Linear,
            );
            state.status = format!("Keyframe added at {:.2}s", t);
        }

        // Remove keyframe at current time
        if ui.button("- Remove keyframe").on_hover_text("Remove keyframe nearest to current frame").clicked() {
            if state.scene.skeleton_templates[tmpl_idx].remove_point_keyframe(sel_name, t) {
                state.status = format!("Keyframe removed at {:.2}s", t);
            }
        }

        // Show current interpolated state
        if let Some(ps) = state.scene.skeleton_templates[tmpl_idx].sample_point(sel_name, t) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Pos: ({:.3}, {:.3})", ps.x, ps.y))
                    .size(10.0)
                    .color(Color32::from_rgb(140, 140, 160)),
            );
        }
    }
}
