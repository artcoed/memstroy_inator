//! Library interface for memstroy-gui, exposing types for testing.

pub mod audio_engine;
pub mod build_info;
pub mod canvas_image_search;
pub mod fx_preview;
pub mod i18n;
pub mod image_effects;
pub mod image_fx_cache;
pub mod image_fx_worker;
pub mod jobs;
pub mod kf_anim;
pub mod settings;
pub mod skeleton_editor;
pub mod split_crop;
pub mod state;
pub mod undo;
pub mod video_cache;
pub mod web_image_search;

// Re-export the main types needed for testing
pub use state::AssetLibrary;
pub use state::EditorState;
pub use state::LibraryClip;
