//! Background jobs orchestration.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use memstroy_core::Scene;
use memstroy_render::{render_preview_frame, render_scene};
use memstroy_tg::download::incremental_refresh;
use tokio::runtime::Handle;
use tracing::warn;

/// Messages from background jobs to the UI thread.
#[derive(Debug)]
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
