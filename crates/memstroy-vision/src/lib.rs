//! Computer-vision helpers for memstroy_generator.
//!
//! Two kinds of operations live here:
//!
//! 1. **Chroma keying** — turning a green-screen frame into RGBA where
//!    the background is fully transparent. The struct
//!    [`HsvChromaKey`] is a CPU implementation suitable for previews
//!    and one-off frame extraction. For full-clip rendering the
//!    `memstroy-render` crate prefers FFmpeg's `chromakey` filter
//!    (much faster), but having a native implementation is required
//!    for live GUI previews and per-frame compositing.
//!
//! 2. **Pose estimation** — extracting body keypoints from each actor
//!    clip so that attached props (caps, glasses, weapons) can follow
//!    body parts. The [`PoseEstimator`] trait abstracts the backend;
//!    a real implementation backed by ONNX Runtime + a pretrained
//!    pose model (MoveNet / YOLO11-pose) is planned for the next
//!    iteration. The current [`StubPoseEstimator`] returns an empty
//!    track so the rest of the pipeline (renderer, GUI) can be
//!    exercised end-to-end.

pub mod chromakey;
pub mod pose;

pub use chromakey::*;
pub use pose::*;
