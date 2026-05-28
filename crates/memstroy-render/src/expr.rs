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
//! `eq=`, `colorbalance=` and the modifier (Wobble / Shake / Pulse /
//! Spin / Walk) overlays. Building blocks are intentionally compact
//! so the resulting filter_complex graphs stay within ffmpeg's
//! expression size limits even with dozens of keyframes per element.

use memstroy_core::keyframe::{ModifierKind, TrackModifier};
use memstroy_core::skeleton::{SkeletonAttachment, SkeletonTemplate};
use memstroy_core::{
    Actor, ActorState, CanvasTransform, ColorCorrection, Easing, Effect, EffectKind, Keyframe,
    Overlay, OverlayState, RenderFrameState, Scene,
};

// ─── PIECEWISE EXPRESSIONS ──────────────────────────────────────────

/// Build a piecewise ffmpeg expression for a scalar function of `t`.
///
/// Each segment uses the easing of the keyframe being interpolated
/// **into** (matching `keyframe::sample` exactly): the previous value
/// is held until that keyframe's `t`, and the curve is applied to the
/// normalised local time `(t - a.t) / (b.t - a.t)`. Output shape:
///
/// ```text
/// if(lt(t, t1), <segment 0..1>, if(lt(t, t2), <segment 1..2>, ... <last value>))
/// ```
pub(crate) fn piecewise<T, F>(kfs: &[Keyframe<T>], getter: F) -> String
where
    F: Fn(&T) -> f32,
{
    piecewise_in_var(kfs, getter, "t")
}

/// Variant of [`piecewise`] that lets the caller pick the time
/// variable name embedded in the expression. The default `t` is what
/// `overlay=`, `rotate=`, `scale=eval=frame` and friends understand;
/// `T` is required when the expression will be evaluated inside the
/// `geq=` filter (geq's expression vocabulary only exposes the
/// uppercase `T` for "frame time in seconds"). All other behaviour —
/// easing, segment ordering, last-value clamp — is identical.
///
/// `var` may be any ffmpeg-side expression that evaluates to seconds,
/// not just a single identifier — e.g. `(t-2.5)` to sample a
/// clip-local layout against the scene-time `t`. The wrapper
/// substitutes `var` literally everywhere the time variable appears,
/// so the caller can mix scene-time and clip-local samples in the
/// same filter graph by passing different `var` strings.
pub(crate) fn piecewise_in_var<T, F>(kfs: &[Keyframe<T>], getter: F, var: &str) -> String
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
        // u = clip((var - a.t)/span, 0, 1) — guards numerical drift
        // at the upper edge so the last segment doesn't overshoot.
        let u = format!("(({var}-{:.6})/{:.6})", a.t, span, var = var);
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
        expr = format!(
            "if(lt({var},{:.6}),{},{})",
            b.t,
            segment,
            expr,
            var = var,
        );
    }
    expr
}

/// Time base for the keyframe layout passed to
/// [`build_element_transform`].
///
/// This drives the time variable used inside `piecewise()` expressions
/// for the legacy layout — it does NOT affect modifiers (which always
/// use clip-local time) or the render-frame layout (always scene-time).
///
/// * [`LayoutTimeBase::Scene`] — keyframe `t` values are absolute
///   scene-time. Used for `Actor.layout`, where the canvas preview
///   calls `keyframe::sample(&actor.layout, t)` with the scene
///   playhead `t` directly.
/// * [`LayoutTimeBase::ClipLocal { t_in }`] — keyframe `t` values are
///   clip-local seconds (`t - t_in`). Used for every `Overlay::*`
///   layout, where the canvas preview computes `sample_t = t - t_in`
///   before calling `keyframe::sample`. The renderer substitutes the
///   ffmpeg time variable `t` with `(t - t_in)` so the resulting
///   expression evaluates the layout at the right offset.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LayoutTimeBase {
    Scene,
    ClipLocal { t_in: f32 },
}

impl LayoutTimeBase {
    /// Build the ffmpeg time-variable expression callers feed into
    /// `piecewise_in_var`. Returns a bare `t` for scene-time and
    /// `(t-{t_in})` for clip-local layouts.
    fn xy_var(&self) -> String {
        match self {
            LayoutTimeBase::Scene => "t".to_string(),
            LayoutTimeBase::ClipLocal { t_in } => format!("(t-{:.6})", t_in),
        }
    }

    /// Same as [`Self::xy_var`] but for the `geq=` filter, which only
    /// understands uppercase `T` as the frame-time variable.
    fn geq_var(&self) -> String {
        match self {
            LayoutTimeBase::Scene => "T".to_string(),
            LayoutTimeBase::ClipLocal { t_in } => format!("(T-{:.6})", t_in),
        }
    }
}

/// Build a piecewise expression for a legacy-layout scalar against a
/// configurable [`LayoutTimeBase`]. Convenience wrapper around
/// [`piecewise_in_var`] that picks the right `var` string.
fn piecewise_layout<T, F>(kfs: &[Keyframe<T>], getter: F, base: LayoutTimeBase) -> String
where
    F: Fn(&T) -> f32,
{
    piecewise_in_var(kfs, getter, &base.xy_var())
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
    /// alpha multiplication can be skipped. Always usable as a
    /// constant for the fast `colorchannelmixer=aa=` path.
    pub opacity_static: f32,
    /// Whether the layout's opacity actually moves between keyframes.
    /// When `true` the renderer must use `opacity_expr_t` (per-frame
    /// alpha via `geq`) instead of the static fast path so the
    /// rendered MP4 matches the canvas preview's per-frame alpha
    /// sample at every scene-time, not just the midpoint.
    pub opacity_animated: bool,
    /// Piecewise opacity expression in the **uppercase `T`** time
    /// variable (frame time in seconds). `T` is the only time
    /// variable exposed by the `geq=` filter, which is the path the
    /// renderer takes when `opacity_animated` is `true`. Always
    /// available — for a single-keyframe layout this is just the
    /// constant value, identical to `opacity_static`.
    pub opacity_expr_t: String,
    /// Static flips sampled at the midpoint keyframe — the predominant
    /// flip state shown in the preview.
    pub hflip: bool,
    pub vflip: bool,
}


struct ParentTransformExpr {
    x: String,
    y: String,
    rot_deg: String,
    scale_x: String,
    scale_y: String,
}

fn expr_add(a: String, b: String) -> String {
    if b == "0" { a } else if a == "0" { b } else { format!("(({})+({}))", a, b) }
}

fn expr_mul(a: String, b: String) -> String {
    if a == "1" { b } else if b == "1" { a } else { format!("(({})*({}))", a, b) }
}

fn expr_scale_add(base: String, delta: String) -> String {
    if delta == "0" { base } else { format!("(({})+({}))", base, delta) }
}

fn canvas_transform_expr(scene: &Scene, element_id: &str) -> Option<(String, String, String, String)> {
    scene.canvas_layouts.iter().find(|cl| cl.element_id == element_id).map(|cl| {
        (
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.x),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.y),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.rotation_deg),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.scale),
        )
    })
}

fn apply_parent_expr(x: String, y: String, parent: &ParentTransformExpr) -> (String, String) {
    let sx = expr_mul(x, parent.scale_x.clone());
    let sy = expr_mul(y, parent.scale_y.clone());
    let theta = format!("(({})*PI/180)", parent.rot_deg);
    let cos_t = format!("cos({})", theta);
    let sin_t = format!("sin({})", theta);
    let wx = format!("(({})+(({})*({})-({})*({})))", parent.x, sx, cos_t, sy, sin_t);
    let wy = format!("(({})+(({})*({})+({})*({})))", parent.y, sx, sin_t, sy, cos_t);
    (wx, wy)
}

fn parent_transform_expr(scene: &Scene, parent_id: &str, visited: &mut Vec<String>) -> Option<ParentTransformExpr> {
    if visited.iter().any(|id| id == parent_id) {
        return None;
    }
    visited.push(parent_id.to_string());

    if parent_id == "__render_frame__" {
        let rf = &scene.render_frame;
        let mods = build_modifier_expr(&rf.modifiers, 0.0);
        let x = expr_add(piecewise(&rf.layout, |s: &RenderFrameState| s.pos.x), mods.dx);
        let y = expr_add(piecewise(&rf.layout, |s: &RenderFrameState| s.pos.y), mods.dy);
        let rot_deg = expr_add(piecewise(&rf.layout, |s: &RenderFrameState| s.rotation_deg), mods.drot_deg);
        return Some(ParentTransformExpr { x, y, rot_deg, scale_x: "1".into(), scale_y: "1".into() });
    }

    let [out_w, out_h] = scene.output.resolution;
    let mut out = if let Some(actor) = scene.actors.iter().find(|a| a.id == parent_id) {
        let mods = build_modifier_expr(&actor.modifiers, actor.t_in.unwrap_or(0.0));
        let (x, y, rot_deg, scale_x) = if let Some((cx, cy, crot, cscale)) = canvas_transform_expr(scene, &actor.id) {
            (cx, cy, crot, cscale)
        } else {
            (
                format!("(({px})*{w:.4})", px = piecewise(&actor.layout, |s: &ActorState| s.pos[0]), w = out_w as f32),
                format!("(({py})*{h:.4})", py = piecewise(&actor.layout, |s: &ActorState| s.pos[1]), h = out_h as f32),
                piecewise(&actor.layout, |s: &ActorState| s.rotation_deg),
                piecewise(&actor.layout, |s: &ActorState| s.scale),
            )
        };
        let x = expr_add(x, mods.dx);
        let y = expr_add(y, mods.dy);
        let rot_deg = expr_add(rot_deg, mods.drot_deg);
        let scale_x = expr_scale_add(scale_x, mods.dscale);
        let scale_y = expr_mul(scale_x.clone(), piecewise(&actor.layout, |s: &ActorState| s.scale_y));
        ParentTransformExpr { x, y, rot_deg, scale_x, scale_y }
    } else if let Some(ov) = scene.overlays.iter().find(|ov| match ov {
        Overlay::Text(o) => o.id == parent_id,
        Overlay::Image(o) => o.id == parent_id,
        Overlay::Video(o) => o.id == parent_id,
    }) {
        let (id, t_in, layout, modifiers) = match ov {
            Overlay::Text(o) => (&o.id, o.t_in, &o.layout, &o.modifiers),
            Overlay::Image(o) => (&o.id, o.t_in, &o.layout, &o.modifiers),
            Overlay::Video(o) => (&o.id, o.t_in, &o.layout, &o.modifiers),
        };
        let base = LayoutTimeBase::ClipLocal { t_in };
        let mods = build_modifier_expr(modifiers, t_in);
        let (x, y, rot_deg, scale_x) = if let Some((cx, cy, crot, cscale)) = canvas_transform_expr(scene, id) {
            (cx, cy, crot, cscale)
        } else {
            (
                format!("(({px})*{w:.4})", px = piecewise_layout(layout, |s: &OverlayState| s.pos[0], base), w = out_w as f32),
                format!("(({py})*{h:.4})", py = piecewise_layout(layout, |s: &OverlayState| s.pos[1], base), h = out_h as f32),
                piecewise_layout(layout, |s: &OverlayState| s.rotation_deg, base),
                piecewise_layout(layout, |s: &OverlayState| s.scale, base),
            )
        };
        let x = expr_add(x, mods.dx);
        let y = expr_add(y, mods.dy);
        let rot_deg = expr_add(rot_deg, mods.drot_deg);
        let scale_x = expr_scale_add(scale_x, mods.dscale);
        let scale_y = expr_mul(scale_x.clone(), piecewise_layout(layout, |s: &OverlayState| s.scale_y, base));
        ParentTransformExpr { x, y, rot_deg, scale_x, scale_y }
    } else {
        return None;
    };

    if let Some(grandparent_id) = scene.element_parent_id(parent_id) {
        if let Some(parent) = parent_transform_expr(scene, grandparent_id, visited) {
            let (x, y) = apply_parent_expr(out.x, out.y, &parent);
            out.x = x;
            out.y = y;
            out.rot_deg = expr_add(out.rot_deg, parent.rot_deg);
            out.scale_x = expr_mul(out.scale_x, parent.scale_x);
            out.scale_y = expr_mul(out.scale_y, parent.scale_y);
        }
    }
    Some(out)
}

/// Build the full overlay-time transform for an element identified by
/// `element_id`. This is the renderer's equivalent of
/// `canvas_preview::get_element_world_pos` + the legacy keyframe
/// sample + modifier eval — but expressed as ffmpeg expressions of `t`.
///
/// Position priority matches the preview:
///   1. `skeleton_attachment` (when bound, the element's world centre
///      snaps to a skeleton point on a host actor). Mirrors
///      `canvas_preview::resolve_overlay_attachment_world` and the
///      `Actor.skeleton_attachments[0]` override path.
///   2. `Scene.canvas_layouts[element_id]` (Free Canvas v2 world px).
///   3. The legacy normalised `[0,1]` `layout`, converted to world
///      space relative to the moving render frame (so animating
///      `render_frame.pos` shifts elements like the preview).
///
/// Scale / rotation / opacity / flips always come from `legacy_layout`
/// (the canvas_layouts override only carries position, mirroring the
/// preview where `get_element_world_pos` returns *only* `WorldPos`).
///
/// `layout_time_base` controls how the layout's keyframe times are
/// interpreted. Actors store keyframes in scene-time and use
/// [`LayoutTimeBase::Scene`]; overlays store keyframes in clip-local
/// time and use [`LayoutTimeBase::ClipLocal { t_in }`]. Without this
/// distinction every overlay animation in a clip whose `t_in > 0`
/// rendered out-of-sync with the canvas preview (the preview samples
/// at `t - t_in`, the renderer at `t`).
///
/// Parent hierarchy is resolved recursively, matching Unity-style
/// local-to-world composition: child position is stored in parent-local
/// space, then parent position/rotation/non-uniform scale are applied.
pub(crate) fn build_element_transform<S>(
    scene: &Scene,
    element_id: &str,
    legacy_layout: &[Keyframe<S>],
    modifiers: &[TrackModifier],
    t_in: f32,
    skeleton_attachment: Option<&SkeletonAttachment>,
    layout_time_base: LayoutTimeBase,
) -> ElementTransform
where
    S: PositionedState + Clone,
{
    let [out_w, out_h] = scene.output.resolution;
    let half_w = out_w as f32 * 0.5;
    let half_h = out_h as f32 * 0.5;

    // ── Render-frame expressions ─────────────────────────────────────
    //
    // The render frame is a moveable / scalable / rotatable rectangle
    // on the world canvas; its contents become the rendered output.
    // To match the canvas preview and the `frame_snapshot` rasterizer
    // (the gold reference: see `frame_snapshot::world_to_output`) we
    // bake the FULL render-frame camera transform into every
    // element's overlay placement here:
    //
    //   1. translate the element by `-rf.pos` so the rf centre lands
    //      at world origin,
    //   2. rotate the result by `-rf.rotation_deg` so the rf becomes
    //      axis-aligned with the output,
    //   3. multiply by `rf.zoom` so the rf rectangle covers the
    //      output's `W × H`,
    //   4. translate by `(W/2, H/2)` so the rf centre is the output
    //      centre.
    //
    // The same `rf.zoom` factor is applied to every element's scale
    // (so the elements shrink/grow when the user zooms the rf), and
    // `-rf.rotation_deg` is added to every element's rotation (so the
    // elements counter-rotate to land axis-aligned in the output).
    //
    // Doing this per-element makes the post-composite camera pass
    // (the old `emit_render_frame_camera`) redundant — and that's
    // important because the post-composite pass operated on a fixed
    // `W × H` buffer that simply doesn't contain world content for
    // `rf.zoom < 1` or `rf.rotation_deg != 0`. The previous behaviour
    // produced transparent corners on rotated rfs and clamped zoomed-
    // out rfs to the centre crop — i.e. the user-visible bug "render
    // doesn't match the canvas selection area at all".
    let rf = &scene.render_frame;
    let rf_pos_x_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.pos.x);
    let rf_pos_y_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.pos.y);
    let rf_zoom_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.zoom);
    let rf_rot_deg_kf = piecewise(&rf.layout, |s: &RenderFrameState| s.rotation_deg);
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
    // RF rotation modifier (Spin / Wobble / Walk drot) layered on top
    // of the eased keyframe sample, in degrees — same convention as
    // `frame_snapshot::sample_render_frame_eased`.
    let rf_rot_deg_eased = if rf_mods.drot_is_zero() {
        rf_rot_deg_kf.clone()
    } else {
        format!("(({})+({}))", rf_rot_deg_kf, rf_mods.drot_deg)
    };
    // RF zoom modifier (Pulse): the snapshot computes
    //     zoom_eased = zoom / max(1e-3, 1 + d_scale)
    // so a positive `d_scale` zooms OUT (covers more world area). We
    // mirror that exactly.
    let rf_zoom_eased = if rf_mods.dscale_is_zero() {
        rf_zoom_kf.clone()
    } else {
        format!(
            "(({})/(max(0.001,1+({}))))",
            rf_zoom_kf, rf_mods.dscale,
        )
    };

    // Detect whether the rf camera is the static identity transform
    // (default RenderFrame, no animation, no modifiers). The fast
    // path skips the cos/sin terms and the zoom multiplier so the
    // typical scene's filter-graph stays compact.
    let rf_zoom_is_one = rf
        .layout
        .iter()
        .all(|kf| (kf.value.zoom - 1.0).abs() < 1.0e-4)
        && rf_mods.dscale_is_zero();
    let rf_rot_is_zero = rf
        .layout
        .iter()
        .all(|kf| kf.value.rotation_deg.abs() < 1.0e-3)
        && rf_mods.drot_is_zero();

    // ── World position priority: skeleton → canvas_layouts → legacy ─
    let canvas_layout = scene
        .canvas_layouts
        .iter()
        .find(|cl| cl.element_id == element_id);
    let skeleton_world = skeleton_attachment
        .and_then(|att| resolve_skeleton_attachment_world(scene, att));
    let (world_x, world_y) = if let Some((sx, sy)) = skeleton_world {
        // Skeleton override fully replaces the world position — same
        // semantics as `canvas_preview::resolve_overlay_attachment_world`,
        // which returns the projected point directly without consulting
        // canvas_layouts or the legacy layout. Modifiers (Wobble / Shake
        // / Walk) still add on top below.
        (sx, sy)
    } else if let Some(cl) = canvas_layout {
        (
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.x),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.y),
        )
    } else {
        // ── Legacy normalised pos → world pixels (decoupled from rf) ──
        //
        // From scene format_version 2 onwards (see
        // `Scene::migrate_decouple_render_frame` in
        // memstroy-core/scene.rs) the legacy `[0..1]` normalised pos
        // is interpreted against a FIXED reference rectangle of size
        // `output.resolution` anchored at world (0, 0). The render
        // frame is now a pure camera viewport — its `pos` / `zoom` /
        // `rotation` drive the composite centring and the camera pass
        // below, but they no longer feed into the world-pixel layout
        // of any element. Result: editing the render frame in the
        // inspector / on canvas no longer drags every overlay along
        // (the v1 bug "при изменении параметров области рендера
        // зачем-то некоторые изменения на холсте каким-то другим
        // элементам тоже начинают применяться, например тексту").
        //
        //     world_x(t) = pos_x(t) * out_w
        //     world_y(t) = pos_y(t) * out_h
        let pos_x = piecewise_layout(legacy_layout, |s: &S| s.pos()[0], layout_time_base);
        let pos_y = piecewise_layout(legacy_layout, |s: &S| s.pos()[1], layout_time_base);
        let wx = format!("(({px})*{W:.4})", px = pos_x, W = out_w as f32);
        let wy = format!("(({py})*{H:.4})", py = pos_y, H = out_h as f32);
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

    let parent_expr = scene.element_parent_id(element_id)
        .and_then(|parent_id| parent_transform_expr(scene, parent_id, &mut vec![element_id.to_string()]));
    let (world_x, world_y) = if let Some(parent) = parent_expr.as_ref() {
        apply_parent_expr(world_x, world_y, parent)
    } else {
        (world_x, world_y)
    };

    // ── World → output canvas: full render-frame camera transform ───
    //
    // Mirrors `frame_snapshot::world_to_output` exactly so the export
    // composes elements at the same output coordinates the snapshot
    // (and the canvas preview) puts them at.
    //
    //     dx = world_x - rf.pos.x
    //     dy = world_y - rf.pos.y
    //     theta = -rf.rotation_deg                  (we un-rotate world)
    //     rx = dx * cos(theta) - dy * sin(theta)
    //     ry = dx * sin(theta) + dy * cos(theta)
    //     centre_x = W/2 + rx * rf.zoom
    //     centre_y = H/2 + ry * rf.zoom
    //
    // Plus per-element scale `*= rf.zoom` and per-element rotation
    // `-= rf.rotation_deg` further down, so each element renders into
    // the output as if photographed by the rf camera.
    let dx_expr = format!("(({wx})-({rfx}))", wx = world_x, rfx = rf_pos_x);
    let dy_expr = format!("(({wy})-({rfy}))", wy = world_y, rfy = rf_pos_y);

    // Apply the inverse rotation of the rf so its rectangle becomes
    // axis-aligned with the output. The fast path skips cos/sin when
    // the rf is never rotated.
    let (rx_expr, ry_expr) = if rf_rot_is_zero {
        (dx_expr.clone(), dy_expr.clone())
    } else {
        // theta = -rf_rotation_deg in radians; we expand this once
        // here so the resulting expression's cos/sin appear with
        // their natural ffmpeg-readable form (`cos(-X)` parses fine).
        let theta_rad = format!("((-({}))*PI/180)", rf_rot_deg_eased);
        let cos_t = format!("cos({})", theta_rad);
        let sin_t = format!("sin({})", theta_rad);
        let rx = format!(
            "(({dx})*({cs})-({dy})*({sn}))",
            dx = dx_expr,
            dy = dy_expr,
            cs = cos_t,
            sn = sin_t,
        );
        let ry = format!(
            "(({dx})*({sn})+({dy})*({cs}))",
            dx = dx_expr,
            dy = dy_expr,
            cs = cos_t,
            sn = sin_t,
        );
        (rx, ry)
    };

    // Scale the offset by the rf zoom (so a zoom-out rf shows a
    // larger area of the world inside the same output rectangle, and
    // a zoom-in rf shows a smaller area).
    let (zx_expr, zy_expr) = if rf_zoom_is_one {
        (rx_expr, ry_expr)
    } else {
        (
            format!("(({})*({}))", rx_expr, rf_zoom_eased),
            format!("(({})*({}))", ry_expr, rf_zoom_eased),
        )
    };

    let centre_x_expr = format!("(({rx})+{half_w:.4})", rx = zx_expr, half_w = half_w);
    let centre_y_expr = format!("(({ry})+{half_h:.4})", ry = zy_expr, half_h = half_h);
    let x_expr = format!("({})-w/2", centre_x_expr);
    let y_expr = format!("({})-h/2", centre_y_expr);

    // ── Scale (with Pulse modifier added, scale_y multiplied) ────────
    //
    // The rf camera's zoom factor multiplies every element's scale
    // so the element shrinks/grows in lock-step with the rf
    // (matches `frame_snapshot::paint_actor`'s
    // `out_w = src_w * scale * zoom`).
    let scale_base = piecewise_layout(legacy_layout, |s: &S| s.scale(), layout_time_base);
    let scale_y_factor = piecewise_layout(legacy_layout, |s: &S| s.scale_y(), layout_time_base);
    let sx_base = if mods.dscale_is_zero() {
        scale_base.clone()
    } else {
        format!("(({})+({}))", scale_base, mods.dscale)
    };
    let sy_base = if mods.dscale_is_zero() {
        format!("({})*({})", scale_base, scale_y_factor)
    } else {
        format!("(({})+({}))*({})", scale_base, mods.dscale, scale_y_factor)
    };
    let sx_base = if let Some(parent) = parent_expr.as_ref() {
        expr_mul(sx_base, parent.scale_x.clone())
    } else {
        sx_base
    };
    let sy_base = if let Some(parent) = parent_expr.as_ref() {
        expr_mul(sy_base, parent.scale_y.clone())
    } else {
        sy_base
    };
    let sx_expr = if rf_zoom_is_one {
        sx_base
    } else {
        format!("(({})*({}))", sx_base, rf_zoom_eased)
    };
    let sy_expr = if rf_zoom_is_one {
        sy_base
    } else {
        format!("(({})*({}))", sy_base, rf_zoom_eased)
    };

    // ── Rotation — combine layout + Spin / Wobble / Walk drot ────────
    //
    // Plus `-rf.rotation_deg` so the element counter-rotates with the
    // un-rotated rf, mirroring `frame_snapshot`'s
    // `rotation_rad = (layout.rotation_deg - rf_state.rotation_deg).to_radians()`.
    let rot_deg_layout = piecewise_layout(legacy_layout, |s: &S| s.rotation_deg(), layout_time_base);
    let layout_has_rot = legacy_layout
        .iter()
        .any(|kf| kf.value.rotation_deg().abs() > 0.05);
    let parent_has_rot = parent_expr.as_ref().map(|p| p.rot_deg != "0").unwrap_or(false);
    let rot_expr = if layout_has_rot || !mods.drot_is_zero() || parent_has_rot || !rf_rot_is_zero {
        let elem_rot_deg = if mods.drot_is_zero() {
            rot_deg_layout
        } else {
            format!("(({})+({}))", rot_deg_layout, mods.drot_deg)
        };
        let elem_rot_deg = if let Some(parent) = parent_expr.as_ref() {
            expr_add(elem_rot_deg, parent.rot_deg.clone())
        } else {
            elem_rot_deg
        };
        let total_deg = if rf_rot_is_zero {
            elem_rot_deg
        } else {
            format!("(({})-({}))", elem_rot_deg, rf_rot_deg_eased)
        };
        Some(format!("(({})*PI/180)", total_deg))
    } else {
        None
    };

    // ── Opacity ─────────────────────────────────────────────────────
    //
    // The canvas preview (`canvas_preview::draw_canvas_elements`,
    // `frame_snapshot::paint_*`) samples the layout's `opacity` field
    // **per frame** at the playhead, then bakes it into the egui mesh
    // tint / `paint_layer_rgba` alpha. To make the rendered MP4
    // match, we mirror that exactly:
    //
    //   * `opacity_expr_t` — full piecewise expression in `T` (frame
    //     time, scene-absolute) ready to drop into the `geq=` filter
    //     so each output frame gets its own alpha multiplier.
    //   * `opacity_static` — midpoint sample, kept as a fast-path
    //     constant for clips whose opacity is genuinely static
    //     (single-keyframe layouts and any other layout where every
    //     keyframe carries the same value). This avoids the per-pixel
    //     `geq` for the overwhelmingly common static-opacity case.
    //   * `opacity_animated` — true when the keyframes actually
    //     differ, telling the caller to take the geq path.
    //
    // Note: the expression is sampled at *scene* time `T`, not
    // clip-local time. This matches the preview, where
    // `keyframe::sample(&layout, t)` is called with the scene
    // playhead — the layout keyframes were authored in scene-time
    // when the user dropped them. The renderer's clip-stream
    // alignment (`time_align_filters` in filtergraph.rs) shifts
    // each clip's PTS so `T` inside its filter chain equals scene
    // time, keeping the geq evaluation on the same axis the
    // preview uses.
    let opacity_static = legacy_layout
        .get(legacy_layout.len() / 2)
        .map(|kf| kf.value.opacity().clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let opacity_animated = legacy_layout
        .windows(2)
        .any(|w| (w[0].value.opacity() - w[1].value.opacity()).abs() > 1e-3);
    let opacity_expr_t =
        piecewise_in_var(legacy_layout, |s: &S| s.opacity().clamp(0.0, 1.0), &layout_time_base.geq_var());

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
        opacity_expr_t,
        hflip,
        vflip,
    }
}

// ─── COLOUR CORRECTION ──────────────────────────────────────────────

/// Build a list of ffmpeg filter snippets that mirror the preview's
/// `ColorCorrection` block.
///
/// Pipeline order (matches `video_cache::apply_effects_cpu`):
///   1. brightness / contrast / saturation  (`eq=`)
///   2. temperature                          (`colorbalance=`)
///   3. lift → gain → gamma per RGB channel  (`lutrgb=` with the
///      DaVinci-style formula `pow((in + lift*(1-in)) * gain, 1/gamma)`)
///   4. master tone curve                    (`curves=preset=none:m='…'`)
///   5. per-channel R / G / B tone curves    (separate `curves=` filters)
///
/// Animated CC params are sampled at the clip's midpoint — same
/// flattening rule we use for opacity / flip; full per-frame `eq=`
/// expressions are technically possible (`eval=frame`) but blow up
/// the filtergraph length when several actors animate CC together.
/// Tone curves and per-channel LGG are NOT animated in the model
/// (the inspector exposes no diamonds for them) so the midpoint
/// sample is exact, not an approximation.
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
    if cc.temperature.abs() > 1e-4 {
        // Temperature warm = +red / −blue, magnitude scaled to keep
        // the slider's [-1, 1] feel consistent with the preview's
        // 30-pixel R/B shift (see `apply_preview_effects` in
        // `canvas_preview.rs`). 0.5× felt right in side-by-side
        // matching against the preview thumbnail.
        let t = cc.temperature.clamp(-1.0, 1.0) * 0.5;
        out.push(format!(
            "colorbalance=rs={:.4}:bs={:.4}",
            t, -t,
        ));
    }

    // ── Lift / Gain / Gamma per RGB channel ──────────────────────────
    //
    // The preview's `apply_effects_cpu` runs (in 0..1 space):
    //
    //   shifted = norm + lift * (1 - norm)
    //   scaled  = shifted * gain
    //   final   = pow(max(0, scaled), 1 / gamma)
    //
    // ffmpeg's `lutrgb` filter accepts per-channel expressions over
    // `val` (the 0..255 input). We rewrite the formula in those
    // terms: `255 * pow(max(0, (val/255 + L*(1-val/255))*G), 1/Gma)`.
    // Identity channels (lift=0, gain=1, gamma=1) are skipped so the
    // filter only fires when the user has actually dialled LGG.
    let lift = cc.lift;
    let gain = [
        cc.gain[0].max(0.0),
        cc.gain[1].max(0.0),
        cc.gain[2].max(0.0),
    ];
    let gamma = [
        cc.gamma[0].max(0.05),
        cc.gamma[1].max(0.05),
        cc.gamma[2].max(0.05),
    ];
    let lgg_active = lift.iter().any(|v| v.abs() > 1e-4)
        || gain.iter().any(|v| (v - 1.0).abs() > 1e-4)
        || gamma.iter().any(|v| (v - 1.0).abs() > 1e-4);
    if lgg_active {
        let chan_expr = |l: f32, g: f32, gma: f32| -> String {
            // Per-channel identity → cheap `val` passthrough so we
            // don't pay for a `pow()` per pixel on untouched channels.
            if l.abs() < 1e-4 && (g - 1.0).abs() < 1e-4 && (gma - 1.0).abs() < 1e-4 {
                return "val".to_string();
            }
            let inv_g = 1.0 / gma;
            format!(
                "clip(255*pow(max(0,(val/255+({l:.4})*(1-val/255))*({g:.4})),{inv:.4}),0,255)",
                l = l,
                g = g,
                inv = inv_g,
            )
        };
        let r_expr = chan_expr(lift[0], gain[0], gamma[0]);
        let g_expr = chan_expr(lift[1], gain[1], gamma[1]);
        let b_expr = chan_expr(lift[2], gain[2], gamma[2]);
        out.push(format!(
            "lutrgb=r='{r}':g='{g}':b='{b}'",
            r = r_expr,
            g = g_expr,
            b = b_expr,
        ));
    }

    // ── Tone curves (master + R / G / B) ─────────────────────────────
    //
    // ffmpeg's `curves` filter accepts a string of space-separated
    // `x/y` control points per channel (master is `m=`). We emit one
    // `curves=` filter per non-identity channel so the export
    // composition matches the preview's order: master FIRST, then
    // each per-channel curve, exactly as `apply_effects_cpu` walks
    // the LUTs (`lut_master` then `lut_r` / `lut_g` / `lut_b`).
    if !cc.curves.is_identity() {
        if !is_curve_identity(&cc.curves.master) {
            out.push(format!(
                "curves=preset=none:m='{}'",
                format_curve(&cc.curves.master),
            ));
        }
        if !is_curve_identity(&cc.curves.red) {
            out.push(format!(
                "curves=preset=none:r='{}'",
                format_curve(&cc.curves.red),
            ));
        }
        if !is_curve_identity(&cc.curves.green) {
            out.push(format!(
                "curves=preset=none:g='{}'",
                format_curve(&cc.curves.green),
            ));
        }
        if !is_curve_identity(&cc.curves.blue) {
            out.push(format!(
                "curves=preset=none:b='{}'",
                format_curve(&cc.curves.blue),
            ));
        }
    }
    out
}


/// Whether a tone curve is the identity `(0,0)–(1,1)` line. Mirrors
/// the private check in `memstroy_core::scene` so we can skip
/// emitting a `curves=` filter for unchanged channels.
fn is_curve_identity(c: &[[f32; 2]]) -> bool {
    c.len() == 2
        && (c[0][0] - 0.0).abs() < 1e-4
        && (c[0][1] - 0.0).abs() < 1e-4
        && (c[1][0] - 1.0).abs() < 1e-4
        && (c[1][1] - 1.0).abs() < 1e-4
}

/// Format a tone curve's control points as a space-separated `x/y`
/// list for ffmpeg's `curves=` filter. Points are sorted by `x` and
/// clamped to `[0, 1]` so out-of-range authoring data doesn't
/// generate filter-parse errors. The endpoints are kept verbatim
/// (the model invariant guarantees `(0,0)` and `(1,1)` are present
/// on the master / per-channel arrays whenever the user added an
/// intermediate point).
fn format_curve(curve: &[[f32; 2]]) -> String {
    let mut pts: Vec<[f32; 2]> = curve
        .iter()
        .map(|p| [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)])
        .collect();
    pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
    pts.iter()
        .map(|p| format!("{:.4}/{:.4}", p[0], p[1]))
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── SKELETON ATTACHMENTS ───────────────────────────────────────────

/// Resolve a `SkeletonAttachment` to a pair of piecewise ffmpeg
/// expressions for its world-pixel x / y position.
///
/// Mirrors `canvas_preview::resolve_overlay_attachment_world`: we find
/// the matching template, locate the host actor that owns the source
/// clip the skeleton was authored on, project the (animated) point's
/// normalised coordinates through the host actor's bbox, and rotate
/// the local offset by the host's per-frame rotation. Returns `None`
/// when the template / point / host can't be resolved (in which case
/// the renderer falls back to canvas_layouts / legacy positioning).
///
/// Source-clip dimensions are not probed — we use the same
/// `(1080, 1920)` fallback the preview uses when the frame cache
/// isn't ready, which keeps the export and the canvas in lock-step
/// for the typical portrait clip aspect ratio. A future revision
/// can ffprobe the clip dimensions if non-portrait sources show
/// drift.
pub(crate) fn resolve_skeleton_attachment_world(
    scene: &Scene,
    attachment: &SkeletonAttachment,
) -> Option<(String, String)> {
    let template = find_skeleton_template(scene, attachment)?;
    let point = template.points.get(&attachment.point_name)?;
    if point.track.is_empty() {
        return None;
    }
    let host = find_host_actor(scene, template)?;
    if !host.visible {
        return None;
    }

    // Host actor's animated bbox dimensions (in source pixels).
    let host_state_scale = piecewise(&host.layout, |s: &ActorState| s.scale);
    let host_state_scale_y = piecewise(&host.layout, |s: &ActorState| s.scale_y);
    let host_rot_deg = piecewise(&host.layout, |s: &ActorState| s.rotation_deg);

    // Source dimensions: fall back to the preview's portrait default.
    // ffprobe-driven probing is left for a follow-up — most user
    // sources are vertical 1080×1920, matching the output canvas.
    let (src_w, src_h) = (1080.0_f32, 1920.0_f32);

    // Normalised point coords with the attachment offset baked in.
    let nx_kf = piecewise(&point.track, |p: &memstroy_core::skeleton::PointState| p.x);
    let ny_kf = piecewise(&point.track, |p: &memstroy_core::skeleton::PointState| p.y);
    let nx = format!("(({nx})+({off:.6})-0.5)", nx = nx_kf, off = attachment.offset[0]);
    let ny = format!("(({ny})+({off:.6})-0.5)", ny = ny_kf, off = attachment.offset[1]);

    // Local-space pixel offset from the host's centre.
    let local_x = format!(
        "(({nx})*{src_w:.4}*({sc}))",
        nx = nx,
        src_w = src_w,
        sc = host_state_scale,
    );
    let local_y = format!(
        "(({ny})*{src_h:.4}*({sc})*({scy}))",
        ny = ny,
        src_h = src_h,
        sc = host_state_scale,
        scy = host_state_scale_y,
    );

    // Rotate the local offset by the host's per-frame rotation so the
    // attachment follows when the host actor spins (preview parity).
    let rot_rad = format!("(({})*PI/180)", host_rot_deg);
    let cs = format!("cos({})", rot_rad);
    let sn = format!("sin({})", rot_rad);
    let rotated_x = format!(
        "(({lx})*({cs})-({ly})*({sn}))",
        lx = local_x,
        ly = local_y,
        cs = cs,
        sn = sn,
    );
    let rotated_y = format!(
        "(({lx})*({sn})+({ly})*({cs}))",
        lx = local_x,
        ly = local_y,
        cs = cs,
        sn = sn,
    );

    // Host's own world centre (skeleton attachments on the host
    // itself are not chained — that would be a self-reference; the
    // host's world pos always comes from canvas_layouts / legacy).
    let host_world = host_world_pos(scene, host);

    let world_x = format!("(({hx})+({rx}))", hx = host_world.0, rx = rotated_x);
    let world_y = format!("(({hy})+({ry}))", hy = host_world.1, ry = rotated_y);
    Some((world_x, world_y))
}

/// Find the skeleton template that an attachment refers to. Templates
/// can be referenced by `name` (the pretty label the user typed) OR
/// by the source clip's filename without extension — the same
/// resolution rule the preview's `resolve_overlay_attachment_world`
/// uses, so the export and the canvas always agree on which template
/// is bound.
fn find_skeleton_template<'a>(
    scene: &'a Scene,
    attachment: &SkeletonAttachment,
) -> Option<&'a SkeletonTemplate> {
    scene.skeleton_templates.iter().find(|tmpl| {
        tmpl.name == attachment.skeleton_id
            || tmpl
                .source_clip
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == attachment.skeleton_id)
                .unwrap_or(false)
    })
}

/// Find the actor that hosts a skeleton template (i.e. whose source
/// clip is the one the skeleton was authored on). Matches the
/// preview's "first actor whose source matches the template's
/// source_clip" rule — by full path AND by filename, the latter
/// guarding against project moves where the absolute path differs
/// but the asset lives under a new `assets_root`.
fn find_host_actor<'a>(scene: &'a Scene, template: &SkeletonTemplate) -> Option<&'a Actor> {
    scene.actors.iter().find(|a| {
        a.source == template.source_clip
            || a.source.file_name() == template.source_clip.file_name()
    })
}

/// Build the host actor's own world-centre expressions WITHOUT
/// recursing into skeleton attachments (that would be circular). We
/// reproduce the canvas_layouts → legacy fallback ladder of
/// `build_element_transform` here because skeleton attachments are
/// the only callers that need the host's world pos in expression
/// form, and inlining keeps the public API single-pass.
fn host_world_pos(scene: &Scene, host: &Actor) -> (String, String) {
    let [out_w, out_h] = scene.output.resolution;

    if let Some(cl) = scene.canvas_layouts.iter().find(|cl| cl.element_id == host.id) {
        return (
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.x),
            piecewise(&cl.keyframes, |t: &CanvasTransform| t.pos.y),
        );
    }

    // ── Legacy normalised pos → world pixels (decoupled from rf) ──
    //
    // Mirrors the v2 contract in `build_element_transform`: the host
    // actor's `[0..1]` normalised `pos` is interpreted against a FIXED
    // reference rectangle of size `output.resolution` — the live render
    // frame no longer feeds into world coordinates. See
    // `Scene::migrate_decouple_render_frame` for the full rationale.
    let pos_x = piecewise(&host.layout, |s: &ActorState| s.pos[0]);
    let pos_y = piecewise(&host.layout, |s: &ActorState| s.pos[1]);
    let wx = format!("(({px})*{W:.4})", px = pos_x, W = out_w as f32);
    let wy = format!("(({py})*{H:.4})", py = pos_y, H = out_h as f32);
    (wx, wy)
}

// ─── EFFECT PARAMETER ANIMATION ─────────────────────────────────────

/// Build the ffmpeg filter chain for a single `Effect`, honouring
/// per-parameter keyframe animation (`Effect.param_kfs` +
/// `animated_params`).
///
/// When the effect has no animated parameters we fall back to the
/// caller-supplied static `static_snippet` so the historical fast
/// path stays untouched. When at least one parameter animates we
/// slice the clip's lifetime into segments at the union of the
/// animated parameters' keyframe times (capped at `MAX_SEGMENTS`),
/// re-sample the effect at each segment's midpoint via
/// `Effect::sampled_at`, generate the static filter snippet for
/// that sample, and gate it with a per-segment
/// `:enable='between(t,a,b)'` clause. Inactive segments pass through
/// transparently — exactly what the user sees in the preview where
/// each frame is rendered with the effect sampled at *that* frame's
/// time.
///
/// The chain is emitted as a comma-joined string suitable for
/// inserting into a single filter-chain (the same shape the existing
/// caller expects from `effect_to_filter`). For multi-filter effects
/// (e.g. `Pixelate` is `scale=…,scale=…`) each component receives
/// its own `enable=` clause so muting works on the whole effect at
/// once.
///
/// `t_in` / `t_out` are the host clip's visible window. `to_filter`
/// must reproduce the snippet `effect_to_filter` would emit for a
/// statically-resolved effect (see `effect_to_filter` in
/// `filtergraph.rs`); we accept it as a closure to keep this helper
/// in `expr.rs` without pulling the per-kind translation rules in.
pub(crate) fn effect_animated_filter_chain<F>(
    eff: &Effect,
    t_in: f32,
    t_out: f32,
    to_filter: F,
) -> Option<String>
where
    F: Fn(&EffectKind, f32) -> Option<String>,
{
    if !eff.enabled {
        return None;
    }
    if !effect_has_animation(eff) {
        let i = eff.intensity.clamp(0.0, 1.0);
        if i <= 0.001 {
            return None;
        }
        return to_filter(&eff.kind, i);
    }

    let segments = effect_segments(eff, t_in, t_out);
    if segments.is_empty() {
        let i = eff.intensity.clamp(0.0, 1.0);
        if i <= 0.001 {
            return None;
        }
        return to_filter(&eff.kind, i);
    }

    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for (a, b) in &segments {
        let mid = ((a + b) * 0.5).max(*a);
        let local = (mid - t_in).max(0.0);
        let sampled = eff.sampled_at(local);
        let i = sampled.intensity.clamp(0.0, 1.0);
        if i <= 0.001 {
            // Muted segment — emit a `null` no-op gated by the same
            // enable so the chain length stays predictable. (We can't
            // simply skip: the chain depends on the segment count
            // matching across consumers that introspect the graph.)
            parts.push(format!(
                "null:enable='between(t,{a:.4},{b:.4})'",
                a = a,
                b = b,
            ));
            continue;
        }
        let Some(snippet) = to_filter(&sampled.kind, i) else {
            continue;
        };
        let enable = format!("between(t,{a:.4},{b:.4})", a = a, b = b);
        parts.push(append_enable_to_chain(&snippet, &enable));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(","))
}

/// Whether an `Effect` has any animated parameter wired up. Used by
/// the renderer to pick between the fast static-filter path and the
/// per-segment animated path.
fn effect_has_animation(eff: &Effect) -> bool {
    if eff.animated_params.is_empty() || eff.param_kfs.is_empty() {
        return false;
    }
    eff.animated_params
        .iter()
        .any(|key| eff.param_kfs.get(key).map_or(false, |kfs| kfs.len() >= 2))
}

/// Build the segment list for an animated effect. Segments are the
/// half-open intervals between consecutive keyframe times of any
/// animated parameter, clamped to `[t_in, t_out]` and capped at
/// `MAX_SEGMENTS` to keep the filter graph within ffmpeg's
/// expression-length limits even on long clips with dense keyframes.
///
/// Returns an empty `Vec` when the clip has zero duration (the
/// renderer falls back to the static path so we still emit a
/// meaningful filter).
fn effect_segments(eff: &Effect, t_in: f32, t_out: f32) -> Vec<(f32, f32)> {
    const MAX_SEGMENTS: usize = 24;
    if t_out <= t_in + 1e-3 {
        return Vec::new();
    }

    // Union of all animated-param keyframe times, in CLIP-LOCAL time.
    let mut times: Vec<f32> = Vec::new();
    for key in &eff.animated_params {
        if let Some(kfs) = eff.param_kfs.get(key) {
            for kf in kfs {
                times.push(kf.t);
            }
        }
    }
    // Convert to scene time and clamp to the clip window.
    times = times
        .into_iter()
        .map(|t| (t + t_in).clamp(t_in, t_out))
        .collect();
    times.push(t_in);
    times.push(t_out);
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-4);

    // Ensure at least `MIN_POINTS` samples across the clip so a coarse
    // 2-3 keyframe animation still produces enough segments to read
    // as smooth in the export. We add a uniform grid spanning
    // [t_in, t_out] and dedup against the existing keyframe times so
    // grid points landing on keyframes don't bloat the segment list.
    const MIN_POINTS: usize = 8;
    if times.len() < MIN_POINTS {
        let span = t_out - t_in;
        let stride = span / (MIN_POINTS as f32 - 1.0);
        for i in 0..MIN_POINTS {
            times.push(t_in + (i as f32) * stride);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-4);
    }

    // Cap segment count: pick a uniform stride.
    if times.len() > MAX_SEGMENTS + 1 {
        let stride = ((times.len() - 1) as f32 / MAX_SEGMENTS as f32).ceil() as usize;
        let mut sub: Vec<f32> = times.iter().step_by(stride.max(1)).copied().collect();
        if *sub.last().unwrap() < t_out - 1e-4 {
            sub.push(t_out);
        }
        times = sub;
    }

    times
        .windows(2)
        .filter(|w| w[1] > w[0] + 1e-4)
        .map(|w| (w[0], w[1]))
        .collect()
}

/// Append `:enable='<expr>'` to every filter in a comma-joined chain.
/// Filters that already use `key=value` syntax get an extra
/// `:enable=…` token; argument-less filters (e.g. `hflip`) are
/// converted into `hflip=enable='…'` form so the option is parsed
/// correctly. Filters that accept positional args (e.g. `unsharp` /
/// `chromakey` in their short form) currently aren't emitted by
/// `effect_to_filter` — every snippet uses named arguments — so we
/// don't need a positional-arg converter here.
fn append_enable_to_chain(snippet: &str, enable_expr: &str) -> String {
    snippet
        .split(',')
        .map(|filter| append_enable_to_filter(filter.trim(), enable_expr))
        .collect::<Vec<_>>()
        .join(",")
}

fn append_enable_to_filter(filter: &str, enable_expr: &str) -> String {
    if filter.is_empty() {
        return filter.to_string();
    }
    if filter.contains('=') {
        format!("{filter}:enable='{enable_expr}'")
    } else {
        // Argument-less filter (`hflip`, `vflip`, `null`). Use `=` to
        // open the option list.
        format!("{filter}=enable='{enable_expr}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memstroy_core::keyframe::Keyframe;

    fn dummy_filter(_kind: &EffectKind, intensity: f32) -> Option<String> {
        Some(format!("noop=i={:.3}", intensity))
    }

    #[test]
    fn animated_chain_emits_per_segment_enable() {
        let mut eff = Effect::blur();
        eff.animated_params.insert("intensity".into());
        eff.param_kfs.insert(
            "intensity".into(),
            vec![
                Keyframe::new(0.0, 0.0_f32),
                Keyframe::new(1.0, 1.0_f32),
            ],
        );
        let chain = effect_animated_filter_chain(&eff, 0.0, 1.0, dummy_filter).unwrap();
        assert!(chain.contains(":enable='between(t,"));
        // At least 3 segments emitted (kf union + densification).
        assert!(chain.matches(":enable='between").count() >= 3);
    }

    #[test]
    fn static_effect_falls_back_to_helper() {
        let eff = Effect::blur();
        let chain = effect_animated_filter_chain(&eff, 0.0, 1.0, dummy_filter).unwrap();
        assert_eq!(chain, "noop=i=1.000");
    }

    #[test]
    fn argless_filter_gets_enable_via_equals() {
        assert_eq!(
            append_enable_to_filter("hflip", "between(t,0,1)"),
            "hflip=enable='between(t,0,1)'"
        );
    }
}
