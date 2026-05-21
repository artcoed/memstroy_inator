//! Pre-made meme text templates with styled animations.

use memstroy_core::{TextOverlay, TextStyle, OverlayState, Keyframe};

pub struct TitleTemplate {
    pub name: &'static str,
    pub icon: &'static str,
    pub description: &'static str,
    /// Default text content (user will edit)
    pub default_text: &'static str,
    /// Text style configuration
    pub style: fn() -> TextStyle,
    /// Position and animation keyframes
    pub layout: fn(duration: f32) -> Vec<Keyframe<OverlayState>>,
}

pub const TEMPLATES: &[TitleTemplate] = &[
    TitleTemplate {
        name: "Impact (top)",
        icon: "\u{1F4A5}",
        description: "Classic meme top text \u{2014} bold white with black outline",
        default_text: "WHEN YOU SEE",
        style: || TextStyle {
            font_size: 72.0,
            color: [255, 255, 255],
            box_color: None,
            outline: Some([0, 0, 0]),
            outline_width: 4.0,
            bold: true,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 0.1], scale: 1.0, rotation_deg: 0.0, opacity: 0.0 }),
            Keyframe::new(0.2, OverlayState { pos: [0.5, 0.1], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.1], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
        ],
    },
    TitleTemplate {
        name: "Impact (bottom)",
        icon: "\u{1F4A5}",
        description: "Classic meme bottom text",
        default_text: "BUT THEN...",
        style: || TextStyle {
            font_size: 72.0,
            color: [255, 255, 255],
            box_color: None,
            outline: Some([0, 0, 0]),
            outline_width: 4.0,
            bold: true,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 0.9], scale: 1.0, rotation_deg: 0.0, opacity: 0.0 }),
            Keyframe::new(0.2, OverlayState { pos: [0.5, 0.9], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.9], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
        ],
    },
    TitleTemplate {
        name: "Subtitle Box",
        icon: "\u{1F4AC}",
        description: "Modern subtitle with white box background",
        default_text: "Subtitle text",
        style: || TextStyle {
            font_size: 48.0,
            color: [0, 0, 0],
            box_color: Some([255, 255, 255]),
            box_padding: 16.0,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 0.85], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.85], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
        ],
    },
    TitleTemplate {
        name: "Pop In",
        icon: "\u{1F3AF}",
        description: "Animated pop-in entry \u{2014} scale from 0 to overshoot to 1",
        default_text: "POP!",
        style: || TextStyle {
            font_size: 96.0,
            color: [255, 220, 60],
            box_color: None,
            outline: Some([0, 0, 0]),
            outline_width: 6.0,
            bold: true,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 0.5], scale: 0.0, rotation_deg: 0.0, opacity: 0.0 }),
            Keyframe::new(0.15, OverlayState { pos: [0.5, 0.5], scale: 1.3, rotation_deg: 5.0, opacity: 1.0 }),
            Keyframe::new(0.25, OverlayState { pos: [0.5, 0.5], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.5], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
        ],
    },
    TitleTemplate {
        name: "Slide Up",
        icon: "\u{2B06}",
        description: "Slides up from below the screen",
        default_text: "Sliding text",
        style: || TextStyle {
            font_size: 56.0,
            color: [255, 255, 255],
            box_color: Some([0, 0, 0]),
            box_padding: 12.0,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 1.2], scale: 1.0, rotation_deg: 0.0, opacity: 0.0 }),
            Keyframe::new(0.4, OverlayState { pos: [0.5, 0.7], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.7], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }),
        ],
    },
    TitleTemplate {
        name: "Tilt Pop",
        icon: "\u{1F525}",
        description: "Tilted, bouncy meme caption",
        default_text: "\u{0418}\u{041C}\u{0411}\u{0410}!!!",
        style: || TextStyle {
            font_size: 88.0,
            color: [255, 60, 60],
            box_color: None,
            outline: Some([255, 255, 255]),
            outline_width: 5.0,
            bold: true,
            ..Default::default()
        },
        layout: |dur| vec![
            Keyframe::new(0.0, OverlayState { pos: [0.5, 0.3], scale: 0.0, rotation_deg: -15.0, opacity: 0.0 }),
            Keyframe::new(0.2, OverlayState { pos: [0.5, 0.3], scale: 1.2, rotation_deg: -8.0, opacity: 1.0 }),
            Keyframe::new(0.4, OverlayState { pos: [0.5, 0.3], scale: 1.0, rotation_deg: -8.0, opacity: 1.0 }),
            Keyframe::new(dur, OverlayState { pos: [0.5, 0.3], scale: 1.0, rotation_deg: -8.0, opacity: 1.0 }),
        ],
    },
];

pub fn add_template_to_scene(
    scene: &mut memstroy_core::Scene,
    template: &TitleTemplate,
    t_in: f32,
    t_out: f32,
) -> usize {
    let dur = t_out - t_in;
    let overlay = memstroy_core::Overlay::Text(TextOverlay {
        id: format!(
            "{}_text_{}",
            template.name.to_lowercase().replace(' ', "_"),
            scene.overlays.len()
        ),
        text: template.default_text.to_string(),
        t_in,
        t_out,
        style: (template.style)(),
        layout: (template.layout)(dur),
        z_index: 100,
        behind_actors: false,
    });
    scene.overlays.push(overlay);
    scene.overlays.len() - 1
}
