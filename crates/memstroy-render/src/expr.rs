//! ffmpeg-expression builders that mirror the canvas preview's
//! per-element transform pipeline.
//!
//! The renderer used to emit linear-only piecewise position/scale
//! expressions and silently dropped most of the preview's
//! transformation pipeline (Free Canvas v2 world positions, render
//! frame motion, keyframe easing, per-frame rotation, modifiers,
//! colour correction). Those drops are exactly what the user sees as
//! "клипы в видеоролике совершенно не так выглядят, как на
//! предпросмотре" — the export ran on a frozen subset of the model.
//!
//! This module owns the maths that translates the preview's
//! world-pixel + render-frame-relative model into ffmpeg `overlay=`
//! expressions, plus helpers for `rotate=`, `colorchannelmixer=aa=`,
//! `eq=`, `colorbalance=`, `curves=` (for tone curves + LGG), the
//! modifier (Wobble / Shake / Pulse / Spin / Walk) overlays, and the
//! Skeleton-Constructor attachment maths. Building blocks are
//! intentionally compact so the resulting filter_complex graphs stay
//! within ffmpeg's expression size limits even with dozens of
//! keyframes per element.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memstroy_core::keyframe::{ModifierKind, TrackModifier};
use memstroy_core::{
    ActorState, CanvasTransform, ColorCorrection, Easing, Effect, EffectKind, Keyframe,
    OverlayState, PointState, RenderFrameState, Scene, SkeletonAttachment,
};

// ─── PIECEWISE EXPRESSIONS ──────────────────────────────────────────

/// Generic piecewise builder parameterised by the time variable name
/// — `"t"` for scene-time tracks and `"(t-t_in)"` for clip-local
/// tracks. Callers normally use [`piecewise`] / [`piecewise_local`]
/// instead of touching this directly.
fn piecewise_with_time<T, F>(time_var: &str, kfs: &[Keyframe<T>], getter: F) -> String
where
    F: Fn(&T) -> f32,
{
    if kfs.is_empty() {
        return "0".to_string();
    }
    if kfs.len() == 1 {
        return format!("{:.6}", getter(&kfs[0].value));
    }
    let mut expr = format!("{:.6}", getter(&kfs.last().unwrap().value));
    for win in kfs.windows(2).rev() {
        let (a, b) = (&win[0], &win[1]);
        let v0 = getter(&a.value);
        let v1 = getter(&b.value);
        let span = (b.t - a.t).max(1e-6);
        let u = format!("(({}-{:.6})/{:.6})", time_var, a.t, span);
        let segment = match b.easing {
            // Step holds the previous value until the next keyframe,
            // matching `Easing::Step::apply(t) → 0.0` in `easing.rs`.
            Easing::Step => format!("{:.6}", v0),
            Easing::Linear => {
                format!("({:.6}+({:.6})*{u})", v0, v1 - v0, u = u)
            }
            Easing::EaseIn => {
                format!("({:.6}+({:.6})*({u})*({u}))", v0, v1 - v0, u = u)
            }
            Easing::EaseOut => {
                format!(
                    "({:.6}+({:.6})*(1-(1-{u})*(1-{u})))",
                    v0,
                    v1 - v0,
                    u = u,
                )
            }
            Easing::EaseInOut => {
                format!(
                    "({v0:.6}+({d:.6})*if(lt({u},0.5),2*({u})*({u}),1-(-2*({u})+2)*(-2*({u})+2)/2))",
                    v0 = v0,
                    d = v1 - v0,
                    u = u,
                )
            }
            Easing::Cubic => {
                // Smoothstep — 3u² − 2u³, the same fallback the
                // preview uses when the keyframe is marked `Cubic`
                // (until param bezier ships).
                format!(
                    "({:.6}+({:.6})*({u})*({u})*(3-2*({u})))",
                    v0,
                    v1 - v0,
                    u = u,
                )
            }
        };
        expr = format!("if(lt({},{:.6}),{},{})", time_var, b.t, segment, expr);
    }
    expr
}

/// Build a piecewise ffmpeg expression for a scalar function of `t`
/// (scene time). Each segment honours the keyframe's `easing` and
/// matches `keyframe::sample` exactly.
pub(crate) fn piecewise<T, F>(kfs: &[Keyframe<T>], getter: F) -> String
where
    F: Fn(&T) -> f32,
{
    piecewise_with_time("t", kfs, getter)
}

/// Same as [`piecewise`] but treats keyframe times as CLIP-LOCAL —
/// i.e. samples at `(t - t_in)`. Used for `Effect::param_kfs` and
/// `ColorCorrection.kfs` which the inspector authors relative to the
/// host clip's t_in (so animations slide along with the clip when the
/// user trims it).
pub(crate) fn piecewise_local<T, F>(kfs: &[Keyframe<T>], t_in: f32, getter: F) -> String
where
    F: Fn(&T) -> f32,
{
    let local = format!("(t-{:.6})", t_in);
    piecewise_with_time(&local, kfs, getter)
}

// ─── MODIFIER (Wobble / Shake / Pulse / Spin / Walk) ────────────────

pub(crate) struct ModifierExpr {
    pub dx: String,
    pub dy: String,
    /// Rotation delta in degrees.
    pub drot_deg: String,
    /// Linear scale delta added on top of the keyframed scale.
    pub dscale: String,
}

impl ModifierExpr {
    pub fn zero() -> Self {
        Self {
            dx: "0".into(),
            dy: "0".into(),
            drot_deg: "0".into(),
            dscale: "0".into(),
        }
    }
    pub fn dx_is_zero(&self) -> bool { self.dx == "0" }
    pub fn dy_is_zero(&self) -> bool { self.dy == "0" }
    pub fn drot_is_zero(&self) -> bool { self.drot_deg == "0" }
    pub fn dscale_is_zero(&self) -> bool { self.dscale == "0" }
}

/// Build the modifier offset expressions in clip-local time `t - t_in`.
///
/// Mirrors `memstroy_core::keyframe::evaluate_modifiers` exactly so
/// the export tracks the on-canvas wobble / shake / pulse / spin /
/// walk that the preview applies on top of the eased keyframe sample.
pub(crate) fn build_modifier_expr(modifiers: &[TrackModifier], t_in: f32) -> ModifierExpr {
    if modifiers.is_empty() || modifiers.iter().all(|m| !m.enabled) {
        return ModifierExpr::zero();
    }
    let local = format!("(t-{:.6})", t_in);
    let mut dx = String::new();
    let mut dy = String::new();
    let mut drot = String::new();
    let mut dscale = String::new();
    let push = |slot: &mut String, term: String| {
        if slot.is_empty() {
            *slot = term;
        } else {
            *slot = format!("({}+{})", slot, term);
        }
    };
    for m in modifiers.iter().filter(|m| m.enabled) {
        match m.kind {
            ModifierKind::Wobble {
                freq_hz,
                amp_x,
                amp_y,
                amp_rot_deg,
                phase,
            } => {
                let omega = format!(
                    "(2*PI*{:.6}*{}+{:.6})",
                    freq_hz, local, phase,
                );
                if amp_x.abs() > 1e-6 {
                    push(&mut dx, format!("({:.6})*sin({})", amp_x, omega));
                }
                if amp_y.abs() > 1e-6 {
                    push(&mut dy, format!("({:.6})*cos({}*0.7)", amp_y, omega));
                }
                if amp_rot_deg.abs() > 1e-6 {
                    push(
                        &mut drot,
                        format!("({:.6})*sin({})", amp_rot_deg, omega),
                    );
                }
            }
            ModifierKind::Shake {
                freq_hz,
                amp_x,
                amp_y,
                seed,
            } => {
                // Same 3-octave sin-sum the preview uses
                // (`evaluate_modifiers` in keyframe.rs). We hash the
                // seed to a phase so two Shake instances with
                // different seeds don't produce identical jitter.
                let phase_x =
                    ((seed as f32) * 0.137).fract() * std::f32::consts::TAU;
                let phase_y =
                    ((seed as f32) * 0.731 + 1.7).fract() * std::f32::consts::TAU;
                let w = format!("(2*PI*{:.6}*{})", freq_hz, local);
                let nx = format!(
                    "(sin(({w})+{px:.6})+0.5*sin(({w})*2.13+{px:.6})+0.25*sin(({w})*4.27+{px:.6}))",
                    w = w,
                    px = phase_x,
                );
                let ny = format!(
                    "(cos(({w})+{py:.6})+0.5*cos(({w})*2.31+{py:.6})+0.25*cos(({w})*3.97+{py:.6}))",
                    w = w,
                    py = phase_y,
                );
                if amp_x.abs() > 1e-6 {
                    push(&mut dx, format!("({:.6})*({})*0.6", amp_x, nx));
                }
                if amp_y.abs() > 1e-6 {
                    push(&mut dy, format!("({:.6})*({})*0.6", amp_y, ny));
                }
            }
            ModifierKind::Pulse { freq_hz, amp_scale } => {
                if amp_scale.abs() > 1e-6 {
                    let omega = format!("(2*PI*{:.6}*{})", freq_hz, local);
                    push(
                        &mut dscale,
                        format!("({:.6})*sin({})", amp_scale, omega),
                    );
                }
            }
            ModifierKind::Spin { speed_dps } => {
                if speed_dps.abs() > 1e-6 {
                    push(&mut drot, format!("({:.6})*{}", speed_dps, local));
                }
            }
            ModifierKind::Walk {
                freq_hz,
                amp_deg,
                bob_y,
                phase,
            } => {
                let omega = format!(
                    "(2*PI*{:.6}*{}+{:.6})",
                    freq_hz, local, phase,
                );
                if amp_deg.abs() > 1e-6 {
                    push(&mut drot, format!("({:.6})*sin({})", amp_deg, omega));
                }
                if bob_y.abs() > 1e-4 {
                    // 2× cadence with a non-negative envelope, exactly
                    // mirroring the preview's `(1 - cos(2w))*0.5` bob.
                    push(
                        &mut dy,
                        format!("({:.6})*(1-cos(2*({})))*0.5", bob_y, omega),
                    );
                }
            }
        }
    }
    ModifierExpr {
        dx: if dx.is_empty() { "0".into() } else { dx },
        dy: if dy.is_empty() { "0".into() } else { dy },
        drot_deg: if drot.is_empty() { "0".into() } else { drot },
        dscale: if dscale.is_empty() { "0".into() } else { dscale },
    }
}

// ─── ELEMENT TRANSFORM ──────────────────────────────────────────────

/// Common subset of `ActorState` and `OverlayState` that the renderer
/// needs to read uniformly.
pub(crate) trait PositionedState {
    fn pos(&self) -> [f32; 2];
    fn scale(&self) -> f32;
    fn scale_y(&self) -> f32;
    fn rotation_deg(&self) -> f32;
    fn opacity(&self) -> f32;
    fn flip_x_anim(&self) -> f32;
    fn flip_y_anim(&self) -> f32;
}
impl PositionedState for ActorState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
    fn scale_y(&self) -> f32 { self.scale_y }
    fn rotation_deg(&self) -> f32 { self.rotation_deg }
    fn opacity(&self) -> f32 { self.opacity }
    fn flip_x_anim(&self) -> f32 { self.flip_x_anim }
    fn flip_y_anim(&self) -> f32 { self.flip_y_anim }
}
impl PositionedState for OverlayState {
    fn pos(&self) -> [f32; 2] { self.pos }
    fn scale(&self) -> f32 { self.scale }
    fn scale_y(&self) -> f32 { self.scale_y }
    fn rotation_deg(&self) -> f32 { self.rotation_deg }
    fn opacity(&self) -> f32 { self.opacity }
    fn flip_x_anim(&self) -> f32 { self.flip_x_anim }
    fn flip_y_anim(&self) -> f32 { self.flip_y_anim }
}

/// Per-element transform expressions ready to plug into ffmpeg.
pub(crate) struct ElementTransform {
    /// Output overlay X expression (top-left of overlay) — for `overlay=x=`.
    pub x_expr: String,
    /// Output overlay Y expression (top-left of overlay) — for `overlay=y=`.
    pub y_expr: String,
    /// Centre X (without `-w/2` adjustment) — for callers that need the
    /// element's centre in output space (e.g. text rasterise → overlay).
    pub centre_x_expr: String,
    pub centre_y_expr: String,
    /// Scale X expression — for `scale=w='iw*<sx>'`.
    pub sx_expr: String,
    /// Scale Y expression — for `scale=h='ih*<sy>'`. Already includes
    /// the legacy `scale_y` factor (so callers never multiply twice).
    pub sy_expr: String,
    /// Per-frame rotation expression in radians (`Some` only when
    /// any keyframe / modifier produces a non-trivial angle, so the
    /// fast path stays free of the costly `rotate` filter).
    pub rot_expr: Option<String>,
    /// Static opacity sample at the layout midpoint. `1.0` means the
    /// alpha multiplication can be skipped.
    pub opacity_static: f32,
    /// Whether the layout's opacity actually moves between keyframes
    /// (the export is still applied as static; this flag lets the
    /// caller log a one-line warning when an animation is silently
    /// flattened).
    pub opacity_animated: bool,
    /// Static flips sampled at the midpoint keyframe — the predominant
    /// flip state shown in the preview.
    pub hflip: bool,
    pub vflip: bool,
}

/// Build the full overlay-time transform for an element identified by
/// `element_id`. This is the renderer's equivalent of
/// `canvas_preview::get_element_world_pos` + the legacy keyframe
/// sample + modifier eval — but expressed as ffmpeg expressions of `t`.
///
/// Position priority matches the preview:
///   1. `Scene.canvas_layouts[element_id]` (Free Canvas v2 world px).
///   2. The legacy normalised `[0,1]` `layout`, converted to world
///      space relative to the moving render frame (so animating
///      `render_frame.pos` shifts elements like the preview).
///
/// Scale / rotation / opacity / flips always come from `legacy_layout`
/// (the canvas_layouts override only carries position, mirroring the
/// preview where `get_element_world_pos` returns *only* `WorldPos`).
pub(crate) fn build_element_transform<S>(
    scene: &Scene,
    element_id: &str,
    legacy_layout: &[Keyframe<S>],
    modifiers: &[TrackModifier],
    t_in: f32,
) -> ElementTransform
where
    S: PositionedState + Clone,
{
    let [out_w, out_h] = scene.output.resolution;
    let half_w = out_w as f32 * 0.5;
    let half_h = out_h as f32 * 0.5;

    // ── Render-frame expressions ─────────────────────────────────────
    let rf = &scene.render_frame;
    let rf_pos_x_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.pos.x);
    let rf_pos_y_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.pos.y);
    let rf_zoom_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.zoom.max(1e-4));
    let rf_mods = build_modifier_expr(&rf.modifiers, 0.0);
    let rf_pos_x = if rf_mods.dx_is_zero() {
        rf_pos_x_kf.clone()
    } else {
        format!("(({})+({}))", rf_pos_x_kf, rf_mods.dx)
    };
    let rf_pos_y = if rf_mods.dy_is_zero() {
        rf_pos_y_kf.clone()
    } else {
        format!("(({})+({}))", rf_pos_y_kf, rf_mods.dy)
    };

    // ── World position (canvas_layouts override OR legacy fallback) ──
    let canvas_layout = scene
        .canvas_layouts
        .iter()
        .find(|cl| cl.element_id == element_id);
    let (world_x, world_y) = if let Some(cl) = canvas_layout {
        (
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.x),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.y),
        )
    } else {
        // Legacy normalised pos → world pixels relative to render frame.
        // world_size_x(t) = out_w / rf_zoom(t)
        // world_x(t) = rf_pos_x(t) + (pos_x(t) - 0.5) * world_size_x(t)
        let pos_x = piecewise(legacy_layout, |s: &S| s.pos()[0]);
        let pos_y = piecewise(legacy_layout, |s: &S| s.pos()[1]);
        let wx = format!(
            "(({rfx})+({px}-0.5)*({W}/({rfz})))",
            rfx = rf_pos_x,
            px = pos_x,
            W = out_w as f32,
            rfz = rf_zoom_kf,
        );
        let wy = format!(
            "(({rfy})+({py}-0.5)*({H}/({rfz})))",
            rfy = rf_pos_y,
            py = pos_y,
            H = out_h as f32,
            rfz = rf_zoom_kf,
        );
        (wx, wy)
    };

    // ── Layered modifier offsets (clip-local time) ───────────────────
    let mods = build_modifier_expr(modifiers, t_in);
    let world_x = if mods.dx_is_zero() {
        world_x
    } else {
        format!("(({})+({}))", world_x, mods.dx)
    };
    let world_y = if mods.dy_is_zero() {
        world_y
    } else {
        format!("(({})+({}))", world_y, mods.dy)
    };

    // ── World → output canvas: composite is centred at render_frame.pos ─
    // The existing `emit_render_frame_camera` does a centre crop +
    // scale based on `rf.zoom` / `rf.rotation_deg`, which matches a
    // composite whose centre IS render_frame.pos. So we bake the
    // translation into overlay X/Y here and leave zoom/rotation to
    // the camera pass — exactly how the preview decomposes the
    // problem.
    let centre_x_expr = format!(
        "(({wx})-({rfx})+{half_w:.4})",
        wx = world_x,
        rfx = rf_pos_x,
        half_w = half_w,
    );
    let centre_y_expr = format!(
        "(({wy})-({rfy})+{half_h:.4})",
        wy = world_y,
        rfy = rf_pos_y,
        half_h = half_h,
    );
    let x_expr = format!("({})-w/2", centre_x_expr);
    let y_expr = format!("({})-h/2", centre_y_expr);

    // ── Scale (with Pulse modifier added, scale_y multiplied) ────────
    let scale_base = piecewise(legacy_layout, |s: &S| s.scale());
    let scale_y_factor = piecewise(legacy_layout, |s: &S| s.scale_y());
    let sx_expr = if mods.dscale_is_zero() {
        scale_base.clone()
    } else {
        format!("(({})+({}))", scale_base, mods.dscale)
    };
    let sy_expr = if mods.dscale_is_zero() {
        format!("({})*({})", scale_base, scale_y_factor)
    } else {
        format!("(({})+({}))*({})", scale_base, mods.dscale, scale_y_factor)
    };

    // ── Rotation — combine layout + Spin / Wobble / Walk drot ────────
    let rot_deg_layout = piecewise(legacy_layout, |s: &S| s.rotation_deg());
    let layout_has_rot = legacy_layout
        .iter()
        .any(|kf| kf.value.rotation_deg().abs() > 0.05);
    let rot_expr = if layout_has_rot || !mods.drot_is_zero() {
        let rot_deg_total = if mods.drot_is_zero() {
            rot_deg_layout
        } else {
            format!("(({})+({}))", rot_deg_layout, mods.drot_deg)
        };
        Some(format!("(({})*PI/180)", rot_deg_total))
    } else {
        None
    };

    // ── Opacity (static — sampled at midpoint to match the dominant
    //    on-canvas alpha; per-frame opacity needs `geq` and is left as
    //    a follow-up because the cost dwarfs the visual benefit). ───
    let opacity_static = legacy_layout
        .get(legacy_layout.len() / 2)
        .map(|kf| kf.value.opacity().clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let opacity_animated = legacy_layout
        .windows(2)
        .any(|w| (w[0].value.opacity() - w[1].value.opacity()).abs() > 1e-3);

    // ── Flip — static midpoint sample, same fallback the preview's
    //    canvas mesh path uses for the dominant side of an animation.
    let mid = legacy_layout.get(legacy_layout.len() / 2);
    let hflip = mid.map(|kf| kf.value.flip_x_anim() < 0.0).unwrap_or(false);
    let vflip = mid.map(|kf| kf.value.flip_y_anim() < 0.0).unwrap_or(false);

    ElementTransform {
        x_expr,
        y_expr,
        centre_x_expr,
        centre_y_expr,
        sx_expr,
        sy_expr,
        rot_expr,
        opacity_static,
        opacity_animated,
        hflip,
        vflip,
    }
}

// ─── SKELETON ATTACHMENTS ───────────────────────────────────────────

/// Override [`ElementTransform`] so the element's centre tracks a
/// named point on a host actor's skeleton. Returns `None` when the
/// matching `SkeletonTemplate` or host `Actor` can't be found — the
/// caller then falls through to `build_element_transform` so the
/// element still renders at its authored position, matching the
/// preview's "skeleton missing → use legacy layout" fallback.
///
/// `host_src_w` / `host_src_h` are the host clip's native pixel
/// dimensions (typically obtained via [`probe_video_dimensions`]
/// before ffmpeg runs, since the renderer can't query iw/ih during
/// graph construction). Sane fallback `(1080, 1920)` matches the
/// preview's `frame_caches` default.
///
/// The returned transform's `sx_expr` / `sy_expr` already incorporate
/// the attachment's `scale` multiplier (and the host's scale when
/// `follow_rotation` is set, the host's rotation too); rotation /
/// opacity / flip on the attached element's own layout are honoured
/// independently.
pub(crate) fn build_skeleton_attachment_transform<S>(
    scene: &Scene,
    attachment: &SkeletonAttachment,
    element_layout: &[Keyframe<S>],
    element_modifiers: &[TrackModifier],
    element_t_in: f32,
    host_src_w: f32,
    host_src_h: f32,
) -> Option<ElementTransform>
where
    S: PositionedState + Clone,
{
    // 1) Locate the skeleton template by id (matches `name` or the
    //    source clip's file stem — same lookup the preview uses).
    let template = scene.skeleton_templates.iter().find(|tmpl| {
        tmpl.name == attachment.skeleton_id
            || tmpl
                .source_clip
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == attachment.skeleton_id)
                .unwrap_or(false)
    })?;
    let point = template.points.get(&attachment.point_name)?;
    if point.track.is_empty() {
        return None;
    }

    // 2) Locate the host actor — same source_clip OR matching file
    //    name (the preview accepts either).
    let host = scene.actors.iter().find(|a| {
        a.source == template.source_clip
            || a.source.file_name() == template.source_clip.file_name()
    })?;
    let host_t_in = host.t_in.unwrap_or(0.0);

    // 3) Build the host's centre-on-canvas + scale + rotation
    //    expressions. We reuse `build_element_transform` for the
    //    centre (so canvas_layouts / render_frame motion / host
    //    modifiers all flow through), then sample scale / rotation
    //    directly to compose the attachment maths.
    let host_xform = build_element_transform(
        scene,
        &host.id,
        &host.layout,
        &host.modifiers,
        host_t_in,
    );
    let host_scale_kf = piecewise(&host.layout, |s: &ActorState| s.scale);
    let host_scale_y_kf = piecewise(&host.layout, |s: &ActorState| s.scale_y);
    let host_mods = build_modifier_expr(&host.modifiers, host_t_in);
    let host_scale = if host_mods.dscale_is_zero() {
        host_scale_kf.clone()
    } else {
        format!("(({})+({}))", host_scale_kf, host_mods.dscale)
    };
    let host_rot_deg_kf = piecewise(&host.layout, |s: &ActorState| s.rotation_deg);
    let host_rot_deg = if host_mods.drot_is_zero() {
        host_rot_deg_kf
    } else {
        format!("(({})+({}))", host_rot_deg_kf, host_mods.drot_deg)
    };
    let host_rot_rad = format!("(({})*PI/180)", host_rot_deg);

    // 4) Sample the skeleton point's normalised x/y at scene time
    //    (the preview passes scene `t` directly to
    //    `template.sample_point`; tracks are authored against scene
    //    time, not clip-local time).
    let off_x = attachment.offset[0];
    let off_y = attachment.offset[1];
    let nx = piecewise(&point.track, |s: &PointState| s.x + off_x - 0.5);
    let ny = piecewise(&point.track, |s: &PointState| s.y + off_y - 0.5);

    // 5) Local offset (in host pixel-space) → rotated by host rotation.
    let host_w = format!("({:.4}*({}))", host_src_w, host_scale);
    let host_h = format!("({:.4}*({})*({}))", host_src_h, host_scale, host_scale_y_kf);
    let local_x = format!("(({})*{})", nx, host_w);
    let local_y = format!("(({})*{})", ny, host_h);

    let centre_x = format!(
        "(({hcx})+({lx})*cos({rot})-({ly})*sin({rot}))",
        hcx = host_xform.centre_x_expr,
        lx = local_x,
        ly = local_y,
        rot = host_rot_rad,
    );
    let centre_y = format!(
        "(({hcy})+({lx})*sin({rot})+({ly})*cos({rot}))",
        hcy = host_xform.centre_y_expr,
        lx = local_x,
        ly = local_y,
        rot = host_rot_rad,
    );

    // 6) The attached element's own layout still drives scale /
    //    rotation / opacity / flip; we just OVERRIDE position.
    //    Combine attachment.scale on top of the layout scale so a
    //    `scale=2.0` attachment doubles the asset's size after the
    //    host's own scaling.
    let element_scale_base = piecewise(element_layout, |s: &S| s.scale());
    let element_scale_y_factor = piecewise(element_layout, |s: &S| s.scale_y());
    let elem_mods = build_modifier_expr(element_modifiers, element_t_in);
    let att_scale = format!("({:.6})", attachment.scale);
    let sx_expr = if elem_mods.dscale_is_zero() {
        format!("({})*({})", element_scale_base, att_scale)
    } else {
        format!("(({})+({}))*({})", element_scale_base, elem_mods.dscale, att_scale)
    };
    let sy_expr = if elem_mods.dscale_is_zero() {
        format!(
            "({})*({})*({})",
            element_scale_base, element_scale_y_factor, att_scale
        )
    } else {
        format!(
            "(({})+({}))*({})*({})",
            element_scale_base, elem_mods.dscale, element_scale_y_factor, att_scale
        )
    };

    // 7) Rotation — element's own rotation_deg, plus host's when
    //    `follow_rotation` is set (matches preview's behaviour).
    let elem_rot_deg = piecewise(element_layout, |s: &S| s.rotation_deg());
    let layout_has_rot = element_layout
        .iter()
        .any(|kf| kf.value.rotation_deg().abs() > 0.05);
    let need_rot = layout_has_rot
        || !elem_mods.drot_is_zero()
        || attachment.follow_rotation;
    let rot_expr = if need_rot {
        let mut deg = if elem_mods.drot_is_zero() {
            elem_rot_deg
        } else {
            format!("(({})+({}))", elem_rot_deg, elem_mods.drot_deg)
        };
        if attachment.follow_rotation {
            deg = format!("(({})+({}))", deg, host_rot_deg);
        }
        Some(format!("(({})*PI/180)", deg))
    } else {
        None
    };

    let opacity_static = element_layout
        .get(element_layout.len() / 2)
        .map(|kf| kf.value.opacity().clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let opacity_animated = element_layout
        .windows(2)
        .any(|w| (w[0].value.opacity() - w[1].value.opacity()).abs() > 1e-3);
    let mid = element_layout.get(element_layout.len() / 2);
    let hflip = mid.map(|kf| kf.value.flip_x_anim() < 0.0).unwrap_or(false);
    let vflip = mid.map(|kf| kf.value.flip_y_anim() < 0.0).unwrap_or(false);

    Some(ElementTransform {
        x_expr: format!("({})-w/2", centre_x),
        y_expr: format!("({})-h/2", centre_y),
        centre_x_expr: centre_x,
        centre_y_expr: centre_y,
        sx_expr,
        sy_expr,
        rot_expr,
        opacity_static,
        opacity_animated,
        hflip,
        vflip,
    })
}

/// Locate the host actor for a `SkeletonAttachment` so the renderer
/// can pre-probe its source dimensions before building the
/// filter_complex graph. Returns `None` when neither the template nor
/// the host can be resolved.
pub(crate) fn skeleton_host_source<'a>(
    scene: &'a Scene,
    attachment: &SkeletonAttachment,
) -> Option<&'a Path> {
    let template = scene.skeleton_templates.iter().find(|tmpl| {
        tmpl.name == attachment.skeleton_id
            || tmpl
                .source_clip
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == attachment.skeleton_id)
                .unwrap_or(false)
    })?;
    let host = scene.actors.iter().find(|a| {
        a.source == template.source_clip
            || a.source.file_name() == template.source_clip.file_name()
    })?;
    Some(host.source.as_path())
}

// ─── EFFECT FILTERS (animated + static) ─────────────────────────────

/// Build a filter snippet for one [`Effect`] honouring per-parameter
/// keyframes when present.
///
/// For effect kinds whose ffmpeg counterparts accept time-varying
/// expressions (`eq`, `hue`, `boxblur`, `gblur`, `vignette`) the
/// returned snippet uses ffmpeg expressions of `t` so the export
/// animates exactly like the preview. For kinds whose ffmpeg
/// counterparts only accept scalar parameters (`pixelate`,
/// `posterize`, `noise`, `chromatic_aberration`, …) we sample at the
/// clip midpoint via `Effect::sampled_at` — same flattening rule the
/// preview-vs-export parity fix uses for opacity.
///
/// Returns `None` when the effect is disabled, has zero static
/// intensity AND no animation, or when the kind has no static
/// translation either (matching the existing `effect_to_filter`
/// `None` arm for `Wave` etc.).
pub(crate) fn effect_filter_at(eff: &Effect, t_in: f32, t_out: Option<f32>) -> Option<String> {
    if !eff.enabled {
        return None;
    }
    let intensity_animated = eff.animated_params.contains("intensity")
        && eff
            .param_kfs
            .get("intensity")
            .map(|kfs| !kfs.is_empty())
            .unwrap_or(false);
    if eff.intensity.clamp(0.0, 1.0) <= 0.001 && !intensity_animated {
        return None;
    }

    // Helper: returns either a piecewise expression for the parameter
    // (when authored as animated AND has at least one keyframe) or
    // the static value as a literal. Always returns a STRING that
    // ffmpeg's expression parser will accept.
    let param_expr = |key: &str, fallback: f32| -> (String, bool) {
        if eff.animated_params.contains(key) {
            if let Some(kfs) = eff.param_kfs.get(key) {
                if !kfs.is_empty() {
                    return (piecewise_local(kfs, t_in, |v: &f32| *v), true);
                }
            }
        }
        (format!("{:.6}", fallback), false)
    };
    let (intensity_expr, _) = param_expr("intensity", eff.intensity);
    let any_anim_p0 = eff.animated_params.contains("p0")
        && eff.param_kfs.get("p0").map(|k| !k.is_empty()).unwrap_or(false);
    let any_anim_p1 = eff.animated_params.contains("p1")
        && eff.param_kfs.get("p1").map(|k| !k.is_empty()).unwrap_or(false);
    let any_anim = intensity_animated || any_anim_p0 || any_anim_p1;

    use EffectKind as K;
    let snippet = match &eff.kind {
        // ── Filters that accept ffmpeg expressions per-frame ────────
        K::Brightness { amount } => {
            let (a, _) = param_expr("p0", *amount);
            // eq=brightness range is [-1, 1]; clamp the product so the
            // filter doesn't reject values outside the documented range.
            format!(
                "eq=brightness='clip(({a})*({i})\\,-1\\,1)':eval=frame",
                a = a,
                i = intensity_expr,
            )
        }
        K::Contrast { amount } => {
            let (a, _) = param_expr("p0", *amount);
            // eq=contrast must stay >= 0 (negative produces bizarre
            // output); preview's formula is `1 + amount*intensity`.
            format!(
                "eq=contrast='max(0\\,1+({a})*({i}))':eval=frame",
                a = a,
                i = intensity_expr,
            )
        }
        K::Saturation { amount } => {
            let (a, _) = param_expr("p0", *amount);
            format!(
                "eq=saturation='max(0\\,1+({a})*({i}))':eval=frame",
                a = a,
                i = intensity_expr,
            )
        }
        K::HueShift { degrees } => {
            let (d, _) = param_expr("p0", *degrees);
            // `hue=h=` accepts an expression evaluated per-frame.
            format!("hue=h='({d})*({i})'", d = d, i = intensity_expr)
        }
        K::Blur { radius } => {
            let (r, _) = param_expr("p0", *radius);
            // boxblur requires a non-negative integer radius after
            // ffmpeg's expression parser converts the result; max(0.5)
            // keeps the radius in the legal range when intensity → 0.
            format!(
                "boxblur=luma_radius='max(0.5\\,({r})*({i}))':luma_power=1",
                r = r,
                i = intensity_expr,
            )
        }
        K::Glow { radius, intensity } => {
            let (r, _) = param_expr("p0", *radius);
            let (g_i, _) = param_expr("p1", *intensity);
            format!(
                "gblur=sigma='max(1\\,({r})*({i}))',eq=brightness='clip(({gi})*({i})*0.15\\,-1\\,1)':eval=frame",
                r = r,
                i = intensity_expr,
                gi = g_i,
            )
        }
        K::Bloom { radius } => {
            let (r, _) = param_expr("p0", *radius);
            format!(
                "gblur=sigma='max(1\\,({r})*({i}))'",
                r = r,
                i = intensity_expr,
            )
        }
        K::Vignette { strength } => {
            let (s, _) = param_expr("p0", *strength);
            format!(
                "vignette=angle='PI/3*clip(({s})*({i})\\,0\\,1)':mode=forward:eval=frame",
                s = s,
                i = intensity_expr,
            )
        }
        // ── Everything else: fall back to the existing static path. ─
        //
        // For these kinds the parameter is fed straight into a
        // non-expression filter slot (Pixelate / Posterize block size,
        // NumColours, Crop ratios, etc.). We honour animation by
        // sampling at the clip midpoint via `Effect::sampled_at` so the
        // export reflects the dominant value the preview shows.
        _ => {
            if any_anim {
                let span = t_out.unwrap_or(t_in + 1.0) - t_in;
                let mid_local = (span * 0.5).max(0.0);
                let snapshot = eff.sampled_at(mid_local);
                let inten = snapshot.intensity.clamp(0.0, 1.0);
                if inten <= 0.001 {
                    return None;
                }
                tracing::debug!(
                    effect_label = %eff.kind.label(),
                    "animated effect parameters baked at midpoint sample",
                );
                return effect_kind_to_static_filter(&snapshot.kind, inten);
            }
            return effect_kind_to_static_filter(&eff.kind, eff.intensity.clamp(0.0, 1.0));
        }
    };
    Some(snippet)
}

/// Static fallback path identical to the original `effect_to_filter`
/// implementation in `filtergraph.rs`. Lifted here so the animated and
/// static paths share a single source of truth.
fn effect_kind_to_static_filter(kind: &EffectKind, i: f32) -> Option<String> {
    use EffectKind as K;
    Some(match kind {
        K::Blur { radius } => format!(
            "boxblur=luma_radius={r}:luma_power=1",
            r = (radius * i).max(0.5) as i32
        ),
        K::Sharpen { amount } => {
            format!("unsharp=5:5:{}:5:5:0", (amount * i).clamp(0.0, 3.0))
        }
        K::Grayscale => format!(
            "colorchannelmixer=.299*{i}+1-{i}:.587*{i}:.114*{i}:0:.299*{i}:.587*{i}+1-{i}:.114*{i}:0:.299*{i}:.587*{i}:.114*{i}+1-{i}",
            i = i,
        ),
        K::Sepia => format!(
            "colorchannelmixer={a}:{b}:{c}:0:{d}:{e}:{f}:0:{g}:{h}:{j}:0",
            a = 0.393 * i + (1.0 - i), b = 0.769 * i, c = 0.189 * i,
            d = 0.349 * i, e = 0.686 * i + (1.0 - i), f = 0.168 * i,
            g = 0.272 * i, h = 0.534 * i, j = 0.131 * i + (1.0 - i),
        ),
        K::Invert => format!(
            "lutrgb=r='val+(255-2*val)*{i}':g='val+(255-2*val)*{i}':b='val+(255-2*val)*{i}'",
            i = i,
        ),
        K::HueShift { degrees } => format!("hue=h={}", degrees * i),
        K::Vignette { strength } => format!(
            "vignette=PI/3*{}:mode=forward",
            (strength * i).clamp(0.0, 1.0)
        ),
        K::Pixelate { block_size } => {
            let bs = (block_size * i).max(1.0) as i32;
            format!(
                "scale=iw/{bs}:ih/{bs}:flags=neighbor,scale=iw*{bs}:ih*{bs}:flags=neighbor",
                bs = bs.max(1)
            )
        }
        K::Posterize { levels } => format!(
            "lutrgb=r='floor(val/(255/{l}))*255/({l}-1)':g='floor(val/(255/{l}))*255/({l}-1)':b='floor(val/(255/{l}))*255/({l}-1)'",
            l = (*levels).max(2),
        ),
        K::Glow { radius, intensity } => {
            let r = (radius * i).max(1.0) as i32;
            format!(
                "gblur=sigma={r},eq=brightness={b}",
                r = r,
                b = (intensity * i * 0.15).clamp(0.0, 0.5),
            )
        }
        K::Brightness { amount } => format!("eq=brightness={}", amount * i),
        K::Contrast { amount } => format!("eq=contrast={}", 1.0 + amount * i),
        K::Saturation { amount } => format!("eq=saturation={}", 1.0 + amount * i),
        K::EdgeDetect { threshold: _ } => "edgedetect=mode=colormix".to_string(),
        K::MirrorH => "hflip".to_string(),
        K::MirrorV => "vflip".to_string(),
        K::ChromaticAberration { offset } => {
            let o = (offset * i).round() as i32;
            format!("rgbashift=rh={l}:bh={r}", l = -o, r = o)
        }
        K::Noise { amount } => {
            let strength = (amount * i * 80.0).clamp(0.0, 100.0) as i32;
            format!("noise=alls={}:allf=t", strength)
        }
        K::Wave { amplitude: _, wavelength: _ } => return None,
        K::OldFilm => format!(
            "colorchannelmixer=.393:.769:.189:0:.349:.686:.168:0:.272:.534:.131:0,vignette=PI/3*0.7,noise=alls={}:allf=t",
            (i * 12.0) as i32,
        ),
        K::Vhs => format!(
            "rgbashift=rh=-{o}:bh={o},noise=alls={n}:allf=t",
            o = (4.0 * i).round() as i32,
            n = (i * 8.0) as i32,
        ),
        K::Glitch { strength: _ } => format!(
            "rgbashift=rh=-{o}:bh={o}",
            o = (i * 6.0).round() as i32
        ),
        K::Bloom { radius } => format!("gblur=sigma={}", (radius * i).max(1.0)),
        K::Mask { .. } => return None,
        K::Crop { left, top, right, bottom } => {
            let l = (left * i).clamp(0.0, 0.49);
            let t = (top * i).clamp(0.0, 0.49);
            let r = (right * i).clamp(0.0, 0.49);
            let b = (bottom * i).clamp(0.0, 0.49);
            let w = (1.0 - l - r).max(0.02);
            let h = (1.0 - t - b).max(0.02);
            format!(
                "crop=w=iw*{w:.4}:h=ih*{h:.4}:x=iw*{l:.4}:y=ih*{t:.4},pad=w=iw/{w:.4}:h=ih/{h:.4}:x=iw*{lp:.4}:y=ih*{tp:.4}:color=0x00000000",
                w = w, h = h, l = l, t = t,
                lp = l / (1.0 - l - r).max(0.001),
                tp = t / (1.0 - t - b).max(0.001),
            )
        }
        K::ColorKey { color, similarity, blend, spill: _, invert } => {
            let key_hex = format!(
                "0x{:02X}{:02X}{:02X}",
                color[0], color[1], color[2],
            );
            let sim = (similarity * i).clamp(0.0, 1.0);
            let blend = blend.clamp(0.0, 1.0);
            if *invert {
                format!("chromahold={}:{}:{}", key_hex, sim, blend)
            } else {
                format!("chromakey={}:{}:{}", key_hex, sim, blend)
            }
        }
    })
}

// ─── COLOUR CORRECTION ──────────────────────────────────────────────

/// Build a list of ffmpeg filter snippets that approximate the
/// preview's `ColorCorrection` block.
///
/// We honour every parameter the inspector exposes:
///
/// * Brightness / contrast / saturation / temperature → `eq=` and
///   `colorbalance=` (animated CC scalars are sampled at the clip
///   midpoint — the inspector's per-param diamond animates these
///   with `kfs[<param>]`, which `cc.sampled_at(mid)` already honours).
/// * Lift / gamma / gain per-channel (LGG) → a `curves=` filter with
///   nine sample points per channel computing
///   `clip(gain * (x + lift) ^ (1/gamma), 0, 1)`.
/// * Master + per-channel tone curves → a second `curves=` filter
///   chained after LGG so they compose in pixel order. Both filter
///   stages are skipped when their inputs are at identity.
pub(crate) fn color_correction_filters(
    cc: &ColorCorrection,
    t_in: f32,
    t_out: Option<f32>,
) -> Vec<String> {
    let span = t_out.unwrap_or(t_in + 1.0) - t_in;
    let mid = (span * 0.5).max(0.0);
    let cc = cc.sampled_at(mid);
    if cc.is_identity() {
        return Vec::new();
    }
    let mut out = Vec::new();

    // ── eq (brightness / contrast / saturation) ─────────────────────
    let mut eq_parts: Vec<String> = Vec::new();
    if cc.brightness.abs() > 1e-4 {
        eq_parts.push(format!(
            "brightness={:.4}",
            cc.brightness.clamp(-1.0, 1.0),
        ));
    }
    if (cc.contrast - 1.0).abs() > 1e-4 {
        eq_parts.push(format!("contrast={:.4}", cc.contrast.max(0.0)));
    }
    if (cc.saturation - 1.0).abs() > 1e-4 {
        eq_parts.push(format!("saturation={:.4}", cc.saturation.max(0.0)));
    }
    if !eq_parts.is_empty() {
        out.push(format!("eq={}", eq_parts.join(":")));
    }

    // ── colorbalance (temperature) ──────────────────────────────────
    if cc.temperature.abs() > 1e-4 {
        // Temperature warm = +red / −blue, magnitude scaled to keep
        // the slider's [-1, 1] feel consistent with the preview's
        // 30-pixel R/B shift in `apply_preview_effects`. 0.5× was a
        // good visual match in side-by-side thumbnails.
        let t = cc.temperature.clamp(-1.0, 1.0) * 0.5;
        out.push(format!(
            "colorbalance=rs={:.4}:bs={:.4}",
            t, -t,
        ));
    }

    // ── LGG (lift / gamma / gain) per channel ───────────────────────
    let lgg_neutral = cc.lift.iter().all(|v| v.abs() < 1e-4)
        && cc.gamma.iter().all(|v| (v - 1.0).abs() < 1e-4)
        && cc.gain.iter().all(|v| (v - 1.0).abs() < 1e-4);
    if !lgg_neutral {
        out.push(format!(
            "curves=red='{}':green='{}':blue='{}'",
            sample_lgg_curve(cc.lift[0], cc.gamma[0], cc.gain[0]),
            sample_lgg_curve(cc.lift[1], cc.gamma[1], cc.gain[1]),
            sample_lgg_curve(cc.lift[2], cc.gamma[2], cc.gain[2]),
        ));
    }

    // ── Master + RGB tone curves ────────────────────────────────────
    if !cc.curves.is_identity() {
        let parts: Vec<String> = [
            ("master", &cc.curves.master),
            ("red", &cc.curves.red),
            ("green", &cc.curves.green),
            ("blue", &cc.curves.blue),
        ]
        .into_iter()
        .filter(|(_, c)| !is_identity_curve_v(c))
        .map(|(name, c)| format!("{}='{}'", name, format_curve_pts(c)))
        .collect();
        if !parts.is_empty() {
            out.push(format!("curves={}", parts.join(":")));
        }
    }

    out
}

/// Sample the `output = clip(gain * (input + lift) ^ (1/gamma), 0, 1)`
/// LGG curve at nine evenly-spaced inputs and format as ffmpeg
/// `curves=` control points. ffmpeg's `curves` filter interpolates
/// between control points with cubic splines, so nine samples are
/// enough to track typical LGG slopes without visible banding.
fn sample_lgg_curve(lift: f32, gamma: f32, gain: f32) -> String {
    let g_inv = 1.0 / gamma.max(1e-3);
    let pts: Vec<String> = (0..=8)
        .map(|i| {
            let x = i as f32 / 8.0;
            // Avoid pow(negative, fractional) which would NaN for
            // sufficiently negative `(x + lift)`.
            let base = (x + lift).max(0.0);
            let y = (gain * base.powf(g_inv)).clamp(0.0, 1.0);
            format!("{:.4}/{:.4}", x, y)
        })
        .collect();
    pts.join(" ")
}

/// Format a tone-curve control-point list for the ffmpeg `curves`
/// filter. The filter expects whitespace-separated `x/y` pairs.
fn format_curve_pts(pts: &[[f32; 2]]) -> String {
    pts.iter()
        .map(|p| format!("{:.4}/{:.4}", p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Module-private mirror of `ToneCurves::is_identity_curve` so the
/// renderer can short-circuit per-channel curves without depending on
/// the private helper in core. Kept in sync with `scene.rs`.
fn is_identity_curve_v(c: &[[f32; 2]]) -> bool {
    c.len() == 2
        && (c[0][0] - 0.0).abs() < 1e-4
        && (c[0][1] - 0.0).abs() < 1e-4
        && (c[1][0] - 1.0).abs() < 1e-4
        && (c[1][1] - 1.0).abs() < 1e-4
}

// ─── FFPROBE: SOURCE DIMENSIONS ─────────────────────────────────────

/// Cache for `ffprobe`-resolved source dimensions. Sharing one
/// instance per `FilterGraphBuilder` keeps the probe at most once per
/// distinct input file even when several skeleton attachments hang
/// off the same actor.
#[derive(Default)]
pub(crate) struct DimensionCache {
    cache: HashMap<PathBuf, Option<(u32, u32)>>,
}

impl DimensionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Probe the video dimensions of `path`, memoising the result.
    /// Returns `None` if ffprobe isn't available or the file can't be
    /// inspected — callers fall back to the default `(1080, 1920)`
    /// the preview also uses when no frame cache is ready.
    pub fn probe(&mut self, path: &Path) -> Option<(u32, u32)> {
        if let Some(hit) = self.cache.get(path) {
            return *hit;
        }
        let resolved = probe_video_dimensions(path);
        self.cache.insert(path.to_path_buf(), resolved);
        resolved
    }
}

/// Run `ffprobe` against `path` and parse the first video stream's
/// `width` / `height`. Internal — most callers should go through
/// [`DimensionCache::probe`] for memoisation.
fn probe_video_dimensions(path: &Path) -> Option<(u32, u32)> {
    let ffmpeg = crate::runner::ffmpeg_binary();
    let mut ffprobe = ffmpeg.clone();
    ffprobe.set_file_name("ffprobe");
    if !ffprobe.exists() {
        ffprobe = std::path::PathBuf::from("ffprobe");
    }
    let mut cmd = std::process::Command::new(&ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "csv=s=x:p=0",
    ])
    .arg(path);
    let out = crate::proc::hide_console_std(&mut cmd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.trim();
    let (w, h) = line.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Default fallback dimensions matching the preview's
/// `frame_caches[idx]` default when the cache hasn't loaded yet.
pub(crate) const DEFAULT_HOST_DIMS: (f32, f32) = (1080.0, 1920.0);
