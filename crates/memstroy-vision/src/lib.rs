//! Computer-vision helpers for memstroy-inator.
//!
//! 1. **Background removal** — rembg-style alpha matting via U²-Netp
//!    (used by the CLI's `remove-bg` subcommand to produce RGBA cutouts).
//! 2. **Pose anchor I/O** — load pre-computed [`memstroy_core::AnchorTrack`]
//!    JSON sidecars so the renderer can pin attached props to body
//!    landmarks. The renderer reads the JSON; nothing in the workspace
//!    currently writes it (anchor JSONs are produced by external
//!    tooling and dropped next to the source clip).
//!
//! Live chroma-keying is handled directly by FFmpeg's `chromakey`
//! filter on the export side and by per-frame CPU helpers inside
//! `memstroy-gui` for the live preview, so this crate no longer
//! ships its own CPU implementation.

pub mod bgremove;
pub mod pose;

pub use bgremove::*;
pub use pose::*;
