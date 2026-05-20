//! Background jobs orchestration.
//!
//! The GUI runs all heavy work (scrape, download, render, preview) on
//! a tokio runtime owned by the [`App`]. Results travel back to the UI
//! thread via a `crossbeam-style` mpsc — except we just use the std
//! channel since the volume is tiny.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Instant;

use memstroy_core::Scene;
use memstroy_render::{render_preview_frame, render_scene};
use memstroy_tg::{download_videos, fetch_all, ChannelCatalog};
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
    DownloadFinished(Result<DownloadSummary, String>),
}

#[derive(Debug, Clone)]
pub struct DownloadSummary {
    pub total: usize,
    pub kept: usize,
    pub downloaded: usize,
    pub skipped: usize,
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
        let started = Instant::now();
        let log_tx = tx.clone();
        let result = render_scene(&scene, &assets, &out_path, |line| {
            let _ = log_tx.send(JobEvent::RenderLog(line.to_string()));
        })
        .await;
        let _ = tx.send(JobEvent::Status(format!(
            "render finished in {:.1}s",
            started.elapsed().as_secs_f32()
        )));
        let _ = tx.send(JobEvent::RenderFinished(
            result.map(|_| out_path).map_err(|e| e.to_string()),
        ));
    });
}

pub fn spawn_download(
    rt: &Handle,
    tx: Sender<JobEvent>,
    channel: String,
    out_dir: PathBuf,
    filter: String,
    max_pages: usize,
    overwrite: bool,
    concurrency: usize,
) {
    rt.spawn(async move {
        let _ = tx.send(JobEvent::Status(format!("scraping {}…", channel)));
        let posts = match fetch_all(&channel, max_pages).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(JobEvent::DownloadFinished(Err(e.to_string())));
                return;
            }
        };
        let total = posts.len();
        let mut kept = posts.clone();
        if !filter.is_empty() {
            kept.retain(|p| p.body_contains(&filter));
        }
        let kept_count = kept.len();
        let _ = tx.send(JobEvent::Status(format!(
            "scraped {} posts, kept {} for download",
            total, kept_count
        )));

        if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
            let _ = tx.send(JobEvent::DownloadFinished(Err(format!("mkdir: {e}"))));
            return;
        }

        let catalog = ChannelCatalog {
            channel,
            fetched_at: chrono::Utc::now().to_rfc3339(),
            posts: kept.clone(),
        };
        if let Err(e) = write_catalog(&out_dir, &catalog).await {
            warn!(error = %e, "writing catalog");
        }

        match download_videos(&kept, &out_dir, overwrite, concurrency).await {
            Ok(stats) => {
                let _ = tx.send(JobEvent::DownloadFinished(Ok(DownloadSummary {
                    total,
                    kept: kept_count,
                    downloaded: stats.downloaded,
                    skipped: stats.skipped,
                    failed: stats.failed,
                })));
            }
            Err(e) => {
                let _ = tx.send(JobEvent::DownloadFinished(Err(e.to_string())));
            }
        }
    });
}

async fn write_catalog(dir: &Path, c: &ChannelCatalog) -> anyhow::Result<()> {
    let path = dir.join("catalog.json");
    tokio::fs::write(path, serde_json::to_vec_pretty(c)?).await?;
    Ok(())
}
