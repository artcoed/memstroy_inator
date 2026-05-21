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
    /// Path of the source clip currently being edited. The skeleton template
    /// is keyed off this path (sidecar `<clip>.skeleton.json`), so it follows
    /// the asset across projects.
    pub clip_path: Option<std::path::PathBuf>,
    /// Index of the actor whose skeleton is being edited. Optional — when
    /// the user opens the editor for a library clip without a corresponding
    /// actor in the scene, this stays `None` and `clip_path` is the source
    /// of truth. Used as a hint to find an associated frame cache.
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
    let screen_rect = ctx.input(|i| i.screen_rect());
    // Cap the window so it never grows past the host viewport — without
    // this, the resizable Window kept inflating each frame because the
    // 9:16 preview demanded more height than the default 700×550 size,
    // creating a feedback loop.
    let max_w = (screen_rect.width() - 40.0).max(400.0).min(1100.0);
    let max_h = (screen_rect.height() - 60.0).max(360.0).min(900.0);

    egui::Window::new("Skeleton Constructor")
        .open(&mut open)
        .default_size([700.0, 550.0])
        .min_width(420.0)
        .min_height(320.0)
        .max_width(max_w)
        .max_height(max_h)
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            skeleton_editor_content(ui, state);
        });

    state.skeleton_editor.open = open;
    open
}

fn skeleton_editor_content(ui: &mut egui::Ui, state: &mut EditorState) {
    // ── Top toolbar: clip selector + template management ──
    skeleton_toolbar(ui, state);
    ui.separator();

    // If no clip/template selected, show help
    if state.skeleton_editor.template_idx.is_none() {
        ui.add_space(20.0);
        ui.label(
            RichText::new("Pick a clip from the library above and create or load a skeleton template.\n\nSkeletons are saved alongside the source clip as <name>.skeleton.json so they follow the asset into every future project automatically.")
                .italics()
                .color(Color32::from_rgb(140, 140, 160)),
        );
        return;
    }

    // ── Main content: frame preview + point list ──
    // ScrollArea is critical: it absorbs any "preview wants to be taller than
    // the window" pressure so the Window itself doesn't keep growing each
    // frame.
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // Reserve space for the right side panel and the separator,
                // then size the preview to fit the rest.
                let right_panel_w = 200.0_f32;
                let separator_w = 12.0_f32;
                let nav_height = 36.0_f32;
                let avail = ui.available_size_before_wrap();
                let preview_max_w = (avail.x - right_panel_w - separator_w).max(180.0);
                let preview_max_h = (avail.y - nav_height).max(160.0);

                // Vertical 9:16 aspect; fit into the available area without
                // demanding more height than we have.
                let aspect = 9.0_f32 / 16.0;
                let by_w_h = preview_max_w / aspect;
                let (pw, ph) = if by_w_h <= preview_max_h {
                    (preview_max_w, by_w_h)
                } else {
                    (preview_max_h * aspect, preview_max_h)
                };

                ui.vertical(|ui| {
                    frame_preview(ui, state, pw, ph);
                    ui.add_space(4.0);
                    frame_navigation(ui, state);
                });

                ui.separator();

                ui.vertical(|ui| {
                    ui.set_min_width(right_panel_w - 20.0);
                    point_list_panel(ui, state);
                });
            });
        });
}

// ─── TOOLBAR ─────────────────────────────────────────────────────────

fn skeleton_toolbar(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Clip:").size(12.0).strong());

        // Library-clip selector. The skeleton is bound to the clip on disk,
        // not to a specific actor instance, so a single skeleton template
        // can be reused across projects.
        let current_label = state.skeleton_editor.clip_path
            .as_ref()
            .and_then(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "(none)".into());

        let lib_paths: Vec<std::path::PathBuf> = state.library.mellstroy_clips.iter()
            .map(|c| c.path.clone())
            .collect();

        egui::ComboBox::from_id_source("skel_clip_select")
            .selected_text(&current_label)
            .show_ui(ui, |ui| {
                for path in &lib_paths {
                    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                    let chosen = state.skeleton_editor.clip_path.as_ref() == Some(path);
                    if ui.selectable_label(chosen, &name).clicked() {
                        on_clip_changed(state, path);
                    }
                }
            });

        ui.separator();

        // Template actions
        if state.skeleton_editor.clip_path.is_some() {
            if state.skeleton_editor.template_idx.is_none() {
                if ui.button(RichText::new("+ Create Skeleton").color(Color32::from_rgb(80, 200, 120)))
                    .clicked()
                {
                    create_template_for_current_clip(state);
                }
                if ui.button("Load from file").clicked() {
                    load_template_for_current_clip(state);
                }
            } else if ui.button("Save").on_hover_text("Save skeleton to <clip>.skeleton.json").clicked() {
                save_current_template(state);
            }
        }
    });
}

/// Public entry point used by `Tools → Skeleton Constructor` menu so the
/// editor opens with a sensible source clip pre-selected.
pub fn select_clip(state: &mut EditorState, clip_path: &std::path::Path) {
    on_clip_changed(state, clip_path);
}

/// Switch the editor to a new source clip. Auto-loads any existing sidecar
/// skeleton, otherwise leaves the user to create one.
fn on_clip_changed(state: &mut EditorState, clip_path: &std::path::Path) {
    state.skeleton_editor.clip_path = Some(clip_path.to_path_buf());
    state.skeleton_editor.selected_point = None;
    state.skeleton_editor.current_frame = 0;
    state.skeleton_editor.fps = 30.0;

    // Find a related actor (if the clip is in the current scene), so we
    // can use its frame cache for the live preview.
    state.skeleton_editor.actor_idx = state.scene.actors.iter()
        .position(|a| a.source == *clip_path);

    // Estimate total_frames from any existing actor; default to a clip-length
    // sidecar if available; otherwise fall back to 90 frames (3s @ 30fps).
    let mut total = 0u32;
    if let Some(ai) = state.skeleton_editor.actor_idx {
        let actor = &state.scene.actors[ai];
        let dur = actor.t_out.unwrap_or(state.scene.output.duration)
            - actor.t_in.unwrap_or(0.0);
        total = (dur * 30.0).ceil() as u32;
    }

    // Try to find the existing template in the scene; if not present,
    // attempt to load the sidecar so the skeleton "follows" the clip
    // across projects automatically.
    let mut tmpl_idx = state.scene.skeleton_templates.iter()
        .position(|t| t.source_clip == *clip_path);
    if tmpl_idx.is_none() {
        if let Some(template) = SkeletonTemplate::load_for_clip(clip_path) {
            if total == 0 && template.clip_duration > 0.0 {
                total = (template.clip_duration * template.fps).ceil() as u32;
            }
            state.scene.skeleton_templates.push(template);
            tmpl_idx = Some(state.scene.skeleton_templates.len() - 1);
        }
    } else if let Some(idx) = tmpl_idx {
        if total == 0 {
            let t = &state.scene.skeleton_templates[idx];
            total = (t.clip_duration * t.fps).ceil() as u32;
        }
    }
    state.skeleton_editor.template_idx = tmpl_idx;
    state.skeleton_editor.total_frames = total.max(1);
}

fn create_template_for_current_clip(state: &mut EditorState) {
    let Some(ref clip_path) = state.skeleton_editor.clip_path.clone() else { return };
    // Estimate duration from any actor that uses this clip.
    let clip_duration = state.scene.actors.iter()
        .find(|a| a.source == *clip_path)
        .map(|a| a.t_out.unwrap_or(state.scene.output.duration) - a.t_in.unwrap_or(0.0))
        .unwrap_or(3.0);

    let name = clip_path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("{}_skeleton", s))
        .unwrap_or_else(|| "skeleton".into());

    let template = SkeletonTemplate {
        name,
        source_clip: clip_path.clone(),
        fps: 30.0,
        clip_duration,
        points: Default::default(),
    };

    state.scene.skeleton_templates.push(template);
    state.skeleton_editor.template_idx = Some(state.scene.skeleton_templates.len() - 1);
    state.skeleton_editor.total_frames = (clip_duration * 30.0).ceil() as u32;
    // Persist immediately so subsequent project loads see the new template.
    save_current_template(state);
    state.status = "Skeleton template created.".into();
}

fn load_template_for_current_clip(state: &mut EditorState) {
    let Some(ref clip_path) = state.skeleton_editor.clip_path.clone() else { return };
    if let Some(template) = SkeletonTemplate::load_for_clip(clip_path) {
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

fn frame_preview(ui: &mut egui::Ui, state: &mut EditorState, width: f32, height: f32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let painter = ui.painter_at(rect);

    // Background
    painter.rect_filled(rect, Rounding::same(4.0), COL_FRAME_BG);

    // Try to show the actual frame from the actor's cache (if there's a
    // matching actor instance in the current scene).
    let t = state.skeleton_editor.current_time();
    let mut frame_shown = false;

    if let Some(actor_idx) = state.skeleton_editor.actor_idx {
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
    // Persist to <clip>.skeleton.json so the work follows the clip across
    // future projects without an explicit save click.
    let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
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
