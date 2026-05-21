//! Skeleton Constructor — user-defined named anchor points on clips.
//!
//! A `SkeletonTemplate` is a set of named points (e.g. "hat", "hand",
//! "logo_spot") that the user manually places frame-by-frame on a
//! specific video clip. Each point has a keyframe track of positions
//! (normalised [0,1] within the clip's frame). Between keyframes the
//! position is interpolated with the chosen easing.
//!
//! Once a skeleton is defined for a clip, any scene element (image,
//! text, another clip) can be "attached" to one of its points —
//! the attached element will follow the point's position over time.
//!
//! Skeletons are saved as `<clip_name>.skeleton.json` alongside the
//! source video, or embedded in the scene file under `skeleton_templates`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::easing::Easing;
use crate::keyframe::{Keyframe, Lerp};

// ─── SKELETON TEMPLATE ───────────────────────────────────────────────

/// A skeleton template for a specific video clip.
/// Contains named anchor points, each with a keyframe track of positions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonTemplate {
    /// Human-readable name for this skeleton (e.g. "mellstroy_919_skeleton").
    pub name: String,
    /// Path to the source clip this skeleton was created for.
    /// Used to match the skeleton to an actor in the scene.
    pub source_clip: PathBuf,
    /// FPS at which positions were sampled/keyframed.
    /// Used to convert frame indices to time values.
    #[serde(default = "default_fps")]
    pub fps: f32,
    /// Total duration of the source clip in seconds.
    #[serde(default)]
    pub clip_duration: f32,
    /// Named anchor points, each with its own keyframe track.
    pub points: BTreeMap<String, SkeletonPoint>,
}

fn default_fps() -> f32 { 30.0 }

impl Default for SkeletonTemplate {
    fn default() -> Self {
        Self {
            name: String::new(),
            source_clip: PathBuf::new(),
            fps: 30.0,
            clip_duration: 0.0,
            points: BTreeMap::new(),
        }
    }
}

/// A single named anchor point in a skeleton.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonPoint {
    /// User-given name (e.g. "hat", "left_hand", "badge_spot").
    pub name: String,
    /// Optional color for visual distinction in the editor (RGB).
    #[serde(default = "default_point_color")]
    pub color: [u8; 3],
    /// Keyframe track: position over time in normalised clip coords [0,1].
    pub track: Vec<Keyframe<PointState>>,
}

fn default_point_color() -> [u8; 3] { [255, 100, 100] }

impl Default for SkeletonPoint {
    fn default() -> Self {
        Self {
            name: String::new(),
            color: default_point_color(),
            track: Vec::new(),
        }
    }
}

/// Per-keyframe state of a skeleton point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointState {
    /// Position in normalised clip coordinates [0,1].
    /// (0,0) = top-left, (1,1) = bottom-right of the video frame.
    pub x: f32,
    pub y: f32,
    /// Optional scale hint (for attached elements). Default 1.0.
    #[serde(default = "one")]
    pub scale: f32,
    /// Optional rotation hint in degrees.
    #[serde(default)]
    pub rotation_deg: f32,
}

fn one() -> f32 { 1.0 }

impl Default for PointState {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5, scale: 1.0, rotation_deg: 0.0 }
    }
}

impl Lerp for PointState {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            x: self.x + (other.x - self.x) * t,
            y: self.y + (other.y - self.y) * t,
            scale: self.scale + (other.scale - self.scale) * t,
            rotation_deg: self.rotation_deg + (other.rotation_deg - self.rotation_deg) * t,
        }
    }
}

// ─── SKELETON ATTACHMENT ─────────────────────────────────────────────

/// Describes how an element is attached to a skeleton point.
/// This is stored on the scene element (actor overlay, image, etc.)
/// and references a skeleton template + point name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonAttachment {
    /// ID of the skeleton template (matches SkeletonTemplate.name or source_clip).
    pub skeleton_id: String,
    /// Name of the point within the skeleton to attach to.
    pub point_name: String,
    /// Offset from the point position in normalised coords.
    #[serde(default)]
    pub offset: [f32; 2],
    /// Scale multiplier applied on top of the point's scale hint.
    #[serde(default = "one")]
    pub scale: f32,
    /// Whether to inherit the point's rotation.
    #[serde(default)]
    pub follow_rotation: bool,
}

impl Default for SkeletonAttachment {
    fn default() -> Self {
        Self {
            skeleton_id: String::new(),
            point_name: String::new(),
            offset: [0.0, 0.0],
            scale: 1.0,
            follow_rotation: false,
        }
    }
}

// ─── PERSISTENCE ─────────────────────────────────────────────────────

impl SkeletonTemplate {
    /// Save this skeleton template as JSON alongside the source clip.
    /// File is named `<clip_stem>.skeleton.json`.
    pub fn save_alongside_clip(&self) -> Result<PathBuf, std::io::Error> {
        let path = self.skeleton_file_path();
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Load a skeleton template from the JSON file alongside a clip.
    pub fn load_for_clip(clip_path: &Path) -> Option<Self> {
        let skel_path = clip_path.with_extension("skeleton.json");
        let raw = std::fs::read_to_string(&skel_path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// The expected file path for this skeleton's JSON.
    pub fn skeleton_file_path(&self) -> PathBuf {
        self.source_clip.with_extension("skeleton.json")
    }

    /// Add a new named point to this skeleton.
    pub fn add_point(&mut self, name: impl Into<String>) -> &mut SkeletonPoint {
        let name = name.into();
        self.points.entry(name.clone()).or_insert_with(|| SkeletonPoint {
            name,
            ..Default::default()
        })
    }

    /// Remove a point by name. Returns the removed point if it existed.
    pub fn remove_point(&mut self, name: &str) -> Option<SkeletonPoint> {
        self.points.remove(name)
    }

    /// Get interpolated state of a point at time `t`.
    pub fn sample_point(&self, point_name: &str, t: f32) -> Option<PointState> {
        let point = self.points.get(point_name)?;
        crate::keyframe::sample(&point.track, t)
    }

    /// Get all point names.
    pub fn point_names(&self) -> Vec<&str> {
        self.points.keys().map(|s| s.as_str()).collect()
    }

    /// Set a keyframe for a point at time `t`. Inserts or updates.
    pub fn set_point_keyframe(&mut self, point_name: &str, t: f32, state: PointState, easing: Easing) {
        let point = self.points.get_mut(point_name);
        let Some(point) = point else { return; };

        // Check if there's already a keyframe close to this time
        let threshold = 0.01;
        if let Some(existing) = point.track.iter_mut().find(|kf| (kf.t - t).abs() < threshold) {
            existing.value = state;
            existing.easing = easing;
        } else {
            point.track.push(Keyframe { t, value: state, easing });
            point.track.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
        }
    }

    /// Remove the keyframe nearest to time `t` for a point.
    pub fn remove_point_keyframe(&mut self, point_name: &str, t: f32) -> bool {
        let point = self.points.get_mut(point_name);
        let Some(point) = point else { return false; };

        let threshold = 0.05;
        if let Some(idx) = point.track.iter().position(|kf| (kf.t - t).abs() < threshold) {
            point.track.remove(idx);
            true
        } else {
            false
        }
    }
}



// ─── ATTACHMENT RESOLVER ─────────────────────────────────────────────

/// Result of resolving a skeleton attachment at a specific time.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedAttachment {
    /// Position in normalised clip coordinates [0,1] (with offset applied).
    pub x: f32,
    pub y: f32,
    /// Combined scale (point scale * attachment scale).
    pub scale: f32,
    /// Rotation in degrees (if follow_rotation is true).
    pub rotation_deg: f32,
}

/// Resolve a `SkeletonAttachment` against available templates at time `t`.
///
/// Searches `templates` for one matching `attachment.skeleton_id` (by name
/// or source_clip filename), then samples the named point at time `t` and
/// applies the attachment's offset and scale.
///
/// Returns `None` if the skeleton or point is not found.
pub fn resolve_skeleton_attachment(
    attachment: &SkeletonAttachment,
    templates: &[SkeletonTemplate],
    t: f32,
) -> Option<ResolvedAttachment> {
    // Find the matching template
    let template = templates.iter().find(|tmpl| {
        tmpl.name == attachment.skeleton_id
            || tmpl.source_clip.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s == attachment.skeleton_id)
                .unwrap_or(false)
    })?;

    // Sample the point
    let point_state = template.sample_point(&attachment.point_name, t)?;

    // Apply offset and scale
    let x = point_state.x + attachment.offset[0];
    let y = point_state.y + attachment.offset[1];
    let scale = point_state.scale * attachment.scale;
    let rotation_deg = if attachment.follow_rotation {
        point_state.rotation_deg
    } else {
        0.0
    };

    Some(ResolvedAttachment { x, y, scale, rotation_deg })
}

/// Convenience: resolve all skeleton attachments for an actor at time `t`.
/// Returns a vec of (attachment_index, resolved_position) pairs.
pub fn resolve_actor_skeleton_attachments(
    actor_skeleton_attachments: &[SkeletonAttachment],
    templates: &[SkeletonTemplate],
    t: f32,
) -> Vec<(usize, ResolvedAttachment)> {
    actor_skeleton_attachments.iter().enumerate()
        .filter_map(|(i, att)| {
            resolve_skeleton_attachment(att, templates, t).map(|r| (i, r))
        })
        .collect()
}
