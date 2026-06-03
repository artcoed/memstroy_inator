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
    /// asset library survives container restarts.
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

    let root = resolve_asset_root(cli.root)?;
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

fn resolve_asset_root(cli_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_root {
        return Ok(path);
    }
    let assets_root = env_path("ASSETS_ROOT");
    let railway_volume = env_path("RAILWAY_VOLUME_MOUNT_PATH");
    match (assets_root, railway_volume) {
        (Some(assets_root), Some(railway_volume)) => {
            if assets_root.starts_with(&railway_volume) {
                Ok(assets_root)
            } else {
                tracing::warn!(
                    assets_root = %assets_root.display(),
                    railway_volume = %railway_volume.display(),
                    "ASSETS_ROOT is outside the Railway volume; using RAILWAY_VOLUME_MOUNT_PATH"
                );
                Ok(railway_volume)
            }
        }
        (Some(assets_root), None) => Ok(assets_root),
        (None, Some(railway_volume)) => Ok(railway_volume),
        (None, None) => Ok(std::env::current_dir()
            .context("getting current dir")?
            .join("assets")),
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(name)?;
    let text = raw.to_string_lossy();
    if text.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore {
        name: &'static str,
        old: Option<OsString>,
    }

    impl EnvRestore {
        fn unset(name: &'static str) -> Self {
            let old = std::env::var_os(name);
            std::env::remove_var(name);
            Self { name, old }
        }

        fn set(name: &'static str, value: &str) -> Self {
            let old = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, old }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(old) = &self.old {
                std::env::set_var(self.name, old);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }

    #[test]
    fn asset_root_prefers_cli_then_assets_root_inside_railway_volume() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _assets = EnvRestore::set("ASSETS_ROOT", "/data/assets");
        let _volume = EnvRestore::set("RAILWAY_VOLUME_MOUNT_PATH", "/data");

        assert_eq!(
            resolve_asset_root(Some(PathBuf::from("/cli/assets"))).unwrap(),
            PathBuf::from("/cli/assets")
        );
        assert_eq!(
            resolve_asset_root(None).unwrap(),
            PathBuf::from("/data/assets")
        );
    }

    #[test]
    fn asset_root_uses_railway_volume_when_assets_root_is_outside_volume() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _assets = EnvRestore::set("ASSETS_ROOT", "/data/assets");
        let _volume = EnvRestore::set("RAILWAY_VOLUME_MOUNT_PATH", "/assets");

        assert_eq!(resolve_asset_root(None).unwrap(), PathBuf::from("/assets"));
    }

    #[test]
    fn asset_root_uses_railway_volume_when_assets_root_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _assets = EnvRestore::unset("ASSETS_ROOT");
        let _volume = EnvRestore::set("RAILWAY_VOLUME_MOUNT_PATH", "/mounted-volume");

        assert_eq!(
            resolve_asset_root(None).unwrap(),
            PathBuf::from("/mounted-volume")
        );
    }
}
