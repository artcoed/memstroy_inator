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
    /// Address to bind the HTTP server on.
    #[arg(long, default_value = "0.0.0.0:8765")]
    addr: SocketAddr,

    /// Asset root directory. Expected to contain `clips/`, `videos/`,
    /// `images/`, `sounds/`, `particles/`, `text/` subdirectories.
    /// Missing subdirectories are created on start-up.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let root = match cli.root {
        Some(p) => p,
        None => std::env::current_dir()
            .context("getting current dir")?
            .join("assets"),
    };
    tokio::fs::create_dir_all(&root)
        .await
        .with_context(|| format!("creating asset root {}", root.display()))?;

    let store = AssetStore::new();
    store.index_dir(&root)?;

    for (kind, count) in store.count_by_kind() {
        tracing::info!(kind = ?kind, count, "indexed");
    }

    let handle = start(cli.addr, store);
    handle.await.context("server task crashed")?;
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
