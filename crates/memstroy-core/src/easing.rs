use serde::{Deserialize, Serialize};

/// Interpolation curve between two keyframes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    /// No interpolation: hold previous value until the next keyframe.
    Step,
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    /// Cubic bezier specified by (x1, y1, x2, y2). Reserved for future use.
    Cubic,
}

impl Easing {
    /// Map a normalised time `t` in [0, 1] through the curve.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Step => 0.0,
            Easing::Linear => t,
            Easing::EaseIn => t * t,
            Easing::EaseOut => 1.0 - (1.0 - t).powi(2),
            Easing::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Easing::Cubic => {
                // Placeholder: smoothstep-like cubic until param-bezier is added.
                t * t * (3.0 - 2.0 * t)
            }
        }
    }
}
