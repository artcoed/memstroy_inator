//! HTTP handlers and the `axum::Router` that ties them together.
//!
//! The router is built once per server start and uses
//! [`crate::store::AssetStore`] as its application state. CORS is
//! permissive (`Any`/`Any`/`Any`) because the server is intentionally
//! "trust the network" — it's expected to live next to the editor on
//! a developer machine, not on the public internet.

use std::path::{Path as FsPath, PathBuf};
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
use tokio::io::AsyncWriteExt;
use tower_http::cors::{Any, CorsLayer};
use tracing::warn;

use crate::model::{AssetKind, AssetSummary};
use crate::store::{ensure_asset_derivatives, AssetStore};

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
        .route("/api/assets", get(list_assets))
        .route("/api/assets/:id", get(get_asset))
        .route("/api/assets/:id/preview", get(get_preview))
        .route("/api/assets/:id/download", get(get_download))
        .route("/api/assets/:id/text", get(get_text))
        .route("/api/admin/assets", post(post_admin_asset))
        .with_state(store)
        .layer(cors)
        .layer(DefaultBodyLimit::disable())
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
    State(store): State<AssetStore>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let kinds = parse_kind_list(q.kind.as_deref());
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let (total, page) = store.filtered(&kinds, q.q.as_deref(), q.offset, limit);
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
    State(store): State<AssetStore>,
    Path(id): Path<String>,
) -> Result<Json<crate::model::AssetEntry>, ApiError> {
    store.get(&id).map(Json).ok_or(ApiError::NotFound)
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
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=300"),
        )
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
    let body = match tokio::fs::File::open(&entry.path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);
            Body::from_stream(stream)
        }
        Err(e) => {
            warn!(path = %entry.path.display(), error = %e, "asset open failed; serving placeholder");
            return Ok(fallback_download_response(&entry.id, entry.kind));
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
}

#[derive(Debug)]
struct UploadedFile {
    tmp_path: PathBuf,
    file_name: String,
}

async fn post_admin_asset(
    State(store): State<AssetStore>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AdminAssetResponse>, ApiError> {
    check_admin_token(&headers)?;

    let root = store.root();
    if root.as_os_str().is_empty() {
        return Err(ApiError::BadRequest("asset root is not configured".into()));
    }

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
                asset = Some(save_upload_field(&tmp_dir, &mut field).await?);
            }
            "thumbnail" | "preview" => {
                thumbnail = Some(save_upload_field(&tmp_dir, &mut field).await?);
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
    let stem = unique_stem(&root.join(kind.subdir()), &sanitize_stem(stem_hint), &ext).await;
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

    if let Err(e) = ensure_asset_derivatives(&dest, kind) {
        warn!(
            path = %dest.display(),
            error = %e,
            "uploaded asset derivative generation failed"
        );
    }

    store
        .index_dir(&root)
        .map_err(|e| ApiError::BadRequest(format!("reindex after upload: {e}").into()))?;
    let entry = store.get(&stem).ok_or_else(|| {
        ApiError::BadRequest(format!("uploaded asset {stem} was not indexed").into())
    })?;

    Ok(Json(AdminAssetResponse {
        created: true,
        asset: AssetSummary::from_entry(&entry),
    }))
}

async fn save_upload_field(
    tmp_dir: &FsPath,
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<UploadedFile, ApiError> {
    let file_name = field
        .file_name()
        .map(sanitize_filename)
        .unwrap_or_else(|| "asset.bin".to_string());
    let tmp_path = tmp_dir.join(format!("upload-{}.tmp", monotonic_stamp()));
    let mut out = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| ApiError::BadRequest(format!("create upload temp file: {e}").into()))?;
    while let Some(chunk) = field.chunk().await.map_err(ApiError::from_multipart)? {
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
    let Ok(expected) = std::env::var("ADMIN_TOKEN") else {
        return Ok(());
    };
    let expected = expected.trim();
    if expected.is_empty() {
        return Ok(());
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);
    let x_token = headers
        .get("x-admin-token")
        .and_then(|h| h.to_str().ok())
        .map(str::trim);
    if bearer == Some(expected) || x_token == Some(expected) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ApiError {
    NotFound,
    Unauthorized,
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
