//! Pose estimation backends.
//!
//! [`OnnxPoseEstimator`] runs a YOLO-pose ONNX model to detect body
//! keypoints. Results are stored as [`AnchorTrack`] JSON files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use memstroy_core::{AnchorSample, AnchorTrack, Keypoint};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::info;

/// COCO-17 keypoint names in standard YOLO-pose output order.
const COCO_KEYPOINTS: &[&str] = &[
    "nose",
    "left_eye",
    "right_eye",
    "left_ear",
    "right_ear",
    "left_shoulder",
    "right_shoulder",
    "left_elbow",
    "right_elbow",
    "left_wrist",
    "right_wrist",
    "left_hip",
    "right_hip",
    "left_knee",
    "right_knee",
    "left_ankle",
    "right_ankle",
];

// ─── TRAIT ───────────────────────────────────────────────────────────

#[async_trait]
pub trait PoseEstimator: Send + Sync {
    async fn estimate(&self, video: &Path, target_fps: f32) -> Result<AnchorTrack>;
    fn id(&self) -> &'static str;
}

/// No-op stub.
pub struct StubPoseEstimator;

#[async_trait]
impl PoseEstimator for StubPoseEstimator {
    async fn estimate(&self, _video: &Path, _target_fps: f32) -> Result<AnchorTrack> {
        Ok(AnchorTrack::default())
    }
    fn id(&self) -> &'static str {
        "stub"
    }
}

// ─── ONNX IMPLEMENTATION ────────────────────────────────────────────

/// ONNX-based pose estimator using YOLO-pose model.
pub struct OnnxPoseEstimator {
    model_path: PathBuf,
    input_size: u32,
    conf_threshold: f32,
}

impl OnnxPoseEstimator {
    pub fn new(model_path: PathBuf) -> Self {
        Self {
            model_path,
            input_size: 640,
            conf_threshold: 0.25,
        }
    }

    pub fn with_input_size(mut self, size: u32) -> Self {
        self.input_size = size;
        self
    }
    pub fn with_confidence(mut self, conf: f32) -> Self {
        self.conf_threshold = conf;
        self
    }
}

#[async_trait]
impl PoseEstimator for OnnxPoseEstimator {
    async fn estimate(&self, video: &Path, target_fps: f32) -> Result<AnchorTrack> {
        if !self.model_path.exists() {
            return Err(anyhow!(
                "Model not found: {}. Download yolov8n-pose.onnx into assets/models/",
                self.model_path.display()
            ));
        }

        let (vid_w, vid_h, vid_fps) = probe_video(video).await?;
        info!(vid_w, vid_h, vid_fps, "probed video");

        let sample_every = (vid_fps / target_fps).max(1.0).round() as u32;
        let effective_fps = vid_fps / sample_every as f32;
        let frame_bytes = (vid_w * vid_h * 3) as usize;

        // Extract sampled frames via ffmpeg
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(video.as_os_str())
            .args([
                "-vf",
                &format!("select=not(mod(n\\,{}))", sample_every),
                "-vsync",
                "vfr",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn ffmpeg")?;

        let mut stdout = child.stdout.take().unwrap();
        let mut all_frames: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut buf = vec![0u8; frame_bytes];
            match stdout.read_exact(&mut buf).await {
                Ok(_) => all_frames.push(buf),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }
        let _ = child.wait().await;
        info!(frames = all_frames.len(), "extracted frames");

        // Run ONNX inference on a blocking thread
        let model_path = self.model_path.clone();
        let input_size = self.input_size;
        let conf_threshold = self.conf_threshold;

        let samples = tokio::task::spawn_blocking(move || -> Result<Vec<AnchorSample>> {
            run_onnx_inference(
                &model_path,
                &all_frames,
                vid_w,
                vid_h,
                input_size,
                conf_threshold,
                effective_fps,
            )
        })
        .await??;

        info!(samples = samples.len(), "pose estimation complete");
        Ok(AnchorTrack {
            fps: effective_fps,
            samples,
        })
    }

    fn id(&self) -> &'static str {
        "onnx-yolo-pose"
    }
}

/// Blocking ONNX inference over all frames.
fn run_onnx_inference(
    model_path: &Path,
    frames: &[Vec<u8>],
    vid_w: u32,
    vid_h: u32,
    input_size: u32,
    conf_threshold: f32,
    effective_fps: f32,
) -> Result<Vec<AnchorSample>> {
    use ort::session::builder::GraphOptimizationLevel;

    let builder =
        ort::session::Session::builder().map_err(|e| anyhow!("ort session builder: {}", e))?;
    let mut builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .unwrap_or_else(|e| e.recover());
    let mut session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("ort load model: {}", e))?;

    let size = input_size as usize;
    let mut samples = Vec::new();

    for (frame_idx, frame_data) in frames.iter().enumerate() {
        let t = frame_idx as f32 / effective_fps;

        // Resize to model input
        let resized = resize_rgb(frame_data, vid_w as usize, vid_h as usize, size, size);

        // Normalize to [0,1] NCHW
        let mut input_data = vec![0.0f32; 3 * size * size];
        for y in 0..size {
            for x in 0..size {
                let src_idx = (y * size + x) * 3;
                let pix = y * size + x;
                input_data[pix] = resized[src_idx] as f32 / 255.0;
                input_data[size * size + pix] = resized[src_idx + 1] as f32 / 255.0;
                input_data[2 * size * size + pix] = resized[src_idx + 2] as f32 / 255.0;
            }
        }

        // Create tensor and run
        let input_tensor = ort::value::Tensor::from_array(([1usize, 3, size, size], input_data))
            .map_err(|e| anyhow!("create tensor: {}", e))?;

        let outputs = session
            .run(ort::inputs![input_tensor])
            .map_err(|e| anyhow!("inference: {}", e))?;

        // Parse output [1, 56, N]
        let output_value = &outputs[0];
        let (shape, data) = output_value
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow!("extract tensor: {}", e))?;

        if shape.len() < 3 {
            continue;
        }

        let num_features = shape[1] as usize;
        let num_dets = shape[2] as usize;

        // Find best detection
        let mut best_conf = 0.0f32;
        let mut best_det: Option<usize> = None;
        for d in 0..num_dets {
            let conf = data[4 * num_dets + d];
            if conf > best_conf && conf > conf_threshold {
                best_conf = conf;
                best_det = Some(d);
            }
        }

        let Some(det) = best_det else {
            continue;
        };

        // Extract keypoints
        let mut points = BTreeMap::new();
        for (kp_idx, &name) in COCO_KEYPOINTS.iter().enumerate() {
            let row_x = 5 + kp_idx * 3;
            let row_y = 5 + kp_idx * 3 + 1;
            let row_c = 5 + kp_idx * 3 + 2;
            if row_c >= num_features {
                break;
            }

            let kp_x = data[row_x * num_dets + det];
            let kp_y = data[row_y * num_dets + det];
            let kp_conf = data[row_c * num_dets + det];

            points.insert(
                name.to_string(),
                Keypoint {
                    x: (kp_x / input_size as f32).clamp(0.0, 1.0),
                    y: (kp_y / input_size as f32).clamp(0.0, 1.0),
                    confidence: kp_conf.clamp(0.0, 1.0),
                },
            );
        }

        // Synthesize head & body_center
        if let (Some(&n), Some(&le), Some(&re)) = (
            points.get("nose"),
            points.get("left_eye"),
            points.get("right_eye"),
        ) {
            points.insert(
                "head".to_string(),
                Keypoint {
                    x: (n.x + le.x + re.x) / 3.0,
                    y: (n.y + le.y + re.y) / 3.0,
                    confidence: (n.confidence + le.confidence + re.confidence) / 3.0,
                },
            );
        }
        if let (Some(&lh), Some(&rh)) = (points.get("left_hip"), points.get("right_hip")) {
            points.insert(
                "body_center".to_string(),
                Keypoint {
                    x: (lh.x + rh.x) / 2.0,
                    y: (lh.y + rh.y) / 2.0,
                    confidence: (lh.confidence + rh.confidence) / 2.0,
                },
            );
        }

        samples.push(AnchorSample { t, points });
    }

    Ok(samples)
}

// ─── UTILITIES ──────────────────────────────────────────────────────

async fn probe_video(path: &Path) -> Result<(u32, u32, f32)> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(path.as_os_str())
        .output()
        .await
        .context("ffprobe")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().split(',').collect();
    if parts.len() < 3 {
        return Err(anyhow!("ffprobe unexpected: {}", stdout));
    }
    let w: u32 = parts[0].parse().context("width")?;
    let h: u32 = parts[1].parse().context("height")?;
    let fps_parts: Vec<&str> = parts[2].split('/').collect();
    let fps = if fps_parts.len() == 2 {
        fps_parts[0].parse::<f32>().unwrap_or(30.0)
            / fps_parts[1].parse::<f32>().unwrap_or(1.0).max(1.0)
    } else {
        parts[2].parse().unwrap_or(30.0)
    };
    Ok((w, h, fps))
}

fn resize_rgb(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dw * dh * 3];
    for dy in 0..dh {
        let sy = dy * sh / dh;
        for dx in 0..dw {
            let sx = dx * sw / dw;
            let si = (sy * sw + sx) * 3;
            let di = (dy * dw + dx) * 3;
            dst[di] = src[si];
            dst[di + 1] = src[si + 1];
            dst[di + 2] = src[si + 2];
        }
    }
    dst
}

/// Save AnchorTrack as JSON alongside the video.
pub fn save_anchor_track(video_path: &Path, track: &AnchorTrack) -> Result<PathBuf> {
    let p = video_path.with_extension("anchors.json");
    std::fs::write(&p, serde_json::to_string_pretty(track)?)?;
    Ok(p)
}

/// Load previously saved AnchorTrack.
pub fn load_anchor_track(video_path: &Path) -> Option<AnchorTrack> {
    let p = video_path.with_extension("anchors.json");
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}
