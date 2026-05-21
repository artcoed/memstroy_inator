//! UI panels — Premiere Pro-style timeline, modern inspector, drag&drop.

use std::path::PathBuf;

use egui::{Color32, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::{AssetDragKind, EditorState, Selection, TrackKind};


// ─── DRAG MODE FOR TIMELINE CLIPS ────────────────────────────────────
//
// Captured once at `drag_started` and stashed in egui's per-id temp memory
// for the duration of the drag, so the mode never flips mid-drag (which
// previously happened when the clip moved out from under the pointer's
// initial edge zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipDragMode {
    Move,
    TrimLeft,
    TrimRight,
}


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

/// Unified asset library: a single scrollable column listing every asset
/// in the project (clips, backgrounds, props, audio). Every row is a drag
/// source — drop on the timeline to add. OS Explorer drops are also routed
/// into this library implicitly via `App::update`.
pub fn library(ui: &mut egui::Ui, state: &mut EditorState, _request_refresh: impl Fn()) {
    // Header
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
            .hint_text("Search assets...")
            .desired_width(ui.available_width()),
    );
    ui.add_space(2.0);
    ui.label(RichText::new("Tip: drag from here to the timeline. You can also drop files directly from your file manager.")
        .size(9.0).italics().color(COL_TEXT_DIM));
    ui.add_space(6.0);

    let search_lower = state.library_search.to_lowercase();

    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
        // ── Clips section ──
        let clip_count = state.library.mellstroy_clips.len();
        ui.label(RichText::new(format!("Clips ({})", clip_count)).size(12.0).strong()
            .color(Color32::from_rgb(220, 130, 50)));
        ui.add_space(2.0);
        if state.library.mellstroy_clips.is_empty() {
            ui.label(RichText::new("No clips. Hit Refresh to download.")
                .italics().color(COL_TEXT_DIM).size(11.0));
        } else {
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
        }
        ui.add_space(8.0);

        // ── Backgrounds ──
        ui.label(RichText::new(format!("Backgrounds ({})", state.library.backgrounds.len()))
            .size(12.0).strong().color(Color32::from_rgb(100, 180, 255)));
        ui.add_space(2.0);
        if state.library.backgrounds.is_empty() {
            ui.label(RichText::new("Drop videos/images into assets/backgrounds/")
                .italics().color(COL_TEXT_DIM).size(11.0));
        } else {
            for p in state.library.backgrounds.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                if !search_lower.is_empty() && !name.to_lowercase().contains(&search_lower) {
                    continue;
                }
                let resp = ui.add(
                    egui::Label::new(RichText::new(format!("\u{1F5BC} {}", name)).size(11.0))
                        .sense(Sense::click_and_drag()),
                );
                if resp.dragged() {
                    state.asset_drag.dragging = Some(p.clone());
                    state.asset_drag.kind = AssetDragKind::Background;
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        state.asset_drag.pos = [pos.x, pos.y];
                    }
                }
                if resp.double_clicked() {
                    add_background_from_path(state, &p);
                }
            }
        }
        ui.add_space(8.0);

        // ── Props (image overlays) ──
        ui.label(RichText::new(format!("Props ({})", state.library.props.len()))
            .size(12.0).strong().color(Color32::from_rgb(200, 200, 100)));
        ui.add_space(2.0);
        if state.library.props.is_empty() {
            ui.label(RichText::new("Drop PNG/WebP into assets/props/")
                .italics().color(COL_TEXT_DIM).size(11.0));
        } else {
            for p in state.library.props.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                if !search_lower.is_empty() && !name.to_lowercase().contains(&search_lower) {
                    continue;
                }
                let resp = ui.add(
                    egui::Label::new(RichText::new(format!("\u{1F5BC} {}", name)).size(11.0))
                        .sense(Sense::click_and_drag()),
                );
                if resp.dragged() {
                    state.asset_drag.dragging = Some(p.clone());
                    state.asset_drag.kind = AssetDragKind::Prop;
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        state.asset_drag.pos = [pos.x, pos.y];
                    }
                }
                if resp.double_clicked() {
                    add_image_overlay(state, &p);
                }
            }
        }
        ui.add_space(8.0);

        // ── Audio ──
        ui.label(RichText::new(format!("Audio ({})", state.library.audio.len()))
            .size(12.0).strong().color(Color32::from_rgb(50, 180, 180)));
        ui.add_space(2.0);
        if state.library.audio.is_empty() {
            ui.label(RichText::new("Drop MP3/WAV/OGG into assets/audio/")
                .italics().color(COL_TEXT_DIM).size(11.0));
        } else {
            for p in state.library.audio.clone() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("(?)").to_string();
                if !search_lower.is_empty() && !name.to_lowercase().contains(&search_lower) {
                    continue;
                }
                let resp = ui.add(
                    egui::Label::new(RichText::new(format!("\u{1F3B5} {}", name)).size(11.0))
                        .sense(Sense::click_and_drag()),
                );
                if resp.dragged() {
                    state.asset_drag.dragging = Some(p.clone());
                    state.asset_drag.kind = AssetDragKind::Audio;
                    if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                        state.asset_drag.pos = [pos.x, pos.y];
                    }
                }
                if resp.double_clicked() {
                    add_audio_from_path(state, &p);
                }
            }
        }
    });
}


fn add_audio_from_path(state: &mut EditorState, path: &PathBuf) {
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("audio_{}", state.scene.audio.len() + 1));
    state.scene.audio.push(AudioTrack {
        id: id.clone(),
        source: path.clone(),
        t_in: state.playhead,
        t_out: None,
        source_start: 0.0,
        volume: 1.0,
    });
    state.selection = Selection::Audio(state.scene.audio.len() - 1);
    state.status = format!("Added audio: {}", id);
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
        layout: vec![Keyframe::new(0.0, OverlayState { pos: [0.5, 0.5], scale: 0.3, scale_y: 1.0, rotation_deg: 0.0, opacity: 1.0 })],
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

    let card_resp = frame.show(ui, |ui| {
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
        });
    }).response;

    // Whole-card click + drag handling. The card is the drag source for the
    // timeline (drop target inside the timeline area decides what happens).
    let card_resp = card_resp.interact(Sense::click_and_drag());
    if card_resp.dragged() {
        state.asset_drag.dragging = Some(clip.path.clone());
        state.asset_drag.kind = AssetDragKind::Clip;
        if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
            state.asset_drag.pos = [pos.x, pos.y];
        }
    }
    if card_resp.double_clicked() {
        // Convenience: double-click adds at playhead without needing to drag.
        add_actor_from_clip(state, &clip.path);
    }
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

    // Output settings — fixed 1080x1920 9:16 short format
    ui.label(RichText::new("Output").size(14.0).strong().color(Color32::from_rgb(100, 200, 255)));
    ui.add_space(4.0);
    ui.label(RichText::new("1080x1920 (9:16)").size(12.0).color(COL_TEXT_DIM));
    ui.add_space(4.0);

    let spec = &mut state.scene.output;
    ui.horizontal(|ui| {
        ui.label("FPS:");
        ui.add(egui::DragValue::new(&mut spec.fps).range(24..=60));
    });
    ui.horizontal(|ui| {
        ui.label("Duration:");
        ui.add(egui::DragValue::new(&mut spec.duration).range(0.5..=60.0).speed(0.1).suffix("s"));
    });
}

fn inspector_actor(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let actor_count = state.scene.actors.len();
    let cache_count = state.frame_caches.len();

    // Header with name (delete button removed — use Delete/Backspace shortcut
    // or right-click on the timeline clip instead).
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Actor: {}", state.scene.actors[i].id))
            .strong().size(14.0).color(COL_CLIP_ACTOR));
    });
    ui.add_space(2.0);
    ui.label(RichText::new(
        state.scene.actors[i].source.file_name().and_then(|s| s.to_str()).unwrap_or("(source)")
    ).size(10.0).color(COL_TEXT_DIM));
    ui.add_space(6.0);

    // Tab bar: Transform | Effects
    ui.horizontal(|ui| {
        if ui.selectable_label(state.inspector_tab == 0, "Transform").clicked() { state.inspector_tab = 0; }
        if ui.selectable_label(state.inspector_tab == 2, "Effects").clicked() { state.inspector_tab = 2; }
    });
    ui.separator();
    ui.add_space(4.0);

    match state.inspector_tab {
        0 => inspector_actor_transform(ui, state, i),
        2 => inspector_actor_effects(ui, state, i, actor_count, cache_count),
        _ => inspector_actor_transform(ui, state, i),
    }
}


fn inspector_actor_transform(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Position & Scale").size(12.0).strong());
    ui.add_space(4.0);

    // Edit the first keyframe directly (simple mode)
    if a.layout.is_empty() {
        a.layout.push(Keyframe::new(0.0, ActorState::default()));
    }

    if let Some(kf) = a.layout.first_mut() {
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
            ui.label(RichText::new("Stretch Y:").size(11.0))
                .on_hover_text("Y-axis stretch on top of uniform scale (1.0 = proportional)");
            ui.add(egui::Slider::new(&mut kf.value.scale_y, 0.1..=5.0).logarithmic(true));
            if ui.small_button("\u{21BB}").on_hover_text("Reset to 1.0 (proportional)").clicked() {
                kf.value.scale_y = 1.0;
            }
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

    ui.add_space(8.0);
    ui.checkbox(&mut a.visible, "Visible");
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


fn inspector_actor_effects(ui: &mut egui::Ui, state: &mut EditorState, i: usize, _actor_count: usize, _cache_count: usize) {
    let a = &mut state.scene.actors[i];

    ui.label(RichText::new("Chroma Key").size(12.0).strong().color(Color32::from_rgb(100, 255, 100)));
    ui.add_space(4.0);

    // Eyedropper
    let mut chroma_changed = false;
    ui.horizontal(|ui| {
        if state.eyedropper_active {
            ui.label(RichText::new("Click preview to pick color...").color(Color32::from_rgb(255, 200, 50)).size(11.0));
        } else if ui.button("Eyedropper").on_hover_text("Pick color from preview").clicked() {
            state.eyedropper_active = true;
        }
        ui.label("Key:");
        if color_edit_u8(ui, &mut a.chroma_key.key_color) {
            chroma_changed = true;
        }
    });

    ui.add_space(4.0);
    if ui.add(egui::Slider::new(&mut a.chroma_key.similarity, 0.0..=1.0).text("Similarity")).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.blend, 0.0..=1.0).text("Blend")).changed() {
        chroma_changed = true;
    }
    if ui.add(egui::Slider::new(&mut a.chroma_key.spill, 0.0..=1.0).text("Spill")).changed() {
        chroma_changed = true;
    }

    // Persist chroma settings as a sidecar next to the source clip so they
    // follow the asset across projects.
    if chroma_changed {
        let src = state.scene.actors[i].source.clone();
        let chroma = state.scene.actors[i].chroma_key.clone();
        let _ = chroma.save_alongside_clip(&src);
    }

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

    // Skeleton Attachments
    inspector_actor_skeleton_attachments(ui, state, i);
}

fn inspector_actor_skeleton_attachments(ui: &mut egui::Ui, state: &mut EditorState, i: usize) {
    egui::CollapsingHeader::new(
        RichText::new("Skeleton Attachments").size(12.0).strong().color(Color32::from_rgb(180, 120, 255))
    ).default_open(false).show(ui, |ui| {
        // List existing skeleton attachments on this actor
        let num_att = state.scene.actors[i].skeleton_attachments.len();
        if num_att == 0 {
            ui.label(RichText::new("No skeleton attachments.").size(10.0).color(COL_TEXT_DIM).italics());
        } else {
            let mut to_remove: Option<usize> = None;
            for ai in 0..num_att {
                let att = &state.scene.actors[i].skeleton_attachments[ai];
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}.{}", att.skeleton_id, att.point_name)).size(11.0));
                    ui.label(RichText::new(format!("s:{:.2}", att.scale)).size(9.0).color(COL_TEXT_DIM));
                    if ui.small_button("x").clicked() {
                        to_remove = Some(ai);
                    }
                });
            }
            if let Some(ri) = to_remove {
                state.scene.actors[i].skeleton_attachments.remove(ri);
            }
        }

        ui.add_space(4.0);

        // "Attach to skeleton point" button — shows available skeletons/points
        let available_skeletons: Vec<(String, Vec<String>)> = state.scene.skeleton_templates.iter()
            .map(|tmpl| {
                let names: Vec<String> = tmpl.points.keys().cloned().collect();
                (tmpl.name.clone(), names)
            })
            .collect();

        if available_skeletons.is_empty() {
            ui.label(RichText::new("No skeleton templates. Use Tools > Skeleton Constructor to create one.")
                .size(9.0).color(COL_TEXT_DIM).italics());
        } else {
            // Combo: pick skeleton
            let skel_id = ui.make_persistent_id("skel_attach_combo");
            let mut sel_skel: usize = ui.ctx().memory(|m| m.data.get_temp(skel_id).unwrap_or(0));
            if sel_skel >= available_skeletons.len() { sel_skel = 0; }

            ui.horizontal(|ui| {
                ui.label("Skeleton:");
                egui::ComboBox::from_id_source("attach_skel_sel")
                    .selected_text(&available_skeletons[sel_skel].0)
                    .show_ui(ui, |ui| {
                        for (si, (name, _)) in available_skeletons.iter().enumerate() {
                            ui.selectable_value(&mut sel_skel, si, name);
                        }
                    });
            });
            ui.ctx().memory_mut(|m| m.data.insert_temp(skel_id, sel_skel));

            // Combo: pick point
            let points = &available_skeletons[sel_skel].1;
            if points.is_empty() {
                ui.label(RichText::new("No points defined in this skeleton.").size(9.0).color(COL_TEXT_DIM));
            } else {
                let pt_id = ui.make_persistent_id("skel_point_combo");
                let mut sel_pt: usize = ui.ctx().memory(|m| m.data.get_temp(pt_id).unwrap_or(0));
                if sel_pt >= points.len() { sel_pt = 0; }

                ui.horizontal(|ui| {
                    ui.label("Point:");
                    egui::ComboBox::from_id_source("attach_pt_sel")
                        .selected_text(&points[sel_pt])
                        .show_ui(ui, |ui| {
                            for (pi, name) in points.iter().enumerate() {
                                ui.selectable_value(&mut sel_pt, pi, name);
                            }
                        });
                });
                ui.ctx().memory_mut(|m| m.data.insert_temp(pt_id, sel_pt));

                if ui.button(RichText::new("+ Attach").size(11.0).color(Color32::from_rgb(80, 200, 120))).clicked() {
                    let attachment = memstroy_core::SkeletonAttachment {
                        skeleton_id: available_skeletons[sel_skel].0.clone(),
                        point_name: points[sel_pt].clone(),
                        offset: [0.0, 0.0],
                        scale: 1.0,
                        follow_rotation: false,
                    };
                    state.scene.actors[i].skeleton_attachments.push(attachment);
                    state.status = format!("Attached to {}.{}", available_skeletons[sel_skel].0, points[sel_pt]);
                }
            }
        }
    });
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
    let overlay_count = state.scene.overlays.len();
    let mut text_action: Option<TextAction> = None;

    {
        let ov = &mut state.scene.overlays[i];

        match ov {
            Overlay::Text(t) => {
                text_action = inspector_text_overlay(ui, t, i, overlay_count, duration);
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

    if let Some(action) = text_action {
        apply_text_action(state, i, action);
    }
}

#[derive(Clone, Copy)]
enum TextAction {
    /// Bump z_index by +1 (and swap with a neighbor if needed for visual order).
    LayerUp,
    /// Bump z_index by -1.
    LayerDown,
    /// Set z_index to (max+1) of all overlays.
    ToFront,
    /// Set z_index to (min-1) of all overlays.
    ToBack,
    /// Delete this overlay.
    Delete,
}

fn apply_text_action(state: &mut EditorState, i: usize, action: TextAction) {
    if i >= state.scene.overlays.len() {
        return;
    }
    match action {
        TextAction::LayerUp => {
            if let Overlay::Text(t) = &mut state.scene.overlays[i] {
                t.z_index = t.z_index.saturating_add(1);
            }
        }
        TextAction::LayerDown => {
            if let Overlay::Text(t) = &mut state.scene.overlays[i] {
                t.z_index = t.z_index.saturating_sub(1);
            }
        }
        TextAction::ToFront => {
            let max_z = state.scene.overlays.iter().filter_map(|o| match o {
                Overlay::Text(t) => Some(t.z_index),
                _ => None,
            }).max().unwrap_or(100);
            if let Overlay::Text(t) = &mut state.scene.overlays[i] {
                t.z_index = max_z.saturating_add(1);
            }
        }
        TextAction::ToBack => {
            let min_z = state.scene.overlays.iter().filter_map(|o| match o {
                Overlay::Text(t) => Some(t.z_index),
                _ => None,
            }).min().unwrap_or(0);
            if let Overlay::Text(t) = &mut state.scene.overlays[i] {
                t.z_index = min_z.saturating_sub(1);
            }
        }
        TextAction::Delete => {
            state.scene.overlays.remove(i);
            state.selection = Selection::None;
            state.status = "\u{1F5D1} Text overlay deleted.".into();
        }
    }
}

fn inspector_text_overlay(
    ui: &mut egui::Ui,
    t: &mut TextOverlay,
    _idx: usize,
    _total: usize,
    duration: f32,
) -> Option<TextAction> {
    let mut action: Option<TextAction> = None;

    // Header (delete button removed — use Delete/Backspace shortcut or
    // right-click on the timeline clip).
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Text: {}", t.id))
            .strong().size(14.0).color(COL_CLIP_OVERLAY));
    });
    ui.add_space(2.0);

    // ID
    ui.horizontal(|ui| {
        ui.label(RichText::new("ID:").size(11.0).color(COL_TEXT_DIM));
        ui.text_edit_singleline(&mut t.id);
    });
    ui.add_space(4.0);

    // Text content
    ui.label(RichText::new("Text:").size(11.0).strong());
    ui.add(
        egui::TextEdit::multiline(&mut t.text)
            .desired_rows(2)
            .desired_width(ui.available_width()),
    );
    ui.add_space(4.0);

    // Timing
    ui.horizontal(|ui| {
        ui.label("In:");
        ui.add(egui::DragValue::new(&mut t.t_in)
            .range(0.0..=duration).speed(0.02).suffix("s"));
        ui.label("Out:");
        ui.add(egui::DragValue::new(&mut t.t_out)
            .range(0.0..=duration).speed(0.02).suffix("s"));
    });
    ui.add_space(8.0);

    // ─── Layer order ──────────────────────────────────────────────
    ui.label(RichText::new("Layer").size(12.0).strong()
        .color(Color32::from_rgb(180, 180, 220)));
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if ui.small_button("\u{2B06}").on_hover_text("Bring forward").clicked() {
            action = Some(TextAction::LayerUp);
        }
        if ui.small_button("\u{2B07}").on_hover_text("Send back").clicked() {
            action = Some(TextAction::LayerDown);
        }
        if ui.small_button("Top").on_hover_text("Bring to front").clicked() {
            action = Some(TextAction::ToFront);
        }
        if ui.small_button("Bot").on_hover_text("Send to back").clicked() {
            action = Some(TextAction::ToBack);
        }
        ui.label(RichText::new(format!("z={}", t.z_index)).size(10.0).color(COL_TEXT_DIM));
    });
    ui.checkbox(&mut t.behind_actors, "Below actors")
        .on_hover_text("Render this text BEHIND actors (above background, below clips)");
    ui.add_space(8.0);

    // Position from first keyframe (so it can be edited from the inspector too)
    if let Some(kf) = t.layout.first_mut() {
        ui.horizontal(|ui| {
            ui.label("X:"); ui.add(egui::DragValue::new(&mut kf.value.pos[0]).range(-2.0..=3.0).speed(0.005));
            ui.label("Y:"); ui.add(egui::DragValue::new(&mut kf.value.pos[1]).range(-2.0..=3.0).speed(0.005));
        });
        ui.add(egui::Slider::new(&mut kf.value.scale, 0.05..=5.0).text("Scale").logarithmic(true));
        ui.add(egui::Slider::new(&mut kf.value.rotation_deg, -180.0..=180.0).text("Rotation"));
        ui.add(egui::Slider::new(&mut kf.value.opacity, 0.0..=1.0).text("Opacity"));
    }
    ui.add_space(8.0);

    // ─── Font ─────────────────────────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new("Font").size(12.0).strong().color(Color32::from_rgb(180, 220, 255)),
    ).default_open(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Family:");
            egui::ComboBox::from_id_source("text_font_family")
                .selected_text(t.style.font.clone())
                .show_ui(ui, |ui| {
                    for fam in COMMON_FONTS {
                        ui.selectable_value(&mut t.style.font, fam.to_string(), *fam);
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut t.style.font)
                    .desired_width(120.0)
                    .hint_text("Family name"),
            );
        });
        ui.add(egui::Slider::new(&mut t.style.font_size, 8.0..=512.0).text("Size"));
        ui.horizontal(|ui| {
            ui.checkbox(&mut t.style.bold, "Bold");
            ui.checkbox(&mut t.style.italic, "Italic");
        });
        ui.horizontal(|ui| {
            ui.label("Color:");
            color_edit_u8(ui, &mut t.style.color);
        });
        ui.horizontal(|ui| {
            ui.label("Align:");
            ui.selectable_value(&mut t.style.align, TextAlign::Left, "Left");
            ui.selectable_value(&mut t.style.align, TextAlign::Center, "Center");
            ui.selectable_value(&mut t.style.align, TextAlign::Right, "Right");
        });
    });
    ui.add_space(4.0);

    // ─── Stroke (glyph outline) ───────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new("Stroke").size(12.0).strong().color(Color32::from_rgb(255, 200, 120)),
    ).default_open(true).show(ui, |ui| {
        let mut has_outline = t.style.outline.is_some();
        ui.checkbox(&mut has_outline, "Stroke text");
        if has_outline && t.style.outline.is_none() {
            t.style.outline = Some([0, 0, 0]);
            if t.style.outline_width <= 0.0 { t.style.outline_width = 4.0; }
        }
        if !has_outline {
            t.style.outline = None;
        }

        if let Some(oc) = t.style.outline.as_mut() {
            ui.horizontal(|ui| {
                ui.label("Color:");
                color_edit_u8(ui, oc);
                ui.label("Width:");
                ui.add(egui::DragValue::new(&mut t.style.outline_width)
                    .range(0.0..=20.0).speed(0.1));
            });
        }
    });
    ui.add_space(4.0);

    // ─── Background plate ─────────────────────────────────────────
    egui::CollapsingHeader::new(
        RichText::new("Background plate").size(12.0).strong().color(Color32::from_rgb(180, 255, 180)),
    ).default_open(true).show(ui, |ui| {
        let mut has_box = t.style.box_color.is_some();
        ui.checkbox(&mut has_box, "Enable plate");
        if has_box && t.style.box_color.is_none() {
            t.style.box_color = Some([255, 255, 255]);
        }
        if !has_box {
            t.style.box_color = None;
        }

        if t.style.box_color.is_some() {
            ui.horizontal(|ui| {
                ui.label("Type:");
                egui::ComboBox::from_id_source("text_box_kind")
                    .selected_text(format!("{:?}", t.style.box_kind))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Solid, "Solid");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::Gradient, "Gradient");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::OutlineOnly, "Outline only");
                        ui.selectable_value(&mut t.style.box_kind, TextBoxKind::None, "None (text only)");
                    });
            });

            if matches!(t.style.box_kind, TextBoxKind::Solid | TextBoxKind::Gradient) {
                if let Some(bc) = t.style.box_color.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label("Color:"); color_edit_u8(ui, bc);
                    });
                }
            }
            if matches!(t.style.box_kind, TextBoxKind::Gradient) {
                if t.style.box_gradient_end.is_none() {
                    t.style.box_gradient_end = Some([60, 60, 60]);
                }
                if let Some(end) = t.style.box_gradient_end.as_mut() {
                    ui.horizontal(|ui| {
                        ui.label("Gradient end:"); color_edit_u8(ui, end);
                    });
                }
            }

            ui.add(egui::Slider::new(&mut t.style.box_opacity, 0.0..=1.0).text("Opacity"));
            ui.add(egui::Slider::new(&mut t.style.box_padding, 0.0..=80.0).text("Padding"));
            ui.add(egui::Slider::new(&mut t.style.box_corner_radius, 0.0..=80.0).text("Corner radius"));

            // Plate border (independent of glyph stroke)
            let mut has_border = t.style.box_outline_color.is_some() || t.style.box_outline_width > 0.0;
            ui.checkbox(&mut has_border, "Plate border");
            if has_border && t.style.box_outline_color.is_none() {
                t.style.box_outline_color = Some([0, 0, 0]);
            }
            if !has_border {
                t.style.box_outline_color = None;
                t.style.box_outline_width = 0.0;
            }
            if let Some(boc) = t.style.box_outline_color.as_mut() {
                ui.horizontal(|ui| {
                    ui.label("Color:"); color_edit_u8(ui, boc);
                    ui.label("Width:");
                    ui.add(egui::DragValue::new(&mut t.style.box_outline_width)
                        .range(0.0..=20.0).speed(0.1));
                });
            }
        }
    });

    action
}

const COMMON_FONTS: &[&str] = &[
    "DejaVuSans",
    "DejaVuSans-Bold",
    "Arial",
    "Helvetica",
    "Impact",
    "Roboto",
    "Times",
    "Courier",
    "Comic Sans MS",
    "Verdana",
    "Tahoma",
    "Georgia",
];


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
        ui.add(egui::DragValue::new(&mut state.playback_speed).range(0.1..=8.0).speed(0.05).prefix("x"));

        ui.separator();

        // Time display
        let duration = state.scene.output.duration;
        ui.label(RichText::new(format_time(state.playhead)).size(13.0).strong().color(COL_TEXT));
        ui.label(RichText::new(format!("/ {}", format_time(duration))).size(11.0).color(COL_TEXT_DIM));

        ui.separator();

        // Split tool — when armed, clicking on a clip cuts it at the click position.
        let split_color = if state.split_tool_active { Color32::from_rgb(255, 80, 80) } else { COL_TEXT };
        if ui.button(RichText::new("\u{2702}").color(split_color))
            .on_hover_text("Split tool: click anywhere on a clip to cut it at that position")
            .clicked()
        {
            state.split_tool_active = !state.split_tool_active;
        }

        // Add Text tool
        if ui.button(RichText::new("\u{1F520} +T").color(Color32::from_rgb(140, 220, 255)))
            .on_hover_text("Add text overlay at playhead")
            .clicked()
        {
            add_text_overlay(state);
        }

        ui.separator();

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

        // Zoom display (read-only — adjust via scrollbar handles)
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{:.0}px/s", state.timeline_zoom)).size(10.0).color(COL_TEXT_DIM));
        });
    });
    ui.add_space(2.0);

    // ── Track area: explicit layout with custom scrollbars ──
    //
    // Layout:
    //   ┌─ ruler (header_w | track_area_w)        ──────┐ (top)
    //   │ ┌──────────┬──────────────────────────┬───┐ │
    //   │ │ track    │ tracks viewport (clipped)│ V │ │
    //   │ │ headers  │                          │ S │ │
    //   │ │          │                          │ B │ │
    //   │ └──────────┴──────────────────────────┴───┘ │
    //   │           horizontal scrollbar             │
    //   └────────────────────────────────────────────┘
    let header_width = 80.0_f32;
    let v_sb_w = 14.0_f32;
    let h_sb_h = 14.0_f32;
    let ruler_height = 22.0_f32;
    let total_avail = ui.available_size_before_wrap();
    let track_area_width = (total_avail.x - header_width - v_sb_w - 6.0).max(120.0);
    let viewport_h = (total_avail.y - ruler_height - h_sb_h - 8.0).max(60.0);

    // ── Auto-length: expand/shrink timeline to fit longest content ──
    {
        let mut max_end: f32 = 0.0;
        for a in &state.scene.actors {
            max_end = max_end.max(a.t_out.unwrap_or(0.0));
        }
        for bg in &state.scene.backgrounds {
            max_end = max_end.max(bg.start + bg.duration);
        }
        for ov in &state.scene.overlays {
            let end = match ov {
                Overlay::Text(t) => t.t_out,
                Overlay::Image(im) => im.t_out,
                Overlay::Video(v) => v.t_out,
            };
            max_end = max_end.max(end);
        }
        for au in &state.scene.audio {
            max_end = max_end.max(au.t_out.unwrap_or(0.0));
        }
        // Auto-fit: timeline length is the end of the longest layer.
        // No padding — when the playhead reaches the last clip's end the
        // loop wraps immediately back to 0 instead of running through dead
        // air.
        let target_duration = max_end.max(2.0);
        state.scene.output.duration = target_duration;
    }

    let duration = state.scene.output.duration.max(0.01);

    // Reserve and compute the master rect for the whole timeline area.
    let master_size = Vec2::new(
        header_width + track_area_width + v_sb_w + 6.0,
        ruler_height + viewport_h + h_sb_h + 8.0,
    );
    let (master_rect, _master_resp) =
        ui.allocate_exact_size(master_size, Sense::hover());

    // Sub-rects.
    let ruler_rect = egui::Rect::from_min_max(
        egui::pos2(master_rect.min.x, master_rect.min.y),
        egui::pos2(
            master_rect.min.x + header_width + track_area_width + 4.0,
            master_rect.min.y + ruler_height,
        ),
    );
    let header_col_rect = egui::Rect::from_min_max(
        egui::pos2(master_rect.min.x, master_rect.min.y + ruler_height + 2.0),
        egui::pos2(
            master_rect.min.x + header_width,
            master_rect.min.y + ruler_height + 2.0 + viewport_h,
        ),
    );
    let tracks_rect = egui::Rect::from_min_max(
        egui::pos2(
            master_rect.min.x + header_width + 2.0,
            master_rect.min.y + ruler_height + 2.0,
        ),
        egui::pos2(
            master_rect.min.x + header_width + 2.0 + track_area_width,
            master_rect.min.y + ruler_height + 2.0 + viewport_h,
        ),
    );
    let v_sb_rect = egui::Rect::from_min_max(
        egui::pos2(
            tracks_rect.max.x + 2.0,
            tracks_rect.min.y,
        ),
        egui::pos2(
            tracks_rect.max.x + 2.0 + v_sb_w,
            tracks_rect.max.y,
        ),
    );
    let h_sb_rect = egui::Rect::from_min_max(
        egui::pos2(
            tracks_rect.min.x,
            tracks_rect.max.y + 4.0,
        ),
        egui::pos2(
            tracks_rect.max.x,
            tracks_rect.max.y + 4.0 + h_sb_h,
        ),
    );

    // Painters.
    let ruler_painter = ui.painter_at(ruler_rect);
    let header_painter = ui.painter_at(header_col_rect);
    let tracks_painter = ui.painter_at(tracks_rect);

    // Background fills.
    header_painter.rect_filled(header_col_rect, Rounding::ZERO, Color32::from_rgb(26, 26, 36));
    tracks_painter.rect_filled(tracks_rect, Rounding::ZERO, COL_BG_TRACK);

    let pps = state.timeline_zoom; // pixels per second

    // Mouse wheel inside the tracks viewport: vertical scroll (and Shift = horizontal).
    let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
    let pointer_in_viewport = ui
        .input(|i| i.pointer.hover_pos())
        .map(|p| tracks_rect.contains(p) || header_col_rect.contains(p))
        .unwrap_or(false);
    if pointer_in_viewport && scroll_delta.y.abs() > 0.1 {
        let shift = ui.input(|i| i.modifiers.shift);
        if shift {
            // Shift+wheel = horizontal pan (in seconds).
            state.timeline_scroll =
                (state.timeline_scroll - scroll_delta.y / pps.max(1.0)).max(0.0);
        } else {
            // Plain wheel = vertical pan (in pixels).
            state.timeline_v_scroll = (state.timeline_v_scroll - scroll_delta.y).max(0.0);
        }
    }
    if pointer_in_viewport && scroll_delta.x.abs() > 0.1 {
        // Horizontal wheel (touchpad) = horizontal pan in seconds.
        state.timeline_scroll =
            (state.timeline_scroll - scroll_delta.x / pps.max(1.0)).max(0.0);
    }

    // ── Ruler ──
    let ruler_resp = ui.interact(
        ruler_rect,
        ui.make_persistent_id("timeline_ruler"),
        Sense::click_and_drag(),
    );
    let painter = ruler_painter;

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

    let v_zoom = state.timeline_v_zoom.max(0.1);
    let num_tracks = state.tracks.len();

    // ── Pre-compute per-track row rectangles for vertical drag-resolution ──
    // (used by clip-drag handlers below to figure out which track the pointer
    // currently hovers over, and whether the user is dragging above the
    // topmost video / below the bottommost audio so we can auto-create a new
    // layer in that direction).
    let mut track_rows: Vec<(f32, f32)> = Vec::with_capacity(num_tracks);
    {
        let mut acc = 0.0_f32;
        for tk in state.tracks.iter() {
            let h = tk.height * v_zoom;
            let top = tracks_rect.min.y + acc - state.timeline_v_scroll;
            let bot = top + h;
            track_rows.push((top, bot));
            acc += h;
        }
    }
    // Pending track-creation actions to perform AFTER the render loop, so we
    // never invalidate iteration. Each entry is the actor or audio index that
    // requested it; we then create a new track and re-assign that index.
    let mut pending_new_video_top: Option<usize> = None;   // new video track at index 0; reassign actor
    let mut pending_new_audio_bottom: Option<usize> = None; // new audio track at end; reassign audio
    let pointer_y: Option<f32> = ui.input(|i| i.pointer.hover_pos().map(|p| p.y));

    // Resolve which track index the pointer is currently over.
    // Returns:
    //   - Some(idx) if pointer.y falls within an existing track row;
    //   - None otherwise (pointer outside the tracks viewport).
    let resolve_target_track = |y: f32| -> Option<usize> {
        for (i, (top, bot)) in track_rows.iter().enumerate() {
            if y >= *top && y < *bot {
                return Some(i);
            }
        }
        None
    };

    // Total scaled height needed to fit all tracks at the current v_zoom.
    let total_tracks_h: f32 = state
        .tracks
        .iter()
        .map(|t| t.height * v_zoom)
        .sum();
    let max_v_scroll = (total_tracks_h - viewport_h).max(0.0);
    state.timeline_v_scroll = state.timeline_v_scroll.max(0.0).min(max_v_scroll);
    let v_scroll = state.timeline_v_scroll;

    let mut acc_y = 0.0_f32;
    for track_idx in 0..num_tracks {
        let track = &state.tracks[track_idx];
        let track_h = track.height * v_zoom;
        let track_kind = track.kind;
        let track_name = track.name.clone();
        let track_muted = track.muted;
        let track_locked = track.locked;

        let row_top = tracks_rect.min.y + acc_y - v_scroll;
        let row_bot = row_top + track_h;
        acc_y += track_h;

        // Cull tracks fully outside the viewport.
        if row_bot < tracks_rect.min.y - 1.0 || row_top > tracks_rect.max.y + 1.0 {
            continue;
        }

        let row_rect = egui::Rect::from_min_max(
            egui::pos2(tracks_rect.min.x, row_top),
            egui::pos2(tracks_rect.max.x, row_bot),
        );
        let painter = &tracks_painter;

        // Track background (alternating).
        let bg = if track_idx % 2 == 0 { COL_BG_TRACK } else { COL_BG_TRACK_ALT };
        painter.rect_filled(row_rect, Rounding::ZERO, bg);

        // Track header (left column, drawn with the header painter so it
        // isn't clipped by the tracks viewport).
        let hdr_rect = egui::Rect::from_min_max(
            egui::pos2(header_col_rect.min.x, row_top),
            egui::pos2(header_col_rect.max.x, row_bot),
        );
        header_painter.rect_filled(hdr_rect, Rounding::ZERO, Color32::from_rgb(30, 30, 42));
        header_painter.text(
            hdr_rect.center(),
            egui::Align2::CENTER_CENTER,
            &track_name,
            egui::FontId::proportional(11.0),
            if track_muted { COL_TEXT_DIM } else { COL_TEXT },
        );

        // Track content area = the viewport sub-rect aligned to this row.
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(tracks_rect.min.x, row_top + 1.0),
            egui::pos2(tracks_rect.max.x, row_bot - 1.0),
        );

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
                        let bg_id = egui::Id::new(("timeline_clip", "background", bi));
                        if let Some(clicked) = draw_clip(ui, painter, content_rect, &bg_elem.id, bg_id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            COL_CLIP_BG, sel, track_h, track_locked, state.split_tool_active)
                        {
                            if clicked < 0.0 {
                                let new_start = (-clicked).max(0.0);
                                let dur = clip_end - clip_start;
                                state.scene.backgrounds[bi].start = new_start;
                                state.scene.backgrounds[bi].duration = dur;
                                to_select = Some(Selection::Background(bi));
                            } else if state.split_tool_active {
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
                    // Use explicit track assignment if set, otherwise default to first video track.
                    // Multiple clips on the same track is allowed (free sequential placement).
                    let assigned_track = if let Some(&assigned) = state.actor_track_assignments.get(&ai) {
                        assigned
                    } else {
                        // Default: first video track for ALL actors (free placement on same track)
                        video_tracks.first().copied().unwrap_or(0)
                    };
                    if assigned_track != track_idx { continue; }

                    let actor = &state.scene.actors[ai];
                    let clip_start = actor.t_in.unwrap_or(0.0);
                    let clip_end = actor.t_out.unwrap_or(duration);
                    let trans_in = actor.transition_in;
                    let trans_out = actor.transition_out;
                    let trans_dur = actor.transition_duration;
                    let sel = state.selection == Selection::Actor(ai);
                    let actor_id = egui::Id::new(("timeline_clip", "actor", ai));
                    if let Some(clicked) = draw_clip(ui, painter, content_rect, &actor.id, actor_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        COL_CLIP_ACTOR, sel, track_h, track_locked, state.split_tool_active)
                    {
                        if clicked == f32::INFINITY {
                            // Trim left edge: adjust t_in
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_in = (clip_start + delta_t).max(0.0).min(clip_end - 0.1);
                            state.scene.actors[ai].t_in = Some(new_in);
                            to_select = Some(Selection::Actor(ai));
                        } else if clicked == f32::NEG_INFINITY {
                            // Trim right edge: adjust t_out
                            let dx = ui.input(|i| i.pointer.delta().x);
                            let delta_t = dx / pps;
                            let new_out = (clip_end + delta_t).max(clip_start + 0.1);
                            state.scene.actors[ai].t_out = Some(new_out);
                            to_select = Some(Selection::Actor(ai));
                        } else if clicked < 0.0 {
                            // Drag: move the actor's time window
                            let mut new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;

                            // ── Undo snapshot on drag start ──
                            if state.timeline_drag.dragging_clip.is_none() {
                                state.undo.push(&state.scene);
                                state.timeline_drag.dragging_clip = Some(ai);
                            }

                            // ── Resolve the destination track from the pointer's Y position ──
                            // Drag inside the timeline viewport → find which track row the
                            // cursor is over. Drag above the topmost video → schedule creation
                            // of a new video track at index 0 (the new top).
                            if let Some(py) = pointer_y {
                                if let Some(target) = resolve_target_track(py) {
                                    if state.tracks[target].kind == TrackKind::Video {
                                        state.actor_track_assignments.insert(ai, target);
                                    }
                                } else {
                                    // Pointer outside any existing row.
                                    let topmost_y = track_rows.first().map(|(t, _)| *t)
                                        .unwrap_or(tracks_rect.min.y);
                                    if py < topmost_y {
                                        pending_new_video_top = Some(ai);
                                    }
                                }
                            } else {
                                state.actor_track_assignments.insert(ai, track_idx);
                            }

                            // ── Snap-to-edges logic ──
                            if state.snap_enabled {
                                let new_end = new_start + dur;
                                let mut snap_targets = collect_clip_edges(state, Some(ai));
                                snap_targets.push(state.playhead);
                                // Pixel-aware snap window: ~3 px on screen so the
                                // clip glides smoothly under the cursor instead of
                                // jumping in 0.1 s (≈ 8 px) chunks.
                                let threshold = (3.0 / state.timeline_zoom.max(1.0)).max(0.001);

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
                        } else if state.split_tool_active {
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
                        painter,
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

                    // Keyframe diamonds on the clip bar — one per layout keyframe.
                    draw_keyframe_diamonds(
                        painter,
                        content_rect,
                        clip_start,
                        clip_end,
                        &state.scene.actors[ai].layout,
                        state.timeline_scroll,
                        pps,
                        track_left,
                        track_right,
                        sel,
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
                        let ov_id = egui::Id::new(("timeline_clip", "overlay", oi));
                        if let Some(clicked) = draw_clip(ui, painter, content_rect, &label, ov_id,
                            clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                            COL_CLIP_OVERLAY, sel, track_h, track_locked, state.split_tool_active)
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
                            } else if state.split_tool_active {
                                to_select = Some(Selection::Overlay(oi));
                                state.playhead = clicked;
                                state.status = "__SPLIT_AT_PLAYHEAD__".into();
                            } else {
                                to_select = Some(Selection::Overlay(oi));
                            }
                        }
                        // Keyframe diamonds for overlays too.
                        let layout_ref: &[Keyframe<OverlayState>] = match &state.scene.overlays[oi] {
                            Overlay::Text(t) => &t.layout,
                            Overlay::Image(im) => &im.layout,
                            Overlay::Video(v) => &v.layout,
                        };
                        draw_keyframe_diamonds(
                            painter,
                            content_rect,
                            clip_start,
                            clip_end,
                            layout_ref,
                            state.timeline_scroll,
                            pps,
                            track_left,
                            track_right,
                            sel,
                        );
                    }
                }
            }
            TrackKind::Audio => {
                let audio_tracks: Vec<usize> = (0..num_tracks).filter(|ti| state.tracks[*ti].kind == TrackKind::Audio).collect();

                for aui in 0..state.scene.audio.len() {
                    // Use explicit assignment if set, otherwise round-robin across audio tracks.
                    let target_track_idx = if let Some(&t) = state.audio_track_assignments.get(&aui) {
                        t
                    } else if audio_tracks.is_empty() {
                        0
                    } else {
                        audio_tracks[aui % audio_tracks.len()]
                    };
                    if target_track_idx != track_idx { continue; }

                    let audio = &state.scene.audio[aui];
                    let clip_start = audio.t_in;
                    let clip_end = audio.t_out.unwrap_or(duration);
                    let sel = state.selection == Selection::Audio(aui);
                    let audio_id = egui::Id::new(("timeline_clip", "audio", aui));
                    if let Some(clicked) = draw_audio_clip(ui, painter, content_rect, &audio.id, audio_id,
                        clip_start, clip_end, state.timeline_scroll, pps, track_left, track_right,
                        sel, track_h, track_locked, state.split_tool_active,
                        state.audio_waveforms.get(aui))
                    {
                        if clicked < 0.0 {
                            // Drag: move the audio clip horizontally.
                            let new_start = (-clicked).max(0.0);
                            let dur = clip_end - clip_start;
                            state.scene.audio[aui].t_in = new_start;
                            state.scene.audio[aui].t_out = Some(new_start + dur);

                            // Vertical: re-assign track based on pointer Y.
                            if let Some(py) = pointer_y {
                                if let Some(target) = resolve_target_track(py) {
                                    if state.tracks[target].kind == TrackKind::Audio {
                                        state.audio_track_assignments.insert(aui, target);
                                    }
                                } else {
                                    let bottommost_y = track_rows.last().map(|(_, b)| *b)
                                        .unwrap_or(tracks_rect.max.y);
                                    if py >= bottommost_y {
                                        pending_new_audio_bottom = Some(aui);
                                    }
                                }
                            }
                        }
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

    // ── Apply pending new-layer creation requests from the drag handlers ──
    // These were stashed inside the loop so we don't invalidate iteration.
    if let Some(actor_idx) = pending_new_video_top {
        // Insert a new video track at index 0 and shift every existing
        // assignment up by 1 so they keep referring to the same physical row.
        let n = state.tracks.iter().filter(|t| t.kind == TrackKind::Video).count() + 1;
        state.tracks.insert(0, crate::state::Track::video(format!("V{}", n)));
        let new_assignments: std::collections::HashMap<usize, usize> = state
            .actor_track_assignments
            .iter()
            .map(|(k, v)| (*k, *v + 1))
            .collect();
        state.actor_track_assignments = new_assignments;
        let new_audio: std::collections::HashMap<usize, usize> = state
            .audio_track_assignments
            .iter()
            .map(|(k, v)| (*k, *v + 1))
            .collect();
        state.audio_track_assignments = new_audio;
        state.actor_track_assignments.insert(actor_idx, 0);
        state.status = format!("\u{2728} New video layer created on top: V{}", n);
    }
    if let Some(audio_idx) = pending_new_audio_bottom {
        let n = state.tracks.iter().filter(|t| t.kind == TrackKind::Audio).count() + 1;
        state.tracks.push(crate::state::Track::audio(format!("A{}", n)));
        let new_track_idx = state.tracks.len() - 1;
        state.audio_track_assignments.insert(audio_idx, new_track_idx);
        state.status = format!("\u{2728} New audio layer created at bottom: A{}", n);
    }

    // Empty state
    if state.scene.actors.is_empty() && state.scene.overlays.is_empty()
        && state.scene.backgrounds.is_empty() && state.scene.audio.is_empty() {
        tracks_painter.text(
            tracks_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drag clips from the library to add them here",
            egui::FontId::proportional(12.0),
            COL_TEXT_DIM,
        );
    }

    // ── Custom scrollbars ──
    // Horizontal scrollbar drives both pan (timeline_scroll in seconds) and
    // local zoom (timeline_zoom in pixels-per-second). The thumb edges can be
    // dragged to resize the visible window.
    let visible_secs_h = (track_area_width / pps.max(1.0)).max(0.0);
    let total_h = duration.max(visible_secs_h.max(0.5));
    let view_a_h = (state.timeline_scroll / total_h).clamp(0.0, 1.0);
    let view_b_h = ((state.timeline_scroll + visible_secs_h) / total_h).clamp(view_a_h, 1.0);
    let (new_a_h, new_b_h) = stretchable_scrollbar(
        ui,
        h_sb_rect,
        true, // horizontal
        view_a_h,
        view_b_h,
    );
    {
        let new_window_secs = ((new_b_h - new_a_h) * total_h).max(0.05);
        state.timeline_scroll = (new_a_h * total_h).max(0.0);
        state.timeline_zoom = (track_area_width / new_window_secs).clamp(2.0, 2000.0);
    }

    // Vertical scrollbar drives both pan (timeline_v_scroll in pixels) and
    // local zoom (timeline_v_zoom multiplier on track heights).
    let total_unscaled_h: f32 = state.tracks.iter().map(|t| t.height).sum::<f32>().max(1.0);
    let total_v = (total_unscaled_h * v_zoom).max(viewport_h);
    let view_a_v = (state.timeline_v_scroll / total_v).clamp(0.0, 1.0);
    let view_b_v = ((state.timeline_v_scroll + viewport_h) / total_v).clamp(view_a_v, 1.0);
    let (new_a_v, new_b_v) = stretchable_scrollbar(
        ui,
        v_sb_rect,
        false, // vertical
        view_a_v,
        view_b_v,
    );
    {
        let new_window_pixels = ((new_b_v - new_a_v) * total_v).max(20.0);
        // viewport_h must equal v_zoom * total_unscaled_h * (new_b_v - new_a_v)
        // → v_zoom = viewport_h / (total_unscaled_h * (new_b_v - new_a_v))
        let denom = (total_unscaled_h * (new_b_v - new_a_v)).max(0.0001);
        let new_v_zoom = (viewport_h / denom).clamp(0.25, 8.0);
        state.timeline_v_zoom = new_v_zoom;
        // Recompute v_scroll using the NEW total height (so position stays consistent).
        let new_total_v = (total_unscaled_h * new_v_zoom).max(viewport_h);
        state.timeline_v_scroll = (new_a_v * new_total_v).max(0.0);
        let _ = new_window_pixels; // silence unused
    }

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
                            pos: [0.5, 0.5], scale: 0.3, scale_y: 1.0, rotation_deg: 0.0, opacity: 1.0
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


/// Stretchable scrollbar widget. Shared between the horizontal time scrollbar
/// and the vertical track scrollbar.
///
/// `view_a_frac`/`view_b_frac` are the start/end of the visible window as
/// fractions of the total content (both in [0, 1], `a <= b`).
///
/// Returns the new (a, b) after user interaction this frame:
///   - dragging the thumb body pans (a and b shift by the same amount);
///   - dragging the thumb's leading/trailing edge resizes the visible window
///     (this is the "local zoom" the user controls by stretching the scrollbar).
fn stretchable_scrollbar(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    horizontal: bool,
    view_a_frac: f32,
    view_b_frac: f32,
) -> (f32, f32) {
    let painter = ui.painter_at(rect);

    // Scrollbar track background.
    let bg_rounding = if horizontal { rect.height() * 0.5 } else { rect.width() * 0.5 };
    painter.rect_filled(rect, Rounding::same(bg_rounding), Color32::from_rgb(20, 20, 30));

    let track_len = if horizontal { rect.width() } else { rect.height() }.max(1.0);
    let cross = if horizontal { rect.height() } else { rect.width() };
    let edge_zone = (cross * 0.6).max(6.0);
    let min_window_frac = (10.0 / track_len).min(0.5);

    let a = view_a_frac.clamp(0.0, 1.0);
    let b = view_b_frac.clamp(a + min_window_frac.min(0.001), 1.0);

    let id = ui.make_persistent_id((
        "scrollbar",
        rect.min.x as i32,
        rect.min.y as i32,
        horizontal,
    ));
    let resp = ui.interact(rect, id, Sense::click_and_drag());

    // Compute thumb pixel range along the primary axis.
    let main_min = if horizontal { rect.min.x } else { rect.min.y };
    let thumb_l = main_min + a * track_len;
    let thumb_r = main_min + b * track_len;

    // Persist drag mode across frames.
    let mode_key = id.with("mode");
    #[derive(Clone, Copy, PartialEq)]
    enum Mode { None, Pan, ResizeStart, ResizeEnd }
    let stored_raw: Option<u8> = ui.ctx().memory(|m| m.data.get_temp(mode_key));
    let mut mode = match stored_raw {
        Some(0) => Mode::Pan,
        Some(1) => Mode::ResizeStart,
        Some(2) => Mode::ResizeEnd,
        _ => Mode::None,
    };

    if resp.drag_started() {
        if let Some(p) = resp.interact_pointer_pos() {
            let coord = if horizontal { p.x } else { p.y };
            mode = if (coord - thumb_l).abs() < edge_zone {
                Mode::ResizeStart
            } else if (coord - thumb_r).abs() < edge_zone {
                Mode::ResizeEnd
            } else {
                Mode::Pan
            };
            let raw: u8 = match mode {
                Mode::Pan => 0,
                Mode::ResizeStart => 1,
                Mode::ResizeEnd => 2,
                Mode::None => 3,
            };
            ui.ctx().memory_mut(|m| m.data.insert_temp(mode_key, raw));
        }
    }
    if !resp.dragged() && !resp.drag_started() {
        mode = Mode::None;
        ui.ctx().memory_mut(|m| m.data.insert_temp(mode_key, 3u8));
    }

    // Hover cursor hint.
    if resp.hovered() {
        if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
            let coord = if horizontal { p.x } else { p.y };
            if (coord - thumb_l).abs() < edge_zone || (coord - thumb_r).abs() < edge_zone {
                ui.ctx().set_cursor_icon(if horizontal {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                });
            } else if coord >= thumb_l && coord <= thumb_r {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
    }

    let mut new_a = a;
    let mut new_b = b;

    if resp.dragged() {
        let d_pixels = if horizontal {
            resp.drag_delta().x
        } else {
            resp.drag_delta().y
        };
        let d_frac = d_pixels / track_len;
        match mode {
            Mode::Pan => {
                let w = b - a;
                new_a = (a + d_frac).clamp(0.0, (1.0 - w).max(0.0));
                new_b = new_a + w;
            }
            Mode::ResizeStart => {
                new_a = (a + d_frac).clamp(0.0, (b - min_window_frac).max(0.0));
            }
            Mode::ResizeEnd => {
                new_b = (b + d_frac).clamp((a + min_window_frac).min(1.0), 1.0);
            }
            Mode::None => {}
        }
    } else if resp.clicked() {
        // Click on track outside thumb: jump (centre thumb at click).
        if let Some(p) = resp.interact_pointer_pos() {
            let coord = if horizontal { p.x } else { p.y };
            let frac = ((coord - main_min) / track_len).clamp(0.0, 1.0);
            let w = b - a;
            new_a = (frac - w * 0.5).clamp(0.0, (1.0 - w).max(0.0));
            new_b = new_a + w;
        }
    }

    // Draw thumb at the (possibly updated) position.
    let display_a = new_a;
    let display_b = new_b.max(new_a + (10.0 / track_len).min(0.999));
    let main_l = main_min + display_a * track_len;
    let main_r = (main_min + display_b * track_len).min(if horizontal { rect.max.x } else { rect.max.y });

    let thumb_rect = if horizontal {
        egui::Rect::from_min_max(
            egui::pos2(main_l, rect.min.y + 2.0),
            egui::pos2(main_r, rect.max.y - 2.0),
        )
    } else {
        egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, main_l),
            egui::pos2(rect.max.x - 2.0, main_r),
        )
    };
    let thumb_color = if resp.hovered() || resp.dragged() {
        Color32::from_rgb(140, 140, 180)
    } else {
        Color32::from_rgb(90, 90, 130)
    };
    painter.rect_filled(thumb_rect, Rounding::same(bg_rounding * 0.8), thumb_color);

    // Draw the two stretch grips inside the thumb edges.
    let grip_color = Color32::from_rgba_premultiplied(255, 255, 255, 90);
    if horizontal {
        let grip_w = 2.0;
        let inset_x = 3.0;
        let y0 = thumb_rect.min.y + 2.0;
        let y1 = thumb_rect.max.y - 2.0;
        if thumb_rect.width() > 14.0 {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(thumb_rect.min.x + inset_x, y0),
                    egui::pos2(thumb_rect.min.x + inset_x + grip_w, y1),
                ),
                Rounding::ZERO, grip_color,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(thumb_rect.max.x - inset_x - grip_w, y0),
                    egui::pos2(thumb_rect.max.x - inset_x, y1),
                ),
                Rounding::ZERO, grip_color,
            );
        }
    } else {
        let grip_h = 2.0;
        let inset_y = 3.0;
        let x0 = thumb_rect.min.x + 2.0;
        let x1 = thumb_rect.max.x - 2.0;
        if thumb_rect.height() > 14.0 {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, thumb_rect.min.y + inset_y),
                    egui::pos2(x1, thumb_rect.min.y + inset_y + grip_h),
                ),
                Rounding::ZERO, grip_color,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, thumb_rect.max.y - inset_y - grip_h),
                    egui::pos2(x1, thumb_rect.max.y - inset_y),
                ),
                Rounding::ZERO, grip_color,
            );
        }
    }

    (new_a, new_b)
}


/// Draw a single clip bar on the timeline. Returns Some(time) if clicked (for split or select).
/// Returns special sentinel values for edge-trim drags:
/// - `f32::INFINITY` signals "trim left edge"
/// - `f32::NEG_INFINITY` signals "trim right edge"
/// - Negative values signal whole-clip drag (new start time encoded as `-new_start`)
/// Shows ResizeHorizontal cursor when hovering within 5px of left/right edge.
///
/// `clip_id` MUST be stable across frames for the same clip (do not include the
/// clip's time in the id, or egui's drag tracking breaks the moment the clip
/// position changes — the user has to release and re-click on every frame).
#[allow(clippy::too_many_arguments)]
fn draw_clip(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    content_rect: egui::Rect,
    label: &str,
    clip_id: egui::Id,
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
    split_mode: bool,
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

    // Interaction (click/drag with edge-trim zones).
    //
    // The id MUST be stable across frames for the same clip. Hashing the
    // clip's time into the id (as we used to) caused the id to change every
    // frame during a drag — egui then dropped its drag state and the user
    // had to release and re-press for each pixel of motion. The caller now
    // supplies a stable per-clip id.
    let id = clip_id;
    let sense = if locked { Sense::hover() } else { Sense::click_and_drag() };
    let resp = ui.interact(bar_rect, id, sense);

    // Edge detection for hover cursor (purely cosmetic; the actual drag mode
    // is captured once at drag_started below and locked for the rest of the
    // drag, so the cursor flicker doesn't affect behaviour).
    let hover_pos = ui.input(|i| i.pointer.hover_pos());

    let near_left_edge = hover_pos.map(|p| (p.x - bar_rect.min.x).abs() < 5.0).unwrap_or(false);
    let near_right_edge = hover_pos.map(|p| (p.x - bar_rect.max.x).abs() < 5.0).unwrap_or(false);

    if resp.hovered() && !locked {
        if split_mode {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        } else if near_left_edge || near_right_edge {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    if resp.clicked() {
        if split_mode {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = x_to_time(pos.x, scroll, pps, track_left);
                return Some(t);
            }
        }
        return Some(clip_start); // signal selection
    }

    // ── Drag handling ────────────────────────────────────────────────
    //
    // Strategy:
    //   * On `drag_started`, freeze the drag mode (Move / TrimLeft /
    //     TrimRight) and snapshot the clip's original start time and the
    //     pointer's press origin. We stash these in egui's per-id temp
    //     memory so they survive across frames.
    //   * On every subsequent `dragged` frame, recompute the proposed new
    //     position from the *total* pointer displacement since press origin,
    //     not from per-frame deltas applied on top of an already-mutated
    //     value. This avoids feedback loops with snapping (where a snapped
    //     position would re-feed itself and cause jitter or sticking) and
    //     keeps motion 1:1 with the cursor.
    let mode_id = id.with("drag_mode");
    let origin_id = id.with("press_origin_x");
    let original_start_id = id.with("original_start");

    if resp.drag_started() && !locked && !split_mode {
        let press_x = ui
            .input(|i| i.pointer.press_origin())
            .map(|p| p.x)
            .unwrap_or(bar_rect.center().x);
        let mode = if (press_x - bar_rect.min.x).abs() < 6.0 {
            ClipDragMode::TrimLeft
        } else if (press_x - bar_rect.max.x).abs() < 6.0 {
            ClipDragMode::TrimRight
        } else {
            ClipDragMode::Move
        };
        ui.data_mut(|d| {
            d.insert_temp(mode_id, mode);
            d.insert_temp(origin_id, press_x);
            d.insert_temp(original_start_id, clip_start);
        });
    }

    if resp.dragged() && !locked && !split_mode {
        let mode: Option<ClipDragMode> = ui.data(|d| d.get_temp(mode_id));
        let press_x: Option<f32> = ui.data(|d| d.get_temp(origin_id));
        let original_start: Option<f32> = ui.data(|d| d.get_temp(original_start_id));
        let cur_x = ui
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
            .map(|p| p.x);

        if let (Some(mode), Some(px), Some(os), Some(cx)) =
            (mode, press_x, original_start, cur_x)
        {
            let total_dx = cx - px;
            match mode {
                ClipDragMode::TrimLeft => return Some(f32::INFINITY),
                ClipDragMode::TrimRight => return Some(f32::NEG_INFINITY),
                ClipDragMode::Move => {
                    let total_dt = total_dx / pps;
                    return Some(-(os + total_dt));
                }
            }
        }
        return Some(clip_start); // fall back to bare select
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
    clip_id: egui::Id,
    clip_start: f32,
    clip_end: f32,
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    selected: bool,
    _track_h: f32,
    locked: bool,
    _split_mode: bool,
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

    // Draw waveform or fallback visualization
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
        } else if wf.extracting {
            // Show "loading" state
            painter.text(
                bar_rect.center(), egui::Align2::CENTER_CENTER,
                "Loading...", egui::FontId::proportional(9.0),
                Color32::from_rgba_premultiplied(255, 255, 255, 100));
        } else {
            // Not started yet - show placeholder bars
            draw_placeholder_waveform(painter, bar_rect);
        }
    } else {
        // No waveform object yet - show placeholder
        draw_placeholder_waveform(painter, bar_rect);
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

    // Interaction.
    //
    // Same stable-id + press-origin strategy as `draw_clip` — see the
    // comment there for the rationale. Audio clips currently support only
    // whole-clip move (no edge-trim) but use the same machinery so that
    // future trim support is a one-liner.
    let id = clip_id;
    let sense = if locked { Sense::hover() } else { Sense::click_and_drag() };
    let resp = ui.interact(bar_rect, id, sense);

    if resp.hovered() && !locked {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }

    if resp.clicked() { return Some(clip_start); }

    let origin_id = id.with("press_origin_x");
    let original_start_id = id.with("original_start");

    if resp.drag_started() && !locked {
        let press_x = ui
            .input(|i| i.pointer.press_origin())
            .map(|p| p.x)
            .unwrap_or(bar_rect.center().x);
        ui.data_mut(|d| {
            d.insert_temp(origin_id, press_x);
            d.insert_temp(original_start_id, clip_start);
        });
    }

    if resp.dragged() && !locked {
        let press_x: Option<f32> = ui.data(|d| d.get_temp(origin_id));
        let original_start: Option<f32> = ui.data(|d| d.get_temp(original_start_id));
        let cur_x = ui
            .input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()))
            .map(|p| p.x);

        if let (Some(px), Some(os), Some(cx)) = (press_x, original_start, cur_x) {
            let total_dt = (cx - px) / pps;
            return Some(-(os + total_dt));
        }
        return Some(clip_start);
    }

    None
}


/// Draw placeholder waveform bars for audio clips that haven't been analyzed yet.
fn draw_placeholder_waveform(painter: &egui::Painter, bar_rect: egui::Rect) {
    let bar_w = bar_rect.width();
    let bar_h = bar_rect.height();
    let center_y = bar_rect.center().y;
    let num_bars = ((bar_w / 4.0) as usize).max(3).min(50);
    for i in 0..num_bars {
        let x = bar_rect.min.x + (i as f32 / num_bars as f32) * bar_w + 2.0;
        let h = bar_h * 0.15 * (1.0 + ((i as f32 * 0.7).sin() * 0.5));
        painter.line_segment(
            [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
            Stroke::new(1.5, Color32::from_rgba_premultiplied(180, 220, 220, 80)),
        );
    }
}


/// Draw small diamond markers on a clip bar, one per layout keyframe.
/// Keyframe times are LOCAL (relative to clip's t_in). Used for the
/// "visual constructor of animations" — gives users an at-a-glance view
/// of when in the clip's timeline parameter changes happen.
#[allow(clippy::too_many_arguments)]
fn draw_keyframe_diamonds<T>(
    painter: &egui::Painter,
    content_rect: egui::Rect,
    clip_start: f32,
    clip_end: f32,
    layout: &[Keyframe<T>],
    scroll: f32,
    pps: f32,
    track_left: f32,
    track_right: f32,
    selected: bool,
) {
    if layout.is_empty() { return; }
    // If there's only a single static keyframe, no need to draw anything —
    // the clip is non-animated.
    if layout.len() == 1 { return; }

    let bar_y_top = content_rect.min.y + 2.0;
    let bar_y_bot = content_rect.max.y - 2.0;
    let cy = (bar_y_top + bar_y_bot) * 0.5;
    let half = 4.0_f32;
    let fill = if selected {
        Color32::from_rgb(255, 230, 80)
    } else {
        Color32::from_rgb(200, 200, 255)
    };
    let stroke = Color32::from_rgb(20, 20, 30);

    for kf in layout {
        let abs_t = clip_start + kf.t;
        if abs_t < clip_start - 0.001 || abs_t > clip_end + 0.001 { continue; }
        let x = (abs_t - scroll) * pps + track_left;
        if x < track_left - half || x > track_right + half { continue; }

        // Diamond shape (45-degree rotated square).
        let pts = vec![
            egui::pos2(x, cy - half),
            egui::pos2(x + half, cy),
            egui::pos2(x, cy + half),
            egui::pos2(x - half, cy),
        ];
        painter.add(egui::Shape::convex_polygon(pts, fill, Stroke::new(1.0, stroke)));
    }
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

                    // Use native texture aspect ratio to prevent distortion
                    let tex_size = tex.size_vec2();
                    let tex_aspect = tex_size.x / tex_size.y;
                    // Scale relative to preview rect, preserving source aspect ratio
                    let actor_h = rect.height() * ascale * 0.5;
                    let actor_w = actor_h * tex_aspect;
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

                // Use source aspect ratio for gizmo too
                let tex_aspect = if let Some(fc) = state.frame_caches.get(sel_idx) {
                    if fc.is_ready() && fc.source_width > 0 && fc.source_height > 0 {
                        fc.source_width as f32 / fc.source_height as f32
                    } else { 9.0 / 16.0 }
                } else { 9.0 / 16.0 };
                let actor_h = rect.height() * ascale * 0.5;
                let actor_w = actor_h * tex_aspect;
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
        ui.put(rect, egui::Label::new(
            RichText::new("Preview\n\nAdd clips and hit play").color(Color32::from_rgb(60, 60, 80)).size(14.0)));
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

fn color_edit_u8(ui: &mut egui::Ui, c: &mut [u8; 3]) -> bool {
    let mut rgb = [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0];
    if ui.color_edit_button_rgb(&mut rgb).changed() {
        c[0] = (rgb[0] * 255.0).round() as u8;
        c[1] = (rgb[1] * 255.0).round() as u8;
        c[2] = (rgb[2] * 255.0).round() as u8;
        true
    } else {
        false
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
    let t = state.playhead;
    add_actor_from_clip_at_time(state, path, t);
}

/// Load chroma sidecar for `path`, falling back to default when absent.
fn load_chroma_for_clip(path: &PathBuf) -> ChromaKeyParams {
    ChromaKeyParams::load_for_clip(path).unwrap_or_default()
}

/// Push an `AudioTrack` matching `actor` so the embedded audio shows up as
/// its own row on the audio lanes. Returns the new index.
fn push_audio_track_for_actor(state: &mut EditorState, actor_id: &str, source: &PathBuf,
                              t_in: f32, t_out: Option<f32>, source_start: f32) -> usize {
    let id = format!("{}_audio", actor_id);
    state.scene.audio.push(AudioTrack {
        id,
        source: source.clone(),
        t_in,
        t_out,
        source_start,
        volume: 1.0,
    });
    state.scene.audio.len() - 1
}

/// Auto-attach a skeleton template for `path` if a sidecar exists and we
/// haven't already loaded it into the scene.
fn ensure_skeleton_template_for_clip(state: &mut EditorState, path: &PathBuf) {
    let already = state.scene.skeleton_templates.iter()
        .any(|t| t.source_clip == *path);
    if already { return; }
    if let Some(template) = SkeletonTemplate::load_for_clip(path) {
        state.scene.skeleton_templates.push(template);
    }
}

/// Add a styled text overlay at the playhead and select it.
/// Returns the index of the new overlay.
pub fn add_text_overlay(state: &mut EditorState) -> usize {
    let counter = state.scene.overlays.len() + 1;
    let id = format!("text_{}", counter);
    let t_in = state.playhead;
    let t_out = (t_in + 3.0).min(state.scene.output.duration.max(t_in + 0.1));

    let max_z = state.scene.overlays.iter().filter_map(|o| match o {
        Overlay::Text(t) => Some(t.z_index),
        _ => None,
    }).max().unwrap_or(99);

    let style = TextStyle {
        font: "DejaVuSans".into(),
        font_size: 96.0,
        color: [255, 255, 255],
        box_color: Some([0, 0, 0]),
        box_padding: 24.0,
        bold: true,
        italic: false,
        outline: Some([0, 0, 0]),
        outline_width: 4.0,
        align: TextAlign::Center,
        box_kind: TextBoxKind::Solid,
        box_corner_radius: 12.0,
        box_opacity: 0.85,
        box_gradient_end: None,
        box_outline_color: None,
        box_outline_width: 0.0,
    };

    let overlay = Overlay::Text(TextOverlay {
        id: id.clone(),
        text: "Text".into(),
        t_in,
        t_out,
        style,
        layout: vec![Keyframe::new(0.0, OverlayState {
            pos: [0.5, 0.5],
            scale: 1.0,
            scale_y: 1.0,
            rotation_deg: 0.0,
            opacity: 1.0,
        })],
        z_index: max_z + 1,
        behind_actors: false,
    });

    state.scene.overlays.push(overlay);
    let idx = state.scene.overlays.len() - 1;
    state.selection = Selection::Overlay(idx);
    state.status = format!("Added text: {}", id);
    idx
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
    let counter = state.scene.actors.len() + 1;
    let id = path.file_stem().and_then(|s| s.to_str())
        .map(|s| format!("{}_{}", s, counter))
        .unwrap_or_else(|| format!("actor_{}", counter));

    let clip_duration = probe_video_duration(path);
    // The timeline auto-grows to fit content (see timeline()'s auto-length
    // pass), so don't clamp the right edge to the current `output.duration`.
    let t_in = t.max(0.0);
    let t_out = t_in + clip_duration.max(0.1);

    // Per-clip chroma settings live next to the source file (`<clip>.chroma.json`).
    // This is independent of the project, so re-using the same Mellstroy clip
    // in another scene starts pre-tuned.
    let chroma = load_chroma_for_clip(path);
    // Likewise, auto-attach a skeleton template if one was saved for this clip.
    ensure_skeleton_template_for_clip(state, path);

    let actor = Actor {
        id: id.clone(),
        source: path.clone(),
        anchors: None,
        chroma_key: chroma,
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: Some(t_in),
        t_out: Some(t_out),
        source_start: 0.0,
        loop_source: false,
        flip_horizontal: false,
        attachments: Vec::new(),
        skeleton_attachments: Vec::new(),
        visible: true,
        color_correction: ColorCorrection::default(),
        transition_in: Transition::Cut,
        transition_out: Transition::Cut,
        transition_duration: 0.3,
    };
    state.scene.actors.push(actor);
    let new_actor_idx = state.scene.actors.len() - 1;

    // Also push an AudioTrack referencing the same source so the embedded
    // audio appears as its own row on the audio lanes (and gains a waveform,
    // volume slider, etc. on the inspector).
    push_audio_track_for_actor(state, &id, path, t_in, Some(t_out), 0.0);

    state.selection = Selection::Actor(new_actor_idx);
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
