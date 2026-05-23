//! **Image editor** — focused floating panel for `Overlay::Image` rows.
//!
//! Replaces the old (video-only) `clip_editor` window. Unlike the
//! generic timeline / inspector controls (which apply to videos AND
//! images alike), this panel collects the operations that **only
//! make sense for static images** — interactive brush masking, quick
//! geometry (rotate / mirror), drag-to-crop, plus a richer library
//! of stylisation effects (pixelate / posterize / edge / glitch /
//! retro presets) than the generic effect dropdown bothers to expose.
//!
//! ## Preview
//!
//! The preview is rendered via the **same** image-effects bake cache
//! the canvas uses (`crate::image_fx_worker::lookup_or_dispatch_image_fx`).
//! The canvas was already showing the user the post-effect picture,
//! but the editor used to display the bare source — so a Brightness
//! slider drag updated the canvas while the panel preview stayed flat.
//! Sharing the cache means the editor preview now reflects the exact
//! pixels the canvas paints (including crop, mirror, hue shift,
//! masks, …) and shares the LRU budget so a slider drag pays the bake
//! cost only once across both surfaces.
//!
//! ## Tools
//!
//! The tool toolbar at the top arms an **interactive** brush mode on
//! the preview area:
//! - `Brush` paints a freehand polygon kept on `EffectKind::Mask`
//!   (`invert: false`) — the picture survives only inside the
//!   painted shape.
//! - `Cutout` paints the same polygon with `invert: true` — the
//!   picture survives only OUTSIDE the painted shape (useful for
//!   removing watermarks or background props in a sticker).
//! - `Crop` drags a rectangle that becomes the overlay's
//!   `EffectKind::Crop` entry. The sliders below stay live so the
//!   user can fine-tune the rect after committing.
//!
//! The remaining controls are non-interactive parameter sliders /
//! preset buttons that compose with the brush via the same effect
//! stack; they fall in five collapsing sections:
//! - Quick colour (Brightness / Contrast / Saturation / Hue)
//! - Geometry (Mirror H/V toggles, ±90° / 180° rotate buttons)
//! - Crop sliders + reset
//! - Stylize (Pixelate, Posterize, EdgeDetect, Bloom, Glitch)
//! - Distortion (Wave, ChromaticAberration)
//! - Retro presets (OldFilm, VHS)
//! - Filters (Grayscale, Sepia, Invert, Vignette, Blur, Sharpen, Glow, Noise)

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::{Effect, EffectKind, MaskShape, Overlay};

use crate::state::{EditorState, ImageBrushTool};

/// Draw the floating "Image Editor" window. Returns `true` if the
/// window is still open after this frame.
pub fn image_editor_window(ctx: &egui::Context, state: &mut EditorState) -> bool {
    let mut open = true;
    egui::Window::new(format!("\u{1F5BC} {}", crate::i18n::t("Image Editor")))
        .open(&mut open)
        .default_size([520.0, 640.0])
        .min_width(380.0)
        .min_height(360.0)
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            image_editor_content(ui, state);
        });
    if !open {
        // Disarm any active brush when the user closes the window so
        // the next reopen starts in the passive (no-tool) state.
        state.image_brush.tool = ImageBrushTool::None;
        state.image_brush.draft.clear();
        state.image_brush.crop_drag_start = None;
    }
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
                    "This panel exposes image-only editing tools (brush mask, crop, stylize).",
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

    // ── Brush toolbar ──
    brush_toolbar(ui, state);
    ui.add_space(4.0);

    // ── Preview ──
    //
    // The preview uses the same baked texture the canvas uses (with
    // the same crop UV inset) so what the user sees here matches the
    // canvas paint exactly. While a brush tool is armed the rect
    // captures click+drag input and accumulates polygon points /
    // crop drag anchors.
    let preview_h = 260.0_f32;
    let preview_w = ui.available_width();
    let interactive = state.image_brush.tool != ImageBrushTool::None;
    let sense = if interactive {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(preview_w, preview_h), sense);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(14, 14, 22));

    // The img_rect we draw the picture into (centred + aspect-locked).
    let img_rect_opt = paint_preview(ui.ctx(), &painter, rect, state, &source, i);

    // Brush input lives over the painted img_rect — once we know it.
    if interactive {
        if let Some(img_rect) = img_rect_opt {
            handle_brush_input(ui, &response, state, img_rect, i);
            draw_brush_overlay(&painter, img_rect, state);
        }
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
    ui.add_space(4.0);
    ui.separator();

    // ── Brush parameters (only relevant while a brush tool is armed) ──
    if state.image_brush.tool == ImageBrushTool::Brush
        || state.image_brush.tool == ImageBrushTool::Cutout
    {
        brush_params_section(ui, state);
        ui.add_space(2.0);
        ui.separator();
    }

    egui::ScrollArea::vertical()
        .id_source(("image_editor_scroll", i))
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            // From here on we mutate the overlay's effects stack. Pull a
            // mut ref directly so each section can push / read effects
            // without re-borrowing.
            //
            // Geometry section needs to mutate the overlay's `layout` as
            // well (rotation), so it gets its own scope BEFORE we lock
            // onto the effects vec.
            geometry_section(ui, state, i);

            let effects: &mut Vec<Effect> = match &mut state.scene.overlays[i] {
                Overlay::Image(im) => &mut im.effects,
                _ => unreachable!(),
            };

            quick_colour_section(ui, effects, i);
            crop_section(ui, effects, i);
            stylize_section(ui, effects, i);
            distortion_section(ui, effects, i);
            retro_section(ui, effects, i);
            filters_section(ui, effects, i);
        });
}

// ─── PREVIEW ──────────────────────────────────────────────────────────

/// Paint the preview thumbnail into `outer_rect`. Returns the centred
/// image rect (after aspect-fit + crop inset) so the brush input
/// handler can map cursor positions back into source-image UV.
fn paint_preview(
    ctx: &egui::Context,
    painter: &egui::Painter,
    outer_rect: Rect,
    state: &EditorState,
    source: &std::path::Path,
    overlay_idx: usize,
) -> Option<Rect> {
    let preview_w = outer_rect.width();
    let preview_h = outer_rect.height();

    // Pull the current effect stack and the source size off the state.
    let effects: Vec<Effect> = match state.scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) => im.effects.clone(),
        _ => return None,
    };

    // Resolve the texture: prefer the effect-baked one (matches canvas),
    // otherwise fall back to the raw decoded image. We always need the
    // raw texture for its source-size hint anyway.
    let raw = state
        .image_textures
        .lock()
        .ok()
        .and_then(|map| match map.get(source) {
            Some(crate::state::ImageTextureSlot::Loaded { texture, size }) => {
                Some((texture.clone(), *size))
            }
            _ => None,
        });

    let mut crop_inset = [0.0_f32; 4];
    let baked: Option<egui::TextureHandle> = if !effects.is_empty() && raw.is_some() {
        match crate::image_fx_worker::lookup_or_dispatch_image_fx(
            state, source, &effects, ctx,
        ) {
            Some((tex, crop)) => {
                crop_inset = crop;
                Some(tex)
            }
            None => None,
        }
    } else {
        None
    };

    let (texture, size) = match (baked, raw) {
        (Some(b), Some((_, sz))) => (b, sz),
        (None, Some((r, sz))) => (r, sz),
        _ => {
            // Decoded source not yet ready — show a placeholder.
            painter.text(
                outer_rect.center(),
                egui::Align2::CENTER_CENTER,
                crate::i18n::t("Preview will appear after canvas renders the image once."),
                egui::FontId::proportional(11.0),
                Color32::from_rgb(120, 120, 140),
            );
            return None;
        }
    };

    let ow = size[0] as f32;
    let oh = size[1] as f32;
    if ow <= 0.0 || oh <= 0.0 {
        return None;
    }

    // Apply the crop inset to both the displayed rect (so the visible
    // rectangle shrinks like the canvas does) AND the UVs (so the
    // pixels outside the crop region are simply not shown).
    let crop_w_factor = (1.0 - crop_inset[0] - crop_inset[2]).max(0.001);
    let crop_h_factor = (1.0 - crop_inset[1] - crop_inset[3]).max(0.001);
    let cropped_aspect = (ow * crop_w_factor) / (oh * crop_h_factor).max(1e-3);

    let pad = 6.0_f32;
    let max_w = preview_w - 2.0 * pad;
    let max_h = preview_h - 2.0 * pad;
    let mut display_w = max_w;
    let mut display_h = display_w / cropped_aspect;
    if display_h > max_h {
        display_h = max_h;
        display_w = display_h * cropped_aspect;
    }
    let off_x = (preview_w - display_w) * 0.5;
    let off_y = (preview_h - display_h) * 0.5;
    let img_rect = Rect::from_min_size(
        Pos2::new(outer_rect.min.x + off_x, outer_rect.min.y + off_y),
        Vec2::new(display_w, display_h),
    );

    let uv_l = crop_inset[0].clamp(0.0, 0.99);
    let uv_t = crop_inset[1].clamp(0.0, 0.99);
    let uv_r = (1.0 - crop_inset[2]).clamp(uv_l + 1.0e-3, 1.0);
    let uv_b = (1.0 - crop_inset[3]).clamp(uv_t + 1.0e-3, 1.0);

    let mut mesh = egui::Mesh::with_texture(texture.id());
    mesh.add_rect_with_uv(
        img_rect,
        Rect::from_min_max(Pos2::new(uv_l, uv_t), Pos2::new(uv_r, uv_b)),
        Color32::WHITE,
    );
    painter.add(egui::Shape::mesh(mesh));
    painter.rect_stroke(
        img_rect,
        Rounding::same(3.0),
        Stroke::new(1.0, Color32::from_rgb(60, 50, 70)),
    );

    Some(img_rect)
}

// ─── BRUSH TOOLBAR ────────────────────────────────────────────────────

fn brush_toolbar(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(crate::i18n::t("Tool:"))
                .size(11.0)
                .color(Color32::from_rgb(170, 170, 200)),
        );
        let tools: [(ImageBrushTool, &str, &str); 4] = [
            (
                ImageBrushTool::None,
                "\u{2196}",
                "View only — no interactive painting",
            ),
            (
                ImageBrushTool::Brush,
                "\u{1F58C}",
                "Brush — paint a freehand mask: pixels INSIDE the painted shape are kept, the rest is masked away",
            ),
            (
                ImageBrushTool::Cutout,
                "\u{2702}",
                "Cutout — paint a freehand mask: pixels INSIDE the painted shape are masked away (erase a region)",
            ),
            (
                ImageBrushTool::Crop,
                "\u{2702}\u{FE0F}",
                "Crop — drag a rectangle to set the crop bounds",
            ),
        ];
        for (tool, glyph, hint) in tools {
            let active = state.image_brush.tool == tool;
            let btn = egui::Button::new(
                RichText::new(glyph)
                    .size(15.0)
                    .color(if active {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(220, 220, 240)
                    }),
            )
            .fill(if active {
                Color32::from_rgb(120, 90, 180)
            } else {
                Color32::from_rgb(40, 40, 56)
            })
            .stroke(Stroke::new(1.0, Color32::from_rgb(80, 70, 100)))
            .rounding(Rounding::same(4.0))
            .min_size(Vec2::new(28.0, 24.0));
            if ui.add(btn).on_hover_text(hint).clicked() {
                // Toggle: clicking the active tool returns to None;
                // clicking a different tool replaces it. Either way
                // we drop any in-progress shape so the user gets a
                // clean slate.
                state.image_brush.tool = if active { ImageBrushTool::None } else { tool };
                state.image_brush.draft.clear();
                state.image_brush.crop_drag_start = None;
                // Cutout implies invert; Brush leaves invert alone so
                // the user can flip it manually if they want a slim
                // "punch out" via the brush tool. The tool button just
                // picks a sensible default.
                if state.image_brush.tool == ImageBrushTool::Cutout {
                    state.image_brush.invert = true;
                } else if state.image_brush.tool == ImageBrushTool::Brush {
                    state.image_brush.invert = false;
                }
            }
        }

        ui.separator();
        // Status hint that mirrors the active tool so the user knows
        // they can drag on the preview now.
        let hint = match state.image_brush.tool {
            ImageBrushTool::None => crate::i18n::t("Pick a tool to draw on the preview."),
            ImageBrushTool::Brush => {
                crate::i18n::t("Drag on the preview to paint a mask polygon.")
            }
            ImageBrushTool::Cutout => {
                crate::i18n::t("Drag on the preview to paint an erase region.")
            }
            ImageBrushTool::Crop => {
                crate::i18n::t("Drag on the preview to define the crop rectangle.")
            }
        };
        ui.label(
            RichText::new(hint)
                .size(10.5)
                .italics()
                .color(Color32::from_rgb(140, 140, 170)),
        );
    });
}

fn brush_params_section(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t("Feather"));
        ui.add(egui::Slider::new(&mut state.image_brush.feather, 0.0..=0.3));
        ui.checkbox(&mut state.image_brush.invert, crate::i18n::t("Invert"))
            .on_hover_text(
                "When checked, the painted polygon is the masked-OUT region (instead of the kept region).",
            );
        if !state.image_brush.draft.is_empty()
            && ui.button(crate::i18n::t("Clear stroke")).clicked()
        {
            state.image_brush.draft.clear();
        }
    });
}

// ─── BRUSH INPUT ──────────────────────────────────────────────────────

/// Map a screen position inside `img_rect` to source-image UV (0..1),
/// honouring the crop UV inset already baked into the displayed rect.
fn screen_to_source_uv(
    p: Pos2,
    img_rect: Rect,
    crop_inset: [f32; 4],
) -> Option<[f32; 2]> {
    if !img_rect.contains(p) {
        return None;
    }
    let rel_x = ((p.x - img_rect.min.x) / img_rect.width().max(1e-3)).clamp(0.0, 1.0);
    let rel_y = ((p.y - img_rect.min.y) / img_rect.height().max(1e-3)).clamp(0.0, 1.0);
    let u = crop_inset[0] + rel_x * (1.0 - crop_inset[0] - crop_inset[2]);
    let v = crop_inset[1] + rel_y * (1.0 - crop_inset[1] - crop_inset[3]);
    Some([u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)])
}

fn handle_brush_input(
    ui: &egui::Ui,
    response: &egui::Response,
    state: &mut EditorState,
    img_rect: Rect,
    overlay_idx: usize,
) {
    // Recover the crop inset that's currently applied to the preview
    // by looking it up in the cache (no dispatch — paint_preview
    // already did that this frame). A miss here just means the
    // baked entry hasn't landed yet, so we treat the inset as zero.
    let crop_inset = current_crop_inset(state, overlay_idx);

    let pointer = ui.input(|i| i.pointer.hover_pos());
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let primary_released = ui.input(|i| i.pointer.any_released());
    let dragging = response.dragged() && primary_down;
    let just_pressed = response.drag_started() || response.clicked();

    match state.image_brush.tool {
        ImageBrushTool::None => {}
        ImageBrushTool::Brush | ImageBrushTool::Cutout => {
            if just_pressed {
                state.image_brush.draft.clear();
                if let Some(p) = pointer {
                    if let Some(uv) = screen_to_source_uv(p, img_rect, crop_inset) {
                        state.image_brush.draft.push(uv);
                    }
                }
            }
            if dragging {
                if let Some(p) = pointer {
                    if let Some(uv) = screen_to_source_uv(p, img_rect, crop_inset) {
                        // Decimate to avoid piling up thousands of
                        // duplicate points when the cursor barely
                        // moves between frames.
                        let push = match state.image_brush.draft.last() {
                            Some(prev) => {
                                let dx = uv[0] - prev[0];
                                let dy = uv[1] - prev[1];
                                (dx * dx + dy * dy).sqrt() > 0.005
                            }
                            None => true,
                        };
                        if push {
                            state.image_brush.draft.push(uv);
                        }
                    }
                }
                ui.ctx().request_repaint();
            }
            if primary_released {
                commit_brush_polygon(state, overlay_idx);
            }
        }
        ImageBrushTool::Crop => {
            if just_pressed {
                if let Some(p) = pointer {
                    if let Some(uv) = screen_to_source_uv(p, img_rect, crop_inset) {
                        state.image_brush.crop_drag_start = Some(uv);
                        state.image_brush.draft.clear();
                        state.image_brush.draft.push(uv);
                        state.image_brush.draft.push(uv);
                    }
                }
            }
            if dragging {
                if let (Some(p), Some(_)) = (pointer, state.image_brush.crop_drag_start) {
                    if let Some(uv) = screen_to_source_uv(p, img_rect, crop_inset) {
                        if state.image_brush.draft.len() < 2 {
                            state.image_brush.draft.push(uv);
                        } else {
                            state.image_brush.draft[1] = uv;
                        }
                    }
                }
                ui.ctx().request_repaint();
            }
            if primary_released {
                commit_brush_crop(state, overlay_idx);
            }
        }
    }
}

/// Look up the current effect-pipeline crop inset for `overlay_idx`
/// (purely a lookup; never dispatches a bake). Used by the brush
/// input handler to compensate for the fact that the displayed
/// preview rect already excludes the cropped border, so a click
/// position should map back to the *uncropped* source UV.
fn current_crop_inset(state: &EditorState, overlay_idx: usize) -> [f32; 4] {
    use crate::image_fx_cache::LookupOutcome;
    let Some(Overlay::Image(im)) = state.scene.overlays.get(overlay_idx) else {
        return [0.0; 4];
    };
    if im.effects.is_empty() {
        return [0.0; 4];
    }
    let sig = crate::image_effects::signature(&im.effects);
    if let LookupOutcome::Ready(slot) = state.image_fx_cache.lookup(&im.source, sig) {
        slot.crop
    } else {
        [0.0; 4]
    }
}

fn commit_brush_polygon(state: &mut EditorState, overlay_idx: usize) {
    let pts: Vec<[f32; 2]> = state
        .image_brush
        .draft
        .iter()
        .map(|p| [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)])
        .collect();
    state.image_brush.draft.clear();
    if pts.len() < 3 {
        return;
    }

    let feather = state.image_brush.feather.clamp(0.0, 0.5);
    let invert = state.image_brush.invert;
    if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
        // Replace any existing freehand-polygon mask so successive
        // brush strokes redraw the shape cleanly. Rect / ellipse mask
        // entries from the canvas mask tools are left untouched —
        // they're a different authoring surface.
        if let Some(idx) = im.effects.iter().position(|e| {
            matches!(
                &e.kind,
                EffectKind::Mask {
                    shape: MaskShape::Polygon { .. },
                    ..
                }
            )
        }) {
            im.effects.remove(idx);
        }
        im.effects.push(Effect::new(EffectKind::Mask {
            shape: MaskShape::Polygon { points: pts },
            feather,
            invert,
        }));
    }
}

fn commit_brush_crop(state: &mut EditorState, overlay_idx: usize) {
    let pts = std::mem::take(&mut state.image_brush.draft);
    state.image_brush.crop_drag_start = None;
    if pts.len() < 2 {
        return;
    }
    let a = pts[0];
    let b = pts[pts.len() - 1];
    let lx = a[0].min(b[0]).clamp(0.0, 1.0);
    let rx = a[0].max(b[0]).clamp(0.0, 1.0);
    let ty = a[1].min(b[1]).clamp(0.0, 1.0);
    let by = a[1].max(b[1]).clamp(0.0, 1.0);
    if (rx - lx).abs() < 0.005 || (by - ty).abs() < 0.005 {
        return;
    }
    // Convert the rect drawn on the picture into Crop-effect insets:
    // we discard the bands OUTSIDE [lx..rx] × [ty..by]. Each value
    // is clamped to 0..0.49 to match `EffectKind::Crop`'s contract.
    let left = lx.clamp(0.0, 0.49);
    let top = ty.clamp(0.0, 0.49);
    let right = (1.0 - rx).clamp(0.0, 0.49);
    let bottom = (1.0 - by).clamp(0.0, 0.49);

    if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
        let kind = EffectKind::Crop {
            left,
            top,
            right,
            bottom,
        };
        if let Some(idx) = im
            .effects
            .iter()
            .position(|e| matches!(e.kind, EffectKind::Crop { .. }))
        {
            im.effects[idx].kind = kind;
        } else {
            im.effects.push(Effect::new(kind));
        }
    }
}

/// Paint the in-progress polygon / rect on top of the preview so the
/// user can see what the next mouse-release will commit. Uses
/// crop-aware mapping (UV → screen) symmetric with the input handler.
fn draw_brush_overlay(
    painter: &egui::Painter,
    img_rect: Rect,
    state: &EditorState,
) {
    let crop_inset = match state.selection {
        crate::state::Selection::Overlay(idx) => current_crop_inset(state, idx),
        _ => [0.0; 4],
    };

    let to_screen = |uv: [f32; 2]| -> Pos2 {
        let denom_x = (1.0 - crop_inset[0] - crop_inset[2]).max(1e-3);
        let denom_y = (1.0 - crop_inset[1] - crop_inset[3]).max(1e-3);
        let rx = ((uv[0] - crop_inset[0]) / denom_x).clamp(0.0, 1.0);
        let ry = ((uv[1] - crop_inset[1]) / denom_y).clamp(0.0, 1.0);
        Pos2::new(
            img_rect.min.x + rx * img_rect.width(),
            img_rect.min.y + ry * img_rect.height(),
        )
    };

    let stroke_col = match state.image_brush.tool {
        ImageBrushTool::Cutout => Color32::from_rgb(255, 120, 120),
        ImageBrushTool::Crop => Color32::from_rgb(120, 220, 255),
        _ => Color32::from_rgb(255, 220, 80),
    };

    match state.image_brush.tool {
        ImageBrushTool::Brush | ImageBrushTool::Cutout => {
            let pts: Vec<Pos2> = state.image_brush.draft.iter().map(|p| to_screen(*p)).collect();
            if pts.len() >= 2 {
                painter.add(egui::Shape::line(pts.clone(), Stroke::new(2.0, stroke_col)));
            }
            if let Some(first) = pts.first() {
                painter.circle_filled(*first, 3.0, stroke_col);
            }
            if let Some(last) = pts.last() {
                painter.circle_filled(*last, 3.0, stroke_col);
            }
        }
        ImageBrushTool::Crop => {
            if state.image_brush.draft.len() >= 2 {
                let a = to_screen(state.image_brush.draft[0]);
                let b = to_screen(state.image_brush.draft[state.image_brush.draft.len() - 1]);
                let r = Rect::from_two_pos(a, b);
                painter.rect_stroke(r, Rounding::same(2.0), Stroke::new(1.5, stroke_col));
                painter.rect_filled(
                    r,
                    Rounding::same(2.0),
                    Color32::from_rgba_unmultiplied(120, 220, 255, 30),
                );
            }
        }
        ImageBrushTool::None => {}
    }
}

// ─── TOOL SECTIONS ────────────────────────────────────────────────────

fn quick_colour_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Quick colour"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(220, 180, 100)),
    )
    .id_source(("image_editor_color", salt))
    .default_open(true)
    .show(ui, |ui| {
        adjust_slider(
            ui,
            effects,
            "Brightness",
            -1.0..=1.0,
            |k| matches!(k, EffectKind::Brightness { .. }),
            |amt| EffectKind::Brightness { amount: amt },
            |k| match k {
                EffectKind::Brightness { amount } => Some(*amount),
                _ => None,
            },
        );
        adjust_slider(
            ui,
            effects,
            "Contrast",
            -1.0..=1.0,
            |k| matches!(k, EffectKind::Contrast { .. }),
            |amt| EffectKind::Contrast { amount: amt },
            |k| match k {
                EffectKind::Contrast { amount } => Some(*amount),
                _ => None,
            },
        );
        adjust_slider(
            ui,
            effects,
            "Saturation",
            -1.0..=1.0,
            |k| matches!(k, EffectKind::Saturation { .. }),
            |amt| EffectKind::Saturation { amount: amt },
            |k| match k {
                EffectKind::Saturation { amount } => Some(*amount),
                _ => None,
            },
        );
        adjust_slider(
            ui,
            effects,
            "Hue \u{00B0}",
            -180.0..=180.0,
            |k| matches!(k, EffectKind::HueShift { .. }),
            |deg| EffectKind::HueShift { degrees: deg },
            |k| match k {
                EffectKind::HueShift { degrees } => Some(*degrees),
                _ => None,
            },
        );
    });
}

/// Image-only geometry: mirror toggles and quick rotate buttons.
/// Quick rotate writes directly into the overlay's first layout
/// keyframe — the canvas, the FFmpeg renderer and the inspector all
/// pick the new angle up via the existing rotation_deg pipeline.
fn geometry_section(ui: &mut egui::Ui, state: &mut EditorState, overlay_idx: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Geometry"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(140, 220, 200)),
    )
    .id_source(("image_editor_geometry", overlay_idx))
    .default_open(true)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            // Mirror H / V — toggle EffectKind::MirrorH / MirrorV.
            if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
                mirror_button(ui, &mut im.effects, "\u{2B6E} Mirror H", true);
                mirror_button(ui, &mut im.effects, "\u{2B6F} Mirror V", false);
            }

            // Rotate buttons mutate the layout's first keyframe so
            // the change persists and is keyframe-friendly. We only
            // touch the static value — animation curves stay intact.
            if ui
                .button(crate::i18n::t("\u{21BA} -90\u{00B0}"))
                .on_hover_text("Rotate 90° counter-clockwise")
                .clicked()
            {
                rotate_overlay(state, overlay_idx, -90.0);
            }
            if ui
                .button(crate::i18n::t("\u{21BB} +90\u{00B0}"))
                .on_hover_text("Rotate 90° clockwise")
                .clicked()
            {
                rotate_overlay(state, overlay_idx, 90.0);
            }
            if ui
                .button(crate::i18n::t("180\u{00B0}"))
                .on_hover_text("Flip 180°")
                .clicked()
            {
                rotate_overlay(state, overlay_idx, 180.0);
            }
            if ui
                .button(crate::i18n::t("Reset"))
                .on_hover_text("Reset rotation to 0°")
                .clicked()
            {
                if let Some(Overlay::Image(im)) =
                    state.scene.overlays.get_mut(overlay_idx)
                {
                    if let Some(kf) = im.layout.first_mut() {
                        kf.value.rotation_deg = 0.0;
                    }
                }
            }
        });
    });
}

fn mirror_button(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    label: &'static str,
    horizontal: bool,
) {
    let target_kind = if horizontal {
        EffectKind::MirrorH
    } else {
        EffectKind::MirrorV
    };
    let idx = effects.iter().position(|e| {
        std::mem::discriminant(&e.kind) == std::mem::discriminant(&target_kind)
    });
    let active = idx.is_some();
    let btn = egui::Button::new(
        RichText::new(crate::i18n::t(label))
            .size(11.0)
            .color(if active {
                Color32::WHITE
            } else {
                Color32::from_rgb(220, 220, 240)
            }),
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
            None => effects.push(Effect::new(target_kind)),
        }
    }
}

fn rotate_overlay(state: &mut EditorState, overlay_idx: usize, delta_deg: f32) {
    if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
        if let Some(kf) = im.layout.first_mut() {
            // Wrap into [-180, 180] so successive +90° clicks don't
            // accumulate into eye-watering large numbers in the
            // inspector.
            let mut r = kf.value.rotation_deg + delta_deg;
            while r > 180.0 {
                r -= 360.0;
            }
            while r < -180.0 {
                r += 360.0;
            }
            kf.value.rotation_deg = r;
        }
    }
}

fn crop_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Crop"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(140, 200, 255)),
    )
    .id_source(("image_editor_crop", salt))
    .default_open(false)
    .show(ui, |ui| {
        let crop_idx = effects
            .iter()
            .position(|e| matches!(e.kind, EffectKind::Crop { .. }));
        let mut left = 0.0_f32;
        let mut top = 0.0_f32;
        let mut right = 0.0_f32;
        let mut bottom = 0.0_f32;
        if let Some(idx) = crop_idx {
            if let EffectKind::Crop {
                left: l,
                top: t,
                right: r,
                bottom: b,
            } = effects[idx].kind
            {
                left = l;
                top = t;
                right = r;
                bottom = b;
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
            changed |= ui
                .add(egui::Slider::new(&mut bottom, 0.0..=0.49))
                .changed();
        });
        if changed {
            let kind = EffectKind::Crop {
                left,
                top,
                right,
                bottom,
            };
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
}

fn stylize_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Stylize"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(255, 170, 220)),
    )
    .id_source(("image_editor_stylize", salt))
    .default_open(false)
    .show(ui, |ui| {
        adjust_slider(
            ui,
            effects,
            "Pixelate",
            0.0..=80.0,
            |k| matches!(k, EffectKind::Pixelate { .. }),
            |amt| EffectKind::Pixelate { block_size: amt },
            |k| match k {
                EffectKind::Pixelate { block_size } => Some(*block_size),
                _ => None,
            },
        );
        // Posterize is integer-valued — render an int slider with a
        // dedicated remove-when-default button rather than the
        // generic adjust_slider helper (which assumes f32 + neutral=0).
        posterize_slider(ui, effects);
        adjust_slider(
            ui,
            effects,
            "Edge detect",
            0.0..=1.0,
            |k| matches!(k, EffectKind::EdgeDetect { .. }),
            |amt| EffectKind::EdgeDetect { threshold: amt },
            |k| match k {
                EffectKind::EdgeDetect { threshold } => Some(*threshold),
                _ => None,
            },
        );
        adjust_slider(
            ui,
            effects,
            "Bloom",
            0.0..=60.0,
            |k| matches!(k, EffectKind::Bloom { .. }),
            |amt| EffectKind::Bloom { radius: amt },
            |k| match k {
                EffectKind::Bloom { radius } => Some(*radius),
                _ => None,
            },
        );
        adjust_slider(
            ui,
            effects,
            "Glitch",
            0.0..=1.0,
            |k| matches!(k, EffectKind::Glitch { .. }),
            |amt| EffectKind::Glitch { strength: amt },
            |k| match k {
                EffectKind::Glitch { strength } => Some(*strength),
                _ => None,
            },
        );
    });
}

fn posterize_slider(ui: &mut egui::Ui, effects: &mut Vec<Effect>) {
    let idx = effects
        .iter()
        .position(|e| matches!(e.kind, EffectKind::Posterize { .. }));
    let mut levels: u32 = match idx.and_then(|i| match &effects[i].kind {
        EffectKind::Posterize { levels } => Some(*levels),
        _ => None,
    }) {
        Some(l) => l,
        None => 0,
    };
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t("Posterize"));
        let avail = ui.available_width();
        let r = ui.add_sized(
            egui::vec2((avail - 56.0).max(120.0), 18.0),
            egui::Slider::new(&mut levels, 0..=24),
        );
        if r.changed() {
            // 0 / 1 = remove (no posterize); 2..32 = active.
            match (idx, levels) {
                (Some(i), 0) | (Some(i), 1) => {
                    effects.remove(i);
                }
                (Some(i), n) => {
                    effects[i].kind = EffectKind::Posterize {
                        levels: n.clamp(2, 32),
                    };
                }
                (None, n) if n >= 2 => {
                    effects.push(Effect::new(EffectKind::Posterize {
                        levels: n.clamp(2, 32),
                    }));
                }
                _ => {}
            }
        }
        if ui.small_button("\u{21BA}").on_hover_text("Reset").clicked() {
            if let Some(i) = idx {
                effects.remove(i);
            }
        }
    });
}

fn distortion_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Distortion"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(180, 160, 240)),
    )
    .id_source(("image_editor_distortion", salt))
    .default_open(false)
    .show(ui, |ui| {
        // Wave needs two parameters; render as a single slider on
        // amplitude and a small horizontal "Wavelength" follow-up.
        wave_sliders(ui, effects);
        adjust_slider(
            ui,
            effects,
            "Chromatic aberration",
            0.0..=20.0,
            |k| matches!(k, EffectKind::ChromaticAberration { .. }),
            |amt| EffectKind::ChromaticAberration { offset: amt },
            |k| match k {
                EffectKind::ChromaticAberration { offset } => Some(*offset),
                _ => None,
            },
        );
    });
}

fn wave_sliders(ui: &mut egui::Ui, effects: &mut Vec<Effect>) {
    let idx = effects
        .iter()
        .position(|e| matches!(e.kind, EffectKind::Wave { .. }));
    let (mut amp, mut wl) = match idx.and_then(|i| match effects[i].kind {
        EffectKind::Wave {
            amplitude,
            wavelength,
        } => Some((amplitude, wavelength)),
        _ => None,
    }) {
        Some(p) => p,
        None => (0.0_f32, 60.0_f32),
    };
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t("Wave amp"));
        changed |= ui
            .add(egui::Slider::new(&mut amp, 0.0..=40.0))
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t("Wave \u{03BB}"));
        changed |= ui
            .add(egui::Slider::new(&mut wl, 4.0..=240.0))
            .changed();
    });
    if changed {
        match (idx, amp.abs() < 0.001) {
            (Some(i), true) => {
                effects.remove(i);
            }
            (Some(i), false) => {
                effects[i].kind = EffectKind::Wave {
                    amplitude: amp,
                    wavelength: wl.max(1.0),
                };
            }
            (None, false) => effects.push(Effect::new(EffectKind::Wave {
                amplitude: amp,
                wavelength: wl.max(1.0),
            })),
            (None, true) => {}
        }
    }
}

fn retro_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Retro"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(255, 200, 140)),
    )
    .id_source(("image_editor_retro", salt))
    .default_open(false)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            filter_button(ui, effects, "Old film", EffectKind::OldFilm);
            filter_button(ui, effects, "VHS", EffectKind::Vhs);
        });
    });
}

fn filters_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Filters"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(180, 240, 180)),
    )
    .id_source(("image_editor_filters", salt))
    .default_open(false)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            filter_button(ui, effects, "Grayscale", EffectKind::Grayscale);
            filter_button(ui, effects, "Sepia", EffectKind::Sepia);
            filter_button(ui, effects, "Invert", EffectKind::Invert);
            filter_button(
                ui,
                effects,
                "Vignette",
                EffectKind::Vignette { strength: 0.5 },
            );
            filter_button(ui, effects, "Blur", EffectKind::Blur { radius: 6.0 });
            filter_button(ui, effects, "Sharpen", EffectKind::Sharpen { amount: 1.0 });
            filter_button(
                ui,
                effects,
                "Glow",
                EffectKind::Glow {
                    radius: 12.0,
                    intensity: 0.6,
                },
            );
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

// ─── HELPERS (slider / preset toggle) ─────────────────────────────────

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
    let mut value = idx.and_then(|i| read(&effects[i].kind)).unwrap_or(0.0);
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t(label));
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
            .color(if active {
                Color32::WHITE
            } else {
                Color32::from_rgb(220, 220, 240)
            }),
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
