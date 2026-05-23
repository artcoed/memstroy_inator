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
    // The window title used to splice in the 🖼 (U+1F5BC) emoji, but
    // that codepoint is not in egui's bundled font and showed up as a
    // missing-glyph box on Windows. Drop it so the title row is just
    // localised text.
    egui::Window::new(crate::i18n::t("Image Editor"))
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

    // ── Save / preview-zoom toolbar ──
    //
    // Sits above the preview pane so the user can (a) bake the
    // current effect stack into a fresh PNG that lands in the
    // project's image library (and can be dropped onto the canvas
    // like any other sticker), and (b) zoom / pan the preview
    // independently of the window size. The "Save" action is what
    // the user asked for in "сохранить изменённый вариант
    // изображения в локальные ресурсы проекта".
    save_and_zoom_toolbar(ui, state, i, &source);
    ui.add_space(4.0);

    // ── Preview ──
    //
    // The preview uses the same baked texture the canvas uses (with
    // the same crop UV inset) so what the user sees here matches the
    // canvas paint exactly. While a brush tool is armed the rect
    // captures click+drag input and accumulates polygon points /
    // crop drag anchors. The pane is rendered inside an `egui::Resize`
    // so the user can drag the bottom-right handle to give the
    // picture more (or less) vertical room.
    let preview_h = state.image_brush.preview_height.clamp(120.0, 1200.0);
    let preview_w = ui.available_width();
    let interactive = state.image_brush.tool != ImageBrushTool::None;
    let sense = if interactive {
        Sense::click_and_drag()
    } else {
        Sense::click_and_drag()
    };
    let resp = egui::Resize::default()
        .id_source(("image_editor_preview_resize", i))
        .resizable([false, true])
        .default_size(Vec2::new(preview_w, preview_h))
        .min_height(120.0)
        .max_height(1200.0)
        .show(ui, |ui| {
            let avail = ui.available_size();
            let (rect, response) = ui.allocate_exact_size(avail, sense);
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(14, 14, 22));

            // The img_rect we draw the picture into (centred + aspect-locked).
            let img_rect_opt = paint_preview(ui.ctx(), &painter, rect, state, &source, i);

            // Wheel zoom + middle/right drag pan, applied regardless
            // of whether a brush tool is armed (so the user can frame
            // the picture before painting).
            handle_preview_pan_zoom(ui, state, &response, rect);

            // Brush input lives over the painted img_rect — once we
            // know it.
            if interactive {
                if let Some(img_rect) = img_rect_opt {
                    handle_brush_input(ui, &response, state, img_rect, i);
                    draw_brush_overlay(&painter, img_rect, state);
                }
            }
            // Surface the actually-allocated pane height back to the
            // outer scope so we can persist it on `EditorState` for
            // a stable initial size after a window-toggle round-trip
            // (egui::Resize keeps its own per-id memory across
            // frames, but `default_size` only honours the first
            // call — round-tripping through our state field gives
            // a sane fallback if the egui memory is cleared).
            avail.y
        });
    // Persist the new height back so the next frame paints with it.
    // `Resize::show` returns the closure's R directly, which is the
    // height we surfaced above.
    state.image_brush.preview_height = resp.max(120.0).min(1200.0);
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

    // ── Brush parameters (only relevant while a mask-painting tool is armed) ──
    if matches!(
        state.image_brush.tool,
        ImageBrushTool::Brush
            | ImageBrushTool::Cutout
            | ImageBrushTool::RectMask
            | ImageBrushTool::EllipseMask
    ) {
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

            // Some sections want auxiliary state (source pixel size for
            // aspect-ratio crop) or want to arm an interactive tool
            // (colour-key Re-pick → Eyedropper). Read / decide that
            // up-front while we still hold an immutable borrow of
            // `state`, then act on the flags AFTER the effects-stack
            // mutation block so the borrow checker stays happy.
            let source_size = source_pixel_size(state, i);

            let effects: &mut Vec<Effect> = match &mut state.scene.overlays[i] {
                Overlay::Image(im) => &mut im.effects,
                _ => unreachable!(),
            };

            // Effects overview comes first so the user can see (and
            // disable / remove) every applied effect at a glance.
            effects_overview_section(ui, effects, i);

            // Order the remaining sections from broad → narrow:
            // - Lookbook (one-click multi-effect presets)
            // - Quick colour (single-knob tone)
            // - Crop + Aspect ratios (geometry framing)
            // - Stylize / Distortion / Retro / Filters (single-fx)
            // - Colour key (eyedropper-driven chroma key)
            presets_section(ui, effects, i);
            quick_colour_section(ui, effects, i);
            crop_section(ui, effects, i);
            aspect_ratio_section(ui, effects, source_size, i);
            stylize_section(ui, effects, i);
            distortion_section(ui, effects, i);
            retro_section(ui, effects, i);
            filters_section(ui, effects, i);

            // Colour-key section returns whether the user clicked
            // "Re-pick" (which arms the Eyedropper tool); we apply
            // that side-effect once the `effects` borrow is dropped.
            let arm_eyedropper = color_key_section(ui, effects, i);
            // Drop the mutable borrow on effects implicitly here, then
            // hand control back to `state` for the eyedropper toggle.
            let _ = effects; // keep borrow alive until here
            if arm_eyedropper {
                state.image_brush.tool = ImageBrushTool::Eyedropper;
                state.image_brush.draft.clear();
                state.image_brush.crop_drag_start = None;
            }
        });
}

// ─── PREVIEW ──────────────────────────────────────────────────────────

/// Paint the preview thumbnail into `outer_rect`. Returns the centred
/// image rect (after aspect-fit + crop inset, then the user-controlled
/// preview zoom + pan) so the brush input handler can map cursor
/// positions back into source-image UV.
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
    // Apply user-controlled zoom + pan around the pane centre. The
    // returned img_rect is what the brush input handler maps cursor
    // positions through, so zooming in still gives pixel-accurate
    // brush strokes.
    let zoom = state.image_brush.preview_zoom.clamp(0.1, 8.0);
    display_w *= zoom;
    display_h *= zoom;
    let pan = state.image_brush.preview_pan;
    let off_x = (preview_w - display_w) * 0.5 + pan[0];
    let off_y = (preview_h - display_h) * 0.5 + pan[1];
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
    // Two-row toolbar: row 1 hosts the seven interactive tools
    // (View, Brush, Cutout, Crop, RectMask, EllipseMask, Eyedropper)
    // grouped into "View / Mask painting / Geometry crop / Colour key";
    // row 2 hosts the contextual hint that mirrors the active tool so
    // the user knows what dragging on the preview will do.
    //
    // Tool buttons use translated text labels rather than emoji
    // glyphs — several useful glyphs (✂, 🖌, ⭮ …) are outside the
    // bundled font's coverage and rendered as missing-glyph boxes
    // on Windows builds, which the user reported as "иконки которые
    // не отрисовываются". Plain text labels render everywhere.
    let tools: [(ImageBrushTool, &str, &str); 7] = [
        (
            ImageBrushTool::None,
            "View",
            "View only — no interactive painting",
        ),
        (
            ImageBrushTool::Brush,
            "Brush",
            "Brush — paint a freehand mask: pixels INSIDE the painted shape are kept, the rest is masked away",
        ),
        (
            ImageBrushTool::Cutout,
            "Cutout",
            "Cutout — paint a freehand mask: pixels INSIDE the painted shape are masked away (erase a region)",
        ),
        (
            ImageBrushTool::Crop,
            "Crop",
            "Crop — drag a rectangle to set the crop bounds",
        ),
        (
            ImageBrushTool::RectMask,
            "Rect",
            "Rectangle mask — drag a rectangle and bake it as a soft-edge mask shape (use Feather / Invert below to tune)",
        ),
        (
            ImageBrushTool::EllipseMask,
            "Ellipse",
            "Ellipse mask — drag a rectangle to define the ellipse's bounding box (use Feather / Invert below to tune)",
        ),
        (
            ImageBrushTool::Eyedropper,
            "Eyedropper",
            "Eyedropper — click on the preview to sample a colour and chroma-key away every pixel close to it",
        ),
    ];

    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(crate::i18n::t("Tool:"))
                .size(11.0)
                .color(Color32::from_rgb(170, 170, 200)),
        );
        for (tool, label, hint) in tools {
            let active = state.image_brush.tool == tool;
            // Tint the buttons by tool family so the toolbar reads at
            // a glance (mask = violet, crop = blue, eyedropper =
            // yellow, view = grey). Active button keeps the strong
            // violet to match the previous look.
            let family_tint = match tool {
                ImageBrushTool::None => Color32::from_rgb(50, 50, 64),
                ImageBrushTool::Brush | ImageBrushTool::Cutout => {
                    Color32::from_rgb(48, 40, 64)
                }
                ImageBrushTool::Crop => Color32::from_rgb(40, 50, 70),
                ImageBrushTool::RectMask | ImageBrushTool::EllipseMask => {
                    Color32::from_rgb(50, 42, 72)
                }
                ImageBrushTool::Eyedropper => Color32::from_rgb(70, 60, 36),
            };
            let btn = egui::Button::new(
                RichText::new(crate::i18n::t(label))
                    .size(11.5)
                    .color(if active {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(220, 220, 240)
                    }),
            )
            .fill(if active {
                Color32::from_rgb(120, 90, 180)
            } else {
                family_tint
            })
            .stroke(Stroke::new(1.0, Color32::from_rgb(80, 70, 100)))
            .rounding(Rounding::same(4.0))
            .min_size(Vec2::new(54.0, 24.0));
            if ui.add(btn).on_hover_text(crate::i18n::t(hint)).clicked() {
                // Toggle: clicking the active tool returns to None;
                // clicking a different tool replaces it. Either way
                // we drop any in-progress shape so the user gets a
                // clean slate.
                state.image_brush.tool = if active { ImageBrushTool::None } else { tool };
                state.image_brush.draft.clear();
                state.image_brush.crop_drag_start = None;
                // Sensible defaults for `invert` based on tool:
                // - Cutout always cuts out (invert = true).
                // - Brush always keeps inside (invert = false).
                // - Rect / Ellipse mask leave invert alone so the
                //   user keeps whatever toggle they last set.
                if state.image_brush.tool == ImageBrushTool::Cutout {
                    state.image_brush.invert = true;
                } else if state.image_brush.tool == ImageBrushTool::Brush {
                    state.image_brush.invert = false;
                }
            }
        }
    });

    // Contextual hint row.
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
        ImageBrushTool::RectMask => {
            crate::i18n::t("Drag on the preview to define a rectangular mask region.")
        }
        ImageBrushTool::EllipseMask => {
            crate::i18n::t("Drag on the preview to define an elliptical mask region.")
        }
        ImageBrushTool::Eyedropper => {
            crate::i18n::t("Click on the preview to sample a colour and chroma-key it.")
        }
    };
    ui.label(
        RichText::new(hint)
            .size(10.5)
            .italics()
            .color(Color32::from_rgb(140, 140, 170)),
    );
}

fn brush_params_section(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(crate::i18n::t("Feather"));
        ui.add(egui::Slider::new(&mut state.image_brush.feather, 0.0..=0.3));
        ui.checkbox(&mut state.image_brush.invert, crate::i18n::t("Invert"))
            .on_hover_text(crate::i18n::t(
                "When checked, the painted polygon is the masked-OUT region (instead of the kept region).",
            ));
        if !state.image_brush.draft.is_empty()
            && ui.button(crate::i18n::t("Clear stroke")).clicked()
        {
            state.image_brush.draft.clear();
        }
    });
}

// ─── SAVE / ZOOM TOOLBAR ──────────────────────────────────────────────

/// Toolbar above the preview pane. Hosts the "save edited variant
/// to the project's image library" action (the user's primary
/// request) plus zoom controls (zoom-out / fit / 1:1 / zoom-in)
/// that drive `state.image_brush.preview_zoom` and `preview_pan`.
fn save_and_zoom_toolbar(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    overlay_idx: usize,
    source: &std::path::Path,
) {
    ui.horizontal(|ui| {
        // ── Save edited image to local library ──
        let save_btn = egui::Button::new(
            RichText::new(crate::i18n::t("Save edited image"))
                .size(11.5)
                .color(Color32::WHITE),
        )
        .fill(Color32::from_rgb(70, 130, 90))
        .stroke(Stroke::new(1.0, Color32::from_rgb(120, 200, 140)))
        .rounding(Rounding::same(4.0))
        .min_size(Vec2::new(150.0, 24.0));
        if ui
            .add(save_btn)
            .on_hover_text(crate::i18n::t(
                "Bake the current effect stack into a fresh PNG and add it to the project's local image library.",
            ))
            .clicked()
        {
            match bake_and_save_edited_image(state, overlay_idx, source) {
                Ok(name) => {
                    state.status = format!(
                        "{} {}",
                        crate::i18n::t("\u{2705} Edited image saved to library:"),
                        name,
                    );
                }
                Err(e) => {
                    state.status = format!(
                        "{} {}",
                        crate::i18n::t("\u{274C} Save edited image failed:"),
                        e,
                    );
                }
            }
        }

        ui.separator();

        // ── Zoom controls ──
        ui.label(
            RichText::new(crate::i18n::t("Zoom:"))
                .size(11.0)
                .color(Color32::from_rgb(170, 170, 200)),
        );
        let zoom_minus = egui::Button::new(RichText::new("-").size(13.0))
            .min_size(Vec2::new(24.0, 22.0));
        if ui
            .add(zoom_minus)
            .on_hover_text(crate::i18n::t("Zoom out"))
            .clicked()
        {
            state.image_brush.preview_zoom =
                (state.image_brush.preview_zoom * 0.8).clamp(0.1, 8.0);
            if state.image_brush.preview_zoom <= 1.0 {
                state.image_brush.preview_pan = [0.0, 0.0];
            }
        }
        let zoom_label = format!("{:.0}%", state.image_brush.preview_zoom * 100.0);
        ui.label(
            RichText::new(zoom_label)
                .size(11.0)
                .color(Color32::from_rgb(220, 220, 240)),
        );
        let zoom_plus = egui::Button::new(RichText::new("+").size(13.0))
            .min_size(Vec2::new(24.0, 22.0));
        if ui
            .add(zoom_plus)
            .on_hover_text(crate::i18n::t("Zoom in"))
            .clicked()
        {
            state.image_brush.preview_zoom =
                (state.image_brush.preview_zoom * 1.25).clamp(0.1, 8.0);
        }

        if ui
            .button(crate::i18n::t("Fit"))
            .on_hover_text(crate::i18n::t("Fit the picture into the preview pane"))
            .clicked()
        {
            state.image_brush.preview_zoom = 1.0;
            state.image_brush.preview_pan = [0.0, 0.0];
        }
        if ui
            .button(crate::i18n::t("1:1"))
            .on_hover_text(crate::i18n::t("Show preview at native pixel scale"))
            .clicked()
        {
            // Approximate 1:1 by setting zoom to a value that brings the
            // image close to its native pixel size at the current pane
            // dimensions. The exact ratio depends on the source size,
            // but `2.0` is a reasonable upper-mid for typical sticker
            // sources on a moderately-sized window.
            state.image_brush.preview_zoom = 2.0;
        }
    });
}

// ─── PREVIEW PAN / ZOOM INPUT ─────────────────────────────────────────

/// Mouse-wheel zoom + middle/secondary-button drag panning for the
/// preview pane. Cumulative — repeated wheel ticks compound the same
/// way they do in any image viewer, and panning is decoupled from
/// brush input so the user can frame the picture even while a tool
/// is armed.
fn handle_preview_pan_zoom(
    ui: &egui::Ui,
    state: &mut EditorState,
    response: &egui::Response,
    rect: Rect,
) {
    // Wheel zoom — only respond when the cursor hovers the preview
    // rect; otherwise scrolling the surrounding scroll area would
    // also stomp the zoom.
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.1 {
            let factor = if scroll > 0.0 { 1.1 } else { 1.0 / 1.1 };
            let new_zoom =
                (state.image_brush.preview_zoom * factor).clamp(0.1, 8.0);
            // Anchor the zoom to the cursor so the pixel under the
            // cursor stays approximately put across the zoom step.
            if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                let cx = rect.center().x;
                let cy = rect.center().y;
                let rel_x = pos.x - cx - state.image_brush.preview_pan[0];
                let rel_y = pos.y - cy - state.image_brush.preview_pan[1];
                let scale = new_zoom / state.image_brush.preview_zoom.max(1e-3);
                state.image_brush.preview_pan[0] += rel_x * (1.0 - scale);
                state.image_brush.preview_pan[1] += rel_y * (1.0 - scale);
            }
            state.image_brush.preview_zoom = new_zoom;
            if state.image_brush.preview_zoom <= 1.0 {
                state.image_brush.preview_pan = [0.0, 0.0];
            }
        }
    }
    // Middle / secondary mouse drag = pan. Egui's `Response::dragged`
    // only reports primary drags by default; we read the raw button
    // state from `Input` so brush-tool primary drags don't double up
    // as pans.
    let middle_down = ui.input(|i| i.pointer.middle_down());
    let secondary_down = ui.input(|i| i.pointer.secondary_down());
    if (middle_down || secondary_down) && response.hovered() {
        let delta = ui.input(|i| i.pointer.delta());
        if delta.length() > 0.01 {
            state.image_brush.preview_pan[0] += delta.x;
            state.image_brush.preview_pan[1] += delta.y;
        }
    }
}

// ─── BAKE & SAVE ──────────────────────────────────────────────────────

/// Decode the source image, run the overlay's full effect stack on
/// the CPU (mirrors the canvas / export pipeline), apply the
/// resulting Crop inset by slicing the buffer, and hand the bytes
/// off to [`EditorState::save_edited_image_to_library`]. Returns the
/// new file's stem on success — the caller turns that into a status
/// toast so the user can find the file in the Images library tab.
fn bake_and_save_edited_image(
    state: &mut EditorState,
    overlay_idx: usize,
    source: &std::path::Path,
) -> Result<String, String> {
    use memstroy_core::Overlay;

    let effects = match state.scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) => im.effects.clone(),
        _ => return Err("not an image overlay".to_string()),
    };
    let img = image::open(source)
        .map_err(|e| format!("decode {}: {}", source.display(), e))?
        .to_rgba8();
    let w = img.width();
    let h = img.height();
    let mut buf = img.into_raw();
    let crop = crate::image_effects::apply_effect_stack(&mut buf, w, h, &effects, 0.0);

    // Apply the accumulated crop inset by slicing the buffer to the
    // visible rectangle. Without this, the saved PNG would still
    // carry transparent / black borders the editor's preview hides
    // through UV trimming. We also clamp the inset to leave at least
    // one pixel in each dimension so a misconfigured Crop doesn't
    // produce a 0×N image.
    let (cl, ct, cr, cb) = crop;
    let left = ((cl * w as f32).round() as u32).min(w.saturating_sub(1));
    let top = ((ct * h as f32).round() as u32).min(h.saturating_sub(1));
    let right = ((cr * w as f32).round() as u32).min(w.saturating_sub(1));
    let bottom = ((cb * h as f32).round() as u32).min(h.saturating_sub(1));
    let new_w = w.saturating_sub(left + right).max(1);
    let new_h = h.saturating_sub(top + bottom).max(1);

    let cropped: Vec<u8> = if new_w == w && new_h == h {
        buf
    } else {
        let mut out = Vec::with_capacity((new_w * new_h * 4) as usize);
        for y in 0..new_h {
            let src_y = y + top;
            let row_start = ((src_y * w + left) * 4) as usize;
            let row_end = row_start + (new_w as usize) * 4;
            out.extend_from_slice(&buf[row_start..row_end]);
        }
        out
    };

    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let asset = state.save_edited_image_to_library(&cropped, new_w, new_h, stem)?;
    Ok(asset.id)
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
        ImageBrushTool::Crop
        | ImageBrushTool::RectMask
        | ImageBrushTool::EllipseMask => {
            // All three tools share the same drag-rectangle
            // workflow: anchor on press, update the second point on
            // drag, commit on release. The shape produced by the
            // commit step differs (`Crop` inset / `Mask{Rect}` /
            // `Mask{Ellipse}`) — see `commit_drag_rectangle`.
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
                commit_drag_rectangle(state, overlay_idx);
            }
        }
        ImageBrushTool::Eyedropper => {
            // Single-click sample: read the source pixel under the
            // cursor and write it into a `ColorKey` effect entry.
            // Using `clicked()` (rather than `drag_started()`) gives
            // us the standard click semantics so a fast tap is
            // enough to trigger the sampler.
            if response.clicked() {
                if let Some(p) = pointer {
                    if let Some(uv) = screen_to_source_uv(p, img_rect, crop_inset) {
                        commit_eyedropper(state, overlay_idx, uv);
                    }
                }
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

fn commit_drag_rectangle(state: &mut EditorState, overlay_idx: usize) {
    // Pulls the drag draft out, validates that the user actually
    // drew something larger than a misclick, and dispatches to the
    // shape-specific commit step based on the active tool.
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

    match state.image_brush.tool {
        ImageBrushTool::Crop => {
            // Convert the rect drawn on the picture into Crop-effect
            // insets: we discard the bands OUTSIDE [lx..rx] × [ty..by].
            // Each value is clamped to 0..0.49 to match
            // `EffectKind::Crop`'s contract.
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
        ImageBrushTool::RectMask => {
            let feather = state.image_brush.feather.clamp(0.0, 0.5);
            let invert = state.image_brush.invert;
            if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
                // Replace the most recent Rect mask if any so
                // successive drags redraw the same shape — leave
                // ellipse / polygon mask entries alone (they're a
                // different authoring surface).
                if let Some(idx) = im.effects.iter().position(|e| {
                    matches!(
                        &e.kind,
                        EffectKind::Mask {
                            shape: MaskShape::Rect { .. },
                            ..
                        }
                    )
                }) {
                    im.effects.remove(idx);
                }
                im.effects.push(Effect::new(EffectKind::Mask {
                    shape: MaskShape::Rect {
                        left: lx,
                        top: ty,
                        right: rx,
                        bottom: by,
                    },
                    feather,
                    invert,
                }));
            }
        }
        ImageBrushTool::EllipseMask => {
            let feather = state.image_brush.feather.clamp(0.0, 0.5);
            let invert = state.image_brush.invert;
            let cx = (lx + rx) * 0.5;
            let cy = (ty + by) * 0.5;
            let rxx = ((rx - lx) * 0.5).max(0.005);
            let ryy = ((by - ty) * 0.5).max(0.005);
            if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
                if let Some(idx) = im.effects.iter().position(|e| {
                    matches!(
                        &e.kind,
                        EffectKind::Mask {
                            shape: MaskShape::Ellipse { .. },
                            ..
                        }
                    )
                }) {
                    im.effects.remove(idx);
                }
                im.effects.push(Effect::new(EffectKind::Mask {
                    shape: MaskShape::Ellipse {
                        cx,
                        cy,
                        rx: rxx,
                        ry: ryy,
                    },
                    feather,
                    invert,
                }));
            }
        }
        _ => {}
    }
}

/// Decode the source image, sample the pixel at `uv`, and push (or
/// update) an `EffectKind::ColorKey` entry on the overlay's effect
/// stack. The decode is one-shot per click so we don't carry the
/// raw pixel buffer around in `EditorState` — sampling once on
/// commit is plenty fast for the small overlay PNGs the editor
/// targets, and skipping the cache keeps the eyedropper path
/// independent of the texture / FX caches.
fn commit_eyedropper(state: &mut EditorState, overlay_idx: usize, uv: [f32; 2]) {
    let source = match state.scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) => im.source.clone(),
        _ => return,
    };
    let img = match image::open(&source) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            state.status = format!(
                "{} {}",
                crate::i18n::t("\u{274C} Eyedropper decode failed:"),
                e,
            );
            return;
        }
    };
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return;
    }
    let px = ((uv[0] * w as f32) as u32).min(w - 1);
    let py = ((uv[1] * h as f32) as u32).min(h - 1);
    let p = img.get_pixel(px, py);
    let rgb = [p[0], p[1], p[2]];

    if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
        let invert = state.image_brush.invert;
        if let Some(idx) = im
            .effects
            .iter()
            .position(|e| matches!(e.kind, EffectKind::ColorKey { .. }))
        {
            // Keep the user's tuned similarity / blend / spill
            // values — only the colour and the invert flag move on
            // resample, so a "re-pick" doesn't reset the dial.
            if let EffectKind::ColorKey {
                color,
                invert: existing_invert,
                ..
            } = &mut im.effects[idx].kind
            {
                *color = rgb;
                *existing_invert = invert;
            }
        } else {
            im.effects.push(Effect::new(EffectKind::ColorKey {
                color: rgb,
                similarity: 0.18,
                blend: 0.10,
                spill: 0.0,
                invert,
            }));
        }
    }
    state.status = format!(
        "{} #{:02X}{:02X}{:02X}",
        crate::i18n::t("\u{1F4A7} Eyedropper picked colour:"),
        rgb[0],
        rgb[1],
        rgb[2],
    );
}

/// Paint the in-progress polygon / rect / ellipse on top of the
/// preview so the user can see what the next mouse-release will
/// commit. Uses crop-aware mapping (UV → screen) symmetric with the
/// input handler.
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
        ImageBrushTool::RectMask | ImageBrushTool::EllipseMask => {
            Color32::from_rgb(200, 140, 255)
        }
        ImageBrushTool::Eyedropper => Color32::from_rgb(255, 220, 80),
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
        ImageBrushTool::Crop | ImageBrushTool::RectMask => {
            if state.image_brush.draft.len() >= 2 {
                let a = to_screen(state.image_brush.draft[0]);
                let b = to_screen(state.image_brush.draft[state.image_brush.draft.len() - 1]);
                let r = Rect::from_two_pos(a, b);
                painter.rect_stroke(r, Rounding::same(2.0), Stroke::new(1.5, stroke_col));
                let fill = match state.image_brush.tool {
                    ImageBrushTool::Crop => {
                        Color32::from_rgba_unmultiplied(120, 220, 255, 30)
                    }
                    _ => Color32::from_rgba_unmultiplied(200, 140, 255, 30),
                };
                painter.rect_filled(r, Rounding::same(2.0), fill);
            }
        }
        ImageBrushTool::EllipseMask => {
            if state.image_brush.draft.len() >= 2 {
                let a = to_screen(state.image_brush.draft[0]);
                let b = to_screen(state.image_brush.draft[state.image_brush.draft.len() - 1]);
                let r = Rect::from_two_pos(a, b);
                let center = r.center();
                let radius = Vec2::new(r.width().abs() * 0.5, r.height().abs() * 0.5);
                // Approximate the ellipse with a polyline so we don't
                // need an egui ellipse primitive (which doesn't exist
                // at the `Shape` level). 48 segments is plenty smooth
                // at typical preview sizes.
                let n = 48;
                let mut pts: Vec<Pos2> = Vec::with_capacity(n + 1);
                for i in 0..=n {
                    let theta = (i as f32 / n as f32) * std::f32::consts::TAU;
                    pts.push(Pos2::new(
                        center.x + radius.x * theta.cos(),
                        center.y + radius.y * theta.sin(),
                    ));
                }
                painter.add(egui::Shape::line(pts, Stroke::new(1.5, stroke_col)));
                // Crosshair on the centre so the user sees where the
                // ellipse will land.
                painter.circle_stroke(center, 2.0, Stroke::new(1.0, stroke_col));
            }
        }
        ImageBrushTool::Eyedropper => {
            // Draw a crosshair at the cursor so the user knows the
            // tool is armed and which pixel will be sampled. The
            // hover position is read from the painter's clip rect via
            // egui's input layer in the top-level `image_editor_content`,
            // but we don't have it here — leave the indicator passive
            // (just a hint label outside).
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
            // Emoji-arrow glyphs (⭮ ⭯) outside the bundled font's
            // coverage rendered as missing-glyph boxes; the basic
            // bidirectional arrows below ship with every desktop
            // font set.
            if let Some(Overlay::Image(im)) = state.scene.overlays.get_mut(overlay_idx) {
                mirror_button(ui, &mut im.effects, "\u{2194} Mirror H", true);
                mirror_button(ui, &mut im.effects, "\u{2195} Mirror V", false);
            }

            // Rotate buttons mutate the layout's first keyframe so
            // the change persists and is keyframe-friendly. We only
            // touch the static value — animation curves stay intact.
            if ui
                .button(crate::i18n::t("\u{21BA} -90\u{00B0}"))
                .on_hover_text(crate::i18n::t("Rotate 90° counter-clockwise"))
                .clicked()
            {
                rotate_overlay(state, overlay_idx, -90.0);
            }
            if ui
                .button(crate::i18n::t("\u{21BB} +90\u{00B0}"))
                .on_hover_text(crate::i18n::t("Rotate 90° clockwise"))
                .clicked()
            {
                rotate_overlay(state, overlay_idx, 90.0);
            }
            if ui
                .button(crate::i18n::t("180\u{00B0}"))
                .on_hover_text(crate::i18n::t("Flip 180°"))
                .clicked()
            {
                rotate_overlay(state, overlay_idx, 180.0);
            }
            if ui
                .button(crate::i18n::t("Reset"))
                .on_hover_text(crate::i18n::t("Reset rotation to 0°"))
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
        if ui.small_button("\u{21BA}").on_hover_text(crate::i18n::t("Reset")).clicked() {
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
        if ui.small_button("\u{21BA}").on_hover_text(crate::i18n::t("Reset")).clicked() {
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


// ─── EFFECTS OVERVIEW ────────────────────────────────────────────────

/// Read the source image's pixel dimensions from the texture cache.
/// Returns `None` when the image hasn't been decoded yet — callers
/// fall back to a 1:1 assumption in that case so the editor doesn't
/// stall on a first-paint race.
fn source_pixel_size(state: &EditorState, overlay_idx: usize) -> Option<[u32; 2]> {
    let source = match state.scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(im)) => im.source.clone(),
        _ => return None,
    };
    state
        .image_textures
        .lock()
        .ok()
        .and_then(|map| match map.get(&source) {
            Some(crate::state::ImageTextureSlot::Loaded { size, .. }) => Some(*size),
            _ => None,
        })
}

/// Render a compact, top-of-scroll-area list of every currently-applied
/// effect on the selected overlay, with per-row enable / disable
/// toggles, intensity slider, and a delete button. The remaining
/// section panels still expose richer parameter controls — this view's
/// job is to give the user a single place to *see* what's stacked
/// (otherwise a collapsed section would hide that an effect is on),
/// to mute / un-mute entries without losing their tuned parameters,
/// and to remove an effect cleanly without hunting through sections.
///
/// Renders nothing when the stack is empty so the editor's first
/// impression remains the preview pane and the tool toolbar.
fn effects_overview_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    if effects.is_empty() {
        // Skip the header entirely so an unedited image gets a clean
        // first impression. The downstream sections still let the
        // user add effects.
        return;
    }
    let count = effects.len();
    egui::CollapsingHeader::new(
        RichText::new(format!(
            "{} ({})",
            crate::i18n::t("Active effects"),
            count,
        ))
        .size(12.0)
        .strong()
        .color(Color32::from_rgb(220, 230, 255)),
    )
    .id_source(("image_editor_overview", salt))
    .default_open(false)
    .show(ui, |ui| {
        // Header row: "Reset all" + "Mute all" / "Unmute all".
        let mut reset_all = false;
        let mut mute_all = false;
        let mut unmute_all = false;
        ui.horizontal(|ui| {
            if ui
                .button(
                    RichText::new(crate::i18n::t("Reset all effects"))
                        .color(Color32::from_rgb(255, 160, 160)),
                )
                .on_hover_text(crate::i18n::t(
                    "Remove every effect on this image. The picture returns to the original decoded pixels.",
                ))
                .clicked()
            {
                reset_all = true;
            }
            if ui
                .button(crate::i18n::t("Mute all"))
                .on_hover_text(crate::i18n::t(
                    "Disable every effect without removing it. Click again on individual rows to re-enable.",
                ))
                .clicked()
            {
                mute_all = true;
            }
            if ui.button(crate::i18n::t("Unmute all")).clicked() {
                unmute_all = true;
            }
        });
        ui.add_space(4.0);

        if reset_all {
            effects.clear();
            return;
        }
        if mute_all {
            for e in effects.iter_mut() {
                e.enabled = false;
            }
        }
        if unmute_all {
            for e in effects.iter_mut() {
                e.enabled = true;
            }
        }

        // Per-effect rows. Build a deletion / reorder index outside
        // the loop so we don't mutate `effects` while iterating.
        let mut to_remove: Option<usize> = None;
        let mut to_move_up: Option<usize> = None;
        let mut to_move_down: Option<usize> = None;
        for (idx, eff) in effects.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut eff.enabled, "")
                    .on_hover_text(crate::i18n::t(
                        "Mute / un-mute this effect without removing it.",
                    ));
                ui.label(
                    RichText::new(eff.kind.label())
                        .size(11.0)
                        .color(if eff.enabled {
                            Color32::from_rgb(220, 220, 240)
                        } else {
                            Color32::from_rgb(120, 120, 140)
                        }),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{1F5D1}")
                        .on_hover_text(crate::i18n::t("Remove this effect"))
                        .clicked()
                    {
                        to_remove = Some(idx);
                    }
                    if ui
                        .small_button("\u{2193}")
                        .on_hover_text(crate::i18n::t("Move down (later in stack)"))
                        .clicked()
                    {
                        to_move_down = Some(idx);
                    }
                    if ui
                        .small_button("\u{2191}")
                        .on_hover_text(crate::i18n::t("Move up (earlier in stack)"))
                        .clicked()
                    {
                        to_move_up = Some(idx);
                    }
                    // Master intensity slider is universal across all
                    // effect kinds — gives the user a single dial to
                    // fade an entry up/down without re-tuning its
                    // inner parameters.
                    ui.add(
                        egui::Slider::new(&mut eff.intensity, 0.0..=1.0)
                            .show_value(false),
                    )
                    .on_hover_text(crate::i18n::t("Master intensity"));
                });
            });
        }
        if let Some(i) = to_remove {
            if i < effects.len() {
                effects.remove(i);
            }
        }
        if let Some(i) = to_move_up {
            if i > 0 && i < effects.len() {
                effects.swap(i, i - 1);
            }
        }
        if let Some(i) = to_move_down {
            if i + 1 < effects.len() {
                effects.swap(i, i + 1);
            }
        }
    });
}

// ─── ASPECT-RATIO QUICK CROP ─────────────────────────────────────────

/// Quick-crop section: one-click buttons that crop the image to a
/// fixed aspect ratio (1:1, 4:3, 3:4, 16:9, 9:16, 5:4, 4:5, 21:9,
/// "Story", "Square") relative to the source pixel dimensions.
///
/// The button computes the inset needed to land on the target ratio
/// while keeping the picture centred (left/right or top/bottom inset
/// is split evenly), then writes a single `EffectKind::Crop` entry
/// — replacing any previous crop so successive clicks cleanly switch
/// between aspect ratios. The crop sliders above stay live so the
/// user can still nudge the rectangle off-centre afterwards.
fn aspect_ratio_section(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    source_size: Option<[u32; 2]>,
    salt: usize,
) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Aspect ratio crop"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(140, 200, 255)),
    )
    .id_source(("image_editor_aspect", salt))
    .default_open(false)
    .show(ui, |ui| {
        ui.label(
            RichText::new(crate::i18n::t(
                "Crop the picture to a target aspect ratio (centred).",
            ))
            .size(10.5)
            .italics()
            .color(Color32::from_rgb(140, 140, 170)),
        );
        ui.add_space(2.0);
        // (label, target_w, target_h) — sorted in the canonical
        // sticker / story / cinema order so the user finds the most
        // common ratios near the top.
        const PRESETS: &[(&str, f32, f32)] = &[
            ("1:1", 1.0, 1.0),
            ("4:5", 4.0, 5.0),
            ("5:4", 5.0, 4.0),
            ("4:3", 4.0, 3.0),
            ("3:4", 3.0, 4.0),
            ("3:2", 3.0, 2.0),
            ("2:3", 2.0, 3.0),
            ("16:9", 16.0, 9.0),
            ("9:16", 9.0, 16.0),
            ("21:9", 21.0, 9.0),
        ];
        // Source size — fallback to (1, 1) when the image hasn't
        // decoded yet so the math still produces a sane inset.
        let (sw, sh) = match source_size {
            Some([w, h]) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (1.0_f32, 1.0_f32),
        };
        ui.horizontal_wrapped(|ui| {
            for (label, tw, th) in PRESETS {
                if ui
                    .button(crate::i18n::t(label))
                    .on_hover_text(crate::i18n::t(
                        "Apply a centred crop to land on this aspect ratio.",
                    ))
                    .clicked()
                {
                    apply_aspect_ratio_crop(effects, sw, sh, *tw, *th);
                }
            }
        });
    });
}

/// Compute the centred Crop inset needed to make the image's visible
/// rectangle match `(target_w, target_h)` aspect, then upsert it on
/// the effect stack. Replaces any existing Crop entry so successive
/// aspect-ratio clicks cleanly switch the framing instead of
/// stacking inset on top of inset. Removes the entry entirely when
/// the target matches the source aspect (no crop needed).
fn apply_aspect_ratio_crop(
    effects: &mut Vec<Effect>,
    source_w: f32,
    source_h: f32,
    target_w: f32,
    target_h: f32,
) {
    let src_aspect = (source_w / source_h.max(1.0)).max(1e-3);
    let tgt_aspect = (target_w / target_h.max(1.0)).max(1e-3);
    let (mut left, mut top, mut right, mut bottom) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
    if (tgt_aspect - src_aspect).abs() < 1e-3 {
        // Same aspect — clear any existing crop.
    } else if tgt_aspect > src_aspect {
        // Target is wider than source → trim top/bottom evenly.
        let visible_v_frac = (src_aspect / tgt_aspect).clamp(0.02, 1.0);
        let crop_each = ((1.0 - visible_v_frac) * 0.5).clamp(0.0, 0.49);
        top = crop_each;
        bottom = crop_each;
    } else {
        // Target is taller than source → trim left/right evenly.
        let visible_h_frac = (tgt_aspect / src_aspect).clamp(0.02, 1.0);
        let crop_each = ((1.0 - visible_h_frac) * 0.5).clamp(0.0, 0.49);
        left = crop_each;
        right = crop_each;
    }
    let no_crop = left == 0.0 && top == 0.0 && right == 0.0 && bottom == 0.0;
    let kind = EffectKind::Crop {
        left,
        top,
        right,
        bottom,
    };
    if let Some(idx) = effects
        .iter()
        .position(|e| matches!(e.kind, EffectKind::Crop { .. }))
    {
        if no_crop {
            effects.remove(idx);
        } else {
            effects[idx].kind = kind;
        }
    } else if !no_crop {
        effects.push(Effect::new(kind));
    }
}

// ─── LOOKBOOK / FILTER PRESETS ───────────────────────────────────────

/// One-click multi-effect filter combinations. Each preset writes a
/// curated set of single-fx entries that together produce a
/// recognisable look (cinematic warm grade, faded film, punchy pop,
/// dramatic high-contrast, …). Re-clicking the same preset overwrites
/// its constituent entries with the preset's tuned values, so the
/// user can iterate by tweaking sliders and then "snap back" to the
/// preset's defaults without rebuilding the stack from scratch.
///
/// Presets compose with the user's other settings — they only touch
/// the kinds they care about, leaving Crop / Mask / Mirror / etc.
/// alone. The dedicated "Reset all effects" button in the overview
/// section is the escape hatch when the user wants to start fresh.
fn presets_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, salt: usize) {
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Lookbook / Presets"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(255, 200, 220)),
    )
    .id_source(("image_editor_presets", salt))
    .default_open(false)
    .show(ui, |ui| {
        ui.label(
            RichText::new(crate::i18n::t(
                "One-click multi-effect looks. Each preset replaces the entries it cares about; other effects stay.",
            ))
            .size(10.5)
            .italics()
            .color(Color32::from_rgb(140, 140, 170)),
        );
        ui.add_space(3.0);
        ui.horizontal_wrapped(|ui| {
            preset_button(ui, effects, "Cinematic", &[
                EffectKind::Contrast { amount: 0.25 },
                EffectKind::Saturation { amount: -0.15 },
                EffectKind::HueShift { degrees: -10.0 },
                EffectKind::Vignette { strength: 0.4 },
            ]);
            preset_button(ui, effects, "Warm", &[
                EffectKind::HueShift { degrees: 12.0 },
                EffectKind::Saturation { amount: 0.18 },
                EffectKind::Brightness { amount: 0.05 },
            ]);
            preset_button(ui, effects, "Cool", &[
                EffectKind::HueShift { degrees: -18.0 },
                EffectKind::Saturation { amount: 0.05 },
                EffectKind::Brightness { amount: 0.02 },
            ]);
            preset_button(ui, effects, "Punchy", &[
                EffectKind::Contrast { amount: 0.4 },
                EffectKind::Saturation { amount: 0.4 },
                EffectKind::Sharpen { amount: 0.6 },
            ]);
            preset_button(ui, effects, "Faded", &[
                EffectKind::Contrast { amount: -0.25 },
                EffectKind::Saturation { amount: -0.25 },
                EffectKind::Brightness { amount: 0.08 },
            ]);
            preset_button(ui, effects, "Vintage", &[
                EffectKind::Sepia,
                EffectKind::Vignette { strength: 0.5 },
                EffectKind::Noise { amount: 0.18 },
                EffectKind::Contrast { amount: -0.1 },
            ]);
            preset_button(ui, effects, "Dramatic", &[
                EffectKind::Contrast { amount: 0.5 },
                EffectKind::Saturation { amount: 0.2 },
                EffectKind::Vignette { strength: 0.7 },
            ]);
            preset_button(ui, effects, "Dreamy", &[
                EffectKind::Bloom { radius: 22.0 },
                EffectKind::Brightness { amount: 0.12 },
                EffectKind::Saturation { amount: 0.1 },
            ]);
            preset_button(ui, effects, "B&W high contrast", &[
                EffectKind::Grayscale,
                EffectKind::Contrast { amount: 0.5 },
            ]);
            preset_button(ui, effects, "Sketch", &[
                EffectKind::Grayscale,
                EffectKind::EdgeDetect { threshold: 0.3 },
            ]);
            preset_button(ui, effects, "Cyberpunk", &[
                EffectKind::HueShift { degrees: -40.0 },
                EffectKind::Saturation { amount: 0.5 },
                EffectKind::ChromaticAberration { offset: 3.0 },
                EffectKind::Bloom { radius: 14.0 },
            ]);
            preset_button(ui, effects, "Pastel", &[
                EffectKind::Saturation { amount: -0.3 },
                EffectKind::Brightness { amount: 0.15 },
                EffectKind::Bloom { radius: 8.0 },
            ]);
        });
    });
}

/// Apply a set of `EffectKind` entries to the stack, replacing any
/// existing entry of the same discriminant in place (so re-clicking
/// a preset converges on the preset's intended values without
/// duplicating). When no entry of that kind exists yet, the new one
/// is pushed onto the back of the stack.
fn preset_button(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    label: &'static str,
    entries: &[EffectKind],
) {
    let btn = egui::Button::new(
        RichText::new(crate::i18n::t(label))
            .size(11.0)
            .color(Color32::from_rgb(245, 230, 240)),
    )
    .fill(Color32::from_rgb(70, 50, 70))
    .stroke(Stroke::new(1.0, Color32::from_rgb(150, 90, 130)))
    .rounding(Rounding::same(4.0))
    .min_size(Vec2::new(72.0, 22.0));
    if ui
        .add(btn)
        .on_hover_text(crate::i18n::t(
            "Apply this preset's effects (replaces matching entries; leaves other effects alone).",
        ))
        .clicked()
    {
        for kind in entries {
            let disc = std::mem::discriminant(kind);
            if let Some(idx) = effects
                .iter()
                .position(|e| std::mem::discriminant(&e.kind) == disc)
            {
                effects[idx].kind = kind.clone();
                effects[idx].enabled = true;
                effects[idx].intensity = 1.0;
            } else {
                effects.push(Effect::new(kind.clone()));
            }
        }
    }
}

// ─── COLOUR-KEY SECTION ──────────────────────────────────────────────

/// When the overlay's effect stack contains a `ColorKey` entry (i.e.
/// the user has used the eyedropper or imported a scene with a
/// pre-applied chroma key), expose a dedicated section with the
/// colour swatch and the FFmpeg-chromakey-mirror sliders
/// (similarity / blend / spill / invert) plus a "Re-pick" button
/// that arms the eyedropper for a fresh sample.
///
/// The section auto-hides when no ColorKey entry exists. Returns
/// `true` when the user clicked "Re-pick" — the caller then arms
/// the Eyedropper tool (we can't do it here because we hold a
/// mutable borrow of `effects` and arming the tool needs a
/// mutable borrow of `state`).
fn color_key_section(
    ui: &mut egui::Ui,
    effects: &mut Vec<Effect>,
    salt: usize,
) -> bool {
    let has_color_key = effects
        .iter()
        .any(|e| matches!(e.kind, EffectKind::ColorKey { .. }));
    if !has_color_key {
        return false;
    }
    let mut activate_eyedropper = false;
    egui::CollapsingHeader::new(
        RichText::new(crate::i18n::t("Colour key (eyedropper)"))
            .size(12.0)
            .strong()
            .color(Color32::from_rgb(255, 220, 140)),
    )
    .id_source(("image_editor_colorkey", salt))
    .default_open(true)
    .show(ui, |ui| {
        let ck_idx = effects
            .iter()
            .position(|e| matches!(e.kind, EffectKind::ColorKey { .. }));
        let mut to_remove: Option<usize> = None;
        if let Some(idx) = ck_idx {
            if let EffectKind::ColorKey {
                color,
                similarity,
                blend,
                spill,
                invert,
            } = &mut effects[idx].kind
            {
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::t("Key colour"));
                    let mut rgb = [
                        color[0] as f32 / 255.0,
                        color[1] as f32 / 255.0,
                        color[2] as f32 / 255.0,
                    ];
                    if ui.color_edit_button_rgb(&mut rgb).changed() {
                        color[0] = (rgb[0] * 255.0).round().clamp(0.0, 255.0) as u8;
                        color[1] = (rgb[1] * 255.0).round().clamp(0.0, 255.0) as u8;
                        color[2] = (rgb[2] * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                    ui.label(format!(
                        "#{:02X}{:02X}{:02X}",
                        color[0], color[1], color[2]
                    ));
                    if ui
                        .button(crate::i18n::t("\u{1F4A7} Re-pick"))
                        .on_hover_text(crate::i18n::t(
                            "Arms the eyedropper — next click on the preview resamples the colour.",
                        ))
                        .clicked()
                    {
                        activate_eyedropper = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::t("Similarity"));
                    ui.add(egui::Slider::new(similarity, 0.0..=1.0))
                        .on_hover_text(crate::i18n::t(
                            "How wide a colour band around the key counts as a match.",
                        ));
                });
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::t("Blend"));
                    ui.add(egui::Slider::new(blend, 0.0..=1.0))
                        .on_hover_text(crate::i18n::t(
                            "Soften the edge of the keyed region.",
                        ));
                });
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::t("Spill"));
                    ui.add(egui::Slider::new(spill, 0.0..=1.0))
                        .on_hover_text(crate::i18n::t(
                            "De-spill: pulls the keyed colour out of remaining edges (export-only).",
                        ));
                });
                ui.checkbox(invert, crate::i18n::t("Invert (keep matching)"))
                    .on_hover_text(crate::i18n::t(
                        "When checked, pixels that DO match the colour are kept and the rest is masked away.",
                    ));
            }
            if ui
                .button(
                    RichText::new(crate::i18n::t("Remove colour key"))
                        .color(Color32::from_rgb(255, 140, 140)),
                )
                .clicked()
            {
                to_remove = Some(idx);
            }
        }
        if let Some(i) = to_remove {
            effects.remove(i);
        }
    });
    activate_eyedropper
}
