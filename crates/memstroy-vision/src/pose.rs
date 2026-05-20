use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use memstroy_core::AnchorTrack;

/// Backend that takes a video path and returns a sparse track of body
/// keypoints sampled across the clip. Real implementations are expected
/// to use a pretrained pose model (MoveNet, YOLO-Pose, BlazePose) via
/// ONNX Runtime; the resulting `AnchorTrack` is then persisted as JSON
/// next to the source clip and consumed by the renderer to attach
/// props to body parts (`AnchorPoint`).
#[async_trait]
pub trait PoseEstimator: Send + Sync {
    /// Run the estimator over `video` and return the sparse anchor
    /// track. `target_fps` is a hint for sample density (the estimator
    /// is free to pick a coarser rate if the video is slow-moving).
    async fn estimate(&self, video: &Path, target_fps: f32) -> Result<AnchorTrack>;

    /// Stable backend identifier for cache invalidation in the GUI.
    fn id(&self) -> &'static str;
}

/// A no-op estimator used while the real ONNX pipeline is in progress.
/// Returns an empty track so callers can still read/write JSON on disk
/// and exercise the renderer end-to-end.
pub struct StubPoseEstimator;

#[async_trait]
impl PoseEstimator for StubPoseEstimator {
    async fn estimate(&self, _video: &Path, _target_fps: f32) -> Result<AnchorTrack> {
        Ok(AnchorTrack::default())
    }
    fn id(&self) -> &'static str { "stub" }
}
