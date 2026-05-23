//! Pose-anchor sidecar I/O.
//!
//! The renderer pins attached props (caps, glasses, badges) to body
//! landmarks by reading a pre-computed [`AnchorTrack`] JSON sidecar
//! placed next to each source clip (typically named
//! `<clip>.anchors.json`). The track stores keyframed COCO-17 keypoints
//! and is consumed by [`memstroy_render::filtergraph`] when emitting
//! the per-attachment overlay filters.
//!
//! Generation of the sidecars themselves is currently performed by
//! external tooling — there is no in-tree pose estimator. Earlier
//! revisions of this crate shipped a YOLO-pose ONNX backend, but it
//! had no in-app callers and was removed in the cleanup pass.

use std::path::Path;

use memstroy_core::AnchorTrack;

/// Load a previously saved [`AnchorTrack`] from `<video>.anchors.json`,
/// or return `None` if the sidecar is missing or fails to parse.
///
/// The renderer is the only consumer today: it tolerates a missing
/// track gracefully and falls back to the actor's static layout
/// position when no anchors are available.
pub fn load_anchor_track(video_path: &Path) -> Option<AnchorTrack> {
    let p = video_path.with_extension("anchors.json");
    std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok())
}
