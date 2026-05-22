//! Clip Editor window — shows source video details with in/out markers.
//!
//! Opens on double-click of a clip (or via a button in inspector).
//! Shows the source video frame, in/out markers, and basic clip info.
//! Also provides pose detection integration and detected point overlay.

use egui::{Color32, RichText, Rounding, Stroke, Vec2};

use crate::state::EditorState;

/// Draw the clip editor window. Returns `true` if the window is still open.
pub fn clip_editor_window(ctx: &egui::Context, state: &mut EditorState) -> bool {
    let mut open = true;

    egui::Window::new("Clip Editor")
        .open(&mut open)
        .default_size([500.0, 400.0])
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            clip_editor_content(ui, state);
        });

    open
}

fn clip_editor_content(ui: &mut egui::Ui, state: &mut EditorState) {
    // Get info about the selected actor (if any)
    let actor_info = match state.selection {
        crate::state::Selection::Actor(i) if i < state.scene.actors.len() => {
            let a = &state.scene.actors[i];
            Some((
                i,
                a.source.clone(),
                a.t_in.unwrap_or(0.0),
                a.t_out.unwrap_or(state.scene.output.duration),
                a.source_start,
            ))
        }
        _ => None,
    };

    if let Some((actor_idx, source, t_in, t_out, source_start)) = actor_info {
        // ── Header ──
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Actor: {}", state.scene.actors[actor_idx].id))
                    .strong()
                    .size(14.0)
                    .color(Color32::from_rgb(220, 130, 50)),
            );
        });
        ui.separator();
        ui.add_space(4.0);

        // ── Source frame display area ──
        let frame_area_height = 200.0;
        let frame_area_width = ui.available_width();
        let (frame_rect, _) = ui.allocate_exact_size(
            Vec2::new(frame_area_width, frame_area_height),
            egui::Sense::hover(),
        );

        let painter = ui.painter_at(frame_rect);
        painter.rect_filled(frame_rect, Rounding::same(6.0), Color32::from_rgb(10, 10, 16));

        // Try to show a frame from the cache if available
        let local_t = state.playhead - t_in + source_start;
        let mut frame_shown = false;
        let mut img_rect_for_overlay = frame_rect;

        if let Some(fc) = state.frame_caches.get_mut(actor_idx) {
            if fc.is_ready() {
                if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                    let tex_size = tex.size_vec2();
                    let aspect = tex_size.x / tex_size.y;
                    let display_h = frame_area_height - 8.0;
                    let display_w = (display_h * aspect).min(frame_area_width - 8.0);
                    let display_h = if display_w < display_h * aspect {
                        display_w / aspect
                    } else {
                        display_h
                    };
                    let offset_x = (frame_area_width - display_w) * 0.5;
                    let offset_y = (frame_area_height - display_h) * 0.5;
                    let img_rect = egui::Rect::from_min_size(
                        egui::pos2(frame_rect.min.x + offset_x, frame_rect.min.y + offset_y),
                        Vec2::new(display_w, display_h),
                    );

                    let mut mesh = egui::Mesh::with_texture(tex.id());
                    mesh.add_rect_with_uv(
                        img_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    painter.add(egui::Shape::mesh(mesh));
                    frame_shown = true;
                    img_rect_for_overlay = img_rect;
                }
            }
        }

        if !frame_shown {
            painter.text(
                frame_rect.center(),
                egui::Align2::CENTER_CENTER,
                "No preview available\n(Extract frames first)",
                egui::FontId::proportional(12.0),
                Color32::from_rgb(100, 100, 120),
            );
        }

        // ── Draw detected pose points as overlay circles on the preview ──
        if !state.detected_points.is_empty() && frame_shown {
            for point in &state.detected_points {
                let px = img_rect_for_overlay.min.x + point[0] * img_rect_for_overlay.width();
                let py = img_rect_for_overlay.min.y + point[1] * img_rect_for_overlay.height();
                painter.circle_filled(
                    egui::pos2(px, py),
                    4.0,
                    Color32::from_rgb(255, 50, 200),
                );
                painter.circle_stroke(
                    egui::pos2(px, py),
                    4.0,
                    Stroke::new(1.5, Color32::WHITE),
                );
            }
        }

        ui.add_space(8.0);

        // ── Pose Detection section ──
        ui.horizontal(|ui| {
            let detect_btn = egui::Button::new(
                RichText::new("Detect Pose")
                    .size(12.0)
                    .color(Color32::WHITE),
            )
            .fill(Color32::from_rgb(140, 60, 200))
            .rounding(Rounding::same(6.0));
            if ui
                .add(detect_btn)
                .on_hover_text("Run pose detection on current frame")
                .clicked()
            {
                state.status = "__DETECT_POSE__".into();
            }

            if !state.detected_points.is_empty() {
                ui.label(
                    RichText::new(format!("{} points", state.detected_points.len()))
                        .size(11.0)
                        .color(Color32::from_rgb(200, 100, 255)),
                );
            }
        });

        ui.add_space(8.0);

        // ── In/Out point markers ──
        ui.label(
            RichText::new("In/Out Points")
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(180, 180, 200)),
        );
        ui.add_space(4.0);

        // Visual in/out bar
        let bar_height = 24.0;
        let bar_width = ui.available_width();
        let (bar_rect, _) =
            ui.allocate_exact_size(Vec2::new(bar_width, bar_height), egui::Sense::hover());
        let painter = ui.painter_at(bar_rect);

        // Background bar
        painter.rect_filled(bar_rect, Rounding::same(3.0), Color32::from_rgb(30, 30, 45));

        // Active region (in to out)
        let duration = state.scene.output.duration.max(0.01);
        let in_frac = (t_in / duration).clamp(0.0, 1.0);
        let out_frac = (t_out / duration).clamp(0.0, 1.0);
        let active_rect = egui::Rect::from_min_max(
            egui::pos2(
                bar_rect.min.x + in_frac * bar_rect.width(),
                bar_rect.min.y + 2.0,
            ),
            egui::pos2(
                bar_rect.min.x + out_frac * bar_rect.width(),
                bar_rect.max.y - 2.0,
            ),
        );
        painter.rect_filled(
            active_rect,
            Rounding::same(2.0),
            Color32::from_rgb(220, 130, 50),
        );

        // In marker
        let in_x = bar_rect.min.x + in_frac * bar_rect.width();
        painter.line_segment(
            [
                egui::pos2(in_x, bar_rect.min.y),
                egui::pos2(in_x, bar_rect.max.y),
            ],
            Stroke::new(2.0, Color32::from_rgb(80, 255, 80)),
        );
        // Out marker
        let out_x = bar_rect.min.x + out_frac * bar_rect.width();
        painter.line_segment(
            [
                egui::pos2(out_x, bar_rect.min.y),
                egui::pos2(out_x, bar_rect.max.y),
            ],
            Stroke::new(2.0, Color32::from_rgb(255, 80, 80)),
        );

        ui.add_space(8.0);

        // ── In/Out numeric editors ──
        // The Out value is hard-capped at the source clip's duration when
        // the frame cache has it — same rule as the timeline trim handle.
        let source_dur_known = state
            .frame_caches
            .get(actor_idx)
            .map(|fc| if fc.is_ready() { fc.duration } else { 0.0 })
            .unwrap_or(0.0);
        let out_upper = if source_dur_known > 0.0 {
            (t_in + (source_dur_known - source_start).max(0.1)).min(duration)
        } else {
            duration
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("In:").size(11.0));
            let mut in_val = t_in;
            if ui
                .add(
                    egui::DragValue::new(&mut in_val)
                        .range(0.0..=duration)
                        .speed(0.02)
                        .suffix("s"),
                )
                .changed()
            {
                if let crate::state::Selection::Actor(i) = state.selection {
                    if i < state.scene.actors.len() {
                        state.scene.actors[i].t_in = Some(in_val);
                    }
                }
            }

            ui.label(RichText::new("Out:").size(11.0));
            let mut out_val = t_out;
            if ui
                .add(
                    egui::DragValue::new(&mut out_val)
                        .range(0.0..=out_upper)
                        .speed(0.02)
                        .suffix("s"),
                )
                .changed()
            {
                if let crate::state::Selection::Actor(i) = state.selection {
                    if i < state.scene.actors.len() {
                        // Clamp again defensively — egui may report a value
                        // slightly past the range when the user types it.
                        state.scene.actors[i].t_out = Some(out_val.min(out_upper));
                    }
                }
            }

            ui.label(RichText::new("Duration:").size(11.0));
            ui.label(
                RichText::new(format!("{:.2}s", t_out - t_in))
                    .size(11.0)
                    .color(Color32::from_rgb(100, 80, 220)),
            );
        });

        ui.add_space(8.0);

        // ── Mark Region button (placeholder) ──
        ui.horizontal(|ui| {
            let mark_btn = egui::Button::new(
                RichText::new("Mark Region")
                    .size(12.0)
                    .color(Color32::WHITE),
            )
            .fill(Color32::from_rgb(80, 50, 150))
            .rounding(Rounding::same(6.0));
            if ui
                .add(mark_btn)
                .on_hover_text("Mark a region for future motion tracking (placeholder)")
                .clicked()
            {
                state.status =
                    "\u{1F6A7} Region marking: Coming in future iteration.".into();
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(4.0);

        // ── Clip info ──
        ui.label(
            RichText::new("Clip Info")
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(180, 180, 200)),
        );
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Source:").size(11.0).color(Color32::from_rgb(140, 140, 160)));
            let source_display = source
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(unknown)");
            ui.label(RichText::new(source_display).size(11.0));
        });

        ui.horizontal(|ui| {
            ui.label(RichText::new("Path:").size(11.0).color(Color32::from_rgb(140, 140, 160)));
            let path_str = source.to_string_lossy();
            let display_path = if path_str.len() > 50 {
                format!("...{}", &path_str[path_str.len() - 47..])
            } else {
                path_str.to_string()
            };
            ui.label(RichText::new(display_path).size(10.0));
        });

        ui.horizontal(|ui| {
            ui.label(RichText::new("Source offset:").size(11.0).color(Color32::from_rgb(140, 140, 160)));
            ui.label(RichText::new(format!("{:.2}s", source_start)).size(11.0));
        });

        // Format detection (based on extension)
        let format = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("unknown")
            .to_uppercase();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Format:").size(11.0).color(Color32::from_rgb(140, 140, 160)));
            ui.label(RichText::new(&format).size(11.0));
        });

        // Show frame cache status
        if let Some(fc) = state.frame_caches.get(actor_idx) {
            if fc.is_ready() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Frames:").size(11.0).color(Color32::from_rgb(140, 140, 160)));
                    ui.label(
                        RichText::new(format!("{} extracted", fc.frame_count))
                            .size(11.0)
                            .color(Color32::from_rgb(80, 200, 80)),
                    );
                });
            }
        }

        // ── Detected Points info ──
        if !state.detected_points.is_empty() {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(
                RichText::new("Detected Pose Points")
                    .size(12.0)
                    .strong()
                    .color(Color32::from_rgb(200, 100, 255)),
            );
            ui.add_space(2.0);
            egui::ScrollArea::vertical().max_height(80.0).show(ui, |ui| {
                for (i, point) in state.detected_points.iter().enumerate() {
                    ui.label(
                        RichText::new(format!(
                            "  Point {}: ({:.3}, {:.3})",
                            i, point[0], point[1]
                        ))
                        .size(10.0)
                        .color(Color32::from_rgb(180, 140, 220)),
                    );
                }
            });
        }
    } else {
        // No actor selected
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("No actor selected")
                    .size(14.0)
                    .italics()
                    .color(Color32::from_rgb(100, 100, 120)),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Select an actor clip on the timeline\nto view it in the clip editor.")
                    .size(11.0)
                    .color(Color32::from_rgb(80, 80, 100)),
            );
        });
    }
}
