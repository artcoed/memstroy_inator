//! Skeleton Constructor — preview-driven point placement editor.
//!
//! The user picks a clip, hits Play, and drags named anchor points on the
//! preview as the timeline plays. Every drag sample becomes a keyframe at
//! the current playhead. Between keyframes the easing curve is selectable
//! (linear, step, ease-in/out, cubic). Each point also has a "Track" mode
//! that loops the playhead between the point's first and last keyframe so
//! the user can review the recorded path.
//!
//! Points are stored as keyframes in a `SkeletonTemplate` and persisted as
//! a `<clip>.skeleton.json` sidecar.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::EditorState;
use crate::video_cache::FrameCache;

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_POINT_DEFAULT: Color32 = Color32::from_rgb(255, 100, 100);
const COL_POINT_SELECTED: Color32 = Color32::from_rgb(255, 220, 80);
const COL_FRAME_BG: Color32 = Color32::from_rgb(20, 20, 30);
const COL_RULER: Color32 = Color32::from_rgb(28, 28, 38);
const COL_TRACK_BG: Color32 = Color32::from_rgb(34, 34, 46);
const COL_TRACK_BG_ALT: Color32 = Color32::from_rgb(38, 38, 52);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 60, 60);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 160);
const COL_TEXT: Color32 = Color32::from_rgb(220, 220, 240);
const COL_KF: Color32 = Color32::from_rgb(255, 200, 50);
const COL_KF_SELECTED: Color32 = Color32::from_rgb(120, 220, 255);
const COL_KF_DIM: Color32 = Color32::from_rgb(120, 120, 140);

// ─── STATE ───────────────────────────────────────────────────────────

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
    /// Currently selected keyframe (point_name, keyframe_index). Editing
    /// the easing for that keyframe shows up below the timeline.
    pub selected_keyframe: Option<(String, usize)>,
    /// Current frame index for navigation.
    pub current_frame: u32,
    /// Total frame count (from frame cache or clip duration * fps).
    pub total_frames: u32,
    /// FPS used for frame navigation.
    pub fps: f32,
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
    /// Whether playback is currently advancing the playhead.
    pub playing: bool,
    /// Wall-clock time of the last play tick (for delta accumulation).
    /// Wrapped in an Option because the timer only starts running when the
    /// user hits Play.
    pub last_play_tick: Option<Instant>,
    /// When `Some(name)`, playback is restricted to the [first..last]
    /// keyframe range of that point and loops back to the first keyframe
    /// at the end. Used by the "Track" toggle in the point list.
    pub track_loop_point: Option<String>,
    /// Auto-name counter; bumps every time a nameless point is added so
    /// the user doesn't need to think up a name to start placing.
    pub name_counter: u32,
}

impl Default for SkeletonEditorState {
    fn default() -> Self {
        Self {
            open: false,
            clip_path: None,
            actor_idx: None,
            template_idx: None,
            selected_point: None,
            selected_keyframe: None,
            current_frame: 0,
            total_frames: 1,
            fps: 30.0,
            dragging_point: None,
            preview_cache: None,
            extract_slot: None,
            timeline_zoom: 80.0,
            timeline_scroll: 0.0,
            playing: false,
            last_play_tick: None,
            track_loop_point: None,
            name_counter: 0,
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

    poll_preview_extraction(state);
    advance_playback(ctx, state);

    let mut open = state.skeleton_editor.open;
    let screen_rect = ctx.input(|i| i.screen_rect());
    let max_w = (screen_rect.width() - 40.0).max(500.0);
    let max_h = (screen_rect.height() - 60.0).max(420.0);

    egui::Window::new("Skeleton Constructor")
        .open(&mut open)
        .default_size([920.0, 660.0])
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

/// Drive the playback playhead forward each frame. When `track_loop_point`
/// is set, the playhead loops between the first and last keyframe of that
/// point so the user can review the recorded path of just that point.
fn advance_playback(ctx: &egui::Context, state: &mut EditorState) {
    if !state.skeleton_editor.playing {
        state.skeleton_editor.last_play_tick = None;
        return;
    }
    let now = Instant::now();
    let dt = match state.skeleton_editor.last_play_tick.replace(now) {
        Some(prev) => (now - prev).as_secs_f32(),
        None => 0.0,
    };
    if dt <= 0.0 {
        ctx.request_repaint();
        return;
    }

    let fps = state.skeleton_editor.fps.max(1.0);
    let total = state.skeleton_editor.total_frames.max(1);
    let mut t = state.skeleton_editor.current_time();

    // Resolve play range: either the point's keyframe span (if tracking)
    // or the full clip duration.
    let (lo, hi) = if let Some(name) = state.skeleton_editor.track_loop_point.clone() {
        if let Some(idx) = state.skeleton_editor.template_idx {
            let p = state.scene.skeleton_templates[idx].points.get(&name);
            if let Some(p) = p {
                if !p.track.is_empty() {
                    let lo = p.track.first().unwrap().t.max(0.0);
                    let hi = p.track.last().unwrap().t.max(lo + 0.01);
                    (lo, hi)
                } else {
                    (0.0, state.skeleton_editor.duration())
                }
            } else {
                (0.0, state.skeleton_editor.duration())
            }
        } else {
            (0.0, state.skeleton_editor.duration())
        }
    } else {
        (0.0, state.skeleton_editor.duration())
    };

    t += dt;
    if t >= hi {
        t = lo;
    }
    let frame = (t * fps).round() as u32;
    state.skeleton_editor.current_frame = frame.min(total.saturating_sub(1));
    ctx.request_repaint();
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
                let right_panel_w = 240.0_f32;
                let separator_w = 12.0_f32;
                let transport_h = 32.0_f32;
                let timeline_h = 64.0_f32;
                let easing_h = 28.0_f32;
                let avail = ui.available_size_before_wrap();
                let preview_max_w = (avail.x - right_panel_w - separator_w).max(220.0);
                let preview_max_h = (avail.y - transport_h - timeline_h - easing_h - 16.0).max(200.0);

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
                    transport_bar(ui, state);
                    ui.add_space(4.0);
                    skeleton_timeline(ui, state, pw);
                    ui.add_space(4.0);
                    keyframe_easing_panel(ui, state);
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

/// Switch the editor to a new source clip.
fn on_clip_changed(state: &mut EditorState, clip_path: &std::path::Path) {
    state.skeleton_editor.clip_path = Some(clip_path.to_path_buf());
    state.skeleton_editor.selected_point = None;
    state.skeleton_editor.selected_keyframe = None;
    state.skeleton_editor.current_frame = 0;
    state.skeleton_editor.fps = 30.0;
    state.skeleton_editor.dragging_point = None;
    state.skeleton_editor.timeline_scroll = 0.0;
    state.skeleton_editor.playing = false;
    state.skeleton_editor.last_play_tick = None;
    state.skeleton_editor.track_loop_point = None;

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

    // Reset auto-name counter to one above the highest pN in the template.
    state.skeleton_editor.name_counter = 0;
    if let Some(idx) = state.skeleton_editor.template_idx {
        for name in state.scene.skeleton_templates[idx].points.keys() {
            if let Some(rest) = name.strip_prefix('p') {
                if let Ok(n) = rest.parse::<u32>() {
                    if n > state.skeleton_editor.name_counter {
                        state.skeleton_editor.name_counter = n;
                    }
                }
            }
        }
    }

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
            if frame_count > 0 {
                state.skeleton_editor.total_frames = frame_count as u32;
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
    if let Some(fc) = state.skeleton_editor.preview_cache.as_mut() {
        if fc.is_ready() {
            if let Some(tex) = fc.frame_at_time(t, ui.ctx()) {
                let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
                painter.image(tex.id(), rect, uv, Color32::WHITE);
                frame_shown = true;
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

    // ── Pointer interaction ──────────────────────────────────────────
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
                if dist < 14.0 && best.as_ref().map(|b| dist < b.1).unwrap_or(true) {
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
        } else if let Some(name) = state.skeleton_editor.selected_point.clone() {
            // No marker under cursor, but a point is selected — drag begins
            // to place that point at the cursor (and onwards).
            state.skeleton_editor.dragging_point = Some(name.clone());
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                place_point_at(state, &name, nx, ny);
            }
        }
    } else if response.clicked() && pointer_in_rect {
        if let Some(name) = hovered_point.clone() {
            state.skeleton_editor.selected_point = Some(name);
        } else if let Some(name) = state.skeleton_editor.selected_point.clone() {
            // Click on empty preview area with a selected point → drop a
            // keyframe at the cursor for that point. Mirrors the old
            // "place mode" behaviour but with no extra toggle.
            if let Some(pos) = pointer_pos {
                let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                place_point_at(state, &name, nx, ny);
            }
        }
    }

    // Hover cursor hint.
    if hovered_point.is_some() && state.skeleton_editor.dragging_point.is_none() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    } else if pointer_in_rect && state.skeleton_editor.dragging_point.is_none() {
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

            // Diamond indicator if there's a keyframe within ~one frame of `t`.
            let kf_proximity = 1.0 / state.skeleton_editor.fps.max(1.0) * 0.6;
            let has_kf = point.track.iter().any(|kf| (kf.t - t).abs() < kf_proximity);
            if has_kf {
                painter.text(
                    Pos2::new(pos.x, pos.y - radius - 4.0),
                    egui::Align2::CENTER_BOTTOM,
                    "\u{25C6}",
                    egui::FontId::proportional(9.0),
                    COL_KF,
                );
            }
        }
    }

    // Border.
    painter.rect_stroke(rect, Rounding::same(4.0), Stroke::new(1.0, Color32::from_rgb(60, 60, 80)));
}

/// Helper: sample a SkeletonPoint at time t.
pub fn sample_point_at(point: &SkeletonPoint, t: f32) -> PointState {
    keyframe::sample(&point.track, t).unwrap_or_default()
}

/// Place the named point at normalised (nx, ny) at the current playhead.
/// Inserts a new keyframe (or updates the closest existing one within ~1
/// frame) and re-saves the sidecar.
fn place_point_at(state: &mut EditorState, point_name: &str, nx: f32, ny: f32) {
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let t = state.skeleton_editor.current_time();
    let ps = PointState { x: nx, y: ny, scale: 1.0, rotation_deg: 0.0 };
    state.scene.skeleton_templates[tmpl_idx]
        .set_point_keyframe(point_name, t, ps, Easing::Linear);
    let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
}

// ─── TRANSPORT BAR ───────────────────────────────────────────────────

/// Compact play/pause + frame nav strip, identical-feeling to the main
/// editor's transport buttons.
fn transport_bar(ui: &mut egui::Ui, state: &mut EditorState) {
    let total = state.skeleton_editor.total_frames.max(1);

    ui.horizontal(|ui| {
        if ui.button("\u{23EE}").on_hover_text("First frame").clicked() {
            state.skeleton_editor.current_frame = 0;
        }
        if ui.button("\u{25C0}").on_hover_text("Previous frame").clicked()
            && state.skeleton_editor.current_frame > 0
        {
            state.skeleton_editor.current_frame -= 1;
        }

        let play_label = if state.skeleton_editor.playing {
            RichText::new("\u{23F8}").size(14.0)
        } else {
            RichText::new("\u{25B6}").size(14.0)
        };
        if ui
            .button(play_label)
            .on_hover_text("Play / Pause (drag a point during playback to record keyframes)")
            .clicked()
        {
            state.skeleton_editor.playing = !state.skeleton_editor.playing;
            state.skeleton_editor.last_play_tick = None;
        }

        if ui.button("\u{25B8}").on_hover_text("Next frame").clicked()
            && state.skeleton_editor.current_frame + 1 < total
        {
            state.skeleton_editor.current_frame += 1;
        }
        if ui.button("\u{23ED}").on_hover_text("Last frame").clicked() {
            state.skeleton_editor.current_frame = total - 1;
        }

        let t = state.skeleton_editor.current_time();
        ui.label(
            RichText::new(format!(
                "{}/{}  {:.2}s / {:.2}s",
                state.skeleton_editor.current_frame + 1,
                total,
                t,
                state.skeleton_editor.duration()
            ))
            .size(11.0)
            .color(COL_TEXT_DIM),
        );

        if let Some(name) = state.skeleton_editor.track_loop_point.clone() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui.button(RichText::new("\u{25CB} stop tracking").size(10.0).color(Color32::from_rgb(255, 160, 80)));
                if resp.clicked() {
                    state.skeleton_editor.track_loop_point = None;
                    state.skeleton_editor.playing = false;
                }
                resp.on_hover_text(format!("Looping over '{}'", name));
            });
        }
    });
}

// ─── TIMELINE ────────────────────────────────────────────────────────

/// Two-row "main-style" timeline: ruler on top, keyframe track below.
/// Click / drag the ruler or the track to scrub. Click a keyframe diamond
/// in the track row to select it; the easing for the selected keyframe
/// shows up in the panel below.
fn skeleton_timeline(ui: &mut egui::Ui, state: &mut EditorState, width: f32) {
    let ruler_h = 22.0_f32;
    let track_h = 38.0_f32;
    let total_h = ruler_h + track_h;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, total_h), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    let duration = state.skeleton_editor.duration().max(0.01);
    let mut pps = state.skeleton_editor.timeline_zoom;

    // Mouse-wheel zoom centred on the cursor.
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
        if scroll.x.abs() > 0.1 {
            state.skeleton_editor.timeline_scroll =
                (state.skeleton_editor.timeline_scroll - scroll.x / pps.max(1.0)).max(0.0);
        }
    }

    let scroll = state.skeleton_editor.timeline_scroll;

    // ── Background ──
    let ruler_rect = Rect::from_min_size(rect.min, Vec2::new(width, ruler_h));
    let track_rect = Rect::from_min_max(
        Pos2::new(rect.min.x, rect.min.y + ruler_h),
        rect.max,
    );
    painter.rect_filled(ruler_rect, Rounding::same(2.0), COL_RULER);
    painter.rect_filled(track_rect, Rounding::same(2.0), COL_TRACK_BG);

    // ── Ruler ticks & labels ──
    let visible_secs = (rect.width() / pps.max(1.0)).max(0.01);
    let step = pick_ruler_step(visible_secs);
    let first_mark = (scroll / step).floor() * step;
    let last_mark = (scroll + visible_secs).min(duration);
    let mut t_mark = first_mark.max(0.0);
    while t_mark <= last_mark + step * 0.5 && t_mark <= duration + 0.0001 {
        let x = rect.min.x + (t_mark - scroll) * pps;
        if x >= rect.min.x && x <= rect.max.x {
            painter.line_segment(
                [
                    Pos2::new(x, ruler_rect.max.y - 6.0),
                    Pos2::new(x, ruler_rect.max.y),
                ],
                Stroke::new(1.0, Color32::from_rgb(80, 80, 100)),
            );
            painter.text(
                Pos2::new(x + 2.0, ruler_rect.min.y + 1.0),
                egui::Align2::LEFT_TOP,
                format!("{:.2}s", t_mark),
                egui::FontId::proportional(9.0),
                COL_TEXT_DIM,
            );
        }
        t_mark += step;
    }

    // Alternate-row tint inside the track to give it the "main timeline" look.
    let alt_band_count = 8;
    for i in 0..alt_band_count {
        let band_w = track_rect.width() / alt_band_count as f32;
        if i % 2 == 0 {
            let x0 = track_rect.min.x + i as f32 * band_w;
            let x1 = (x0 + band_w).min(track_rect.max.x);
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(x0, track_rect.min.y),
                    Pos2::new(x1, track_rect.max.y),
                ),
                Rounding::ZERO,
                COL_TRACK_BG_ALT,
            );
        }
    }

    // ── Loop range underlay (when tracking a point) ──
    let loop_range = state
        .skeleton_editor
        .track_loop_point
        .as_ref()
        .and_then(|name| {
            state.skeleton_editor.template_idx.and_then(|idx| {
                let p = state.scene.skeleton_templates[idx].points.get(name)?;
                if p.track.is_empty() {
                    return None;
                }
                Some((p.track.first()?.t, p.track.last()?.t))
            })
        });
    if let Some((lo, hi)) = loop_range {
        let x0 = (rect.min.x + (lo - scroll) * pps)
            .clamp(rect.min.x, rect.max.x);
        let x1 = (rect.min.x + (hi - scroll) * pps)
            .clamp(rect.min.x, rect.max.x);
        if x1 > x0 {
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(x0, track_rect.min.y),
                    Pos2::new(x1, track_rect.max.y),
                ),
                Rounding::ZERO,
                Color32::from_rgba_premultiplied(255, 160, 60, 22),
            );
        }
    }

    // ── Keyframe diamonds (interactive) ──
    let mut keyframe_hits: Vec<(String, usize, Pos2)> = Vec::new();
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        let template = &state.scene.skeleton_templates[tmpl_idx];
        let selected = state.skeleton_editor.selected_point.clone();
        let track_y = track_rect.center().y;
        for (name, point) in &template.points {
            let active = selected.as_deref() == Some(name) || selected.is_none();
            for (kf_idx, kf) in point.track.iter().enumerate() {
                let x = rect.min.x + (kf.t - scroll) * pps;
                if x < rect.min.x - 4.0 || x > rect.max.x + 4.0 {
                    continue;
                }
                let center = Pos2::new(x, track_y);
                let is_selected_kf = state.skeleton_editor.selected_keyframe.as_ref()
                    == Some(&(name.clone(), kf_idx));
                let col = if is_selected_kf {
                    COL_KF_SELECTED
                } else if active {
                    Color32::from_rgb(point.color[0], point.color[1], point.color[2])
                } else {
                    COL_KF_DIM
                };
                let r = if is_selected_kf { 6.0 } else { 4.5 };
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(center.x, center.y - r),
                        Pos2::new(center.x + r * 0.8, center.y),
                        Pos2::new(center.x, center.y + r),
                        Pos2::new(center.x - r * 0.8, center.y),
                    ],
                    col,
                    Stroke::new(0.8, Color32::BLACK),
                ));
                keyframe_hits.push((name.clone(), kf_idx, center));
            }
        }
    }

    // ── Playhead ──
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

    // ── Interaction: keyframe selection / scrub ──
    let pointer_pos = response.interact_pointer_pos();
    let primary_clicked = response.clicked();
    let primary_dragged = response.dragged();

    let mut clicked_kf: Option<(String, usize)> = None;
    if primary_clicked {
        if let Some(p) = pointer_pos {
            // Hit-test keyframe diamonds first (only the track row).
            if track_rect.contains(p) {
                let mut best: Option<(f32, (String, usize))> = None;
                for (name, idx, c) in &keyframe_hits {
                    let d = ((p.x - c.x).powi(2) + (p.y - c.y).powi(2)).sqrt();
                    if d < 8.0 && best.as_ref().map(|b| d < b.0).unwrap_or(true) {
                        best = Some((d, (name.clone(), *idx)));
                    }
                }
                if let Some((_, hit)) = best {
                    clicked_kf = Some(hit);
                }
            }
        }
    }

    if let Some((name, idx)) = clicked_kf {
        state.skeleton_editor.selected_point = Some(name.clone());
        state.skeleton_editor.selected_keyframe = Some((name.clone(), idx));
        // Snap playhead to the keyframe time.
        if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
            if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get(&name) {
                if let Some(kf) = p.track.get(idx) {
                    let frame = (kf.t * state.skeleton_editor.fps).round() as u32;
                    state.skeleton_editor.current_frame =
                        frame.min(state.skeleton_editor.total_frames.saturating_sub(1));
                }
            }
        }
    } else if primary_clicked || primary_dragged {
        // Plain scrub.
        if let Some(p) = pointer_pos {
            let local_x = (p.x - rect.min.x).max(0.0);
            let new_t = (scroll + local_x / pps.max(1.0)).clamp(0.0, duration);
            let new_frame = (new_t * state.skeleton_editor.fps).round() as u32;
            state.skeleton_editor.current_frame =
                new_frame.min(state.skeleton_editor.total_frames.saturating_sub(1));
        }
    }

    // Border.
    painter.rect_stroke(
        rect,
        Rounding::same(2.0),
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

// ─── KEYFRAME EASING PANEL ───────────────────────────────────────────

/// Tiny strip below the timeline. Visible when a keyframe is selected on
/// the timeline; lets the user pick the easing curve used to interpolate
/// INTO that keyframe from the previous one.
fn keyframe_easing_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some((name, idx)) = state.skeleton_editor.selected_keyframe.clone() else {
        ui.allocate_space(Vec2::new(1.0, 24.0));
        return;
    };
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };

    // Read current easing (and verify the keyframe still exists).
    let current_easing = match state.scene.skeleton_templates[tmpl_idx]
        .points
        .get(&name)
        .and_then(|p| p.track.get(idx))
    {
        Some(kf) => kf.easing,
        None => {
            state.skeleton_editor.selected_keyframe = None;
            return;
        }
    };

    let mut new_easing = current_easing;
    let mut delete_kf = false;

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("KF '{}' #{}", name, idx + 1))
                .size(11.0)
                .color(COL_TEXT_DIM),
        );
        ui.label(RichText::new("transition:").size(10.0).color(COL_TEXT_DIM));

        for (label, value) in &[
            ("Step", Easing::Step),
            ("Linear", Easing::Linear),
            ("EaseIn", Easing::EaseIn),
            ("EaseOut", Easing::EaseOut),
            ("EaseInOut", Easing::EaseInOut),
            ("Bezier", Easing::Cubic),
        ] {
            let is_sel = current_easing == *value;
            if ui
                .selectable_label(is_sel, RichText::new(*label).size(10.0))
                .clicked()
            {
                new_easing = *value;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(RichText::new("delete kf").color(Color32::from_rgb(255, 120, 120)))
                .clicked()
            {
                delete_kf = true;
            }
        });
    });

    if new_easing != current_easing {
        if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get_mut(&name) {
            if let Some(kf) = p.track.get_mut(idx) {
                kf.easing = new_easing;
            }
        }
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
    }

    if delete_kf {
        if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get_mut(&name) {
            if idx < p.track.len() {
                p.track.remove(idx);
            }
        }
        state.skeleton_editor.selected_keyframe = None;
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
    }
}

// ─── POINT LIST PANEL ────────────────────────────────────────────────

/// Compact point list. One row per point: colour dot, name (selectable),
/// keyframe count, "Track" toggle (loop playback over this point's
/// keyframe range), delete. The "+" button auto-generates names like
/// `p1`, `p2`, ... so the user can immediately start placing.
fn point_list_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.label(RichText::new("Points").size(14.0).strong());
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        if ui
            .button(RichText::new("+ Add point").color(Color32::from_rgb(120, 220, 140)))
            .on_hover_text("Add a new point with an auto-generated name and start placing it")
            .clicked()
        {
            add_auto_point(state);
        }
    });

    ui.add_space(8.0);

    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };

    let point_names: Vec<String> = state.scene.skeleton_templates[tmpl_idx]
        .points
        .keys()
        .cloned()
        .collect();

    if point_names.is_empty() {
        ui.label(
            RichText::new("No points yet — press \"+ Add point\".")
                .size(11.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );
        return;
    }

    let mut to_remove: Option<String> = None;
    let mut to_select: Option<String> = None;
    let mut toggle_track: Option<String> = None;

    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
        for name in &point_names {
            let is_selected = state.skeleton_editor.selected_point.as_deref() == Some(name);
            let is_tracking = state.skeleton_editor.track_loop_point.as_deref() == Some(name);
            let point = &state.scene.skeleton_templates[tmpl_idx].points[name];
            let color = Color32::from_rgb(point.color[0], point.color[1], point.color[2]);
            let num_kf = point.track.len();

            let row_bg = if is_selected {
                Color32::from_rgb(45, 45, 65)
            } else {
                Color32::TRANSPARENT
            };

            let frame = egui::Frame::none()
                .fill(row_bg)
                .rounding(Rounding::same(4.0))
                .inner_margin(egui::Margin::same(4.0));

            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 5.0, color);

                    let resp = ui.selectable_label(
                        is_selected,
                        RichText::new(name).size(12.0).color(COL_TEXT),
                    );
                    if resp.clicked() {
                        to_select = Some(name.clone());
                    }

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui
                                .small_button("\u{1F5D1}")
                                .on_hover_text("Remove point")
                                .clicked()
                            {
                                to_remove = Some(name.clone());
                            }

                            let track_label = if is_tracking { "\u{25A0}" } else { "\u{25B6}" };
                            let track_color = if is_tracking {
                                Color32::from_rgb(255, 160, 80)
                            } else {
                                Color32::from_rgb(140, 200, 255)
                            };
                            if ui
                                .small_button(RichText::new(track_label).color(track_color))
                                .on_hover_text(if is_tracking {
                                    "Stop tracking"
                                } else {
                                    "Track: loop playback over this point's keyframe range"
                                })
                                .clicked()
                            {
                                toggle_track = Some(name.clone());
                            }

                            ui.label(
                                RichText::new(format!("{} kf", num_kf))
                                    .size(9.0)
                                    .color(COL_TEXT_DIM),
                            );
                        },
                    );
                });
            });
        }
    });

    if let Some(name) = to_select {
        state.skeleton_editor.selected_point = Some(name);
        state.skeleton_editor.selected_keyframe = None;
    }

    if let Some(name) = toggle_track {
        if state.skeleton_editor.track_loop_point.as_deref() == Some(&name) {
            state.skeleton_editor.track_loop_point = None;
            state.skeleton_editor.playing = false;
        } else {
            state.skeleton_editor.selected_point = Some(name.clone());
            state.skeleton_editor.track_loop_point = Some(name.clone());
            // Jump the playhead to the start of the loop range and start
            // playback immediately so "Track" feels like a one-click "show
            // me what this point does".
            if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get(&name) {
                if let Some(first) = p.track.first() {
                    let f = (first.t * state.skeleton_editor.fps).round() as u32;
                    state.skeleton_editor.current_frame =
                        f.min(state.skeleton_editor.total_frames.saturating_sub(1));
                }
            }
            state.skeleton_editor.playing = true;
            state.skeleton_editor.last_play_tick = None;
        }
    }

    if let Some(name) = to_remove {
        state.scene.skeleton_templates[tmpl_idx].remove_point(&name);
        if state.skeleton_editor.selected_point.as_deref() == Some(&name) {
            state.skeleton_editor.selected_point = None;
        }
        if state.skeleton_editor.track_loop_point.as_deref() == Some(&name) {
            state.skeleton_editor.track_loop_point = None;
        }
        if state
            .skeleton_editor
            .selected_keyframe
            .as_ref()
            .map(|(n, _)| n == &name)
            .unwrap_or(false)
        {
            state.skeleton_editor.selected_keyframe = None;
        }
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
        state.status = format!("Removed point: {}", name);
    }
}

/// Pick the next free `pN` name and add the point.
fn add_auto_point(state: &mut EditorState) {
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let mut n = state.skeleton_editor.name_counter + 1;
    let name = loop {
        let candidate = format!("p{}", n);
        if !state.scene.skeleton_templates[tmpl_idx]
            .points
            .contains_key(&candidate)
        {
            break candidate;
        }
        n += 1;
    };
    state.scene.skeleton_templates[tmpl_idx].add_point(&name);
    state.skeleton_editor.selected_point = Some(name.clone());
    state.skeleton_editor.selected_keyframe = None;
    state.skeleton_editor.name_counter = n;
    state.status = format!("Added point: {}. Click on the frame to place it.", name);
}

// Suppress dead-code warning for COL_POINT_DEFAULT (kept for future use /
// reference colour for the marker palette).
#[allow(dead_code)]
const _UNUSED_COLORS: [Color32; 1] = [COL_POINT_DEFAULT];
