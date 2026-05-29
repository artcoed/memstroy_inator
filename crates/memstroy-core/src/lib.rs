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
pub mod effects;
pub mod layout_sample;
pub mod parent_transform;

pub use scene::*;
pub use layout_sample::*;
pub use parent_transform::*;
pub use easing::*;
pub use keyframe::*;
pub use anchor::*;
pub use ai_pipeline::*;
pub use canvas::*;
pub use skeleton::*;
pub use scripting::*;
pub use effects::*;

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
        let mut scene: Scene = match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => serde_yaml::from_str(&raw)?,
            Some("json") => serde_json::from_str(&raw)?,
            // Project-native ".memstroy" file. Always JSON, with the
            // scene tucked under a "scene" key so it can sit alongside
            // editor layout / per-tab metadata in the same document.
            // See `EditorState::save_memstroy` for the writer side.
            Some("memstroy") => {
                let bundle: serde_json::Value = serde_json::from_str(&raw)?;
                let scene_value = bundle
                    .get("scene")
                    .cloned()
                    .ok_or_else(|| SceneError::UnknownFormat(
                        ".memstroy file missing required \"scene\" key".into(),
                    ))?;
                serde_json::from_value(scene_value)?
            }
            other => return Err(SceneError::UnknownFormat(other.unwrap_or("").into())),
        };
        // Apply every legacy-format upgrade in the right order:
        //   1. `migrate_decouple_render_frame` rewrites legacy
        //      normalised positions so render-frame edits stop
        //      perturbing other elements;
        //   2. `backfill_animated_params` marks parameters that
        //      already vary across keyframes as animated, preserving
        //      animations authored before the per-param toggle existed.
        scene.upgrade_legacy();
        Ok(scene)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SceneError> {
        let path = path.as_ref();
        let serialized = match path.extension().and_then(|s| s.to_str()) {
            Some("yaml") | Some("yml") => serde_yaml::to_string(self)?,
            Some("json") => serde_json::to_string_pretty(self)?,
            // `.memstroy` minimal write — only the scene, no layout.
            // The GUI's `save_memstroy` writes a richer bundle that also
            // captures editor layout; this fallback exists for the CLI
            // and tests, where a Scene-only round-trip is enough.
            Some("memstroy") => {
                let bundle = serde_json::json!({
                    "format": "memstroy",
                    "format_version": 1,
                    "scene": self,
                });
                serde_json::to_string_pretty(&bundle)?
            }
            other => return Err(SceneError::UnknownFormat(other.unwrap_or("").into())),
        };
        std::fs::write(path, serialized)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests_roundtrip {
    use super::*;

    /// Loading then re-saving a fresh scene must produce byte-identical
    /// YAML (apart from formatter quirks). This catches accidental
    /// schema bloat from the new animated_params / param_kfs fields.
    #[test]
    fn empty_animated_params_not_serialised() {
        let scene = Scene::default();
        let yaml = serde_yaml::to_string(&scene).unwrap();
        assert!(!yaml.contains("animated_params"),
            "default scene should not write animated_params (got:\n{})", yaml);
        assert!(!yaml.contains("param_kfs"),
            "default scene should not write effect param_kfs (got:\n{})", yaml);
    }
}
