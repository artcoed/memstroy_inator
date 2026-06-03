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
//!   3. GET `/api/assets?kind=clip&limit=N` to enumerate clips
//!      the server now has.
//!   4. For each clip, download it sequentially via
//!      `/api/assets/<id>/download` and write the bytes into the
//!      editor's `assets/mellstroy/` directory. After each clip is
//!      downloaded, the GUI reloads the library so clips appear
//!      immediately one by one.
//!   5. Download thumbnails and description sidecars for each clip
//!      so the library panel can show captions and previews.
//!
//! When the server is unreachable or the catalogue is empty, the user
//! gets a clear status string instead of a silent no-op.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use memstroy_core::{Overlay, Scene};
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
    RenderOutputChosen(Option<PathBuf>),
    RenderFinished(Result<PathBuf, String>),
    RefreshProgress(String),
    /// Mid-refresh signal: the worker has just downloaded one or more
    /// clips into `clips_dir` and the GUI should re-scan the local
    /// library so they appear in the panel without waiting for the
    /// entire refresh to finish. Carries the latest progress text so
    /// the UI can update both the status bar and the library list in
    /// one shot.
    RefreshLibraryReloaded(String),
    RefreshFinished(Result<RefreshSummary, String>),
    /// Web image search returned a (possibly empty) list of hits, plus
    /// an optional cursor (`next_offset`) for the next page. The
    /// `page_offset` echoes the request's start offset so the UI can
    /// distinguish "fresh search → replace results" (offset == 0) from
    /// "append page → push onto existing results" (offset > 0). The
    /// error variant carries a human-readable string suitable for
    /// surfacing on the panel's status line.
    WebSearchFinished {
        page_offset: u32,
        /// Which UI initiated the search — panel grid vs canvas popup.
        target: WebSearchTarget,
        result: Result<(Vec<crate::web_image_search::WebImageHit>, Option<u32>), String>,
    },
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
    /// AI background removal for a canvas image-search overlay finished.
    AiBgRemoveFinished {
        overlay_idx: usize,
        path: PathBuf,
        result: Result<(), String>,
    },
    /// A background image-effects bake completed. The RGBA buffer
    /// inside is uploaded as an `egui::TextureHandle` on the UI
    /// thread (texture upload requires `&egui::Context`) and stashed
    /// in `state.image_fx_cache` keyed by `(path, sig)`. While the
    /// bake is running the canvas falls back to drawing the
    /// unprocessed image, so the UI never blocks on the effect
    /// pipeline. See `image_fx_worker::submit_image_fx_job`.
    ImageFxReady(crate::image_fx_worker::ImageFxResult),
    /// A lazy-download of a server-only clip has completed. The GUI
    /// drops the clip onto canvas/timeline at `drop_target` once
    /// the bytes have landed locally. Carries the local path so the
    /// caller can update the library list (mark as downloaded).
    ClipDownloaded {
        server_id: String,
        result: Result<PathBuf, String>,
        drop_target: ClipDropTarget,
    },
    /// One paginated page from the assets-server catalogue finished
    /// loading. The UI merges these summaries into the active library
    /// tab without doing a full filesystem rescan.
    ServerAssetsPageLoaded {
        tab: crate::state::LibraryTab,
        query: String,
        offset: u64,
        limit: u64,
        result: Result<ServerAssetsPage, String>,
    },
    /// A generic server-backed asset (image/sound/video) finished
    /// downloading after the user dropped its preview.
    ServerAssetDownloaded {
        server_id: String,
        kind: crate::state::AssetDragKind,
        result: Result<PathBuf, String>,
        drop_target: ServerAssetDropTarget,
    },
    /// Background ffprobe finished for a clip placed on the timeline.
    VideoDurationProbed {
        actor_id: String,
        path: PathBuf,
        duration: Option<f32>,
    },
    /// Background filesystem scan finished — apply on the UI thread.
    LibraryScanned(crate::state::LibraryScanSnapshot),
    /// Background library scan failed (worker thread panic / join error).
    LibraryReloadAborted,
}

/// Where to drop a freshly-downloaded clip when the lazy-download
/// path completes. The caller picks the target at the moment of
/// drag-end so the deferred completion knows what to do.
/// Identifies which search UI should receive `WebSearchFinished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebSearchTarget {
    #[default]
    Panel,
    Canvas,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ClipDropTarget {
    /// Drop on canvas at world position.
    CanvasAt { world_x: f32, world_y: f32 },
    /// Drop on timeline at scene-time.
    TimelineAt { t: f32 },
    /// Fill an existing Mellstroy-footage sequence slot after download.
    SequenceSlot { actor_id: String },
    /// Just download into the cache; don't auto-place.
    None,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ServerAssetDropTarget {
    CanvasAt { world_x: f32, world_y: f32 },
    TimelineAt { t: f32, track_idx: Option<usize> },
    None,
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

/// Stamp every actor / overlay's `z_order` field on the supplied
/// scene from the editor's timeline-track assignments. Must be called
/// on the cloned scene right before `spawn_render` so the renderer's
/// `emit_z_ordered_elements` pass reproduces the preview canvas's
/// layer stacking exactly.
///
/// Stacking rule (mirrors `canvas_preview::draw_canvas_overlays` and
/// `frame_snapshot::paint_frame`):
///   * Lower track index = higher up the timeline panel = drawn LAST
///     (visually on top)  → HIGHER `z_order` value.
///   * Higher track index = lower on the panel = drawn FIRST (behind)
///     → LOWER `z_order` value.
///
/// We map `z_order = -(track as i32) - 1`. The `- 1` offset makes
/// even track 0 produce `z_order = -1` (nonzero), which is what the
/// renderer keys on to enable explicit z-ordering — without it, a
/// scene with all elements on track 0 would still trip the all-zero
/// legacy fallback and the user-visible bug ("clips above an image
/// in preview disappear behind the image after render") would
/// resurrect itself.
///
/// Defaults for un-assigned elements mirror the canvas helpers:
///   * Actors  → first video track (`actor_track_index`),
///   * Overlays → second video track if present, else first
///     (`default_overlay_track`).
pub fn populate_render_z_order(state: &crate::state::EditorState, scene: &mut Scene) {
    use crate::state::TrackKind;

    let z_for_track = |track: usize| -> i32 { -(track as i32) - 1 };

    let default_actor_track = (0..state.tracks.len())
        .find(|i| state.tracks[*i].kind == TrackKind::Video)
        .unwrap_or(0);

    let video_track_indices: Vec<usize> = (0..state.tracks.len())
        .filter(|i| state.tracks[*i].kind == TrackKind::Video)
        .collect();
    let default_overlay_track = if video_track_indices.len() >= 2 {
        video_track_indices[1]
    } else if !video_track_indices.is_empty() {
        video_track_indices[0]
    } else {
        0
    };

    for (idx, actor) in scene.actors.iter_mut().enumerate() {
        let track = state
            .actor_track_assignments
            .get(&idx)
            .copied()
            .unwrap_or(default_actor_track);
        actor.z_order = z_for_track(track);
    }
    for (idx, ov) in scene.overlays.iter_mut().enumerate() {
        let track = state
            .overlay_track_assignments
            .get(&idx)
            .copied()
            .unwrap_or(default_overlay_track);
        let z = z_for_track(track);
        match ov {
            Overlay::Text(t) => t.z_order = z,
            Overlay::Image(i) => i.z_order = z,
            Overlay::Video(v) => v.z_order = z,
        }
    }
}

/// Trigger a server-driven Telegram refresh and download clips
/// one by one into `clips_dir`. See the module-level docs for the full flow.
///
/// `server_url` should be the base URL of a `memstroy-assets-server`
/// instance (no trailing slash necessary). `channel` and `limit` are
/// forwarded as the `/api/ingest/tg` request body.
///
/// ## Sequential download strategy
///
/// The previous version only downloaded metadata and thumbnails,
/// leaving the actual video files to be downloaded lazily when the
/// user dragged them onto the canvas. This caused confusion because
/// clips didn't appear in the project immediately. We now download
/// clips **sequentially, one by one**, and reload the library after
/// each download so the user sees clips appearing in real-time.
///
/// Progress messages flow back through `JobEvent::RefreshProgress`
/// and `JobEvent::RefreshLibraryReloaded` so the user sees
/// "Downloaded X / Y clips" updating live in the status bar.
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

        info!("spawn_refresh: clips_dir = {}", clips_dir.display());

        // The GUI may have been configured with a wildcard bind URL
        // (e.g. `http://0.0.0.0:8765`) — that is a valid bind address
        // for the server but `connect(2)` to it fails on Windows with
        // `WSAEADDRNOTAVAIL`, which is exactly the "Refresh failed:
        // Server unreachable" the user kept hitting. Normalise to a
        // routable loopback before any HTTP call goes out.
        let server = crate::state::rewrite_server_url_for_client(&server_url)
            .trim_end_matches('/')
            .to_string();

        progress(format!(
            "Asking {} to ingest @{} (limit {})",
            server, channel, limit
        ));

        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
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
                progress("Server accepted ingest request, downloading clips...".into());
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

        // 2. Prepare the local mirror directory up-front so we can
        //    start writing files as soon as the server has them.
        if let Err(e) = tokio::fs::create_dir_all(&clips_dir).await {
            let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                "Couldn't create local clips dir {}: {e}",
                clips_dir.display()
            ))));
            return;
        }
        let thumbs_dir = clips_dir.join("thumbs");
        let _ = tokio::fs::create_dir_all(&thumbs_dir).await;

        // 3. Poll server for clips. Start immediately without delay.
        let list_url = format!("{}/api/assets?kind=clip&limit={}", server, limit);

        progress("Fetching clip list from server...".into());

        let mut new_count = 0usize;
        let failed = 0usize;
        let mut downloaded_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Poll the server for up to 30 seconds or until we get clips
        let max_wait = Duration::from_secs(30);
        let poll_interval = Duration::from_millis(500); // Poll every 500ms for faster response
        let started = std::time::Instant::now();

        let listing: ListResponse = loop {
            progress("Fetching clip list from server...".into());

            match client.get(&list_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<ListResponse>().await {
                        Ok(list) => {
                            if list.items.is_empty() {
                                if started.elapsed() >= max_wait {
                                    let _ = tx.send(JobEvent::RefreshFinished(Err(
                                        "Server returned no clips after 60 seconds. Try again or check the channel name.".to_string()
                                    )));
                                    return;
                                }
                                progress(format!(
                                    "Server has no clips yet, waiting... ({:.0}s elapsed)",
                                    started.elapsed().as_secs_f32()
                                ));
                                tokio::time::sleep(poll_interval).await;
                                continue;
                            }
                            // Got clips!
                            break list;
                        }
                        Err(e) => {
                            let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                                "Couldn't parse server listing: {e}"
                            ))));
                            return;
                        }
                    }
                }
                Ok(resp) => {
                    let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                        "Server listing failed: HTTP {}",
                        resp.status()
                    ))));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(JobEvent::RefreshFinished(Err(format!(
                        "Couldn't fetch listing: {e}"
                    ))));
                    return;
                }
            }
        };

        let total_clips = listing.items.len();
        progress(format!("Found {} clips on server, downloading metadata...", total_clips));

        // Download only metadata (description + thumbnail) for each clip
        // The actual video will be downloaded lazily when first used
        for (idx, item) in listing.items.iter().enumerate() {
            if downloaded_ids.contains(&item.id) {
                continue;
            }

            let safe_id = sanitise_id(&item.id);
            let txt_path = clips_dir.join(format!("{}.txt", safe_id));
            let thumb_jpg = thumbs_dir.join(format!("{}.jpg", safe_id));

            info!(
                "Processing clip {}/{}: id={}, safe_id={}, txt_path={}, thumb_path={}",
                idx + 1,
                total_clips,
                item.id,
                safe_id,
                txt_path.display(),
                thumb_jpg.display()
            );

            // Skip if metadata already downloaded locally
            // We only download metadata (txt + thumbnail), not the video itself
            if txt_path.exists() && find_local_thumbnail(&clips_dir, &safe_id).is_some() {
                info!("Clip {} already has metadata, skipping", safe_id);
                downloaded_ids.insert(item.id.clone());
                new_count += 1;
                continue;
            }

            progress(format!(
                "Downloading metadata {} / {} ({})",
                idx + 1,
                total_clips,
                safe_id
            ));

            // Download description sidecar (always, even if video exists)
            if !item.description.is_empty() {
                match tokio::fs::write(&txt_path, item.description.as_bytes()).await {
                    Ok(_) => {
                        info!("✓ Wrote description for {} ({} bytes)", safe_id, item.description.len());
                        // Verify file was created
                        if txt_path.exists() {
                            info!("✓ Verified txt file exists: {}", txt_path.display());
                        } else {
                            warn!("✗ txt file not found after write: {}", txt_path.display());
                        }
                    }
                    Err(e) => warn!("✗ Failed to write description for {}: {}", safe_id, e),
                }
            } else {
                info!("Clip {} has no description, creating empty txt file", safe_id);
                let _ = tokio::fs::write(&txt_path, b"").await;
            }

            // Download thumbnail (always, even if video exists)
            if find_local_thumbnail(&clips_dir, &safe_id).is_none() {
                let thumb_url = format!(
                    "{}/api/assets/{}/preview",
                    server, item.id
                );
                info!("Downloading thumbnail from: {}", thumb_url);
                match download_thumbnail(&client, &thumb_url, &thumb_jpg).await {
                    Ok(_) => {
                        info!("✓ Downloaded thumbnail for {}", safe_id);
                        // Verify file was created
                        if thumb_jpg.exists() {
                            let metadata = tokio::fs::metadata(&thumb_jpg).await;
                            match metadata {
                                Ok(m) => info!("✓ Verified thumbnail exists: {} ({} bytes)", thumb_jpg.display(), m.len()),
                                Err(e) => warn!("✗ Can't read thumbnail metadata: {}", e),
                            }
                        } else {
                            warn!("✗ Thumbnail not found after download: {}", thumb_jpg.display());
                        }
                    }
                    Err(e) => warn!("✗ Failed to download thumbnail for {}: {}", safe_id, e),
                }
            } else {
                info!("Thumbnail already exists for {}", safe_id);
            }

            // Mark as "known" but not downloaded (video will be downloaded on first use)
            downloaded_ids.insert(item.id.clone());
            new_count += 1;

            // Small delay to avoid overwhelming the server
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        generate_thumbnails(&clips_dir, &thumbs_dir).await;

        let _ = tx.send(JobEvent::RefreshFinished(Ok(RefreshSummary {
            new_clips: new_count,
            total_clips: total_clips,
            failed,
        })));
    });
}

/// Asset summary returned by `GET /api/assets?kind=clip`.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerAssetSummary {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub label: String,
    /// Free-form description (cleaned Telegram caption for clips, or
    /// the contents of a `<id>.txt` sidecar for any other kind). The
    /// server already truncates this to 240 chars in `AssetSummary`.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub duration_secs: Option<f32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(skip)]
    pub local_preview: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse {
    #[allow(dead_code)]
    pub total: u64,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub limit: u64,
    #[serde(default)]
    pub has_more: bool,
    pub items: Vec<ServerAssetSummary>,
}

#[derive(Debug, Clone)]
pub struct ServerAssetsPage {
    pub total: u64,
    pub offset: u64,
    pub limit: u64,
    pub has_more: bool,
    pub items: Vec<ServerAssetSummary>,
}

pub fn spawn_server_assets_page(
    rt: &Handle,
    tx: Sender<JobEvent>,
    server_url: String,
    tab: crate::state::LibraryTab,
    kind_token: &'static str,
    query: String,
    offset: u64,
    limit: u64,
    preview_cache_root: PathBuf,
) {
    rt.spawn(async move {
        let result = fetch_server_assets_page(
            server_url,
            kind_token,
            query.clone(),
            offset,
            limit,
            preview_cache_root,
        )
        .await;
        let _ = tx.send(JobEvent::ServerAssetsPageLoaded {
            tab,
            query,
            offset,
            limit,
            result,
        });
    });
}

async fn fetch_server_assets_page(
    server_url: String,
    kind_token: &'static str,
    query: String,
    offset: u64,
    limit: u64,
    preview_cache_root: PathBuf,
) -> Result<ServerAssetsPage, String> {
    let server = crate::state::rewrite_server_url_for_client(&server_url)
        .trim_end_matches('/')
        .to_string();
    if server.is_empty() {
        return Err("assets-server URL is empty".into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| format!("HTTP client init: {e}"))?;
    let mut url = format!(
        "{}/api/assets?kind={}&offset={}&limit={}",
        server, kind_token, offset, limit
    );
    let query = query.trim();
    if !query.is_empty() {
        url.push_str("&q=");
        url.push_str(&url_query_encode(query));
    }
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("server unavailable: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(180).collect();
        return Err(format!("server list HTTP {status}: {snippet}"));
    }
    let mut list: ListResponse = resp
        .json()
        .await
        .map_err(|e| format!("server list parse failed: {e}"))?;

    let preview_dir = preview_cache_root.join(kind_token);
    let _ = tokio::fs::create_dir_all(&preview_dir).await;
    for item in &mut list.items {
        let Some(preview_url) = item.preview_url.clone() else {
            continue;
        };
        let safe_id = sanitise_id(&item.id);
        let target = preview_dir.join(format!("{safe_id}.jpg"));
        if target.exists() {
            item.local_preview = Some(target);
            continue;
        }
        let full_url = if preview_url.starts_with("http://") || preview_url.starts_with("https://")
        {
            preview_url
        } else {
            format!("{}{}", server, preview_url)
        };
        match download_thumbnail(&client, &full_url, &target).await {
            Ok(()) => item.local_preview = Some(target),
            Err(e) => {
                warn!(
                    id = %item.id,
                    error = %e,
                    "server preview download failed"
                );
            }
        }
    }

    let limit = if list.limit == 0 { limit } else { list.limit };
    let has_more = if list.limit == 0 {
        offset.saturating_add(list.items.len() as u64) < list.total
    } else {
        list.has_more
    };
    Ok(ServerAssetsPage {
        total: list.total,
        offset: list.offset,
        limit,
        has_more,
        items: list.items,
    })
}

fn url_query_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b))
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Locate a local thumbnail for a clip stem, mirroring server-side
/// `find_thumbnail` resolution order.
pub fn find_local_thumbnail(clips_dir: &std::path::Path, stem: &str) -> Option<PathBuf> {
    let mp4_path = clips_dir.join(format!("{stem}.mp4"));

    for ext in ["png", "jpg", "jpeg", "webp"] {
        let cand = clips_dir.join(format!("{stem}.thumb.{ext}"));
        if cand.exists() {
            return Some(cand);
        }
    }

    let thumbs_dir = clips_dir.join("thumbs");
    if thumbs_dir.is_dir() {
        for ext in ["jpg", "jpeg", "png", "webp"] {
            let cand = thumbs_dir.join(format!("{stem}.{ext}"));
            if cand.exists() {
                return Some(cand);
            }
        }
    }

    for ext in ["png", "jpg", "jpeg", "webp"] {
        let cand = clips_dir.join(format!("{stem}.{ext}"));
        if cand.exists() && cand != mp4_path {
            return Some(cand);
        }
    }

    None
}

fn is_valid_image_bytes(bytes: &[u8]) -> bool {
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return true;
    }
    if bytes.len() >= 8 && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return true;
    }
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

async fn download_response_to_file(
    client: &reqwest::Client,
    url: &str,
    target: &std::path::Path,
    min_bytes: usize,
    require_image: bool,
) -> Result<(), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("HTTP {status} (content-type={ct}): {snippet}"));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let content_len = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    if ct.starts_with("text/") {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("unexpected content-type={ct}: {snippet}"));
    }

    let bytes = resp.bytes().await.map_err(|e| format!("read error: {e}"))?;
    if let Some(cl) = content_len {
        if cl != bytes.len() as u64 {
            return Err(format!(
                "incomplete download: content-length={cl} got={}",
                bytes.len()
            ));
        }
    }
    if bytes.len() < min_bytes {
        return Err(format!(
            "download too small: {} bytes (content-type={ct})",
            bytes.len()
        ));
    }
    if require_image && !is_valid_image_bytes(&bytes) {
        return Err(format!(
            "not a valid image: {} bytes (content-type={ct})",
            bytes.len()
        ));
    }

    let tmp = target.with_extension("partial");
    if let Some(parent) = tmp.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| format!("write error: {e}"))?;
    match tokio::fs::rename(&tmp, target).await {
        Ok(()) => Ok(()),
        Err(first_err) => {
            let _ = tokio::fs::remove_file(target).await;
            tokio::fs::rename(&tmp, target)
                .await
                .map_err(|e| format!("rename error: {e}; first attempt: {first_err}"))
        }
    }
}

/// HTTP GET → file. Buffers the whole body in memory because the
/// per-clip files are small enough (TG preview pages cap clip duration
/// to ~60 s).
pub async fn download_file(
    client: &reqwest::Client,
    url: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    // Guard against HTML/error pages or truncated bodies saved as `.mp4`.
    // The GUI's `is_usable_local_video` uses the same 4KB threshold.
    download_response_to_file(client, url, target, 4097, false).await?;
    if !crate::state::EditorState::is_usable_local_video(target) {
        let _ = tokio::fs::remove_file(target).await;
        return Err("download is not a valid video file".into());
    }
    Ok(())
}

/// Download a preview JPEG/PNG/WebP from the assets server. Thumbnails
/// are often well under 4KB (ffmpeg uses `scale=160:-1`), so they
/// must not go through the video size guard in [`download_file`].
pub async fn download_thumbnail(
    client: &reqwest::Client,
    url: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    download_response_to_file(client, url, target, 64, true).await
}

/// Strip characters that aren't safe in filenames (defence-in-depth —
/// the server already restricts ids to a sane character class).
pub fn sanitise_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Lazy download of a server-only clip. Used when the user drags an
/// undownloaded library entry onto the canvas / timeline. Spawns a
/// background task that fetches `/api/assets/{server_id}/download`
/// into `local_path`. On completion sends a `ClipDownloaded` event so
/// the App can spawn the actor at `drop_target`.
pub fn spawn_clip_download(
    rt: &Handle,
    tx: Sender<JobEvent>,
    server_url: String,
    server_id: String,
    local_path: PathBuf,
    drop_target: ClipDropTarget,
) {
    rt.spawn(async move {
        let server = crate::state::rewrite_server_url_for_client(&server_url)
            .trim_end_matches('/')
            .to_string();
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(JobEvent::ClipDownloaded {
                    server_id: server_id.clone(),
                    result: Err(format!("HTTP client init: {e}")),
                    drop_target,
                });
                return;
            }
        };
        let url = format!("{}/api/assets/{}/download", server, server_id);
        if let Some(parent) = local_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let result = download_file(&client, &url, &local_path)
            .await
            .map(|_| local_path.clone());
        if result.is_ok() {
            if let Some(clips_dir) = local_path.parent() {
                let thumbs_dir = clips_dir.join("thumbs");
                generate_thumbnails(clips_dir, &thumbs_dir).await;
            }
        }
        let _ = tx.send(JobEvent::ClipDownloaded {
            server_id,
            result,
            drop_target,
        });
    });
}

pub fn spawn_server_asset_download(
    rt: &Handle,
    tx: Sender<JobEvent>,
    server_url: String,
    server_id: String,
    kind: crate::state::AssetDragKind,
    local_path: PathBuf,
    drop_target: ServerAssetDropTarget,
) {
    rt.spawn(async move {
        let server = crate::state::rewrite_server_url_for_client(&server_url)
            .trim_end_matches('/')
            .to_string();
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(JobEvent::ServerAssetDownloaded {
                    server_id,
                    kind,
                    result: Err(format!("HTTP client init: {e}")),
                    drop_target,
                });
                return;
            }
        };
        let url = format!("{}/api/assets/{}/download", server, server_id);
        if let Some(parent) = local_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let result = match kind {
            crate::state::AssetDragKind::Image | crate::state::AssetDragKind::Particle => {
                download_response_to_file(&client, &url, &local_path, 64, true).await
            }
            crate::state::AssetDragKind::Video | crate::state::AssetDragKind::Clip => {
                download_file(&client, &url, &local_path).await
            }
            crate::state::AssetDragKind::Sound => {
                download_response_to_file(&client, &url, &local_path, 128, false).await
            }
            crate::state::AssetDragKind::None => Err("no asset kind for download".into()),
        }
        .map(|_| local_path.clone());

        if result.is_ok() && matches!(kind, crate::state::AssetDragKind::Video) {
            if let Some(videos_dir) = local_path.parent() {
                let _ = generate_video_library_thumbnails(videos_dir).await;
            }
        }

        let _ = tx.send(JobEvent::ServerAssetDownloaded {
            server_id,
            kind,
            result,
            drop_target,
        });
    });
}

/// For every `*.mp4` file in `clips_dir` without a matching
/// `thumbs/<stem>.jpg`, run a quick ffmpeg pass to extract a frame.
pub(crate) async fn generate_thumbnails(
    clips_dir: &std::path::Path,
    thumbs_dir: &std::path::Path,
) -> usize {
    let bin = ffmpeg_binary();
    let ffmpeg_ok = {
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        memstroy_render::hide_console_std(&mut cmd).status().is_ok()
    };
    if !ffmpeg_ok {
        warn!("ffmpeg not found — skipping thumbnail generation");
        return 0;
    }

    let _ = std::fs::create_dir_all(thumbs_dir);
    let entries = match std::fs::read_dir(clips_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut generated = 0usize;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if ext != "mp4" {
            continue;
        }
        let stem = match p.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if find_local_thumbnail(clips_dir, &stem).is_some() {
            continue;
        }
        let thumb = thumbs_dir.join(format!("{}.jpg", stem));

        let result = {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                "0.5",
                "-i",
                &p.to_string_lossy(),
                "-frames:v",
                "1",
                "-vf",
                "scale=120:-1",
                "-q:v",
                "6",
            ])
            .arg(&thumb)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
            memstroy_render::hide_console_tokio(&mut cmd).status().await
        };
        match result {
            Ok(status) if status.success() => {
                info!(id = %stem, "generated thumbnail");
                generated += 1;
            }
            Ok(_) => {
                // Some clips fail at ss=0.5 (very short). Retry from 0.
                let mut cmd = tokio::process::Command::new(&bin);
                cmd.args([
                    "-y",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-i",
                    &p.to_string_lossy(),
                    "-frames:v",
                    "1",
                    "-vf",
                    "scale=120:-1",
                    "-q:v",
                    "6",
                ])
                .arg(&thumb)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
                if memstroy_render::hide_console_tokio(&mut cmd)
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    info!(id = %stem, "generated thumbnail (retry from 0)");
                    generated += 1;
                }
            }
            Err(e) => warn!(id = %stem, error = %e, "ffmpeg thumbnail failed"),
        }
    }
    generated
}

const VIDEO_LIBRARY_EXTENSIONS: &[&str] = &["mp4", "mov", "webm", "avi", "mkv", "m4v"];

fn asset_thumbnail_exists(video_path: &std::path::Path) -> bool {
    [
        video_path.with_extension("thumb.png"),
        video_path.with_extension("thumb.jpg"),
    ]
    .iter()
    .any(|p| p.is_file())
}

/// For each video in `videos_dir` without a sibling `.thumb.jpg`, extract
/// the first decoded frame for the library grid.
pub(crate) async fn generate_video_library_thumbnails(videos_dir: &std::path::Path) -> usize {
    let bin = ffmpeg_binary();
    let ffmpeg_ok = {
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        memstroy_render::hide_console_std(&mut cmd).status().is_ok()
    };
    if !ffmpeg_ok {
        warn!("ffmpeg not found — skipping video library thumbnail generation");
        return 0;
    }

    let _ = std::fs::create_dir_all(videos_dir);
    let entries = match std::fs::read_dir(videos_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut generated = 0usize;
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !VIDEO_LIBRARY_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        if asset_thumbnail_exists(&p) {
            continue;
        }
        let thumb = p.with_extension("thumb.jpg");
        let result = {
            let mut cmd = tokio::process::Command::new(&bin);
            cmd.args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                &p.to_string_lossy(),
                "-frames:v",
                "1",
                "-vf",
                "scale=120:-1",
                "-q:v",
                "6",
            ])
            .arg(&thumb)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
            memstroy_render::hide_console_tokio(&mut cmd).status().await
        };
        if result.map(|s| s.success()).unwrap_or(false) {
            info!(path = %p.display(), "generated video library thumbnail");
            generated += 1;
        } else {
            warn!(path = %p.display(), "ffmpeg video library thumbnail failed");
        }
    }
    generated
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
use crate::web_image_search::WebImageHit;

/// Default user-agent string sent with every DuckDuckGo request.
/// DDG sometimes serves a different / shorter HTML to clients without
/// a UA, which makes the `vqd` token harder to find — pretending to
/// be a recent Firefox sidesteps that.
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Fire a web image search and post the result back via `tx`.
///
/// `page_offset` is forwarded as DuckDuckGo's `s=` parameter — pass
/// `0` for the first page; subsequent pages reuse the offset returned
/// in the previous response. The whole flow runs on the runtime's
/// worker pool — no await on the caller's side. The function never
/// panics; every failure is mapped to a
/// `JobEvent::WebSearchFinished { result: Err(_), .. }`.
/// Optional search modifiers forwarded from the canvas image-search popup.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebImageSearchOptions {
    /// When set, results are sorted so aspect ratios closest to this
    /// value appear first (width / height).
    pub aspect_ratio: Option<f32>,
    /// Append hints so DuckDuckGo tends to return PNGs on transparent bg.
    pub transparent_only: bool,
}

pub fn spawn_web_image_search(
    rt: &Handle,
    tx: Sender<JobEvent>,
    query: String,
    page_offset: u32,
    target: WebSearchTarget,
    options: WebImageSearchOptions,
) {
    rt.spawn(async move {
        let result = run_web_image_search(&query, page_offset, options).await;
        let _ = tx.send(JobEvent::WebSearchFinished {
            page_offset,
            target,
            result,
        });
    });
}

async fn run_web_image_search(
    query: &str,
    page_offset: u32,
    options: WebImageSearchOptions,
) -> Result<(Vec<WebImageHit>, Option<u32>), String> {
    let effective_query = if options.transparent_only {
        format!("{query} png transparent background")
    } else {
        query.to_string()
    };
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))?;

    // 1. Fetch the search-results landing page so we can scrape the
    //    `vqd` token DuckDuckGo embeds in its HTML / inline JS.
    let landing = client
        .get("https://duckduckgo.com/")
        .query(&[
            ("q", effective_query.as_str()),
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

    // 2. Hit the JSON endpoint with the scraped token. `s=N` is the
    //    DDG start offset. First page uses 0; subsequent pages reuse
    //    the offset extracted from the previous response's `next`
    //    field.
    let s_str = page_offset.to_string();
    let json_resp = client
        .get("https://duckduckgo.com/i.js")
        .query(&[
            ("l", "us-en"),
            ("o", "json"),
            ("q", effective_query.as_str()),
            ("vqd", vqd.as_str()),
            ("f", ",,,,,,"),
            ("p", "1"),
            ("s", s_str.as_str()),
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

    let mut hits = parse_ddg_results(&json);
    if let Some(target_ar) = options.aspect_ratio.filter(|r| r.is_finite() && *r > 0.01) {
        hits.sort_by(|a, b| {
            let ra = a.width as f32 / a.height.max(1) as f32;
            let rb = b.width as f32 / b.height.max(1) as f32;
            let da = (ra - target_ar).abs();
            let db = (rb - target_ar).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let next_offset = parse_ddg_next_offset(&json);
    if hits.is_empty() && page_offset == 0 {
        return Err(format!(
            "No image results for \"{}\" (DuckDuckGo returned an empty list).",
            query
        ));
    }
    Ok((hits, next_offset))
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
        if matches!(
            prev,
            b'&' | b'?' | b',' | b'(' | b' ' | b'\n' | b'\t' | b';'
        ) {
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
    let mut hits = Vec::with_capacity(arr.len().min(crate::web_image_search::MAX_RESULTS_PER_PAGE));
    for entry in arr
        .iter()
        .take(crate::web_image_search::MAX_RESULTS_PER_PAGE)
    {
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
        let width = entry.get("width").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        let height = entry.get("height").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
        if image.is_empty() {
            continue;
        }
        hits.push(WebImageHit::new(image, thumb, title, url, width, height));
    }
    hits
}

/// Pull the next-page start offset out of a DDG `i.js` response.
///
/// DuckDuckGo embeds a `next` field that looks like
/// `i.js?q=foo&s=100&dl=…&l=us-en&...`. We only care about the `s=`
/// parameter — that's the offset to feed back as `s` on the next
/// request. Returns `None` when the response has no `next` field
/// (end of results), when the field can't be parsed, or when the
/// extracted offset is identical to the current page (would loop
/// forever).
fn parse_ddg_next_offset(v: &serde_json::Value) -> Option<u32> {
    let next_raw = v.get("next").and_then(|n| n.as_str())?;
    // `s=` is at most one occurrence in the path/query string. Find
    // it tolerantly without spinning up the `url` crate just for
    // this single parse.
    let s_pos = next_raw.find("s=")?;
    let after = &next_raw[s_pos + 2..];
    let end = after
        .as_bytes()
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    after[..end].parse::<u32>().ok()
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
        .connect_timeout(Duration::from_secs(5))
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
        return Err(format!("Decoded image failed: {e}"));
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
        downloaded: true,
        server_id: None,
        duration_secs: None,
        width: None,
        height: None,
    })
}

fn ext_from_mime(mime: &str) -> Option<&'static str> {
    let m = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
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

// ─── AI background removal (canvas image search) ─────────────────────

/// Run U²-Netp on `path`, optionally gated by a UV-space polygon mask
/// (brush selection). Overwrites the file with a PNG cutout.
pub fn spawn_ai_background_remove(
    rt: &Handle,
    tx: Sender<JobEvent>,
    overlay_idx: usize,
    path: PathBuf,
    model_path: PathBuf,
    mask_polygon_uv: Option<Vec<[f32; 2]>>,
) {
    rt.spawn(async move {
        let task_path = path.clone();
        let task_model_path = model_path.clone();
        let task_mask = mask_polygon_uv.clone();
        let result = match tokio::spawn(async move {
            run_ai_background_remove(&task_path, &task_model_path, task_mask.as_deref()).await
        })
        .await
        {
            Ok(result) => result,
            Err(join_err) if join_err.is_panic() => {
                Err("AI cutout worker crashed while removing the background.".to_string())
            }
            Err(join_err) => Err(format!("AI cutout worker stopped: {join_err}")),
        };
        let _ = tx.send(JobEvent::AiBgRemoveFinished {
            overlay_idx,
            path,
            result,
        });
    });
}

async fn run_ai_background_remove(
    path: &std::path::Path,
    model_path: &std::path::Path,
    mask_polygon_uv: Option<&[[f32; 2]]>,
) -> Result<(), String> {
    use memstroy_vision::bgremove::{BackgroundRemover, U2NetpBgRemover};

    let remover = U2NetpBgRemover::new(model_path.to_path_buf());
    let mut rgba = remover.remove(path).await.map_err(|e| e.to_string())?;

    if let Some(poly) = mask_polygon_uv {
        apply_uv_polygon_gate(&mut rgba, poly);
    }

    rgba.save(path).map_err(|e| format!("save cutout: {e}"))
}

/// Zero alpha outside the UV polygon (keep interior).
fn apply_uv_polygon_gate(rgba: &mut image::RgbaImage, poly: &[[f32; 2]]) {
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || poly.len() < 3 {
        return;
    }
    let wf = w as f32;
    let hf = h as f32;
    for y in 0..h {
        for x in 0..w {
            let u = (x as f32 + 0.5) / wf;
            let v = (y as f32 + 0.5) / hf;
            if !point_in_polygon(u, v, poly) {
                rgba.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
            } else {
                let p = rgba.get_pixel(x, y);
                if p[3] > 0 {
                    rgba.put_pixel(x, y, image::Rgba([p[0], p[1], p[2], p[3]]));
                }
            }
        }
    }
}

fn point_in_polygon(x: f32, y: f32, poly: &[[f32; 2]]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let xi = poly[i][0];
        let yi = poly[i][1];
        let xj = poly[j][0];
        let yj = poly[j][1];
        let intersect =
            ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi).max(1e-6) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}
