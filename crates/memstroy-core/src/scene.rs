use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::anchor::AnchorPoint;
use crate::keyframe::Keyframe;

/// Top-level scene description. Saved as `*.scene.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub output: OutputSpec,
    #[serde(default)]
    pub backgrounds: Vec<Background>,
    #[serde(default)]
    pub camera: Vec<Keyframe<CameraState>>,
    #[serde(default)]
    pub actors: Vec<Actor>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
}

fn default_format_version() -> u32 { 1 }

impl Default for Scene {
    fn default() -> Self {
        Self {
            format_version: 1,
            output: OutputSpec::default(),
            backgrounds: Vec::new(),
            camera: Vec::new(),
            actors: Vec::new(),
            overlays: Vec::new(),
            audio: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    /// `[width, height]` in pixels. Vertical 1080x1920 by default for
    /// Shorts/Reels/TikTok.
    pub resolution: [u32; 2],
    pub fps: u32,
    /// Total length of the meme in seconds.
    pub duration: f32,
    #[serde(default)]
    pub background_color: [u8; 3],
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            resolution: [1080, 1920],
            fps: 60,
            duration: 8.0,
            background_color: [255, 255, 255],
        }
    }
}

/// One background segment in the timeline. Backgrounds play sequentially
/// (later segments cut/transition over earlier ones once `start + duration`
/// elapses for the previous segment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Background {
    pub id: String,
    pub source: MediaSource,
    /// Time at which this segment starts (seconds).
    #[serde(default)]
    pub start: f32,
    /// How long this segment is shown (seconds).
    pub duration: f32,
    #[serde(default)]
    pub fit: Fit,
    /// Transition into this segment from the previous one.
    #[serde(default)]
    pub transition: Transition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    Image { path: PathBuf },
    Video { path: PathBuf, #[serde(default)] r#loop: bool, #[serde(default)] start_at: f32 },
    SolidColor { color: [u8; 3] },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Cover the full frame, may crop.
    #[default]
    Cover,
    /// Fit entirely, may letterbox.
    Contain,
    /// 1:1 pixel mapping at the centre.
    Original,
    /// Stretch to fill, may distort.
    Stretch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    #[default]
    Cut,
    /// Hard snap with a 1-2 frame flash; meme-style "punch" cut.
    Snap,
    Fade,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
}

/// Camera transform applied uniformly to the background composite.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CameraState {
    pub zoom: f32,
    /// Centre of attention in normalised [0, 1] coordinates over the
    /// composite background canvas.
    pub center: [f32; 2],
    #[serde(default)]
    pub rotation_deg: f32,
}

impl Default for CameraState {
    fn default() -> Self {
        Self { zoom: 1.0, center: [0.5, 0.5], rotation_deg: 0.0 }
    }
}

impl crate::keyframe::Lerp for CameraState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            zoom: self.zoom.lerp(&other.zoom, t),
            center: self.center.lerp(&other.center, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
        }
    }
}

/// A "Mellstroy" actor: a chroma-keyed source clip plus animation tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    /// Chroma-keyed source video (raw clip downloaded from TG).
    pub source: PathBuf,
    /// Optional path to pre-computed pose anchors JSON (`AnchorTrack`).
    #[serde(default)]
    pub anchors: Option<PathBuf>,
    #[serde(default)]
    pub chroma_key: ChromaKeyParams,
    /// `[t, state]` keyframes for layout (position/scale/rotation).
    #[serde(default)]
    pub layout: Vec<Keyframe<ActorState>>,
    /// Time-window during which the actor is visible. `None` = whole scene.
    #[serde(default)]
    pub t_in: Option<f32>,
    #[serde(default)]
    pub t_out: Option<f32>,
    /// Where to start playing from inside the source clip (seconds).
    #[serde(default)]
    pub source_start: f32,
    /// Whether to loop the source clip if the visible window is longer.
    #[serde(default)]
    pub loop_source: bool,
    #[serde(default)]
    pub flip_horizontal: bool,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ActorState {
    /// Position of the actor anchor (typically body center) in normalised
    /// scene coordinates.
    pub pos: [f32; 2],
    pub scale: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    #[serde(default = "one")]
    pub opacity: f32,
}

fn one() -> f32 { 1.0 }

impl Default for ActorState {
    fn default() -> Self {
        Self { pos: [0.5, 0.7], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }
    }
}

impl crate::keyframe::Lerp for ActorState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            scale: self.scale.lerp(&other.scale, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
            opacity: self.opacity.lerp(&other.opacity, t),
        }
    }
}

/// Chroma-keying parameters for actor source video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChromaKeyParams {
    /// Target colour in RGB. Defaults to a Telegram-channel green.
    pub key_color: [u8; 3],
    /// Hue tolerance in degrees [0, 180]. Higher = more permissive.
    pub similarity: f32,
    /// Edge softness [0, 1].
    pub blend: f32,
    /// Spill suppression strength [0, 1].
    pub spill: f32,
}

impl Default for ChromaKeyParams {
    fn default() -> Self {
        Self {
            key_color: [0, 177, 64], // standard chroma green
            similarity: 0.20,
            blend: 0.10,
            spill: 0.30,
        }
    }
}

/// A prop attached to an actor (cap, glasses, weapon, etc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub asset: PathBuf,
    pub anchor: AnchorPoint,
    /// Pixel-space offset relative to the anchor, in actor-local frame.
    #[serde(default)]
    pub offset: [f32; 2],
    /// Scale multiplier applied on top of the actor's scale.
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    /// If true, the prop rotates to follow shoulder line / limb angle.
    #[serde(default)]
    pub follow_rotation: bool,
    #[serde(default)]
    pub z_above_actor: bool,
}

/// Anything drawn on top of (or below) actors but above background:
/// text, image, sub-video, sticker animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Overlay {
    Text(TextOverlay),
    Image(ImageOverlay),
    Video(VideoOverlay),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextOverlay {
    pub id: String,
    pub text: String,
    /// Visible window.
    pub t_in: f32,
    pub t_out: f32,
    #[serde(default)]
    pub style: TextStyle,
    /// Position keyframes; if a single keyframe is provided it stays still.
    pub layout: Vec<Keyframe<OverlayState>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextStyle {
    /// Logical font family. Falls back to bundled DejaVu if unknown.
    #[serde(default = "default_font")]
    pub font: String,
    pub font_size: f32,
    pub color: [u8; 3],
    /// White meme box behind the text. None = transparent text only.
    #[serde(default)]
    pub box_color: Option<[u8; 3]>,
    /// Padding inside the box, in pixels.
    #[serde(default)]
    pub box_padding: f32,
    #[serde(default)]
    pub bold: bool,
    /// Outline colour for stroke around the glyphs.
    #[serde(default)]
    pub outline: Option<[u8; 3]>,
    #[serde(default)]
    pub outline_width: f32,
    #[serde(default)]
    pub align: TextAlign,
}

fn default_font() -> String { "DejaVuSans".into() }

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: 96.0,
            color: [0, 0, 0],
            box_color: Some([255, 255, 255]),
            box_padding: 24.0,
            bold: true,
            outline: None,
            outline_width: 0.0,
            align: TextAlign::Center,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign { Left, #[default] Center, Right }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageOverlay {
    pub id: String,
    pub source: PathBuf,
    pub t_in: f32,
    pub t_out: f32,
    pub layout: Vec<Keyframe<OverlayState>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoOverlay {
    pub id: String,
    pub source: PathBuf,
    pub t_in: f32,
    pub t_out: f32,
    #[serde(default)]
    pub source_start: f32,
    #[serde(default)]
    pub loop_source: bool,
    #[serde(default)]
    pub chroma_key: Option<ChromaKeyParams>,
    pub layout: Vec<Keyframe<OverlayState>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct OverlayState {
    /// Centre position in normalised scene coordinates [0, 1].
    pub pos: [f32; 2],
    pub scale: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    #[serde(default = "one")]
    pub opacity: f32,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self { pos: [0.5, 0.5], scale: 1.0, rotation_deg: 0.0, opacity: 1.0 }
    }
}

impl crate::keyframe::Lerp for OverlayState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            scale: self.scale.lerp(&other.scale, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
            opacity: self.opacity.lerp(&other.opacity, t),
        }
    }
}

/// Audio track on the timeline (background music, voice-over, sfx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrack {
    pub id: String,
    pub source: PathBuf,
    #[serde(default)]
    pub t_in: f32,
    #[serde(default)]
    pub t_out: Option<f32>,
    #[serde(default)]
    pub source_start: f32,
    #[serde(default = "one")]
    pub volume: f32,
}
