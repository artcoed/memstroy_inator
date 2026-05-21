use serde::{Deserialize, Serialize};

use crate::easing::Easing;

/// A single keyframe carrying a value of type `T` at time `t` (seconds).
///
/// `easing` controls the curve used to interpolate **into** this keyframe
/// from the previous one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Keyframe<T> {
    pub t: f32,
    pub value: T,
    #[serde(default)]
    pub easing: Easing,
}

impl<T> Keyframe<T> {
    pub fn new(t: f32, value: T) -> Self {
        Self { t, value, easing: Easing::default() }
    }

    pub fn with_easing(mut self, e: Easing) -> Self {
        self.easing = e;
        self
    }
}

/// Linear interpolation helper for tuples of floats.
pub trait Lerp {
    fn lerp(&self, other: &Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        self + (other - self) * t
    }
}

impl Lerp for [f32; 2] {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        [self[0].lerp(&other[0], t), self[1].lerp(&other[1], t)]
    }
}

/// Sample a keyframe track at time `t`. Returns `None` only if the track
/// is empty. Otherwise clamps to first/last value outside the range.
pub fn sample<T: Clone + Lerp>(track: &[Keyframe<T>], t: f32) -> Option<T> {
    if track.is_empty() {
        return None;
    }
    if t <= track[0].t {
        return Some(track[0].value.clone());
    }
    let last = track.last().unwrap();
    if t >= last.t {
        return Some(last.value.clone());
    }
    for w in track.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if t >= a.t && t <= b.t {
            let span = (b.t - a.t).max(1e-6);
            let raw = (t - a.t) / span;
            let curved = b.easing.apply(raw);
            return Some(a.value.lerp(&b.value, curved));
        }
    }
    Some(last.value.clone())
}

/// Insert a keyframe at `t`, replacing any existing keyframe within
/// `epsilon`. Returns the index of the (possibly replaced or inserted) entry.
/// Sorts the track by time afterwards.
pub fn upsert_keyframe<T: Clone + Lerp>(
    track: &mut Vec<Keyframe<T>>,
    t: f32,
    value: T,
) -> usize {
    let epsilon = 1.0e-3;
    if let Some(pos) = track
        .iter()
        .position(|kf| (kf.t - t).abs() < epsilon)
    {
        track[pos].value = value;
        return pos;
    }
    track.push(Keyframe::new(t, value));
    track.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    track
        .iter()
        .position(|kf| (kf.t - t).abs() < epsilon)
        .unwrap_or(0)
}



// ─── ANIMATION MODIFIERS ─────────────────────────────────────────────
//
// A "modifier" perturbs the sampled value of a track over time without
// requiring per-keyframe edits. Examples: wobble (sinusoidal sway),
// shake (pseudo-random jitter), pulse (scale breathing), spin
// (continuous rotation). Modifiers are layered on top of the eased
// keyframe value during preview / export.
//
// Each modifier owns a `t_start` / `t_end` window in clip-local seconds.
// When `t` is outside the window, the modifier contributes nothing.
//
// Modifiers do not mutate the underlying keyframes — they live in a
// separate `Vec<TrackModifier>` next to the keyframe list, so they can
// be toggled, retimed, and tweaked without disturbing user-authored
// keyframes.

/// One modifier instance applied to an animated element. Modifiers are
/// additive (they perturb the sampled state) and composable — the
/// editor lets the user stack multiple modifiers on a single element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackModifier {
    /// Activation window in clip-local seconds. `t_end < t_start` is
    /// treated as "always active". Defaults span the whole clip.
    #[serde(default)]
    pub t_start: f32,
    #[serde(default = "default_t_end")]
    pub t_end: f32,
    /// Whether this modifier is currently active (cheap toggle from the UI).
    #[serde(default = "default_true_mod")]
    pub enabled: bool,
    pub kind: ModifierKind,
}

fn default_t_end() -> f32 { f32::MAX }
fn default_true_mod() -> bool { true }

impl Default for TrackModifier {
    fn default() -> Self {
        Self {
            t_start: 0.0,
            t_end: f32::MAX,
            enabled: true,
            kind: ModifierKind::default(),
        }
    }
}

/// Different kinds of perturbations a modifier can apply.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModifierKind {
    /// Smooth sinusoidal sway in position (and optionally rotation).
    /// `freq_hz` cycles per second, `amp_x`/`amp_y` in world pixels,
    /// `amp_rot_deg` rocking angle in degrees.
    Wobble {
        freq_hz: f32,
        amp_x: f32,
        amp_y: f32,
        #[serde(default)]
        amp_rot_deg: f32,
        #[serde(default)]
        phase: f32,
    },
    /// Pseudo-random high-frequency jitter (camera-shake style).
    Shake {
        freq_hz: f32,
        amp_x: f32,
        amp_y: f32,
        #[serde(default)]
        seed: u32,
    },
    /// Periodic scale breathing — adds `amp_scale * sin(2π·f·t)` on top
    /// of the sampled scale.
    Pulse {
        freq_hz: f32,
        amp_scale: f32,
    },
    /// Continuous rotation at `speed_dps` degrees per second.
    Spin {
        speed_dps: f32,
    },
}

impl Default for ModifierKind {
    fn default() -> Self {
        ModifierKind::Wobble {
            freq_hz: 2.0,
            amp_x: 6.0,
            amp_y: 6.0,
            amp_rot_deg: 0.0,
            phase: 0.0,
        }
    }
}

/// Result of evaluating all modifiers at a given time. Additive on top of
/// the eased keyframe sample. Position deltas are in world pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModifierDelta {
    pub dx: f32,
    pub dy: f32,
    pub d_scale: f32,
    pub d_rotation_deg: f32,
}

impl ModifierDelta {
    pub fn is_zero(&self) -> bool {
        self.dx.abs() < 1e-4
            && self.dy.abs() < 1e-4
            && self.d_scale.abs() < 1e-4
            && self.d_rotation_deg.abs() < 1e-4
    }
}

/// Evaluate all enabled modifiers at time `t` (clip-local seconds) and
/// return the additive delta to layer on top of the eased keyframe value.
pub fn evaluate_modifiers(modifiers: &[TrackModifier], t: f32) -> ModifierDelta {
    let mut out = ModifierDelta::default();
    for m in modifiers {
        if !m.enabled { continue; }
        // `t_end < t_start` means "always active".
        let in_window = if m.t_end < m.t_start {
            true
        } else {
            t >= m.t_start && t <= m.t_end
        };
        if !in_window { continue; }
        match m.kind {
            ModifierKind::Wobble { freq_hz, amp_x, amp_y, amp_rot_deg, phase } => {
                let w = std::f32::consts::TAU * freq_hz * t + phase;
                out.dx += amp_x * w.sin();
                out.dy += amp_y * (w * 0.7).cos();
                out.d_rotation_deg += amp_rot_deg * w.sin();
            }
            ModifierKind::Shake { freq_hz, amp_x, amp_y, seed } => {
                // Hash-based jitter: stable per-frame at a given t/seed.
                let phase_x = ((seed as f32) * 0.137).fract() * std::f32::consts::TAU;
                let phase_y = ((seed as f32) * 0.731 + 1.7).fract() * std::f32::consts::TAU;
                let w = std::f32::consts::TAU * freq_hz * t;
                let n_x = (w * 1.0 + phase_x).sin()
                    + 0.5 * (w * 2.13 + phase_x).sin()
                    + 0.25 * (w * 4.27 + phase_x).sin();
                let n_y = (w * 1.0 + phase_y).cos()
                    + 0.5 * (w * 2.31 + phase_y).cos()
                    + 0.25 * (w * 3.97 + phase_y).cos();
                out.dx += amp_x * n_x * 0.6;
                out.dy += amp_y * n_y * 0.6;
            }
            ModifierKind::Pulse { freq_hz, amp_scale } => {
                let w = std::f32::consts::TAU * freq_hz * t;
                out.d_scale += amp_scale * w.sin();
            }
            ModifierKind::Spin { speed_dps } => {
                out.d_rotation_deg += speed_dps * t;
            }
        }
    }
    out
}

/// Convenience constructors for common modifier presets.
impl TrackModifier {
    pub fn wobble() -> Self {
        Self {
            kind: ModifierKind::Wobble {
                freq_hz: 2.0,
                amp_x: 6.0,
                amp_y: 6.0,
                amp_rot_deg: 0.0,
                phase: 0.0,
            },
            ..Self::default()
        }
    }
    pub fn shake() -> Self {
        Self {
            kind: ModifierKind::Shake {
                freq_hz: 12.0,
                amp_x: 4.0,
                amp_y: 4.0,
                seed: 42,
            },
            ..Self::default()
        }
    }
    pub fn pulse() -> Self {
        Self {
            kind: ModifierKind::Pulse {
                freq_hz: 1.5,
                amp_scale: 0.06,
            },
            ..Self::default()
        }
    }
    pub fn spin() -> Self {
        Self {
            kind: ModifierKind::Spin { speed_dps: 60.0 },
            ..Self::default()
        }
    }

    /// Human-readable label for UI display.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            ModifierKind::Wobble { .. } => "Wobble",
            ModifierKind::Shake { .. } => "Shake",
            ModifierKind::Pulse { .. } => "Pulse",
            ModifierKind::Spin { .. } => "Spin",
        }
    }
}
