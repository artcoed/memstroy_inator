//! Render a [`Scene`] into a final MP4 file via FFmpeg.
//!
//! The renderer translates the high-level scene description into an
//! FFmpeg invocation: a list of `-i` inputs followed by a single
//! `filter_complex` graph. FFmpeg does the heavy lifting (decode,
//! chromakey, overlay, scale, drawtext, encode) which lets us hit
//! 1080x1920@60fps without writing a custom GPU pipeline yet.
//!
//! Two entry points are exposed:
//!
//! - [`render_scene`] — full clip render to MP4.
//! - [`render_preview_frame`] — single-frame PNG, used by the GUI for
//!   the timeline scrubber and inspector.
//!
//! The built command is also exposed via [`build_plan`] so the GUI
//! can show it for debugging.

pub mod compositor;
mod expr;
pub mod filtergraph;
pub mod fonts;
pub mod plan;
pub mod proc;
pub mod runner;
pub mod text_rasterize;

pub use compositor::{render_preview_frame_cpu, render_scene_cpu};
pub use filtergraph::*;
pub use fonts::*;
pub use plan::*;
pub use proc::*;
pub use runner::*;
pub use text_rasterize::*;
