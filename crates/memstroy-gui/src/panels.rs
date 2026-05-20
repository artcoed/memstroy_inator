//! UI panels. Each function takes the editor state by mutable reference
//! and the egui context; they are wired up from `App::update`.

use std::path::PathBuf;

use egui::{Color32, RichText};
use memstroy_core::*;

use crate::state::{EditorState, Selection};

pub fn library(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading("Library");
    ui.separator();

    ui.collapsing("Mellstroy clips", |ui| {
        if state.library.mellstroy_clips.is_empty() {
            ui.label(RichText::new("No clips found.").italics().color(Color32::GRAY));
            ui.label("Run Channel → Download to populate.");
        }
        for clip in state.library.mellstroy_clips.clone() {
            let name = clip
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(?)")
                .to_string();
            ui.horizontal(|ui| {
                if ui.button("+").on_hover_text("Add as actor").clicked() {
                    add_actor_from_clip(state, &clip);
                }
                ui.label(name);
            });
        }
    });

    ui.collapsing("Backgrounds", |ui| {
        if state.library.backgrounds.is_empty() {
            ui.label(RichText::new("Drop images/videos into assets/backgrounds.").italics());
        }
        for p in state.library.backgrounds.clone() {
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(?)")
                .to_string();
            if ui.button(name).clicked() {
                add_background_from_path(state, &p);
            }
        }
    });

    ui.collapsing("Props", |ui| {
        if state.library.props.is_empty() {
            ui.label(RichText::new("Drop PNG/SVG props into assets/props.").italics());
        }
        for p in state.library.props.clone() {
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(?)")
                .to_string();
            ui.label(name);
        }
    });
}

pub fn inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading("Inspector");
    ui.separator();

    match state.selection {
        Selection::None => {
            ui.label(
                RichText::new("Select an actor, overlay, or background.")
                    .italics()
                    .color(Color32::GRAY),
            );
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
            ui.label("Camera keyframe editing — coming soon.");
        }
    }
}

fn output_spec_editor(ui: &mut egui::Ui, spec: &mut OutputSpec) {
    ui.collapsing("Output", |ui| {
        ui.horizontal(|ui| {
            ui.label("Resolution:");
            ui.add(egui::DragValue::new(&mut spec.resolution[0]).range(64..=4096));
            ui.label("×");
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
    ui.label(RichText::new(format!("Actor: {}", a.id)).strong());
    ui.label(format!("Source: {}", a.source.display()));
    ui.checkbox(&mut a.flip_horizontal, "Flip horizontally");
    ui.checkbox(&mut a.loop_source, "Loop source");
    ui.add(egui::DragValue::new(&mut a.source_start).speed(0.05).prefix("source_start="));

    ui.collapsing("Chroma key", |ui| {
        ui.horizontal(|ui| {
            ui.label("Color:");
            color_edit_u8(ui, &mut a.chroma_key.key_color);
        });
        ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text("similarity"));
        ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text("blend"));
        ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text("spill"));
    });

    ui.collapsing("Layout keyframes", |ui| {
        if a.layout.is_empty() {
            if ui.button("Add starting keyframe").clicked() {
                a.layout.push(Keyframe::new(0.0, ActorState::default()));
            }
        }
        let mut to_remove = None;
        for (i, kf) in a.layout.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("t={:.2}", kf.t));
                ui.add(egui::DragValue::new(&mut kf.t).range(0.0..=600.0).speed(0.05));
                ui.add(egui::DragValue::new(&mut kf.value.pos[0]).range(-2.0..=3.0).speed(0.01).prefix("x="));
                ui.add(egui::DragValue::new(&mut kf.value.pos[1]).range(-2.0..=3.0).speed(0.01).prefix("y="));
                ui.add(egui::DragValue::new(&mut kf.value.scale).range(0.05..=8.0).speed(0.01).prefix("s="));
                if ui.small_button("✕").clicked() { to_remove = Some(i); }
            });
        }
        if let Some(i) = to_remove { a.layout.remove(i); }
        if ui.button("+ Add keyframe").clicked() {
            let last = a.layout.last().cloned().unwrap_or_else(|| Keyframe::new(0.0, ActorState::default()));
            a.layout.push(Keyframe::new(last.t + 1.0, last.value));
        }
    });

    ui.collapsing(format!("Attachments ({})", a.attachments.len()), |ui| {
        ui.label(
            RichText::new(
                "Attachments follow body anchors detected by the pose pass. \
                 Run Tools → Detect anchors on the source clip first.",
            )
            .italics(),
        );
    });
}

fn overlay_editor(ui: &mut egui::Ui, o: &mut Overlay) {
    match o {
        Overlay::Text(t) => {
            ui.label(RichText::new(format!("Text overlay: {}", t.id)).strong());
            ui.text_edit_multiline(&mut t.text);
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut t.t_in).speed(0.05).prefix("in="));
                ui.add(egui::DragValue::new(&mut t.t_out).speed(0.05).prefix("out="));
            });
            ui.add(egui::Slider::new(&mut t.style.font_size, 16.0..=512.0).text("font_size"));
            ui.horizontal(|ui| {
                ui.label("color:");
                color_edit_u8(ui, &mut t.style.color);
            });
            let mut has_box = t.style.box_color.is_some();
            ui.checkbox(&mut has_box, "white plate");
            if has_box && t.style.box_color.is_none() {
                t.style.box_color = Some([255, 255, 255]);
            }
            if !has_box {
                t.style.box_color = None;
            }
            if let Some(box_color) = &mut t.style.box_color {
                color_edit_u8(ui, box_color);
                ui.add(egui::Slider::new(&mut t.style.box_padding, 0.0..=128.0).text("padding"));
            }
        }
        Overlay::Image(i) => {
            ui.label(RichText::new(format!("Image overlay: {}", i.id)).strong());
            ui.label(format!("Source: {}", i.source.display()));
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut i.t_in).speed(0.05).prefix("in="));
                ui.add(egui::DragValue::new(&mut i.t_out).speed(0.05).prefix("out="));
            });
        }
        Overlay::Video(v) => {
            ui.label(RichText::new(format!("Video overlay: {}", v.id)).strong());
            ui.label(format!("Source: {}", v.source.display()));
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut v.t_in).speed(0.05).prefix("in="));
                ui.add(egui::DragValue::new(&mut v.t_out).speed(0.05).prefix("out="));
            });
        }
    }
}

fn background_editor(ui: &mut egui::Ui, b: &mut Background) {
    ui.label(RichText::new(format!("Background: {}", b.id)).strong());
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(&mut b.start).speed(0.05).prefix("start="));
        ui.add(egui::DragValue::new(&mut b.duration).speed(0.05).prefix("duration="));
    });
    egui::ComboBox::from_label("fit")
        .selected_text(format!("{:?}", b.fit))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.fit, Fit::Cover, "Cover");
            ui.selectable_value(&mut b.fit, Fit::Contain, "Contain");
            ui.selectable_value(&mut b.fit, Fit::Stretch, "Stretch");
            ui.selectable_value(&mut b.fit, Fit::Original, "Original");
        });
    egui::ComboBox::from_label("transition")
        .selected_text(format!("{:?}", b.transition))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut b.transition, Transition::Cut, "Cut");
            ui.selectable_value(&mut b.transition, Transition::Snap, "Snap");
            ui.selectable_value(&mut b.transition, Transition::Fade, "Fade");
            ui.selectable_value(&mut b.transition, Transition::SlideLeft, "Slide left");
            ui.selectable_value(&mut b.transition, Transition::SlideRight, "Slide right");
            ui.selectable_value(&mut b.transition, Transition::SlideUp, "Slide up");
            ui.selectable_value(&mut b.transition, Transition::SlideDown, "Slide down");
        });
}

pub fn timeline(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading("Timeline");
    ui.add(
        egui::Slider::new(&mut state.playhead, 0.0..=state.scene.output.duration as f32)
            .text("playhead (s)"),
    );
    ui.separator();

    let mut to_select: Option<Selection> = None;
    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
        for (i, b) in state.scene.backgrounds.iter().enumerate() {
            let resp = ui.selectable_label(
                state.selection == Selection::Background(i),
                format!("[bg] {} ({:.1}s @ {:.1})", b.id, b.duration, b.start),
            );
            if resp.clicked() {
                to_select = Some(Selection::Background(i));
            }
        }
        for (i, a) in state.scene.actors.iter().enumerate() {
            let resp = ui.selectable_label(
                state.selection == Selection::Actor(i),
                format!("[actor] {}", a.id),
            );
            if resp.clicked() {
                to_select = Some(Selection::Actor(i));
            }
        }
        for (i, o) in state.scene.overlays.iter().enumerate() {
            let label = match o {
                Overlay::Text(t) => format!("[text] {} \"{}\"", t.id, ellipsis(&t.text, 24)),
                Overlay::Image(im) => format!("[image] {} {}", im.id, im.source.display()),
                Overlay::Video(v) => format!("[video] {} {}", v.id, v.source.display()),
            };
            let resp = ui.selectable_label(state.selection == Selection::Overlay(i), label);
            if resp.clicked() {
                to_select = Some(Selection::Overlay(i));
            }
        }
        if state.scene.actors.is_empty()
            && state.scene.overlays.is_empty()
            && state.scene.backgrounds.is_empty()
        {
            ui.label(
                RichText::new(
                    "Empty scene. Add a background, drop an actor from the library, \
                 then add a text overlay.",
                )
                .italics()
                .color(Color32::GRAY),
            );
        }
    });
    if let Some(sel) = to_select {
        state.selection = sel;
    }
}

pub fn preview(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.heading("Preview");
    ui.separator();
    let avail = ui.available_size_before_wrap();
    let target_aspect =
        state.scene.output.resolution[0] as f32 / state.scene.output.resolution[1] as f32;
    let mut h = avail.y.min(900.0);
    let mut w = h * target_aspect;
    if w > avail.x {
        w = avail.x;
        h = w / target_aspect;
    }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(20, 22, 28));

    if let Some(p) = &state.last_preview {
        let uri = format!("file://{}", p.display());
        ui.put(rect, egui::Image::from_uri(uri).fit_to_exact_size(rect.size()));
    } else {
        ui.put(
            rect,
            egui::Label::new(
                RichText::new(
                    "Preview placeholder.\nClick \"Render preview\" \
                     to ask FFmpeg for a still at the playhead.",
                )
                .color(Color32::LIGHT_GRAY),
            ),
        );
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
        out.push('…');
        out
    }
}

fn add_actor_from_clip(state: &mut EditorState, path: &PathBuf) {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("actor_{}", s))
        .unwrap_or_else(|| format!("actor_{}", state.scene.actors.len() + 1));
    let actor = Actor {
        id,
        source: path.clone(),
        anchors: None,
        chroma_key: ChromaKeyParams::default(),
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: None,
        t_out: None,
        source_start: 0.0,
        loop_source: false,
        flip_horizontal: false,
        attachments: Vec::new(),
    };
    state.scene.actors.push(actor);
    state.selection = Selection::Actor(state.scene.actors.len() - 1);
    state.status = format!("Added actor from {}", path.display());
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
        id,
        source,
        start: 0.0,
        duration: state.scene.output.duration,
        fit: Fit::Cover,
        transition: Transition::Cut,
    };
    state.scene.backgrounds.push(bg);
    state.selection = Selection::Background(state.scene.backgrounds.len() - 1);
    state.status = format!("Added background {}", path.display());
}
