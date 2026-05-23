//! HTTP handlers and the `axum::Router` that ties them together.
//!
//! The router is built once per server start and uses
//! [`crate::store::AssetStore`] as its application state. CORS is
//! permissive (`Any`/`Any`/`Any`) because the server is intentionally
//! "trust the network" — it's expected to live next to the editor on
//! a developer machine, not on the public internet.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

use crate::ingest;
use crate::model::{AssetKind, AssetSummary};
use crate::store::AssetStore;

/// Build the public router. Exposed via `crate::router` so tests can
/// drive it through `tower::ServiceExt::oneshot` without binding a
/// real socket.
pub fn router(store: AssetStore) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/api/health", get(health))
        .route("/api/assets", get(list_assets))
        .route("/api/assets/:id", get(get_asset))
        .route("/api/assets/:id/preview", get(get_preview))
        .route("/api/assets/:id/download", get(get_download))
        .route("/api/assets/:id/text", get(get_text))
        .route("/api/ingest/tg", post(post_ingest_tg))
        .with_state(store)
        .layer(cors)
}

// ---------------------------------------------------------------------------
// /api/health
// ---------------------------------------------------------------------------

async fn health(State(store): State<AssetStore>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "count": store.count(),
    }))
}

// ---------------------------------------------------------------------------
// /api/assets (listing)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// Comma-separated list of kinds to include. Repeated `?kind=` is
    /// also accepted (axum collapses to the last one for `Option`, so
    /// we encourage callers to use the comma form).
    kind: Option<String>,
    /// Case-insensitive substring filter.
    q: Option<String>,
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
}

const DEFAULT_LIMIT: u64 = 24;
// Lifted from 200 → 5000 because the GUI's "Refresh from Telegram"
// flow asks for the **whole** clip catalogue (`?kind=clip&limit=…`)
// in a single call so it can mirror new files into its local cache.
// With a 200-row cap, channels that contained 400+ clips silently
// got truncated and the user only ever saw the first 200 — exactly
// the "клипы с сервера не подгружаются" symptom they reported.
// 5000 is a comfortable headroom for any single Telegram channel
// while still keeping a single response under a few MB.
const MAX_LIMIT: u64 = 5000;

#[derive(Debug, Serialize)]
struct ListResponse {
    total: u64,
    offset: u64,
    items: Vec<AssetSummary>,
}

async fn list_assets(
    State(store): State<AssetStore>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let kinds = parse_kind_list(q.kind.as_deref());
    let limit = q
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    let (total, page) = store.filtered(&kinds, q.q.as_deref(), q.offset, limit);
    let items = page.iter().map(AssetSummary::from_entry).collect();

    Ok(Json(ListResponse {
        total,
        offset: q.offset,
        items,
    }))
}

fn parse_kind_list(raw: Option<&str>) -> Vec<AssetKind> {
    let Some(raw) = raw else { return Vec::new() };
    raw.split(',')
        .filter_map(|t| AssetKind::parse_token(t))
        .collect()
}

// ---------------------------------------------------------------------------
// /api/assets/:id  (full record)
// ---------------------------------------------------------------------------

async fn get_asset(
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Json<crate::model::AssetEntry>, ApiError> {
    store
        .get(&id)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

// ---------------------------------------------------------------------------
// /api/assets/:id/preview
// ---------------------------------------------------------------------------

async fn get_preview(
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let entry = store.get(&id).ok_or(ApiError::NotFound)?;
    let thumb = entry.thumbnail.ok_or(ApiError::NotFound)?;
    let bytes = tokio::fs::read(&thumb)
        .await
        .map_err(|e| {
            warn!(path = %thumb.display(), error = %e, "thumbnail read failed");
            ApiError::NotFound
        })?;
    let mime = mime_guess::from_path(&thumb).first_or_octet_stream();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"))
        .body(Body::from(bytes))
        .expect("valid response"))
}

// ---------------------------------------------------------------------------
// /api/assets/:id/download
// ---------------------------------------------------------------------------

async fn get_download(
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let entry = store.get(&id).ok_or(ApiError::NotFound)?;
    let bytes = tokio::fs::read(&entry.path).await.map_err(|e| {
        warn!(path = %entry.path.display(), error = %e, "asset read failed");
        ApiError::NotFound
    })?;
    let mime = mime_guess::from_path(&entry.path).first_or_octet_stream();
    let filename = entry
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_filename)
        .unwrap_or_else(|| entry.id.clone());
    let disposition = format!("attachment; filename=\"{}\"", filename);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(Body::from(bytes))
        .expect("valid response"))
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' | '\r' | '\n' => '_',
            other => other,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// /api/assets/:id/text
// ---------------------------------------------------------------------------

async fn get_text(
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let entry = store.get(&id).ok_or(ApiError::NotFound)?;
    if entry.kind != AssetKind::Text {
        return Err(ApiError::NotFound);
    }
    let body = tokio::fs::read_to_string(&entry.path)
        .await
        .map_err(|e| {
            warn!(path = %entry.path.display(), error = %e, "text read failed");
            ApiError::NotFound
        })?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .body(Body::from(body))
        .expect("valid response"))
}

// ---------------------------------------------------------------------------
// /api/ingest/tg
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TgIngestRequest {
    channel: String,
    #[serde(default = "default_ingest_limit")]
    limit: u32,
}

fn default_ingest_limit() -> u32 {
    // Default catalog depth for a fresh ingest. The previous default
    // of 32 capped channels at the most recent ~32 posts, which made
    // the "Refresh from Telegram" button feel anaemic on rich
    // channels that already have hundreds of clips. 500 is enough to
    // pull the typical multi-year backlog while still respecting
    // Telegram's preview-page rate (≈16 posts per page → ~32 page
    // fetches with the existing 250 ms delay between pages).
    500
}

async fn post_ingest_tg(
    State(store): State<AssetStore>,
    Json(body): Json<TgIngestRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let channel = body.channel.trim().to_string();
    if channel.is_empty() {
        return Err(ApiError::BadRequest("channel must not be empty".into()));
    }
    let limit = body.limit.max(1);
    ingest::spawn_tg_ingest(store, channel.clone(), limit);
    Ok(Json(json!({
        "started": true,
        "channel": channel,
        "limit": limit,
    })))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ApiError {
    NotFound,
    BadRequest(Arc<str>),
}

impl From<String> for ApiError {
    fn from(s: String) -> Self {
        ApiError::BadRequest(Arc::from(s.into_boxed_str()))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "not_found" })),
            )
                .into_response(),
            ApiError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_request", "message": msg.as_ref() })),
            )
                .into_response(),
        }
    }
}
