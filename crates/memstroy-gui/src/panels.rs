//! UI panels — Premiere Pro-style timeline, modern inspector, drag&drop.

use std::path::PathBuf;

use egui::{Color32, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{AssetDragKind, EditorState, Selection, TrackKind};


// ─── COLORS ──────────────────────────────────────────────────────────

const COL_BG_DARK: Color32 = Color32::from_rgb(18, 18, 26);
const COL_BG_TRACK: Color32 = Color32::from_rgb(24, 24, 34);
const COL_BG_TRACK_ALT: Color32 = Color32::from_rgb(28, 28, 38);
const COL_RULER: Color32 = Color32::from_rgb(32, 32, 44);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 60, 60);
const COL_ACCENT: Color32 = Color32::from_rgb(100, 80, 220);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 160);
const COL_TEXT: Color32 = Color32::from_rgb(220, 220, 240);
const COL_CLIP_ACTOR: Color32 = Color32::from_rgb(220, 130, 50);
const COL_CLIP_BG: Color32 = Color32::from_rgb(60, 130, 220);
const COL_CLIP_OVERLAY: Color32 = Color32::from_rgb(80, 200, 120);
const COL_CLIP_AUDIO: Color32 = Color32::from_rgb(50, 180, 180);
const COL_SELECTED: Color32 = Color32::from_rgb(255, 220, 80);


// ─── LIBRARY ─────────────────────────────────────────────────────────

pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    ui.horizontal(|ui| {
        let clips_tab = ui.selectable_label(!state.assets_tab_active, "Clips");
        let assets_tab = ui.selectable_label(state.assets_tab_active, "Assets");
        if clips_tab.clicked() { state.assets_tab_active = false; }
        if assets_tab.clicked() { state.assets_tab_active = true; }
    });
    ui.separator();
    ui.add_space(4.0);

    if state.assets_tab_active {
        assets_panel(ui, state);
    } else {
        clips_panel(ui, state);
    }
}


fn clips_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Library").size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let btn = egui::Button::new(RichText::new("Refresh").color(Color32::WHITE).size(12.0))
                .fill(Color32::from_rgb(80, 50, 180))
                .rounding(Rounding::same(6.0));
            if ui.add_enabled(!state.refreshing, btn).clicked() {
                state.status = "__REFRESH_REQUESTED__".into();
            }
        });
    });
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut state.library_search)
            .hint_text("Search clips...")
            .desired_width(ui.available_width()),
    );
    ui.add_space(4.0);

    let clip_count = state.library.mellstroy_clips.len();
    let search_lower = state.library_search.to_lowercase();
    ui.label(RichText::new(format!("Clips ({})", clip_count)).size(12.0).color(COL_TEXT_DIM));

    if state.library.mellstroy_clips.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new("No clips. Hit Refresh to download.").italics().color(COL_TEXT_DIM).size(12.0));
    } else {
        egui::ScrollArea::vertical().max_height(ui.available_height() - 60.0).show(ui, |ui| {
            for idx in 0..state.library.mellstroy_clips.len() {
                let clip = &state.library.mellstroy_clips[idx];
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
    if !state.library.backgrounds.is_empty() {
        egui::CollapsingHeader::new(
            RichText::new(format!("Backgrounds ({})", state.library.backgrounds.len()))
                .size(12.0).color(Color32::from_rgb(100, 180, 255)),
        ).show(ui, |ui| {
            for p in state.library.backgrounds.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                let resp = ui.button(RichText::new(&name).size(11.0));
                // Drag support: start drag on press+move
                if resp.dragged() {
                    state.asset_drag.dragging = Some(p.clone());
                    state.asset_drag.kind = AssetDragKind::Background;
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        state.asset_drag.pos = [pos.x, pos.y];
                    }
                }
                if resp.clicked() {
                    add_background_from_path(state, &p);
                }
            }
        });
    }
}


fn assets_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.label(RichText::new("Props & Images").size(14.0).strong());
    ui.add_space(4.0);

    if state.library.props.is_empty() {
        ui.label(RichText::new("No props found.\nAdd PNG/WebP to assets/props/").italics().color(COL_TEXT_DIM).size(12.0));
    } else {
        egui::ScrollArea::vertical().show(ui, |ui| {
            for p in state.library.props.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                let resp = ui.horizontal(|ui| {
                    ui.label(RichText::new("\u{1F5BC}").size(14.0));
                    let r = ui.label(RichText::new(&name).size(11.0));
                    if ui.small_button("+").on_hover_text("Add as image overlay").clicked() {
                        add_image_overlay(state, &p);
                    }
                    r
                }).inner;
                // Drag support
                if resp.dragged() {
                    state.asset_drag.dragging = Some(p.clone());
                    state.asset_drag.kind = AssetDragKind::Prop;
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        state.asset_drag.pos = [pos.x, pos.y];
                    }
                }
            }
        });
    }

    ui.add_space(10.0);
    ui.label(RichText::new("Backgrounds").size(14.0).strong());
    ui.add_space(4.0);

    if state.library.backgrounds.is_empty() {
        ui.label(RichText::new("No backgrounds found.").italics().color(COL_TEXT_DIM).size(12.0));
    } else {
        for p in state.library.backgrounds.clone() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
            if ui.button(RichText::new(&name).size(11.0)).clicked() {
                add_background_from_path(state, &p);
            }
        }
    }
}


fn add_image_overlay(state: &mut EditorState, path: &PathBuf) {
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("img_{}", s))
        .unwrap_or_else(|| format!("img_{}", state.scene.overlays.len() + 1));
    let overlay = Overlay::Image(ImageOverlay {
        id: id.clone(),
        source: path.clone(),
        t_in: state.playhead,
        t_out: (state.playhead + 3.0).min(state.scene.output.duration),
        layout: vec![Keyframe::new(0.0, OverlayState { pos: [0.5, 0.5], scale: 0.3, rotation_deg: 0.0, opacity: 1.0 })],
    });
    state.scene.overlays.push(overlay);
    state.selection = Selection::Overlay(state.scene.overlays.len() - 1);
    state.status = format!("Added overlay: {}", id);
}

fn clip_card(ui: &mut egui::Ui, state: &mut EditorState, clip: &crate::state::LibraryClip) {
    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 32, 48))
        .rounding(Rounding::same(4.0))
        .inner_margin(egui::Margin::same(3.0))
        .stroke(Stroke::new(1.0, Color32::from_rgb(50, 50, 70)));

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
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
                let (rect, _) = ui.allocate_exact_size(thumb_size, Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(3.0), Color32::from_rgb(40, 40, 55));
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                    format!("{}", clip.id), egui::FontId::proportional(11.0), COL_TEXT_DIM);
            }

            ui.vertical(|ui| {
                let desc = clean_clip_text(&clip.description);
                let display = if desc.is_empty() { format!("Clip #{}", clip.id) }
                    else if desc.chars().count() > 35 { format!("{}...", desc.chars().take(32).collect::<String>()) }
                    else { desc };
                ui.label(RichText::new(format!("#{}", clip.id)).size(9.0).color(Color32::from_rgb(120, 100, 200)));
                ui.label(RichText::new(display).size(11.0).color(COL_TEXT));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add_btn = egui::Button::new(RichText::new("+").size(13.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(60, 160, 80))
                    .rounding(Rounding::same(8.0))
                    .min_size(Vec2::new(22.0, 22.0));
                if ui.add(add_btn).on_hover_text("Add to scene (or drag to timeline)").clicked() {
                    add_actor_from_clip(state, &clip.path);
                }
            });
        });
    });
    ui.add_space(2.0);
}


// ─── INSPECTOR ───────────────────────────────────────────────────────

pub fn inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Inspector").size(16.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if state.eyedropper_active {
                ui.label(RichText::new("PICK").size(10.0).color(Color32::from_rgb(255, 200, 50)));
            }
        });
    });
    ui.separator();
    ui.add_space(4.0);

    match state.selection {
        Selection::None => {
            inspector_nothing(ui, state);
        }
        Selection::Actor(i) => {
            if i < state.scene.actors.len() {
                inspector_actor(ui, state, i);
            }
        }
        Selection::Overlay(i) => {
            if i < state.scene.overlays.len() {
                inspector_overlay(ui, state, i);
            }
        }
        Selection::Background(i) => {
            if i < state.scene.backgrounds.len() {
                inspector_background(ui, state, i);
            }
        }
        Selection::Audio(i) => {
            if i < state.scene.audio.len() {
                inspector_audio(ui, state, i);
            }
        }
        Selection::Camera(_) => {
            ui.label("Camera editing coming soon.");
        }
    }
}


fn inspector_nothing(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.add_space(20.0);
    ui.label(RichText::new("Select a clip on the timeline").italics().color(COL_TEXT_DIM).size(13.0));
    ui.add_space(20.0);
    ui.separator();
    ui.add_space(8.0);

    // Output settings always visible when nothing selected
    ui.label(RichText::new("Output Settings").size(14.0).strong().color(Color32::from_rgb(100, 200, 255)));
    ui.add_space(4.0);
    let spec = &mut state.scene.output;
    ui.horizontal(|ui| {
        ui.label("Size:");
        ui.add(egui::DragValue::new(&mut spec.resolution[0]).range(64..=4096).speed(1.0));
        ui.label("x");
        ui.add(egui::DragValue::new(&mut spec.resolution[1]).range(64..=4096).speed(1.0));
    });
    ui.horizontal(|ui| {
        ui.label("FPS:");
        ui.add(egui::DragValue::new(&mut spec.fps).range(24..=120));
    });
    ui.horizontal(|ui| {
        ui.label("Duration:");
        ui.add(egui::DragValue::new(&mut spec.duration).range(0.5..=600.0).speed(0.1).suffix("s"));
    });
}


fn inspector_actor(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let actor_count = state.scene.actors.len();
    let cache_count = state.frame_caches.len();

    // Header with name and quick actions
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Actor: {}", state.scene.actors[i].id))
            .strong().size(14.0).color(COL_CLIP_ACTOR));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("\u{1F5D1}").on_hover_text("Delete").clicked() {
                state.status = "__DELETE_SELECTED__".into();
            }
            if ui.small_button("\u{1F4CB}").on_hover_text("Duplicate").clicked() {
                state.status = "__DUPLICATE_SELECTED__".into();
            }
        });
    });
    ui.add_space(2.0);
    ui.label(RichText::new(
        state.scene.actors[i].source.file_name().and_then(|s| s.to_str()).unwrap_or("(source)")
    ).size(10.0).color(COL_TEXT_DIM));
    ui.add_space(6.0);

    // Tab bar: Transform | Timing | Effects
    ui.horizontal(|ui| {
        if ui.selectable_label(state.inspector_tab == 0, "Transform").clicked() { state.inspector_tab = 0; }
        if ui.selectable_label(state.inspector_tab == 1, "Timing").clicked() { state.inspector_tab = 1; }
        if ui.selectable_label(state.inspector_tab == 2, "Effects").clicked() { state.inspector_tab = 2; }
    });
    ui.separator();
    ui.add_space(4.0);

    match state.inspector_tab {
        0 => inspector_actor_transform(ui, state, i),
        1 => inspector_actor_timing(ui, state, i),
        2 => inspector_actor_effects(ui, state, i, actor_count, cache_count),
        _ => {}
    }
}


fn inspector_actor_transform(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let a = &mut state.scene.actors[i];

    // Current keyframe (or interpolated) display
    ui.label(RichText::new("Position & Scale").size(12.0).strong());
    ui.add_space(2.0);

    // Show the keyframe closest to playhead or allow editing the interpolated state
    let playhead = state.playhead;
    let t_in = a.t_in.unwrap_or(0.0);
    let local_t = playhead - t_in;

    // Find or create nearest keyframe
    let kf_idx = a.layout.iter().position(|kf| (kf.t - local_t).abs() < 0.05);

    if let Some(ki) = kf_idx {
        let kf = &mut a.layout[ki];
        inspector_keyframe_editor(ui, kf);
    } else if !a.layout.is_empty() {
        // Show interpolated values (read-only preview)
        ui.label(RichText::new("(interpolated at current time)").size(10.0).color(COL_TEXT_DIM));
        if ui.button(RichText::new("+ Add Keyframe Here").size(11.0)).clicked() {
            let val = if a.layout.len() >= 2 {
                // Simple nearest value
                a.layout.last().map(|k| k.value).unwrap_or_default()
            } else {
                a.layout.first().map(|k| k.value).unwrap_or_default()
            };
            a.layout.push(Keyframe::new(local_t.max(0.0), val));
            a.layout.sort_by(|x, y| x.t.partial_cmp(&y.t).unwrap());
        }
        // Still show the values of the nearest keyframe for reference
        if let Some(kf) = a.layout.first_mut() {
            inspector_keyframe_editor(ui, kf);
        }
    } else {
        ui.label(RichText::new("No keyframes").color(COL_TEXT_DIM).size(11.0));
        if ui.button("+ Add Keyframe").clicked() {
            a.layout.push(Keyframe::new(0.0, ActorState::default()));
        }
    }

    ui.add_space(8.0);

    // Keyframe list (compact)
    ui.label(RichText::new("All Keyframes").size(12.0).strong());
    ui.add_space(2.0);

    let mut to_remove = None;
    egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
        for (ki, kf) in a.layout.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{:.2}s", kf.t)).size(10.0).color(COL_ACCENT));
                ui.label(RichText::new(format!("({:.0}%, {:.0}%)", kf.value.pos[0]*100.0, kf.value.pos[1]*100.0)).size(10.0).color(COL_TEXT_DIM));
                ui.label(RichText::new(format!("s:{:.2}", kf.value.scale)).size(10.0).color(COL_TEXT_DIM));
                if ui.small_button("x").clicked() { to_remove = Some(ki); }
            });
        }
    });
    if let Some(ri) = to_remove {
        state.scene.actors[i].layout.remove(ri);
    }
}


fn inspector_keyframe_editor(ui: &mut egui::Ui, kf: &mut Keyframe<ActorState>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Time:").size(11.0));
        ui.add(egui::DragValue::new(&mut kf.t).range(0.0..=600.0).speed(0.02).suffix("s"));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("X:").size(11.0));
        ui.add(egui::DragValue::new(&mut kf.value.pos[0]).range(-2.0..=3.0).speed(0.005));
        ui.label(RichText::new("Y:").size(11.0));
        ui.add(egui::DragValue::new(&mut kf.value.pos[1]).range(-2.0..=3.0).speed(0.005));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Scale:").size(11.0));
        ui.add(egui::Slider::new(&mut kf.value.scale, 0.05..=5.0).logarithmic(true));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Rotation:").size(11.0));
        ui.add(egui::Slider::new(&mut kf.value.rotation_deg, -360.0..=360.0).suffix("\u{00B0}"));
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Opacity:").size(11.0));
        ui.add(egui::Slider::new(&mut kf.value.opacity, 0.0..=1.0));
    });
}

fn inspector_actor_timing(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let duration = state.scene.output.duration;
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Clip Timing").size(12.0).strong());
    ui.add_space(4.0);

    let mut t_in = a.t_in.unwrap_or(0.0);
    let mut t_out = a.t_out.unwrap_or(duration);

    ui.horizontal(|ui| {
        ui.label("In:");
        ui.add(egui::DragValue::new(&mut t_in).range(0.0..=duration).speed(0.02).suffix("s"));
        ui.label("Out:");
        ui.add(egui::DragValue::new(&mut t_out).range(0.0..=duration).speed(0.02).suffix("s"));
    });
    a.t_in = Some(t_in);
    a.t_out = Some(t_out);

    ui.horizontal(|ui| {
        ui.label("Duration:");
        ui.label(RichText::new(format!("{:.2}s", t_out - t_in)).color(COL_ACCENT));
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label("Source offset:");
        ui.add(egui::DragValue::new(&mut a.source_start).range(0.0..=300.0).speed(0.02).suffix("s"));
    });

    ui.add_space(4.0);
    ui.checkbox(&mut a.loop_source, "Loop source clip");
    ui.checkbox(&mut a.flip_horizontal, "Flip horizontal");
    ui.checkbox(&mut a.visible, "Visible");

    ui.add_space(8.0);

    // Layer order
    let actor_count = state.scene.actors.len();
    ui.label(RichText::new("Layer Order").size(12.0).strong());
    ui.horizontal(|ui| {
        if i > 0 && ui.button("\u{2191} Up").clicked() {
            state.scene.actors.swap(i, i - 1);
            let cc = state.frame_caches.len();
            if cc > i { state.frame_caches.swap(i, i - 1); }
            state.selection = Selection::Actor(i - 1);
        }
        if i + 1 < actor_count && ui.button("\u{2193} Down").clicked() {
            state.scene.actors.swap(i, i + 1);
            let cc = state.frame_caches.len();
            if cc > i + 1 { state.frame_caches.swap(i, i + 1); }
            state.selection = Selection::Actor(i + 1);
        }
    });
}


fn inspector_actor_effects(ui: &mut egui::Ui, state: &mut EditorState, i: usize, _actor_count: usize, _cache_count: usize) {
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Chroma Key").size(12.0).strong().color(Color32::from_rgb(100, 255, 100)));
    ui.add_space(4.0);

    // Eyedropper
    ui.horizontal(|ui| {
        if state.eyedropper_active {
            ui.label(RichText::new("Click preview to pick color...").color(Color32::from_rgb(255, 200, 50)).size(11.0));
        } else if ui.button("Eyedropper").on_hover_text("Pick color from preview").clicked() {
            state.eyedropper_active = true;
        }
        ui.label("Key:");
        color_edit_u8(ui, &mut a.chroma_key.key_color);
    });

    ui.add_space(4.0);
    ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text("Similarity"));
    ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text("Blend"));
    ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text("Spill"));
}

fn inspector_overlay(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let duration = state.scene.output.duration;
    let ov = &mut state.scene.overlays[i];

    match ov {
        Overlay::Text(t) => {
            ui.label(RichText::new(format!("Text: {}", t.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.text_edit_multiline(&mut t.text);
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("In:");
                ui.add(egui::DragValue::new(&mut t.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label("Out:");
                ui.add(egui::DragValue::new(&mut t.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });
            ui.add(egui::Slider::new(&mut t.style.font_size, 16.0..=512.0).text("Font Size"));
            ui.horizontal(|ui| { ui.label("Color:"); color_edit_u8(ui, &mut t.style.color); });
            let mut has_box = t.style.box_color.is_some();
            ui.checkbox(&mut has_box, "Background plate");
            if has_box && t.style.box_color.is_none() { t.style.box_color = Some([255, 255, 255]); }
            if !has_box { t.style.box_color = None; }
        }
        Overlay::Image(im) => {
            ui.label(RichText::new(format!("Image: {}", im.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("In:");
                ui.add(egui::DragValue::new(&mut im.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label("Out:");
                ui.add(egui::DragValue::new(&mut im.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });
            if let Some(kf) = im.layout.first_mut() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("X:"); ui.add(egui::DragValue::new(&mut kf.value.pos[0]).speed(0.005));
                    ui.label("Y:"); ui.add(egui::DragValue::new(&mut kf.value.pos[1]).speed(0.005));
                });
                ui.add(egui::Slider::new(&mut kf.value.scale, 0.05..=5.0).text("Scale").logarithmic(true));
                ui.add(egui::Slider::new(&mut kf.value.opacity, 0.0..=1.0).text("Opacity"));
            }
        }
        Overlay::Video(v) => {
            ui.label(RichText::new(format!("Video: {}", v.id)).strong().size(14.0).color(COL_CLIP_OVERLAY));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("In:");
                ui.add(egui::DragValue::new(&mut v.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
                ui.label("Out:");
                ui.add(egui::DragValue::new(&mut v.t_out).range(0.0..=duration).speed(0.02).suffix("s"));
            });
        }
    }
}


fn inspector_background(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let b = &mut state.scene.backgrounds[i];
    ui.label(RichText::new(format!("Background: {}", b.id)).strong().size(14.0).color(COL_CLIP_BG));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Start:");
        ui.add(egui::DragValue::new(&mut b.start).speed(0.02).suffix("s"));
        ui.label("Duration:");
        ui.add(egui::DragValue::new(&mut b.duration).speed(0.02).suffix("s"));
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

fn inspector_audio(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let duration = state.scene.output.duration;
    let audio = &mut state.scene.audio[i];
    ui.label(RichText::new(format!("Audio: {}", audio.id)).strong().size(14.0).color(COL_CLIP_AUDIO));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("In:");
        ui.add(egui::DragValue::new(&mut audio.t_in).range(0.0..=duration).speed(0.02).suffix("s"));
        let mut t_out = audio.t_out.unwrap_or(duration);
        ui.label("Out:");
        if ui.add(egui::DragValue::new(&mut t_out).range(0.0..=duration).speed(0.02).suffix("s")).changed() {
            audio.t_out = Some(t_out);
        }
    });
    ui.add_space(4.0);
    ui.add(egui::Slider::new(&mut audio.volume, 0.0..=2.0).text("Volume"));
    ui.horizontal(|ui| {
        ui.label("Source offset:");
        ui.add(egui::DragValue::new(&mut audio.source_start).range(0.0..=600.0).speed(0.02).suffix("s"));
    });
}


// ─── TIMELINE ────────────────────────────────────────────────────────

pub fn timeline(ui: &mut egui::Ui, state: &mut EditorState) {
    // ── Toolbar ──
    ui.horizontal(|ui| {
        // Play/Pause
        let play_label = if state.playing { "\u{23F8}" } else { "\u{25B6}" };
        let play_btn = egui::Button::new(RichText::new(play_label).size(16.0).color(Color32::WHITE))
            .fill(if state.playing { Color32::from_rgb(200, 60, 60) } else { Color32::from_rgb(50, 170, 70) })
            .rounding(Rounding::same(6.0)).min_size(Vec2::new(32.0, 24.0));
        if ui.add(play_btn).clicked() { state.playing = !state.playing; }

        ui.add(egui::DragValue::new(&mut state.playback_speed).range(0.1..=8.0).speed(0.05).prefix("x"));

        ui.separator();

        // Time display
        let duration = state.scene.output.duration;
        ui.label(RichText::new(format_time(state.playhead)).size(13.0).strong().color(COL_TEXT));
        ui.label(RichText::new(format!("/ {}", format_time(duration))).size(11.0).color(COL_TEXT_DIM));

        ui.separator();

        // Undo/Redo
        if ui.add_enabled(state.undo.can_undo(), egui::Button::new("\u{21A9}").min_size(Vec2::new(24.0, 20.0))).clicked() { state.undo(); }
        if ui.add_enabled(state.undo.can_redo(), egui::Button::new("\u{21AA}").min_size(Vec2::new(24.0, 20.0))).clicked() { state.redo(); }

        ui.separator();

        // Tools
        if ui.button("\u{1F5D1}").on_hover_text("Delete (Del)").clicked() { state.status = "__DELETE_SELECTED__".into(); }
        if ui.button("\u{2702}").on_hover_text("Split at playhead").clicked() { state.status = "__SPLIT_AT_PLAYHEAD__".into(); }

        let razor_color = if state.razor_mode { Color32::from_rgb(255, 80, 80) } else { COL_TEXT };
        if ui.button(RichText::new("\u{1FA92}").color(razor_color)).on_hover_text("Razor tool").clicked() {
            state.razor_mode = !state.razor_mode;
        }

        ui.separator();

        // Snap toggle
        let snap_color = if state.snap_enabled { COL_ACCENT } else { COL_TEXT_DIM };
        if ui.button(RichText::new("S").color(snap_color)).on_hover_text("Snap to grid/edges").clicked() {
            state.snap_enabled = !state.snap_enabled;
        }

        // Zoom controls
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                state.timeline_zoom = (state.timeline_zoom * 1.3).min(500.0);
            }
            if ui.button("-").on_hover_text("Zoom out").clicked() {
                state.timeline_zoom = (state.timeline_zoom / 1.3).max(20.0);
            }
            ui.label(RichText::new(format!("{:.0}px/s", state.timeline_zoom)).size(10.0).color(COL_TEXT_DIM));
        });
    });
    ui.add_space(2.0);

    // ── Track area: header column + scrollable tracks ──
    let available = ui.available_size();
    let header_width = 80.0_f32;
    let track_area_width = (available.x - header_width - 4.0).max(100.0);
    let duration = state.scene.output.duration.max(0.01);
    let pps = state.timeline_zoom; // pixels per second

    // Handle zoom via scroll wheel on timeline area
    let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
    if scroll_delta.y.abs() > 0.1 && ui.rect_contains_pointer(ui.max_rect()) {
        if ui.input(|i| i.modifiers.ctrl) {
            // Ctrl+scroll = zoom
            let factor = if scroll_delta.y > 0.0 { 1.08 } else { 1.0 / 1.08 };
            state.timeline_zoom = (state.timeline_zoom * factor).clamp(20.0, 500.0);
        } else {
            // Scroll = horizontal pan
            state.timeline_scroll = (state.timeline_scroll - scroll_delta.y / pps).max(0.0);
        }
    }

    // ── Ruler ──
    let ruler_height = 22.0;
    let (ruler_rect, ruler_resp) = ui.allocate_exact_size(
        Vec2::new(header_width + track_area_width + 4.0, ruler_height), Sense::click_and_drag());
    let painter = ui.painter_at(ruler_rect);

    let track_left = ruler_rect.min.x + header_width + 2.0;
    let track_right = track_left + track_area_width;
    let ruler_track_rect = egui::Rect::from_min_max(
        egui::pos2(track_left, ruler_rect.min.y),
        egui::pos2(track_right, ruler_rect.max.y),
    );
    painter.rect_filled(ruler_track_rect, Rounding::ZERO, COL_RULER);

    // Time markers on ruler
    draw_ruler_marks(&painter, ruler_track_rect, state.timeline_scroll, pps, duration);

    // Playhead on ruler
    let ph_x = time_to_x(state.playhead, state.timeline_scroll, pps, track_left, track_right);
    if let Some(x) = ph_x {
        let tri = 5.0;
        painter.add(egui::Shape::convex_polygon(
            vec![egui::pos2(x - tri, ruler_rect.min.y), egui::pos2(x + tri, ruler_rect.min.y), egui::pos2(x, ruler_rect.min.y + tri * 1.5)],
            COL_PLAYHEAD, Stroke::NONE));
        painter.line_segment([egui::pos2(x, ruler_rect.min.y + tri * 1.5), egui::pos2(x, ruler_rect.max.y)],
            Stroke::new(1.5, COL_PLAYHEAD));
    }

    // Click ruler to seek
    if ruler_resp.clicked() || ruler_resp.dragged() {
        if let Some(pos) = ruler_resp.interact_pointer_pos() {
            if pos.x >= track_left && pos.x <= track_right {
                state.playhead = x_to_time(pos.x, state.timeline_scroll, pps, track_left).clamp(0.0, duration);
            }
        }
    }


    // ── Track rows ──
    let mut to_select: Option<Selection> = None;

    egui::ScrollArea::vertical().show(ui, |ui| {
        let num_tracks = state.tracks.len();
        for track_idx in 0..num_tracks {
            let track = &state.tracks[track_idx];
            let track_h = track.height;
            let track_kind = track.kind;
            let track_name = track.name.clone();
            let track_muted = track.muted;
            let track_locked = track.locked;

            let (row_rect, _) = ui.allocate_exact_size(
                Vec2::new(header_width + track_area_width + 4.0, track_h), Sense::hover());
            let painter = ui.painter_at(row_rect);

            // Track background (alternating)
            let bg = if track_idx % 2 == 0 { COL_BG_TRACK } else { COL_BG_TRACK_ALT };
            painter.rect_filled(row_rect, Rounding::ZERO, bg);

            // Header area
            let hdr_rect = egui::Rect::from_min_size(row_rect.min, Vec2::new(header_width, track_h));
            painter.rect_filled(hdr_rect, Rounding::ZERO, Color32::from_rgb(30, 30, 42));
            painter.text(hdr_rect.center(), egui::Align2::CENTER_CENTER,
                &track_name, egui::FontId::proportional(11.0),
                if track_muted { COL_TEXT_DIM } else { COL_TEXT });

            // Track content area
            let content_left = row_rect.min.x + header_width + 2.0;
            let content_rect = egui::Rect::from_min_max(
                egui::pos2(content_left, row_rect.min.y + 1.0),
                egui::pos2(row_rect.max.x, row_rect.max.y - 1.0));

            // Draw clips on this track
            match track_kind {
                TrackKind::Video => {
                    // Draw backgrounds on track 0
                    if track_idx == 0 {
                        for bi in 0..state.scene.backgrounds.len() {
                            let bg_elem = &state.scene.backgrounds[bi];
                            let clip_start = bg_elem.start;
                            let clip_end = bg_elem.start + bg_elem.duration;
                            let sel = state.selection == Selection::Background(bi);
                            if let Some(clicked) = draw_clip(ui, &painter, content_rect, &bg_elem.id,
                                clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                                COL_CLIP_BG, sel, track_h, track_locked, state.razor_mode)
                            {
                                if clicked < 0.0 {
                                    let new_start = (-clicked).max(0.0);
                                    let dur = clip_end - clip_start;
                                    state.scene.backgrounds[bi].start = new_start;
                                    state.scene.backgrounds[bi].duration = dur;
                                    to_select = Some(Selection::Background(bi));
                                } else if state.razor_mode {
                                    to_select = Some(Selection::Background(bi));
                                    state.playhead = clicked;
                                    state.status = "__SPLIT_AT_PLAYHEAD__".into();
                                } else {
                                    to_select = Some(Selection::Background(bi));
                                }
                            }
                        }
                    }

                    // Draw actors assigned to this track (by index mod video tracks)
                    let video_tracks: Vec<usize> = (0..num_tracks).filter(|ti| state.tracks[*ti].kind == TrackKind::Video).collect();
                    let vt_pos = video_tracks.iter().position(|t| *t == track_idx);

                    for ai in 0..state.scene.actors.len() {
                        // Assign actor to track by round-robin across video tracks
                        let target_vt = if video_tracks.is_empty() { 0 } else { ai % video_tracks.len() };
                        if vt_pos != Some(target_vt) { continue; }

                        let actor = &state.scene.actors[ai];
                        let clip_start = actor.t_in.unwrap_or(0.0);
                        let clip_end = actor.t_out.unwrap_or(duration);
                        let sel = state.selection == Selection::Actor(ai);
                        if let Some(clicked) = draw_clip(ui, &painter, content_rect, &actor.id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            COL_CLIP_ACTOR, sel, track_h, track_locked, state.razor_mode)
                        {
                            if clicked < 0.0 {
                                // Drag: move the actor's time window
                                let new_start = (-clicked).max(0.0);
                                let dur = clip_end - clip_start;
                                state.scene.actors[ai].t_in = Some(new_start);
                                state.scene.actors[ai].t_out = Some(new_start + dur);
                                to_select = Some(Selection::Actor(ai));
                            } else if state.razor_mode {
                                to_select = Some(Selection::Actor(ai));
                                state.playhead = clicked;
                                state.status = "__SPLIT_AT_PLAYHEAD__".into();
                            } else {
                                to_select = Some(Selection::Actor(ai));
                            }
                        }
                    }

                    // Draw overlays on video tracks (track 1+)
                    if vt_pos.unwrap_or(0) >= 1 || video_tracks.len() <= 1 {
                        for oi in 0..state.scene.overlays.len() {
                            let target_vt = if video_tracks.len() >= 2 { 1 } else { 0 };
                            if vt_pos != Some(target_vt) && video_tracks.len() > 1 { continue; }

                            let ov = &state.scene.overlays[oi];
                            let (clip_start, clip_end, label) = match ov {
                                Overlay::Text(t) => (t.t_in, t.t_out, format!("T: {}", ellipsis(&t.text, 10))),
                                Overlay::Image(im) => (im.t_in, im.t_out, format!("I: {}", im.id)),
                                Overlay::Video(v) => (v.t_in, v.t_out, format!("V: {}", v.id)),
                            };
                            let sel = state.selection == Selection::Overlay(oi);
                            if let Some(clicked) = draw_clip(ui, &painter, content_rect, &label,
                                clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                                COL_CLIP_OVERLAY, sel, track_h, track_locked, state.razor_mode)
                            {
                                if clicked < 0.0 {
                                    let new_start = (-clicked).max(0.0);
                                    let dur = clip_end - clip_start;
                                    let new_end = new_start + dur;
                                    match &mut state.scene.overlays[oi] {
                                        Overlay::Text(t) => { t.t_in = new_start; t.t_out = new_end; }
                                        Overlay::Image(im) => { im.t_in = new_start; im.t_out = new_end; }
                                        Overlay::Video(v) => { v.t_in = new_start; v.t_out = new_end; }
                                    }
                                    to_select = Some(Selection::Overlay(oi));
                                } else if state.razor_mode {
                                    to_select = Some(Selection::Overlay(oi));
                                    state.playhead = clicked;
                                    state.status = "__SPLIT_AT_PLAYHEAD__".into();
                                } else {
                                    to_select = Some(Selection::Overlay(oi));
                                }
                            }
                        }
                    }
                }
                TrackKind::Audio => {
                    let audio_tracks: Vec<usize> = (0..num_tracks).filter(|ti| state.tracks[*ti].kind == TrackKind::Audio).collect();
                    let at_pos = audio_tracks.iter().position(|t| *t == track_idx);

                    for aui in 0..state.scene.audio.len() {
                        let target_at = if audio_tracks.is_empty() { 0 } else { aui % audio_tracks.len() };
                        if at_pos != Some(target_at) { continue; }

                        let audio = &state.scene.audio[aui];
                        let clip_start = audio.t_in;
                        let clip_end = audio.t_out.unwrap_or(duration);
                        let sel = state.selection == Selection::Audio(aui);
                        if let Some(clicked) = draw_audio_clip(ui, &painter, content_rect, &audio.id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            sel, track_h, track_locked, state.razor_mode,
                            state.audio_waveforms.get(aui))
                        {
                            to_select = Some(Selection::Audio(aui));
                        }
                    }
                }
            }

            // Playhead line on each track
            let ph_x = time_to_x(state.playhead, state.timeline_scroll, pps, track_left, track_right);
            if let Some(x) = ph_x {
                painter.line_segment(
                    [egui::pos2(x, row_rect.min.y), egui::pos2(x, row_rect.max.y)],
                    Stroke::new(1.0, COL_PLAYHEAD));
            }
        }

        // Empty state
        if state.scene.actors.is_empty() && state.scene.overlays.is_empty()
            && state.scene.backgrounds.is_empty() && state.scene.audio.is_empty() {
            ui.add_space(20.0);
            ui.label(RichText::new("Drag clips from the library or click + to add them here")
                .italics().color(COL_TEXT_DIM).size(12.0));
        }
    });

    if let Some(sel) = to_select {
        state.selection = sel;
    }
}


/// Draw a single clip bar on the timeline. Returns Some(time) if clicked (for razor or select).
/// Returns special sentinel values for edge-trim drags:
/// - `f32::INFINITY` signals "trim left edge"
/// - `f32::NEG_INFINITY` signals "trim right edge"
/// - Negative values signal whole-clip drag (new start time encoded as `-new_start`)
/// Shows ResizeHorizontal cursor when hovering within 5px of left/right edge.
#[allow(clippy::too_many_arguments)]
fn draw_clip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    content_rect: egui::Rect,
    label: &str,
    clip_start: f32,
    clip_end: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    color: Color32,
    selected: bool,
    _track_h: f32,
    locked: bool,
    razor_mode: bool,
) -> Option<f32> {
    let x_start = (clip_start - scroll) * pps + track_left;
    let x_end = (clip_end - scroll) * pps + track_left;

    // Clip is off-screen
    if x_end < track_left || x_start > track_right { return None; }

    let x_start = x_start.max(track_left);
    let x_end = x_end.min(track_right);

    if x_end - x_start < 2.0 { return None; }

    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(x_start, content_rect.min.y + 2.0),
        egui::pos2(x_end, content_rect.max.y - 2.0),
    );

    // Fill
    let fill = if selected {
        Color32::from_rgb(color.r().saturating_add(30), color.g().saturating_add(30), color.b().saturating_add(30))
    } else { color };
    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Selection border
    if selected {
        painter.rect_stroke(bar_rect.expand(1.0), Rounding::same(5.0), Stroke::new(2.0, COL_SELECTED));
    }

    // Label inside
    if bar_rect.width() > 30.0 {
        let text = if bar_rect.width() > 80.0 { label.to_string() } else { ellipsis(label, 6) };
        painter.text(
            egui::pos2(bar_rect.min.x + 6.0, bar_rect.center().y),
            egui::Align2::LEFT_CENTER, &text,
            egui::FontId::proportional(10.0), Color32::WHITE);
    }

    // Interaction (click/drag with edge-trim zones)
    let id = ui.make_persistent_id(("clip", label, clip_start as u32));
    let sense = if locked { Sense::hover() } else { Sense::click_and_drag() };
    let resp = ui.interact(bar_rect, id, sense);

    // Edge-trim zone detection (5px from left/right edge)
    const TRIM_ZONE_PX: f32 = 5.0;
    let hover_pos = ui.input(|i| i.pointer.hover_pos());
    let near_left_edge = hover_pos
        .map(|p| p.x >= bar_rect.min.x && p.x <= bar_rect.min.x + TRIM_ZONE_PX && bar_rect.y_range().contains(p.y))
        .unwrap_or(false);
    let near_right_edge = hover_pos
        .map(|p| p.x >= bar_rect.max.x - TRIM_ZONE_PX && p.x <= bar_rect.max.x && bar_rect.y_range().contains(p.y))
        .unwrap_or(false);

    if resp.hovered() && !locked {
        if razor_mode {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if near_left_edge || near_right_edge {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    if resp.clicked() {
        if razor_mode {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = x_to_time(pos.x, scroll, pps, track_left);
                return Some(t);
            }
        }
        return Some(clip_start); // signal selection
    }

    // Drag handling: edge-trim vs whole-clip move
    if resp.dragged() && !locked && !razor_mode {
        let dx = resp.drag_delta().x;
        let delta_secs = dx / pps;

        // Determine drag origin position from the initial press
        let drag_origin = resp.interact_pointer_pos().unwrap_or_default();
        let started_near_left = drag_origin.x <= bar_rect.min.x + TRIM_ZONE_PX;
        let started_near_right = drag_origin.x >= bar_rect.max.x - TRIM_ZONE_PX;

        if started_near_left && delta_secs.abs() > 0.001 {
            // Trim left edge: encode as f32::INFINITY with delta stored in sign
            // Convention: INFINITY signals trim-left, actual delta comes from drag
            return Some(f32::INFINITY);
        } else if started_near_right && delta_secs.abs() > 0.001 {
            // Trim right edge: encode as NEG_INFINITY signals trim-right
            return Some(f32::NEG_INFINITY);
        } else if delta_secs.abs() > 0.001 {
            // Normal move: return the NEW start time as negative
            return Some(-(clip_start + delta_secs));
        }
        return Some(clip_start); // just select if no movement
    }

    None
}


/// Draw an audio clip with waveform visualization.
#[allow(clippy::too_many_arguments)]
fn draw_audio_clip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    content_rect: egui::Rect,
    label: &str,
    clip_start: f32,
    clip_end: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    selected: bool,
    _track_h: f32,
    locked: bool,
    _razor_mode: bool,
    waveform: Option<&crate::state::AudioWaveform>,
) -> Option<f32> {
    let x_start = (clip_start - scroll) * pps + track_left;
    let x_end = (clip_end - scroll) * pps + track_left;

    if x_end < track_left || x_start > track_right { return None; }

    let x_start = x_start.max(track_left);
    let x_end = x_end.min(track_right);
    if x_end - x_start < 2.0 { return None; }

    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(x_start, content_rect.min.y + 2.0),
        egui::pos2(x_end, content_rect.max.y - 2.0),
    );

    // Fill
    let fill = if selected { Color32::from_rgb(70, 200, 200) } else { COL_CLIP_AUDIO };
    painter.rect_filled(bar_rect, Rounding::same(4.0), fill);

    // Draw waveform
    if let Some(wf) = waveform {
        if wf.ready && !wf.peaks.is_empty() {
            let bar_w = bar_rect.width();
            let bar_h = bar_rect.height();
            let center_y = bar_rect.center().y;
            let num_samples = (bar_w as usize).min(wf.peaks.len());

            if num_samples > 1 {
                let step = wf.peaks.len() as f32 / num_samples as f32;
                for i in 0..num_samples {
                    let sample_idx = (i as f32 * step) as usize;
                    let peak = wf.peaks.get(sample_idx).copied().unwrap_or(0.0);
                    let h = peak * bar_h * 0.4;
                    let x = bar_rect.min.x + (i as f32 / num_samples as f32) * bar_w;
                    painter.line_segment(
                        [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
                        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 255, 255, 120)));
                }
            }
        }
    }

    // Selection border
    if selected {
        painter.rect_stroke(bar_rect.expand(1.0), Rounding::same(5.0), Stroke::new(2.0, COL_SELECTED));
    }

    // Label
    if bar_rect.width() > 40.0 {
        painter.text(
            egui::pos2(bar_rect.min.x + 4.0, bar_rect.min.y + 4.0),
            egui::Align2::LEFT_TOP, label,
            egui::FontId::proportional(9.0), Color32::WHITE);
    }

    // Interaction
    let id = ui.make_persistent_id(("audio_clip", label, clip_start as u32));
    let resp = ui.interact(bar_rect, id, if locked { Sense::hover() } else { Sense::click() });
    if resp.clicked() { return Some(clip_start); }

    None
}


/// Draw ruler time marks with proper spacing.
fn draw_ruler_marks(painter: &egui::Painter, rect: egui::Rect, scroll: f32, pps: f32, duration: f32) {
    // Choose step based on zoom level
    let step = choose_ruler_step_pps(pps);
    let start_t = (scroll / step).floor() * step;
    let end_t = scroll + rect.width() / pps;

    let mut t = start_t;
    while t <= end_t.min(duration) {
        let x = rect.min.x + (t - scroll) * pps;
        if x >= rect.min.x && x <= rect.max.x {
            let is_major = (t / step).round() as i32 % 5 == 0 || step >= duration;
            let tick_h = if is_major { rect.height() * 0.7 } else { rect.height() * 0.35 };
            painter.line_segment(
                [egui::pos2(x, rect.max.y - tick_h), egui::pos2(x, rect.max.y)],
                Stroke::new(1.0, Color32::from_rgb(80, 80, 100)));
            if is_major {
                painter.text(egui::pos2(x, rect.min.y + 2.0), egui::Align2::CENTER_TOP,
                    format_time(t), egui::FontId::proportional(9.0), COL_TEXT_DIM);
            }
        }
        t += step;
    }
}

/// Convert time to X pixel position. Returns None if off-screen.
fn time_to_x(t: f32, scroll: f32, pps: f32, track_left: f32, track_right: f32) -> Option<f32> {
    let x = track_left + (t - scroll) * pps;
    if x >= track_left && x <= track_right { Some(x) } else { None }
}

/// Convert X pixel position back to time.
fn x_to_time(x: f32, scroll: f32, pps: f32, track_left: f32) -> f32 {
    scroll + (x - track_left) / pps
}

/// Choose ruler step based on pixels-per-second zoom.
fn choose_ruler_step_pps(pps: f32) -> f32 {
    // Target ~60-100px between major marks (every 5 steps)
    let target_px = 80.0;
    let step_secs = target_px / pps / 5.0;
    // Round to nice values
    if step_secs < 0.02 { 0.01 }
    else if step_secs < 0.05 { 0.02 }
    else if step_secs < 0.1 { 0.05 }
    else if step_secs < 0.2 { 0.1 }
    else if step_secs < 0.5 { 0.2 }
    else if step_secs < 1.0 { 0.5 }
    else if step_secs < 2.0 { 1.0 }
    else if step_secs < 5.0 { 2.0 }
    else if step_secs < 10.0 { 5.0 }
    else { 10.0 }
}

fn format_time(t: f32) -> String {
    let mins = (t / 60.0).floor() as u32;
    let secs = t % 60.0;
    if mins > 0 { format!("{}:{:05.2}", mins, secs) }
    else { format!("{:.2}s", secs) }
}


// ─── PREVIEW ─────────────────────────────────────────────────────────

pub fn preview(ui: &mut egui::Ui, state: &mut EditorState) {
    let avail = ui.available_size_before_wrap();
    let target_aspect = state.scene.output.resolution[0] as f32 / state.scene.output.resolution[1] as f32;
    let mut h = avail.y.min(800.0);
    let mut w = h * target_aspect;
    if w > avail.x { w = avail.x; h = w / target_aspect; }

    let offset_x = (avail.x - w) * 0.5;
    let offset_y = (avail.y - h) * 0.5;

    // Preview needs click+drag for gizmo manipulation
    let (full_rect, preview_resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
    let rect = egui::Rect::from_min_size(
        egui::pos2(full_rect.min.x + offset_x, full_rect.min.y + offset_y), Vec2::new(w, h));

    ui.painter().rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(10, 10, 16));

    // Render ALL visible actors (composited bottom-to-top) using frame_at_time for texture reuse
    let t = state.playhead;
    let mut any_frame_shown = false;

    // Collect actor info to avoid borrow conflicts
    let actor_data: Vec<_> = state.scene.actors.iter().enumerate().map(|(idx, actor)| {
        (idx, actor.visible, actor.t_in.unwrap_or(0.0), actor.t_out.unwrap_or(f32::MAX),
         actor.source_start, actor.chroma_key.clone(),
         actor.layout.first().map(|kf| kf.value).unwrap_or_default())
    }).collect();

    for (actor_idx, visible, t_in, t_out, source_start, ref chroma_key, actor_state) in actor_data.iter() {
        if !visible { continue; }
        if t < *t_in || t > *t_out { continue; }

        let local_t = t - t_in + source_start;

        if let Some(fc) = state.frame_caches.get_mut(*actor_idx) {
            if fc.is_ready() {
                // Use frame_at_time which reuses a single TextureHandle per actor (no allocation per frame)
                if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                    // Compute actor rect based on layout state (position/scale)
                    let ax = actor_state.pos[0];
                    let ay = actor_state.pos[1];
                    let ascale = actor_state.scale;
                    let rotation_rad = actor_state.rotation_deg.to_radians();

                    // Actor rect centered at (ax, ay) in normalized coords, scaled
                    let actor_w = rect.width() * ascale * 0.5;
                    let actor_h = rect.height() * ascale * 0.5;
                    let cx = rect.min.x + ax * rect.width();
                    let cy = rect.min.y + ay * rect.height();

                    let tint = Color32::from_rgba_unmultiplied(255, 255, 255, (actor_state.opacity * 255.0) as u8);

                    if rotation_rad.abs() > 0.001 {
                        // Draw rotated: compute rotated corner positions
                        let cos_r = rotation_rad.cos();
                        let sin_r = rotation_rad.sin();
                        let hw = actor_w * 0.5;
                        let hh = actor_h * 0.5;

                        // Corners relative to center (top-left, top-right, bottom-right, bottom-left)
                        let corners_local = [
                            [-hw, -hh],
                            [ hw, -hh],
                            [ hw,  hh],
                            [-hw,  hh],
                        ];

                        // Rotate each corner and translate to screen position
                        let rotated_positions: Vec<egui::Pos2> = corners_local.iter().map(|[lx, ly]| {
                            let rx = lx * cos_r - ly * sin_r + cx;
                            let ry = lx * sin_r + ly * cos_r + cy;
                            egui::pos2(rx, ry)
                        }).collect();

                        // UV corners matching position corners (TL, TR, BR, BL)
                        let uv_corners = [
                            egui::pos2(0.0, 0.0),
                            egui::pos2(1.0, 0.0),
                            egui::pos2(1.0, 1.0),
                            egui::pos2(0.0, 1.0),
                        ];

                        // Draw as two textured triangles via mesh
                        let mut mesh = egui::Mesh::with_texture(tex.id());
                        for i in 0..4 {
                            mesh.vertices.push(egui::epaint::Vertex {
                                pos: rotated_positions[i],
                                uv: uv_corners[i],
                                color: tint,
                            });
                        }
                        // Triangle 1: TL, TR, BR
                        mesh.indices.extend_from_slice(&[0, 1, 2]);
                        // Triangle 2: TL, BR, BL
                        mesh.indices.extend_from_slice(&[0, 2, 3]);

                        ui.painter().add(egui::Shape::mesh(mesh));
                    } else {
                        // No rotation: draw axis-aligned image (fast path)
                        let actor_rect = egui::Rect::from_center_size(
                            egui::pos2(cx, cy), Vec2::new(actor_w, actor_h));
                        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        ui.painter().image(tex.id(), actor_rect, uv, tint);
                    }
                    any_frame_shown = true;
                }
            } else if fc.extracting && !any_frame_shown {
                ui.put(rect, egui::Label::new(
                    RichText::new("Extracting frames...").color(Color32::from_rgb(180, 150, 60)).size(14.0)));
            }
        }
    }

    // ─── GIZMO: Interactive transform on selected actor ───
    if let Selection::Actor(sel_idx) = state.selection {
        if sel_idx < state.scene.actors.len() {
            let a = &state.scene.actors[sel_idx];
            if let Some(kf) = a.layout.first() {
                let ax = kf.value.pos[0];
                let ay = kf.value.pos[1];
                let ascale = kf.value.scale;

                let actor_w = rect.width() * ascale * 0.5;
                let actor_h = rect.height() * ascale * 0.5;
                let cx = rect.min.x + ax * rect.width();
                let cy = rect.min.y + ay * rect.height();
                let gizmo_rect = egui::Rect::from_center_size(egui::pos2(cx, cy), Vec2::new(actor_w, actor_h));

                // Draw selection frame
                ui.painter().rect_stroke(gizmo_rect, Rounding::same(2.0),
                    Stroke::new(2.0, Color32::from_rgb(255, 220, 80)));

                // Corner handles
                let handle_size = 8.0;
                let corners = [gizmo_rect.left_top(), gizmo_rect.right_top(),
                               gizmo_rect.left_bottom(), gizmo_rect.right_bottom()];
                for corner in &corners {
                    let hr = egui::Rect::from_center_size(*corner, Vec2::splat(handle_size));
                    ui.painter().rect_filled(hr, Rounding::same(2.0), Color32::from_rgb(255, 220, 80));
                }

                // Center crosshair
                ui.painter().circle_stroke(egui::pos2(cx, cy), 6.0,
                    Stroke::new(1.5, Color32::from_rgb(255, 220, 80)));
            }

            // Handle drag on preview to move actor position
            if preview_resp.dragged() && !state.eyedropper_active {
                let delta = preview_resp.drag_delta();
                let dx_norm = delta.x / rect.width();
                let dy_norm = delta.y / rect.height();

                if let Some(kf) = state.scene.actors[sel_idx].layout.first_mut() {
                    kf.value.pos[0] = (kf.value.pos[0] + dx_norm).clamp(-0.5, 1.5);
                    kf.value.pos[1] = (kf.value.pos[1] + dy_norm).clamp(-0.5, 1.5);
                }
            }

            // Handle scroll wheel on preview to scale actor
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.5 && preview_resp.hovered() {
                let scale_delta = scroll * 0.002;
                if let Some(kf) = state.scene.actors[sel_idx].layout.first_mut() {
                    kf.value.scale = (kf.value.scale + scale_delta).clamp(0.05, 5.0);
                }
            }
        }
    }

    // Eyedropper handling
    if state.eyedropper_active && preview_resp.clicked() {
        if let Some(pos) = preview_resp.interact_pointer_pos() {
            let u = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let v = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
            if let Selection::Actor(idx) = state.selection {
                if idx < state.scene.actors.len() {
                    let t_in = state.scene.actors[idx].t_in.unwrap_or(0.0);
                    let src_start = state.scene.actors[idx].source_start;
                    let local_t = state.playhead - t_in + src_start;
                    if let Some(fc) = state.frame_caches.get_mut(idx) {
                        if let Some(img) = fc.raw_frame_at_time(local_t) {
                            let px = ((u * img.size[0] as f32) as usize).min(img.size[0].saturating_sub(1));
                            let py = ((v * img.size[1] as f32) as usize).min(img.size[1].saturating_sub(1));
                            let pixel = img.pixels[py * img.size[0] + px];
                            state.scene.actors[idx].chroma_key.key_color = [pixel.r(), pixel.g(), pixel.b()];
                            state.status = format!("Picked: ({}, {}, {})", pixel.r(), pixel.g(), pixel.b());
                        }
                    }
                }
            }
            state.eyedropper_active = false;
        }
    }
    if state.eyedropper_active && preview_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }

    if !any_frame_shown && state.frame_caches.is_empty() {
        if let Some(p) = &state.last_preview {
            let uri = format!("file://{}", p.display());
            ui.put(rect, egui::Image::from_uri(uri).fit_to_exact_size(rect.size()));
        } else {
            ui.put(rect, egui::Label::new(
                RichText::new("Preview\n\nAdd clips and hit play").color(Color32::from_rgb(60, 60, 80)).size(14.0)));
        }
    } else if !any_frame_shown {
        ui.put(rect, egui::Label::new(
            RichText::new("No clip active at this time").color(Color32::from_rgb(60, 60, 80)).size(13.0)));
    }
}


/// Apply chroma-key processing to a ColorImage.
fn apply_chroma_key(image: &mut egui::ColorImage, params: &ChromaKeyParams) {
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

        let dr = r - kr;
        let dg = g - kg;
        let db = b - kb;
        let distance = (dr * dr + dg * dg + db * db).sqrt() / 1.732_050_8;

        if distance < similarity {
            *pixel = Color32::from_rgba_unmultiplied(pixel.r(), pixel.g(), pixel.b(), 0);
        } else if distance < similarity + blend {
            let t = (distance - similarity) / blend.max(0.001);
            let alpha = (t * 255.0).round() as u8;
            let mut new_g = pixel.g() as f32;
            let spill_factor = spill * (1.0 - distance);
            new_g = (new_g - new_g * spill_factor).max(0.0);
            *pixel = Color32::from_rgba_unmultiplied(pixel.r(), new_g.round() as u8, pixel.b(), alpha);
        } else if distance < similarity + blend + 0.15 {
            let proximity = 1.0 - ((distance - similarity - blend) / 0.15).clamp(0.0, 1.0);
            let spill_factor = spill * proximity;
            let new_g = (pixel.g() as f32 * (1.0 - spill_factor)).round() as u8;
            *pixel = Color32::from_rgba_unmultiplied(pixel.r(), new_g, pixel.b(), pixel.a());
        }
    }
}

// ─── HELPERS ─────────────────────────────────────────────────────────

fn color_edit_u8(ui: &mut egui::Ui, c: &mut [u8; 3]) {
    let mut rgb = [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0];
    if ui.color_edit_button_rgb(&mut rgb).changed() {
        c[0] = (rgb[0] * 255.0).round() as u8;
        c[1] = (rgb[1] * 255.0).round() as u8;
        c[2] = (rgb[2] * 255.0).round() as u8;
    }
}

fn ellipsis(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}...", s.chars().take(n).collect::<String>()) }
}

fn clean_clip_text(raw: &str) -> String {
    let noise: &[&str] = &["Имба", "Топ", "Херня", "имба", "топ", "херня", "—", "\u{2014}"];
    let mut s: String = raw.chars().filter(|c| {
        c.is_ascii() || ('\u{0400}'..='\u{04FF}').contains(c) || *c == ' ' || *c == '.' || *c == ',' || *c == '!' || *c == '?'
    }).collect();
    for n in noise { s = s.replace(n, ""); }
    while s.contains("  ") { s = s.replace("  ", " "); }
    s.trim_matches(|c: char| c == ' ' || c == '-').to_string()
}

pub fn add_actor_from_clip(state: &mut EditorState, path: &PathBuf) {
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("mellstroy_{}", s))
        .unwrap_or_else(|| format!("actor_{}", state.scene.actors.len() + 1));

    let clip_duration = probe_video_duration(path);
    let t_in = state.playhead;
    let t_out = (t_in + clip_duration).min(state.scene.output.duration);

    let actor = Actor {
        id: id.clone(),
        source: path.clone(),
        anchors: None,
        chroma_key: ChromaKeyParams::default(),
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: Some(t_in),
        t_out: Some(t_out),
        source_start: 0.0,
        loop_source: false,
        flip_horizontal: false,
        attachments: Vec::new(),
        visible: true,
    };
    state.scene.actors.push(actor);
    state.selection = Selection::Actor(state.scene.actors.len() - 1);
    state.status = format!("Added: {}", id);
}

fn add_background_from_path(state: &mut EditorState, path: &PathBuf) {
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| s.to_string()).unwrap_or_else(|| format!("bg_{}", state.scene.backgrounds.len() + 1));

    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let source = if ["jpg", "jpeg", "png", "webp"].iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        MediaSource::Image { path: path.clone() }
    } else {
        MediaSource::Video { path: path.clone(), r#loop: true, start_at: 0.0 }
    };

    let dur = if ["mp4", "mov", "webm"].iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        probe_video_duration(path)
    } else { state.scene.output.duration };

    let bg = Background {
        id, source, start: state.playhead,
        duration: dur.min(state.scene.output.duration - state.playhead),
        fit: Fit::Cover, transition: Transition::Cut,
    };
    state.scene.backgrounds.push(bg);
    state.selection = Selection::Background(state.scene.backgrounds.len() - 1);
    state.status = "Background added".into();
}

fn probe_video_duration(path: &PathBuf) -> f32 {
    let ffprobe = {
        let mut p = memstroy_render::ffmpeg_binary();
        p.set_file_name("ffprobe");
        if !p.exists() { PathBuf::from("ffprobe") } else { p }
    };
    match std::process::Command::new(&ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path).output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().parse::<f32>().unwrap_or(5.0),
        Err(_) => 5.0,
    }
}


// ─── KEYBOARD SHORTCUTS ──────────────────────────────────────────────

/// Handle JKL/IO keyboard shortcuts for playback and clip trimming.
///
/// - J: decrease playback speed or play backwards
/// - K: pause
/// - L: increase playback speed or start playing
/// - I: set t_in of selected clip to current playhead
/// - O: set t_out of selected clip to current playhead
pub fn handle_keyboard_shortcuts(ctx: &egui::Context, state: &mut EditorState) {
    let j_pressed = ctx.input(|i| i.key_pressed(egui::Key::J));
    let k_pressed = ctx.input(|i| i.key_pressed(egui::Key::K));
    let l_pressed = ctx.input(|i| i.key_pressed(egui::Key::L));
    let i_pressed = ctx.input(|i| i.key_pressed(egui::Key::I));
    let o_pressed = ctx.input(|i| i.key_pressed(egui::Key::O));

    // J: decrease speed or reverse
    if j_pressed {
        if !state.playing {
            state.playing = true;
            state.playback_speed = -1.0;
        } else if state.playback_speed > -4.0 {
            state.playback_speed = (state.playback_speed - 1.0).max(-4.0);
            if state.playback_speed == 0.0 {
                state.playback_speed = -1.0;
            }
        }
    }

    // K: pause
    if k_pressed {
        state.playing = false;
    }

    // L: increase speed or start playing
    if l_pressed {
        if !state.playing {
            state.playing = true;
            state.playback_speed = 1.0;
        } else if state.playback_speed < 4.0 {
            state.playback_speed = (state.playback_speed + 1.0).min(4.0);
            if state.playback_speed == 0.0 {
                state.playback_speed = 1.0;
            }
        }
    }

    // I: set t_in of selected clip to current playhead
    if i_pressed {
        let ph = state.playhead;
        match state.selection {
            Selection::Actor(idx) => {
                if idx < state.scene.actors.len() {
                    state.scene.actors[idx].t_in = Some(ph);
                    state.status = format!("Set in-point to {:.2}s", ph);
                }
            }
            Selection::Overlay(idx) => {
                if idx < state.scene.overlays.len() {
                    match &mut state.scene.overlays[idx] {
                        Overlay::Text(t) => { t.t_in = ph; }
                        Overlay::Image(im) => { im.t_in = ph; }
                        Overlay::Video(v) => { v.t_in = ph; }
                    }
                    state.status = format!("Set in-point to {:.2}s", ph);
                }
            }
            Selection::Background(idx) => {
                if idx < state.scene.backgrounds.len() {
                    let old_end = state.scene.backgrounds[idx].start + state.scene.backgrounds[idx].duration;
                    state.scene.backgrounds[idx].start = ph;
                    state.scene.backgrounds[idx].duration = (old_end - ph).max(0.01);
                    state.status = format!("Set in-point to {:.2}s", ph);
                }
            }
            Selection::Audio(idx) => {
                if idx < state.scene.audio.len() {
                    state.scene.audio[idx].t_in = ph;
                    state.status = format!("Set in-point to {:.2}s", ph);
                }
            }
            _ => {}
        }
    }

    // O: set t_out of selected clip to current playhead
    if o_pressed {
        let ph = state.playhead;
        match state.selection {
            Selection::Actor(idx) => {
                if idx < state.scene.actors.len() {
                    state.scene.actors[idx].t_out = Some(ph);
                    state.status = format!("Set out-point to {:.2}s", ph);
                }
            }
            Selection::Overlay(idx) => {
                if idx < state.scene.overlays.len() {
                    match &mut state.scene.overlays[idx] {
                        Overlay::Text(t) => { t.t_out = ph; }
                        Overlay::Image(im) => { im.t_out = ph; }
                        Overlay::Video(v) => { v.t_out = ph; }
                    }
                    state.status = format!("Set out-point to {:.2}s", ph);
                }
            }
            Selection::Background(idx) => {
                if idx < state.scene.backgrounds.len() {
                    let start = state.scene.backgrounds[idx].start;
                    state.scene.backgrounds[idx].duration = (ph - start).max(0.01);
                    state.status = format!("Set out-point to {:.2}s", ph);
                }
            }
            Selection::Audio(idx) => {
                if idx < state.scene.audio.len() {
                    state.scene.audio[idx].t_out = Some(ph);
                    state.status = format!("Set out-point to {:.2}s", ph);
                }
            }
            _ => {}
        }
    }
}
