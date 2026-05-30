use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::model::{ClipEntry, DownloadState, TgPost};
use crate::scrape::{build_client, fetch_all};

/// Download every `TgPost::downloadable_video()` into `dir`. Files are named
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
            p.downloadable_video().map(|u| {
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

/// Incremental refresh: scrape all posts, find new ones that haven't
/// been downloaded yet, download them oldest-first, and update the
/// persistent state.
///
/// Returns the updated state and download stats.
pub async fn incremental_refresh(
    channel: &str,
    clips_dir: &Path,
    state_path: &Path,
    filter: &str,
    max_pages: usize,
    concurrency: usize,
    mut on_progress: impl FnMut(&str),
) -> Result<(DownloadState, DownloadStats)> {
    // Load existing state
    let mut state = DownloadState::load(state_path);
    state.channel = channel.to_string();

    on_progress("Scanning channel...");

    // Scrape all posts
    let all_posts = fetch_all(channel, max_pages).await?;
    let total_scraped = all_posts.len();

    // Filter by keyword
    let matching: Vec<TgPost> = all_posts
        .into_iter()
        .filter(|p| {
            if filter.is_empty() {
                true
            } else {
                p.body_contains(filter)
            }
        })
        .filter(|p| p.downloadable_video().is_some())
        .collect();

    on_progress(&format!(
        "Found {} posts with video (total scraped: {})",
        matching.len(),
        total_scraped
    ));

    // Register all matching posts in state (mark new ones as not downloaded)
    let mut new_ids: Vec<u64> = Vec::new();
    for post in &matching {
        if !state.clips.contains_key(&post.id) {
            state.clips.insert(
                post.id,
                ClipEntry {
                    id: post.id,
                    description: post.clean_description(),
                    filename: format!("{}.mp4", post.id),
                    downloaded: false,
                    full_text: post.text.clone(),
                    date: post.date.clone(),
                },
            );
            new_ids.push(post.id);
        }
    }

    // Find all pending downloads (not yet downloaded), sorted ascending (oldest first)
    let mut pending: Vec<u64> = state.pending_ids();
    pending.sort();

    if pending.is_empty() {
        on_progress("All clips are up to date!");
        state.last_refresh = Some(chrono::Utc::now().to_rfc3339());
        state.save(state_path)?;
        return Ok((state, DownloadStats::default()));
    }

    on_progress(&format!(
        "Downloading {} new clips (oldest first)...",
        pending.len()
    ));

    // Build a lookup from id -> post (for video URLs)
    let post_by_id: std::collections::HashMap<u64, &TgPost> =
        matching.iter().map(|p| (p.id, p)).collect();

    // Download sequentially from oldest to newest for predictability
    fs::create_dir_all(clips_dir).await.context("mkdir clips dir")?;
    let client = build_client()?;
    let mut stats = DownloadStats::default();

    // Use concurrent download but within ordered chunks
    let work: Vec<(u64, String, PathBuf)> = pending
        .iter()
        .filter_map(|id| {
            post_by_id.get(id).and_then(|p| {
                p.downloadable_video().map(|url| {
                    (*id, url.to_string(), clips_dir.join(format!("{}.mp4", id)))
                })
            })
        })
        .collect();

    let downloaded_ids = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));

    futures::stream::iter(work)
        .for_each_concurrent(concurrency, |(id, url, target)| {
            let client = client.clone();
            let downloaded_ids = downloaded_ids.clone();
            async move {
                if target.exists() {
                    downloaded_ids.lock().unwrap().push(id);
                    return;
                }
                match fetch_one(&client, &url, &target).await {
                    Ok(bytes) => {
                        downloaded_ids.lock().unwrap().push(id);
                        info!(id, bytes, "downloaded");
                    }
                    Err(e) => {
                        warn!(id, error = %e, "download failed");
                    }
                }
            }
        })
        .await;

    let successfully_downloaded = downloaded_ids.lock().unwrap().clone();
    stats.downloaded = successfully_downloaded.len();

    // Mark downloaded in state
    for id in &successfully_downloaded {
        if let Some(entry) = state.clips.get_mut(id) {
            entry.downloaded = true;
        }
    }

    stats.failed = pending.len() - successfully_downloaded.len();
    state.last_refresh = Some(chrono::Utc::now().to_rfc3339());
    state.save(state_path)?;

    // Generate thumbnails for newly downloaded clips
    on_progress("Generating thumbnails...");
    let thumbs_dir = clips_dir.join("thumbs");
    let _ = fs::create_dir_all(&thumbs_dir).await;
    for id in &successfully_downloaded {
        let video_path = clips_dir.join(format!("{}.mp4", id));
        let thumb_path = thumbs_dir.join(format!("{}.jpg", id));
        if !thumb_path.exists() {
            let _ = generate_thumbnail(&video_path, &thumb_path).await;
        }
    }

    on_progress(&format!(
        "Done! {} downloaded, {} failed",
        stats.downloaded, stats.failed
    ));

    Ok((state, stats))
}

/// Extract a thumbnail frame from a video at the first decodable frame.
///
/// Strategy:
///   1. First attempt: read from the natural start of the file (no
///      `-ss` seek) and grab `-frames:v 1`. This is the most reliable
///      path on short clips, on clips whose first kf isn't a keyframe,
///      and on the tiny Telegram preview snippets we sometimes
///      download.
///   2. If that fails, retry with `-ss 0.1` to skip past a possibly
///      malformed initial header.
///
/// We also capture stderr so the warn log in
pub async fn generate_thumbnail(video: &Path, output: &Path) -> Result<()> {
    let ffmpeg = {
        let mut p = memstroy_render::ffmpeg_binary();
        p.set_file_name("ffmpeg");
        if p.exists() { p } else { std::path::PathBuf::from("ffmpeg") }
    };
    let status = tokio::process::Command::new(&ffmpeg)
        .args([
            "-y", "-hide_banner", "-loglevel", "error",
            "-i",
        ])
        .arg(video.as_os_str())
        .args([
            "-frames:v", "1",
            "-vf", "scale=160:-1",
            "-q:v", "5",
        ])
        .arg(output.as_os_str())
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("ffmpeg thumbnail failed for {}", video.display());
    }
    Ok(())
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

/// Maximum size of a single downloaded clip. Telegram clips are well under
/// this; the cap stops an attacker-influenced URL (or an oversized CDN object)
/// from streaming unbounded data to disk and filling the volume (DoS).
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

async fn try_fetch(client: &reqwest::Client, url: &str, target: &Path) -> Result<u64> {
    // Refuse non-public targets. The download URLs come from attacker-
    // controllable channel HTML (og:video / <video src> / CDN regex matches),
    // so without this the fetch could be steered at internal addresses (SSRF).
    if !crate::model::is_public_http_url_str(url) {
        return Err(anyhow::anyhow!("refusing to fetch non-public URL: {}", url));
    }
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("HTTP {} for {}", resp.status(), url));
    }
    // Reject early when the server advertises an oversized body.
    if let Some(len) = resp.content_length() {
        if len > MAX_DOWNLOAD_BYTES {
            return Err(anyhow::anyhow!(
                "refusing oversized download: {} bytes (max {}) for {}",
                len,
                MAX_DOWNLOAD_BYTES,
                url
            ));
        }
    }
    let tmp = target.with_extension("mp4.partial");
    let mut file = fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        // Abort and clean up if the running total blows past the cap (handles
        // a missing/lying Content-Length).
        if total > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = fs::remove_file(&tmp).await;
            return Err(anyhow::anyhow!(
                "download exceeded {} bytes; aborted for {}",
                MAX_DOWNLOAD_BYTES,
                url
            ));
        }
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
