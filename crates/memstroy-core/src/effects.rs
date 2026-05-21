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
    ]
}
