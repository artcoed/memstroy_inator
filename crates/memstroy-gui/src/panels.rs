//! UI panels — modern dark theme with emoji and accent colors.

use std::path::PathBuf;

use egui::{Color32, RichText, Rounding, Vec2};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

// ─── LIBRARY ─────────────────────────────────────────────────────────

pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new("\u{1F3AC} Library").size(18.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(
                RichText::new("\u{1F504} Refresh Clips")
                    .color(Color32::WHITE)
                    .size(13.0),
            )
            .fill(Color32::from_rgb(80, 50, 180))
            .rounding(Rounding::same(8.0));

            if ui.add_enabled(!state.refreshing, btn).clicked() {
                // Signal the app to start refresh
                state.status = "__REFRESH_REQUESTED__".into();
            }
        });
    });

    ui.add_space(6.0);

    // Mellstroy clips section
    let clip_count = state.library.mellstroy_clips.len();
    egui::CollapsingHeader::new(
        RichText::new(format!("\u{1F525} Mellstroy Clips ({})", clip_count))
            .size(14.0)
            .color(Color32::from_rgb(255, 150, 50)),
    )
    .default_open(true)
    .show(ui, |ui| {
        if state.library.mellstroy_clips.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("No clips yet.\nHit \u{1F504} Refresh Clips to download from Telegram!")
                    .italics()
                    .color(Color32::from_rgb(140, 140, 160))
                    .size(12.0),
            );
            ui.add_space(8.0);
        } else {
            egui::ScrollArea::vertical()
                .max_height(400.0)
                .show(ui, |ui| {
                    let clips = state.library.mellstroy_clips.clone();
                    for clip in &clips {
                        clip_card(ui, state, clip);
                    }
                });
        }
    });

    ui.add_space(10.0);

    // Backgrounds
    egui::CollapsingHeader::new(
        RichText::new(format!(
            "\u{1F5BC} Backgrounds ({})",
            state.library.backgrounds.len()
        ))
        .size(14.0)
        .color(Color32::from_rgb(100, 180, 255)),
    )
    .show(ui, |ui| {
        if state.library.backgrounds.is_empty() {
            ui.label(
                RichText::new("Drop images/videos into assets/backgrounds/")
                    .italics()
                    .color(Color32::from_rgb(140, 140, 160))
                    .size(12.0),
            );
        }
        for p in state.library.backgrounds.clone() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
            if ui
                .button(RichText::new(format!("\u{1F5BC} {}", name)).size(12.0))
                .clicked()
            {
                add_background_from_path(state, &p);
            }
        }
    });

    ui.add_space(10.0);

    // Props
    egui::CollapsingHeader::new(
        RichText::new(format!("\u{1F3A9} Props ({})", state.library.props.len()))
            .size(14.0)
            .color(Color32::from_rgb(255, 100, 200)),
    )
    .show(ui, |ui| {
        if state.library.props.is_empty() {
            ui.label(
                RichText::new("Drop PNG props into assets/props/")
                    .italics()
                    .color(Color32::from_rgb(140, 140, 160))
                    .size(12.0),
            );
        }
        for p in state.library.props.clone() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
            ui.label(RichText::new(format!("\u{1F3A9} {}", name)).size(12.0));
        }
    });
}

fn clip_card(ui: &mut egui::Ui, state: &mut EditorState, clip: &crate::state::LibraryClip) {
    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 32, 48))
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::same(8.0))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(50, 50, 70)));

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            // Add button
            let add_btn = egui::Button::new(
                RichText::new("+").size(16.0).color(Color32::WHITE).strong(),
            )
            .fill(Color32::from_rgb(60, 180, 80))
            .rounding(Rounding::same(12.0))
            .min_size(Vec2::new(28.0, 28.0));

            if ui.add(add_btn).on_hover_text("Add as actor to scene").clicked() {
                add_actor_from_clip(state, &clip.path);
            }

            ui.vertical(|ui| {
                ui.label(
                    RichText::new(format!("#{}", clip.id))
                        .size(11.0)
                        .color(Color32::from_rgb(140, 100, 255))
                        .strong(),
                );
                let desc = if clip.description.len() > 60 {
                    format!("{}...", &clip.description[..57])
                } else if clip.description.is_empty() {
                    "No description".to_string()
                } else {
                    clip.description.clone()
                };
                ui.label(
                    RichText::new(desc)
                        .size(11.0)
                        .color(Color32::from_rgb(180, 180, 200)),
                );
            });
        });
    });
    ui.add_space(4.0);
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
            if let Some(a) = state.scene.actors.get_mut(i) {
                actor_editor(ui, a);
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
        let mut to_remove = None;
        for (i, kf) in a.layout.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut kf.t).range(0.0..=600.0).speed(0.05).prefix("t="));
                ui.add(egui::DragValue::new(&mut kf.value.pos[0]).range(-2.0..=3.0).speed(0.01).prefix("x="));
                ui.add(egui::DragValue::new(&mut kf.value.pos[1]).range(-2.0..=3.0).speed(0.01).prefix("y="));
                ui.add(egui::DragValue::new(&mut kf.value.scale).range(0.05..=8.0).speed(0.01).prefix("s="));
                if ui.small_button("\u{2716}").clicked() {
                    to_remove = Some(i);
                }
            });
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
        ui.add_space(12.0);

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

        ui.add_space(12.0);

        // Playhead slider
        ui.add(
            egui::Slider::new(&mut state.playhead, 0.0..=state.scene.output.duration)
                .text("s")
                .show_value(true),
        );

        // Delete / Duplicate buttons
        ui.add_space(8.0);
        if ui.button("\u{1F5D1}").on_hover_text("Delete selected (Del)").clicked() {
            // Will be handled by app shortcut logic via flag
            state.status = "__DELETE_SELECTED__".into();
        }
        if ui.button("\u{1F4CB}").on_hover_text("Duplicate selected (Ctrl+D)").clicked() {
            state.status = "__DUPLICATE_SELECTED__".into();
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(2.0);

    // Visual timeline tracks
    let duration = state.scene.output.duration;
    let avail_width = ui.available_width() - 100.0; // reserve label space
    let track_height = 24.0;
    let track_spacing = 3.0;

    let mut to_select: Option<Selection> = None;

    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
        // Backgrounds
        for (i, bg) in state.scene.backgrounds.clone().iter().enumerate() {
            let selected = state.selection == Selection::Background(i);
            if draw_track_bar(
                ui,
                &format!("\u{1F304} {}", bg.id),
                bg.start,
                bg.start + bg.duration,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(60, 120, 200),
                selected,
            ) {
                to_select = Some(Selection::Background(i));
            }
            ui.add_space(track_spacing);
        }

        // Actors
        for (i, actor) in state.scene.actors.clone().iter().enumerate() {
            let t_start = actor.t_in.unwrap_or(0.0);
            let t_end = actor.t_out.unwrap_or(duration);
            let selected = state.selection == Selection::Actor(i);
            if draw_track_bar(
                ui,
                &format!("\u{1F3AD} {}", actor.id),
                t_start,
                t_end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(200, 120, 50),
                selected,
            ) {
                to_select = Some(Selection::Actor(i));
            }
            ui.add_space(track_spacing);
        }

        // Overlays
        for (i, ov) in state.scene.overlays.clone().iter().enumerate() {
            let (label, t_start, t_end) = match ov {
                Overlay::Text(t) => (
                    format!("\u{1F4DD} {}", ellipsis(&t.text, 12)),
                    t.t_in,
                    t.t_out,
                ),
                Overlay::Image(im) => (format!("\u{1F5BC} {}", im.id), im.t_in, im.t_out),
                Overlay::Video(v) => (format!("\u{1F3AC} {}", v.id), v.t_in, v.t_out),
            };
            let selected = state.selection == Selection::Overlay(i);
            if draw_track_bar(
                ui,
                &label,
                t_start,
                t_end,
                duration,
                avail_width,
                track_height,
                Color32::from_rgb(100, 200, 100),
                selected,
            ) {
                to_select = Some(Selection::Overlay(i));
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

    if let Some(sel) = to_select {
        state.selection = sel;
    }

    // Draw playhead marker
    // (visual only — the actual position is controlled by the slider above)
}

/// Draw a single horizontal track bar. Returns true if clicked.
fn draw_track_bar(
    ui: &mut egui::Ui,
    label: &str,
    t_start: f32,
    t_end: f32,
    total_duration: f32,
    avail_width: f32,
    height: f32,
    color: Color32,
    selected: bool,
) -> bool {
    let label_width = 90.0;
    let track_width = avail_width;

    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(label_width + track_width + 8.0, height),
        egui::Sense::click_and_drag(),
    );

    let painter = ui.painter_at(rect);

    // Label
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
    let track_rect = egui::Rect::from_min_size(
        egui::pos2(track_left, rect.min.y + 2.0),
        egui::vec2(track_width, height - 4.0),
    );
    painter.rect_filled(track_rect, Rounding::same(3.0), Color32::from_rgb(25, 25, 38));

    // Bar representing the element's time span
    let bar_start_frac = (t_start / total_duration).clamp(0.0, 1.0);
    let bar_end_frac = (t_end / total_duration).clamp(0.0, 1.0);
    let bar_left = track_left + bar_start_frac * track_width;
    let bar_right = track_left + bar_end_frac * track_width;
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(bar_left, rect.min.y + 3.0),
        egui::pos2(bar_right.max(bar_left + 4.0), rect.min.y + height - 3.0),
    );

    let fill = if selected {
        color.gamma_multiply(1.3)
    } else if resp.hovered() {
        color.gamma_multiply(1.1)
    } else {
        color
    };

    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Selection border
    if selected {
        painter.rect_stroke(
            bar_rect.expand(1.0),
            Rounding::same(5.0),
            egui::Stroke::new(2.0, Color32::WHITE),
        );
    }

    // Time label inside bar
    if bar_rect.width() > 40.0 {
        painter.text(
            bar_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{:.1}-{:.1}s", t_start, t_end),
            egui::FontId::proportional(9.0),
            Color32::WHITE,
        );
    }

    resp.clicked()
}

// ─── PREVIEW ─────────────────────────────────────────────────────────

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
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());

    // Draw preview background
    ui.painter().rect_filled(
        rect,
        Rounding::same(8.0),
        Color32::from_rgb(10, 10, 16),
    );

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
}

// ─── HELPERS ─────────────────────────────────────────────────────────

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

fn add_actor_from_clip(state: &mut EditorState, path: &PathBuf) {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("mellstroy_{}", s))
        .unwrap_or_else(|| format!("actor_{}", state.scene.actors.len() + 1));
    let actor = Actor {
        id: id.clone(),
        source: path.clone(),
        anchors: None,
        chroma_key: ChromaKeyParams::default(),
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: None,
        t_out: None,
        source_start: 0.0,
        loop_source: true,
        flip_horizontal: false,
        attachments: Vec::new(),
    };
    state.scene.actors.push(actor);
    state.selection = Selection::Actor(state.scene.actors.len() - 1);
    state.status = format!("\u{2705} Added actor: {}", id);
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
