use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::anchor::AnchorPoint;
use crate::canvas::{CanvasTransform, RenderFrame};
use crate::keyframe::Keyframe;
use crate::skeleton::{SkeletonAttachment, SkeletonTemplate};

// ─── ANIMATED PARAM IDS ──────────────────────────────────────────────
//
// Stable string identifiers for every per-element parameter that the
// inspector exposes as keyframable. Stored in `animated_params` so we
// can keep YAML/JSON serialization stable across renames in the inspector
// UI. When a parameter is in this set, editing it inserts a keyframe at
// the playhead; otherwise, the parameter has a single static value
// broadcast across all keyframes (canvas-first: drag on canvas writes
// the kf at playhead and auto-marks the param as animated).

pub mod param_ids {
    pub const POS_X: &str       = "pos_x";
    pub const POS_Y: &str       = "pos_y";
    pub const SCALE: &str       = "scale";
    pub const SCALE_Y: &str     = "scale_y";
    pub const ROTATION: &str    = "rotation";
    pub const OPACITY: &str     = "opacity";
    pub const FLIP_X: &str      = "flip_x";
    pub const FLIP_Y: &str      = "flip_y";

    /// Effect-stack parameter ids are encoded as `fx_<index>_<sub>` where
    /// `<sub>` is one of: intensity, p0, p1 (effect-specific). The inspector
    /// uses this convention so the same animation system handles them.
    pub fn fx_param(idx: usize, sub: &str) -> String {
        format!("fx_{}_{}", idx, sub)
    }

    /// All transform params for actors / overlays in inspector display order.
    pub const TRANSFORM_PARAMS: &[&str] = &[
        POS_X, POS_Y, SCALE, SCALE_Y, ROTATION, OPACITY, FLIP_X, FLIP_Y,
    ];

    /// Human-readable label for a known param id. Returns the id back as
    /// a fallback for unknown params (e.g. fx_*_*).
    pub fn label(id: &str) -> &'static str {
        match id {
            POS_X => "Position X",
            POS_Y => "Position Y",
            SCALE => "Scale",
            SCALE_Y => "Stretch Y",
            ROTATION => "Rotation",
            OPACITY => "Opacity",
            FLIP_X => "Flip X",
            FLIP_Y => "Flip Y",
            _ => "param",
        }
    }
}

/// Top-level scene description. Saved as `*.scene.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub output: OutputSpec,
    #[serde(default)]
    pub backgrounds: Vec<Background>,
    /// Legacy camera keyframes. Superseded by `render_frame` in v2 scenes
    /// but kept for backward compatibility.
    #[serde(default)]
    pub camera: Vec<Keyframe<CameraState>>,
    #[serde(default)]
    pub actors: Vec<Actor>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
    /// **Free Canvas v2**: The output render frame — a movable/animatable
    /// rectangle on the infinite canvas. Defines what portion of the canvas
    /// ends up in the rendered video. Replaces the old `camera` field.
    #[serde(default)]
    pub render_frame: RenderFrame,
    /// **Free Canvas v2**: Per-actor world-pixel canvas transforms.
    /// When present, these override the legacy normalised `layout` keyframes.
    /// Indexed by actor id → keyframe track.
    #[serde(default)]
    pub canvas_layouts: Vec<CanvasLayout>,
    /// **Skeleton Constructor**: User-defined skeleton templates for clips.
    /// Each template defines named anchor points with per-frame positions.
    #[serde(default)]
    pub skeleton_templates: Vec<SkeletonTemplate>,
}

/// Associates an element (by id) with a canvas-space keyframe track.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasLayout {
    /// ID of the actor/overlay this layout belongs to.
    pub element_id: String,
    /// Keyframes in world-pixel space.
    pub keyframes: Vec<Keyframe<CanvasTransform>>,
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
            render_frame: RenderFrame::default(),
            canvas_layouts: Vec::new(),
            skeleton_templates: Vec::new(),
        }
    }
}

impl Scene {
    /// Inspect every actor / overlay layout and back-fill its
    /// `animated_params` set so that any parameter that actually varies
    /// across keyframes is marked as animated. Used at load-time for
    /// scenes saved before the per-param toggle existed: it preserves
    /// their animations so editing a static param later doesn't broadcast
    /// over previously-animated motion.
    pub fn backfill_animated_params(&mut self) {
        for a in &mut self.actors {
            backfill_actor(a);
        }
        for ov in &mut self.overlays {
            match ov {
                Overlay::Text(t) => backfill_overlay_text(t),
                Overlay::Image(im) => backfill_overlay_image(im),
                Overlay::Video(v) => backfill_overlay_video(v),
            }
        }
    }
}

fn varies(values: impl IntoIterator<Item = f32>) -> bool {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else { return false };
    iter.any(|v| (v - first).abs() > 1.0e-4)
}

fn backfill_actor(a: &mut Actor) {
    if a.layout.len() < 2 {
        return;
    }
    use param_ids::*;
    let mark = |s: &mut BTreeSet<String>, id: &str, vary: bool| {
        if vary {
            s.insert(id.to_string());
        }
    };
    let v = &a.layout;
    mark(&mut a.animated_params, POS_X,
         varies(v.iter().map(|kf| kf.value.pos[0])));
    mark(&mut a.animated_params, POS_Y,
         varies(v.iter().map(|kf| kf.value.pos[1])));
    mark(&mut a.animated_params, SCALE,
         varies(v.iter().map(|kf| kf.value.scale)));
    mark(&mut a.animated_params, SCALE_Y,
         varies(v.iter().map(|kf| kf.value.scale_y)));
    mark(&mut a.animated_params, ROTATION,
         varies(v.iter().map(|kf| kf.value.rotation_deg)));
    mark(&mut a.animated_params, OPACITY,
         varies(v.iter().map(|kf| kf.value.opacity)));
    mark(&mut a.animated_params, FLIP_X,
         varies(v.iter().map(|kf| kf.value.flip_x_anim)));
    mark(&mut a.animated_params, FLIP_Y,
         varies(v.iter().map(|kf| kf.value.flip_y_anim)));
}

fn backfill_overlay_image(o: &mut ImageOverlay) {
    backfill_overlay_layout(&o.layout, &mut o.animated_params);
}
fn backfill_overlay_video(o: &mut VideoOverlay) {
    backfill_overlay_layout(&o.layout, &mut o.animated_params);
}
fn backfill_overlay_text(o: &mut TextOverlay) {
    backfill_overlay_layout(&o.layout, &mut o.animated_params);
}

fn backfill_overlay_layout(layout: &[Keyframe<OverlayState>], set: &mut BTreeSet<String>) {
    if layout.len() < 2 {
        return;
    }
    use param_ids::*;
    let mark = |s: &mut BTreeSet<String>, id: &str, vary: bool| {
        if vary {
            s.insert(id.to_string());
        }
    };
    mark(set, POS_X,    varies(layout.iter().map(|kf| kf.value.pos[0])));
    mark(set, POS_Y,    varies(layout.iter().map(|kf| kf.value.pos[1])));
    mark(set, SCALE,    varies(layout.iter().map(|kf| kf.value.scale)));
    mark(set, SCALE_Y,  varies(layout.iter().map(|kf| kf.value.scale_y)));
    mark(set, ROTATION, varies(layout.iter().map(|kf| kf.value.rotation_deg)));
    mark(set, OPACITY,  varies(layout.iter().map(|kf| kf.value.opacity)));
    mark(set, FLIP_X,   varies(layout.iter().map(|kf| kf.value.flip_x_anim)));
    mark(set, FLIP_Y,   varies(layout.iter().map(|kf| kf.value.flip_y_anim)));
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
    /// **Skeleton Constructor**: Elements attached to skeleton points on this actor.
    /// Each entry binds an external element to a named point from a SkeletonTemplate.
    #[serde(default)]
    pub skeleton_attachments: Vec<SkeletonAttachment>,
    /// **Animation modifiers**: layered perturbations (wobble/shake/pulse/spin)
    /// applied on top of the keyframe-sampled state. Empty by default.
    #[serde(default)]
    pub modifiers: Vec<crate::keyframe::TrackModifier>,
    /// Whether this actor is visible in preview. Defaults to true.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Color correction parameters for this actor.
    #[serde(default)]
    pub color_correction: ColorCorrection,
    /// Transition applied when the actor enters its visible window.
    #[serde(default)]
    pub transition_in: Transition,
    /// Transition applied when the actor leaves its visible window.
    #[serde(default)]
    pub transition_out: Transition,
    /// Duration of the in/out transitions in seconds.
    #[serde(default = "default_transition_duration")]
    pub transition_duration: f32,
    /// **Effects stack**: ordered list of post-processing effects (blur,
    /// glow, hue shift, …) applied on top of chroma key + colour
    /// correction. The user can stack arbitrarily many of them and
    /// re-order via the inspector.
    #[serde(default)]
    pub effects: Vec<crate::effects::Effect>,
    /// **Animated parameter set**: when a param id is in this set, editing
    /// the param in the inspector or on the canvas inserts a keyframe at
    /// the playhead; otherwise the new value is broadcast to every kf in
    /// `layout` (single static value). Empty for fresh actors.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub animated_params: BTreeSet<String>,
}

fn default_true() -> bool { true }
fn default_transition_duration() -> f32 { 0.3 }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ActorState {
    /// Position of the actor anchor (typically body center) in normalised
    /// scene coordinates.
    pub pos: [f32; 2],
    pub scale: f32,
    /// Y-axis stretch factor multiplied on top of `scale`. Default 1.0 means
    /// uniform scaling. Values != 1.0 produce a non-proportional stretch.
    #[serde(default = "one")]
    pub scale_y: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    #[serde(default = "one")]
    pub opacity: f32,
    /// Animatable horizontal flip. Range −1.0..=1.0 (default 1.0).
    /// At 1 the element is upright; at −1 it is fully mirrored. Values
    /// in between produce a 3D-like "card-fold" effect because the
    /// renderer squashes horizontal scale by `|flip_x_anim|` so the
    /// element appears to rotate around the Y axis.
    #[serde(default = "one")]
    pub flip_x_anim: f32,
    /// Animatable vertical flip. Same semantics as `flip_x_anim` but
    /// around the X axis.
    #[serde(default = "one")]
    pub flip_y_anim: f32,
}

fn one() -> f32 { 1.0 }

impl Default for ActorState {
    fn default() -> Self {
        Self {
            pos: [0.5, 0.7],
            scale: 1.0,
            scale_y: 1.0,
            rotation_deg: 0.0,
            opacity: 1.0,
            flip_x_anim: 1.0,
            flip_y_anim: 1.0,
        }
    }
}

impl crate::keyframe::Lerp for ActorState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            scale: self.scale.lerp(&other.scale, t),
            scale_y: self.scale_y.lerp(&other.scale_y, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
            opacity: self.opacity.lerp(&other.opacity, t),
            flip_x_anim: self.flip_x_anim.lerp(&other.flip_x_anim, t),
            flip_y_anim: self.flip_y_anim.lerp(&other.flip_y_anim, t),
        }
    }
}

/// Color correction parameters applied to an actor or overlay.
///
/// Two layers of controls:
/// 1. **Quick**: brightness / contrast / saturation / temperature — for
///    low-effort tweaks (also kept for backward compatibility with older
///    project files).
/// 2. **Pro** (DaVinci-style):
///    - `lift`  — per-RGB shadow offset (neutral = 0).
///    - `gamma` — per-RGB midtone gamma (neutral = 1).
///    - `gain`  — per-RGB highlight gain (neutral = 1).
///    - `curves` — master + per-channel tone curves (sorted control points).
///
/// Apply order (matches the resolve / NLE convention):
///   1. brightness/contrast/saturation/temperature (legacy block)
///   2. lift → gain → gamma per channel
///   3. master curve, then R / G / B curves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorCorrection {
    #[serde(default)]
    pub brightness: f32,
    #[serde(default = "one")]
    pub contrast: f32,
    #[serde(default = "one")]
    pub saturation: f32,
    #[serde(default)]
    pub temperature: f32,

    /// Per-RGB shadow offset. Neutral = `[0, 0, 0]`. Range typically `[-0.5, 0.5]`.
    #[serde(default = "default_lift")]
    pub lift: [f32; 3],
    /// Per-RGB midtone gamma. Neutral = `[1, 1, 1]`. Range typically `[0.2, 4.0]`.
    #[serde(default = "default_gamma")]
    pub gamma: [f32; 3],
    /// Per-RGB highlight gain. Neutral = `[1, 1, 1]`. Range typically `[0.0, 4.0]`.
    #[serde(default = "default_gain")]
    pub gain: [f32; 3],

    /// Master + per-channel tone curves. Neutral curve = `[(0,0), (1,1)]`.
    #[serde(default)]
    pub curves: ToneCurves,
}

fn default_lift() -> [f32; 3] { [0.0, 0.0, 0.0] }
fn default_gamma() -> [f32; 3] { [1.0, 1.0, 1.0] }
fn default_gain() -> [f32; 3] { [1.0, 1.0, 1.0] }

/// Master + per-channel tone curves. Each curve is a list of `[input, output]`
/// control points in 0..1, sorted by input. The endpoints (`x=0` and `x=1`)
/// are always present; intermediate points can be added/removed in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToneCurves {
    #[serde(default = "identity_curve")]
    pub master: Vec<[f32; 2]>,
    #[serde(default = "identity_curve")]
    pub red: Vec<[f32; 2]>,
    #[serde(default = "identity_curve")]
    pub green: Vec<[f32; 2]>,
    #[serde(default = "identity_curve")]
    pub blue: Vec<[f32; 2]>,
}

fn identity_curve() -> Vec<[f32; 2]> { vec![[0.0, 0.0], [1.0, 1.0]] }

impl Default for ToneCurves {
    fn default() -> Self {
        Self {
            master: identity_curve(),
            red: identity_curve(),
            green: identity_curve(),
            blue: identity_curve(),
        }
    }
}

impl ToneCurves {
    /// Whether all four curves are at the identity `(0,0)–(1,1)`.
    pub fn is_identity(&self) -> bool {
        is_identity_curve(&self.master)
            && is_identity_curve(&self.red)
            && is_identity_curve(&self.green)
            && is_identity_curve(&self.blue)
    }

    /// Sample a curve at `x` in 0..1 using piecewise-linear interpolation
    /// across its control points. Out-of-range x clamps to the endpoint y.
    pub fn sample(curve: &[[f32; 2]], x: f32) -> f32 {
        if curve.is_empty() { return x; }
        if curve.len() == 1 { return curve[0][1]; }
        if x <= curve[0][0] { return curve[0][1]; }
        if x >= curve[curve.len() - 1][0] { return curve[curve.len() - 1][1]; }
        for w in curve.windows(2) {
            if x >= w[0][0] && x <= w[1][0] {
                let span = w[1][0] - w[0][0];
                if span < 1e-6 { return w[0][1]; }
                let t = (x - w[0][0]) / span;
                return w[0][1] + (w[1][1] - w[0][1]) * t;
            }
        }
        x
    }
}

fn is_identity_curve(c: &[[f32; 2]]) -> bool {
    c.len() == 2
        && (c[0][0] - 0.0).abs() < 1e-4 && (c[0][1] - 0.0).abs() < 1e-4
        && (c[1][0] - 1.0).abs() < 1e-4 && (c[1][1] - 1.0).abs() < 1e-4
}

impl Default for ColorCorrection {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            temperature: 0.0,
            lift: [0.0; 3],
            gamma: [1.0; 3],
            gain: [1.0; 3],
            curves: ToneCurves::default(),
        }
    }
}

impl ColorCorrection {
    /// Whether every parameter is at its neutral / identity value, i.e. this
    /// correction would be a no-op. Used as a fast-path so untouched clips
    /// skip the full CPU correction pipeline entirely.
    pub fn is_identity(&self) -> bool {
        self.brightness.abs() < 1e-4
            && (self.contrast - 1.0).abs() < 1e-4
            && (self.saturation - 1.0).abs() < 1e-4
            && self.temperature.abs() < 1e-4
            && self.lift.iter().all(|v| v.abs() < 1e-4)
            && self.gamma.iter().all(|v| (v - 1.0).abs() < 1e-4)
            && self.gain.iter().all(|v| (v - 1.0).abs() < 1e-4)
            && self.curves.is_identity()
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

impl ChromaKeyParams {
    /// Path of the per-clip chroma sidecar file (`<clip>.chroma.json`).
    pub fn sidecar_path(clip_path: &std::path::Path) -> std::path::PathBuf {
        clip_path.with_extension("chroma.json")
    }

    /// Save these chroma settings as JSON next to the source clip so they
    /// follow the asset across projects.
    pub fn save_alongside_clip(&self, clip_path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        let path = Self::sidecar_path(clip_path);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Load chroma settings from `<clip>.chroma.json`, if present.
    pub fn load_for_clip(clip_path: &std::path::Path) -> Option<Self> {
        let path = Self::sidecar_path(clip_path);
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
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
    /// **Animation modifiers**: layered perturbations.
    #[serde(default)]
    pub modifiers: Vec<crate::keyframe::TrackModifier>,
    /// **Skeleton attachment**: when set, this overlay's screen position
    /// is locked to a named point of an actor's skeleton template.
    #[serde(default)]
    pub skeleton_attachment: Option<SkeletonAttachment>,
    /// Stacking order among overlays of the same actor-relation.
    /// Higher values draw on top of lower ones. Default 100.
    #[serde(default = "default_text_z")]
    pub z_index: i32,
    /// When true, this text is drawn UNDER the actors (between background
    /// and actors). When false (default), it draws on top of actors.
    #[serde(default)]
    pub behind_actors: bool,
    /// **Effects stack**: same layered post-processing system as actors.
    #[serde(default)]
    pub effects: Vec<crate::effects::Effect>,
    /// **Animated parameter set** — see Actor::animated_params.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub animated_params: BTreeSet<String>,
}

fn default_text_z() -> i32 { 100 }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextBoxKind {
    /// No background plate.
    None,
    /// Solid filled rectangle (default).
    Solid,
    /// Two-color vertical gradient.
    Gradient,
    /// Only an outlined rectangle, no fill.
    OutlineOnly,
}

impl Default for TextBoxKind {
    fn default() -> Self { TextBoxKind::Solid }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextStyle {
    /// Logical font family. Falls back to bundled DejaVu if unknown.
    #[serde(default = "default_font")]
    pub font: String,
    pub font_size: f32,
    pub color: [u8; 3],
    /// Solid plate colour. `None` = transparent text only.
    #[serde(default)]
    pub box_color: Option<[u8; 3]>,
    /// Padding inside the box, in pixels.
    #[serde(default)]
    pub box_padding: f32,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    /// Outline colour for stroke around the glyphs.
    #[serde(default)]
    pub outline: Option<[u8; 3]>,
    #[serde(default)]
    pub outline_width: f32,
    #[serde(default)]
    pub align: TextAlign,

    // ─── Background plate styling ────────────────────────────────────
    /// What kind of background plate to draw. Only used when `box_color`
    /// is `Some(_)` — keeps backward compatibility with legacy scenes.
    #[serde(default)]
    pub box_kind: TextBoxKind,
    /// Corner radius of the background plate, in pixels.
    #[serde(default)]
    pub box_corner_radius: f32,
    /// Plate opacity (0.0 .. 1.0). Defaults to 1.0.
    #[serde(default = "one")]
    pub box_opacity: f32,
    /// When `box_kind = Gradient`, the second (bottom) colour of the gradient.
    #[serde(default)]
    pub box_gradient_end: Option<[u8; 3]>,
    /// Plate outline colour (used by `Solid+border` and `OutlineOnly`).
    #[serde(default)]
    pub box_outline_color: Option<[u8; 3]>,
    /// Plate outline width in pixels.
    #[serde(default)]
    pub box_outline_width: f32,
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
            italic: false,
            outline: None,
            outline_width: 0.0,
            align: TextAlign::Center,
            box_kind: TextBoxKind::Solid,
            box_corner_radius: 8.0,
            box_opacity: 1.0,
            box_gradient_end: None,
            box_outline_color: None,
            box_outline_width: 0.0,
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
    /// **Animation modifiers**: layered perturbations.
    #[serde(default)]
    pub modifiers: Vec<crate::keyframe::TrackModifier>,
    /// **Skeleton attachment**: when set, this overlay's screen position
    /// is locked to a named point of an actor's skeleton template.
    #[serde(default)]
    pub skeleton_attachment: Option<SkeletonAttachment>,
    /// **Effects stack**.
    #[serde(default)]
    pub effects: Vec<crate::effects::Effect>,
    /// **Animated parameter set** — see Actor::animated_params.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub animated_params: BTreeSet<String>,
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
    /// **Animation modifiers**: layered perturbations.
    #[serde(default)]
    pub modifiers: Vec<crate::keyframe::TrackModifier>,
    /// **Skeleton attachment**: when set, this overlay's screen position
    /// is locked to a named point of an actor's skeleton template.
    #[serde(default)]
    pub skeleton_attachment: Option<SkeletonAttachment>,
    /// **Effects stack**.
    #[serde(default)]
    pub effects: Vec<crate::effects::Effect>,
    /// **Animated parameter set** — see Actor::animated_params.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub animated_params: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct OverlayState {
    /// Centre position in normalised scene coordinates [0, 1].
    pub pos: [f32; 2],
    pub scale: f32,
    /// Y-axis stretch factor multiplied on top of `scale`. Default 1.0 means
    /// uniform scaling.
    #[serde(default = "one")]
    pub scale_y: f32,
    #[serde(default)]
    pub rotation_deg: f32,
    #[serde(default = "one")]
    pub opacity: f32,
    /// Animatable horizontal flip (3D fold). See `ActorState::flip_x_anim`.
    #[serde(default = "one")]
    pub flip_x_anim: f32,
    /// Animatable vertical flip (3D fold). See `ActorState::flip_y_anim`.
    #[serde(default = "one")]
    pub flip_y_anim: f32,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            pos: [0.5, 0.5],
            scale: 1.0,
            scale_y: 1.0,
            rotation_deg: 0.0,
            opacity: 1.0,
            flip_x_anim: 1.0,
            flip_y_anim: 1.0,
        }
    }
}

impl crate::keyframe::Lerp for OverlayState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            scale: self.scale.lerp(&other.scale, t),
            scale_y: self.scale_y.lerp(&other.scale_y, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
            opacity: self.opacity.lerp(&other.opacity, t),
            flip_x_anim: self.flip_x_anim.lerp(&other.flip_x_anim, t),
            flip_y_anim: self.flip_y_anim.lerp(&other.flip_y_anim, t),
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
    /// Playback rate multiplier (1.0 = normal). Applied via rodio's
    /// `Source::speed` when the sink is built. Affects pitch as well as
    /// duration (i.e. classic "scrub speed", not time-stretch).
    #[serde(default = "one")]
    pub speed: f32,
    /// If set, this audio track belongs to the actor with this `id`. The
    /// editor uses this to keep clip & audio in lock-step: moving / trimming
    /// / deleting the actor mirrors the same change on the bound audio so
    /// they always export together.
    #[serde(default)]
    pub parent_actor: Option<String>,
}
