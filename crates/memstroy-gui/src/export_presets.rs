//! Export preset configurations for popular platforms.

use memstroy_core::OutputSpec;

pub struct ExportPreset {
    pub name: &'static str,
    pub icon: &'static str,
    pub resolution: [u32; 2],
    pub fps: u32,
    pub aspect_label: &'static str,
    pub max_duration: f32,
    pub description: &'static str,
}

pub const PRESETS: &[ExportPreset] = &[
    ExportPreset {
        name: "TikTok / Reels / Shorts",
        icon: "\u{1F4F1}",
        resolution: [1080, 1920],
        fps: 60,
        aspect_label: "9:16",
        max_duration: 60.0,
        description: "Vertical video for TikTok, Instagram Reels, YouTube Shorts",
    },
    ExportPreset {
        name: "YouTube / Twitter",
        icon: "\u{1F4FA}",
        resolution: [1920, 1080],
        fps: 30,
        aspect_label: "16:9",
        max_duration: 600.0,
        description: "Landscape video for YouTube and Twitter",
    },
    ExportPreset {
        name: "Instagram Post",
        icon: "\u{1F7E6}",
        resolution: [1080, 1080],
        fps: 30,
        aspect_label: "1:1",
        max_duration: 60.0,
        description: "Square video for Instagram feed",
    },
    ExportPreset {
        name: "Twitter Card",
        icon: "\u{1F426}",
        resolution: [1280, 720],
        fps: 30,
        aspect_label: "16:9",
        max_duration: 140.0,
        description: "Optimized for Twitter inline playback",
    },
    ExportPreset {
        name: "WhatsApp / Telegram",
        icon: "\u{1F4AC}",
        resolution: [720, 1280],
        fps: 30,
        aspect_label: "9:16",
        max_duration: 30.0,
        description: "Compact size for messaging apps",
    },
];

pub fn apply_preset(spec: &mut OutputSpec, preset: &ExportPreset) {
    spec.resolution = preset.resolution;
    spec.fps = preset.fps;
    spec.duration = spec.duration.min(preset.max_duration);
}
