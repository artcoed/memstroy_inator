//! Library interface for memstroy-gui, exposing types for testing.

pub mod audio_engine;
pub mod autosave;
pub mod build_info;
pub mod canvas_image_search;
pub mod canvas_preview;
pub mod editor_limits;
pub mod frame_snapshot;
pub mod fx_preview;
pub mod i18n;
pub mod image_effects;
pub mod image_fx_cache;
pub mod image_fx_worker;
pub mod jobs;
pub mod kf_anim;
pub mod panels;
pub mod settings;
pub mod shell_reveal;
pub mod skeleton_editor;
pub mod split_crop;
pub mod state;
pub mod system_fonts;
pub mod undo;
pub mod video_cache;
pub mod web_image_search;

pub mod app {
    pub(crate) fn scene_timeline_end(scene: &memstroy_core::Scene) -> f32 {
        let mut max_end = 0.0_f32;
        for actor in &scene.actors {
            max_end = max_end.max(actor.t_out.unwrap_or(scene.output.duration));
        }
        for bg in &scene.backgrounds {
            max_end = max_end.max(bg.start + bg.duration);
        }
        for ov in &scene.overlays {
            let end = match ov {
                memstroy_core::Overlay::Text(t) => t.t_out,
                memstroy_core::Overlay::Image(i) => i.t_out,
                memstroy_core::Overlay::Video(v) => v.t_out,
            };
            max_end = max_end.max(end);
        }
        for audio in &scene.audio {
            if audio.deleted {
                continue;
            }
            max_end = max_end.max(audio.t_out.unwrap_or(scene.output.duration));
        }
        max_end
    }
}

// Re-export the main types needed for testing
pub use state::AssetLibrary;
pub use state::EditorState;
pub use state::LibraryClip;
