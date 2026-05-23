//! **Image editor** — focused floating panel for `Overlay::Image` rows.
//!
//! Replaces the old (video-only) `clip_editor` window. The intent is
//! to expose the editing operations that **only make sense for static
//! images** — cropping, quick colour adjustments, classic filter
//! presets — without the in/out-point trimmer or pose-detection UI
//! that the timeline / inspector cover for video clips.
//!
//! The window operates on whichever `Overlay::Image` is currently
//! selected. Every control mutates the overlay's `effects` stack so
//! the result composes cleanly with the rest of the editor (preview
//! cache, ffmpeg renderer, save/load round-trip).

use egui::{Color32, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::{Effect, EffectKind, Overlay};

use crate::state::EditorState;

/// Draw the floating "Image Editor" window. Returns `true` if the
/// window is still open after this frame.
pub fn image_editor_window(ctx: &egui::Context, state: &mut EditorState) -> bool {
    let mut open = true;
    egui::Window::new(format!("\u{1F5BC} {}", crate::i18n::t("Image Editor")))
        .open(&mut open)
        .default_size([460.0, 520.0])
        .min_width(360.0)
        .min_height(320.0)
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            image_editor_content(ui, state);
        });
    open
}

fn image_editor_content(ui: &mut egui::Ui, state: &mut EditorState) {
    // Resolve the currently-selected image overlay; bail out with a
    // friendly hint when the selection is anything else (text, video,
    // actor, …) so the editor doesn't pretend to be applicable.
    let overlay_idx = match state.selection {
        crate::state::Selection::Overlay(i) if i < state.scene.overlays.len() => {
            match &state.scene.overlays[i] {
                Overlay::Image(_) => Some(i),
                _ => None,
            }
        }
        _ => None,
    };

    let Some(i) = overlay_idx else {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(crate::i18n::t("Select an image overlay to edit it."))
                    .size(12.0)
                    .italics()
                    .color(Color32::from_rgb(150, 150, 170)),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(crate::i18n::t(
                    "This panel exposes image-only editing tools (crop, quick colour, filters).",
                ))
                .size(10.5)
                .color(Color32::from_rgb(120, 120, 150)),
            );
        });
        return;
    };

    // Snapshot the bits we need before borrowing the overlay mutably
    // for control rendering.
    let (id, source) = match &state.scene.overlays[i] {
        Overlay::Image(im) => (im.id.clone(), im.source.clone()),
        _ => unreachable!(),
    };

    // ── Header ──
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{}: {}", crate::i18n::t("Image"), id))
                .strong()
                .size(13.0)
                .color(Color32::from_rgb(220, 130, 200)),
        );
    });
    ui.separator();
    ui.add_space(2.0);

    // ── Preview ── try to show the loaded texture; fall back to a
    // grey placeholder + hint when the cache hasn't decoded the file
    // yet (decoding happens on the canvas paint, not here).
    let preview_h = 180.0_f32;
    let preview_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(preview_w, preview_h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(14, 14, 22));

    let mut painted = false;
    if let Ok(map) = state.image_textures.lock() {
        if let Some(crate::state::ImageTextureSlot::Loaded { texture, size }) = map.get(&source) {
            let ow = size[0] as f32;
            let oh = size[1] as f32;
            if ow > 0.0 && oh > 0.0 {
                let aspect = ow / oh;
                let pad = 6.0_f32;
                let max_w = preview_w - 2.0 * pad;
                let max_h = preview_h - 2.0 * pad;
                let mut display_w = max_w;
                let mut display_h = display_w / aspect;
                if display_h > max_h {
                    display_h = max_h;
                    display_w = display_h * aspect;
                }
                let off_x = (preview_w - display_w) * 0.5;
                let off_y = (preview_h - display_h) * 0.5;
                let img_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.min.x + off_x, rect.min.y + off_y),
                    Vec2::new(display_w, display_h),
                );
                let mut mesh = egui::Mesh::with_texture(texture.id());
                mesh.add_rect_with_uv(
                    img_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                painter.add(egui::Shape::mesh(mesh));
                painter.rect_stroke(
                    img_rect,
                    Rounding::same(3.0),
                    Stroke::new(1.0, Color32::from_rgb(60, 50, 70)),
                );
                painted = true;
            }
        }
    }
    if !painted {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            crate::i18n::t("Preview will appear after canvas renders the image once."),
            egui::FontId::proportional(11.0),
            Color32::from_rgb(120, 120, 140),
        );
    }
    ui.add_space(6.0);

    // ── Source info row ──
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(crate::i18n::t("Source:"))
                .size(10.5)
                .color(Color32::from_rgb(140, 140, 160)),
        );
        let label = source
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)");
        ui.label(RichText::new(label).size(10.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let fmt = source
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("?")
                .to_uppercase();
            ui.label(
                RichText::new(fmt)
                    .size(10.0)
                    .color(Color32::from_rgb(180, 140, 220)),
            );
        });
    });
    ui.add_space(6.0);
    ui.separator();

    // From here on we mutate the overlay's effects stack. Pull a mut
    // ref directly so each section can push / read effects without
    // re-borrowing.
    let effects: &mut Vec<Effect> = match &mut state.scene.overlays[i] {
        Overlay::Image(im) => &mut im.effects,
        _ => unreachable!(),
    };

    // ── Quick colour adjustments ──
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Quick colour"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(220, 180, 100)),
    )
    .id_source(("image_editor_color", i))
    .default_open(true)
    .show(ui, |ui| {
        adjust_slider(ui, effects, "Brightness", -1.0..=1.0, |k| {
            matches!(k, EffectKind::Brightness { .. })
        }, |amt| EffectKind::Brightness { amount: amt }, |k| match k {
            EffectKind::Brightness { amount } => Some(*amount),
            _ => None,
        });
        adjust_slider(ui, effects, "Contrast", -1.0..=1.0, |k| {
            matches!(k, EffectKind::Contrast { .. })
        }, |amt| EffectKind::Contrast { amount: amt }, |k| match k {
            EffectKind::Contrast { amount } => Some(*amount),
            _ => None,
        });
        adjust_slider(ui, effects, "Saturation", -1.0..=1.0, |k| {
            matches!(k, EffectKind::Saturation { .. })
        }, |amt| EffectKind::Saturation { amount: amt }, |k| match k {
            EffectKind::Saturation { amount } => Some(*amount),
            _ => None,
        });
        adjust_slider(ui, effects, "Hue \u{00B0}", -180.0..=180.0, |k| {
            matches!(k, EffectKind::HueShift { .. })
        }, |deg| EffectKind::HueShift { degrees: deg }, |k| match k {
            EffectKind::HueShift { degrees } => Some(*degrees),
            _ => None,
        });
    });

    // ── Crop ──
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Crop"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(140, 200, 255)),
    )
    .id_source(("image_editor_crop", i))
    .default_open(false)
    .show(ui, |ui| {
        // Find or create the single Crop effect.
        let crop_idx = effects
            .iter()
            .position(|e| matches!(e.kind, EffectKind::Crop { .. }));
        let mut left = 0.0_f32;
        let mut top = 0.0_f32;
        let mut right = 0.0_f32;
        let mut bottom = 0.0_f32;
        if let Some(idx) = crop_idx {
            if let EffectKind::Crop { left: l, top: t, right: r, bottom: b } = effects[idx].kind {
                left = l; top = t; right = r; bottom = b;
            }
        }
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Left"));
            changed |= ui.add(egui::Slider::new(&mut left, 0.0..=0.49)).changed();
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Top"));
            changed |= ui.add(egui::Slider::new(&mut top, 0.0..=0.49)).changed();
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Right"));
            changed |= ui.add(egui::Slider::new(&mut right, 0.0..=0.49)).changed();
        });
        ui.horizontal(|ui| {
            ui.label(crate::i18n::t("Bottom"));
            changed |= ui.add(egui::Slider::new(&mut bottom, 0.0..=0.49)).changed();
        });
        if changed {
            let kind = EffectKind::Crop { left, top, right, bottom };
            match crop_idx {
                Some(idx) => effects[idx].kind = kind,
                None => effects.push(Effect::new(kind)),
            }
        }
        if ui.button(crate::i18n::t("Reset crop")).clicked() {
            if let Some(idx) = crop_idx {
                effects.remove(idx);
            }
        }
    });

    // ── Quick filter presets ──
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Filters"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(180, 240, 180)),
    )
    .id_source(("image_editor_filters", i))
    .default_open(false)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            filter_button(ui, effects, "Grayscale", EffectKind::Grayscale);
            filter_button(ui, effects, "Sepia", EffectKind::Sepia);
            filter_button(ui, effects, "Invert", EffectKind::Invert);
            filter_button(ui, effects, "Vignette", EffectKind::Vignette { strength: 0.5 });
            filter_button(ui, effects, "Blur", EffectKind::Blur { radius: 6.0 });
            filter_button(ui, effects, "Sharpen", EffectKind::Sharpen { amount: 1.0 });
            filter_button(ui, effects, "Glow", EffectKind::Glow { radius: 12.0, intensity: 0.6 });
            filter_button(ui, effects, "Noise", EffectKind::Noise { amount: 0.2 });
        });
        ui.add_space(4.0);
        if ui
            .button(
                RichText::new(crate::i18n::t("Clear all effects"))
                    .color(Color32::from_rgb(255, 140, 140)),
            )
            .clicked()
        {
            effects.clear();
        }
    });
}

/// Render one labelled slider that mirrors a "single-instance" effect
/// in the stack: dragging the slider keeps exactly one matching
/// effect entry; sliding to the neutral value (0.0) removes it. The
/// closure pair reads the current value and constructs the new
/// `EffectKind` from a fresh value, keeping the helper agnostic to
/// the variant's tuple shape.
fn adjust_slider<F, R, M>(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    label: &'static str,
    range: std::ops::RangeInclusive<f32>,
    matches_fn: M,
    make: F,
    read: R,
) where
    F: Fn(f32) -> EffectKind,
    R: Fn(&EffectKind) -> Option<f32>,
    M: Fn(&EffectKind) -> bool,
{
    let idx = effects.iter().position(|e| matches_fn(&e.kind));
    let mut value = idx
        .and_then(|i| read(&effects[i].kind))
        .unwrap_or(0.0);
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t(label));
        // Reserve a comfortable slider width even when the panel is
        // narrow so the user has room to drag instead of pixel-pecking.
        let avail = ui.available_width();
        let r = ui.add_sized(
            egui::vec2((avail - 56.0).max(120.0), 18.0),
            egui::Slider::new(&mut value, range.clone()),
        );
        if r.changed() {
            // Treat values within +/- 0.001 of zero as "remove" so the
            // user can clear an adjustment by dragging back to centre.
            let neutral = value.abs() < 0.001;
            match (idx, neutral) {
                (Some(i), true) => {
                    effects.remove(i);
                }
                (Some(i), false) => {
                    effects[i].kind = make(value);
                }
                (None, false) => {
                    effects.push(Effect::new(make(value)));
                }
                (None, true) => { /* no-op */ }
            }
        }
        if ui.small_button("\u{21BA}").on_hover_text("Reset").clicked() {
            if let Some(i) = idx {
                effects.remove(i);
            }
        }
    });
}

/// Toggle a filter preset on/off. If an effect of the same variant
/// already exists, the click removes it; otherwise it appends a fresh
/// entry with sensible defaults.
fn filter_button(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    label: &'static str,
    template: EffectKind,
) {
    let idx = effects
        .iter()
        .position(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(&template));
    let active = idx.is_some();
    let btn = egui::Button::new(
        RichText::new(crate::i18n::t(label))
            .size(11.0)
            .color(if active { Color32::WHITE } else { Color32::from_rgb(220, 220, 240) }),
    )
    .fill(if active {
        Color32::from_rgb(120, 90, 180)
    } else {
        Color32::from_rgb(40, 40, 56)
    })
    .stroke(Stroke::new(1.0, Color32::from_rgb(80, 70, 100)))
    .rounding(Rounding::same(4.0));
    if ui.add(btn).clicked() {
        match idx {
            Some(i) => {
                effects.remove(i);
            }
            None => {
                effects.push(Effect::new(template));
            }
        }
    }
}
