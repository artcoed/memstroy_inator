//! Binary entry point for `memstroy-assets-server`.
//!
//! Parses CLI args, builds the [`AssetStore`], indexes the persistent
//! asset root once, and then runs the axum server until it returns.

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
    /// asset library survives container restarts (e.g. `--root /data/assets`).
    #[arg(long)]
    root: Option<PathBuf>,
}

#[tokio::main]
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
    for kind in memstroy_assets_server::AssetKind::ALL {
        tokio::fs::create_dir_all(root.join(kind.subdir()))
            .await
            .with_context(|| format!("creating {} directory", kind.subdir()))?;
    }

    let store = AssetStore::new();
    store.index_dir(&root)?;

    for (kind, count) in store.count_by_kind() {
        tracing::info!(kind = ?kind, count, "indexed");
    }

    tracing::info!(root = %root.display(), "persistent asset volume ready");

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
