//! Skeleton Constructor — inspector-driven point placement editor.
//!
//! Originally lived in a floating "Skeleton Editor" window. The window
//! has been retired: every piece of skeleton authoring is now embedded
//! into the inspector for any video-layer element (actor or video
//! overlay). The user picks a clip on the timeline, the inspector
//! shows the matching skeleton template, and points are placed by
//! dragging them directly on the main canvas while the timeline plays.
//!
//! Points are stored as keyframes in a `SkeletonTemplate` and persisted
//! as a `<clip>.skeleton.json` sidecar.

use std::time::Instant;

use egui::{Color32, Pos2, Rect, RichText, Rounding, Sense, Stroke, Vec2};
use memstroy_core::*;

use crate::state::EditorState;

// ─── COLORS ──────────────────────────────────────────────────────────

const COL_POINT_DEFAULT: Color32 = Color32::from_rgb(255, 100, 100);
const COL_RULER: Color32 = Color32::from_rgb(32, 30, 20);
const COL_TRACK_BG: Color32 = Color32::from_rgb(38, 36, 26);
const COL_TRACK_BG_ALT: Color32 = Color32::from_rgb(42, 40, 28);
const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 60, 60);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 160);
const COL_TEXT: Color32 = Color32::from_rgb(220, 220, 240);
#[allow(dead_code)]
const COL_KF: Color32 = Color32::from_rgb(255, 200, 50);
const COL_KF_SELECTED: Color32 = Color32::from_rgb(120, 220, 255);
const COL_KF_DIM: Color32 = Color32::from_rgb(120, 120, 140);

// ─── TIMELINE LAYOUT CONSTANTS ───────────────────────────────────────

const TIMELINE_RULER_H: f32 = 22.0;
const TIMELINE_ROW_H: f32 = 22.0;
const TIMELINE_MAX_VISIBLE_ROWS: usize = 6;

// ─── STATE ───────────────────────────────────────────────────────────

/// Persistent state for the inspector skeleton editor (lives in
/// EditorState).
///
/// The struct still exists because the timeline / playhead / point
/// list cooperate across multiple frames and need a stable home
/// outside the per-frame egui closure. Most fields that used to be
/// derived from the floating window's clip picker are now driven by
/// the currently selected element on the timeline.
pub struct SkeletonEditorState {
    /// Path of the source clip currently being edited. Mirrors the
    /// selected element's source so the inspector can detect a
    /// selection change and reset transient state.
    pub clip_path: Option<std::path::PathBuf>,
    /// Index of the skeleton template being edited (in
    /// `scene.skeleton_templates`).
    pub template_idx: Option<usize>,
    /// Currently selected point name.
    pub selected_point: Option<String>,
    /// Currently selected keyframe (point_name, keyframe_index).
    pub selected_keyframe: Option<(String, usize)>,
    /// FPS used for frame navigation. Synced from the template (or
    /// defaulted to 30).
    pub fps: f32,
    /// Name of the point currently being dragged on the canvas. Only
    /// the canvas now drives drag — the in-inspector preview was
    /// retired.
    pub dragging_point: Option<String>,
    /// Local time ruler zoom (pixels per second) for the inspector
    /// timeline.
    pub timeline_zoom: f32,
    /// Local time ruler scroll offset (seconds).
    pub timeline_scroll: f32,
    /// Wall-clock time of the last play tick (for delta accumulation
    /// when the "Track point" loop is engaged).
    pub last_play_tick: Option<Instant>,
    /// When `Some(name)`, the main scene playback is restricted to the
    /// `[first..last]` keyframe range of that point and loops back to
    /// the first keyframe at the end. Used by the "Track" toggle in
    /// the point list.
    pub track_loop_point: Option<String>,
    /// Per-point reference image used as a visual guide on the canvas.
    /// The user picks an image from the project library so they can
    /// align a point under a feature (e.g. the centre of a hat). The
    /// guide is **not saved** to the skeleton template — only the
    /// point's screen coordinates / keyframes are persisted.
    pub point_guide_images: std::collections::HashMap<String, std::path::PathBuf>,
    /// Auto-name counter; bumps every time a nameless point is added so
    /// the user doesn't need to think up a name to start placing.
    pub name_counter: u32,
    /// Clip duration the timeline horizontal zoom was last fitted to.
    pub fitted_for_duration: f32,
    /// Pixel width the timeline was last fitted to.
    pub fitted_for_width: f32,
    /// Vertical scroll offset (rows) for the per-point timeline tracks.
    pub timeline_v_scroll: usize,
}

impl Default for SkeletonEditorState {
    fn default() -> Self {
        Self {
            clip_path: None,
            template_idx: None,
            selected_point: None,
            selected_keyframe: None,
            fps: 30.0,
            dragging_point: None,
            timeline_zoom: 80.0,
            timeline_scroll: 0.0,
            last_play_tick: None,
            track_loop_point: None,
            point_guide_images: std::collections::HashMap::new(),
            name_counter: 0,
            fitted_for_duration: 0.0,
            fitted_for_width: 0.0,
            timeline_v_scroll: 0,
        }
    }
}

// ─── SOURCE-CLIP DESCRIPTOR ──────────────────────────────────────────

/// Describes the video-layer element the inspector is editing the
/// skeleton for. Built once by the inspector caller from the active
/// selection (actor / video overlay) and threaded into every helper
/// so the timeline / point list / canvas drag all use the same clip
/// time-base.
#[derive(Clone)]
pub struct SourceClipCtx {
    /// Source video file. Used to find / create the matching
    /// `SkeletonTemplate`.
    pub source: std::path::PathBuf,
    /// Where in the timeline the clip starts (scene seconds).
    pub t_in: f32,
    /// Where in the timeline the clip ends (scene seconds).
    pub t_out: f32,
    /// Where in the source file the clip's first visible frame
    /// originates (seconds). For overlays this is 0; for actors it's
    /// `actor.source_start`.
    #[allow(dead_code)]
    pub source_start: f32,
    /// Playback speed multiplier — the renderer maps a scene-time
    /// `t` to clip-local `(t - t_in) * speed + source_start`.
    pub speed: f32,
    /// Optional frame-cache index for the actor backing this clip.
    /// `None` for video overlays.
    #[allow(dead_code)]
    pub frame_cache_idx: Option<usize>,
}

impl SourceClipCtx {
    /// Map a scene-time playhead into clip-local seconds (the time
    /// base used by `SkeletonTemplate` keyframes).
    pub fn clip_local_time(&self, scene_t: f32) -> f32 {
        let t_in = self.t_in;
        let t_out = self.t_out;
        let speed = self.speed.max(0.0001);
        let t = if scene_t >= t_in && scene_t <= t_out {
            (scene_t - t_in) * speed
        } else if scene_t < t_in {
            0.0
        } else {
            (t_out - t_in) * speed
        };
        t.max(0.0)
    }

    /// Visible duration of the clip in clip-local seconds.
    pub fn clip_local_duration(&self) -> f32 {
        let speed = self.speed.max(0.0001);
        ((self.t_out - self.t_in) * speed).max(0.0)
    }

    /// Convenience: build a context for an actor at index `i`.
    pub fn from_actor(state: &EditorState, i: usize) -> Option<Self> {
        let actor = state.scene.actors.get(i)?;
        Some(Self {
            source: actor.source.clone(),
            t_in: actor.t_in.unwrap_or(0.0),
            t_out: actor.t_out.unwrap_or(state.scene.output.duration),
            source_start: actor.source_start,
            speed: actor.speed.max(0.0001),
            frame_cache_idx: Some(i),
        })
    }

    /// Convenience: build a context for a video overlay at index `i`.
    pub fn from_video_overlay(state: &EditorState, i: usize) -> Option<Self> {
        let ov = state.scene.overlays.get(i)?;
        match ov {
            Overlay::Video(v) => Some(Self {
                source: v.source.clone(),
                t_in: v.t_in,
                t_out: v.t_out,
                source_start: v.source_start,
                speed: 1.0,
                frame_cache_idx: None,
            }),
            _ => None,
        }
    }
}

// ─── SELECTION SYNC ──────────────────────────────────────────────────

/// Reset transient state when the user switches to a different source
/// clip (e.g. selects another actor on the timeline). Lazy: called on
/// every inspector paint and short-circuits when the clip hasn't
/// changed.
pub fn sync_to_source_clip(state: &mut EditorState, ctx: &SourceClipCtx) {
    let same_clip = state
        .skeleton_editor
        .clip_path
        .as_deref()
        .map(|p| p == ctx.source)
        .unwrap_or(false);
    if !same_clip {
        on_clip_changed(state, &ctx.source);
    } else {
        // Re-resolve the template index every paint — saving the
        // sidecar can shuffle the array, and the project-load path
        // pushes templates in batches.
        let tmpl_idx = state.scene.skeleton_templates.iter().position(|t| {
            t.source_clip == ctx.source || t.source_clip.file_name() == ctx.source.file_name()
        });
        state.skeleton_editor.template_idx = tmpl_idx;
    }
    if let Some(idx) = state.skeleton_editor.template_idx {
        if let Some(t) = state.scene.skeleton_templates.get(idx) {
            if t.fps > 0.5 {
                state.skeleton_editor.fps = t.fps;
            }
        }
    }
}

fn on_clip_changed(state: &mut EditorState, clip_path: &std::path::Path) {
    state.skeleton_editor.clip_path = Some(clip_path.to_path_buf());
    state.skeleton_editor.selected_point = None;
    state.skeleton_editor.selected_keyframe = None;
    state.skeleton_editor.dragging_point = None;
    state.skeleton_editor.timeline_scroll = 0.0;
    state.skeleton_editor.fitted_for_duration = 0.0;
    state.skeleton_editor.fitted_for_width = 0.0;
    state.skeleton_editor.timeline_v_scroll = 0;
    state.skeleton_editor.last_play_tick = None;
    state.skeleton_editor.track_loop_point = None;

    // Try to locate an existing template (in-scene first, then sidecar).
    let mut tmpl_idx = state.scene.skeleton_templates.iter().position(|t| {
        t.source_clip == clip_path || t.source_clip.file_name() == clip_path.file_name()
    });
    if tmpl_idx.is_none() {
        if let Some(template) = SkeletonTemplate::load_for_clip(clip_path) {
            state.scene.skeleton_templates.push(template);
            tmpl_idx = Some(state.scene.skeleton_templates.len() - 1);
        }
    }
    state.skeleton_editor.template_idx = tmpl_idx;

    // Reset auto-name counter to one above the highest pN in the template.
    state.skeleton_editor.name_counter = 0;
    if let Some(idx) = state.skeleton_editor.template_idx {
        if let Some(t) = state.scene.skeleton_templates.get(idx) {
            if t.fps > 0.5 {
                state.skeleton_editor.fps = t.fps;
            }
            for name in t.points.keys() {
                if let Some(rest) = name.strip_prefix('p') {
                    if let Ok(n) = rest.parse::<u32>() {
                        if n > state.skeleton_editor.name_counter {
                            state.skeleton_editor.name_counter = n;
                        }
                    }
                }
            }
        }
    }
}

// ─── TRACK-POINT PLAYBACK LOOPING ────────────────────────────────────

/// When the user has armed "Track" on a point, the main scene playhead
/// is gently looped over the point's keyframe range. The inspector
/// invokes this every frame the inspector is visible. Returns `true`
/// when the playhead was actively rewritten this frame so the caller
/// can trigger a repaint.
pub fn advance_track_loop(
    egui_ctx: &egui::Context,
    state: &mut EditorState,
    ctx: &SourceClipCtx,
) -> bool {
    let Some(name) = state.skeleton_editor.track_loop_point.clone() else {
        state.skeleton_editor.last_play_tick = None;
        return false;
    };
    if !state.playing {
        state.skeleton_editor.last_play_tick = None;
        return false;
    }
    let Some(idx) = state.skeleton_editor.template_idx else {
        return false;
    };
    let pt = match state
        .scene
        .skeleton_templates
        .get(idx)
        .and_then(|t| t.points.get(&name))
    {
        Some(p) if p.track.len() >= 2 => p,
        _ => return false,
    };
    let speed = ctx.speed.max(0.0001);
    // Translate the point's clip-local kf range back into scene time.
    let lo_local = pt.track.first().unwrap().t;
    let hi_local = pt.track.last().unwrap().t;
    let lo_scene = ctx.t_in + lo_local / speed;
    let hi_scene = ctx.t_in + hi_local / speed;
    let span = (hi_scene - lo_scene).max(1.0 / state.skeleton_editor.fps.max(1.0));

    let now = Instant::now();
    let prev = state.skeleton_editor.last_play_tick.replace(now);
    let _dt = match prev {
        Some(p) => (now - p).as_secs_f32(),
        None => 0.0,
    };

    let mut did_clamp = false;
    if state.playhead < lo_scene {
        state.playhead = lo_scene;
        did_clamp = true;
    }
    if state.playhead > hi_scene {
        // Wrap by overflow so a long dt doesn't snap to the start
        // exactly when the user dragged the window.
        let overflow = (state.playhead - hi_scene).rem_euclid(span);
        state.playhead = lo_scene + overflow;
        did_clamp = true;
    }
    if did_clamp {
        egui_ctx.request_repaint();
    }
    true
}

// ─── INSPECTOR ENTRY POINT ───────────────────────────────────────────

/// Render the full skeleton-editor block inside the inspector for a
/// video-layer element. Use this from `inspector_actor` /
/// `inspector_overlay` (video variant) inside a `CollapsingHeader` or
/// tab body.
pub fn inspector_skeleton_section(ui: &mut egui::Ui, state: &mut EditorState, ctx: &SourceClipCtx) {
    sync_to_source_clip(state, ctx);
    advance_track_loop(ui.ctx(), state, ctx);

    ui.label(
        RichText::new(crate::i18n::t("Skeleton Constructor"))
            .size(13.0)
            .strong()
            .color(Color32::WHITE),
    );
    ui.label(
        RichText::new(crate::i18n::t(
            "Drag points directly on the canvas while the timeline \
                 plays — every drag sample becomes a keyframe at the \
                 current playhead.",
        ))
        .size(9.0)
        .color(COL_TEXT_DIM)
        .italics(),
    );
    ui.add_space(6.0);

    // ── Template create / save / loop-fragment row ──
    skeleton_inspector_toolbar(ui, state, ctx);

    if state.skeleton_editor.template_idx.is_none() {
        ui.add_space(6.0);
        ui.label(
            RichText::new(crate::i18n::t(
                "No skeleton for this clip yet. Hit \"Create Skeleton\" \
                 to start placing points on the canvas.",
            ))
            .size(10.0)
            .italics()
            .color(COL_TEXT_DIM),
        );
        return;
    }

    ui.add_space(6.0);
    point_list_panel(ui, state);

    ui.add_space(6.0);
    ui.separator();
    ui.add_space(4.0);

    // ── Per-point keyframe timeline (clip-local time) ──
    let avail_w = ui.available_width().max(180.0);
    fit_timeline_to_clip_if_needed(state, ctx, avail_w);
    skeleton_timeline(ui, state, ctx, avail_w);

    ui.add_space(2.0);
    keyframe_easing_panel(ui, state);

    ui.add_space(6.0);
    ui.separator();
    ui.add_space(4.0);
    point_guide_image_panel(ui, state);
}

fn skeleton_inspector_toolbar(ui: &mut egui::Ui, state: &mut EditorState, ctx: &SourceClipCtx) {
    ui.horizontal_wrapped(|ui| {
        if state.skeleton_editor.template_idx.is_none() {
            if ui
                .button(
                    RichText::new(crate::i18n::t("+ Create Skeleton"))
                        .color(Color32::from_rgb(80, 200, 120)),
                )
                .on_hover_text(crate::i18n::t(
                    "Create a fresh <clip>.skeleton.json sidecar for this source.",
                ))
                .clicked()
            {
                create_template_for_clip(state, ctx);
            }
        } else {
            if ui
                .small_button(format!("S {}", crate::i18n::t("Save")))
                .on_hover_text(crate::i18n::t("Save skeleton to <clip>.skeleton.json"))
                .clicked()
            {
                save_current_template(state);
            }
        }

        // Loop-current-fragment toggle: clamps the main playhead to
        // [t_in, t_out] of the selected element instead of looping the
        // whole scene. Reuses the existing `loop_mode` / `loop_region`
        // pipeline so playback / shortcuts stay consistent.
        let active = state.loop_mode
            && state
                .loop_region
                .map(|(a, b)| {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    (lo - ctx.t_in).abs() < 0.02 && (hi - ctx.t_out).abs() < 0.02
                })
                .unwrap_or(false);
        let loop_color = if active {
            Color32::from_rgb(255, 180, 80)
        } else {
            COL_TEXT_DIM
        };
        let loop_label = RichText::new(format!("↻ {}", crate::i18n::t("Loop fragment"),))
            .size(11.0)
            .color(loop_color);
        if ui
            .button(loop_label)
            .on_hover_text(crate::i18n::t(
                "Loop just this clip's [in, out] range during playback \
                 instead of the whole scene. Click again to release.",
            ))
            .clicked()
        {
            if active {
                state.loop_mode = false;
                state.loop_region = None;
            } else {
                state.loop_mode = true;
                state.loop_region = Some((ctx.t_in, ctx.t_out));
                if state.playhead < ctx.t_in || state.playhead > ctx.t_out {
                    state.playhead = ctx.t_in;
                }
            }
        }
    });
}

// ─── TEMPLATE LIFECYCLE ──────────────────────────────────────────────

fn create_template_for_clip(state: &mut EditorState, ctx: &SourceClipCtx) {
    // Reuse an existing template if one is already loaded for this
    // source (the sidecar may have been parsed during a previous
    // selection but the user hit Create after deleting it from disk).
    if state
        .scene
        .skeleton_templates
        .iter()
        .any(|t| t.source_clip == ctx.source)
    {
        sync_to_source_clip(state, ctx);
        return;
    }
    let name = ctx
        .source
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("{}_skeleton", s))
        .unwrap_or_else(|| "skeleton".into());
    let template = SkeletonTemplate {
        name,
        source_clip: ctx.source.clone(),
        fps: 30.0,
        clip_duration: ctx.clip_local_duration().max(0.5),
        points: Default::default(),
    };
    state.scene.skeleton_templates.push(template);
    state.skeleton_editor.template_idx = Some(state.scene.skeleton_templates.len() - 1);
    save_current_template(state);
    state.status = crate::i18n::t("Skeleton template created.").into();
}

fn save_current_template(state: &mut EditorState) {
    let Some(idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let template = &state.scene.skeleton_templates[idx];
    match template.save_alongside_clip() {
        Ok(path) => {
            state.status = format!("{} {}", crate::i18n::t("Skeleton saved:"), path.display())
        }
        Err(e) => state.status = format!("{} {}", crate::i18n::t("Save failed:"), e),
    }
}

// ─── PUBLIC POINT MUTATION (USED BY CANVAS) ──────────────────────────

/// Place the named point at normalised (nx, ny) at the given
/// clip-local time. Inserts a new keyframe (or updates the closest
/// existing one within ~1 frame) and re-saves the sidecar.
///
/// Public because the canvas drag handler in `canvas_preview` writes
/// keyframes via this entry point.
pub fn place_point_at_clip_time(
    state: &mut EditorState,
    point_name: &str,
    nx: f32,
    ny: f32,
    clip_local_t: f32,
) {
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };
    let ps = PointState {
        x: nx.clamp(0.0, 1.0),
        y: ny.clamp(0.0, 1.0),
        scale: 1.0,
        rotation_deg: 0.0,
    };
    state.scene.skeleton_templates[tmpl_idx].set_point_keyframe(
        point_name,
        clip_local_t.max(0.0),
        ps,
        Easing::Linear,
    );
    let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
}

/// Helper: sample a SkeletonPoint at clip-local time t.
pub fn sample_point_at(point: &SkeletonPoint, t: f32) -> PointState {
    keyframe::sample(&point.track, t).unwrap_or_default()
}

// ─── TIMELINE FIT ────────────────────────────────────────────────────

fn fit_timeline_to_clip_if_needed(state: &mut EditorState, ctx: &SourceClipCtx, rendered_w: f32) {
    let dur = ctx.clip_local_duration().max(0.05);
    let want_dur = state.skeleton_editor.fitted_for_duration;
    let want_w = state.skeleton_editor.fitted_for_width;
    let dur_changed = (want_dur - dur).abs() > 0.05 || want_dur <= 0.0;
    let width_changed = (want_w - rendered_w).abs() > 4.0 || want_w <= 0.0;
    if !dur_changed && !width_changed {
        return;
    }
    let target_w = (rendered_w - 24.0).max(60.0);
    let pps = (target_w / dur).clamp(8.0, 800.0);
    state.skeleton_editor.timeline_zoom = pps;
    state.skeleton_editor.timeline_scroll = 0.0;
    state.skeleton_editor.fitted_for_duration = dur;
    state.skeleton_editor.fitted_for_width = rendered_w;
}

// ─── INSPECTOR TIMELINE ──────────────────────────────────────────────

fn skeleton_timeline_height(state: &EditorState) -> f32 {
    let n_points = state
        .skeleton_editor
        .template_idx
        .and_then(|idx| state.scene.skeleton_templates.get(idx))
        .map(|t| t.points.len())
        .unwrap_or(0);
    let visible_rows = n_points.clamp(1, TIMELINE_MAX_VISIBLE_ROWS) as f32;
    TIMELINE_RULER_H + TIMELINE_ROW_H * visible_rows
}

fn skeleton_timeline(ui: &mut egui::Ui, state: &mut EditorState, ctx: &SourceClipCtx, width: f32) {
    let total_h = skeleton_timeline_height(state);

    // Snapshot per-point row info up front so the immutable borrow on
    // `scene.skeleton_templates` is released before we mutate state.
    let mut all_point_rows: Vec<(String, [u8; 3])> = Vec::new();
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        if let Some(t) = state.scene.skeleton_templates.get(tmpl_idx) {
            for (name, p) in &t.points {
                all_point_rows.push((name.clone(), p.color));
            }
        }
    }
    let total_rows = all_point_rows.len();

    let max_visible = TIMELINE_MAX_VISIBLE_ROWS;
    let visible_rows = total_rows.clamp(1, max_visible);
    let max_v_scroll = total_rows.saturating_sub(max_visible);
    if state.skeleton_editor.timeline_v_scroll > max_v_scroll {
        state.skeleton_editor.timeline_v_scroll = max_v_scroll;
    }
    let v_scroll = state.skeleton_editor.timeline_v_scroll;

    let mut point_rows: Vec<(String, [u8; 3])> = Vec::new();
    if total_rows == 0 {
        point_rows.push(("__empty__".into(), [120, 120, 140]));
    } else {
        let end = (v_scroll + max_visible).min(total_rows);
        for (name, color) in &all_point_rows[v_scroll..end] {
            point_rows.push((name.clone(), *color));
        }
    }

    let _track_h = TIMELINE_ROW_H * visible_rows as f32;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, total_h), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    let duration = ctx.clip_local_duration().max(0.05);
    let mut pps = state.skeleton_editor.timeline_zoom;

    if response.hovered() {
        let (scroll, modifiers) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers));
        if scroll.y.abs() > 0.1 {
            if modifiers.shift && max_v_scroll > 0 {
                let delta_rows = (scroll.y / 24.0).round() as i32;
                let new_v = (v_scroll as i32 - delta_rows).clamp(0, max_v_scroll as i32) as usize;
                state.skeleton_editor.timeline_v_scroll = new_v;
            } else {
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
                state.skeleton_editor.fitted_for_duration = duration;
                state.skeleton_editor.fitted_for_width = width;
            }
        }
        if scroll.x.abs() > 0.1 {
            state.skeleton_editor.timeline_scroll =
                (state.skeleton_editor.timeline_scroll - scroll.x / pps.max(1.0)).max(0.0);
        }
    }

    let scroll = state.skeleton_editor.timeline_scroll;

    let ruler_rect = Rect::from_min_size(rect.min, Vec2::new(width, TIMELINE_RULER_H));
    let track_rect = Rect::from_min_max(
        Pos2::new(rect.min.x, rect.min.y + TIMELINE_RULER_H),
        rect.max,
    );
    painter.rect_filled(ruler_rect, Rounding::same(2.0), COL_RULER);
    painter.rect_filled(track_rect, Rounding::same(2.0), COL_TRACK_BG);

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

    // Track-loop range underlay (when "Track" is engaged).
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
        let x0 = (rect.min.x + (lo - scroll) * pps).clamp(rect.min.x, rect.max.x);
        let x1 = (rect.min.x + (hi - scroll) * pps).clamp(rect.min.x, rect.max.x);
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

    // Per-point row background tinting.
    {
        let mut y = track_rect.min.y;
        for (i, _) in point_rows.iter().enumerate() {
            if i % 2 == 1 {
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(track_rect.min.x, y),
                        Pos2::new(track_rect.max.x, y + TIMELINE_ROW_H),
                    ),
                    Rounding::ZERO,
                    COL_TRACK_BG_ALT,
                );
            }
            y += TIMELINE_ROW_H;
            if i + 1 < point_rows.len() {
                painter.line_segment(
                    [
                        Pos2::new(track_rect.min.x, y),
                        Pos2::new(track_rect.max.x, y),
                    ],
                    Stroke::new(0.5, Color32::from_rgb(54, 52, 36)),
                );
            }
        }
    }

    // Keyframe diamonds.
    let mut keyframe_hits: Vec<(String, usize, Pos2, f32)> = Vec::new();
    if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
        let template = &state.scene.skeleton_templates[tmpl_idx];
        let selected = state.skeleton_editor.selected_point.clone();
        let mut row_top = track_rect.min.y;
        for (name, _color) in &point_rows {
            let row_center_y = row_top + TIMELINE_ROW_H * 0.5;
            row_top += TIMELINE_ROW_H;
            let Some(point) = template.points.get(name) else {
                continue;
            };

            let active = selected.as_deref() == Some(name) || selected.is_none();
            painter.text(
                Pos2::new(track_rect.min.x + 4.0, row_center_y),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(point.color[0], point.color[1], point.color[2]),
            );

            for (kf_idx, kf) in point.track.iter().enumerate() {
                let x = rect.min.x + (kf.t - scroll) * pps;
                if x < rect.min.x - 4.0 || x > rect.max.x + 4.0 {
                    continue;
                }
                let center = Pos2::new(x, row_center_y);
                let is_selected_kf = state.skeleton_editor.selected_keyframe.as_ref()
                    == Some(&(name.clone(), kf_idx));
                let col = if is_selected_kf {
                    COL_KF_SELECTED
                } else if active {
                    Color32::from_rgb(point.color[0], point.color[1], point.color[2])
                } else {
                    COL_KF_DIM
                };
                let r = if is_selected_kf { 5.5 } else { 4.0 };
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
                keyframe_hits.push((name.clone(), kf_idx, center, r));
            }
        }
    }

    // Playhead — synced to the main scene playhead, mapped to clip-local time.
    let cur_t = ctx.clip_local_time(state.playhead);
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

    // Interaction: click a keyframe to select / snap the playhead.
    let pointer_pos = response.interact_pointer_pos();
    let primary_clicked = response.clicked();
    let primary_dragged = response.dragged();

    let mut clicked_kf: Option<(String, usize)> = None;
    if primary_clicked {
        if let Some(p) = pointer_pos {
            if track_rect.contains(p) {
                let mut best: Option<(f32, (String, usize))> = None;
                for (name, idx, c, r) in &keyframe_hits {
                    let d = ((p.x - c.x).powi(2) + (p.y - c.y).powi(2)).sqrt();
                    let hit_radius = (r * 1.6).max(6.0);
                    if d < hit_radius && best.as_ref().map(|b| d < b.0).unwrap_or(true) {
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
        if let Some(tmpl_idx) = state.skeleton_editor.template_idx {
            if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get(&name) {
                if let Some(kf) = p.track.get(idx) {
                    let scene_t = ctx.t_in + kf.t / ctx.speed.max(0.0001);
                    state.playhead = scene_t.clamp(0.0, state.scene.output.duration);
                }
            }
        }
    } else if primary_clicked || primary_dragged {
        // Plain scrub — moves the main playhead in scene time.
        if let Some(p) = pointer_pos {
            let local_x = (p.x - rect.min.x).max(0.0);
            let new_local_t = (scroll + local_x / pps.max(1.0)).clamp(0.0, duration);
            let scene_t = ctx.t_in + new_local_t / ctx.speed.max(0.0001);
            state.playhead = scene_t.clamp(0.0, state.scene.output.duration);
        }
    }

    painter.rect_stroke(
        rect,
        Rounding::same(2.0),
        Stroke::new(1.0, Color32::from_rgb(62, 60, 42)),
    );
}

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

fn keyframe_easing_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some((name, idx)) = state.skeleton_editor.selected_keyframe.clone() else {
        return;
    };
    let Some(tmpl_idx) = state.skeleton_editor.template_idx else {
        return;
    };

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

    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{} '{}' #{}", crate::i18n::t("KF"), name, idx + 1))
                .size(11.0)
                .color(COL_TEXT_DIM),
        );
        ui.label(
            RichText::new(crate::i18n::t("transition:"))
                .size(10.0)
                .color(COL_TEXT_DIM),
        );

        for (label, value) in &[
            (crate::i18n::t("Step"), Easing::Step),
            (crate::i18n::t("Linear"), Easing::Linear),
            (crate::i18n::t("EaseIn"), Easing::EaseIn),
            (crate::i18n::t("EaseOut"), Easing::EaseOut),
            (crate::i18n::t("EaseInOut"), Easing::EaseInOut),
            (crate::i18n::t("Bezier"), Easing::Cubic),
        ] {
            let is_sel = current_easing == *value;
            if ui
                .selectable_label(is_sel, RichText::new(*label).size(10.0))
                .clicked()
            {
                new_easing = *value;
            }
        }

        if ui
            .small_button(
                RichText::new(crate::i18n::t("delete kf")).color(Color32::from_rgb(255, 120, 120)),
            )
            .clicked()
        {
            delete_kf = true;
        }
    });

    if new_easing != current_easing {
        if let Some(p) = state.scene.skeleton_templates[tmpl_idx]
            .points
            .get_mut(&name)
        {
            if let Some(kf) = p.track.get_mut(idx) {
                kf.easing = new_easing;
            }
        }
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
    }

    if delete_kf {
        if let Some(p) = state.scene.skeleton_templates[tmpl_idx]
            .points
            .get_mut(&name)
        {
            if idx < p.track.len() {
                p.track.remove(idx);
            }
        }
        state.skeleton_editor.selected_keyframe = None;
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
    }
}

// ─── POINT LIST PANEL ────────────────────────────────────────────────

fn point_list_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(crate::i18n::t("Points")).size(12.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(
                    RichText::new(crate::i18n::t("+ Add point"))
                        .color(Color32::from_rgb(120, 220, 140)),
                )
                .on_hover_text(crate::i18n::t(
                    "Add a new point with an auto-generated name and \
                     start placing it on the canvas.",
                ))
                .clicked()
            {
                add_auto_point(state);
            }
        });
    });

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
            RichText::new(crate::i18n::t("No points yet — press \"+ Add point\"."))
                .size(11.0)
                .color(COL_TEXT_DIM)
                .italics(),
        );
        return;
    }

    let mut to_remove: Option<String> = None;
    let mut to_select: Option<String> = None;
    let mut toggle_track: Option<String> = None;
    let mut drop_image_on_point: Option<(String, std::path::PathBuf)> = None;

    let drag_is_image = state.asset_drag.dragging.is_some()
        && state.asset_drag.kind == crate::state::AssetDragKind::Image;
    let dragged_image_path = if drag_is_image {
        state.asset_drag.dragging.clone()
    } else {
        None
    };
    let pointer_pos_for_drop = ui.input(|i| i.pointer.hover_pos());
    let pointer_released_for_drop = ui.input(|i| i.pointer.any_released());

    egui::ScrollArea::vertical()
        .id_source(("inspector_skel_points", tmpl_idx))
        .max_height(180.0)
        .show(ui, |ui| {
            for name in &point_names {
                let is_selected =
                    state.skeleton_editor.selected_point.as_deref() == Some(name);
                let is_tracking =
                    state.skeleton_editor.track_loop_point.as_deref() == Some(name);
                let point = &state.scene.skeleton_templates[tmpl_idx].points[name];
                let color = Color32::from_rgb(point.color[0], point.color[1], point.color[2]);
                let num_kf = point.track.len();
                let has_guide = state
                    .skeleton_editor
                    .point_guide_images
                    .contains_key(name);

                let row_bg = if is_selected {
                    Color32::from_rgb(50, 48, 32)
                } else {
                    Color32::TRANSPARENT
                };
                let frame = egui::Frame::none()
                    .fill(row_bg)
                    .rounding(Rounding::same(4.0))
                    .inner_margin(egui::Margin::symmetric(4.0, 2.0));
                let frame_resp = frame
                    .show(ui, |ui| {
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
                                        .small_button("×")
                                        .on_hover_text(crate::i18n::t("Remove point"))
                                        .clicked()
                                    {
                                        to_remove = Some(name.clone());
                                    }
                                    let track_label =
                                        if is_tracking { "\u{25A0}" } else { "\u{25B6}" };
                                    let track_color = if is_tracking {
                                        Color32::from_rgb(255, 160, 80)
                                    } else {
                                        Color32::from_rgb(140, 200, 255)
                                    };
                                    if ui
                                        .small_button(
                                            RichText::new(track_label).color(track_color),
                                        )
                                        .on_hover_text(if is_tracking {
                                            crate::i18n::t("Stop tracking")
                                        } else {
                                            crate::i18n::t(
                                                "Track: loop scene playback over this point's keyframe range",
                                            )
                                        })
                                        .clicked()
                                    {
                                        toggle_track = Some(name.clone());
                                    }
                                    if has_guide {
                                        ui.label(
                                            RichText::new("□")
                                                .size(10.0)
                                                .color(Color32::from_rgb(180, 220, 180)),
                                        )
                                        .on_hover_text(crate::i18n::t(
                                            "Guide image is set — drag a different image \
                                             from the Images library to replace it.",
                                        ));
                                    }
                                    ui.label(
                                        RichText::new(format!(
                                            "{} {}",
                                            num_kf,
                                            crate::i18n::t("kf")
                                        ))
                                        .size(9.0)
                                        .color(COL_TEXT_DIM),
                                    );
                                },
                            );
                        });
                    })
                    .response;

                let row_rect = frame_resp.rect;
                let is_drop_hover = drag_is_image
                    && pointer_pos_for_drop
                        .map(|p| row_rect.contains(p))
                        .unwrap_or(false);
                if is_drop_hover {
                    let painter = ui.painter_at(row_rect);
                    painter.rect_stroke(
                        row_rect,
                        Rounding::same(4.0),
                        Stroke::new(1.5, Color32::from_rgb(180, 220, 255)),
                    );
                    if pointer_released_for_drop {
                        if let Some(p) = dragged_image_path.clone() {
                            drop_image_on_point = Some((name.clone(), p));
                        }
                    }
                }
            }
        });

    if let Some(name) = to_select {
        state.skeleton_editor.selected_point = Some(name);
        state.skeleton_editor.selected_keyframe = None;
    }

    if let Some(name) = toggle_track {
        if state.skeleton_editor.track_loop_point.as_deref() == Some(&name) {
            state.skeleton_editor.track_loop_point = None;
        } else {
            state.skeleton_editor.selected_point = Some(name.clone());
            state.skeleton_editor.track_loop_point = Some(name.clone());
            // Jump the main scene playhead to the point's first kf.
            if let Some(p) = state.scene.skeleton_templates[tmpl_idx].points.get(&name) {
                if let Some(first) = p.track.first() {
                    // We don't have the source-clip context here; the
                    // inspector entry point will clamp on its next paint
                    // via `advance_track_loop`.
                    let scene_t_hint = first.t;
                    if state.playhead < scene_t_hint {
                        state.playhead = scene_t_hint;
                    }
                }
            }
            state.playing = true;
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
        state.skeleton_editor.point_guide_images.remove(&name);
        let _ = state.scene.skeleton_templates[tmpl_idx].save_alongside_clip();
        state.status = format!("{} {}", crate::i18n::t("Removed point:"), name);
    }

    if let Some((point_name, image_path)) = drop_image_on_point {
        state
            .skeleton_editor
            .point_guide_images
            .insert(point_name.clone(), image_path.clone());
        state.skeleton_editor.selected_point = Some(point_name.clone());
        state.asset_drag.dragging = None;
        state.asset_drag.kind = crate::state::AssetDragKind::None;
        state.asset_drag.label.clear();
        state.asset_drag.thumbnail = None;
        state.status = format!(
            "{} '{}': {}",
            crate::i18n::t("Guide image set for"),
            point_name,
            image_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(crate::i18n::t("(image)")),
        );
    }
}

// ─── GUIDE IMAGE PANEL ───────────────────────────────────────────────

fn point_guide_image_panel(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.label(
        RichText::new(crate::i18n::t("Guide image"))
            .size(12.0)
            .strong()
            .color(Color32::WHITE),
    );
    ui.label(
        RichText::new(crate::i18n::t(
            "Drag an image from the Images library onto a point row above \
             or into the box below. Visual aid only — not saved to the template.",
        ))
        .size(9.0)
        .color(COL_TEXT_DIM)
        .italics(),
    );
    ui.add_space(4.0);

    let Some(point_name) = state.skeleton_editor.selected_point.clone() else {
        ui.label(
            RichText::new(crate::i18n::t("Select a point first."))
                .size(10.0)
                .italics()
                .color(COL_TEXT_DIM),
        );
        return;
    };

    let current = state
        .skeleton_editor
        .point_guide_images
        .get(&point_name)
        .cloned();
    if let Some(p) = &current {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "□ {}",
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(crate::i18n::t("image")),
                ))
                .size(10.0)
                .color(COL_TEXT),
            );
            if ui
                .small_button(crate::i18n::t("clear"))
                .on_hover_text(crate::i18n::t("Remove the guide image"))
                .clicked()
            {
                state.skeleton_editor.point_guide_images.remove(&point_name);
            }
        });
        ui.add_space(2.0);
    }

    let drag_is_image = state.asset_drag.dragging.is_some()
        && state.asset_drag.kind == crate::state::AssetDragKind::Image;
    let dragged_path = if drag_is_image {
        state.asset_drag.dragging.clone()
    } else {
        None
    };
    let dragged_label = if drag_is_image {
        state.asset_drag.label.clone()
    } else {
        String::new()
    };
    let pointer_pos = ui.input(|i| i.pointer.hover_pos());
    let pointer_released = ui.input(|i| i.pointer.any_released());

    let zone_h = 56.0_f32;
    let zone_w = ui.available_width().max(120.0);
    let (zone_rect, _) = ui.allocate_exact_size(Vec2::new(zone_w, zone_h), Sense::hover());
    let painter = ui.painter_at(zone_rect);
    let hovered = drag_is_image && pointer_pos.map(|p| zone_rect.contains(p)).unwrap_or(false);
    let bg = if hovered {
        Color32::from_rgb(50, 80, 100)
    } else if current.is_some() {
        Color32::from_rgb(36, 40, 52)
    } else {
        Color32::from_rgb(32, 30, 20)
    };
    let stroke = if hovered {
        Stroke::new(1.5, Color32::from_rgb(180, 220, 255))
    } else {
        Stroke::new(1.0, Color32::from_rgb(62, 60, 42))
    };
    painter.rect_filled(zone_rect, Rounding::same(6.0), bg);
    painter.rect_stroke(zone_rect, Rounding::same(6.0), stroke);

    let caption = if hovered {
        format!(
            "\u{2935} {} \"{}\" \u{2192} {}",
            crate::i18n::t("drop"),
            if dragged_label.is_empty() {
                crate::i18n::t("image")
            } else {
                dragged_label.as_str()
            },
            point_name,
        )
    } else if current.is_some() {
        crate::i18n::t("Drop another image to replace.").to_string()
    } else {
        format!(
            "{} '{}'.",
            crate::i18n::t("Drag an image here for"),
            point_name
        )
    };
    painter.text(
        zone_rect.center(),
        egui::Align2::CENTER_CENTER,
        caption,
        egui::FontId::proportional(11.0),
        if hovered {
            Color32::from_rgb(220, 240, 255)
        } else {
            COL_TEXT_DIM
        },
    );

    if hovered && pointer_released {
        if let Some(p) = dragged_path {
            state
                .skeleton_editor
                .point_guide_images
                .insert(point_name.clone(), p.clone());
            state.asset_drag.dragging = None;
            state.asset_drag.kind = crate::state::AssetDragKind::None;
            state.asset_drag.label.clear();
            state.asset_drag.thumbnail = None;
            state.status = format!(
                "{} '{}': {}",
                crate::i18n::t("Guide image set for"),
                point_name,
                p.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(crate::i18n::t("(image)")),
            );
        }
    }
}

// ─── AUTO-NAMED POINT INSERTION ──────────────────────────────────────

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
    state.status = format!(
        "{} {}. {}",
        crate::i18n::t("Added point:"),
        name,
        crate::i18n::t("Drag it on the canvas to record keyframes.")
    );
}

// Suppress dead-code warning for the legacy default-point colour
// constant (kept for the future palette expansion).
#[allow(dead_code)]
const _UNUSED_COLORS: [Color32; 1] = [COL_POINT_DEFAULT];
