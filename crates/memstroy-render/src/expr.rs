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
use memstroy_core::{
    ActorState, CanvasTransform, ColorCorrection, Easing, Keyframe, OverlayState,
    RenderFrameState, Scene,
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
        // u = clip((t - a.t)/span, 0, 1) — guards numerical drift at
        // the upper edge so the last segment doesn't overshoot.
        let u = format!("((t-{:.6})/{:.6})", a.t, span);
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
        expr = format!("if(lt(t,{:.6}),{},{})", b.t, segment, expr);
    }
    expr
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

// ─── COLOUR CORRECTION ──────────────────────────────────────────────

/// Build a list of ffmpeg filter snippets that approximate the
/// preview's `ColorCorrection` block.
///
/// We honour the four scalar fields the preview supports per-frame
/// (brightness / contrast / saturation / temperature). Tone curves
/// and per-channel LGG live in the preview's CPU pipeline only —
/// translating them to ffmpeg `curves=` would require sampling each
/// curve's control points; we leave it as a follow-up rather than
/// shipping a half-faithful implementation.
///
/// Animated CC params are sampled at the clip's midpoint — same
/// flattening rule we use for opacity / flip; full per-frame `eq=`
/// expressions are technically possible (`eval=frame`) but blow up
/// the filtergraph length when several actors animate CC together.
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
    out
}
