//! AI Meme Generation Pipeline
//!
//! This module defines the structured data formats for AI-driven meme montage.
//!
//! ## How it works:
//! 1. The editor exports a "Project Context" (ProjectInput) — description of available assets,
//!    current scene state, and user's creative prompt
//! 2. An AI (GPT-4, Claude, etc.) receives this context and generates a "Montage Plan" (MontageOutput)
//! 3. The editor applies the MontageOutput to the scene, creating/modifying clips, keyframes, text, timing
//!
//! ## Usage flow:
//! User writes prompt → Editor builds ProjectInput → Send to AI API → AI returns MontageOutput → Editor applies
//!
//! ## Example prompt: "Сделай мем где Мелстрой бьет по столу когда видит цену биткоина"
//!
//! The AI receives:
//! - Available clips (with descriptions, durations)
//! - Available backgrounds
//! - Canvas size, fps, duration constraints
//! - The user's creative prompt
//!
//! The AI returns:
//! - Which clips to use and at what times
//! - Text overlays with content and timing
//! - Keyframe animations (position, scale, rotation over time)
//! - Audio track selections
//! - Transitions between segments

use serde::{Deserialize, Serialize};

/// INPUT: Everything the AI needs to know about the project to generate a montage plan.
/// This is serialized to JSON and sent as context to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInput {
    /// User's creative prompt describing what meme to make
    pub prompt: String,

    /// Available mellstroy clips with metadata
    pub available_clips: Vec<ClipInfo>,

    /// Available background assets
    pub available_backgrounds: Vec<AssetInfo>,

    /// Available prop/overlay images
    pub available_props: Vec<AssetInfo>,

    /// Available audio tracks
    pub available_audio: Vec<AssetInfo>,

    /// Output canvas constraints
    pub canvas: CanvasConstraints,

    /// Current scene state (if editing existing scene)
    pub current_scene: Option<SceneSnapshot>,

    /// Style preferences
    pub style: StyleHints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipInfo {
    /// Unique clip identifier
    pub id: String,
    /// Human-readable description of what happens in the clip
    pub description: String,
    /// Duration in seconds
    pub duration: f32,
    /// File path (for the editor to locate)
    pub path: String,
    /// Tags for semantic search (e.g., ["удар", "стол", "эмоция", "крик"])
    pub tags: Vec<String>,
    /// Detected emotions/actions (from future pose/expression analysis)
    pub detected_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: String,
    pub path: String,
    pub description: String,
    /// Duration (for video/audio assets), None for images
    pub duration: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanvasConstraints {
    /// Output resolution [width, height]
    pub resolution: [u32; 2],
    pub fps: u32,
    /// Maximum duration in seconds
    pub max_duration: f32,
    /// Preferred duration (AI should aim for this)
    pub target_duration: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneSnapshot {
    /// Current actors on timeline
    pub actors: Vec<ActorSnapshot>,
    /// Current text overlays
    pub texts: Vec<TextSnapshot>,
    /// Current duration
    pub duration: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorSnapshot {
    pub clip_id: String,
    pub t_in: f32,
    pub t_out: f32,
    pub position: [f32; 2],
    pub scale: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSnapshot {
    pub text: String,
    pub t_in: f32,
    pub t_out: f32,
    pub position: [f32; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleHints {
    /// Preferred text style: "meme_impact", "subtitle", "minimal"
    pub text_style: String,
    /// Preferred pacing: "fast", "normal", "slow", "comedic_timing"
    pub pacing: String,
    /// Whether to use chroma key
    pub use_chroma_key: bool,
    /// Preferred transitions
    pub transitions: Vec<String>,
}

/// OUTPUT: The AI's montage plan that the editor will apply to create the final meme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MontageOutput {
    /// Total scene duration the AI decided on
    pub duration: f32,

    /// Ordered list of clip placements
    pub clips: Vec<ClipPlacement>,

    /// Text overlays to add
    pub texts: Vec<TextPlacement>,

    /// Background segments
    pub backgrounds: Vec<BackgroundPlacement>,

    /// Audio tracks to add
    pub audio: Vec<AudioPlacement>,

    /// Keyframe animations for actors
    pub animations: Vec<ActorAnimation>,

    /// AI's reasoning (for debugging/user review)
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipPlacement {
    /// Which clip to use (references ClipInfo.id)
    pub clip_id: String,
    /// Where on the timeline this clip starts
    pub timeline_start: f32,
    /// How long to show it (may be shorter than clip duration = trimmed)
    pub duration: f32,
    /// Where in the source clip to start (for trimming)
    pub source_offset: f32,
    /// Initial position [x, y] in normalized coords (0-1)
    pub position: [f32; 2],
    /// Initial scale
    pub scale: f32,
    /// Whether to apply chroma key
    pub chroma_key: bool,
    /// Layer order (higher = on top)
    pub layer: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPlacement {
    /// The text content
    pub text: String,
    /// When to show
    pub t_in: f32,
    /// When to hide
    pub t_out: f32,
    /// Position [x, y] normalized
    pub position: [f32; 2],
    /// Font size
    pub font_size: f32,
    /// Text color [r, g, b] 0-255
    pub color: [u8; 3],
    /// Whether to show white box behind text (meme style)
    pub box_background: bool,
    /// Animation: "none", "fade_in", "scale_up", "slide_from_bottom"
    pub animation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundPlacement {
    /// Asset id or "solid_color"
    pub asset_id: String,
    pub start: f32,
    pub duration: f32,
    /// For solid color backgrounds
    pub color: Option<[u8; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPlacement {
    pub asset_id: String,
    pub t_in: f32,
    pub t_out: f32,
    pub volume: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorAnimation {
    /// Which clip placement this animation applies to (index in clips array)
    pub clip_index: usize,
    /// Keyframes for this actor
    pub keyframes: Vec<AnimKeyframe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimKeyframe {
    /// Time relative to clip's timeline_start
    pub t: f32,
    pub position: [f32; 2],
    pub scale: f32,
    pub rotation_deg: f32,
    pub opacity: f32,
}

/// Converts a MontageOutput into scene mutations.
/// This is what the editor calls after receiving AI output.
impl MontageOutput {
    /// Apply this montage plan to a Scene, replacing its contents.
    pub fn apply_to_scene(&self, scene: &mut crate::Scene, clips_dir: &std::path::Path) {
        // Set duration
        scene.output.duration = self.duration;

        // Clear existing content
        scene.actors.clear();
        scene.overlays.clear();
        scene.backgrounds.clear();
        scene.audio.clear();

        // Add clips as actors
        for cp in &self.clips {
            let source = clips_dir.join(&cp.clip_id); // Simplified path resolution
            let actor = crate::Actor {
                id: cp.clip_id.clone(),
                source,
                anchors: None,
                chroma_key: if cp.chroma_key {
                    crate::ChromaKeyParams::default()
                } else {
                    crate::ChromaKeyParams { similarity: 0.0, ..Default::default() }
                },
                layout: vec![crate::Keyframe::new(0.0, crate::ActorState {
                    pos: cp.position,
                    scale: cp.scale,
                    rotation_deg: 0.0,
                    opacity: 1.0,
                })],
                t_in: Some(cp.timeline_start),
                t_out: Some(cp.timeline_start + cp.duration),
                source_start: cp.source_offset,
                loop_source: false,
                flip_horizontal: false,
                attachments: Vec::new(),
                visible: true,
                color_correction: crate::ColorCorrection::default(),
            };
            scene.actors.push(actor);
        }

        // Apply animations
        for anim in &self.animations {
            if anim.clip_index < scene.actors.len() {
                let actor = &mut scene.actors[anim.clip_index];
                actor.layout = anim.keyframes.iter().map(|kf| {
                    crate::Keyframe::new(kf.t, crate::ActorState {
                        pos: kf.position,
                        scale: kf.scale,
                        rotation_deg: kf.rotation_deg,
                        opacity: kf.opacity,
                    })
                }).collect();
            }
        }

        // Add text overlays
        for tp in &self.texts {
            let overlay = crate::Overlay::Text(crate::TextOverlay {
                id: format!("ai_text_{}", tp.t_in as u32),
                text: tp.text.clone(),
                t_in: tp.t_in,
                t_out: tp.t_out,
                style: crate::TextStyle {
                    font_size: tp.font_size,
                    color: tp.color,
                    box_color: if tp.box_background { Some([255, 255, 255]) } else { None },
                    ..Default::default()
                },
                layout: vec![crate::Keyframe::new(0.0, crate::OverlayState {
                    pos: tp.position,
                    scale: 1.0,
                    rotation_deg: 0.0,
                    opacity: 1.0,
                })],
            });
            scene.overlays.push(overlay);
        }

        // Add backgrounds
        for bg in &self.backgrounds {
            let source = if let Some(color) = bg.color {
                crate::MediaSource::SolidColor { color }
            } else {
                crate::MediaSource::Video {
                    path: clips_dir.join(&bg.asset_id),
                    r#loop: true,
                    start_at: 0.0,
                }
            };
            scene.backgrounds.push(crate::Background {
                id: bg.asset_id.clone(),
                source,
                start: bg.start,
                duration: bg.duration,
                fit: crate::Fit::Cover,
                transition: crate::Transition::Cut,
            });
        }

        // Add audio
        for ap in &self.audio {
            scene.audio.push(crate::AudioTrack {
                id: ap.asset_id.clone(),
                source: clips_dir.join(&ap.asset_id),
                t_in: ap.t_in,
                t_out: Some(ap.t_out),
                source_start: 0.0,
                volume: ap.volume,
            });
        }
    }
}
