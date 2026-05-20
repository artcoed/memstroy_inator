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
