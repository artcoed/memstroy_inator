//! Free Canvas model — world-pixel coordinate system.
//!
//! The canvas is an infinite 2D plane where elements are positioned in
//! world pixels. The **RenderFrame** is a rectangular region on this
//! canvas (default 1080x1920) whose contents become the final output.
//! It can be moved, scaled, and rotated over time via keyframes — this
//! replaces the old `camera` array.
//!
//! The editor viewport (what the user sees) is a separate camera that
//! pans/zooms over the canvas for editing convenience; it does NOT
//! affect the rendered output.

use serde::{Deserialize, Serialize};

use crate::keyframe::{Keyframe, Lerp};

// ─── WORLD-PIXEL POSITION ────────────────────────────────────────────

/// A position on the canvas in world pixels.
/// Origin (0, 0) is arbitrary; the render frame defines what ends up
/// in the output.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldPos {
    pub x: f32,
    pub y: f32,
}

impl Default for WorldPos {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

impl Lerp for WorldPos {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            x: self.x + (other.x - self.x) * t,
            y: self.y + (other.y - self.y) * t,
        }
    }
}

// ─── CANVAS ELEMENT STATE ────────────────────────────────────────────

/// Transform state for any element on the canvas (actor, overlay, image).
/// All coordinates are in world pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CanvasTransform {
    /// Centre position in world pixels.
    pub pos: WorldPos,
    /// Width in world pixels (height is derived from aspect ratio).
    pub width: f32,
    /// Uniform scale multiplier applied on top of width.
    #[serde(default = "one")]
    pub scale: f32,
    /// Rotation in degrees.
    #[serde(default)]
    pub rotation_deg: f32,
    /// Opacity [0, 1].
    #[serde(default = "one")]
    pub opacity: f32,
}

fn one() -> f32 { 1.0 }

impl Default for CanvasTransform {
    fn default() -> Self {
        Self {
            pos: WorldPos::default(),
            width: 500.0,
            scale: 1.0,
            rotation_deg: 0.0,
            opacity: 1.0,
        }
    }
}

impl Lerp for CanvasTransform {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            width: self.width.lerp(&other.width, t),
            scale: self.scale.lerp(&other.scale, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
            opacity: self.opacity.lerp(&other.opacity, t),
        }
    }
}

// ─── RENDER FRAME ────────────────────────────────────────────────────

/// The RenderFrame defines the output region on the canvas.
/// Everything inside this rectangle at each point in time ends up in
/// the final rendered video. It is animatable just like any other element.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderFrame {
    /// Output resolution in pixels (width, height). This is the final
    /// file resolution (e.g. 1080x1920 for vertical video).
    pub resolution: [u32; 2],
    /// Keyframed state of the render frame on the canvas.
    /// `pos` = centre of the frame in world pixels.
    /// `width` is ignored here (driven by resolution + zoom).
    /// `scale` acts as inverse zoom: scale=2 means the frame covers
    /// a 2x larger area of the canvas (zoom out).
    pub layout: Vec<Keyframe<RenderFrameState>>,
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            resolution: [1080, 1920],
            layout: vec![Keyframe::new(0.0, RenderFrameState::default())],
        }
    }
}

/// Per-keyframe state of the render frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RenderFrameState {
    /// Centre position in world pixels.
    pub pos: WorldPos,
    /// Zoom level. 1.0 = 1:1 mapping (1 canvas pixel = 1 output pixel).
    /// >1.0 = zoomed in (less canvas area shown). <1.0 = zoomed out.
    #[serde(default = "one")]
    pub zoom: f32,
    /// Rotation of the frame in degrees.
    #[serde(default)]
    pub rotation_deg: f32,
}

impl Default for RenderFrameState {
    fn default() -> Self {
        Self {
            pos: WorldPos { x: 540.0, y: 960.0 },
            zoom: 1.0,
            rotation_deg: 0.0,
        }
    }
}

impl Lerp for RenderFrameState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            pos: self.pos.lerp(&other.pos, t),
            zoom: self.zoom.lerp(&other.zoom, t),
            rotation_deg: self.rotation_deg.lerp(&other.rotation_deg, t),
        }
    }
}

// ─── EDITOR VIEWPORT ─────────────────────────────────────────────────

/// The editor's viewport camera. Controls what portion of the infinite
/// canvas is visible in the preview panel. Does NOT affect rendering.
#[derive(Debug, Clone, Copy)]
pub struct EditorViewport {
    /// Centre of the viewport in world pixels.
    pub center: WorldPos,
    /// Zoom level (screen pixels per world pixel).
    /// 1.0 = 1:1. 0.5 = zoomed out (see more canvas). 2.0 = zoomed in.
    pub zoom: f32,
}

impl Default for EditorViewport {
    fn default() -> Self {
        Self {
            center: WorldPos { x: 540.0, y: 960.0 },
            zoom: 0.5, // start zoomed out so the full 1080x1920 frame fits
        }
    }
}

impl EditorViewport {
    /// Convert a world-pixel position to screen-pixel position relative
    /// to the viewport's top-left corner.
    /// `viewport_size` is the size of the preview panel in screen pixels.
    pub fn world_to_screen(&self, world: WorldPos, viewport_size: [f32; 2]) -> [f32; 2] {
        let dx = (world.x - self.center.x) * self.zoom;
        let dy = (world.y - self.center.y) * self.zoom;
        [
            viewport_size[0] * 0.5 + dx,
            viewport_size[1] * 0.5 + dy,
        ]
    }

    /// Convert a screen-pixel position (relative to viewport top-left)
    /// back to world-pixel coordinates.
    pub fn screen_to_world(&self, screen: [f32; 2], viewport_size: [f32; 2]) -> WorldPos {
        let dx = (screen[0] - viewport_size[0] * 0.5) / self.zoom;
        let dy = (screen[1] - viewport_size[1] * 0.5) / self.zoom;
        WorldPos {
            x: self.center.x + dx,
            y: self.center.y + dy,
        }
    }

    /// Size of the visible world area in world pixels.
    pub fn visible_world_size(&self, viewport_size: [f32; 2]) -> [f32; 2] {
        [
            viewport_size[0] / self.zoom,
            viewport_size[1] / self.zoom,
        ]
    }

    /// Pan the viewport by a screen-pixel delta.
    pub fn pan(&mut self, screen_delta: [f32; 2]) {
        self.center.x -= screen_delta[0] / self.zoom;
        self.center.y -= screen_delta[1] / self.zoom;
    }

    /// Zoom towards a screen-pixel point (keeps that point stationary).
    pub fn zoom_at(&mut self, screen_pos: [f32; 2], viewport_size: [f32; 2], factor: f32) {
        let world_before = self.screen_to_world(screen_pos, viewport_size);
        self.zoom = (self.zoom * factor).clamp(0.01, 50.0);
        let world_after = self.screen_to_world(screen_pos, viewport_size);
        // Adjust center to keep world_before at the same screen position
        self.center.x += world_before.x - world_after.x;
        self.center.y += world_before.y - world_after.y;
    }

    /// Fit the render frame into the viewport with some padding.
    pub fn fit_render_frame(&mut self, frame_center: WorldPos, frame_resolution: [u32; 2], viewport_size: [f32; 2]) {
        self.center = frame_center;
        let aspect_x = viewport_size[0] / frame_resolution[0] as f32;
        let aspect_y = viewport_size[1] / frame_resolution[1] as f32;
        // Use the smaller factor so the whole frame fits, with 10% padding
        self.zoom = aspect_x.min(aspect_y) * 0.85;
    }
}

// ─── COORDINATE CONVERSION HELPERS ───────────────────────────────────

/// Convert legacy normalised coordinates [0,1] to world pixels given
/// a reference rect (position + size). Useful for migration.
pub fn normalized_to_world(norm: [f32; 2], rect_center: WorldPos, rect_size: [f32; 2]) -> WorldPos {
    WorldPos {
        x: rect_center.x + (norm[0] - 0.5) * rect_size[0],
        y: rect_center.y + (norm[1] - 0.5) * rect_size[1],
    }
}

/// Convert world pixels back to normalised coordinates relative to a rect.
pub fn world_to_normalized(world: WorldPos, rect_center: WorldPos, rect_size: [f32; 2]) -> [f32; 2] {
    [
        0.5 + (world.x - rect_center.x) / rect_size[0],
        0.5 + (world.y - rect_center.y) / rect_size[1],
    ]
}
