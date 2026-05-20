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

    // Quick Preset row (TikTok / YouTube / IG / etc.)
    inspector_export_preset_row(ui, state);

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

/// Render the "Quick Preset" combo + Apply button used in the Output Settings.
fn inspector_export_preset_row(ui: &mut egui::Ui, state: &mut EditorState) {
    use crate::export_presets::{apply_preset, PRESETS};

    // Selected preset index lives in egui memory so it persists across frames.
    let mem_id = egui::Id::new("export_preset_idx");
    let mut idx: usize = ui
        .ctx()
        .memory(|m| m.data.get_temp::<usize>(mem_id).unwrap_or(0));
    if idx >= PRESETS.len() {
        idx = 0;
    }

    ui.horizontal(|ui| {
        ui.label("Preset:");
        let preset = &PRESETS[idx];
        egui::ComboBox::from_id_source("export_preset_combo")
            .selected_text(format!("{} {}", preset.icon, preset.name))
            .show_ui(ui, |ui| {
                for (i, p) in PRESETS.iter().enumerate() {
                    let label = format!(
                        "{} {} ({}, {}x{} @ {} fps)",
                        p.icon, p.name, p.aspect_label, p.resolution[0], p.resolution[1], p.fps
                    );
                    if ui.selectable_value(&mut idx, i, label).clicked() {
                        // selection changes are persisted below
                    }
                }
            });
        if ui
            .button(RichText::new("Apply").size(11.0))
            .on_hover_text(PRESETS[idx].description)
            .clicked()
        {
            let preset = &PRESETS[idx];
            apply_preset(&mut state.scene.output, preset);
            state.status = format!(
                "\u{1F39E} Preset applied: {} ({}x{} @ {} fps)",
                preset.name, preset.resolution[0], preset.resolution[1], preset.fps
            );
        }
    });

    ui.ctx().memory_mut(|m| m.data.insert_temp(mem_id, idx));
    ui.add_space(4.0);
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

    ui.add_space(12.0);

    // Color Correction
    egui::CollapsingHeader::new(
        RichText::new("Color Correction").size(12.0).strong().color(Color32::from_rgb(200, 180, 255))
    ).default_open(true).show(ui, |ui| {
        let cc = &mut state.scene.actors[i].color_correction;
        ui.add(egui::Slider::new(&mut cc.brightness, -1.0..=1.0).text("Brightness"));
        ui.add(egui::Slider::new(&mut cc.contrast, 0.0..=3.0).text("Contrast"));
        ui.add(egui::Slider::new(&mut cc.saturation, 0.0..=3.0).text("Saturation"));
        ui.add(egui::Slider::new(&mut cc.temperature, -1.0..=1.0).text("Temperature"));
        ui.add_space(4.0);
        if ui.small_button("Reset").clicked() {
            let cc = &mut state.scene.actors[i].color_correction;
            *cc = memstroy_core::ColorCorrection::default();
        }
    });

    ui.add_space(12.0);

    // Transitions in/out (visible window edges)
    inspector_actor_transitions(ui, state, i);
}

fn inspector_actor_transitions(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    egui::CollapsingHeader::new(
        RichText::new("Transitions").size(12.0).strong().color(Color32::from_rgb(255, 180, 100))
    ).default_open(true).show(ui, |ui| {
        let a = &mut state.scene.actors[i];

        ui.horizontal(|ui| {
            ui.label("In:");
            transition_combo(ui, "transition_in", &mut a.transition_in);
        });
        ui.horizontal(|ui| {
            ui.label("Out:");
            transition_combo(ui, "transition_out", &mut a.transition_out);
        });
        ui.horizontal(|ui| {
            ui.label("Duration:");
            ui.add(
                egui::DragValue::new(&mut a.transition_duration)
                    .range(0.0..=5.0)
                    .speed(0.02)
                    .suffix("s"),
            );
        });
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "Cut = no effect. Fade = opacity. Slide* = enter/exit by sliding off-screen.",
            )
            .size(10.0)
            .color(COL_TEXT_DIM)
            .italics(),
        );
    });
}

fn transition_combo(ui: &mut egui::Ui, id: &str, t: &mut Transition) {
    egui::ComboBox::from_id_source(id)
        .selected_text(format!("{:?}", t))
        .show_ui(ui, |ui| {
            ui.selectable_value(t, Transition::Cut, "Cut");
            ui.selectable_value(t, Transition::Fade, "Fade");
            ui.selectable_value(t, Transition::SlideLeft, "SlideLeft");
            ui.selectable_value(t, Transition::SlideRight, "SlideRight");
            ui.selectable_value(t, Transition::SlideUp, "SlideUp");
            ui.selectable_value(t, Transition::SlideDown, "SlideDown");
        });
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


// ─── SNAP HELPER ─────────────────────────────────────────────────────

/// Snap a time value to the closest target if within threshold.
/// Returns the snapped value if close enough, otherwise returns `t` unchanged.
fn snap_time(t: f32, targets: &[f32], threshold: f32) -> f32 {
    let mut best = t;
    let mut best_dist = threshold;
    for &target in targets {
        let dist = (t - target).abs();
        if dist < best_dist {
            best = target;
            best_dist = dist;
        }
    }
    best
}

/// Collect all clip edges (start/end times) from the scene, excluding a specific actor index.
fn collect_clip_edges(state: &EditorState, exclude_actor: Option<usize>) -> Vec<f32> {
    let mut edges = Vec::new();
    let duration = state.scene.output.duration;

    for (i, a) in state.scene.actors.iter().enumerate() {
        if exclude_actor == Some(i) { continue; }
        edges.push(a.t_in.unwrap_or(0.0));
        edges.push(a.t_out.unwrap_or(duration));
    }
    for bg in &state.scene.backgrounds {
        edges.push(bg.start);
        edges.push(bg.start + bg.duration);
    }
    for ov in &state.scene.overlays {
        let (s, e) = match ov {
            Overlay::Text(t) => (t.t_in, t.t_out),
            Overlay::Image(im) => (im.t_in, im.t_out),
            Overlay::Video(v) => (v.t_in, v.t_out),
        };
        edges.push(s);
        edges.push(e);
    }
    for au in &state.scene.audio {
        edges.push(au.t_in);
        edges.push(au.t_out.unwrap_or(duration));
    }
    edges
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

        // Loop preview toggle
        let loop_color = if state.loop_mode { Color32::from_rgb(255, 180, 80) } else { COL_TEXT_DIM };
        if ui
            .button(RichText::new("\u{1F501} Loop").size(11.0).color(loop_color))
            .on_hover_text(
                "Loop preview: Shift+click on the ruler to set loop start, Shift+click again for end. \
                Shift+drag = define a region.",
            )
            .clicked()
        {
            state.loop_mode = !state.loop_mode;
            if !state.loop_mode {
                state.loop_pending_start = None;
            }
        }

        // Add Title (template picker)
        if ui
            .button(RichText::new("\u{1F4DD} Add Title").size(11.0).color(COL_ACCENT))
            .on_hover_text("Insert a styled meme title at the playhead")
            .clicked()
        {
            state.title_picker_open = true;
        }

        // Curve editor toggle
        let curve_color = if state.curve_editor_open { COL_ACCENT } else { COL_TEXT_DIM };
        if ui.button(RichText::new("Curve").size(11.0).color(curve_color)).on_hover_text("Toggle curve editor").clicked() {
            state.curve_editor_open = !state.curve_editor_open;
        }

        // Clip editor toggle
        let clip_ed_color = if state.clip_editor_open { COL_ACCENT } else { COL_TEXT_DIM };
        if ui.button(RichText::new("Clip").size(11.0).color(clip_ed_color)).on_hover_text("Toggle clip editor").clicked() {
            state.clip_editor_open = !state.clip_editor_open;
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
                let clicked_t = x_to_time(pos.x, state.timeline_scroll, pps, track_left)
                    .clamp(0.0, duration);
                let shift_held = ui.input(|i| i.modifiers.shift);

                if state.loop_mode && shift_held {
                    // Shift+drag to define a region
                    if ruler_resp.dragged() {
                        let press = ruler_resp.interact_pointer_pos().unwrap_or(pos);
                        let press_t = x_to_time(press.x, state.timeline_scroll, pps, track_left)
                            .clamp(0.0, duration);
                        let drag_t = clicked_t;
                        let (a, b) = if press_t <= drag_t {
                            (press_t, drag_t)
                        } else {
                            (drag_t, press_t)
                        };
                        if (b - a).abs() > 0.01 {
                            state.loop_region = Some((a, b));
                            state.status =
                                format!("\u{1F501} Loop region: {:.2}s - {:.2}s", a, b);
                        }
                        state.loop_pending_start = None;
                    } else if ruler_resp.clicked() {
                        match state.loop_pending_start.take() {
                            None => {
                                state.loop_pending_start = Some(clicked_t);
                                state.status = format!(
                                    "\u{1F501} Loop start set to {:.2}s. Shift+click for end.",
                                    clicked_t
                                );
                            }
                            Some(start) => {
                                let (a, b) = if start <= clicked_t {
                                    (start, clicked_t)
                                } else {
                                    (clicked_t, start)
                                };
                                if (b - a).abs() > 0.01 {
                                    state.loop_region = Some((a, b));
                                    state.status = format!(
                                        "\u{1F501} Loop region: {:.2}s - {:.2}s",
                                        a, b
                                    );
                                }
                            }
                        }
                    }
                } else {
                    state.playhead = clicked_t;
                }
            }
        }
    }

    // Draw loop region band on the ruler.
    if state.loop_mode {
        if let Some((ls, le)) = state.loop_region {
            let (ls, le) = if ls <= le { (ls, le) } else { (le, ls) };
            let lx0 = (ls - state.timeline_scroll) * pps + track_left;
            let lx1 = (le - state.timeline_scroll) * pps + track_left;
            let lx0c = lx0.clamp(track_left, track_right);
            let lx1c = lx1.clamp(track_left, track_right);
            if lx1c > lx0c {
                let band = egui::Rect::from_min_max(
                    egui::pos2(lx0c, ruler_rect.min.y),
                    egui::pos2(lx1c, ruler_rect.max.y),
                );
                painter.rect_filled(
                    band,
                    Rounding::ZERO,
                    Color32::from_rgba_premultiplied(255, 180, 80, 60),
                );
                // Handles at start/end
                let handle_color = Color32::from_rgb(255, 180, 80);
                painter.line_segment(
                    [egui::pos2(lx0c, ruler_rect.min.y), egui::pos2(lx0c, ruler_rect.max.y)],
                    Stroke::new(2.0, handle_color),
                );
                painter.line_segment(
                    [egui::pos2(lx1c, ruler_rect.min.y), egui::pos2(lx1c, ruler_rect.max.y)],
                    Stroke::new(2.0, handle_color),
                );
            }
        }
        // Pending start marker
        if let Some(start) = state.loop_pending_start {
            let lx = (start - state.timeline_scroll) * pps + track_left;
            if lx >= track_left && lx <= track_right {
                painter.line_segment(
                    [egui::pos2(lx, ruler_rect.min.y), egui::pos2(lx, ruler_rect.max.y)],
                    Stroke::new(1.5, Color32::from_rgba_premultiplied(255, 220, 120, 200)),
                );
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
                        let trans_in = actor.transition_in;
                        let trans_out = actor.transition_out;
                        let trans_dur = actor.transition_duration;
                        let sel = state.selection == Selection::Actor(ai);
                        if let Some(clicked) = draw_clip(ui, &painter, content_rect, &actor.id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            COL_CLIP_ACTOR, sel, track_h, track_locked, state.razor_mode)
                        {
                            if clicked < 0.0 {
                                // Drag: move the actor's time window
                                let mut new_start = (-clicked).max(0.0);
                                let dur = clip_end - clip_start;

                                // ── Undo snapshot on drag start ──
                                if state.timeline_drag.dragging_clip.is_none() {
                                    state.undo.push(&state.scene);
                                    state.timeline_drag.dragging_clip = Some(ai);
                                }

                                // ── Snap-to-edges logic ──
                                if state.snap_enabled {
                                    let new_end = new_start + dur;
                                    let mut snap_targets = collect_clip_edges(state, Some(ai));
                                    snap_targets.push(state.playhead);
                                    let threshold = 0.1;

                                    let snapped_start = snap_time(new_start, &snap_targets, threshold);
                                    let snapped_end = snap_time(new_end, &snap_targets, threshold);

                                    // Prefer start snap, fall back to end snap
                                    if (snapped_start - new_start).abs() < threshold {
                                        new_start = snapped_start;
                                    } else if (snapped_end - new_end).abs() < threshold {
                                        new_start = snapped_end - dur;
                                    }
                                }

                                state.scene.actors[ai].t_in = Some(new_start);
                                state.scene.actors[ai].t_out = Some(new_start + dur);
                                to_select = Some(Selection::Actor(ai));
                            } else if state.razor_mode {
                                to_select = Some(Selection::Actor(ai));
                                state.playhead = clicked;
                                state.status = "__SPLIT_AT_PLAYHEAD__".into();
                            } else {
                                // ── Ctrl+click multi-select ──
                                let ctrl_held = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                                if ctrl_held {
                                    // Toggle in multi_select
                                    if let Some(pos) = state.multi_select.iter().position(|&x| x == ai) {
                                        state.multi_select.remove(pos);
                                    } else {
                                        state.multi_select.push(ai);
                                    }
                                } else {
                                    state.multi_select.clear();
                                }
                                to_select = Some(Selection::Actor(ai));
                            }
                        }

                        // Transition indicators on the clip bar (faded gradient near edges).
                        draw_transition_indicators(
                            &painter,
                            content_rect,
                            clip_start,
                            clip_end,
                            trans_in,
                            trans_out,
                            trans_dur,
                            state.timeline_scroll,
                            pps,
                            track_left,
                            track_right,
                        );
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

    // ── Library asset drag-to-track: drop handling ──
    // When an asset is being dragged from the library and mouse is released over timeline,
    // determine which track row and time position to drop it on.
    let mouse_released = ui.input(|i| i.pointer.any_released());
    if state.asset_drag.dragging.is_some() && mouse_released {
        let mouse_pos = ui.input(|i| i.pointer.hover_pos());
        if let Some(pos) = mouse_pos {
            // Calculate which track the mouse is over by Y position
            // Track rows start after the ruler (approx offset)
            let track_y_start = ui.min_rect().min.y; // approximate top of tracks area
            let mut accumulated_y = track_y_start;
            let mut _drop_track: Option<usize> = None;

            for (tidx, track) in state.tracks.iter().enumerate() {
                let track_bottom = accumulated_y + track.height;
                if pos.y >= accumulated_y && pos.y < track_bottom {
                    _drop_track = Some(tidx);
                    break;
                }
                accumulated_y = track_bottom;
            }

            // Determine time position from X
            let drop_time = x_to_time(pos.x, state.timeline_scroll, pps, track_left)
                .clamp(0.0, duration);

            let asset_path = state.asset_drag.dragging.clone().unwrap();
            let kind = state.asset_drag.kind;

            match kind {
                AssetDragKind::Clip => {
                    // Create actor at that time on that track
                    add_actor_from_clip_at_time(state, &asset_path, drop_time);
                }
                AssetDragKind::Background => {
                    // Add background starting at that time
                    add_background_from_path_at_time(state, &asset_path, drop_time);
                }
                AssetDragKind::Prop => {
                    // Add as image overlay at drop time
                    let id = asset_path.file_stem().and_then(|s| s.to_str())
                        .map(|s| format!("img_{}", s))
                        .unwrap_or_else(|| format!("img_{}", state.scene.overlays.len() + 1));
                    let overlay = Overlay::Image(ImageOverlay {
                        id: id.clone(),
                        source: asset_path.clone(),
                        t_in: drop_time,
                        t_out: (drop_time + 3.0).min(state.scene.output.duration),
                        layout: vec![Keyframe::new(0.0, OverlayState {
                            pos: [0.5, 0.5], scale: 0.3, rotation_deg: 0.0, opacity: 1.0
                        })],
                    });
                    state.scene.overlays.push(overlay);
                    state.selection = Selection::Overlay(state.scene.overlays.len() - 1);
                    state.status = format!("Dropped overlay: {}", id);
                }
                AssetDragKind::Audio => {
                    // Add audio track at drop time
                    let id = asset_path.file_stem().and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("audio_{}", state.scene.audio.len() + 1));
                    state.scene.audio.push(AudioTrack {
                        id,
                        source: asset_path.clone(),
                        t_in: drop_time,
                        t_out: None,
                        source_start: 0.0,
                        volume: 1.0,
                    });
                    state.selection = Selection::Audio(state.scene.audio.len() - 1);
                    state.status = "Dropped audio track.".into();
                }
                AssetDragKind::None => {}
            }

            // Clear the drag state
            state.asset_drag.dragging = None;
            state.asset_drag.kind = AssetDragKind::None;
        }
    }

    // ── Draw visual indicator while dragging from library ──
    if let Some(ref _dragged_path) = state.asset_drag.dragging {
        let drag_pos = egui::pos2(state.asset_drag.pos[0], state.asset_drag.pos[1]);
        // Draw a ghost indicator at the drop position
        let ghost_w = 80.0;
        let ghost_h = 20.0;
        let ghost_rect = egui::Rect::from_center_size(drag_pos, Vec2::new(ghost_w, ghost_h));
        let painter = ui.painter();
        painter.rect_stroke(
            ghost_rect,
            Rounding::same(3.0),
            Stroke::new(2.0, Color32::from_rgba_premultiplied(255, 200, 50, 180)),
        );
        painter.rect_filled(
            ghost_rect,
            Rounding::same(3.0),
            Color32::from_rgba_premultiplied(255, 200, 50, 40),
        );
        painter.text(
            ghost_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop here",
            egui::FontId::proportional(9.0),
            Color32::from_rgb(255, 220, 80),
        );
    }

    // ── Reset drag state when mouse is released (no active drag) ──
    let any_dragging = ui.input(|i| i.pointer.any_down());
    if !any_dragging {
        state.timeline_drag.dragging_clip = None;
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

    // Color-coded left stripe (3px wide, brighter) for visual clip identification
    {
        let stripe_w = 3.0_f32;
        let stripe_rect = egui::Rect::from_min_max(
            egui::pos2(bar_rect.min.x, bar_rect.min.y + 1.0),
            egui::pos2(bar_rect.min.x + stripe_w, bar_rect.max.y - 1.0),
        );
        let stripe_color = Color32::from_rgb(
            color.r().saturating_add(60),
            color.g().saturating_add(60),
            color.b().saturating_add(60),
        );
        painter.rect_filled(stripe_rect, Rounding::same(2.0), stripe_color);
    }

    // Selection border
    if selected {
        painter.rect_stroke(bar_rect.expand(1.0), Rounding::same(5.0), Stroke::new(2.0, COL_SELECTED));
    }

    // Label inside
    if bar_rect.width() > 30.0 {
        let text = if bar_rect.width() > 80.0 { label.to_string() } else { ellipsis(label, 6) };
        painter.text(
            egui::pos2(bar_rect.min.x + 8.0, bar_rect.center().y),
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


/// Draw a small triangle marker + faded gradient overlay representing a
/// non-`Cut` transition at either edge of an actor clip on the timeline.
#[allow(clippy::too_many_arguments)]
fn draw_transition_indicators(
    painter: &egui::Painter,
    content_rect: egui::Rect,
    clip_start: f32,
    clip_end: f32,
    trans_in: Transition,
    trans_out: Transition,
    trans_dur: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
) {
    if trans_dur <= 0.0 {
        return;
    }

    let x_start = (clip_start - scroll) * pps + track_left;
    let x_end = (clip_end - scroll) * pps + track_left;
    if x_end < track_left || x_start > track_right {
        return;
    }

    let band_w = (trans_dur * pps).clamp(2.0, (clip_end - clip_start) * pps * 0.5);

    // In-edge band: from x_start..x_start+band_w
    if !matches!(trans_in, Transition::Cut) {
        let bx0 = x_start.max(track_left);
        let bx1 = (x_start + band_w).min(track_right);
        if bx1 > bx0 + 1.0 {
            let band = egui::Rect::from_min_max(
                egui::pos2(bx0, content_rect.min.y + 2.0),
                egui::pos2(bx1, content_rect.max.y - 2.0),
            );
            painter.rect_filled(
                band,
                Rounding::same(2.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 50),
            );
            // Triangle marker pointing right at the in-edge
            let tri = 4.0;
            let ty = content_rect.min.y + 4.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(bx0, ty),
                    egui::pos2(bx0 + tri * 1.4, ty + tri),
                    egui::pos2(bx0, ty + tri * 2.0),
                ],
                Color32::from_rgb(255, 220, 120),
                Stroke::NONE,
            ));
        }
    }

    // Out-edge band: from x_end-band_w..x_end
    if !matches!(trans_out, Transition::Cut) {
        let bx0 = (x_end - band_w).max(track_left);
        let bx1 = x_end.min(track_right);
        if bx1 > bx0 + 1.0 {
            let band = egui::Rect::from_min_max(
                egui::pos2(bx0, content_rect.min.y + 2.0),
                egui::pos2(bx1, content_rect.max.y - 2.0),
            );
            painter.rect_filled(
                band,
                Rounding::same(2.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 50),
            );
            let tri = 4.0;
            let ty = content_rect.min.y + 4.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(bx1, ty),
                    egui::pos2(bx1 - tri * 1.4, ty + tri),
                    egui::pos2(bx1, ty + tri * 2.0),
                ],
                Color32::from_rgb(255, 220, 120),
                Stroke::NONE,
            ));
        }
    }
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
         actor.layout.first().map(|kf| kf.value).unwrap_or_default(),
         actor.transition_in, actor.transition_out, actor.transition_duration)
    }).collect();

    for (actor_idx, visible, t_in, t_out, source_start, ref chroma_key,
         actor_state, trans_in, trans_out, trans_dur) in actor_data.iter() {
        if !visible { continue; }
        if t < *t_in || t > *t_out { continue; }

        let local_t = t - t_in + source_start;

        // Compute transition modulation (opacity + slide offset, normalized).
        let (trans_alpha, trans_offset) =
            compute_actor_transition(t, *t_in, *t_out, *trans_in, *trans_out, *trans_dur);

        if let Some(fc) = state.frame_caches.get_mut(*actor_idx) {
            if fc.is_ready() {
                // Use frame_at_time which reuses a single TextureHandle per actor (no allocation per frame)
                if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                    // Compute actor rect based on layout state (position/scale)
                    let ax = actor_state.pos[0] + trans_offset[0];
                    let ay = actor_state.pos[1] + trans_offset[1];
                    let ascale = actor_state.scale;
                    let rotation_rad = actor_state.rotation_deg.to_radians();

                    // Actor rect centered at (ax, ay) in normalized coords, scaled
                    let actor_w = rect.width() * ascale * 0.5;
                    let actor_h = rect.height() * ascale * 0.5;
                    let cx = rect.min.x + ax * rect.width();
                    let cy = rect.min.y + ay * rect.height();

                    let final_alpha = (actor_state.opacity * trans_alpha).clamp(0.0, 1.0);
                    let tint = Color32::from_rgba_unmultiplied(255, 255, 255, (final_alpha * 255.0) as u8);

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

    // ─── TEXT OVERLAY RENDERING & INLINE EDITING ───
    {
        let playhead = state.playhead;
        let mut clicked_overlay: Option<usize> = None;

        for (ov_idx, ov) in state.scene.overlays.iter().enumerate() {
            if let Overlay::Text(text_ov) = ov {
                // Check if overlay is active at current playhead
                if playhead >= text_ov.t_in && playhead <= text_ov.t_out {
                    // Calculate position on preview rect
                    let ov_state = text_ov.layout.first()
                        .map(|kf| kf.value)
                        .unwrap_or_default();
                    let ox = rect.min.x + ov_state.pos[0] * rect.width();
                    let oy = rect.min.y + ov_state.pos[1] * rect.height();
                    let scale_factor = ov_state.scale * (rect.width() / 1080.0);
                    let font_size = text_ov.style.font_size * scale_factor * 0.3;

                    // Draw the text at that position
                    let text_color = Color32::from_rgb(
                        text_ov.style.color[0],
                        text_ov.style.color[1],
                        text_ov.style.color[2],
                    );

                    let galley = ui.painter().layout_no_wrap(
                        text_ov.text.clone(),
                        egui::FontId::proportional(font_size.max(8.0)),
                        text_color,
                    );
                    let text_rect = egui::Rect::from_center_size(
                        egui::pos2(ox, oy),
                        galley.size(),
                    );

                    // Draw background plate if configured
                    if let Some(box_col) = text_ov.style.box_color {
                        let pad = 4.0;
                        let bg_rect = text_rect.expand(pad);
                        ui.painter().rect_filled(
                            bg_rect,
                            Rounding::same(2.0),
                            Color32::from_rgb(box_col[0], box_col[1], box_col[2]),
                        );
                    }

                    ui.painter().galley(text_rect.min, galley, text_color);

                    // Check if user clicked on this text overlay region
                    if preview_resp.clicked() && !state.eyedropper_active {
                        if let Some(pos) = preview_resp.interact_pointer_pos() {
                            if text_rect.expand(6.0).contains(pos) {
                                clicked_overlay = Some(ov_idx);
                            }
                        }
                    }

                    // Draw selection indicator if this overlay is selected
                    if state.selection == Selection::Overlay(ov_idx) {
                        ui.painter().rect_stroke(
                            text_rect.expand(3.0),
                            Rounding::same(2.0),
                            Stroke::new(1.5, Color32::from_rgb(80, 200, 120)),
                        );
                    }
                }
            }
        }

        // Handle click selection
        if let Some(ov_idx) = clicked_overlay {
            state.selection = Selection::Overlay(ov_idx);
            state.editing_text_overlay = Some(ov_idx);
        }

        // Handle Escape to stop editing
        let escape_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if escape_pressed {
            state.editing_text_overlay = None;
        }

        // If clicked outside overlays on preview (and not eyedropper), stop editing
        if preview_resp.clicked() && !state.eyedropper_active && clicked_overlay.is_none() {
            state.editing_text_overlay = None;
        }

        // Show floating TextEdit when editing_text_overlay is Some
        if let Some(edit_idx) = state.editing_text_overlay {
            if edit_idx < state.scene.overlays.len() {
                if let Overlay::Text(text_ov) = &state.scene.overlays[edit_idx] {
                    let ov_state = text_ov.layout.first()
                        .map(|kf| kf.value)
                        .unwrap_or_default();
                    let ox = rect.min.x + ov_state.pos[0] * rect.width();
                    let oy = rect.min.y + ov_state.pos[1] * rect.height();

                    let area_id = ui.id().with("text_edit_overlay");
                    egui::Area::new(area_id)
                        .fixed_pos(egui::pos2(ox - 80.0, oy + 15.0))
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                if let Overlay::Text(ref mut t) = &mut state.scene.overlays[edit_idx] {
                                    let response = ui.add(
                                        egui::TextEdit::multiline(&mut t.text)
                                            .desired_width(200.0)
                                            .desired_rows(2)
                                            .hint_text("Enter text...")
                                    );
                                    if response.lost_focus() {
                                        state.editing_text_overlay = None;
                                    }
                                }
                            });
                        });
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


/// Compute the visual modulation produced by `transition_in` / `transition_out`
/// at scene time `t` for an actor whose visible window is `[t_in, t_out]`.
///
/// Returns `(alpha, [dx, dy])` where `alpha` is multiplied with the actor's
/// existing opacity and `[dx, dy]` is a normalised position offset (scene
/// space, [0, 1] coordinates).
fn compute_actor_transition(
    t: f32,
    t_in: f32,
    t_out: f32,
    trans_in: Transition,
    trans_out: Transition,
    duration: f32,
) -> (f32, [f32; 2]) {
    if duration <= 0.0 {
        return (1.0, [0.0, 0.0]);
    }
    let mut alpha = 1.0_f32;
    let mut offset = [0.0_f32, 0.0_f32];

    // In transition: progress from 0 → 1 across the first `duration` seconds.
    if t >= t_in && t <= t_in + duration && !matches!(trans_in, Transition::Cut) {
        let p = ((t - t_in) / duration).clamp(0.0, 1.0);
        match trans_in {
            Transition::Fade | Transition::Snap => alpha = p,
            Transition::SlideLeft => offset[0] = -(1.0 - p),
            Transition::SlideRight => offset[0] = 1.0 - p,
            Transition::SlideUp => offset[1] = -(1.0 - p),
            Transition::SlideDown => offset[1] = 1.0 - p,
            Transition::Cut => {}
        }
    }

    // Out transition: progress from 0 → 1 across the last `duration` seconds.
    if t <= t_out && t >= t_out - duration && !matches!(trans_out, Transition::Cut) {
        let p = ((t_out - t) / duration).clamp(0.0, 1.0);
        // p == 1 at t_out - duration (start of out), 0 at t_out (full out)
        match trans_out {
            Transition::Fade | Transition::Snap => alpha = alpha.min(p),
            Transition::SlideLeft => offset[0] += -(1.0 - p),
            Transition::SlideRight => offset[0] += 1.0 - p,
            Transition::SlideUp => offset[1] += -(1.0 - p),
            Transition::SlideDown => offset[1] += 1.0 - p,
            Transition::Cut => {}
        }
    }

    (alpha.clamp(0.0, 1.0), offset)
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

/// Apply color correction (brightness, contrast, saturation, temperature) to a ColorImage.
pub fn apply_color_correction(image: &mut egui::ColorImage, params: &memstroy_core::ColorCorrection) {
    // Early exit if parameters are all defaults
    if (params.brightness - 0.0).abs() < 0.001
        && (params.contrast - 1.0).abs() < 0.001
        && (params.saturation - 1.0).abs() < 0.001
        && (params.temperature - 0.0).abs() < 0.001
    {
        return;
    }

    for pixel in image.pixels.iter_mut() {
        let mut r = pixel.r() as f32 / 255.0;
        let mut g = pixel.g() as f32 / 255.0;
        let mut b = pixel.b() as f32 / 255.0;

        // Brightness: add offset
        r += params.brightness;
        g += params.brightness;
        b += params.brightness;

        // Contrast: multiply around midpoint 0.5
        r = (r - 0.5) * params.contrast + 0.5;
        g = (g - 0.5) * params.contrast + 0.5;
        b = (b - 0.5) * params.contrast + 0.5;

        // Saturation: lerp toward luminance
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        r = luma + (r - luma) * params.saturation;
        g = luma + (g - luma) * params.saturation;
        b = luma + (b - luma) * params.saturation;

        // Temperature: shift warm (positive) / cool (negative)
        // Positive temperature: add red, subtract blue
        // Negative temperature: add blue, subtract red
        r += params.temperature * 0.1;
        b -= params.temperature * 0.1;

        // Clamp
        r = r.clamp(0.0, 1.0);
        g = g.clamp(0.0, 1.0);
        b = b.clamp(0.0, 1.0);

        *pixel = Color32::from_rgba_unmultiplied(
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
            pixel.a(),
        );
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
        color_correction: ColorCorrection::default(),
        transition_in: Transition::Cut,
        transition_out: Transition::Cut,
        transition_duration: 0.3,
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

/// Add an actor from a clip at a specific time (used by drag-to-track).
fn add_actor_from_clip_at_time(state: &mut EditorState, path: &PathBuf, t: f32) {
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("mellstroy_{}", s))
        .unwrap_or_else(|| format!("actor_{}", state.scene.actors.len() + 1));

    let clip_duration = probe_video_duration(path);
    let t_in = t;
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
        color_correction: ColorCorrection::default(),
        transition_in: Transition::Cut,
        transition_out: Transition::Cut,
        transition_duration: 0.3,
    };
    state.scene.actors.push(actor);
    state.selection = Selection::Actor(state.scene.actors.len() - 1);
    state.status = format!("Dropped actor: {}", id);
}

/// Add a background at a specific time (used by drag-to-track).
fn add_background_from_path_at_time(state: &mut EditorState, path: &PathBuf, t: f32) {
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
        id, source, start: t,
        duration: dur.min(state.scene.output.duration - t),
        fit: Fit::Cover, transition: Transition::Cut,
    };
    state.scene.backgrounds.push(bg);
    state.selection = Selection::Background(state.scene.backgrounds.len() - 1);
    state.status = "Background dropped".into();
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


// ─── AI GENERATION PANEL ─────────────────────────────────────────────

/// AI meme generation floating window.
/// Shows prompt input, generate button, result paste area, and apply button.
pub fn ai_generate_window(ctx: &egui::Context, state: &mut EditorState) {
    if !state.ai_window_open {
        return;
    }

    let mut open = state.ai_window_open;
    egui::Window::new("AI Meme Generator")
        .open(&mut open)
        .resizable(true)
        .default_width(500.0)
        .default_height(600.0)
        .show(ctx, |ui| {
            ui.label(RichText::new("AI-Powered Meme Montage").size(16.0).strong()
                .color(Color32::from_rgb(180, 120, 255)));
            ui.add_space(4.0);
            ui.label(RichText::new("Describe the meme you want to create. The AI will generate a montage plan.")
                .size(11.0).color(COL_TEXT_DIM));
            ui.add_space(8.0);

            // Prompt input
            ui.label(RichText::new("Creative Prompt:").size(12.0).strong());
            ui.add(
                egui::TextEdit::multiline(&mut state.ai_prompt)
                    .desired_width(ui.available_width())
                    .desired_rows(3)
                    .hint_text("e.g. Сделай мем где Мелстрой бьет по столу когда видит цену биткоина")
            );
            ui.add_space(8.0);

            // Generate button - builds ProjectInput and copies to clipboard
            ui.horizontal(|ui| {
                let generate_btn = egui::Button::new(
                    RichText::new("Generate (Copy to Clipboard)").color(Color32::WHITE).size(13.0)
                ).fill(Color32::from_rgb(100, 60, 200)).rounding(Rounding::same(6.0));

                if ui.add(generate_btn).clicked() && !state.ai_prompt.is_empty() {
                    let project_input = build_project_input(state);
                    if let Ok(json) = serde_json::to_string_pretty(&project_input) {
                        ui.output_mut(|o| o.copied_text = json.clone());
                        state.status = "AI ProjectInput copied to clipboard!".into();
                    }
                }
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);

            // Result paste area
            ui.label(RichText::new("Paste AI Response (MontageOutput JSON):").size(12.0).strong());
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut state.ai_result_json)
                        .desired_width(ui.available_width())
                        .desired_rows(8)
                        .hint_text("Paste the AI's JSON response here...")
                        .code_editor()
                );
            });
            ui.add_space(8.0);

            // Apply button
            ui.horizontal(|ui| {
                let apply_btn = egui::Button::new(
                    RichText::new("Apply to Scene").color(Color32::WHITE).size(13.0)
                ).fill(Color32::from_rgb(50, 160, 80)).rounding(Rounding::same(6.0));

                if ui.add(apply_btn).clicked() && !state.ai_result_json.is_empty() {
                    match serde_json::from_str::<memstroy_core::MontageOutput>(&state.ai_result_json) {
                        Ok(output) => {
                            let clips_dir = state.clips_dir();
                            // Save undo snapshot
                            state.undo.push(&state.scene);
                            output.apply_to_scene(&mut state.scene, &clips_dir);
                            state.status = "AI montage applied to scene!".into();
                        }
                        Err(e) => {
                            state.status = format!("JSON parse error: {}", e);
                        }
                    }
                }

                if ui.button("Clear").clicked() {
                    state.ai_result_json.clear();
                }
            });

            ui.add_space(8.0);

            // Show AI reasoning if result was applied
            if !state.ai_result_json.is_empty() {
                if let Ok(output) = serde_json::from_str::<memstroy_core::MontageOutput>(&state.ai_result_json) {
                    if !output.reasoning.is_empty() {
                        ui.separator();
                        ui.add_space(4.0);
                        ui.label(RichText::new("AI Reasoning:").size(12.0).strong()
                            .color(Color32::from_rgb(100, 200, 255)));
                        ui.label(RichText::new(&output.reasoning).size(11.0).color(COL_TEXT_DIM));
                    }
                }
            }
        });
    state.ai_window_open = open;
}

/// Build a ProjectInput from the current editor state.
fn build_project_input(state: &EditorState) -> memstroy_core::ProjectInput {
    use memstroy_core::*;

    let available_clips: Vec<ClipInfo> = state.library.mellstroy_clips.iter().map(|c| {
        ClipInfo {
            id: format!("{}", c.id),
            description: c.description.clone(),
            duration: 5.0, // default estimate
            path: c.path.display().to_string(),
            tags: Vec::new(),
            detected_actions: Vec::new(),
        }
    }).collect();

    let available_backgrounds: Vec<AssetInfo> = state.library.backgrounds.iter().map(|p| {
        AssetInfo {
            id: p.file_stem().and_then(|s| s.to_str()).unwrap_or("bg").to_string(),
            path: p.display().to_string(),
            description: p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string(),
            duration: None,
        }
    }).collect();

    let available_props: Vec<AssetInfo> = state.library.props.iter().map(|p| {
        AssetInfo {
            id: p.file_stem().and_then(|s| s.to_str()).unwrap_or("prop").to_string(),
            path: p.display().to_string(),
            description: p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string(),
            duration: None,
        }
    }).collect();

    let current_scene = if !state.scene.actors.is_empty() || !state.scene.overlays.is_empty() {
        Some(SceneSnapshot {
            actors: state.scene.actors.iter().map(|a| {
                let pos = a.layout.first().map(|kf| kf.value.pos).unwrap_or([0.5, 0.5]);
                let scale = a.layout.first().map(|kf| kf.value.scale).unwrap_or(1.0);
                ActorSnapshot {
                    clip_id: a.id.clone(),
                    t_in: a.t_in.unwrap_or(0.0),
                    t_out: a.t_out.unwrap_or(state.scene.output.duration),
                    position: pos,
                    scale,
                }
            }).collect(),
            texts: state.scene.overlays.iter().filter_map(|ov| {
                if let Overlay::Text(t) = ov {
                    let pos = t.layout.first().map(|kf| kf.value.pos).unwrap_or([0.5, 0.5]);
                    Some(TextSnapshot {
                        text: t.text.clone(),
                        t_in: t.t_in,
                        t_out: t.t_out,
                        position: pos,
                    })
                } else {
                    None
                }
            }).collect(),
            duration: state.scene.output.duration,
        })
    } else {
        None
    };

    ProjectInput {
        prompt: state.ai_prompt.clone(),
        available_clips,
        available_backgrounds,
        available_props,
        available_audio: Vec::new(),
        canvas: CanvasConstraints {
            resolution: state.scene.output.resolution,
            fps: state.scene.output.fps,
            max_duration: 60.0,
            target_duration: state.scene.output.duration,
        },
        current_scene,
        style: StyleHints {
            text_style: "meme_impact".to_string(),
            pacing: "fast".to_string(),
            use_chroma_key: true,
            transitions: vec!["cut".to_string(), "snap".to_string()],
        },
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
