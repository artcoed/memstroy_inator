//! Computer-vision helpers for memstroy_generator.
//!
//! 1. **Chroma keying** — green-screen removal (CPU + FFmpeg paths)
//! 2. **Pose estimation** — body keypoint detection via ONNX Runtime
//! 3. **Background removal** — rembg-style alpha matting via U²-Netp

pub mod bgremove;
pub mod chromakey;
pub mod pose;

pub use bgremove::*;
pub use chromakey::*;
pub use pose::*;
