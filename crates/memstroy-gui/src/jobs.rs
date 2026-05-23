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
    /// Web image search returned a (possibly empty) list of hits.
    /// The error variant carries a human-readable string suitable for
    /// surfacing on the panel's status line.
    WebSearchFinished(Result<Vec<crate::web_image_search::WebImageHit>, String>),
    /// A web image download for the row identified by `request_id` has
    /// completed. `image_url` is repeated so the panel can match the
    /// response back to its row even after the user has scrolled or
    /// re-searched. `place_on_canvas` mirrors the request flag — when
    /// `true` the App handler also drops an `Overlay::Image` at the
    /// playhead, exactly like the Ctrl+V paste flow.
    WebImageDownloaded {
        request_id: u64,
        image_url: String,
        place_on_canvas: bool,
        result: Result<crate::state::LibraryAsset, String>,
    },
    /// A background image-effects bake completed. The RGBA buffer
    /// inside is uploaded as an `egui::TextureHandle` on the UI
    /// thread (texture upload requires `&egui::Context`) and stashed
    /// in `state.image_fx_cache` keyed by `(path, sig)`. While the
    /// bake is running the canvas falls back to drawing the
    /// unprocessed image, so the UI never blocks on the effect
    /// pipeline. See `image_fx_worker::submit_image_fx_job`.
    ImageFxReady(crate::image_fx_worker::ImageFxResult),
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



// ─────────────────────────────────────────────────────────────────────
// Web image search & download (DuckDuckGo backend)
// ─────────────────────────────────────────────────────────────────────
//
// The image search panel (`crate::web_image_search`) needs two things:
//   1. given a free-form query string, return a flat list of hits;
//   2. given an image URL, save the bytes locally as a normal entry
//      in the project's image library (mirrors the Ctrl+V paste flow
//      in `state.rs::save_clipboard_image_to_library`).
//
// Both operations live here so the spawn / channel / runtime plumbing
// stays in one place — the panel module stays UI-only.
//
// The search uses **DuckDuckGo's image endpoint** (`/i.js?o=json`).
// Two HTTP calls per query:
//   1. GET `https://duckduckgo.com/?q=…&iax=images&ia=images` →
//      scrape a `vqd` token out of the response HTML.
//   2. GET `https://duckduckgo.com/i.js?o=json&q=…&vqd=…&p=1` →
//      JSON list of results.
// No API key is required and the endpoint is content-only (no JS
// execution), which is why it works from a plain HTTP client. If
// DuckDuckGo changes the markup the `vqd` extraction returns `None`
// and the panel surfaces a clear error string.

use crate::state::LibraryAsset;
use crate::web_image_search::{WebImageHit, MAX_RESULTS};

/// Default user-agent string sent with every DuckDuckGo request.
/// DDG sometimes serves a different / shorter HTML to clients without
/// a UA, which makes the `vqd` token harder to find — pretending to
/// be a recent Firefox sidesteps that.
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Fire a web image search and post the result back via `tx`.
///
/// The whole flow runs on the runtime's worker pool — no await on the
/// caller's side. The function never panics; every failure is mapped
/// to a `JobEvent::WebSearchFinished(Err(_))`.
pub fn spawn_web_image_search(rt: &Handle, tx: Sender<JobEvent>, query: String) {
    rt.spawn(async move {
        let result = run_web_image_search(&query).await;
        let _ = tx.send(JobEvent::WebSearchFinished(result));
    });
}

async fn run_web_image_search(query: &str) -> Result<Vec<WebImageHit>, String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))?;

    // 1. Fetch the search-results landing page so we can scrape the
    //    `vqd` token DuckDuckGo embeds in its HTML / inline JS.
    let landing = client
        .get("https://duckduckgo.com/")
        .query(&[
            ("q", query),
            ("iax", "images"),
            ("ia", "images"),
            ("t", "h_"),
        ])
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Accept-Language", "en-US,en;q=0.5")
        .send()
        .await
        .map_err(|e| format!("Couldn't reach DuckDuckGo: {e}"))?;

    if !landing.status().is_success() {
        return Err(format!("DuckDuckGo landing HTTP {}", landing.status()));
    }
    let html = landing
        .text()
        .await
        .map_err(|e| format!("Couldn't read DDG landing body: {e}"))?;
    let vqd = extract_vqd(&html).ok_or_else(|| {
        // The most common cause of a missing vqd is an outage / a
        // CAPTCHA wall — neither is fatal but the user should know.
        "DuckDuckGo refused the search (no vqd token in response). Try again later or tweak the query.".to_string()
    })?;

    // 2. Hit the JSON endpoint with the scraped token.
    let json_resp = client
        .get("https://duckduckgo.com/i.js")
        .query(&[
            ("l", "us-en"),
            ("o", "json"),
            ("q", query),
            ("vqd", vqd.as_str()),
            ("f", ",,,,,,"),
            ("p", "1"),
            ("v7exp", "a"),
        ])
        .header("Accept", "application/json")
        .header("Referer", "https://duckduckgo.com/")
        .header("Accept-Language", "en-US,en;q=0.5")
        .send()
        .await
        .map_err(|e| format!("Image-search request failed: {e}"))?;
    if !json_resp.status().is_success() {
        return Err(format!("Image search HTTP {}", json_resp.status()));
    }
    let json: serde_json::Value = json_resp
        .json()
        .await
        .map_err(|e| format!("Couldn't parse image-search JSON: {e}"))?;

    let hits = parse_ddg_results(&json);
    if hits.is_empty() {
        return Err(format!(
            "No image results for \"{}\" (DuckDuckGo returned an empty list).",
            query
        ));
    }
    Ok(hits)
}

/// Pull the `vqd` token out of DuckDuckGo's landing HTML.
///
/// Over the years DDG has inlined the token a few different ways:
///   * `vqd="3-12345-..."` (double quotes — most common)
///   * `vqd='3-12345-...'` (single quotes — older builds)
///   * `vqd=3-12345-...&` / `vqd=3-12345-...,` (URL or JS literal)
/// We try them in order of likelihood and stop at the first hit.
fn extract_vqd(html: &str) -> Option<String> {
    // Quoted forms first — they bound the token cleanly.
    for needle in ["vqd=\"", "vqd='", "vqd=&quot;"] {
        if let Some(pos) = html.find(needle) {
            let rest = &html[pos + needle.len()..];
            // The token characters are `[A-Za-z0-9-_]`; stop at the
            // first byte outside that set (covers `"`, `'`, `&`, `,`,
            // `)`, …).
            let end = rest
                .as_bytes()
                .iter()
                .position(|c| !c.is_ascii_alphanumeric() && *c != b'-' && *c != b'_')
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
    }
    // Bare form last — looser, only fires when the quoted variants
    // didn't match. Skip occurrences inside `requestVqd(` etc. by
    // requiring the preceding char to be `&`, `?`, `,`, `(`, or
    // whitespace.
    let bytes = html.as_bytes();
    let needle = b"vqd=";
    let mut search_from = 0;
    while let Some(rel) = find_subslice(&bytes[search_from..], needle) {
        let pos = search_from + rel;
        let prev = if pos == 0 { b'?' } else { bytes[pos - 1] };
        if matches!(prev, b'&' | b'?' | b',' | b'(' | b' ' | b'\n' | b'\t' | b';') {
            let rest = &html[pos + needle.len()..];
            let end = rest
                .as_bytes()
                .iter()
                .position(|c| !c.is_ascii_alphanumeric() && *c != b'-' && *c != b'_')
                .unwrap_or(rest.len());
            if end > 0 {
                return Some(rest[..end].to_string());
            }
        }
        search_from = pos + needle.len();
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_ddg_results(v: &serde_json::Value) -> Vec<WebImageHit> {
    let arr = match v.get("results").and_then(|r| r.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut hits = Vec::with_capacity(arr.len().min(MAX_RESULTS));
    for entry in arr.iter().take(MAX_RESULTS) {
        let image = entry
            .get("image")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let thumb = entry
            .get("thumbnail")
            .and_then(|s| s.as_str())
            .unwrap_or(&image)
            .to_string();
        let title = entry
            .get("title")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let url = entry
            .get("url")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let width = entry
            .get("width")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32;
        let height = entry
            .get("height")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32;
        if image.is_empty() {
            continue;
        }
        hits.push(WebImageHit::new(image, thumb, title, url, width, height));
    }
    hits
}

/// Download one image to `dest_dir` and report the result back via `tx`.
///
/// `request_id` and `image_url` are echoed back unchanged in the
/// `WebImageDownloaded` event so the panel can match the response to
/// its row even if the user has re-searched in the meantime. The
/// function deduplicates filenames against existing files in the
/// destination directory and never panics.
pub fn spawn_web_image_download(
    rt: &Handle,
    tx: Sender<JobEvent>,
    image_url: String,
    title_hint: String,
    dest_dir: PathBuf,
    request_id: u64,
    place_on_canvas: bool,
) {
    rt.spawn(async move {
        let result = run_web_image_download(&image_url, &title_hint, &dest_dir).await;
        let _ = tx.send(JobEvent::WebImageDownloaded {
            request_id,
            image_url,
            place_on_canvas,
            result,
        });
    });
}

async fn run_web_image_download(
    image_url: &str,
    title_hint: &str,
    dest_dir: &std::path::Path,
) -> Result<LibraryAsset, String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))?;

    let resp = client
        .get(image_url)
        .header("Accept", "image/*,*/*;q=0.8")
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} from {}", resp.status(), image_url));
    }
    // Pick an extension. Prefer the response's Content-Type because
    // CDNs sometimes lie in the URL path (e.g. ?w=600 with no ext).
    let ext = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(ext_from_mime)
        .or_else(|| ext_from_url(image_url))
        .unwrap_or("png");
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Couldn't read response body: {e}"))?;

    if bytes.is_empty() {
        return Err("Empty response body".to_string());
    }
    // Cheap sanity check: feed the bytes through the `image` crate.
    // If decoding fails the image is unusable downstream, so we'd
    // rather fail loudly than save junk into the library.
    if let Err(e) = image::load_from_memory(&bytes) {
        return Err(format!("Decoded image failed: {e}"))
    }

    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| format!("create {}: {}", dest_dir.display(), e))?;

    // Build a deterministic-ish filename: `web_<title>_<unix-ms>.<ext>`.
    // Falling back to `web_image` when the title is empty / non-ASCII
    // keeps the path safe across filesystems.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stem = sanitise_filename_stem(title_hint);
    let mut filename = format!("web_{stem}_{stamp}.{ext}");
    let mut path = dest_dir.join(&filename);
    let mut suffix = 1u32;
    while path.exists() {
        filename = format!("web_{stem}_{stamp}_{suffix}.{ext}");
        path = dest_dir.join(&filename);
        suffix += 1;
        if suffix > 1000 {
            break;
        }
    }
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| format!("write {}: {}", path.display(), e))?;

    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("web_image")
        .to_string();
    let label = if title_hint.trim().is_empty() {
        id.clone()
    } else {
        title_hint.to_string()
    };
    Ok(LibraryAsset {
        id,
        path: path.clone(),
        label,
        thumbnail: Some(path),
    })
}

fn ext_from_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next().unwrap_or(mime).trim().to_ascii_lowercase();
    match m.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn ext_from_url(url: &str) -> Option<&'static str> {
    let no_query = url.split('?').next().unwrap_or(url);
    let last = no_query.rsplit('/').next().unwrap_or(no_query);
    let dot = last.rfind('.')?;
    match last[dot + 1..].to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "webp" => Some("webp"),
        "gif" => Some("gif"),
        "bmp" => Some("bmp"),
        _ => None,
    }
}

/// Trim a title down to a filesystem-safe stem of at most 60 chars.
/// Non-ASCII alphanumerics get folded to `_` so we don't have to drag
/// a full slugify implementation into the GUI crate.
fn sanitise_filename_stem(title: &str) -> String {
    let mut out = String::with_capacity(title.len().min(60));
    for ch in title.chars() {
        if out.len() >= 60 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if ch == ' ' || ch == '-' || ch == '_' {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed
    }
}
