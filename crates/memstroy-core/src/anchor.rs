use serde::{Deserialize, Serialize};

/// Body landmarks usable as attachment anchors. Mirrors a subset of the
/// COCO-17 / MediaPipe pose keypoint vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AnchorPoint {
    Nose,
    Head,           // synthesized centre between eyes/nose
    LeftEye,
    RightEye,
    LeftEar,
    RightEar,
    LeftShoulder,
    RightShoulder,
    LeftElbow,
    RightElbow,
    LeftWrist,
    RightWrist,
    LeftHip,
    RightHip,
    LeftKnee,
    RightKnee,
    LeftAnkle,
    RightAnkle,
    /// Centre of bounding box, fallback when no pose is detected.
    BodyCenter,
}

/// Per-frame keypoint detected by the pose estimator. Coordinates are
/// in normalised [0, 1] image space, `confidence` in [0, 1].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Keypoint {
    pub x: f32,
    pub y: f32,
    pub confidence: f32,
}

/// Sparse anchor track: keyed by the pose landmark, valued as a list of
/// `(time, keypoint)` samples. Sampling rate is up to the estimator
/// (e.g. every Nth frame), the renderer interpolates between samples.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnchorTrack {
    pub fps: f32,
    pub samples: Vec<AnchorSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorSample {
    pub t: f32,
    /// Map keyed by landmark name (`AnchorPoint` serialised). Stored as
    /// string-keyed to keep the JSON file diff-friendly.
    pub points: std::collections::BTreeMap<String, Keypoint>,
}
