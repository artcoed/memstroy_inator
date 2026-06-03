//! HTTP handlers and the `axum::Router` that ties them together.
//!
//! The router is built once per server start and uses
//! [`crate::store::AssetStore`] as its application state. CORS is
//! permissive (`Any`/`Any`/`Any`) because the server is intentionally
//! "trust the network" — it's expected to live next to the editor on
//! a developer machine, not on the public internet.

use std::io::SeekFrom;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use crate::bucket::{env_truthy, BucketStore};
use crate::model::{AssetKind, AssetSummary};
use crate::store::{ensure_asset_derivatives, entry_from_primary, AssetStore};

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

const DEFAULT_MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct AppState {
    store: AssetStore,
    bucket: Option<Arc<BucketStore>>,
}

impl AppState {
    fn new(store: AssetStore, bucket: Option<BucketStore>) -> Self {
        Self {
            store,
            bucket: bucket.map(Arc::new),
        }
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
    router_with_bucket(store, None)
}

/// Build the public router backed by an optional S3-compatible bucket.
pub fn router_with_bucket(store: AssetStore, bucket: Option<BucketStore>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    let state = AppState::new(store, bucket);

    Router::new()
        .route("/api/health", get(health))
        .route("/api/assets", get(list_assets))
        .route("/api/assets/:id", get(get_asset))
        .route("/api/assets/:id/preview", get(get_preview))
        .route("/api/assets/:id/download", get(get_download))
        .route("/api/assets/:id/text", get(get_text))
        .route("/api/admin/assets", post(post_admin_asset))
        .with_state(state)
        .layer(cors)
        .layer(DefaultBodyLimit::disable())
}

// ---------------------------------------------------------------------------
// /api/health
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let store = &state.store;
    let root = store.root();
    let root_metadata = tokio::fs::metadata(&root).await.ok();
    let railway_volume_mount_path = env_string("RAILWAY_VOLUME_MOUNT_PATH");
    let root_inside_railway_volume = railway_volume_mount_path.as_deref().map(|mount| {
        let mount = FsPath::new(mount);
        !mount.as_os_str().is_empty() && root.starts_with(mount)
    });
    let counts_by_kind: Vec<_> = store
        .count_by_kind()
        .into_iter()
        .map(|(kind, count)| json!({ "kind": kind, "count": count }))
        .collect();

    let bucket = state.bucket.as_ref();
    Json(json!({
        "ok": true,
        "storage_backend": if bucket.is_some() { "bucket" } else { "local" },
        "bucket_configured": bucket.is_some(),
        "bucket_name": bucket.map(|b| b.bucket_name()),
        "bucket_endpoint": bucket.map(|b| b.endpoint_url()),
        "bucket_region": bucket.map(|b| b.region()),
        "bucket_presign_secs": bucket.map(|b| b.presign_secs()),
        "count": store.count(),
        "counts_by_kind": counts_by_kind,
        "asset_root": root,
        "asset_root_exists": root_metadata.is_some(),
        "asset_root_is_dir": root_metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        "asset_root_writable": root_writable(&store.root()).await,
        "assets_root_env": env_string("ASSETS_ROOT"),
        "railway_volume_mount_path": railway_volume_mount_path,
        "root_inside_railway_volume": root_inside_railway_volume,
        "admin_token_configured": configured_admin_token().is_some(),
        "max_upload_bytes": max_upload_bytes(),
        "version": env!("CARGO_PKG_VERSION"),
        "git_hash": option_env!("GIT_HASH").unwrap_or("unknown"),
    }))
}

fn env_string(name: &str) -> Option<String> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

async fn root_writable(root: &FsPath) -> bool {
    if root.as_os_str().is_empty() {
        return false;
    }
    if tokio::fs::create_dir_all(root).await.is_err() {
        return false;
    }
    let probe = root.join(format!(".healthcheck-{}.tmp", monotonic_stamp()));
    match tokio::fs::write(&probe, b"ok").await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&probe).await;
            true
        }
        Err(_) => false,
    }
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
const MAX_LIMIT: u64 = 5000;

#[derive(Debug, Serialize)]
struct ListResponse {
    total: u64,
    offset: u64,
    limit: u64,
    has_more: bool,
    items: Vec<AssetSummary>,
}

async fn list_assets(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let kinds = parse_kind_list(q.kind.as_deref());
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let (total, page) = state
        .store
        .filtered(&kinds, q.q.as_deref(), q.offset, limit);
    let items: Vec<AssetSummary> = page.iter().map(AssetSummary::from_entry).collect();
    let has_more = q.offset.saturating_add(items.len() as u64) < total;

    Ok(Json(ListResponse {
        total,
        offset: q.offset,
        limit,
        has_more,
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
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::model::AssetEntry>, ApiError> {
    state.store.get(&id).map(Json).ok_or(ApiError::NotFound)
}

// ---------------------------------------------------------------------------
// /api/assets/:id/preview
// ---------------------------------------------------------------------------

async fn get_preview(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(entry) = state.store.get(&id) else {
        return Ok(fallback_preview_response());
    };
    if let (Some(bucket), Some(key)) =
        (state.bucket.as_ref(), entry.thumbnail_object_key.as_deref())
    {
        return redirect_to_presigned(bucket, key, "preview")
            .await
            .or_else(|e| {
                warn!(id = %entry.id, key, error = ?e, "bucket preview redirect failed; serving placeholder");
                Ok(fallback_preview_response())
            });
    }
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

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %thumb.display(), error = %e, "thumbnail metadata failed; serving placeholder");
            return Ok(fallback_preview_response());
        }
    };
    let etag = weak_etag(&metadata);
    if request_etag_matches(&headers, &etag) {
        return Ok(not_modified_response(&etag));
    }

    let mime = mime_guess::from_path(&thumb).first_or_octet_stream();
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CONTENT_LENGTH, metadata.len().to_string())
        .header(header::ETAG, etag)
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=86400, immutable"),
        )
        .body(body)
        .expect("valid response"))
}

// ---------------------------------------------------------------------------
// /api/assets/:id/download
// ---------------------------------------------------------------------------

async fn get_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(entry) = state.store.get(&id) else {
        return Ok(fallback_download_response(&id, AssetKind::Clip));
    };
    if let (Some(bucket), Some(key)) = (state.bucket.as_ref(), entry.object_key.as_deref()) {
        return redirect_to_presigned(bucket, key, "download")
            .await
            .or_else(|e| {
                warn!(id = %entry.id, key, error = ?e, "bucket download redirect failed; serving placeholder");
                Ok(fallback_download_response(&entry.id, entry.kind))
            });
    }

    // Check file size and stream large files instead of loading into memory
    let metadata = match tokio::fs::metadata(&entry.path).await {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "metadata read failed; serving placeholder");
            return Ok(fallback_download_response(&entry.id, entry.kind));
        }
    };

    stream_file_response(&entry.path, &entry.id, &headers, metadata)
        .await
        .map_err(|e| {
            warn!(path = %entry.path.display(), error = ?e, "asset stream failed; serving placeholder");
            e
        })
        .or_else(|_| Ok(fallback_download_response(&entry.id, entry.kind)))
}

async fn redirect_to_presigned(
    bucket: &BucketStore,
    key: &str,
    purpose: &'static str,
) -> Result<Response, ApiError> {
    let url = bucket
        .presigned_get_url(key)
        .await
        .map_err(|e| ApiError::BadRequest(format!("presign bucket object: {e}").into()))?;
    let location = HeaderValue::from_str(&url)
        .map_err(|e| ApiError::BadRequest(format!("invalid presigned URL: {e}").into()))?;
    Ok(Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, location)
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        )
        .header("x-memstroy-storage", HeaderValue::from_static("bucket"))
        .header(
            "x-memstroy-redirect-purpose",
            HeaderValue::from_static(purpose),
        )
        .body(Body::empty())
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

async fn stream_file_response(
    path: &FsPath,
    id: &str,
    headers: &HeaderMap,
    metadata: std::fs::Metadata,
) -> Result<Response, ApiError> {
    let file_size = metadata.len();
    let etag = weak_etag(&metadata);
    if request_etag_matches(headers, &etag) {
        return Ok(not_modified_response(&etag));
    }

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_filename)
        .unwrap_or_else(|| id.to_string());
    let disposition = format!("attachment; filename=\"{}\"", filename);

    let range = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .and_then(|raw| parse_byte_range(raw, file_size));

    let Some((start, end)) = range else {
        if headers.get(header::RANGE).is_some() {
            return Ok(range_not_satisfiable(file_size));
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| ApiError::BadRequest(format!("open asset: {e}").into()))?;
        let stream = tokio_util::io::ReaderStream::new(file);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CONTENT_DISPOSITION, disposition)
            .header(header::CONTENT_LENGTH, file_size.to_string())
            .header(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            )
            .header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"))
            .header(header::ETAG, etag)
            .body(Body::from_stream(stream))
            .expect("valid response"));
    };

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::BadRequest(format!("open asset: {e}").into()))?;
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(|e| ApiError::BadRequest(format!("seek asset: {e}").into()))?;
    let read_len = end.saturating_sub(start).saturating_add(1);
    let stream = tokio_util::io::ReaderStream::new(file.take(read_len));
    Ok(Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CONTENT_LENGTH, read_len.to_string())
        .header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{file_size}"),
        )
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=86400"),
        )
        .header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .header(header::ETAG, etag)
        .body(Body::from_stream(stream))
        .expect("valid response"))
}

fn parse_byte_range(raw: &str, file_size: u64) -> Option<(u64, u64)> {
    if file_size == 0 {
        return None;
    }
    let spec = raw.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start_raw, end_raw) = spec.split_once('-')?;
    if start_raw.is_empty() {
        let suffix_len = end_raw.trim().parse::<u64>().ok()?;
        if suffix_len == 0 {
            return None;
        }
        let start = file_size.saturating_sub(suffix_len);
        return Some((start, file_size - 1));
    }
    let start = start_raw.trim().parse::<u64>().ok()?;
    if start >= file_size {
        return None;
    }
    let end = if end_raw.trim().is_empty() {
        file_size - 1
    } else {
        end_raw.trim().parse::<u64>().ok()?.min(file_size - 1)
    };
    (start <= end).then_some((start, end))
}

fn weak_etag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("W/\"{:x}-{:x}\"", metadata.len(), modified)
}

fn request_etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .map(|raw| {
            raw.split(',')
                .any(|part| part.trim() == etag || part.trim() == "*")
        })
        .unwrap_or(false)
}

fn not_modified_response(etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=86400"),
        )
        .body(Body::empty())
        .expect("valid response")
}

fn range_not_satisfiable(file_size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{file_size}"))
        .header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"))
        .body(Body::empty())
        .expect("valid response")
}

// ---------------------------------------------------------------------------
// /api/assets/:id/text
// ---------------------------------------------------------------------------

async fn get_text(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let Some(entry) = state.store.get(&id) else {
        return Ok(fallback_text_response());
    };
    if entry.kind != AssetKind::Text {
        return Err(ApiError::NotFound);
    }

    if let (Some(bucket), Some(key)) = (state.bucket.as_ref(), entry.object_key.as_deref()) {
        const MAX_TEXT_SIZE: usize = 25 * 1024 * 1024;
        let body = match bucket.get_small_text(key, MAX_TEXT_SIZE).await {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(fallback_text_response()),
            Err(e) => {
                warn!(id = %entry.id, key, error = %e, "bucket text read failed; serving placeholder");
                return Ok(fallback_text_response());
            }
        };
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )
            .body(Body::from(body))
            .expect("valid response"));
    }

    let metadata = match tokio::fs::metadata(&entry.path).await {
        Ok(m) => m,
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "metadata read failed; serving placeholder");
            return Ok(fallback_text_response());
        }
    };

    const MAX_TEXT_SIZE: u64 = 25 * 1024 * 1024;
    if metadata.len() > MAX_TEXT_SIZE {
        return Err(ApiError::BadRequest(
            format!(
                "Text file too large: {} bytes (max {})",
                metadata.len(),
                MAX_TEXT_SIZE
            )
            .into(),
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
// /api/admin/assets
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AdminAssetResponse {
    created: bool,
    asset: AssetSummary,
    storage_backend: String,
    object_key: Option<String>,
    thumbnail_object_key: Option<String>,
    asset_root: PathBuf,
    stored_path: PathBuf,
    size_bytes: u64,
}

#[derive(Debug)]
struct UploadedFile {
    tmp_path: PathBuf,
    file_name: String,
}

async fn post_admin_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AdminAssetResponse>, ApiError> {
    check_admin_token(&headers)?;

    let store = state.store.clone();
    let bucket = state.bucket.clone();
    let root = store.root();
    if root.as_os_str().is_empty() {
        return Err(ApiError::BadRequest("asset root is not configured".into()));
    }
    let max_upload_bytes = max_upload_bytes();

    let tmp_dir = root.join(".tmp");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| ApiError::BadRequest(format!("create temp dir: {e}").into()))?;

    let mut kind: Option<AssetKind> = None;
    let mut id: Option<String> = None;
    let mut label: Option<String> = None;
    let mut description = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut asset: Option<UploadedFile> = None;
    let mut thumbnail: Option<UploadedFile> = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(ApiError::from_multipart)?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "asset" | "file" => {
                asset = Some(save_upload_field(&tmp_dir, &mut field, max_upload_bytes).await?);
            }
            "thumbnail" | "preview" => {
                thumbnail = Some(save_upload_field(&tmp_dir, &mut field, max_upload_bytes).await?);
            }
            "kind" => {
                let raw = field.text().await.map_err(ApiError::from_multipart)?;
                kind = AssetKind::parse_token(&raw);
                if kind.is_none() {
                    return Err(ApiError::BadRequest(
                        format!("unsupported kind: {raw}").into(),
                    ));
                }
            }
            "id" => id = non_empty(field.text().await.map_err(ApiError::from_multipart)?),
            "label" => label = non_empty(field.text().await.map_err(ApiError::from_multipart)?),
            "description" => description = field.text().await.map_err(ApiError::from_multipart)?,
            "tags" => {
                tags = field
                    .text()
                    .await
                    .map_err(ApiError::from_multipart)?
                    .split(|c: char| c == ',' || c == '\n' || c == '\r')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let kind = kind.ok_or_else(|| ApiError::BadRequest("kind is required".into()))?;
    let asset = asset.ok_or_else(|| ApiError::BadRequest("asset file is required".into()))?;
    let ext = extension_from_name(&asset.file_name)
        .ok_or_else(|| ApiError::BadRequest("asset file must have an extension".into()))?;
    if !kind
        .primary_extensions()
        .iter()
        .any(|allowed| *allowed == ext)
    {
        cleanup_tmp(Some(&asset), thumbnail.as_ref()).await;
        return Err(ApiError::BadRequest(
            format!("extension .{ext} is not valid for kind {kind:?}").into(),
        ));
    }

    let stem_hint = id
        .as_deref()
        .or_else(|| {
            FsPath::new(&asset.file_name)
                .file_stem()
                .and_then(|s| s.to_str())
        })
        .unwrap_or("asset");
    let requested_stem = sanitize_stem(stem_hint);
    let stem = if bucket.is_some() {
        unique_stem_in_store(&store, &requested_stem)
    } else {
        unique_stem(&root.join(kind.subdir()), &requested_stem, &ext).await
    };
    let dest_dir = root.join(kind.subdir());
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| ApiError::BadRequest(format!("create destination dir: {e}").into()))?;
    let dest = dest_dir.join(format!("{stem}.{ext}"));

    tokio::fs::rename(&asset.tmp_path, &dest)
        .await
        .map_err(|e| ApiError::BadRequest(format!("persist asset: {e}").into()))?;

    if let Some(label) = label {
        write_sidecar(&dest.with_extension("label"), &label).await?;
    }
    write_sidecar(&dest.with_extension("txt"), &description).await?;
    if !tags.is_empty() {
        write_sidecar(&dest.with_extension("tags"), &tags.join("\n")).await?;
    }

    if let Some(thumbnail) = thumbnail {
        if let Some(thumb_ext) = extension_from_name(&thumbnail.file_name) {
            if matches!(thumb_ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                let thumb_dest = dest_dir.join(format!("{stem}.thumb.{thumb_ext}"));
                tokio::fs::rename(&thumbnail.tmp_path, &thumb_dest)
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("persist thumbnail: {e}").into()))?;
            } else {
                cleanup_tmp(None, Some(&thumbnail)).await;
            }
        }
    }

    let derivative_path = dest.clone();
    let derivative_result =
        tokio::task::spawn_blocking(move || ensure_asset_derivatives(&derivative_path, kind))
            .await
            .map_err(|e| {
                ApiError::BadRequest(format!("asset derivative task failed: {e}").into())
            })?;
    if let Err(e) = derivative_result {
        warn!(
            path = %dest.display(),
            error = %e,
            "uploaded asset derivative generation failed"
        );
    }

    let entry = if let Some(bucket) = bucket.as_ref() {
        let uploaded = bucket
            .upload_local_asset_files(&dest, kind)
            .await
            .map_err(|e| ApiError::BadRequest(format!("upload asset to bucket: {e}").into()))?;
        let mut entry = entry_from_primary(&dest, kind)
            .map_err(|e| ApiError::BadRequest(format!("index uploaded asset: {e}").into()))?;
        entry.object_key = Some(uploaded.primary_key.clone());
        entry.thumbnail_object_key = uploaded.thumbnail_key.clone();
        entry.path = PathBuf::from(format!(
            "s3://{}/{}",
            bucket.bucket_name(),
            uploaded.primary_key
        ));
        entry.thumbnail = None;
        let entry = store
            .upsert_entry(&root, entry)
            .map_err(|e| ApiError::BadRequest(format!("index uploaded asset: {e}").into()))?;
        if !env_truthy("MEMSTROY_BUCKET_KEEP_LOCAL") {
            cleanup_uploaded_local_files(&dest).await;
        }
        entry
    } else {
        store
            .upsert_primary(&root, &dest, kind)
            .map_err(|e| ApiError::BadRequest(format!("index uploaded asset: {e}").into()))?
    };

    info!(
        id = %entry.id,
        kind = ?kind,
        bytes = entry.size_bytes,
        root = %root.display(),
        path = %entry.path.display(),
        "admin asset uploaded"
    );

    Ok(Json(AdminAssetResponse {
        created: true,
        asset: AssetSummary::from_entry(&entry),
        storage_backend: if entry.object_key.is_some() {
            "bucket".to_string()
        } else {
            "local".to_string()
        },
        object_key: entry.object_key.clone(),
        thumbnail_object_key: entry.thumbnail_object_key.clone(),
        asset_root: root,
        stored_path: entry.path.clone(),
        size_bytes: entry.size_bytes,
    }))
}

async fn save_upload_field(
    tmp_dir: &FsPath,
    field: &mut axum::extract::multipart::Field<'_>,
    max_bytes: u64,
) -> Result<UploadedFile, ApiError> {
    let file_name = field
        .file_name()
        .map(sanitize_filename)
        .unwrap_or_else(|| "asset.bin".to_string());
    let tmp_path = tmp_dir.join(format!("upload-{}.tmp", monotonic_stamp()));
    let mut out = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| ApiError::BadRequest(format!("create upload temp file: {e}").into()))?;
    let mut total: u64 = 0;
    while let Some(chunk) = field.chunk().await.map_err(ApiError::from_multipart)? {
        total = total.saturating_add(chunk.len() as u64);
        if total > max_bytes {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(ApiError::PayloadTooLarge {
                limit: max_bytes,
                actual: total,
            });
        }
        out.write_all(&chunk)
            .await
            .map_err(|e| ApiError::BadRequest(format!("write upload chunk: {e}").into()))?;
    }
    out.flush()
        .await
        .map_err(|e| ApiError::BadRequest(format!("flush upload: {e}").into()))?;
    Ok(UploadedFile {
        tmp_path,
        file_name,
    })
}

fn check_admin_token(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = configured_admin_token() else {
        return Ok(());
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);
    let x_token = headers
        .get("x-admin-token")
        .and_then(|h| h.to_str().ok())
        .map(str::trim);
    if bearer == Some(expected.as_str()) || x_token == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

fn configured_admin_token() -> Option<String> {
    for name in ["ADMIN_TOKEN", "MEMSTROY_ADMIN_TOKEN"] {
        let Ok(raw) = std::env::var(name) else {
            continue;
        };
        let token = raw.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

fn max_upload_bytes() -> u64 {
    for name in ["MEMSTROY_MAX_UPLOAD_BYTES", "MAX_UPLOAD_BYTES"] {
        if let Ok(raw) = std::env::var(name) {
            if let Ok(value) = raw.trim().parse::<u64>() {
                if value > 0 {
                    return value;
                }
            }
        }
    }
    DEFAULT_MAX_UPLOAD_BYTES
}

async fn write_sidecar(path: &FsPath, body: &str) -> Result<(), ApiError> {
    tokio::fs::write(path, body.as_bytes())
        .await
        .map_err(|e| ApiError::BadRequest(format!("write {}: {e}", path.display()).into()))
}

async fn cleanup_tmp(asset: Option<&UploadedFile>, thumbnail: Option<&UploadedFile>) {
    if let Some(asset) = asset {
        let _ = tokio::fs::remove_file(&asset.tmp_path).await;
    }
    if let Some(thumbnail) = thumbnail {
        let _ = tokio::fs::remove_file(&thumbnail.tmp_path).await;
    }
}

async fn unique_stem(dir: &FsPath, requested: &str, ext: &str) -> String {
    let base = if requested.is_empty() {
        "asset"
    } else {
        requested
    };
    let mut stem = base.to_string();
    let mut n = 1u32;
    while dir.join(format!("{stem}.{ext}")).exists() {
        stem = format!("{base}-{n}");
        n += 1;
    }
    stem
}

fn unique_stem_in_store(store: &AssetStore, requested: &str) -> String {
    let base = if requested.is_empty() {
        "asset"
    } else {
        requested
    };
    let mut stem = base.to_string();
    let mut n = 1u32;
    while store.get(&stem).is_some() {
        stem = format!("{base}-{n}");
        n += 1;
    }
    stem
}

async fn cleanup_uploaded_local_files(primary: &FsPath) {
    let mut paths = vec![
        primary.to_path_buf(),
        primary.with_extension("label"),
        primary.with_extension("txt"),
        primary.with_extension("tags"),
        primary.with_extension("meta.json"),
    ];
    if let (Some(parent), Some(stem)) = (
        primary.parent(),
        primary.file_stem().and_then(|s| s.to_str()),
    ) {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            paths.push(parent.join(format!("{stem}.thumb.{ext}")));
            paths.push(parent.join("thumbs").join(format!("{stem}.{ext}")));
        }
    }
    for path in paths {
        let _ = tokio::fs::remove_file(path).await;
    }
}

fn extension_from_name(name: &str) -> Option<String> {
    FsPath::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn sanitize_stem(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(80));
    for ch in raw.chars().take(80) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "asset".to_string()
    } else {
        trimmed
    }
}

fn non_empty(raw: String) -> Option<String> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn monotonic_stamp() -> u128 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    (nanos << 16) ^ seq
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ApiError {
    NotFound,
    Unauthorized,
    PayloadTooLarge { limit: u64, actual: u64 },
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
            ApiError::NotFound => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
            }
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized" })),
            )
                .into_response(),
            ApiError::PayloadTooLarge { limit, actual } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "error": "payload_too_large",
                    "limit": limit,
                    "actual": actual,
                })),
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

impl ApiError {
    fn from_multipart(e: axum::extract::multipart::MultipartError) -> Self {
        ApiError::BadRequest(e.to_string().into())
    }
}
