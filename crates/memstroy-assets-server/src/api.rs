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
        // Limit request body size to 10MB to prevent OOM
        .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024))
}

// ---------------------------------------------------------------------------
// /api/health
// ---------------------------------------------------------------------------

async fn health(State(store): State<AssetStore>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "count": store.count(),
        "version": env!("CARGO_PKG_VERSION"),
        "git_hash": option_env!("GIT_HASH").unwrap_or("unknown"),
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
// Reduced from 5000 to 500 for memory-constrained environments (500MB RAM).
// Large responses can cause OOM on small servers. Clients should paginate
// through the catalogue instead of requesting everything at once.
const MAX_LIMIT: u64 = 500;

// Maximum file size to load into memory at once (50MB).
// Files larger than this will be streamed chunk-by-chunk.
const MAX_IN_MEMORY_SIZE: u64 = 50 * 1024 * 1024;

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
    
    // Stream thumbnails to avoid loading large images into memory
    let file = tokio::fs::File::open(&thumb)
        .await
        .map_err(|e| {
            warn!(path = %thumb.display(), error = %e, "thumbnail open failed");
            ApiError::NotFound
        })?;
    
    let mime = mime_guess::from_path(&thumb).first_or_octet_stream();
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);
    
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"))
        .body(body)
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
    
    // Check file size and stream large files instead of loading into memory
    let metadata = tokio::fs::metadata(&entry.path).await.map_err(|e| {
        warn!(path = %entry.path.display(), error = %e, "metadata read failed");
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
    
    let file_size = metadata.len();
    
    // For small files, load into memory for better performance
    // For large files, stream to avoid OOM
    let body = if file_size <= MAX_IN_MEMORY_SIZE {
        let bytes = tokio::fs::read(&entry.path).await.map_err(|e| {
            warn!(path = %entry.path.display(), error = %e, "asset read failed");
            ApiError::NotFound
        })?;
        Body::from(bytes)
    } else {
        let file = tokio::fs::File::open(&entry.path).await.map_err(|e| {
            warn!(path = %entry.path.display(), error = %e, "asset open failed");
            ApiError::NotFound
        })?;
        let stream = tokio_util::io::ReaderStream::new(file);
        Body::from_stream(stream)
    };
    
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .body(body)
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
    
    // Check file size before loading
    let metadata = tokio::fs::metadata(&entry.path).await.map_err(|e| {
        warn!(path = %entry.path.display(), error = %e, "metadata read failed");
        ApiError::NotFound
    })?;
    
    // Limit text file size to prevent OOM (10MB max for text files)
    const MAX_TEXT_SIZE: u64 = 10 * 1024 * 1024;
    if metadata.len() > MAX_TEXT_SIZE {
        return Err(ApiError::BadRequest(
            format!("Text file too large: {} bytes (max {})", metadata.len(), MAX_TEXT_SIZE).into()
        ));
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
    // Reduced from 500 to 100 for memory-constrained environments (500MB RAM).
    // Large ingests can cause OOM on small servers. Users can run multiple
    // smaller ingests if needed.
    100
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
