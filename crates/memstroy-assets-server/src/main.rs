//! Binary entry point for `memstroy-assets-server`.
//!
//! Parses CLI args, builds the [`AssetStore`], walks the asset root
//! once, logs how many entries were found per kind, and then runs the
//! axum server until it returns.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use memstroy_assets_server::{start, AssetStore};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "memstroy-assets-server",
    version,
    about = "HTTP server that serves shared editor assets to the GUI"
)]
struct Cli {
    /// Address to bind the HTTP server on. When `PORT` env var is set
    /// (e.g. on Railway / Heroku-style hosts), the server binds to
    /// `0.0.0.0:$PORT` regardless of this flag.
    #[arg(long, default_value = "0.0.0.0:8765")]
    addr: SocketAddr,

    /// Asset root directory. Expected to contain `clips/`, `videos/`,
    /// `images/`, `sounds/`, `particles/`, `text/` subdirectories.
    /// Missing subdirectories are created on start-up.
    ///
    /// On Railway this should point at a mounted volume so the
    /// scraped asset library survives container restarts (e.g.
    /// `--root /data/assets`).
    #[arg(long)]
    root: Option<PathBuf>,

    /// Telegram channel to scrape automatically on startup, right
    /// after the cleanup + initial index. Pairs with the startup
    /// cleanup: cleanup wipes the old library, auto-ingest immediately
    /// repopulates it with fresh clips. If omitted (and
    /// `MEMSTROY_INGEST_CHANNEL` is also unset), auto-ingest is
    /// skipped and the server boots with whatever survived cleanup —
    /// the `POST /api/ingest/tg` endpoint still works for manual
    /// invocations.
    #[arg(long)]
    ingest_channel: Option<String>,

    /// How many recent clips the auto-ingest job should fetch.
    /// Matches the default used by `POST /api/ingest/tg`. The env
    /// fallback is `MEMSTROY_INGEST_LIMIT`.
    #[arg(long)]
    ingest_limit: Option<u32>,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // Honour PORT env var for cloud platforms (Railway, Heroku, etc.).
    // These platforms inject `PORT=12345` and expect the app to listen
    // on `0.0.0.0:12345`. Falls back to the CLI default otherwise.
    let addr: SocketAddr = match std::env::var("PORT") {
        Ok(p) => format!("0.0.0.0:{}", p)
            .parse()
            .with_context(|| format!("parsing PORT={p}"))?,
        Err(_) => cli.addr,
    };

    let root = match cli.root {
        Some(p) => p,
        None => match std::env::var("ASSETS_ROOT") {
            Ok(p) => PathBuf::from(p),
            Err(_) => std::env::current_dir()
                .context("getting current dir")?
                .join("assets"),
        },
    };
    tokio::fs::create_dir_all(&root)
        .await
        .with_context(|| format!("creating asset root {}", root.display()))?;

    // Auto-cleanup on startup to free disk space (critical for 500MB Railway volume)
    tracing::info!("Running startup cleanup to free disk space...");
    if let Err(e) = cleanup_old_clips(&root).await {
        tracing::warn!(error = %e, "startup cleanup failed, continuing anyway");
    }

    let store = AssetStore::new();
    store.index_dir(&root)?;

    for (kind, count) in store.count_by_kind() {
        tracing::info!(kind = ?kind, count, "indexed");
    }

    // Kick off the Telegram ingest job on startup if a channel is
    // configured. Pairs with the startup cleanup above: cleanup
    // intentionally wipes the old library on every boot, and this
    // immediately starts pulling fresh clips so the server is useful
    // again as quickly as possible. The ingest task runs in the
    // background — it does not block the HTTP server from coming up,
    // and the store is re-indexed by the task itself when downloads
    // finish.
    let ingest_channel = cli
        .ingest_channel
        .or_else(|| std::env::var("MEMSTROY_INGEST_CHANNEL").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ingest_limit = cli
        .ingest_limit
        .or_else(|| {
            std::env::var("MEMSTROY_INGEST_LIMIT")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
        })
        .unwrap_or(10);

    match ingest_channel {
        Some(channel) => {
            tracing::info!(
                channel = %channel,
                limit = ingest_limit,
                "auto-ingest: scheduling Telegram scrape on startup",
            );
            let _ = memstroy_assets_server::ingest::try_spawn_tg_ingest(
                store.clone(),
                channel,
                ingest_limit,
            );
        }
        None => {
            tracing::info!(
                "auto-ingest: no channel configured \
                 (set --ingest-channel or MEMSTROY_INGEST_CHANNEL to enable); \
                 manual POST /api/ingest/tg still works",
            );
        }
    }

    tracing::info!(
        "Server optimized for low-memory environments (500MB RAM):\n\
         - File streaming enabled for files >50MB\n\
         - Max response limit: 500 items\n\
         - Max ingest batch: 100 items\n\
         - Concurrent downloads: 2\n\
         - Worker threads: 2"
    );

    // Warn loudly when the mutating endpoints are left unauthenticated. Safe by
    // default for LAN/localhost use; a public (e.g. Railway) deployment should
    // set MEMSTROY_API_TOKEN so /api/cleanup and /api/ingest/tg require a token.
    if memstroy_assets_server::api::configured_api_token().is_none() {
        tracing::warn!(
            "MEMSTROY_API_TOKEN is not set — the mutating endpoints \
             (/api/cleanup, /api/ingest/tg) are UNAUTHENTICATED. Set \
             MEMSTROY_API_TOKEN to require 'Authorization: Bearer <token>' on a \
             public deployment."
        );
    }

    let handle = start(addr, store);
    
    // Wait for Ctrl+C signal
    tokio::select! {
        result = handle => {
            result.context("server task crashed")?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal, exiting...");
        }
    }
    
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,memstroy_assets_server=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Delete all clips and their metadata to free disk space.
/// This runs automatically on server startup to ensure we have space for new ingests.
async fn cleanup_old_clips(root: &PathBuf) -> Result<()> {
    let clips_dir = root.join("clips");
    let thumbs_dir = clips_dir.join("thumbs");
    
    let mut deleted_files = 0u64;
    let mut freed_bytes = 0u64;
    
    // Delete all files in clips/ directory
    if clips_dir.exists() {
        let mut entries = tokio::fs::read_dir(&clips_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    freed_bytes += metadata.len();
                }
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "failed to delete file");
                } else {
                    deleted_files += 1;
                }
            }
        }
    }
    
    // Delete all thumbnails
    if thumbs_dir.exists() {
        let mut entries = tokio::fs::read_dir(&thumbs_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    freed_bytes += metadata.len();
                }
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "failed to delete thumbnail");
                } else {
                    deleted_files += 1;
                }
            }
        }
    }
    
    tracing::info!(
        deleted_files,
        freed_mb = freed_bytes / 1024 / 1024,
        "Startup cleanup complete"
    );
    
    Ok(())
}
