//! Library interface for memstroy-gui, exposing types for testing.

pub mod undo;
pub mod video_cache;
pub mod split_crop;
pub mod skeleton_editor;
pub mod kf_anim;
pub mod settings;
pub mod image_fx_cache;
pub mod fx_preview;
pub mod jobs;
pub mod web_image_search;
pub mod build_info;
pub mod i18n;
pub mod audio_engine;
pub mod image_effects;
pub mod image_fx_worker;
pub mod state;

// Re-export the main types needed for testing
pub use state::EditorState;
pub use state::LibraryClip;
pub use state::AssetLibrary;
