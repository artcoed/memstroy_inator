//! S3-compatible object-storage backend.
//!
//! Railway Buckets expose an S3-compatible API. In bucket mode the
//! server keeps the searchable catalogue in memory, but redirects large
//! preview/download traffic to short-lived presigned bucket URLs so the
//! Railway web service does not proxy video bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_s3::config::{BehaviorVersion, Builder as S3ConfigBuilder, Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::model::{AssetEntry, AssetKind, AssetMediaMeta};

const DEFAULT_PRESIGN_SECS: u64 = 60 * 60;
const MAX_SIDECAR_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct BucketConfig {
    pub endpoint_url: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub force_path_style: bool,
    pub presign_secs: u64,
}

/// Handle for Railway Bucket / S3-compatible storage.
#[derive(Clone)]
pub struct BucketStore {
    client: Client,
    bucket: String,
    endpoint_url: String,
    region: String,
    presign_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BucketObject {
    key: String,
    size: u64,
}

#[derive(Debug, Clone)]
pub struct UploadedBucketAsset {
    pub primary_key: String,
    pub thumbnail_key: Option<String>,
}

pub struct BucketObjectStream {
    pub body: ByteStream,
    pub content_length: Option<i64>,
    pub content_type: Option<String>,
    pub e_tag: Option<String>,
}

impl BucketStore {
    /// Build a bucket store when enough env vars are present. Returns
    /// `Ok(None)` when bucket mode is not configured.
    pub async fn from_env() -> Result<Option<Self>> {
        let Some(config) = BucketConfig::from_env()? else {
            return Ok(None);
        };
        Ok(Some(Self::from_config(config).await))
    }

    async fn from_config(config: BucketConfig) -> Self {
        let credentials = Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.clone(),
            None,
            None,
            "memstroy-railway-bucket",
        );
        let mut builder = S3ConfigBuilder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(credentials)
            .region(Region::new(config.region.clone()))
            .endpoint_url(config.endpoint_url.clone());
        if config.force_path_style {
            builder = builder.force_path_style(true);
        }
        let client = Client::from_conf(builder.build());
        Self {
            client,
            bucket: config.bucket,
            endpoint_url: config.endpoint_url,
            region: config.region,
            presign_secs: config.presign_secs,
        }
    }

    pub fn bucket_name(&self) -> &str {
        &self.bucket
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn presign_secs(&self) -> u64 {
        self.presign_secs
    }

    pub fn virtual_root(&self) -> PathBuf {
        PathBuf::from(format!("s3://{}", self.bucket))
    }

    pub async fn presigned_get_url(&self, key: &str) -> Result<String> {
        let config = PresigningConfig::expires_in(Duration::from_secs(self.presign_secs))
            .context("build S3 presigning config")?;
        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .with_context(|| format!("presign bucket object {key}"))?;
        Ok(request.uri().to_string())
    }

    pub async fn get_object_stream(&self, key: &str) -> Result<BucketObjectStream> {
        self.get_object_stream_inner(key, None).await
    }

    pub async fn get_object_stream_range(
        &self,
        key: &str,
        start: u64,
        end: u64,
    ) -> Result<BucketObjectStream> {
        self.get_object_stream_inner(key, Some((start, end))).await
    }

    async fn get_object_stream_inner(
        &self,
        key: &str,
        range: Option<(u64, u64)>,
    ) -> Result<BucketObjectStream> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some((start, end)) = range {
            req = req.range(format!("bytes={start}-{end}"));
        }
        let object = req
            .send()
            .await
            .with_context(|| format!("get bucket object {key}"))?;
        Ok(BucketObjectStream {
            content_length: object.content_length(),
            content_type: object.content_type().map(str::to_string),
            e_tag: object.e_tag().map(str::to_string),
            body: object.body,
        })
    }

    pub async fn put_file(
        &self,
        key: &str,
        path: &Path,
        content_type: Option<String>,
    ) -> Result<()> {
        let body = ByteStream::read_from()
            .path(path)
            .build()
            .await
            .with_context(|| format!("read upload body {}", path.display()))?;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .cache_control("public, max-age=31536000, immutable");
        if let Some(content_type) = content_type {
            req = req.content_type(content_type);
        }
        req.send()
            .await
            .with_context(|| format!("put bucket object {key}"))?;
        Ok(())
    }

    pub async fn put_bytes(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<()> {
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .cache_control("public, max-age=31536000, immutable");
        if let Some(content_type) = content_type {
            req = req.content_type(content_type);
        }
        req.send()
            .await
            .with_context(|| format!("put bucket object {key}"))?;
        Ok(())
    }

    pub async fn get_small_text(&self, key: &str, max_bytes: usize) -> Result<Option<String>> {
        let object = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(object) => object,
            Err(e) => {
                debug!(key, error = %e, "bucket sidecar missing or unreadable");
                return Ok(None);
            }
        };
        if object.content_length().unwrap_or_default() > max_bytes as i64 {
            warn!(key, max_bytes, "bucket sidecar too large, ignoring");
            return Ok(None);
        }
        let bytes = object
            .body
            .collect()
            .await
            .with_context(|| format!("read bucket object {key}"))?
            .into_bytes();
        if bytes.len() > max_bytes {
            warn!(
                key,
                max_bytes, "bucket sidecar too large after read, ignoring"
            );
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        Ok((!text.is_empty()).then_some(text))
    }

    pub async fn index_entries(&self) -> Result<Vec<AssetEntry>> {
        let objects = self.list_all_objects().await?;
        let by_key: BTreeMap<String, BucketObject> = objects
            .iter()
            .cloned()
            .map(|object| (object.key.clone(), object))
            .collect();
        let keys: BTreeSet<String> = by_key.keys().cloned().collect();
        let mut entries = Vec::new();

        for object in objects {
            let Some((kind, stem, extension)) = primary_key_parts(&object.key) else {
                continue;
            };
            let label = self
                .get_small_text(&sidecar_key(kind, &stem, "label"), MAX_SIDECAR_BYTES)
                .await?
                .unwrap_or_else(|| stem.clone());
            let description = if kind == AssetKind::Text {
                self.get_small_text(&object.key, MAX_SIDECAR_BYTES)
                    .await?
                    .unwrap_or_default()
            } else {
                self.get_small_text(&sidecar_key(kind, &stem, "txt"), MAX_SIDECAR_BYTES)
                    .await?
                    .unwrap_or_default()
            };
            let tags = self
                .get_small_text(&sidecar_key(kind, &stem, "tags"), MAX_SIDECAR_BYTES)
                .await?
                .map(parse_tags)
                .unwrap_or_default();
            let media_meta = self
                .get_small_text(&sidecar_key(kind, &stem, "meta.json"), MAX_SIDECAR_BYTES)
                .await?
                .and_then(|raw| serde_json::from_str::<AssetMediaMeta>(&raw).ok())
                .unwrap_or_default();
            let thumbnail_key = find_bucket_thumbnail(kind, &stem, &object.key, &keys);
            let path = PathBuf::from(format!("s3://{}/{}", self.bucket, object.key));
            let file_name = object
                .key
                .rsplit('/')
                .next()
                .unwrap_or(&object.key)
                .to_string();

            entries.push(AssetEntry {
                id: stem,
                kind,
                label,
                description,
                path,
                thumbnail: None,
                object_key: Some(object.key),
                thumbnail_object_key: thumbnail_key,
                size_bytes: object.size,
                file_name,
                extension,
                duration_secs: media_meta.duration_secs,
                width: media_meta.width,
                height: media_meta.height,
                tags,
            });
        }

        info!(count = entries.len(), bucket = %self.bucket, "bucket asset index loaded");
        Ok(entries)
    }

    pub async fn upload_local_asset_files(
        &self,
        primary_path: &Path,
        kind: AssetKind,
    ) -> Result<UploadedBucketAsset> {
        let stem = primary_path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("{} has no stem", primary_path.display()))?;
        let filename = primary_path
            .file_name()
            .and_then(|s| s.to_str())
            .with_context(|| format!("{} has no filename", primary_path.display()))?;
        let primary_key = format!("{}/{}", kind.subdir(), filename);
        self.put_file(&primary_key, primary_path, content_type_for(primary_path))
            .await?;

        let mut thumbnail_key = None;
        for sidecar in known_local_sidecars(primary_path) {
            if !sidecar.is_file() {
                continue;
            }
            let Some(name) = sidecar.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let key = format!("{}/{}", kind.subdir(), name);
            self.put_file(&key, &sidecar, content_type_for(&sidecar))
                .await?;
            if is_thumbnail_key(&key) {
                thumbnail_key = Some(key);
            }
        }

        let parent = primary_path.parent().unwrap_or_else(|| Path::new(""));
        let thumbs_dir = parent.join("thumbs");
        for ext in ["jpg", "jpeg", "png", "webp"] {
            let thumb = thumbs_dir.join(format!("{stem}.{ext}"));
            if thumb.is_file() {
                let key = format!("{}/thumbs/{stem}.{ext}", kind.subdir());
                self.put_file(&key, &thumb, content_type_for(&thumb))
                    .await?;
                if thumbnail_key.is_none() {
                    thumbnail_key = Some(key);
                }
            }
        }

        if kind == AssetKind::Image && thumbnail_key.is_none() {
            thumbnail_key = Some(primary_key.clone());
        }

        Ok(UploadedBucketAsset {
            primary_key,
            thumbnail_key,
        })
    }

    /// Upload all local asset files into the bucket preserving the
    /// `clips/...`, `videos/...` layout. Intended as a one-time
    /// migration from Railway Volume to Railway Bucket.
    pub async fn migrate_local_tree(&self, root: &Path) -> Result<u64> {
        let mut uploaded = 0u64;
        for kind in AssetKind::ALL {
            let sub = root.join(kind.subdir());
            if !sub.is_dir() {
                continue;
            }
            for ent in WalkDir::new(&sub)
                .max_depth(4)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !ent.file_type().is_file() {
                    continue;
                }
                let path = ent.path();
                if path.components().any(|c| c.as_os_str() == ".tmp") {
                    continue;
                }
                let rel = path
                    .strip_prefix(root)
                    .with_context(|| format!("strip root from {}", path.display()))?;
                let Some(key) = path_to_bucket_key(rel) else {
                    continue;
                };
                self.put_file(&key, path, content_type_for(path)).await?;
                uploaded += 1;
            }
        }
        info!(uploaded, root = %root.display(), bucket = %self.bucket, "local asset tree migrated to bucket");
        Ok(uploaded)
    }

    async fn list_all_objects(&self) -> Result<Vec<BucketObject>> {
        let mut out = Vec::new();
        let mut continuation_token = None;
        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
            if let Some(token) = continuation_token {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("list bucket {}", self.bucket))?;
            for object in resp.contents() {
                let Some(key) = object.key() else {
                    continue;
                };
                out.push(BucketObject {
                    key: key.to_string(),
                    size: object.size().unwrap_or_default().max(0) as u64,
                });
            }
            continuation_token = resp.next_continuation_token().map(ToOwned::to_owned);
            if continuation_token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

impl BucketConfig {
    fn from_env() -> Result<Option<Self>> {
        let backend = env_string("MEMSTROY_STORAGE_BACKEND")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let wants_bucket = matches!(
            backend.as_str(),
            "bucket" | "s3" | "object" | "object_storage"
        );

        let endpoint_url = env_first(&[
            "AWS_ENDPOINT_URL_S3",
            "AWS_ENDPOINT_URL",
            "RAILWAY_BUCKET_ENDPOINT",
            "RAILWAY_BUCKET_ENDPOINT_URL",
            "S3_ENDPOINT_URL",
            "BUCKET_ENDPOINT",
            "BUCKET_ENDPOINT_URL",
        ])
        .or_else(|| wants_bucket.then(|| env_first(&["ENDPOINT"])).flatten());
        let bucket = env_first(&[
            "AWS_BUCKET",
            "AWS_BUCKET_NAME",
            "AWS_S3_BUCKET",
            "AWS_S3_BUCKET_NAME",
            "RAILWAY_BUCKET_NAME",
            "S3_BUCKET",
            "S3_BUCKET_NAME",
            "BUCKET_NAME",
        ])
        .or_else(|| wants_bucket.then(|| env_first(&["BUCKET"])).flatten());
        let access_key_id = env_first(&[
            "AWS_ACCESS_KEY_ID",
            "RAILWAY_BUCKET_ACCESS_KEY_ID",
            "S3_ACCESS_KEY_ID",
            "S3_ACCESS_KEY",
            "BUCKET_ACCESS_KEY_ID",
        ])
        .or_else(|| {
            wants_bucket
                .then(|| env_first(&["ACCESS_KEY_ID"]))
                .flatten()
        });
        let secret_access_key = env_first(&[
            "AWS_SECRET_ACCESS_KEY",
            "RAILWAY_BUCKET_SECRET_ACCESS_KEY",
            "S3_SECRET_ACCESS_KEY",
            "S3_SECRET_KEY",
            "BUCKET_SECRET_ACCESS_KEY",
        ])
        .or_else(|| {
            wants_bucket
                .then(|| env_first(&["SECRET_ACCESS_KEY"]))
                .flatten()
        });

        if !wants_bucket
            && endpoint_url.is_none()
            && bucket.is_none()
            && access_key_id.is_none()
            && secret_access_key.is_none()
        {
            return Ok(None);
        }

        let missing = [
            ("endpoint URL", endpoint_url.as_ref()),
            ("bucket name", bucket.as_ref()),
            ("access key id", access_key_id.as_ref()),
            ("secret access key", secret_access_key.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.is_none().then_some(name))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            anyhow::bail!(
                "bucket storage is selected but required S3 variables are missing: {}",
                missing.join(", ")
            );
        }

        let region = env_first(&[
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "RAILWAY_BUCKET_REGION",
            "S3_REGION",
            "BUCKET_REGION",
        ])
        .or_else(|| wants_bucket.then(|| env_first(&["REGION"])).flatten())
        .unwrap_or_else(|| "auto".to_string());
        let style = env_first(&["AWS_S3_URL_STYLE", "S3_URL_STYLE", "BUCKET_URL_STYLE"])
            .unwrap_or_else(|| "virtual".to_string())
            .to_ascii_lowercase();
        let force_path_style = matches!(
            style.as_str(),
            "path" | "path-style" | "path_style" | "force-path-style"
        );
        let presign_secs = env_string("MEMSTROY_BUCKET_PRESIGN_SECS")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PRESIGN_SECS)
            .min(60 * 60 * 24 * 7);

        Ok(Some(Self {
            endpoint_url: endpoint_url.expect("checked above"),
            region,
            bucket: bucket.expect("checked above"),
            access_key_id: access_key_id.expect("checked above"),
            secret_access_key: secret_access_key.expect("checked above"),
            force_path_style,
            presign_secs,
        }))
    }
}

pub(crate) fn env_truthy(name: &str) -> bool {
    env_string(name)
        .map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| env_string(name))
}

fn env_string(name: &str) -> Option<String> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn primary_key_parts(key: &str) -> Option<(AssetKind, String, String)> {
    let mut parts = key.split('/');
    let kind = AssetKind::from_subdir(parts.next()?)?;
    let rest = parts.collect::<Vec<_>>().join("/");
    if rest.is_empty() {
        return None;
    }
    if rest
        .split('/')
        .any(|part| part.eq_ignore_ascii_case("thumbs"))
    {
        return None;
    }
    if is_thumbnail_key(key) || key.contains(".meta.") {
        return None;
    }
    let file_name = rest.rsplit('/').next()?;
    let (stem, extension) = file_name.rsplit_once('.')?;
    if matches!(extension, "tags" | "label") {
        return None;
    }
    let extension = extension.to_ascii_lowercase();
    if !kind
        .primary_extensions()
        .iter()
        .any(|allowed| *allowed == extension)
    {
        return None;
    }
    Some((kind, stem.to_string(), extension))
}

fn find_bucket_thumbnail(
    kind: AssetKind,
    stem: &str,
    primary_key: &str,
    keys: &BTreeSet<String>,
) -> Option<String> {
    if kind == AssetKind::Image {
        return Some(primary_key.to_string());
    }
    let subdir = kind.subdir();
    for ext in ["png", "jpg", "jpeg", "webp"] {
        let key = format!("{subdir}/{stem}.thumb.{ext}");
        if keys.contains(&key) {
            return Some(key);
        }
    }
    for ext in ["jpg", "jpeg", "png", "webp"] {
        let key = format!("{subdir}/thumbs/{stem}.{ext}");
        if keys.contains(&key) {
            return Some(key);
        }
    }
    for ext in ["png", "jpg", "jpeg", "webp"] {
        let key = format!("{subdir}/{stem}.{ext}");
        if keys.contains(&key) && key != primary_key {
            return Some(key);
        }
    }
    None
}

fn sidecar_key(kind: AssetKind, stem: &str, ext: &str) -> String {
    format!("{}/{}.{}", kind.subdir(), stem, ext)
}

fn known_local_sidecars(primary: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for ext in ["label", "txt", "tags", "meta.json"] {
        out.push(primary.with_extension(ext));
    }
    if let (Some(parent), Some(stem)) = (
        primary.parent(),
        primary.file_stem().and_then(|s| s.to_str()),
    ) {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            out.push(parent.join(format!("{stem}.thumb.{ext}")));
        }
    }
    out
}

fn is_thumbnail_key(key: &str) -> bool {
    key.contains(".thumb.")
}

fn parse_tags(raw: String) -> Vec<String> {
    raw.split(|c: char| c == ',' || c == '\n' || c == '\r')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn content_type_for(path: &Path) -> Option<String> {
    mime_guess::from_path(path)
        .first()
        .map(|mime| mime.essence_str().to_string())
}

fn path_to_bucket_key(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().replace('\\', "/")),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_key_parts_accepts_real_assets() {
        assert_eq!(
            primary_key_parts("clips/tg_123.mp4"),
            Some((AssetKind::Clip, "tg_123".to_string(), "mp4".to_string()))
        );
        assert_eq!(
            primary_key_parts("images/cat.PNG"),
            Some((AssetKind::Image, "cat".to_string(), "png".to_string()))
        );
    }

    #[test]
    fn primary_key_parts_rejects_sidecars_and_thumbs() {
        assert_eq!(primary_key_parts("clips/tg_123.txt"), None);
        assert_eq!(primary_key_parts("clips/tg_123.tags"), None);
        assert_eq!(primary_key_parts("clips/tg_123.thumb.jpg"), None);
        assert_eq!(primary_key_parts("clips/thumbs/tg_123.jpg"), None);
        assert_eq!(primary_key_parts("clips/tg_123.meta.json"), None);
    }

    #[test]
    fn thumbnail_lookup_prefers_canonical_sidecar() {
        let keys = BTreeSet::from([
            "clips/tg_123.mp4".to_string(),
            "clips/tg_123.thumb.jpg".to_string(),
            "clips/thumbs/tg_123.jpg".to_string(),
        ]);
        assert_eq!(
            find_bucket_thumbnail(AssetKind::Clip, "tg_123", "clips/tg_123.mp4", &keys),
            Some("clips/tg_123.thumb.jpg".to_string())
        );
    }
}
