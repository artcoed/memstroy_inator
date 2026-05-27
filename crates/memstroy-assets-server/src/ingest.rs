//! Background ingest jobs.
//!
//! The HTTP layer kicks these off via `tokio::spawn`, returns
//! immediately to the caller, and lets the spawned task drive the
//! scrape + download to completion. When the job finishes (or fails)
//! we re-index the asset store so the new files become visible to the
//! GUI without requiring a server restart.

use std::time::Duration;
use tracing::{info, warn};

use crate::store::AssetStore;

/// Spawn a Telegram channel ingest job. The new clips are saved into
/// `<asset-root>/clips/` and the store is re-indexed when the
/// downloads finish.
///
/// `limit` caps how many of the most recent matching posts we'll pull
/// down. Internally this maps to roughly `limit / 16` Telegram preview
/// pages (with a small margin), since each preview page surfaces at
/// most ~16 posts.
///
/// The task will abort gracefully if it takes longer than 30 seconds
/// to start, preventing hangs when there's no internet connection.
pub fn spawn_tg_ingest(store: AssetStore, channel: String, limit: u32) {
    tokio::spawn(async move {
        // Set a timeout for the entire ingest operation to prevent hangs
        // when there's no internet connection or the server is shutting down
        // Increased to 5 minutes to allow downloading multiple clips
        let timeout_duration = Duration::from_secs(300); // 5 minutes
        
        match tokio::time::timeout(timeout_duration, run_ingest(store, channel, limit)).await {
            Ok(_) => {
                info!("Telegram ingest completed successfully");
            }
            Err(_) => {
                warn!("Telegram ingest timed out after {} seconds", timeout_duration.as_secs());
            }
        }
    });
}

async fn run_ingest(store: AssetStore, channel: String, limit: u32) {
    eprintln!("=== run_ingest started: channel={}, limit={} ===", channel, limit);
    let root = store.root();
    if root.as_os_str().is_empty() {
        warn!("ingest aborted: store has no asset root configured");
        eprintln!("=== run_ingest: no asset root ===");
        return;
    }
    let clips_dir = root.join("clips");
    eprintln!("=== clips_dir: {} ===", clips_dir.display());
    if let Err(e) = tokio::fs::create_dir_all(&clips_dir).await {
        warn!(error = %e, dir = %clips_dir.display(), "failed to create clips dir");
        eprintln!("=== failed to create clips dir: {} ===", e);
        return;
    }

    // Translate post-count into Telegram preview pages. ~16 posts
    // per page; round up and add a little padding.
    let max_pages = ((limit as usize / 16) + 2).max(2);
    info!(
        channel = %channel,
        limit,
        max_pages,
        "starting Telegram ingest"
    );

    info!("Fetching posts from Telegram...");
    eprintln!("=== Fetching posts from Telegram... ===");
    let posts = match memstroy_tg::fetch_all(&channel, max_pages).await {
        Ok(p) => {
            info!("Fetched {} posts from Telegram", p.len());
            eprintln!("=== Fetched {} posts ===", p.len());
            p
        }
        Err(e) => {
            warn!(error = %e, channel = %channel, "fetch_all failed");
            eprintln!("=== fetch_all failed: {} ===", e);
            return;
        }
    };

    // Newest-first, then keep only `limit` posts.
    let mut posts = posts;
    posts.sort_by(|a, b| b.id.cmp(&a.id));
    posts.truncate(limit as usize);
    info!("After sorting and truncating: {} posts", posts.len());

    // Create thumbs directory upfront
    let thumbs_dir = clips_dir.join("thumbs");
    if let Err(e) = tokio::fs::create_dir_all(&thumbs_dir).await {
        warn!(error = %e, dir = %thumbs_dir.display(), "failed to create thumbs dir");
    }
    
    let client = match memstroy_tg::build_client() {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(error = %e, "failed to build HTTP client for thumbnails, will skip thumbnail downloads");
            None
        }
    };

    // Download videos and create metadata sequentially (one by one)
    // This ensures atomic operations: if disk fills up, we have complete clips with metadata
    info!("Starting sequential download and metadata creation for {} posts", posts.len());
    
    let mut downloaded = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut total_bytes = 0u64;
    
    for (idx, post) in posts.iter().enumerate() {
        let stem = post.id.to_string();
        let video_path = clips_dir.join(format!("{}.mp4", stem));
        
        info!("Processing clip {}/{}: id={}", idx + 1, posts.len(), post.id);
        
        // Step 1: Download video if it has a URL and doesn't exist
        let video_downloaded = if let Some(video_url) = post.primary_video() {
            if video_path.exists() {
                info!("Video already exists for {}, skipping download", stem);
                skipped += 1;
                true
            } else {
                info!("Downloading video for {}", stem);
                // Use memstroy_tg's download logic
                let posts_slice = &[post.clone()];
                match memstroy_tg::download_videos(posts_slice, &clips_dir, false, 1).await {
                    Ok(stats) if stats.downloaded > 0 => {
                        info!("✓ Downloaded video {} ({} bytes)", stem, stats.bytes);
                        downloaded += 1;
                        total_bytes += stats.bytes;
                        true
                    }
                    Ok(_) => {
                        warn!("✗ Video download returned 0 for {}", stem);
                        failed += 1;
                        false
                    }
                    Err(e) => {
                        warn!("✗ Failed to download video {}: {}", stem, e);
                        failed += 1;
                        false
                    }
                }
            }
        } else {
            info!("Post {} has no video URL, skipping", stem);
            false
        };
        
        // Step 2: Create metadata ONLY if video was downloaded successfully
        if video_downloaded && video_path.exists() {
            let description = post.clean_description();
            
            // Create txt file with description (always, even if empty)
            let txt_path = clips_dir.join(format!("{}.txt", stem));
            if let Err(e) = tokio::fs::write(&txt_path, description.as_bytes()).await {
                warn!(error = %e, path = %txt_path.display(), "failed to write description");
            } else {
                info!("✓ Created txt metadata for {} ({} bytes)", stem, description.len());
            }
            
            // Create label file (only if description is not empty)
            if !description.is_empty() {
                let label_path = clips_dir.join(format!("{}.label", stem));
                let short_label: String = description
                    .chars()
                    .take(60)
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !short_label.is_empty() {
                    if let Err(e) = tokio::fs::write(&label_path, short_label.as_bytes()).await {
                        warn!(error = %e, path = %label_path.display(), "failed to write label");
                    }
                }
            }
            
            // Download thumbnail
            if let Some(ref http_client) = client {
                let thumb_path = thumbs_dir.join(format!("{}.jpg", stem));
                if !thumb_path.exists() && !post.images.is_empty() {
                    if let Some(thumb_url) = post.images.first() {
                        match http_client.get(thumb_url).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                match resp.bytes().await {
                                    Ok(bytes) => {
                                        if let Err(e) = tokio::fs::write(&thumb_path, &bytes).await {
                                            warn!(error = %e, path = %thumb_path.display(), "failed to write thumbnail");
                                        } else {
                                            info!("✓ Downloaded thumbnail for {}", stem);
                                        }
                                    }
                                    Err(e) => warn!(error = %e, id = post.id, "failed to read thumbnail bytes"),
                                }
                            }
                            Ok(resp) => warn!(id = post.id, status = %resp.status(), "thumbnail download failed"),
                            Err(e) => warn!(error = %e, id = post.id, "thumbnail request failed"),
                        }
                    }
                }
            }
            
            info!("✓ Completed clip {}/{}: {}", idx + 1, posts.len(), stem);
        }
    }
    
    info!(
        downloaded = downloaded,
        skipped = skipped,
        failed = failed,
        bytes = total_bytes,
        "Sequential ingest complete"
    );

    // Reindex after video downloads complete so clips appear with metadata
    info!("Reindexing store after ingest...");
    if let Err(e) = store.index_dir(&root) {
        warn!(error = %e, "post-ingest reindex failed");
    } else {
        info!("Store reindexed successfully");
    }
}
