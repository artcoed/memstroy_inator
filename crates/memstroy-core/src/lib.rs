//! Domain model for memstroy_generator scenes.
//!
//! A `Scene` describes a meme video: output size, duration, layered
//! background tracks, mellstroy "actors" (chroma-keyed clips) with
//! attached props, overlays (text/image/video), and camera moves.
//!
//! All animatable values are expressed via `Keyframe<T>` lists with an
//! easing curve between consecutive keyframes.
//!
//! The format is round-trippable as YAML (preferred) or JSON.

pub mod scene;
pub mod easing;
pub mod keyframe;
pub mod anchor;
pub mod ai_pipeline;
pub mod canvas;
pub mod skeleton;
pub mod scripting;

pub use scene::*;
pub use easing::*;
pub use keyframe::*;
pub use anchor::*;
pub use ai_pipeline::*;
pub use canvas::*;
pub use skeleton::*;
pub use scripting::*;

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SceneError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown scene format (expected .yaml/.yml/.json): {0}")]
    UnknownFormat(String),
}

impl Scene {
    pub fn load(path: impl AsRef<Path>) -> Result<Scene, SceneError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)?;
        match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => Ok(serde_yaml::from_str(&raw)?),
            Some("json") => Ok(serde_json::from_str(&raw)?),
            other => Err(SceneError::UnknownFormat(other.unwrap_or("").into())),
        }
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SceneError> {
        let path = path.as_ref();
        let serialized = match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => serde_yaml::to_string(self)?,
            Some("json") => serde_json::to_string_pretty(self)?,
            other => return Err(SceneError::UnknownFormat(other.unwrap_or("").into())),
        };
        std::fs::write(path, serialized)?;
        Ok(())
    }
}
