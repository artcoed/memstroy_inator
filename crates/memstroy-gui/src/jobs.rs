//! Background jobs orchestration.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use memstroy_core::Scene;
use memstroy_render::{render_preview_frame, render_scene, ffmpeg_binary};
use memstroy_tg::download::incremental_refresh;
use tokio::runtime::Handle;
use tracing::{info, warn};

/// Messages from background jobs to the UI thread.
#[derive(Debug)]
#[allow(dead_code)]
pub enum JobEvent {
    Status(String),
    PreviewReady(PathBuf),
    PreviewFailed(String),
    RenderLog(String),
    RenderFinished(Result<PathBuf, String>),
    RefreshProgress(String),
    RefreshFinished(Result<RefreshSummary, String>),
}

#[derive(Debug, Clone)]
pub struct RefreshSummary {
    pub new_clips: usize,
    pub total_clips: usize,
    pub failed: usize,
}

pub fn spawn_preview(
    rt: &Handle,
    tx: Sender<JobEvent>,
    scene: Scene,
    assets: PathBuf,
    t: f32,
    out_png: PathBuf,
) {
    rt.spawn(async move {
        match render_preview_frame(&scene, &assets, t, &out_png).await {
            Ok(()) => {
                let _ = tx.send(JobEvent::PreviewReady(out_png));
            }
            Err(e) => {
                warn!(error = %e, "preview render failed");
                let _ = tx.send(JobEvent::PreviewFailed(e.to_string()));
            }
        }
    });
}

pub fn spawn_render(
    rt: &Handle,
    tx: Sender<JobEvent>,
    scene: Scene,
    assets: PathBuf,
    out_path: PathBuf,
) {
    rt.spawn(async move {
        let log_tx = tx.clone();
        let result = render_scene(&scene, &assets, &out_path, |line| {
            let _ = log_tx.send(JobEvent::RenderLog(line.to_string()));
        })
        .await;
        let _ = tx.send(JobEvent::RenderFinished(
            result.map(|_| out_path).map_err(|e| e.to_string()),
        ));
    });
}

pub fn spawn_refresh(
    rt: &Handle,
    tx: Sender<JobEvent>,
    channel: String,
    clips_dir: PathBuf,
    state_path: PathBuf,
    filter: String,
    max_pages: usize,
    concurrency: usize,
) {
    rt.spawn(async move {
        let progress_tx = tx.clone();
        let result = incremental_refresh(
            &channel,
            &clips_dir,
            &state_path,
            &filter,
            max_pages,
            concurrency,
            |msg| {
                let _ = progress_tx.send(JobEvent::RefreshProgress(msg.to_string()));
            },
        )
        .await;

        match result {
            Ok((state, stats)) => {
                // Generate thumbnails for newly downloaded clips
                let progress_tx2 = tx.clone();
                let _ = progress_tx2.send(JobEvent::RefreshProgress("Generating thumbnails...".into()));
                generate_clip_thumbnails(&clips_dir, &state).await;

                let _ = tx.send(JobEvent::RefreshFinished(Ok(RefreshSummary {
                    new_clips: stats.downloaded,
                    total_clips: state.downloaded_count(),
                    failed: stats.failed,
                })));
            }
            Err(e) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(e.to_string())));
            }
        }
    });
}


/// Generate thumbnail images (first frame) for all downloaded clips that
/// don't already have one. Uses ffmpeg -ss 0 -frames:v 1 to extract.
async fn generate_clip_thumbnails(
    clips_dir: &std::path::Path,
    state: &memstroy_tg::model::DownloadState,
) {
    let thumbs_dir = clips_dir.join("thumbs");
    if let Err(e) = std::fs::create_dir_all(&thumbs_dir) {
        warn!("Failed to create thumbs dir: {}", e);
        return;
    }

    let bin = ffmpeg_binary();
    for clip in state.all_clips_sorted() {
        if !clip.downloaded {
            continue;
        }
        let thumb_path = thumbs_dir.join(format!("{}.jpg", clip.id));
        if thumb_path.exists() {
            continue; // already have thumbnail
        }
        let source = clips_dir.join(&clip.filename);
        if !source.exists() {
            continue;
        }

        let result = tokio::process::Command::new(&bin)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel", "error",
                "-ss", "0.5",
                "-i", &source.to_string_lossy(),
                "-frames:v", "1",
                "-vf", "scale=120:-1",
                "-q:v", "6",
            ])
            .arg(&thumb_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        match result {
            Ok(status) if status.success() => {
                info!("Generated thumbnail for clip {}", clip.id);
            }
            Ok(_) => {
                // Try with ss=0 if 0.5 failed (very short clip)
                let _ = tokio::process::Command::new(&bin)
                    .args([
                        "-y",
                        "-hide_banner",
                        "-loglevel", "error",
                        "-i", &source.to_string_lossy(),
                        "-frames:v", "1",
                        "-vf", "scale=120:-1",
                        "-q:v", "6",
                    ])
                    .arg(&thumb_path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
            Err(e) => {
                warn!("Failed to run ffmpeg for thumbnail {}: {}", clip.id, e);
            }
        }
    }
}
