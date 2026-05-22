//! Background ingest jobs.
//!
//! The HTTP layer kicks these off via `tokio::spawn`, returns
//! immediately to the caller, and lets the spawned task drive the
//! scrape + download to completion. When the job finishes (or fails)
//! we re-index the asset store so the new files become visible to the
//! GUI without requiring a server restart.

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
pub fn spawn_tg_ingest(store: AssetStore, channel: String, limit: u32) {
    tokio::spawn(async move {
        let root = store.root();
        if root.as_os_str().is_empty() {
            warn!("ingest aborted: store has no asset root configured");
            return;
        }
        let clips_dir = root.join("clips");
        if let Err(e) = tokio::fs::create_dir_all(&clips_dir).await {
            warn!(error = %e, dir = %clips_dir.display(), "failed to create clips dir");
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

        let posts = match memstroy_tg::fetch_all(&channel, max_pages).await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, channel = %channel, "fetch_all failed");
                return;
            }
        };

        // Newest-first, then keep only `limit` posts.
        let mut posts = posts;
        posts.sort_by(|a, b| b.id.cmp(&a.id));
        posts.truncate(limit as usize);

        match memstroy_tg::download_videos(&posts, &clips_dir, false, 4).await {
            Ok(stats) => info!(
                downloaded = stats.downloaded,
                skipped = stats.skipped,
                failed = stats.failed,
                bytes = stats.bytes,
                "Telegram ingest complete"
            ),
            Err(e) => warn!(error = %e, "download_videos failed"),
        }

        if let Err(e) = store.index_dir(&root) {
            warn!(error = %e, "post-ingest reindex failed");
        }
    });
}
