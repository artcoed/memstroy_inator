//! UI panels — modern dark theme with emoji and accent colors.

use std::path::PathBuf;

use egui::{Color32, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

// ─── LIBRARY ─────────────────────────────────────────────────────────

pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Library").size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(
                RichText::new("Refresh")
                    .color(Color32::WHITE)
                    .size(12.0),
            )
            .fill(Color32::from_rgb(80, 50, 180))
            .rounding(Rounding::same(6.0));

            if ui.add_enabled(!state.refreshing, btn).clicked() {
                state.status = "__REFRESH_REQUESTED__".into();
            }
        });
    });

    ui.add_space(4.0);

    // Search input
    ui.add(
        egui::TextEdit::singleline(&mut state.library_search)
            .hint_text("Search clips...")
            .desired_width(ui.available_width()),
    );

    ui.add_space(4.0);

    // Clips list
    let clip_count = state.library.mellstroy_clips.len();
    let search_lower = state.library_search.to_lowercase();

    ui.label(
        RichText::new(format!("Clips ({})", clip_count))
            .size(12.0)
            .color(Color32::from_rgb(150, 150, 170)),
    );

    if state.library.mellstroy_clips.is_empty() {
        ui.add_space(8.0);
        ui.label(
            RichText::new("No clips. Hit Refresh to download.")
                .italics()
                .color(Color32::from_rgb(140, 140, 160))
                .size(12.0),
        );
    } else {
        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 60.0)
            .show(ui, |ui| {
                for idx in 0..state.library.mellstroy_clips.len() {
                    let clip = &state.library.mellstroy_clips[idx];
                    // Filter by search
                    if !search_lower.is_empty() {
                        let clean = clean_clip_text(&clip.description).to_lowercase();
                        let id_str = clip.id.to_string();
                        if !clean.contains(&search_lower) && !id_str.contains(&search_lower) {
                            continue;
                        }
                    }
                    let clip = state.library.mellstroy_clips[idx].clone();
                    clip_card(ui, state, &clip);
                }
            });
    }

    ui.add_space(6.0);

    // Backgrounds (compact)
    if !state.library.backgrounds.is_empty() {
        egui::CollapsingHeader::new(
            RichText::new(format!("Backgrounds ({})", state.library.backgrounds.len()))
                .size(12.0)
                .color(Color32::from_rgb(100, 180, 255)),
        )
        .show(ui, |ui| {
            for p in state.library.backgrounds.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                if ui
                    .button(RichText::new(&name).size(11.0))
                    .clicked()
                {
                    add_background_from_path(state, &p);
                }
            }
        });
    }
}

fn clip_card(ui: &mut egui::Ui, state: &mut EditorState, clip: &crate::state::LibraryClip) {
    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 32, 48))
        .rounding(Rounding::same(4.0))
        .inner_margin(egui::Margin::same(3.0))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(50, 50, 70)));

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            // Thumbnail (fixed 48x48 square, cover-fill)
            let thumb_size = Vec2::new(48.0, 48.0);
            if let Some(thumb) = &clip.thumbnail {
                let uri = format!("file://{}", thumb.display());
                ui.add(
                    egui::Image::from_uri(uri)
                        .fit_to_exact_size(thumb_size)
                        .maintain_aspect_ratio(false)
                        .rounding(Rounding::same(3.0)),
                );
            } else {
                let (rect, _) = ui.allocate_exact_size(thumb_size, egui::Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 55));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}", clip.id),
                    egui::FontId::proportional(11.0),
                    Color32::from_rgb(100, 100, 120),
                );
            }

            ui.vertical(|ui| {
                let desc = clean_clip_text(&clip.description);
                let display = if desc.is_empty() {
                    format!("Clip #{}", clip.id)
                } else if desc.chars().count() > 35 {
                    let truncated: String = desc.chars().take(32).collect();
                    format!("{}...", truncated)
                } else {
                    desc
                };
                ui.label(
                    RichText::new(format!("#{}", clip.id))
                        .size(9.0)
                        .color(Color32::from_rgb(120, 100, 200)),
                );
                ui.label(
                    RichText::new(display)
                        .size(11.0)
                        .color(Color32::from_rgb(200, 200, 220)),
                );
            });

            // Add button
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add_btn = egui::Button::new(
                    RichText::new("+").size(13.0).color(Color32::WHITE),
                )
                .fill(Color32::from_rgb(60, 160, 80))
                .rounding(Rounding::same(8.0))
                .min_size(Vec2::new(22.0, 22.0));

                if ui.add(add_btn).on_hover_text("Add to scene").clicked() {
                    add_actor_from_clip(state, &clip.path);
                }
            });
        });
    });
    ui.add_space(2.0);
}

// ─── INSPECTOR ───────────────────────────────────────────────────────

pub fn inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading(RichText::new("\u{1F50D} Inspector").size(18.0).strong());
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);

    match state.selection {
        Selection::None => {
            ui.label(
                RichText::new(
                    "\u{1F447} Select an actor, overlay, or background\nfrom the timeline below.",
                )
                .italics()
                .color(Color32::from_rgb(140, 140, 160)),
            );
            ui.add_space(12.0);
            output_spec_editor(ui, &mut state.scene.output);
        }
        Selection::Actor(i) => {
            let actor_count = state.scene.actors.len();
            let cache_count = state.frame_caches.len();
            if i < actor_count {
                // Eyedropper button
                ui.horizontal(|ui| {
                    if state.eyedropper_active {
                        ui.label(RichText::new("\u{1F50D} Click preview to pick chroma-key color...").color(Color32::from_rgb(255, 200, 50)));
                    } else {
                        if ui.button("\u{1F50D} Eyedropper").on_hover_text("Pick chroma-key color from preview").clicked() {
                            state.eyedropper_active = true;
                        }
                    }
                });
                ui.add_space(4.0);
                // Layer reorder controls
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Layer order:").size(11.0));
                    if i > 0 {
                        if ui.small_button("\u{2B06}").on_hover_text("Move layer up (renders later = on top)").clicked() {
                            state.scene.actors.swap(i, i - 1);
                            if cache_count > i {
                                state.frame_caches.swap(i, i - 1);
                            }
                            state.selection = Selection::Actor(i - 1);
                        }
                    }
                    if i + 1 < actor_count {
                        if ui.small_button("\u{2B07}").on_hover_text("Move layer down (renders earlier = behind)").clicked() {
                            state.scene.actors.swap(i, i + 1);
                            if cache_count > i + 1 {
                                state.frame_caches.swap(i, i + 1);
                            }
                            state.selection = Selection::Actor(i + 1);
                        }
                    }
                });
                ui.add_space(4.0);
                if let Some(a) = state.scene.actors.get_mut(i) {
                    actor_editor(ui, a);
                }
            }
        }
        Selection::Overlay(i) => {
            if let Some(o) = state.scene.overlays.get_mut(i) {
                overlay_editor(ui, o);
            }
        }
        Selection::Background(i) => {
            if let Some(b) = state.scene.backgrounds.get_mut(i) {
                background_editor(ui, b);
            }
        }
        Selection::Camera(_) => {
            ui.label("\u{1F4F7} Camera keyframe editing \u{2014} coming soon.");
        }
    }
}

fn output_spec_editor(ui: &mut egui::Ui, spec: &mut OutputSpec) {
    egui::CollapsingHeader::new(
        RichText::new("\u{2699} Output Settings")
            .size(14.0)
            .color(Color32::from_rgb(100, 200, 255)),
    )
    .default_open(true)
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Resolution:");
            ui.add(egui::DragValue::new(&mut spec.resolution[0]).range(64..=4096));
            ui.label("\u{00D7}");
            ui.add(egui::DragValue::new(&mut spec.resolution[1]).range(64..=4096));
        });
        ui.horizontal(|ui| {
            ui.label("FPS:");
            ui.add(egui::DragValue::new(&mut spec.fps).range(24..=120));
        });
        ui.horizontal(|ui| {
            ui.label("Duration (s):");
            ui.add(egui::DragValue::new(&mut spec.duration).range(0.5..=600.0).speed(0.1));
        });
    });
}

fn actor_editor(ui: &mut egui::Ui, a: &mut Actor) {
    ui.label(
        RichText::new(format!("\u{1F3AD} Actor: {}", a.id))
            .strong()
            .size(15.0)
            .color(Color32::from_rgb(255, 150, 50)),
    );
    ui.label(
        RichText::new(format!("Source: {}", a.source.display()))
            .size(11.0)
            .color(Color32::from_rgb(150, 150, 170)),
    );
    ui.add_space(6.0);

    ui.checkbox(&mut a.visible, "\u{1F441} Visible");
    ui.checkbox(&mut a.flip_horizontal, "\u{1F500} Flip horizontally");
    ui.checkbox(&mut a.loop_source, "\u{1F501} Loop source");
    ui.add(egui::DragValue::new(&mut a.source_start).speed(0.05).prefix("Start: "));

    ui.add_space(8.0);
    egui::CollapsingHeader::new(
        RichText::new("\u{1F7E2} Chroma Key").color(Color32::from_rgb(100, 255, 100)),
    )
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Key color:");
            color_edit_u8(ui, &mut a.chroma_key.key_color);
        });
        ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text("Similarity"));
        ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text("Blend"));
        ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text("Spill"));
    });

    ui.add_space(8.0);
    egui::CollapsingHeader::new(
        RichText::new("\u{1F4CD} Keyframes").color(Color32::from_rgb(255, 200, 100)),
    )
    .default_open(true)
    .show(ui, |ui| {
        if a.layout.is_empty() {
            if ui.button("\u{2795} Add starting keyframe").clicked() {
                a.layout.push(Keyframe::new(0.0, ActorState::default()));
            }
        }

        // Show keyframes as a mini timeline bar
        if !a.layout.is_empty() {
            let avail_w = ui.available_width();
            let duration = a.t_out.unwrap_or(8.0) - a.t_in.unwrap_or(0.0);
            let bar_height = 20.0;
            let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, bar_height), Sense::hover());
            let painter = ui.painter_at(bar_rect);
            painter.rect_filled(bar_rect, Rounding::same(3.0), Color32::from_rgb(30, 30, 45));

            // Draw keyframe diamonds on the bar
            for kf in &a.layout {
                let frac = (kf.t / duration.max(0.01)).clamp(0.0, 1.0);
                let x = bar_rect.min.x + frac * bar_rect.width();
                let y = bar_rect.center().y;
                // Diamond shape
                let size = 5.0;
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(x, y - size),
                        egui::pos2(x + size, y),
                        egui::pos2(x, y + size),
                        egui::pos2(x - size, y),
                    ],
                    Color32::from_rgb(255, 200, 50),
                    Stroke::new(1.0, Color32::from_rgb(200, 150, 30)),
                ));
            }
        }

        // Editable keyframe list (improved layout)
        let mut to_remove = None;
        for (i, kf) in a.layout.iter_mut().enumerate() {
            let frame = egui::Frame::none()
                .fill(Color32::from_rgb(28, 28, 40))
                .rounding(Rounding::same(4.0))
                .inner_margin(egui::Margin::same(4.0));
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("KF {}", i + 1)).size(10.0).color(Color32::from_rgb(255, 200, 50)));
                    ui.add(egui::DragValue::new(&mut kf.t).range(0.0..=600.0).speed(0.05).prefix("t: ").suffix("s"));
                    if ui.small_button("\u{2716}").clicked() {
                        to_remove = Some(i);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Pos:");
                    ui.add(egui::DragValue::new(&mut kf.value.pos[0]).range(-2.0..=3.0).speed(0.01).prefix("X "));
                    ui.add(egui::DragValue::new(&mut kf.value.pos[1]).range(-2.0..=3.0).speed(0.01).prefix("Y "));
                });
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut kf.value.scale).range(0.05..=8.0).speed(0.01).prefix("Scale: "));
                    ui.add(egui::DragValue::new(&mut kf.value.rotation_deg).range(-360.0..=360.0).speed(0.5).prefix("Rot: ").suffix("\u{00B0}"));
                    ui.add(egui::DragValue::new(&mut kf.value.opacity).range(0.0..=1.0).speed(0.01).prefix("\u{03B1}: "));
                });
            });
            ui.add_space(2.0);
        }
        if let Some(i) = to_remove {
            a.layout.remove(i);
        }
        if ui.button("\u{2795} Add keyframe").clicked() {
            let last = a.layout.last().cloned().unwrap_or_else(|| Keyframe::new(0.0, ActorState::default()));
            a.layout.push(Keyframe::new(last.t + 1.0, last.value));
        }
    });
}

fn overlay_editor(ui: &mut egui::Ui, o: &mut Overlay) {
    match o {
        Overlay::Text(t) => {
            ui.label(
                RichText::new(format!("\u{1F4DD} Text: {}", t.id))
                    .strong()
                    .size(15.0)
                    .color(Color32::from_rgb(100, 200, 255)),
            );
            ui.text_edit_multiline(&mut t.text);
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut t.t_in).speed(0.05).prefix("in="));
                ui.add(egui::DragValue::new(&mut t.t_out).speed(0.05).prefix("out="));
            });
            ui.add(egui::Slider::new(&mut t.style.font_size, 16.0..=512.0).text("Font size"));
            ui.horizontal(|ui| {
                ui.label("Color:");
                color_edit_u8(ui, &mut t.style.color);
            });
            let mut has_box = t.style.box_color.is_some();
            ui.checkbox(&mut has_box, "\u{2B1C} White plate");
            if has_box && t.style.box_color.is_none() {
                t.style.box_color = Some([255, 255, 255]);
            }
            if !has_box {
                t.style.box_color = None;
            }
        }
        Overlay::Image(i) => {
            ui.label(RichText::new(format!("\u{1F5BC} Image: {}", i.id)).strong().size(15.0));
            ui.label(format!("Source: {}", i.source.display()));
        }
        Overlay::Video(v) => {
            ui.label(RichText::new(format!("\u{1F3AC} Video: {}", v.id)).strong().size(15.0));
            ui.label(format!("Source: {}", v.source.display()));
        }
    }
}

fn background_editor(ui: &mut egui::Ui, b: &mut Background) {
    ui.label(
        RichText::new(format!("\u{1F304} Background: {}", b.id))
            .strong()
            .size(15.0)
            .color(Color32::from_rgb(100, 180, 255)),
    );
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut b.start).speed(0.05).prefix("start="));
        ui.add(egui::DragValue::new(&mut b.duration).speed(0.05).prefix("dur="));
    });
    egui::ComboBox::from_label("Fit")
        .selected_text(format!("{:?}", b.fit))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.fit, Fit::Cover, "Cover");
            ui.selectable_value(&mut b.fit, Fit::Contain, "Contain");
            ui.selectable_value(&mut b.fit, Fit::Stretch, "Stretch");
            ui.selectable_value(&mut b.fit, Fit::Original, "Original");
        });
    egui::ComboBox::from_label("Transition")
        .selected_text(format!("{:?}", b.transition))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.transition, Transition::Cut, "Cut");
            ui.selectable_value(&mut b.transition, Transition::Snap, "Snap");
            ui.selectable_value(&mut b.transition, Transition::Fade, "Fade");
        });
}

// ─── TIMELINE ────────────────────────────────────────────────────────

pub fn timeline(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new("\u{23F1} Timeline").size(16.0).strong());
        ui.add_space(8.0);

        // Play/Pause button
        let play_label = if state.playing { "\u{23F8}" } else { "\u{25B6}" };
        let play_btn = egui::Button::new(
            RichText::new(play_label).size(16.0).color(Color32::WHITE),
        )
        .fill(if state.playing {
            Color32::from_rgb(200, 80, 80)
        } else {
            Color32::from_rgb(60, 180, 80)
        })
        .rounding(Rounding::same(6.0))
        .min_size(egui::vec2(32.0, 24.0));
        if ui.add(play_btn).on_hover_text("Play/Pause (Space)").clicked() {
            state.playing = !state.playing;
        }

        // Speed DragValue
        ui.add_space(4.0);
        ui.add(
            egui::DragValue::new(&mut state.playback_speed)
                .range(0.1..=8.0)
                .speed(0.05)
                .prefix("Speed: ")
                .suffix("x"),
        );

        ui.add_space(8.0);

        // Undo/redo buttons
        let can_undo = state.undo.can_undo();
        let can_redo = state.undo.can_redo();
        if ui
            .add_enabled(can_undo, egui::Button::new("\u{21A9}").min_size(egui::vec2(28.0, 22.0)))
            .on_hover_text("Undo (Ctrl+Z)")
            .clicked()
        {
            state.undo();
        }
        if ui
            .add_enabled(can_redo, egui::Button::new("\u{21AA}").min_size(egui::vec2(28.0, 22.0)))
            .on_hover_text("Redo (Ctrl+Y)")
            .clicked()
        {
            state.redo();
        }

        ui.add_space(8.0);

        // Playhead time display
        ui.label(
            RichText::new(format!("{:.2}s / {:.1}s", state.playhead, state.scene.output.duration))
                .size(12.0)
                .color(Color32::from_rgb(180, 180, 220))
                .strong(),
        );

        // Delete / Duplicate / Split / Merge buttons
        ui.add_space(8.0);
        if ui.button("\u{1F5D1}").on_hover_text("Delete (Del)").clicked() {
            state.status = "__DELETE_SELECTED__".into();
        }
        if ui.button("\u{1F4CB}").on_hover_text("Duplicate (Ctrl+D)").clicked() {
            state.status = "__DUPLICATE_SELECTED__".into();
        }
        if ui
            .button("\u{2702}")
            .on_hover_text("Split selected at playhead")
            .clicked()
        {
            state.status = "__SPLIT_AT_PLAYHEAD__".into();
        }
        if ui
            .button("\u{1F517}")
            .on_hover_text("Merge selected with next sibling")
            .clicked()
        {
            state.status = "__MERGE_NEXT__".into();
        }

        // Razor mode toggle
        ui.add_space(4.0);
        let razor_btn = egui::Button::new(
            RichText::new("\u{1FA92}").size(14.0).color(if state.razor_mode {
                Color32::from_rgb(255, 80, 80)
            } else {
                Color32::WHITE
            }),
        )
        .fill(if state.razor_mode {
            Color32::from_rgb(80, 30, 30)
        } else {
            Color32::from_rgb(50, 50, 70)
        })
        .rounding(Rounding::same(6.0))
        .min_size(egui::vec2(28.0, 22.0));
        if ui.add(razor_btn).on_hover_text("Razor tool: click a clip to split at that point").clicked() {
            state.razor_mode = !state.razor_mode;
        }

        // Node editor toggle
        ui.add_space(2.0);
        if ui
            .button("\u{1F9E9}")
            .on_hover_text("Toggle node editor (scaffold)")
            .clicked()
        {
            state.node_editor_open = !state.node_editor_open;
        }
    });
    ui.add_space(2.0);
    ui.separator();

    // ─── Scrubber / Ruler at the top of the track area ───
    let duration = state.scene.output.duration.max(0.01);
    let avail_width = ui.available_width() - 100.0; // reserve label space
    let label_width = 90.0_f32;
    let track_width = avail_width.max(50.0);
    let ruler_height = 20.0;

    let (ruler_rect, ruler_resp) = ui.allocate_exact_size(
        egui::vec2(label_width + track_width + 8.0, ruler_height),
        Sense::click_and_drag(),
    );
    let ruler_track_left = ruler_rect.min.x + label_width + 4.0;
    let ruler_track_right = ruler_track_left + track_width;
    let painter = ui.painter_at(ruler_rect);

    // Draw ruler background
    let ruler_track_rect = egui::Rect::from_min_max(
        egui::pos2(ruler_track_left, ruler_rect.min.y),
        egui::pos2(ruler_track_right, ruler_rect.max.y),
    );
    painter.rect_filled(ruler_track_rect, Rounding::same(2.0), Color32::from_rgb(30, 30, 45));

    // Draw time markers
    let step = choose_ruler_step(duration);
    let mut t = 0.0_f32;
    while t <= duration {
        let frac = t / duration;
        let x = ruler_track_left + frac * track_width;
        let is_major = (t / step).round() as i32 % 5 == 0 || step >= duration;
        let tick_h = if is_major { ruler_height * 0.7 } else { ruler_height * 0.4 };
        painter.line_segment(
            [
                egui::pos2(x, ruler_rect.max.y - tick_h),
                egui::pos2(x, ruler_rect.max.y),
            ],
            Stroke::new(1.0, Color32::from_rgb(100, 100, 130)),
        );
        if is_major {
            painter.text(
                egui::pos2(x, ruler_rect.min.y + 2.0),
                egui::Align2::CENTER_TOP,
                format!("{:.1}s", t),
                egui::FontId::proportional(9.0),
                Color32::from_rgb(150, 150, 180),
            );
        }
        t += step;
    }

    // Draw playhead indicator on ruler
    let ph_frac = (state.playhead / duration).clamp(0.0, 1.0);
    let ph_x = ruler_track_left + ph_frac * track_width;
    // Triangle head
    let tri_size = 5.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(ph_x - tri_size, ruler_rect.min.y),
            egui::pos2(ph_x + tri_size, ruler_rect.min.y),
            egui::pos2(ph_x, ruler_rect.min.y + tri_size * 1.5),
        ],
        Color32::from_rgb(255, 80, 80),
        Stroke::NONE,
    ));
    painter.line_segment(
        [
            egui::pos2(ph_x, ruler_rect.min.y + tri_size * 1.5),
            egui::pos2(ph_x, ruler_rect.max.y),
        ],
        Stroke::new(1.5, Color32::from_rgb(255, 80, 80)),
    );

    // Handle click/drag on ruler to set playhead
    if ruler_resp.clicked() || ruler_resp.dragged() {
        if let Some(pos) = ruler_resp.interact_pointer_pos() {
            let frac = ((pos.x - ruler_track_left) / track_width).clamp(0.0, 1.0);
            state.playhead = frac * duration;
        }
    }
    if ruler_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    ui.add_space(2.0);

    // Visual timeline tracks — use ALL remaining height
    let track_height = 24.0;
    let track_spacing = 3.0;

    let mut to_select: Option<Selection> = None;
    let mut razor_split: Option<(Selection, f32)> = None;

    // Snapshot scene before any drag mutation, commit only if a drag started this frame.
    let scene_snapshot = state.scene.clone();
    let mut drag_started_this_frame = false;

    egui::ScrollArea::vertical().show(ui, |ui| {
        // Backgrounds
        for i in 0..state.scene.backgrounds.len() {
            let id_str = format!("bg-{}", i);
            let label;
            let mut start;
            let mut end;
            {
                let bg = &state.scene.backgrounds[i];
                label = format!("\u{1F304} {}", bg.id);
                start = bg.start;
                end = bg.start + bg.duration;
            }
            let resp = draw_track_bar(
                ui,
                &id_str,
                &label,
                &mut start,
                &mut end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(60, 120, 200),
                state.selection == Selection::Background(i),
                state.playhead,
                state.razor_mode,
            );
            if resp.drag_started {
                drag_started_this_frame = true;
            }
            if resp.changed {
                let bg = &mut state.scene.backgrounds[i];
                let new_start = start.max(0.0);
                let new_end = end.max(new_start + 0.05);
                bg.start = new_start;
                bg.duration = new_end - new_start;
            }
            if resp.clicked {
                if state.razor_mode {
                    if let Some(razor_t) = resp.razor_time {
                        razor_split = Some((Selection::Background(i), razor_t));
                    }
                } else {
                    to_select = Some(Selection::Background(i));
                }
            }
            ui.add_space(track_spacing);
        }

        // Actors — clips use scene output duration as ruler scale for all tracks
        for i in 0..state.scene.actors.len() {
            let id_str = format!("act-{}", i);
            let label;
            let mut start;
            let mut end;
            {
                let a = &state.scene.actors[i];
                label = format!("[{}] \u{1F3AD} {}", i + 1, a.id);
                start = a.t_in.unwrap_or(0.0);
                // If no t_out, span full track width
                end = a.t_out.unwrap_or(duration);
            }
            let resp = draw_track_bar(
                ui,
                &id_str,
                &label,
                &mut start,
                &mut end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(200, 120, 50),
                state.selection == Selection::Actor(i),
                state.playhead,
                state.razor_mode,
            );
            if resp.drag_started {
                drag_started_this_frame = true;
            }
            if resp.changed {
                let a = &mut state.scene.actors[i];
                let new_start = start.max(0.0);
                let new_end = end.max(new_start + 0.05);
                a.t_in = Some(new_start);
                a.t_out = Some(new_end);
                if resp.move_delta != 0.0 {
                    for kf in a.layout.iter_mut() {
                        kf.t = (kf.t + resp.move_delta).max(0.0);
                    }
                }
            }
            if resp.clicked {
                if state.razor_mode {
                    if let Some(razor_t) = resp.razor_time {
                        razor_split = Some((Selection::Actor(i), razor_t));
                    }
                } else {
                    to_select = Some(Selection::Actor(i));
                }
            }
            ui.add_space(track_spacing);
        }

        // Overlays
        for i in 0..state.scene.overlays.len() {
            let id_str = format!("ov-{}", i);
            let label;
            let mut start;
            let mut end;
            {
                let ov = &state.scene.overlays[i];
                label = match ov {
                    Overlay::Text(t) => format!("\u{1F4DD} {}", ellipsis(&t.text, 12)),
                    Overlay::Image(im) => format!("\u{1F5BC} {}", im.id),
                    Overlay::Video(v) => format!("\u{1F3AC} {}", v.id),
                };
                let (s, e) = match ov {
                    Overlay::Text(t) => (t.t_in, t.t_out),
                    Overlay::Image(im) => (im.t_in, im.t_out),
                    Overlay::Video(v) => (v.t_in, v.t_out),
                };
                start = s;
                end = e;
            }
            let resp = draw_track_bar(
                ui,
                &id_str,
                &label,
                &mut start,
                &mut end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(100, 200, 100),
                state.selection == Selection::Overlay(i),
                state.playhead,
                state.razor_mode,
            );
            if resp.drag_started {
                drag_started_this_frame = true;
            }
            if resp.changed {
                let new_start = start.max(0.0);
                let new_end = end.max(new_start + 0.05);
                let move_delta = resp.move_delta;
                let ov = &mut state.scene.overlays[i];
                match ov {
                    Overlay::Text(t) => {
                        t.t_in = new_start;
                        t.t_out = new_end;
                        if move_delta != 0.0 {
                            for kf in t.layout.iter_mut() {
                                kf.t = (kf.t + move_delta).max(0.0);
                            }
                        }
                    }
                    Overlay::Image(im) => {
                        im.t_in = new_start;
                        im.t_out = new_end;
                        if move_delta != 0.0 {
                            for kf in im.layout.iter_mut() {
                                kf.t = (kf.t + move_delta).max(0.0);
                            }
                        }
                    }
                    Overlay::Video(v) => {
                        v.t_in = new_start;
                        v.t_out = new_end;
                        if move_delta != 0.0 {
                            for kf in v.layout.iter_mut() {
                                kf.t = (kf.t + move_delta).max(0.0);
                            }
                        }
                    }
                }
            }
            if resp.clicked {
                if state.razor_mode {
                    if let Some(razor_t) = resp.razor_time {
                        razor_split = Some((Selection::Overlay(i), razor_t));
                    }
                } else {
                    to_select = Some(Selection::Overlay(i));
                }
            }
            ui.add_space(track_spacing);
        }

        // Audio tracks
        for i in 0..state.scene.audio.len() {
            let id_str = format!("audio-{}", i);
            let label;
            let mut start;
            let mut end;
            {
                let audio = &state.scene.audio[i];
                label = format!("\u{1F50A} {}", audio.id);
                start = audio.t_in;
                end = audio.t_out.unwrap_or(duration);
            }
            let resp = draw_track_bar(
                ui,
                &id_str,
                &label,
                &mut start,
                &mut end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(80, 180, 180), // teal for audio
                false, // no selection tracking for audio yet
                state.playhead,
                state.razor_mode,
            );
            if resp.changed {
                let audio = &mut state.scene.audio[i];
                audio.t_in = start.max(0.0);
                audio.t_out = Some(end.max(start + 0.05));
            }
            ui.add_space(track_spacing);
        }

        if state.scene.actors.is_empty()
            && state.scene.overlays.is_empty()
            && state.scene.backgrounds.is_empty()
        {
            ui.add_space(20.0);
            ui.label(
                RichText::new(
                    "\u{1F4AD} Empty scene. Add clips from library \u{2192} they appear here.",
                )
                .italics()
                .color(Color32::from_rgb(120, 120, 140))
                .size(12.0),
            );
        }
    });

    if drag_started_this_frame {
        state.undo.push(&scene_snapshot);
    }

    if let Some(sel) = to_select {
        state.selection = sel;
    }

    // Handle razor split (set selection + playhead, then trigger split)
    if let Some((sel, t)) = razor_split {
        state.selection = sel;
        state.playhead = t;
        state.status = "__SPLIT_AT_PLAYHEAD__".into();
    }
}

/// Result of interacting with a single timeline track row.
struct BarResponse {
    /// User clicked the bar (without dragging).
    clicked: bool,
    /// A drag (resize OR move) started on this frame — caller should snapshot for undo.
    drag_started: bool,
    /// Bar bounds were modified this frame.
    changed: bool,
    /// Net seconds the body was translated this frame (signed). 0 unless body-drag.
    move_delta: f32,
    /// If razor mode is active and the bar was clicked, this is the time at the click point.
    razor_time: Option<f32>,
}

/// Draw a single horizontal track row: label, track background, draggable bar with
/// resize handles on each edge, and the playhead vertical line over this row.
#[allow(clippy::too_many_arguments)]
fn draw_track_bar(
    ui: &mut egui::Ui,
    id_source: &str,
    label: &str,
    t_start: &mut f32,
    t_end: &mut f32,
    total_duration: f32,
    avail_width: f32,
    height: f32,
    color: Color32,
    selected: bool,
    playhead: f32,
    razor_mode: bool,
) -> BarResponse {
    let label_width = 90.0_f32;
    let track_width = avail_width.max(50.0);

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(label_width + track_width + 8.0, height),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);

    // Label column
    let label_rect = egui::Rect::from_min_size(rect.min, egui::vec2(label_width, height));
    painter.text(
        label_rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(180, 180, 200),
    );

    // Track background
    let track_left = rect.min.x + label_width + 4.0;
    let track_top = rect.min.y + 2.0;
    let track_h = height - 4.0;
    let track_rect = egui::Rect::from_min_size(
        egui::pos2(track_left, track_top),
        egui::vec2(track_width, track_h),
    );
    painter.rect_filled(track_rect, Rounding::same(3.0), Color32::from_rgb(25, 25, 38));

    // Compute initial bar geometry (in pixels) from t_start/t_end
    let secs_per_pixel = total_duration / track_width.max(1.0);
    let (bar_left, bar_right) = t_to_px(*t_start, *t_end, total_duration, track_left, track_width);
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(bar_left, track_top + 1.0),
        egui::pos2(bar_right, track_top + track_h - 1.0),
    );

    // Compute handle / body sub-rects.
    let handle_w = 6.0_f32.min(bar_rect.width() / 3.0).max(2.0);
    let body_rect = egui::Rect::from_min_max(
        egui::pos2(bar_rect.min.x + handle_w, bar_rect.min.y),
        egui::pos2(bar_rect.max.x - handle_w, bar_rect.max.y),
    );
    let left_handle = egui::Rect::from_min_max(
        egui::pos2(bar_rect.min.x - 1.0, bar_rect.min.y),
        egui::pos2(bar_rect.min.x + handle_w, bar_rect.max.y),
    );
    let right_handle = egui::Rect::from_min_max(
        egui::pos2(bar_rect.max.x - handle_w, bar_rect.min.y),
        egui::pos2(bar_rect.max.x + 1.0, bar_rect.max.y),
    );

    // Allocate independent interactions. egui's hit-testing prefers the
    // most-recently-allocated overlapping widget, so allocate body first
    // and the edge handles last so they win over body on the edges.
    let id_root = ui.make_persistent_id(id_source);
    let body = ui
        .interact(body_rect, id_root.with("body"), Sense::click_and_drag())
        .on_hover_cursor(if razor_mode {
            egui::CursorIcon::Crosshair
        } else {
            egui::CursorIcon::Grab
        });
    let left = ui
        .interact(left_handle, id_root.with("left"), Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    let right = ui
        .interact(right_handle, id_root.with("right"), Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);

    let mut changed = false;
    let mut move_delta = 0.0_f32;
    let drag_started = body.drag_started() || left.drag_started() || right.drag_started();

    if left.dragged() {
        let dx = left.drag_delta().x;
        let new_start = (*t_start + dx * secs_per_pixel).clamp(0.0, *t_end - 0.05);
        if (new_start - *t_start).abs() > f32::EPSILON {
            *t_start = new_start;
            changed = true;
        }
    }
    if right.dragged() {
        let dx = right.drag_delta().x;
        let new_end = (*t_end + dx * secs_per_pixel).clamp(*t_start + 0.05, total_duration);
        if (new_end - *t_end).abs() > f32::EPSILON {
            *t_end = new_end;
            changed = true;
        }
    }
    if body.dragged() {
        let dx = body.drag_delta().x;
        let delta_secs = dx * secs_per_pixel;
        let dur = *t_end - *t_start;
        let new_start = (*t_start + delta_secs).clamp(0.0, (total_duration - dur).max(0.0));
        let actual = new_start - *t_start;
        if actual.abs() > f32::EPSILON {
            *t_start = new_start;
            *t_end = new_start + dur;
            move_delta = actual;
            changed = true;
        }
    }

    // Re-compute final bar after any drag.
    let (bar_left, bar_right) = t_to_px(*t_start, *t_end, total_duration, track_left, track_width);
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(bar_left, track_top + 1.0),
        egui::pos2(bar_right.max(bar_left + 4.0), track_top + track_h - 1.0),
    );

    let any_hover = body.hovered() || left.hovered() || right.hovered();
    let fill = if selected {
        Color32::from_rgb(
            color.r().saturating_add(40),
            color.g().saturating_add(40),
            color.b().saturating_add(40),
        )
    } else if any_hover {
        Color32::from_rgb(
            color.r().saturating_add(20),
            color.g().saturating_add(20),
            color.b().saturating_add(20),
        )
    } else {
        color
    };
    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Draw subtle handle indicators inside the bar.
    let indicator_w = handle_w.min(bar_rect.width() / 4.0).max(1.5);
    let handle_color = Color32::from_rgba_premultiplied(255, 255, 255, 90);
    let left_h_rect = egui::Rect::from_min_max(
        bar_rect.min,
        egui::pos2(bar_rect.min.x + indicator_w, bar_rect.max.y),
    );
    let right_h_rect = egui::Rect::from_min_max(
        egui::pos2(bar_rect.max.x - indicator_w, bar_rect.min.y),
        bar_rect.max,
    );
    painter.rect_filled(left_h_rect, Rounding::same(2.0), handle_color);
    painter.rect_filled(right_h_rect, Rounding::same(2.0), handle_color);

    // Selection border
    if selected {
        painter.rect_stroke(
            bar_rect.expand(1.0),
            Rounding::same(5.0),
            Stroke::new(2.0, Color32::WHITE),
        );
    }

    // Time label inside bar
    if bar_rect.width() > 50.0 {
        painter.text(
            bar_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{:.1}-{:.1}s", *t_start, *t_end),
            egui::FontId::proportional(9.0),
            Color32::WHITE,
        );
    }

    // Playhead vertical line over this track segment (drawn LAST so it sits on top).
    if playhead >= 0.0 && playhead <= total_duration {
        let ph_x = track_left + (playhead / total_duration).clamp(0.0, 1.0) * track_width;
        painter.line_segment(
            [
                egui::pos2(ph_x, track_top - 1.0),
                egui::pos2(ph_x, track_top + track_h + 1.0),
            ],
            Stroke::new(1.5, Color32::from_rgb(255, 80, 80)),
        );
    }

    BarResponse {
        clicked: body.clicked() || left.clicked() || right.clicked(),
        drag_started,
        changed,
        move_delta,
        razor_time: if razor_mode && (body.clicked() || left.clicked() || right.clicked()) {
            // Compute the time at the click position
            if let Some(pos) = body.interact_pointer_pos()
                .or_else(|| left.interact_pointer_pos())
                .or_else(|| right.interact_pointer_pos())
            {
                let frac = ((pos.x - track_left) / track_width).clamp(0.0, 1.0);
                Some(frac * total_duration)
            } else {
                None
            }
        } else {
            None
        },
    }
}

fn t_to_px(t_start: f32, t_end: f32, total: f32, track_left: f32, track_width: f32) -> (f32, f32) {
    let s = (t_start / total).clamp(0.0, 1.0);
    let e = (t_end / total).clamp(0.0, 1.0);
    let left = track_left + s * track_width;
    let right = (track_left + e * track_width).max(left + 4.0);
    (left, right)
}

// ─── PREVIEW ─────────────────────────────────────────────────────────

/// Apply chroma-key processing to a ColorImage in-place.
/// Uses RGB Euclidean distance (normalized to 0..1) for keying.
fn apply_chroma_key(image: &mut egui::ColorImage, params: &memstroy_core::ChromaKeyParams) {
    let kr = params.key_color[0] as f32 / 255.0;
    let kg = params.key_color[1] as f32 / 255.0;
    let kb = params.key_color[2] as f32 / 255.0;

    let similarity = params.similarity;
    let blend = params.blend;
    let spill = params.spill;

    for pixel in image.pixels.iter_mut() {
        let r = pixel.r() as f32 / 255.0;
        let g = pixel.g() as f32 / 255.0;
        let b = pixel.b() as f32 / 255.0;

        // Euclidean distance in RGB space, normalized by sqrt(3) so max distance = 1.0
        let dr = r - kr;
        let dg = g - kg;
        let db = b - kb;
        let distance = (dr * dr + dg * dg + db * db).sqrt() / 1.732_050_8; // sqrt(3)

        if distance < similarity {
            // Fully transparent
            *pixel = Color32::from_rgba_unmultiplied(pixel.r(), pixel.g(), pixel.b(), 0);
        } else if distance < similarity + blend {
            // Feathered edge: proportional alpha
            let t = (distance - similarity) / blend.max(0.001);
            let alpha = (t * 255.0).round() as u8;
            // Spill suppression on feathered pixels
            let mut new_g = pixel.g() as f32;
            let spill_factor = spill * (1.0 - distance);
            new_g = (new_g - new_g * spill_factor).max(0.0);
            *pixel = Color32::from_rgba_unmultiplied(
                pixel.r(),
                new_g.round() as u8,
                pixel.b(),
                alpha,
            );
        } else {
            // Spill suppression for pixels near the key color
            if distance < similarity + blend + 0.15 {
                let proximity = 1.0 - ((distance - similarity - blend) / 0.15).clamp(0.0, 1.0);
                let spill_factor = spill * proximity;
                let new_g = (pixel.g() as f32 * (1.0 - spill_factor)).round() as u8;
                *pixel = Color32::from_rgba_unmultiplied(pixel.r(), new_g, pixel.b(), pixel.a());
            }
        }
    }
}

pub fn preview(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading(RichText::new("\u{1F440} Preview").size(16.0).strong());
    ui.add_space(4.0);

    let avail = ui.available_size_before_wrap();
    let target_aspect =
        state.scene.output.resolution[0] as f32 / state.scene.output.resolution[1] as f32;
    let mut h = avail.y.min(800.0);
    let mut w = h * target_aspect;
    if w > avail.x {
        w = avail.x;
        h = w / target_aspect;
    }

    // Center the preview rect both horizontally and vertically
    let offset_x = (avail.x - w) * 0.5;
    let offset_y = (avail.y - h) * 0.5;

    // Make preview clickable for eyedropper
    let preview_sense = if state.eyedropper_active {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };

    // Reserve full available space then position content in center
    let (full_rect, preview_resp) = ui.allocate_exact_size(avail, preview_sense);
    let rect = egui::Rect::from_min_size(
        egui::pos2(full_rect.min.x + offset_x, full_rect.min.y + offset_y),
        egui::vec2(w, h),
    );

    // Draw preview background
    ui.painter().rect_filled(
        rect,
        Rounding::same(8.0),
        Color32::from_rgb(10, 10, 16),
    );

    // Layer compositing: iterate actors bottom-to-top
    let t = state.playhead;
    let mut any_frame_shown = false;
    let eyedropper_active = state.eyedropper_active;

    // Collect actor data we need (to avoid borrow conflicts with frame_caches)
    let actor_data: Vec<_> = state.scene.actors.iter().enumerate().map(|(idx, actor)| {
        (
            idx,
            actor.visible,
            actor.t_in.unwrap_or(0.0),
            actor.t_out.unwrap_or(f32::MAX),
            actor.source_start,
            actor.chroma_key.clone(),
        )
    }).collect();

    for (actor_idx, visible, t_in, t_out, source_start, chroma_key) in actor_data.iter() {
        // Skip invisible actors
        if !visible {
            continue;
        }

        // Check if this actor is active at current playhead time
        if t < *t_in || t > *t_out {
            continue; // clip not active at this time
        }

        // Compute local time within this clip
        let local_t = t - t_in + source_start;

        // Get frame from this actor's cache
        if let Some(fc) = state.frame_caches.get_mut(*actor_idx) {
            if fc.is_ready() {
                if let Some(mut img) = fc.raw_frame_at_time(local_t) {
                    // Apply chroma-key ONLY if eyedropper is NOT active
                    if !eyedropper_active {
                        apply_chroma_key(&mut img, chroma_key);
                    }
                    // Upload to texture and display
                    let tex = ui.ctx().load_texture(
                        format!("layer_{}", actor_idx),
                        img,
                        egui::TextureOptions::LINEAR,
                    );
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
                    any_frame_shown = true;
                }
            } else if fc.extracting {
                // Show extracting indicator only if no other frame is shown
                if !any_frame_shown {
                    ui.put(rect, egui::Label::new(
                        RichText::new("\u{23F3} Extracting frames...")
                            .color(Color32::from_rgb(180, 150, 60)).size(14.0),
                    ));
                }
            }
        }
    }

    // Handle eyedropper click
    if state.eyedropper_active && preview_resp.clicked() {
        if let Some(pos) = preview_resp.interact_pointer_pos() {
            // Compute UV coordinates within the preview rect
            let u = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let v = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);

            // Sample from the selected actor's raw frame at current playhead
            if let Selection::Actor(idx) = state.selection {
                if idx < state.scene.actors.len() {
                    let actor_t_in = state.scene.actors[idx].t_in.unwrap_or(0.0);
                    let actor_source_start = state.scene.actors[idx].source_start;
                    let local_t = state.playhead - actor_t_in + actor_source_start;
                    if let Some(fc) = state.frame_caches.get_mut(idx) {
                        if let Some(img) = fc.raw_frame_at_time(local_t) {
                            let px = (u * img.size[0] as f32) as usize;
                            let py = (v * img.size[1] as f32) as usize;
                            let px = px.min(img.size[0].saturating_sub(1));
                            let py = py.min(img.size[1].saturating_sub(1));
                            let pixel = img.pixels[py * img.size[0] + px];
                            // Set the chroma key color
                            state.scene.actors[idx].chroma_key.key_color = [pixel.r(), pixel.g(), pixel.b()];
                            state.status = format!("Picked color: ({}, {}, {})", pixel.r(), pixel.g(), pixel.b());
                        }
                    }
                }
            }
            state.eyedropper_active = false;
        }
    }

    // Show eyedropper cursor
    if state.eyedropper_active && preview_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }

    if !any_frame_shown && state.frame_caches.is_empty() {
        // Fallback: old PNG-based preview or placeholder
        if let Some(p) = &state.last_preview {
            let uri = format!("file://{}", p.display());
            ui.put(rect, egui::Image::from_uri(uri).fit_to_exact_size(rect.size()));
        } else {
            ui.put(
                rect,
                egui::Label::new(
                    RichText::new(
                        "\u{1F3AC}\n\nRender \u{2192} Preview frame\nto see your meme here",
                    )
                    .color(Color32::from_rgb(80, 80, 100))
                    .size(16.0),
                ),
            );
        }
    } else if !any_frame_shown {
        // Frame caches exist but no clip is active at current time
        ui.put(
            rect,
            egui::Label::new(
                RichText::new("\u{1F3AC}\n\nNo clip active at this time")
                    .color(Color32::from_rgb(80, 80, 100))
                    .size(14.0),
            ),
        );
    }
}

// ─── HELPERS ─────────────────────────────────────────────────────────

/// Choose a reasonable step size for ruler tick marks based on total duration.
fn choose_ruler_step(duration: f32) -> f32 {
    if duration <= 2.0 {
        0.1
    } else if duration <= 10.0 {
        0.5
    } else if duration <= 30.0 {
        1.0
    } else if duration <= 60.0 {
        2.0
    } else if duration <= 300.0 {
        5.0
    } else {
        10.0
    }
}

fn color_edit_u8(ui: &mut egui::Ui, c: &mut [u8; 3]) {
    let mut rgb = [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
    ];
    if ui.color_edit_button_rgb(&mut rgb).changed() {
        c[0] = (rgb[0] * 255.0).round() as u8;
        c[1] = (rgb[1] * 255.0).round() as u8;
        c[2] = (rgb[2] * 255.0).round() as u8;
    }
}

fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push_str("...");
        out
    }
}

/// Strip emoji spam and noise words (Имба, Топ, Херня, etc) from clip descriptions.
fn clean_clip_text(raw: &str) -> String {
    let noise: &[&str] = &[
        "Имба", "Топ", "Херня", "имба", "топ", "херня",
        "—", "\u{2014}", "\u{2764}\u{FE0F}\u{200D}\u{1F525}", // ❤️‍🔥
    ];
    let mut s = raw.to_string();
    // Remove common emoji
    s = s.chars().filter(|c| {
        // Keep ASCII, Cyrillic, and basic punctuation
        c.is_ascii() || ('\u{0400}'..='\u{04FF}').contains(c) || *c == ' ' || *c == '.' || *c == ',' || *c == '!' || *c == '?'
    }).collect();
    for n in noise {
        s = s.replace(n, "");
    }
    // Collapse multiple spaces/dashes
    while s.contains("  ") { s = s.replace("  ", " "); }
    while s.contains("--") { s = s.replace("--", "-"); }
    s = s.trim_matches(|c: char| c == ' ' || c == '-' || c == '\u{2014}').to_string();
    s
}

fn add_actor_from_clip(state: &mut EditorState, path: &PathBuf) {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("mellstroy_{}", s))
        .unwrap_or_else(|| format!("actor_{}", state.scene.actors.len() + 1));

    // Probe video duration via ffprobe for accurate timeline display
    let clip_duration = probe_video_duration(path);

    let actor = Actor {
        id: id.clone(),
        source: path.clone(),
        anchors: None,
        chroma_key: ChromaKeyParams::default(),
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: Some(0.0),
        t_out: Some(clip_duration),
        source_start: 0.0,
        loop_source: true,
        flip_horizontal: false,
        attachments: Vec::new(),
        visible: true,
    };

    // Expand scene output duration to fit the clip if needed
    if clip_duration > state.scene.output.duration {
        state.scene.output.duration = clip_duration;
    }

    state.scene.actors.push(actor);
    state.selection = Selection::Actor(state.scene.actors.len() - 1);
    state.status = "__EXTRACT_FRAMES__".into();
}

/// Probe video duration using ffprobe. Returns duration in seconds, or 8.0 as fallback.
fn probe_video_duration(path: &std::path::Path) -> f32 {
    let ffmpeg_bin = memstroy_render::ffmpeg_binary();
    let ffprobe = {
        let mut p = ffmpeg_bin.clone();
        p.set_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
        if p.exists() { p } else { std::path::PathBuf::from("ffprobe") }
    };

    match std::process::Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.trim().parse::<f32>().unwrap_or(8.0)
        }
        Err(_) => 8.0,
    }
}

fn add_background_from_path(state: &mut EditorState, path: &PathBuf) {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("bg_{}", s))
        .unwrap_or_else(|| format!("bg_{}", state.scene.backgrounds.len() + 1));
    let kind_image = matches!(
        path.extension().and_then(|s| s.to_str()).map(|s| s.to_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp")
    );
    let source = if kind_image {
        MediaSource::Image { path: path.clone() }
    } else {
        MediaSource::Video { path: path.clone(), r#loop: true, start_at: 0.0 }
    };
    let bg = Background {
        id: id.clone(),
        source,
        start: 0.0,
        duration: state.scene.output.duration,
        fit: Fit::Cover,
        transition: Transition::Cut,
    };
    state.scene.backgrounds.push(bg);
    state.selection = Selection::Background(state.scene.backgrounds.len() - 1);
    state.status = format!("\u{2705} Added background: {}", id);
}
