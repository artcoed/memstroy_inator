//! Safe ranges for inspector / transport controls so extreme values
//! cannot destabilise preview or export.

/// Timeline transport preview speed.
pub const PLAYBACK_SPEED: std::ops::RangeInclusive<f32> = 0.1..=4.0;

/// Per-clip playback multiplier on actors / overlays / audio.
pub const CLIP_SPEED: std::ops::RangeInclusive<f32> = 0.05..=8.0;

pub const SCALE: std::ops::RangeInclusive<f32> = 0.01..=8.0;
pub const SCALE_Y: std::ops::RangeInclusive<f32> = 0.01..=8.0;
pub const OPACITY: std::ops::RangeInclusive<f32> = 0.0..=1.0;
pub const ROTATION_DEG: std::ops::RangeInclusive<f32> = -720.0..=720.0;
pub const POS_NORM: std::ops::RangeInclusive<f32> = -5.0..=5.0;
pub const FONT_SIZE_PX: std::ops::RangeInclusive<f32> = 4.0..=512.0;
pub const OUTLINE_WIDTH: std::ops::RangeInclusive<f32> = 0.0..=64.0;
pub const VOLUME_DB: std::ops::RangeInclusive<f32> = -60.0..=24.0;

pub fn clamp_playback_speed(v: f32) -> f32 {
    clamp_finite(v, PLAYBACK_SPEED)
}

pub fn clamp_clip_speed(v: f32) -> f32 {
    clamp_finite(v, CLIP_SPEED)
}

pub fn clamp_scale(v: f32) -> f32 {
    clamp_finite(v, SCALE)
}

pub fn clamp_scale_y(v: f32) -> f32 {
    clamp_finite(v, SCALE_Y)
}

pub fn clamp_opacity(v: f32) -> f32 {
    clamp_finite(v, OPACITY)
}

pub fn clamp_rotation_deg(v: f32) -> f32 {
    clamp_finite(v, ROTATION_DEG)
}

pub fn clamp_pos_norm(v: f32) -> f32 {
    clamp_finite(v, POS_NORM)
}

pub fn clamp_font_size(v: f32) -> f32 {
    clamp_finite(v, FONT_SIZE_PX)
}

pub fn clamp_outline_width(v: f32) -> f32 {
    clamp_finite(v, OUTLINE_WIDTH)
}

pub fn clamp_volume_db(v: f32) -> f32 {
    clamp_finite(v, VOLUME_DB)
}

fn clamp_finite(v: f32, range: std::ops::RangeInclusive<f32>) -> f32 {
    if !v.is_finite() {
        return *range.start();
    }
    v.clamp(*range.start(), *range.end())
}
