//! Skeleton Editor window — frame-by-frame point placement UI.
//!
//! Opens as a floating egui::Window. The user selects a clip from the
//! library, navigates frame-by-frame on a proper time ruler with zoom,
//! and places/drags named anchor points directly on the preview frame.
//! Points are stored as keyframes in a SkeletonTemplate.

use std::sync::{Arc, Mutex};

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::EditorState;
use crate::video_cache::FrameCache;

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_POINT_DEFAULT: Color32 = Color32::from_rgb(255, 100, 100);
const COL_POINT_SELECTED: Color32 = Color32::from_rgb(255, 220, 80);
const COL_FRAME_BG: Color32 = Color32::from_rgb(20, 20, 30);
const COL_RULER: Color32 = Color32::from_rgb(32, 32, 44);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 60, 60);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 160);
const COL_TEXT: Color32 = Color32::from_rgb(220, 220, 240);

/// Persistent state for the skeleton editor (lives in EditorState).
pub struct SkeletonEditorState {
    /// Whether the skeleton editor window is open.
    pub open: bool,
    /// Path of the source clip currently being edited.
    pub clip_path: Option<std::path::PathBuf>,
    /// Optional bound actor (used as a hint to find an existing frame
    /// cache from the main editor).
    pub actor_idx: Option<usize>,
    /// Index of the skeleton template being edited (in scene.skeleton_templates).
    pub template_idx: Option<usize>,
    /// Currently selected point name.
    pub selected_point: Option<String>,
    /// Current frame index for navigation.
    pub current_frame: u32,
    /// Total frame count (from frame cache or clip duration * fps).
    pub total_frames: u32,
    /// FPS used for frame navigation.
    pub fps: f32,
    /// New point name input buffer.
    pub new_point_name: String,
    /// Place mode — clicking on the frame creates / updates the selected
    /// point's keyframe at the current time.
    pub place_mode: bool,
    /// Name of the point currently being dragged on the preview frame.
    pub dragging_point: Option<String>,
    /// Per-clip frame cache used for the live preview when no scene actor
    /// is bound to the selected clip. Built lazily on clip change.
    pub preview_cache: Option<FrameCache>,
    /// Background extraction slot used by `preview_cache`.
    pub extract_slot: Option<Arc<Mutex<Option<(f32, usize, std::path::PathBuf)>>>>,
    /// Local time ruler zoom (pixels per second).
    pub timeline_zoom: f32,
    /// Local time ruler scroll offset (seconds).
    pub timeline_scroll: f32,
}

impl Default for SkeletonEditorState {
    fn default() -> Self {
        Self {
            open: false,
            clip_path: None,
            actor_idx: None,
            template_idx: None,
            selected_point: None,
            current_frame: 0,
            total_frames: 1,
            fps: 30.0,
            new_point_name: String::new(),
            place_mode: false,
            dragging_point: None,
            preview_cache: None,
            extract_slot: None,
            timeline_zoom: 80.0,
            timeline_scroll: 0.0,
        }
    }
}

impl SkeletonEditorState {
    pub fn current_time(&self) -> f32 {
        self.current_frame as f32 / self.fps.max(1.0)
    }

    pub fn duration(&self) -> f32 {
        self.total_frames as f32 / self.fps.max(1.0)
    }
}

// ─── MAIN WINDOW ─────────────────────────────────────────────────────

/// Render the skeleton editor window. Returns whether it remains open.
pub fn skeleton_editor_window(ctx: &egui::Context, state: &mut EditorState) -> bool {
    if !state.skeleton_editor.open {
        return false;
    }

    // Poll any in-flight frame extraction so the preview is ready as soon
    // as ffmpeg finishes. Done outside the Window block to keep the borrow
    // scope small.
    poll_preview_extraction(state);

    let mut open = state.skeleton_editor.open;
    let screen_rect = ctx.input(|i| i.screen_rect());
    let max_w = (screen_rect.width() - 40.0).max(500.0);
    let max_h = (screen_rect.height() - 60.0).max(420.0);

    egui::Window::new("Skeleton Constructor")
        .open(&mut open)
        .default_size([900.0, 640.0])
        .min_width(560.0)
        .min_height(420.0)
        .max_width(max_w)
        .max_height(max_h)
        .resizable(true)
        .collapsible(true)
        .show(ctx, |ui| {
            skeleton_editor_content(ui, state);
        });

    state.skeleton_editor.open = open;
    open
}

fn skeleton_editor_content(ui: &mut egui::Ui, state: &mut EditorState) {
    skeleton_toolbar(ui, state);
    ui.separator();

    if state.skeleton_editor.template_idx.is_none() {
        ui.add_space(20.0);
        ui.label(
            RichText::new(
                "Pick a clip from the library above and create a skeleton template.\n\
                The skeleton is saved alongside the source clip as <name>.skeleton.json so it follows the asset across projects.",
            )
            .italics()
            .color(COL_TEXT_DIM),
        );
        return;
    }

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let right_panel_w = 220.0_f32;
                let separator_w = 12.0_f32;
                let nav_height = 36.0_f32;
                let ruler_height = 26.0_f32;
                let avail = ui.available_size_before_wrap();
                let preview_max_w = (avail.x - right_panel_w - separator_w).max(220.0);
                let preview_max_h = (avail.y - nav_height - ruler_height - 8.0).max(200.0);

                let aspect = 9.0_f32 / 16.0;
                let by_w_h = preview_max_w / aspect;
                let (pw, ph) = if by_w_h <= preview_max_h {
                    (preview_max_w, by_w_h)
                } else {
                    (preview_max_h * aspect, preview_max_h)
                };

                ui.vertical(|ui| {
                    frame_preview(ui, state, pw, ph);
                    ui.add_space(4.0);
                    timeline_ruler(ui, state, pw);
                    ui.add_space(2.0);
                    frame_navigation(ui, state);
                });

                ui.separator();

                ui.vertical(|ui| {
                    ui.set_min_width(right_panel_w - 20.0);
                    point_list_panel(ui, state);
                });
            });
        });
}

// ─── TOOLBAR ─────────────────────────────────────────────────────────

fn skeleton_toolbar(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Clip:").size(12.0).strong());

        let current_label = state
            .skeleton_editor
            .clip_path
            .as_ref()
            .and_then(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| "(none)".into());

        let lib_paths: Vec<std::path::PathBuf> = state
            .library
            .mellstroy_clips
            .iter()
            .map(|c| c.path.clone())
            .collect();

        let mut chosen: Option<std::path::PathBuf> = None;
        egui::ComboBox::from_id_source("skel_clip_select")
            .selected_text(&current_label)
            .show_ui(ui, |ui| {
                for path in &lib_paths {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("(?)")
                        .to_string();
                    let is_chosen = state.skeleton_editor.clip_path.as_ref() == Some(path);
                    if ui.selectable_label(is_chosen, &name).clicked() {
                        chosen = Some(path.clone());
                    }
                }
            });
        if let Some(path) = chosen {
            on_clip_changed(state, &path);
        }

        ui.separator();

        if state.skeleton_editor.clip_path.is_some() {
            if state.skeleton_editor.template_idx.is_none() {
                if ui
                    .button(
                        RichText::new("+ Create Skeleton")
                            .color(Color32::from_rgb(80, 200, 120)),
                    )
                    .clicked()
                {
                    create_template_for_current_clip(state);
                }
            } else if ui
                .button("Save")
                .on_hover_text("Save skeleton to <clip>.skeleton.json")
                .clicked()
            {
                save_current_template(state);
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Status hint
            let (msg, col) = if state.skeleton_editor.preview_cache.is_some() {
                let pc = state.skeleton_editor.preview_cache.as_ref().unwrap();
                if pc.is_ready() {
                    (
                        format!("preview {}f", pc.frame_count),
                        Color32::from_rgb(120, 180, 120),
                    )
                } else if pc.extracting {
                    ("extracting...".to_string(), Color32::from_rgb(255, 200, 80))
                } else {
                    ("preview pending".to_string(), COL_TEXT_DIM)
                }
            } else if state.skeleton_editor.actor_idx.is_some() {
                ("actor cache".to_string(), COL_TEXT_DIM)
            } else {
                ("no preview".to_string(), COL_TEXT_DIM)
            };
            ui.label(RichText::new(msg).size(10.0).color(col));
        });
    });
}

/// Public entry point used by `Tools → Skeleton Constructor` menu so the
/// editor opens with a sensible source clip pre-selected.
pub fn select_clip(state: &mut EditorState, clip_path: &std::path::Path) {
    on_clip_changed(state, clip_path);
}

/// Switch the editor to a new source clip. Auto-loads any existing sidecar
/// skeleton, otherwise leaves the user to create one. Also kicks off a
/// preview frame extraction if no actor cache for this clip exists.
fn on_clip_changed(state: &mut EditorState, clip_path: &std::path::Path) {
    state.skeleton_editor.clip_path = Some(clip_path.to_path_buf());
    state.skeleton_editor.selected_point = None;
    state.skeleton_editor.current_frame = 0;
    state.skeleton_editor.fps = 30.0;
    state.skeleton_editor.dragging_point = None;
    state.skeleton_editor.timeline_scroll = 0.0;

    state.skeleton_editor.actor_idx = state
        .scene
        .actors
        .iter()
        .position(|a| a.source == *clip_path);

    let mut total = 0u32;
    if let Some(ai) = state.skeleton_editor.actor_idx {
        let actor = &state.scene.actors[ai];
        let dur = actor.t_out.unwrap_or(state.scene.output.duration)
            - actor.t_in.unwrap_or(0.0);
        total = (dur * 30.0).ceil() as u32;
    }

    let mut tmpl_idx = state
        .scene
        .skeleton_templates
        .iter()
        .position(|t| t.source_clip == *clip_path);
    if tmpl_idx.is_none() {
        if let Some(template) = SkeletonTemplate::load_for_clip(clip_path) {
            if total == 0 && template.clip_duration > 0.0 {
                total = (template.clip_duration * template.fps).ceil() as u32;
            }
            state.scene.skeleton_templates.push(template);
            tmpl_idx = Some(state.scene.skeleton_templates.len() - 1);
        }
    } else if let Some(idx) = tmpl_idx {
        if total == 0 {
            let t = &state.scene.skeleton_templates[idx];
            total = (t.clip_duration * t.fps).ceil() as u32;
        }
    }
    state.skeleton_editor.template_idx = tmpl_idx;
    state.skeleton_editor.total_frames = total.max(1);

    // Kick off a dedicated preview cache when we don't have an actor-bound
    // one. The user requested previews for *any* library clip, not only
    // those already on the canvas.
    let actor_cache_ready = state
        .skeleton_editor
        .actor_idx
        .and_then(|i| state.frame_caches.get(i))
        .map(|fc| fc.is_ready())
        .unwrap_or(false);

    if !actor_cache_ready {
        start_preview_extraction(state, clip_path);
    } else {
        state.skeleton_editor.preview_cache = None;
        state.skeleton_editor.extract_slot = None;
    }
}

/// Kick off background extraction of a per-clip preview cache.
fn start_preview_extraction(state: &mut EditorState, clip_path: &std::path::Path) {
    let mut cache = FrameCache::new(clip_path.to_path_buf(), usize::MAX);
    cache.extracting = true;
    state.skeleton_editor.preview_cache = Some(cache);

    let slot: Arc<Mutex<Option<(f32, usize, std::path::PathBuf)>>> =
        Arc::new(Mutex::new(None));
    state.skeleton_editor.extract_slot = Some(slot.clone());

    if !clip_path.exists() {
        return;
    }
    let path = clip_path.to_path_buf();
    FrameCache::start_extraction_thread(path, move |duration, frame_count, cache_dir| {
        if let Ok(mut s) = slot.lock() {
            *s = Some((duration, frame_count, cache_dir));
        }
    });
}

/// Poll the extraction slot and finalize the preview cache when ready.
fn poll_preview_extraction(state: &mut EditorState) {
    let slot = match state.skeleton_editor.extract_slot.clone() {
        Some(s) => s,
        None => return,
    };
    let result = if let Ok(mut s) = slot.lock() { s.take() } else { None };
    if let Some((duration, frame_count, cache_dir)) = result {
        if let Some(cache) = state.skeleton_editor.preview_cache.as_mut() {
            cache.set_ready(duration, frame_count, cache_dir);
            // Update total frames using actual extracted count.
            if frame_count > 0 {
                state.skeleton_editor.total_frames = frame_count as u32;
                // Also update template metadata if it had an underestimate.
                if let Some(idx) = state.skeleton_editor.template_idx {
                    if let Some(t) = state.scene.skeleton_templates.get_mut(idx) {
                        if t.clip_duration <= 0.01 {
                            t.clip_duration = duration;
                        }
                    }
                }
            }
        }
        state.skeleton_editor.extract_slot = None;
    }
}

fn create_template_for_current_clip(state: &mut EditorState) {
    let Some(ref clip_path) = state.skeleton_editor.clip_path.clone() else {
        return;
    };
    let clip_duration = state
        .scene
        .actors
        .iter()
        .find(|a| a.source == *clip_path)
        .map(|a| a.t_out.unwrap_or(state.scene.output.duration) - a.t_in.unwrap_or(0.0))
        .or_else(|| {
            state
                .skeleton_editor
                .preview_cache
                .as_ref()
                .filter(|fc| fc.is_ready())
                .map(|fc| fc.duration)
        })
        .unwrap_or(3.0);

    let name = clip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("{}_skeleton", s))
        .unwrap_or_else(|| "skeleton".into());

    let template = SkeletonTemplate {
        name,
        source_clip: clip_path.clone(),
        fps: 30.0,
        clip_duration,
        points: Default::default(),
    };

    state.scene.skeleton_templates.push(template);
    state.skeleton_editor.template_idx = Some(state.scene.skeleton_templates.len() - 1);
    state.skeleton_editor.total_frames = (clip_duration * 30.0).ceil() as u32;
    save_current_template(state);
    state.status = "Skeleton template created.".into();
}

fn save_current_template(state: &mut EditorState) {
    let Some(idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let template = &state.scene.skeleton_templates[idx];
    match template.save_alongside_clip() {
        Ok(path) => state.status = format!("Skeleton saved: {}", path.display()),
        Err(e) => state.status = format!("Save failed: {}", e),
    }
}

// ─── FRAME PREVIEW ───────────────────────────────────────────────────

fn frame_preview(ui: &mut egui::Ui, state: &mut EditorState, width: f32, height: f32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, Rounding::same(4.0), COL_FRAME_BG);

    let t = state.skeleton_editor.current_time();
    let mut frame_shown = false;

    // 1) Try the dedicated preview cache (preferred — covers ANY library clip).
    if !frame_shown {
        if let Some(fc) = state.skeleton_editor.preview_cache.as_mut() {
            if fc.is_ready() {
                if let Some(tex) = fc.frame_at_time(t, ui.ctx()) {
                    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                    painter.image(tex.id(), rect, uv, Color32::WHITE);
                    frame_shown = true;
                }
            }
        }
    }

    // 2) Fallback: actor-bound cache from the main scene.
    if !frame_shown {
        if let Some(actor_idx) = state.skeleton_editor.actor_idx {
            if let Some(fc) = state.frame_caches.get_mut(actor_idx) {
                if fc.is_ready() {
                    let actor = &state.scene.actors[actor_idx];
                    let local_t = t + actor.source_start;
                    if let Some(tex) = fc.frame_at_time(local_t, ui.ctx()) {
                        let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                        painter.image(tex.id(), rect, uv, Color32::WHITE);
                        frame_shown = true;
                    }
                }
            }
        }
    }

    if !frame_shown {
        let extracting = state
            .skeleton_editor
            .preview_cache
            .as_ref()
            .map(|fc| fc.extracting)
            .unwrap_or(false);
        let msg = if extracting {
            "Extracting preview frames...".to_string()
        } else {
            format!("Frame {}", state.skeleton_editor.current_frame)
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            msg,
            egui::FontId::proportional(14.0),
            COL_TEXT_DIM,
        );
    }

    // ── Pointer interaction order ─────────────────────────────────────
    //
    // 1. If we're already dragging a point, update it (drag continues until
    //    the pointer is released even if it leaves the marker hit zone).
    // 2. Otherwise, on a fresh primary-press inside the rect, try to start
    //    a drag on a nearby point (or in place mode, drop a keyframe).
    // 3. On release, commit the drag.
    let pointer_pos = response.interact_pointer_pos();
    let pointer_in_rect = pointer_pos.map(|p| rect.contains(p)).unwrap_or(false);
    let primary_down = ui.input(|i| i.pointer.primary_down());
    let primary_released = ui.input(|i| i.pointer.any_released());

    let mut hovered_point: Option<String> = None;
    if pointer_in_rect && state.skeleton_editor.dragging_point.is_none() {
        if let (Some(pos), Some(tmpl_idx)) =
            (pointer_pos, state.skeleton_editor.template_idx)
        {
            let template = &state.scene.skeleton_templates[tmpl_idx];
            let mut best: Option<(String, f32)> = None;
            for (name, point) in &template.points {
                let ps = sample_point_at(point, t);
                let sx = rect.min.x + ps.x * rect.width();
                let sy = rect.min.y + ps.y * rect.height();
                let dist = ((pos.x - sx).powi(2) + (pos.y - sy).powi(2)).sqrt();
                if dist < 12.0 && best.as_ref().map(|b| dist < b.1).unwrap_or(true) {
                    best = Some((name.clone(), dist));
                }
            }
            if let Some((name, _)) = best {
                hovered_point = Some(name);
            }
        }
    }

    // Continue / start drag.
    if let Some(name) = state.skeleton_editor.dragging_point.clone() {
        // Active drag — keep updating while primary stays down.
        if primary_down {
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                place_point_at(state, &name, nx, ny);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        if primary_released || !primary_down {
            state.skeleton_editor.dragging_point = None;
        }
    } else if response.drag_started() && pointer_in_rect {
        // Begin a drag.
        if let Some(name) = hovered_point.clone() {
            state.skeleton_editor.selected_point = Some(name.clone());
            state.skeleton_editor.dragging_point = Some(name);
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                let dragging = state.skeleton_editor.dragging_point.clone().unwrap();
                place_point_at(state, &dragging, nx, ny);
            }
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if state.skeleton_editor.place_mode
            && state.skeleton_editor.selected_point.is_some()
        {
            // In place mode without hitting a point: drop a fresh keyframe at
            // the cursor for the currently selected point.
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                let name = state.skeleton_editor.selected_point.clone().unwrap();
                state.skeleton_editor.dragging_point = Some(name.clone());
                place_point_at(state, &name, nx, ny);
            }
        }
    } else if response.clicked() && pointer_in_rect {
        // Plain click without drag — selection or single-shot place.
        if let Some(name) = hovered_point.clone() {
            state.skeleton_editor.selected_point = Some(name);
        } else if state.skeleton_editor.place_mode
            && state.skeleton_editor.selected_point.is_some()
        {
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                let name = state.skeleton_editor.selected_point.clone().unwrap();
                place_point_at(state, &name, nx, ny);
            }
        }
    }

    // Hover cursor hint when hovering over a point.
    if hovered_point.is_some() && state.skeleton_editor.dragging_point.is_none() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    } else if state.skeleton_editor.place_mode
        && pointer_in_rect
        && state.skeleton_editor.dragging_point.is_none()
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }

    // Draw skeleton points on top of the frame.
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        let template = &state.scene.skeleton_templates[tmpl_idx];

        for (name, point) in &template.points {
            let point_state = sample_point_at(point, t);
            let screen_x = rect.min.x + point_state.x * rect.width();
            let screen_y = rect.min.y + point_state.y * rect.height();
            let pos = Pos2::new(screen_x, screen_y);

            let is_selected = state.skeleton_editor.selected_point.as_deref() == Some(name);
            let is_dragging = state.skeleton_editor.dragging_point.as_deref() == Some(name);
            let color = if is_selected || is_dragging {
                COL_POINT_SELECTED
            } else {
                Color32::from_rgb(point.color[0], point.color[1], point.color[2])
            };

            let radius = if is_selected || is_dragging { 9.0 } else { 6.5 };
            // Drop shadow for visibility on bright frames.
            painter.circle_filled(
                pos + Vec2::new(0.0, 1.0),
                radius + 0.5,
                Color32::from_black_alpha(180),
            );
            painter.circle_filled(pos, radius, color);
            painter.circle_stroke(pos, radius, Stroke::new(1.5, Color32::WHITE));

            painter.text(
                Pos2::new(pos.x + 11.0, pos.y - 6.0),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(11.0),
                color,
            );

            let has_kf = point.track.iter().any(|kf| (kf.t - t).abs() < 0.02);
            if has_kf {
                painter.text(
                    Pos2::new(pos.x, pos.y - radius - 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    "\u{25C6}",
                    egui::FontId::proportional(9.0),
                    Color32::from_rgb(255, 200, 50),
                );
            }
        }
    }

    // Border
    painter.rect_stroke(rect, Rounding::same(4.0), Stroke::new(1.0, Color32::from_rgb(60, 60, 80)));
}

/// Helper: sample a SkeletonPoint at time t (without needing &SkeletonTemplate).
pub fn sample_point_at(point: &SkeletonPoint, t: f32) -> PointState {
    keyframe::sample(&point.track, t).unwrap_or_default()
}

fn place_point_at(state: &mut EditorState, point_name: &str, nx: f32, ny: f32) {
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let t = state.skeleton_editor.current_time();
    let ps = PointState { x: nx, y: ny, scale: 1.0, rotation_deg: 0.0 };
    state.scene.skeleton_templates[tmpl_idx]
        .set_point_keyframe(point_name, t, ps, Easing::Linear);
    let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
    state.status = format!(
        "Point '{}' set at ({:.2}, {:.2}) t={:.2}s",
        point_name, nx, ny, t
    );
}

// ─── TIMELINE RULER ──────────────────────────────────────────────────

/// Scrubable time ruler with mouse-wheel zoom and keyframe ticks.
fn timeline_ruler(ui: &mut egui::Ui, state: &mut EditorState, width: f32) {
    let height = 26.0_f32;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    let duration = state.skeleton_editor.duration().max(0.01);
    let mut pps = state.skeleton_editor.timeline_zoom;

    // Mouse-wheel zoom centred on the cursor (Ctrl-not-required).
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y.abs() > 0.1 {
            let factor = if scroll.y > 0.0 { 1.1 } else { 1.0 / 1.1 };
            let new_pps = (pps * factor).clamp(8.0, 800.0);
            if let Some(pos) = response.hover_pos() {
                let local_x = (pos.x - rect.min.x).max(0.0);
                let t_under = state.skeleton_editor.timeline_scroll + local_x / pps.max(1.0);
                pps = new_pps;
                state.skeleton_editor.timeline_zoom = pps;
                state.skeleton_editor.timeline_scroll =
                    (t_under - local_x / pps.max(1.0)).max(0.0);
            } else {
                state.skeleton_editor.timeline_zoom = new_pps;
                pps = new_pps;
            }
        }
        // Horizontal scroll (touchpad / shift+wheel).
        if scroll.x.abs() > 0.1 {
            state.skeleton_editor.timeline_scroll =
                (state.skeleton_editor.timeline_scroll - scroll.x / pps.max(1.0)).max(0.0);
        }
    }

    let scroll = state.skeleton_editor.timeline_scroll;

    // Background.
    painter.rect_filled(rect, Rounding::same(3.0), COL_RULER);

    // Time markers (auto step).
    let visible_secs = (rect.width() / pps.max(1.0)).max(0.01);
    let step = pick_ruler_step(visible_secs);
    let first_mark = (scroll / step).floor() * step;
    let last_mark = (scroll + visible_secs).min(duration);
    let mut t_mark = first_mark.max(0.0);
    while t_mark <= last_mark + step * 0.5 && t_mark <= duration + 0.0001 {
        let x = rect.min.x + (t_mark - scroll) * pps;
        if x >= rect.min.x && x <= rect.max.x {
            painter.line_segment(
                [Pos2::new(x, rect.max.y - 6.0), Pos2::new(x, rect.max.y)],
                Stroke::new(1.0, Color32::from_rgb(80, 80, 100)),
            );
            painter.text(
                Pos2::new(x + 2.0, rect.min.y + 2.0),
                egui::Align2::LEFT_TOP,
                format!("{:.2}s", t_mark),
                egui::FontId::proportional(9.0),
                COL_TEXT_DIM,
            );
        }
        t_mark += step;
    }

    // Draw keyframe ticks for the selected point (or all points if none selected).
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        let template = &state.scene.skeleton_templates[tmpl_idx];
        let selected = state.skeleton_editor.selected_point.as_deref();
        for (name, point) in &template.points {
            let active = selected == Some(name) || selected.is_none();
            for kf in &point.track {
                let x = rect.min.x + (kf.t - scroll) * pps;
                if x < rect.min.x - 2.0 || x > rect.max.x + 2.0 {
                    continue;
                }
                let col = if active {
                    Color32::from_rgb(255, 200, 50)
                } else {
                    Color32::from_rgb(100, 100, 120)
                };
                let center = Pos2::new(x, rect.center().y);
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(center.x, center.y - 4.0),
                        Pos2::new(center.x + 3.5, center.y),
                        Pos2::new(center.x, center.y + 4.0),
                        Pos2::new(center.x - 3.5, center.y),
                    ],
                    col,
                    Stroke::new(0.8, Color32::BLACK),
                ));
            }
        }
    }

    // Playhead.
    let cur_t = state.skeleton_editor.current_time();
    let ph_x = rect.min.x + (cur_t - scroll) * pps;
    if ph_x >= rect.min.x && ph_x <= rect.max.x {
        painter.line_segment(
            [Pos2::new(ph_x, rect.min.y), Pos2::new(ph_x, rect.max.y)],
            Stroke::new(1.5, COL_PLAYHEAD),
        );
        let tri = 5.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(ph_x - tri, rect.min.y),
                Pos2::new(ph_x + tri, rect.min.y),
                Pos2::new(ph_x, rect.min.y + tri * 1.4),
            ],
            COL_PLAYHEAD,
            Stroke::NONE,
        ));
    }

    // Click / drag to scrub.
    if response.clicked() || response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let local_x = (pos.x - rect.min.x).max(0.0);
            let new_t = (scroll + local_x / pps.max(1.0)).clamp(0.0, duration);
            let new_frame = (new_t * state.skeleton_editor.fps).round() as u32;
            state.skeleton_editor.current_frame =
                new_frame.min(state.skeleton_editor.total_frames.saturating_sub(1));
        }
    }

    // Border.
    painter.rect_stroke(
        rect,
        Rounding::same(3.0),
        Stroke::new(1.0, Color32::from_rgb(60, 60, 80)),
    );
}

/// Pick a "nice" step size in seconds for ruler labels.
fn pick_ruler_step(visible_secs: f32) -> f32 {
    let target = visible_secs / 8.0;
    for &candidate in &[
        0.05_f32, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0,
    ] {
        if candidate >= target {
            return candidate;
        }
    }
    60.0
}

// ─── FRAME NAVIGATION ────────────────────────────────────────────────

fn frame_navigation(ui: &mut egui::Ui, state: &mut EditorState) {
    let total = state.skeleton_editor.total_frames.max(1);

    ui.horizontal(|ui| {
        if ui.button("\u{23EE}").on_hover_text("First frame").clicked() {
            state.skeleton_editor.current_frame = 0;
        }
        if ui.button("\u{25C0}").on_hover_text("Previous frame").clicked() {
            if state.skeleton_editor.current_frame > 0 {
                state.skeleton_editor.current_frame -= 1;
            }
        }

        let mut frame = state.skeleton_editor.current_frame;
        ui.add(
            egui::Slider::new(&mut frame, 0..=total.saturating_sub(1))
                .show_value(false)
                .clamp_to_range(true),
        );
        state.skeleton_editor.current_frame = frame;

        if ui.button("\u{25B6}").on_hover_text("Next frame").clicked() {
            if state.skeleton_editor.current_frame < total - 1 {
                state.skeleton_editor.current_frame += 1;
            }
        }
        if ui.button("\u{23ED}").on_hover_text("Last frame").clicked() {
            state.skeleton_editor.current_frame = total - 1;
        }

        let t = state.skeleton_editor.current_time();
        ui.label(
            RichText::new(format!("{}/{} ({:.2}s)", frame + 1, total, t))
                .size(11.0)
                .color(COL_TEXT_DIM),
        );
    });
}

// ─── POINT LIST PANEL ────────────────────────────────────────────────

fn point_list_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.label(RichText::new("Points").size(14.0).strong());
    ui.add_space(4.0);

    // Add new point — auto-selects + arms place mode for one-click placement.
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.skeleton_editor.new_point_name)
                .hint_text("Point name...")
                .desired_width(120.0),
        );
        let enter = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button("+").on_hover_text("Add point and start placing").clicked() || enter {
            let name = state.skeleton_editor.new_point_name.trim().to_string();
            if !name.is_empty() {
                if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
                    state.scene.skeleton_templates[tmpl_idx].add_point(&name);
                    state.skeleton_editor.selected_point = Some(name.clone());
                    state.skeleton_editor.new_point_name.clear();
                    // Auto-arm place mode so the very next click on the
                    // frame drops the first keyframe — saves a button press.
                    state.skeleton_editor.place_mode = true;
                    state.status =
                        format!("Added point: {}. Click on the frame to place it.", name);
                }
            }
        }
    });

    ui.add_space(8.0);

    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let t = state.skeleton_editor.current_time();

    let point_names: Vec<String> = state.scene.skeleton_templates[tmpl_idx]
        .points
        .keys()
        .cloned()
        .collect();

    let mut to_remove: Option<String> = None;

    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
        for name in &point_names {
            let is_selected = state.skeleton_editor.selected_point.as_deref() == Some(name);
            let point = &state.scene.skeleton_templates[tmpl_idx].points[name];
            let color = Color32::from_rgb(point.color[0], point.color[1], point.color[2]);
            let has_kf = point.track.iter().any(|kf| (kf.t - t).abs() < 0.02);
            let num_kf = point.track.len();

            let frame = egui::Frame::none()
                .fill(if is_selected {
                    Color32::from_rgb(40, 40, 60)
                } else {
                    Color32::TRANSPARENT
                })
                .rounding(Rounding::same(4.0))
                .inner_margin(egui::Margin::same(4.0));

            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 5.0, color);

                    let resp =
                        ui.selectable_label(is_selected, RichText::new(name).size(12.0));
                    if resp.clicked() {
                        state.skeleton_editor.selected_point = Some(name.clone());
                    }

                    if has_kf {
                        ui.label(
                            RichText::new("\u{25C6}")
                                .size(10.0)
                                .color(Color32::from_rgb(255, 200, 50)),
                        );
                    }

                    ui.label(
                        RichText::new(format!("{}kf", num_kf))
                            .size(9.0)
                            .color(Color32::from_rgb(100, 100, 120)),
                    );

                    if ui.small_button("\u{1F5D1}").on_hover_text("Remove point").clicked() {
                        to_remove = Some(name.clone());
                    }
                });
            });
        }
    });

    if let Some(name) = to_remove {
        state.scene.skeleton_templates[tmpl_idx].remove_point(&name);
        if state.skeleton_editor.selected_point.as_deref() == Some(&name) {
            state.skeleton_editor.selected_point = None;
        }
        state.status = format!("Removed point: {}", name);
    }

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    if let Some(ref sel_name) = state.skeleton_editor.selected_point.clone() {
        ui.label(
            RichText::new(format!("Selected: {}", sel_name))
                .size(12.0)
                .strong(),
        );
        ui.add_space(4.0);

        // Place mode toggle (kept for clarity, but the workflow no longer
        // requires it: drag a marker directly to move it, and "+" auto-arms
        // place mode for new points).
        let place_color = if state.skeleton_editor.place_mode {
            Color32::from_rgb(255, 80, 80)
        } else {
            Color32::from_rgb(80, 200, 120)
        };
        let place_text = if state.skeleton_editor.place_mode {
            "Placing... (click frame)"
        } else {
            "Place mode"
        };
        if ui
            .button(RichText::new(place_text).color(place_color))
            .on_hover_text("Toggle: when ON, clicking on the frame places a keyframe at the cursor")
            .clicked()
        {
            state.skeleton_editor.place_mode = !state.skeleton_editor.place_mode;
        }

        ui.add_space(2.0);
        ui.label(
            RichText::new("Tip: drag a point on the frame to move it.")
                .size(9.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );

        ui.add_space(4.0);

        if ui
            .button("+ Keyframe here")
            .on_hover_text("Add/update keyframe at current frame")
            .clicked()
        {
            let current_state = state.scene.skeleton_templates[tmpl_idx]
                .sample_point(sel_name, t)
                .unwrap_or_default();
            state.scene.skeleton_templates[tmpl_idx]
                .set_point_keyframe(sel_name, t, current_state, Easing::Linear);
            let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
            state.status = format!("Keyframe added at {:.2}s", t);
        }

        if ui
            .button("- Remove keyframe")
            .on_hover_text("Remove keyframe nearest to current frame")
            .clicked()
        {
            if state.scene.skeleton_templates[tmpl_idx].remove_point_keyframe(sel_name, t) {
                let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
                state.status = format!("Keyframe removed at {:.2}s", t);
            }
        }

        if let Some(ps) = state.scene.skeleton_templates[tmpl_idx].sample_point(sel_name, t) {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Pos: ({:.3}, {:.3})", ps.x, ps.y))
                    .size(10.0)
                    .color(COL_TEXT_DIM),
            );
        }
    }
}

// Suppress dead-code warning for COL_POINT_DEFAULT (kept for future use /
// reference colour for the marker palette).
#[allow(dead_code)]
const _UNUSED_COLORS: [Color32; 1] = [COL_POINT_DEFAULT];
