//! Background jobs orchestration.
//!
//! ## Telegram refresh = server-driven HTTP flow
//!
//! The GUI no longer scrapes Telegram itself. Instead it talks to a
//! local `memstroy-assets-server` instance (default
//! `http://127.0.0.1:8765`) and asks **it** to ingest a channel:
//!
//!   1. POST `/api/ingest/tg` with `{channel, limit}` so the server
//!      kicks off the scrape + download in the background.
//!   2. Wait a short grace period for the server to do its work
//!      (Telegram preview pages are small, so a few seconds is usually
//!      enough; the user can hit Refresh again to pick up more).
//!   3. GET `/api/assets?kind=clip&limit=200` to enumerate every clip
//!      the server now has.
//!   4. For each clip the GUI doesn't yet have a local copy of, GET
//!      `/api/assets/<id>/download` and write the bytes into the
//!      editor's `assets/mellstroy/` directory so the existing
//!      `library_clips_tab` UI keeps reading from disk.
//!   5. Generate a thumbnail per new clip via local ffmpeg, mirroring
//!      what the old direct-scrape path did.
//!
//! When the server is unreachable or the catalogue is empty, the user
//! gets a clear status string instead of a silent no-op.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use memstroy_core::Scene;
use memstroy_render::{ffmpeg_binary, render_scene};
use serde::Deserialize;
use tokio::runtime::Handle;
use tracing::{info, warn};

/// Messages from background jobs to the UI thread.
#[derive(Debug)]
#[allow(dead_code)]
pub enum JobEvent {
    Status(String),
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

/// Trigger a server-driven Telegram refresh and pull any new clips
/// down into `clips_dir`. See the module-level docs for the full flow.
///
/// `server_url` should be the base URL of a `memstroy-assets-server`
/// instance (no trailing slash necessary). `channel` and `limit` are
/// forwarded as the `/api/ingest/tg` request body.
pub fn spawn_refresh(
    rt: &Handle,
    tx: Sender<JobEvent>,
    server_url: String,
    channel: String,
    limit: u32,
    clips_dir: PathBuf,
) {
    rt.spawn(async move {
        let progress = |s: String| {
            let _ = tx.send(JobEvent::RefreshProgress(s));
        };
        let server = server_url.trim_end_matches('/').to_string();

        progress(format!("Asking {} to ingest @{} (limit {})", server, channel, limit));

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                    "Couldn't build HTTP client: {e}"
                ))));
                return;
            }
        };

        // 1. POST /api/ingest/tg — fire-and-forget kick to the server.
        let ingest_url = format!("{}/api/ingest/tg", server);
        let body = serde_json::json!({ "channel": channel, "limit": limit });
        match client.post(&ingest_url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => {
                progress("Server accepted ingest request, waiting for downloads...".into());
            }
            Ok(resp) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                    "Server rejected ingest: HTTP {} ({})",
                    resp.status(),
                    server
                ))));
                return;
            }
            Err(e) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                    "Server unreachable ({}): {e}\nIs `memstroy-assets-server` running?",
                    server
                ))));
                return;
            }
        }

        // 2. Wait for the server to scrape + download. Telegram preview
        //    pages are small so 6s covers the common case; the user can
        //    hit Refresh again to pick up newer clips.
        tokio::time::sleep(Duration::from_secs(6)).await;

        // 3. GET /api/assets?kind=clip — list everything the server has.
        let list_url = format!("{}/api/assets?kind=clip&limit=200", server);
        let listing: ListResponse = match client.get(&list_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                        "Couldn't parse server listing: {e}"
                    ))));
                    return;
                }
            },
            Ok(resp) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                    "Listing failed: HTTP {}",
                    resp.status()
                ))));
                return;
            }
            Err(e) => {
                let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                    "Listing failed: {e}"
                ))));
                return;
            }
        };

        progress(format!(
            "Server has {} clip(s). Syncing missing ones to local cache...",
            listing.total
        ));

        if let Err(e) = tokio::fs::create_dir_all(&clips_dir).await {
            let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                "Couldn't create local clips dir {}: {e}",
                clips_dir.display()
            ))));
            return;
        }
        let thumbs_dir = clips_dir.join("thumbs");
        let _ = tokio::fs::create_dir_all(&thumbs_dir).await;

        // 4. Download anything the GUI doesn't already have on disk.
        let mut new_count = 0usize;
        let mut failed = 0usize;
        for item in &listing.items {
            // Server ids match the ingest filenames: `{id}.mp4`.
            let file_name = format!("{}.mp4", sanitise_id(&item.id));
            let local_path = clips_dir.join(&file_name);
            // Always mirror the description as a sidecar so the GUI's
            // local library scan (which reads `<stem>.txt` next to each
            // mp4) can show the Telegram caption even after the server
            // is offline. We write the file unconditionally because the
            // server is the source of truth for descriptions and may
            // have re-cleaned an existing one.
            if !item.description.is_empty() {
                let txt_path = clips_dir.join(format!("{}.txt", sanitise_id(&item.id)));
                if let Err(e) = tokio::fs::write(&txt_path, item.description.as_bytes()).await {
                    warn!(
                        id = %item.id,
                        error = %e,
                        "failed to mirror description sidecar locally"
                    );
                }
            }
            if local_path.exists() {
                continue;
            }
            let dl_url = format!("{}/api/assets/{}/download", server, item.id);
            match download_file(&client, &dl_url, &local_path).await {
                Ok(_) => {
                    info!(id = %item.id, "downloaded clip from server");
                    new_count += 1;
                }
                Err(e) => {
                    warn!(id = %item.id, error = %e, "clip download failed");
                    failed += 1;
                }
            }
        }

        if new_count > 0 {
            progress("Generating thumbnails...".into());
            generate_thumbnails(&clips_dir, &thumbs_dir).await;
        }

        let _ = tx.send(JobEvent::RefreshFinished(Ok(RefreshSummary {
            new_clips: new_count,
            total_clips: listing.items.len(),
            failed,
        })));
    });
}

/// Asset summary returned by `GET /api/assets?kind=clip`.
#[derive(Debug, Clone, Deserialize)]
struct ServerAssetSummary {
    id: String,
    #[allow(dead_code)]
    label: String,
    /// Free-form description (cleaned Telegram caption for clips, or
    /// the contents of a `<id>.txt` sidecar for any other kind). The
    /// server already truncates this to 240 chars in `AssetSummary`.
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ListResponse {
    total: u64,
    items: Vec<ServerAssetSummary>,
}

/// HTTP GET → file. Buffers the whole body in memory because the
/// per-clip files are small enough (TG preview pages cap clip duration
/// to ~60 s).
async fn download_file(
    client: &reqwest::Client,
    url: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read error: {e}"))?;
    tokio::fs::write(target, &bytes)
        .await
        .map_err(|e| format!("write error: {e}"))
}

/// Strip characters that aren't safe in filenames (defence-in-depth —
/// the server already restricts ids to a sane character class).
fn sanitise_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// For every `*.mp4` file in `clips_dir` without a matching
/// `thumbs/<stem>.jpg`, run a quick ffmpeg pass to extract a frame.
async fn generate_thumbnails(clips_dir: &std::path::Path, thumbs_dir: &std::path::Path) {
    let bin = ffmpeg_binary();
    let ffmpeg_ok = std::process::Command::new(&bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !ffmpeg_ok {
        warn!("ffmpeg not found — skipping thumbnail generation");
        return;
    }

    let entries = match std::fs::read_dir(clips_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() { continue; }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if ext != "mp4" { continue; }
        let stem = match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let thumb = thumbs_dir.join(format!("{}.jpg", stem));
        if thumb.exists() { continue; }

        let result = tokio::process::Command::new(&bin)
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-ss", "0.5",
                "-i", &p.to_string_lossy(),
                "-frames:v", "1",
                "-vf", "scale=120:-1",
                "-q:v", "6",
            ])
            .arg(&thumb)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        match result {
            Ok(status) if status.success() => {
                info!(id = %stem, "generated thumbnail");
            }
            Ok(_) => {
                // Some clips fail at ss=0.5 (very short). Retry from 0.
                let _ = tokio::process::Command::new(&bin)
                    .args([
                        "-y", "-hide_banner", "-loglevel", "error",
                        "-i", &p.to_string_lossy(),
                        "-frames:v", "1",
                        "-vf", "scale=120:-1",
                        "-q:v", "6",
                    ])
                    .arg(&thumb)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
            Err(e) => warn!(id = %stem, error = %e, "ffmpeg thumbnail failed"),
        }
    }
}
