//! # memstroy-assets-server
//!
//! HTTP service that exposes the project's shared asset library to the
//! editor GUI in a *lazy* fashion: the GUI never has to walk the disk
//! itself, never has to load every clip into memory, and can fetch
//! thumbnails / metadata on demand as the user scrolls through the
//! sidebar.
//!
//! ## Why a separate process?
//!
//! The editor (`memstroy-gui`) and the headless renderer
//! (`memstroy-render`) both want to consume the same library of clips,
//! images, sounds, particles and snippet templates. Rather than have
//! every tool bake in its own filesystem index, this crate runs a tiny
//! `axum`-based HTTP server in front of the on-disk asset tree. The
//! server is the single source of truth and:
//!
//! * builds an in-memory index on start-up by walking the configured
//!   asset root,
//! * serves paginated, kind-filtered, full-text-searchable summaries
//!   over `GET /api/assets`,
//! * streams thumbnails and full asset bodies on demand so the editor
//!   can keep its working set tiny,
//! * accepts admin upload requests via `POST /api/admin/assets`,
//!   persists the files and sidecar metadata under the configured
//!   asset root, and refreshes the index immediately.
//!
//! There is no authentication. The server is meant to be reachable
//! only from the developer's own LAN (or `localhost` next to the GUI).
//!
//! ## Public surface
//!
//! [`start`] spawns the server on a tokio runtime and returns a
//! [`tokio::task::JoinHandle`]. [`router`] returns the underlying
//! [`axum::Router`] for tests that want to drive the service in-process
//! without binding a TCP listener.

#![warn(rust_2018_idioms)]

pub mod api;
pub mod model;
pub mod store;

pub use model::{AssetEntry, AssetKind, AssetSummary};
pub use store::AssetStore;

use std::net::SocketAddr;
use tokio::task::JoinHandle;

/// Build the [`axum::Router`] that backs the public HTTP API. Useful
/// when you want to drive the service via `tower::ServiceExt::oneshot`
/// for tests instead of going through a real TCP socket.
pub fn router(store: AssetStore) -> axum::Router {
    api::router(store)
}

/// Start the HTTP server on the given address with the given asset
/// store. The returned [`JoinHandle`] resolves when the server stops
/// (either gracefully or because of a fatal error such as a failed
/// bind). Errors are logged with `tracing::error!`; this function does
/// not propagate them so the caller can decide whether to keep
/// running.
///
pub fn start(addr: SocketAddr, store: AssetStore) -> JoinHandle<()> {
    let app = router(store);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, %addr, "failed to bind assets server");
                return;
            }
        };
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| addr.to_string());
        tracing::info!(addr = %local, "memstroy-assets-server listening");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "assets server crashed");
        }
    })
}
