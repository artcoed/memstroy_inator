use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::model::TgPost;
use crate::scrape::build_client;

/// Download every `TgPost::primary_video()` into `dir`. Files are named
/// `{id}.mp4`. Existing files are skipped unless `overwrite` is true.
pub async fn download_videos(
    posts: &[TgPost],
    dir: &Path,
    overwrite: bool,
    concurrency: usize,
) -> Result<DownloadStats> {
    fs::create_dir_all(dir).await.context("mkdir output dir")?;
    let client = build_client()?;
    let stats = std::sync::Arc::new(std::sync::Mutex::new(DownloadStats::default()));

    let work: Vec<(u64, String, PathBuf)> = posts
        .iter()
        .filter_map(|p| {
            p.primary_video().map(|u| {
                (p.id, u.to_string(), dir.join(format!("{}.mp4", p.id)))
            })
        })
        .collect();

    let total = work.len();
    info!(total, dir = %dir.display(), "starting downloads");

    futures::stream::iter(work)
        .for_each_concurrent(concurrency, |(id, url, target)| {
            let client = client.clone();
            let stats = stats.clone();
            async move {
                if !overwrite && target.exists() {
                    stats.lock().unwrap().skipped += 1;
                    return;
                }
                match fetch_one(&client, &url, &target).await {
                    Ok(bytes) => {
                        let mut s = stats.lock().unwrap();
                        s.downloaded += 1;
                        s.bytes += bytes;
                        info!(id, target = %target.display(), bytes, "ok");
                    }
                    Err(e) => {
                        warn!(id, url = %url, error = %e, "download failed");
                        stats.lock().unwrap().failed += 1;
                    }
                }
            }
        })
        .await;

    let final_stats = std::mem::take(&mut *stats.lock().unwrap());
    Ok(final_stats)
}

async fn fetch_one(client: &reqwest::Client, url: &str, target: &Path) -> Result<u64> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match try_fetch(client, url, target).await {
            Ok(b) => return Ok(b),
            Err(e) if attempt < 3 => {
                warn!(error = %e, attempt, "transient error, retrying");
                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn try_fetch(client: &reqwest::Client, url: &str, target: &Path) -> Result<u64> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("HTTP {} for {}", resp.status(), url));
    }
    let tmp = target.with_extension("mp4.partial");
    let mut file = fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    fs::rename(&tmp, target).await?;
    Ok(total)
}

#[derive(Debug, Default, Clone)]
pub struct DownloadStats {
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
}
