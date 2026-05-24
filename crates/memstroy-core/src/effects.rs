//! Generic effect stack that can be applied to any video / overlay clip.
//!
//! An [`Effect`] is one entry in the stack; the stack is just a `Vec<Effect>`
//! evaluated top-down at preview / export time. Each effect kind owns its
//! own parameter struct, but every effect carries the same envelope:
//! `enabled` toggle and `intensity` 0..1 master, so a user can temporarily
//! mute an effect or dial it down without losing its tuned parameters.
//!
//! The stack is intentionally simple — effects are pure colour / pixel
//! transforms applied AFTER chroma-key and colour correction. A subset
//! also affect geometry (zoom / mirror) but those are still expressed
//! per-pixel so they compose without special cases.

use serde::{Deserialize, Serialize};

/// One entry in an element's effect stack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Effect {
    /// Cheap mute toggle from the UI.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Master amount 0..1; multiplies the per-effect strength so the user
    /// can fade an effect in/out without re-tuning its inner params.
    #[serde(default = "default_one")]
    pub intensity: f32,
    pub kind: EffectKind,

    /// Per-effect-parameter keyframe tracks. Keyed by a string param id
    /// such as `"intensity"`, `"radius"`, or `"amount"` so the same map
    /// works for every `EffectKind` variant. When a key is present
    /// AND the corresponding entry in `animated_params` is set, the
    /// renderer should `keyframe::sample(track, t)` to obtain the
    /// time-varying value; otherwise the static value on the variant /
    /// the `intensity` field is used.
    ///
    /// NOTE: full renderer support for animated effect params is a
    /// future extension; the inspector's per-param "Animated" toggle
    /// stores the user's intent here so the wiring lands incrementally.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub param_kfs: std::collections::BTreeMap<String, Vec<crate::keyframe::Keyframe<f32>>>,

    /// Set of effect-parameter ids the user has flagged as animatable
    /// for this effect (mirrors the per-element `animated_params`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub animated_params: std::collections::BTreeSet<String>,
}

fn default_true() -> bool { true }
fn default_one() -> f32 { 1.0 }

impl Default for Effect {
    fn default() -> Self {
        Self {
            enabled: true,
            intensity: 1.0,
            kind: EffectKind::default(),
            param_kfs: std::collections::BTreeMap::new(),
            animated_params: std::collections::BTreeSet::new(),
        }
    }
}

/// Library of supported effect kinds. This is the menu the user sees in
/// the inspector's "Add effect" dropdown. Adding a new kind here lights
/// it up everywhere the stack is consumed (preview + ffmpeg export).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectKind {
    /// Box / Gaussian-style blur. `radius` in pixels.
    Blur { radius: f32 },
    /// Sharpen via unsharp mask. `amount` 0..3.
    Sharpen { amount: f32 },
    /// Desaturate / black & white.
    Grayscale,
    /// Sepia tint.
    Sepia,
    /// Negative colours.
    Invert,
    /// Hue rotation in degrees.
    HueShift { degrees: f32 },
    /// Vignette (darkened corners). `strength` 0..1.
    Vignette { strength: f32 },
    /// Pixelate / mosaic. `block_size` in source pixels.
    Pixelate { block_size: f32 },
    /// Posterize — quantise colours into N levels. `levels` 2..32.
    Posterize { levels: u32 },
    /// Glow / bloom — bright pixels bleed onto neighbours.
    Glow { radius: f32, intensity: f32 },
    /// Gain in stops via brightness multiplier.
    Brightness { amount: f32 },
    /// Contrast around 50% grey.
    Contrast { amount: f32 },
    /// Saturation around grayscale.
    Saturation { amount: f32 },
    /// Edge detection (Sobel-ish). `threshold` 0..1.
    EdgeDetect { threshold: f32 },
    /// Mirror horizontally.
    MirrorH,
    /// Mirror vertically.
    MirrorV,
    /// Chromatic aberration: split RGB channels by `offset` pixels.
    ChromaticAberration { offset: f32 },
    /// Noise / film grain. `amount` 0..1 controls the noise sigma.
    Noise { amount: f32 },
    /// Sinusoidal wave distortion. `amplitude` (px), `wavelength` (px).
    Wave { amplitude: f32, wavelength: f32 },
    /// Old-film vignette + grain + slight desaturation. Simple preset.
    OldFilm,
    /// VHS-style chromatic shift + scanlines. Simple preset.
    Vhs,
    /// Glitch — block-shifted pixel offset. `strength` 0..1.
    Glitch { strength: f32 },
    /// Bloom — soft bright halo around highlights.
    Bloom { radius: f32 },
    /// Crop the source to a rectangular sub-region. Each value is a
    /// normalised inset from the corresponding edge in 0..0.49 — e.g.
    /// `left=0.1` discards the leftmost 10% of pixels. The visible
    /// rectangle is therefore `[left .. 1.0 - right]` × `[top .. 1.0 - bottom]`.
    /// Acts as a Photoshop-style "Crop" tool when applied to images
    /// AND a coarse-grained mask when applied to videos / actors.
    Crop { left: f32, top: f32, right: f32, bottom: f32 },
    /// **Free-form mask**: keeps pixels INSIDE `shape` (or outside when
    /// `invert` is set), zeroing the alpha of the rest. The shape
    /// coordinates live in the element's own UV space (0..1 mapped to
    /// the element's bounding box) so the mask follows the element
    /// without re-tuning when it is moved / resized on the canvas.
    ///
    /// `feather` softens the edge of the mask. Expressed as a fraction
    /// of the element's smaller dimension (0..0.5), it produces an
    /// alpha gradient that fades from 1 inside the shape to 0 outside.
    /// Authored interactively via the canvas mask tools (rectangle,
    /// ellipse, freehand) — see `EditorState::mask_tool`.
    Mask {
        shape: MaskShape,
        #[serde(default)]
        feather: f32,
        #[serde(default)]
        invert: bool,
    },
    /// **Colour-key mask**: keys out (or keeps) every pixel within
    /// HSV similarity of `color`. Authored interactively via the
    /// canvas eyedropper mask tool — clicking on the picture samples
    /// the underlying pixel colour and pushes a `ColorKey` entry onto
    /// the layer's effect stack. Unlike the chroma-key colour the
    /// user sets on `Actor.chroma_key` / `Overlay::Image.chroma_key`
    /// (which keys the SOURCE pixels before the effect stack), this
    /// effect runs alongside the rest of the stack so it composes
    /// with feathered masks and other geometry-based mask shapes.
    ///
    /// Parameters mirror the FFmpeg `chromakey` filter: `similarity`
    /// is the colour-distance threshold, `blend` softens the edge of
    /// the keyed region, and `spill` controls de-spill suppression
    /// (pulls the keyed colour out of remaining edges so green halos
    /// don't survive). `intensity` (the master envelope) blends
    /// between "no keying" and "fully keyed" so the same effect can
    /// be faded in / out without re-tuning the colour distance.
    ///
    /// `invert` flips the polarity: with `invert = false` (default)
    /// pixels whose colour is close to `color` become transparent;
    /// with `invert = true` pixels FAR from `color` become transparent
    /// — useful for "keep only the green parts" instead of "remove
    /// the green parts".
    ColorKey {
        /// Target colour as RGB 0..255.
        color: [u8; 3],
        /// Similarity threshold (0..1). Larger values key a wider
        /// range of pixels around `color`.
        #[serde(default = "default_color_key_similarity")]
        similarity: f32,
        /// Edge softening (0..1).
        #[serde(default = "default_color_key_blend")]
        blend: f32,
        /// De-spill amount (0..1). Reduces the keyed colour's
        /// presence in surviving edge pixels.
        #[serde(default)]
        spill: f32,
        /// Invert the keep/remove polarity (see kind doc).
        #[serde(default)]
        invert: bool,
    },
}

fn default_color_key_similarity() -> f32 { 0.18 }
fn default_color_key_blend() -> f32 { 0.10 }

/// A 2D mask shape in element-local UV coordinates (0..1 spans the
/// bounding box). All `EffectKind::Mask` shapes round-trip through the
/// scene file the same way as the parent effect — they're intentionally
/// simple enough to YAML-serialise without custom helpers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum MaskShape {
    /// Axis-aligned rectangle, edges in UV.
    Rect { left: f32, top: f32, right: f32, bottom: f32 },
    /// Ellipse with centre and radii in UV. `rx`, `ry` are radii (not
    /// diameters); a circle is `rx == ry`.
    Ellipse { cx: f32, cy: f32, rx: f32, ry: f32 },
    /// Closed polygon — the outline is implicitly closed by joining the
    /// last point back to the first when rendering / hit-testing. UV
    /// coordinates per vertex. Drawn by the freehand mask tool.
    Polygon { points: Vec<[f32; 2]> },
}

impl MaskShape {
    /// Default rectangle covering the whole element — used as the
    /// preset value when the user picks "Mask (rect)" from the
    /// inspector dropdown. Subsequently overwritten by the canvas
    /// mask tool when the user actually paints a shape.
    pub fn rect_full() -> Self {
        Self::Rect { left: 0.1, top: 0.1, right: 0.9, bottom: 0.9 }
    }

    /// Default ellipse centred on the element.
    pub fn ellipse_full() -> Self {
        Self::Ellipse { cx: 0.5, cy: 0.5, rx: 0.4, ry: 0.4 }
    }

    /// Test whether a UV point falls inside this shape.
    /// `(0,0)` is the top-left of the element, `(1,1)` is the bottom-right.
    pub fn contains_uv(&self, u: f32, v: f32) -> bool {
        match self {
            MaskShape::Rect { left, top, right, bottom } => {
                u >= *left && u <= *right && v >= *top && v <= *bottom
            }
            MaskShape::Ellipse { cx, cy, rx, ry } => {
                let rx = rx.max(1e-6);
                let ry = ry.max(1e-6);
                let du = (u - cx) / rx;
                let dv = (v - cy) / ry;
                du * du + dv * dv <= 1.0
            }
            MaskShape::Polygon { points } => point_in_polygon(u, v, points),
        }
    }

    /// Signed-distance-style margin: positive when the point is well
    /// inside the shape, 0 at the edge, negative outside. Returned in
    /// the SAME units as `u`, `v` (UV 0..1). The preview pipeline uses
    /// it to compute feathered alpha:
    ///   `alpha = clamp(margin / feather + 0.5, 0, 1)`
    pub fn signed_margin_uv(&self, u: f32, v: f32) -> f32 {
        match self {
            MaskShape::Rect { left, top, right, bottom } => {
                let dx = (u - *left).min(*right - u);
                let dy = (v - *top).min(*bottom - v);
                dx.min(dy)
            }
            MaskShape::Ellipse { cx, cy, rx, ry } => {
                let rx = rx.max(1e-6);
                let ry = ry.max(1e-6);
                let du = (u - cx) / rx;
                let dv = (v - cy) / ry;
                // Convert "1 - r" into a UV-scaled distance by
                // multiplying by the average radius — close enough for
                // a soft feather and avoids a square-root per pixel.
                let r = (du * du + dv * dv).sqrt();
                (1.0 - r) * 0.5 * (rx + ry)
            }
            MaskShape::Polygon { points } => {
                let inside = point_in_polygon(u, v, points);
                let d = polygon_min_edge_dist(u, v, points);
                if inside { d } else { -d }
            }
        }
    }
}

#[inline]
fn point_in_polygon(u: f32, v: f32, pts: &[[f32; 2]]) -> bool {
    if pts.len() < 3 { return false; }
    let mut inside = false;
    let n = pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let pi = pts[i];
        let pj = pts[j];
        if ((pi[1] > v) != (pj[1] > v))
            && (u < (pj[0] - pi[0]) * (v - pi[1]) / (pj[1] - pi[1] + 1e-12) + pi[0])
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[inline]
fn polygon_min_edge_dist(u: f32, v: f32, pts: &[[f32; 2]]) -> f32 {
    if pts.len() < 2 { return 1.0; }
    let mut best = f32::MAX;
    let n = pts.len();
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let d = point_to_segment_dist(u, v, a, b);
        if d < best { best = d; }
    }
    best
}

#[inline]
fn point_to_segment_dist(px: f32, py: f32, a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let l2 = dx * dx + dy * dy;
    if l2 <= 1e-12 {
        let qx = px - a[0];
        let qy = py - a[1];
        return (qx * qx + qy * qy).sqrt();
    }
    let t = ((px - a[0]) * dx + (py - a[1]) * dy) / l2;
    let t = t.clamp(0.0, 1.0);
    let qx = px - (a[0] + t * dx);
    let qy = py - (a[1] + t * dy);
    (qx * qx + qy * qy).sqrt()
}

impl Default for EffectKind {
    fn default() -> Self {
        EffectKind::Blur { radius: 6.0 }
    }
}

impl EffectKind {
    /// Short label used in the inspector header.
    pub fn label(&self) -> &'static str {
        match self {
            EffectKind::Blur { .. } => "Blur",
            EffectKind::Sharpen { .. } => "Sharpen",
            EffectKind::Grayscale => "Grayscale",
            EffectKind::Sepia => "Sepia",
            EffectKind::Invert => "Invert",
            EffectKind::HueShift { .. } => "Hue shift",
            EffectKind::Vignette { .. } => "Vignette",
            EffectKind::Pixelate { .. } => "Pixelate",
            EffectKind::Posterize { .. } => "Posterize",
            EffectKind::Glow { .. } => "Glow",
            EffectKind::Brightness { .. } => "Brightness",
            EffectKind::Contrast { .. } => "Contrast",
            EffectKind::Saturation { .. } => "Saturation",
            EffectKind::EdgeDetect { .. } => "Edge detect",
            EffectKind::MirrorH => "Mirror H",
            EffectKind::MirrorV => "Mirror V",
            EffectKind::ChromaticAberration { .. } => "Chromatic aberration",
            EffectKind::Noise { .. } => "Noise",
            EffectKind::Wave { .. } => "Wave",
            EffectKind::OldFilm => "Old film",
            EffectKind::Vhs => "VHS",
            EffectKind::Glitch { .. } => "Glitch",
            EffectKind::Bloom { .. } => "Bloom",
            EffectKind::Crop { .. } => "Crop",
            EffectKind::Mask { .. } => "Mask",
            EffectKind::ColorKey { .. } => "Chroma Key",
        }
    }
}

impl Effect {
    pub fn new(kind: EffectKind) -> Self {
        Self {
            enabled: true,
            intensity: 1.0,
            kind,
            param_kfs: std::collections::BTreeMap::new(),
            animated_params: std::collections::BTreeSet::new(),
        }
    }

    /// Convenience constructors for the most-used presets. Used by the
    /// inspector "+ Effect" menu so the user gets sane starting values.
    pub fn blur() -> Self { Self::new(EffectKind::Blur { radius: 6.0 }) }
    pub fn sharpen() -> Self { Self::new(EffectKind::Sharpen { amount: 0.6 }) }
    pub fn grayscale() -> Self { Self::new(EffectKind::Grayscale) }
    pub fn sepia() -> Self { Self::new(EffectKind::Sepia) }
    pub fn invert() -> Self { Self::new(EffectKind::Invert) }
    pub fn hue_shift() -> Self { Self::new(EffectKind::HueShift { degrees: 60.0 }) }
    pub fn vignette() -> Self { Self::new(EffectKind::Vignette { strength: 0.6 }) }
    pub fn pixelate() -> Self { Self::new(EffectKind::Pixelate { block_size: 12.0 }) }
    pub fn posterize() -> Self { Self::new(EffectKind::Posterize { levels: 6 }) }
    pub fn glow() -> Self { Self::new(EffectKind::Glow { radius: 12.0, intensity: 0.6 }) }
    pub fn brightness() -> Self { Self::new(EffectKind::Brightness { amount: 0.2 }) }
    pub fn contrast() -> Self { Self::new(EffectKind::Contrast { amount: 0.3 }) }
    pub fn saturation() -> Self { Self::new(EffectKind::Saturation { amount: 0.4 }) }
    pub fn edge_detect() -> Self { Self::new(EffectKind::EdgeDetect { threshold: 0.2 }) }
    pub fn mirror_h() -> Self { Self::new(EffectKind::MirrorH) }
    pub fn mirror_v() -> Self { Self::new(EffectKind::MirrorV) }
    pub fn chromatic_aberration() -> Self { Self::new(EffectKind::ChromaticAberration { offset: 4.0 }) }
    pub fn noise() -> Self { Self::new(EffectKind::Noise { amount: 0.15 }) }
    pub fn wave() -> Self { Self::new(EffectKind::Wave { amplitude: 6.0, wavelength: 60.0 }) }
    pub fn old_film() -> Self { Self::new(EffectKind::OldFilm) }
    pub fn vhs() -> Self { Self::new(EffectKind::Vhs) }
    pub fn glitch() -> Self { Self::new(EffectKind::Glitch { strength: 0.5 }) }
    pub fn bloom() -> Self { Self::new(EffectKind::Bloom { radius: 18.0 }) }
    pub fn crop() -> Self {
        Self::new(EffectKind::Crop {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        })
    }
    /// Rectangular mask preset. Intentionally larger than `crop()` so
    /// the picture is still visible after the effect lands and the
    /// user can edit the bounds via the canvas mask tool.
    pub fn mask_rect() -> Self {
        Self::new(EffectKind::Mask {
            shape: MaskShape::rect_full(),
            feather: 0.0,
            invert: false,
        })
    }
    pub fn mask_ellipse() -> Self {
        Self::new(EffectKind::Mask {
            shape: MaskShape::ellipse_full(),
            feather: 0.05,
            invert: false,
        })
    }
    pub fn mask_freehand() -> Self {
        // Default to a tiny diamond-ish shape so the effect renders
        // *something* immediately even before the user paints.
        let pts = vec![
            [0.5, 0.15], [0.85, 0.5], [0.5, 0.85], [0.15, 0.5],
        ];
        Self::new(EffectKind::Mask {
            shape: MaskShape::Polygon { points: pts },
            feather: 0.0,
            invert: false,
        })
    }

    /// Colour-key mask preset. Defaults to a Telegram-channel green
    /// (matching `ChromaKeyParams::default`) so the entry shows
    /// visible behaviour the moment it's added; the eyedropper tool
    /// overrides `color` immediately after committing this preset
    /// when the user paints with `MaskTool::Eyedropper`.
    pub fn color_key() -> Self {
        Self::new(EffectKind::ColorKey {
            color: [0, 177, 64],
            similarity: default_color_key_similarity(),
            blend: default_color_key_blend(),
            spill: 0.0,
            invert: false,
        })
    }

    /// Sample a single named parameter at clip-local time `t_local`.
    ///
    /// When `key` is in `animated_params` AND `param_kfs[key]` has at
    /// least one keyframe, the eased keyframe value is returned;
    /// otherwise `fallback` is passed through. The renderer / preview
    /// path uses this to honour per-parameter keyframes without having
    /// to special-case every effect kind.
    ///
    /// Standard keys used by the inspector:
    /// - `"intensity"` — master amount on `Effect.intensity`.
    /// - `"p0"` and (for two-param kinds) `"p1"` — primary / secondary
    ///   per-kind parameter (radius / amount / threshold / …).
    pub fn sample_param_at(&self, t_local: f32, key: &str, fallback: f32) -> f32 {
        if !self.animated_params.contains(key) {
            return fallback;
        }
        let Some(kfs) = self.param_kfs.get(key) else { return fallback; };
        if kfs.is_empty() {
            return fallback;
        }
        crate::keyframe::sample(kfs, t_local).unwrap_or(fallback)
    }

    /// Return a clone of this effect with every animated parameter
    /// replaced by its keyframe-sampled value at `t_local`. Consumers
    /// (preview pipeline, ffmpeg filtergraph builder) that want to
    /// honour animation just call `eff.sampled_at(t_local)` and treat
    /// the result as a static-effect description.
    pub fn sampled_at(&self, t_local: f32) -> Self {
        use EffectKind as K;
        let mut out = self.clone();
        out.intensity = self.sample_param_at(t_local, "intensity", self.intensity);
        out.kind = match &self.kind {
            K::Blur { radius } => K::Blur {
                radius: self.sample_param_at(t_local, "p0", *radius),
            },
            K::Sharpen { amount } => K::Sharpen {
                amount: self.sample_param_at(t_local, "p0", *amount),
            },
            K::HueShift { degrees } => K::HueShift {
                degrees: self.sample_param_at(t_local, "p0", *degrees),
            },
            K::Vignette { strength } => K::Vignette {
                strength: self.sample_param_at(t_local, "p0", *strength),
            },
            K::Pixelate { block_size } => K::Pixelate {
                block_size: self.sample_param_at(t_local, "p0", *block_size),
            },
            K::Posterize { levels } => K::Posterize {
                levels: (self
                    .sample_param_at(t_local, "p0", *levels as f32)
                    as u32)
                    .max(2),
            },
            K::Glow { radius, intensity } => K::Glow {
                radius: self.sample_param_at(t_local, "p0", *radius),
                intensity: self.sample_param_at(t_local, "p1", *intensity),
            },
            K::Brightness { amount } => K::Brightness {
                amount: self.sample_param_at(t_local, "p0", *amount),
            },
            K::Contrast { amount } => K::Contrast {
                amount: self.sample_param_at(t_local, "p0", *amount),
            },
            K::Saturation { amount } => K::Saturation {
                amount: self.sample_param_at(t_local, "p0", *amount),
            },
            K::EdgeDetect { threshold } => K::EdgeDetect {
                threshold: self.sample_param_at(t_local, "p0", *threshold),
            },
            K::ChromaticAberration { offset } => K::ChromaticAberration {
                offset: self.sample_param_at(t_local, "p0", *offset),
            },
            K::Noise { amount } => K::Noise {
                amount: self.sample_param_at(t_local, "p0", *amount),
            },
            K::Wave { amplitude, wavelength } => K::Wave {
                amplitude: self.sample_param_at(t_local, "p0", *amplitude),
                wavelength: self.sample_param_at(t_local, "p1", *wavelength),
            },
            K::Glitch { strength } => K::Glitch {
                strength: self.sample_param_at(t_local, "p0", *strength),
            },
            K::Bloom { radius } => K::Bloom {
                radius: self.sample_param_at(t_local, "p0", *radius),
            },
            K::Crop { left, top, right, bottom } => K::Crop {
                left: self.sample_param_at(t_local, "p0", *left).clamp(0.0, 0.49),
                top: self.sample_param_at(t_local, "p1", *top).clamp(0.0, 0.49),
                right: self.sample_param_at(t_local, "p2", *right).clamp(0.0, 0.49),
                bottom: self.sample_param_at(t_local, "p3", *bottom).clamp(0.0, 0.49),
            },
            // Mask shape geometry has historically not been animated
            // (the geometry was never sampled from `param_kfs`), but
            // the per-shape scalar fields ARE numerical and so are
            // sampled here under stable per-field keys
            // ("rect_left" / "ellipse_cx" / …). Polygon stays static
            // — keyframing a `Vec<[f32;2]>` of varying length needs a
            // typed kf track, which is a future extension.
            K::Mask { shape, feather, invert } => {
                let new_shape = match shape {
                    MaskShape::Rect { left, top, right, bottom } => MaskShape::Rect {
                        left:   self.sample_param_at(t_local, "rect_left",   *left).clamp(0.0, 1.0),
                        top:    self.sample_param_at(t_local, "rect_top",    *top).clamp(0.0, 1.0),
                        right:  self.sample_param_at(t_local, "rect_right",  *right).clamp(0.0, 1.0),
                        bottom: self.sample_param_at(t_local, "rect_bottom", *bottom).clamp(0.0, 1.0),
                    },
                    MaskShape::Ellipse { cx, cy, rx, ry } => MaskShape::Ellipse {
                        cx: self.sample_param_at(t_local, "ellipse_cx", *cx),
                        cy: self.sample_param_at(t_local, "ellipse_cy", *cy),
                        rx: self.sample_param_at(t_local, "ellipse_rx", *rx).max(0.0),
                        ry: self.sample_param_at(t_local, "ellipse_ry", *ry).max(0.0),
                    },
                    MaskShape::Polygon { points } => MaskShape::Polygon {
                        points: points.clone(),
                    },
                };
                K::Mask {
                    shape: new_shape,
                    feather: self.sample_param_at(t_local, "p0", *feather).clamp(0.0, 0.5),
                    invert: *invert,
                }
            }
            K::ColorKey { color, similarity, blend, spill, invert } => K::ColorKey {
                color: *color,
                similarity: self.sample_param_at(t_local, "p0", *similarity).clamp(0.0, 1.0),
                blend: self.sample_param_at(t_local, "p1", *blend).clamp(0.0, 1.0),
                spill: self.sample_param_at(t_local, "p2", *spill).clamp(0.0, 1.0),
                invert: *invert,
            },
            // Kinds without numeric parameters pass through unchanged.
            K::Grayscale | K::Sepia | K::Invert | K::MirrorH | K::MirrorV
                | K::OldFilm | K::Vhs => self.kind.clone(),
        };
        out
    }
}

/// Every effect kind known to the editor, in display order. Used by the
/// inspector to populate the "+ Add effect" menu without having to
/// hand-list every entry next to it.
pub fn all_effect_presets() -> Vec<Effect> {
    vec![
        Effect::blur(),
        Effect::sharpen(),
        Effect::grayscale(),
        Effect::sepia(),
        Effect::invert(),
        Effect::hue_shift(),
        Effect::vignette(),
        Effect::pixelate(),
        Effect::posterize(),
        Effect::glow(),
        Effect::brightness(),
        Effect::contrast(),
        Effect::saturation(),
        Effect::edge_detect(),
        Effect::mirror_h(),
        Effect::mirror_v(),
        Effect::chromatic_aberration(),
        Effect::noise(),
        Effect::wave(),
        Effect::old_film(),
        Effect::vhs(),
        Effect::glitch(),
        Effect::bloom(),
        // Crop is intentionally NOT exposed in the generic effect
        // picker any more — it duplicated the rectangle mask tool
        // from a UX standpoint, so we now expose only one rectangle
        // operation (the rectangle mask). Crop entries are still
        // round-tripped from older scenes and are still authored
        // via the dedicated image editor panel (which manipulates
        // EffectKind::Crop directly through sliders), so the
        // constructor itself stays public.
        Effect::mask_rect(),
        Effect::mask_ellipse(),
        Effect::mask_freehand(),
        Effect::color_key(),
    ]
}
