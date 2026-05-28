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

// ---------------------------------------------------------------------------
// Embedded placeholder assets
// ---------------------------------------------------------------------------
//
// The asset volume on production deployments can be empty (fresh redeploy,
// volume reset, lost data) while editor clients still hold references to
// asset ids from previous sessions. To keep drag-and-drop usable in that
// situation we ship a tiny set of placeholder files inside the binary and
// serve them whenever the real asset is missing. This is intentionally
// permissive — the server is "trust the network" and prefers a working
// download over a strict 404.

/// 1-second 640x360 H.264 MP4 placeholder used for missing clips/videos.
const FALLBACK_VIDEO: &[u8] = include_bytes!("../assets/fallback.mp4");

/// JPEG thumbnail used for missing previews.
const FALLBACK_IMAGE: &[u8] = include_bytes!("../assets/fallback.jpg");

/// 1-second silent WAV used for missing sound assets.
const FALLBACK_SOUND: &[u8] = include_bytes!("../assets/fallback.wav");

fn fallback_entry(id: &str, kind: AssetKind) -> crate::model::AssetEntry {
    use std::path::PathBuf;
    let (ext, size) = match kind {
        AssetKind::Sound => ("wav", FALLBACK_SOUND.len() as u64),
        AssetKind::Image => ("jpg", FALLBACK_IMAGE.len() as u64),
        AssetKind::Particle => ("json", 0),
        AssetKind::Text => ("txt", 0),
        AssetKind::Clip | AssetKind::Video => ("mp4", FALLBACK_VIDEO.len() as u64),
    };
    crate::model::AssetEntry {
        id: id.to_string(),
        kind,
        label: id.to_string(),
        description: String::new(),
        path: PathBuf::from(format!("placeholder/{}.{}", id, ext)),
        thumbnail: None,
        size_bytes: size,
        tags: Vec::new(),
    }
}

fn fallback_download_response(id: &str, kind: AssetKind) -> Response {
    let (bytes, mime, ext): (&'static [u8], &'static str, &'static str) = match kind {
        AssetKind::Sound => (FALLBACK_SOUND, "audio/wav", "wav"),
        AssetKind::Image => (FALLBACK_IMAGE, "image/jpeg", "jpg"),
        AssetKind::Particle => (b"{}", "application/json", "json"),
        AssetKind::Text => (b"", "text/plain; charset=utf-8", "txt"),
        AssetKind::Clip | AssetKind::Video => (FALLBACK_VIDEO, "video/mp4", "mp4"),
    };
    let disposition = format!("attachment; filename=\"{}.{}\"", sanitize_filename(id), ext);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(
            "x-memstroy-placeholder",
            HeaderValue::from_static("missing-asset"),
        )
        .body(Body::from(bytes))
        .expect("valid response")
}

fn fallback_preview_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=60"),
        )
        .header(
            "x-memstroy-placeholder",
            HeaderValue::from_static("missing-asset"),
        )
        .body(Body::from(FALLBACK_IMAGE))
        .expect("valid response")
}

fn fallback_text_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .header(
            "x-memstroy-placeholder",
            HeaderValue::from_static("missing-asset"),
        )
        .body(Body::empty())
        .expect("valid response")
}

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
        .route("/api/test-metadata", post(test_metadata))
        .route("/api/cleanup", post(cleanup_old_files))
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
    Ok(Json(
        store
            .get(&id)
            .unwrap_or_else(|| fallback_entry(&id, AssetKind::Clip)),
    ))
}

// ---------------------------------------------------------------------------
// /api/assets/:id/preview
// ---------------------------------------------------------------------------

async fn get_preview(
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let Some(entry) = store.get(&id) else {
        return Ok(fallback_preview_response());
    };
    let Some(thumb) = entry.thumbnail else {
        return Ok(fallback_preview_response());
    };

    // Stream thumbnails to avoid loading large images into memory
    let file = match tokio::fs::File::open(&thumb).await {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %thumb.display(), error = %e, "thumbnail open failed; serving placeholder");
            return Ok(fallback_preview_response());
        }
    };

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
    let Some(entry) = store.get(&id) else {
        return Ok(fallback_download_response(&id, AssetKind::Clip));
    };

    // Check file size and stream large files instead of loading into memory
    let metadata = match tokio::fs::metadata(&entry.path).await {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "metadata read failed; serving placeholder");
            return Ok(fallback_download_response(&entry.id, entry.kind));
        }
    };

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
        match tokio::fs::read(&entry.path).await {
            Ok(bytes) => Body::from(bytes),
            Err(e) => {
                warn!(path = %entry.path.display(), error = %e, "asset read failed; serving placeholder");
                return Ok(fallback_download_response(&entry.id, entry.kind));
            }
        }
    } else {
        match tokio::fs::File::open(&entry.path).await {
            Ok(file) => {
                let stream = tokio_util::io::ReaderStream::new(file);
                Body::from_stream(stream)
            }
            Err(e) => {
                warn!(path = %entry.path.display(), error = %e, "asset open failed; serving placeholder");
                return Ok(fallback_download_response(&entry.id, entry.kind));
            }
        }
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
    let Some(entry) = store.get(&id) else {
        return Ok(fallback_text_response());
    };
    if entry.kind != AssetKind::Text {
        return Err(ApiError::NotFound);
    }

    // Check file size before loading
    let metadata = match tokio::fs::metadata(&entry.path).await {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "metadata read failed; serving placeholder");
            return Ok(fallback_text_response());
        }
    };

    // Limit text file size to prevent OOM (10MB max for text files)
    const MAX_TEXT_SIZE: u64 = 10 * 1024 * 1024;
    if metadata.len() > MAX_TEXT_SIZE {
        return Err(ApiError::BadRequest(
            format!("Text file too large: {} bytes (max {})", metadata.len(), MAX_TEXT_SIZE).into()
        ));
    }

    let body = match tokio::fs::read_to_string(&entry.path).await {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "text read failed; serving placeholder");
            return Ok(fallback_text_response());
        }
    };
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
    // Limited to 10 clips to prevent excessive downloads and disk usage.
    // Users can run multiple smaller ingests if needed.
    10
}

async fn post_ingest_tg(
    State(store): State<AssetStore>,
    Json(body): Json<TgIngestRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let channel = body.channel.trim().to_string();
    if channel.is_empty() {
        return Err(ApiError::BadRequest("channel must not be empty".into()));
    }
    // Limit to maximum 10 clips per request
    let limit = body.limit.max(1).min(10);
    ingest::spawn_tg_ingest(store, channel.clone(), limit);
    Ok(Json(json!({
        "started": true,
        "channel": channel,
        "limit": limit,
    })))
}

// Test endpoint to verify metadata creation works
async fn test_metadata(State(store): State<AssetStore>) -> Result<Json<serde_json::Value>, ApiError> {
    let root = store.root();
    let clips_dir = root.join("clips");
    let test_id = "test123";
    let test_txt = clips_dir.join(format!("{}.txt", test_id));
    let test_content = "Test metadata content";
    
    match tokio::fs::write(&test_txt, test_content.as_bytes()).await {
        Ok(_) => {
            // Verify file exists
            let exists = test_txt.exists();
            let content = tokio::fs::read_to_string(&test_txt).await.unwrap_or_default();
            
            // Reindex to pick up the test file
            let _ = store.index_dir(&root);
            
            Ok(Json(json!({
                "success": true,
                "test_file": test_txt.display().to_string(),
                "exists": exists,
                "content": content,
                "clips_dir": clips_dir.display().to_string(),
            })))
        }
        Err(e) => Ok(Json(json!({
            "success": false,
            "error": e.to_string(),
            "clips_dir": clips_dir.display().to_string(),
        })))
    }
}

// Manual cleanup endpoint to delete all clips and free disk space
async fn cleanup_old_files(State(store): State<AssetStore>) -> Result<Json<serde_json::Value>, ApiError> {
    let root = store.root();
    let clips_dir = root.join("clips");
    let thumbs_dir = clips_dir.join("thumbs");
    
    let mut deleted_files = 0u64;
    let mut freed_bytes = 0u64;
    
    // Delete all files in clips/ directory
    if clips_dir.exists() {
        let mut entries = tokio::fs::read_dir(&clips_dir).await.map_err(|e| {
            ApiError::BadRequest(format!("Failed to read clips directory: {}", e).into())
        })?;
        
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            ApiError::BadRequest(format!("Failed to read directory entry: {}", e).into())
        })? {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    freed_bytes += metadata.len();
                }
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(path = %path.display(), error = %e, "failed to delete file");
                } else {
                    deleted_files += 1;
                }
            }
        }
    }
    
    // Delete all thumbnails
    if thumbs_dir.exists() {
        let mut entries = tokio::fs::read_dir(&thumbs_dir).await.map_err(|e| {
            ApiError::BadRequest(format!("Failed to read thumbs directory: {}", e).into())
        })?;
        
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            ApiError::BadRequest(format!("Failed to read directory entry: {}", e).into())
        })? {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = tokio::fs::metadata(&path).await {
                    freed_bytes += metadata.len();
                }
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!(path = %path.display(), error = %e, "failed to delete thumbnail");
                } else {
                    deleted_files += 1;
                }
            }
        }
    }
    
    // Reindex after cleanup
    let _ = store.index_dir(&root);
    
    Ok(Json(json!({
        "success": true,
        "deleted_files": deleted_files,
        "freed_bytes": freed_bytes,
        "freed_mb": freed_bytes / 1024 / 1024,
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
