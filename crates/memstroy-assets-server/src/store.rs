//! In-memory index of the asset library on disk.
//!
//! [`AssetStore`] is cheap to clone (it's a thin handle around an
//! `Arc<RwLock<Inner>>`) so it can be shared with axum handlers via
//! `with_state(...)`. Callers re-run [`AssetStore::index_dir`] whenever
//! the on-disk library changes to refresh the cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::model::{AssetEntry, AssetKind, AssetMediaMeta};

#[derive(Default)]
struct Inner {
    root: PathBuf,
    by_id: BTreeMap<String, AssetEntry>,
}

/// Cheap-to-clone handle to the asset index. All clones share the
/// same underlying state.
#[derive(Clone, Default)]
pub struct AssetStore {
    inner: Arc<RwLock<Inner>>,
}

impl AssetStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asset root currently associated with the store. Returns an
    /// empty path before the first call to [`Self::index_dir`].
    pub fn root(&self) -> PathBuf {
        self.inner
            .read()
            .expect("store rwlock poisoned")
            .root
            .clone()
    }

    /// Walk `root` (which is expected to contain `clips/`, `videos/`,
    /// `images/`, `sounds/`, `particles/`, `text/`) and rebuild the
    /// in-memory index from scratch.
    ///
    /// Missing kind subdirectories are silently ignored. Files whose
    /// extension is not on the kind's primary-extension allow-list are
    /// treated as sidecars and skipped. Sibling files such as
    /// `<stem>.txt`, `<stem>.tags`, `<stem>.label`, `<stem>.thumb.png`
    /// are picked up as metadata for their primary asset.
    ///
    pub fn index_dir(&self, root: &Path) -> Result<()> {
        let root = root.to_path_buf();
        let mut by_id: BTreeMap<String, AssetEntry> = BTreeMap::new();

        for kind in AssetKind::ALL {
            let sub = root.join(kind.subdir());
            if !sub.exists() {
                debug!(kind = ?kind, dir = %sub.display(), "subdir missing, skipping");
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
                let path = ent.path().to_path_buf();
                if !is_primary_for_kind(&path, kind) {
                    continue;
                }
                let _ = ensure_asset_derivatives(&path, kind);
                match entry_from_primary(&path, kind) {
                    Ok(entry) => {
                        by_id.insert(entry.id.clone(), entry);
                    }
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "asset indexing failed");
                    }
                }
            }
        }

        let mut inner = self.inner.write().expect("store rwlock poisoned");
        inner.root = root.clone();
        inner.by_id = by_id;
        info!(count = inner.by_id.len(), root = %root.display(), "asset index rebuilt");
        Ok(())
    }

    /// Insert or replace one primary asset in the in-memory index.
    ///
    /// Upload handlers use this after persisting a new file so readers
    /// do not pay for a full recursive reindex on every admin upload.
    pub fn upsert_primary(&self, root: &Path, path: &Path, kind: AssetKind) -> Result<AssetEntry> {
        let entry = entry_from_primary(path, kind)?;
        let mut inner = self.inner.write().expect("store rwlock poisoned");
        inner.root = root.to_path_buf();
        inner.by_id.insert(entry.id.clone(), entry.clone());
        debug!(
            id = %entry.id,
            kind = ?entry.kind,
            path = %entry.path.display(),
            "asset index upserted"
        );
        Ok(entry)
    }

    /// Total number of indexed assets.
    pub fn count(&self) -> usize {
        self.inner
            .read()
            .expect("store rwlock poisoned")
            .by_id
            .len()
    }

    /// Counts grouped by kind, sorted by [`AssetKind::ALL`] order.
    pub fn count_by_kind(&self) -> Vec<(AssetKind, usize)> {
        let guard = self.inner.read().expect("store rwlock poisoned");
        AssetKind::ALL
            .iter()
            .copied()
            .map(|k| (k, guard.by_id.values().filter(|e| e.kind == k).count()))
            .collect()
    }

    /// Look up one asset by id.
    pub fn get(&self, id: &str) -> Option<AssetEntry> {
        self.inner
            .read()
            .expect("store rwlock poisoned")
            .by_id
            .get(id)
            .cloned()
    }

    /// Snapshot of every indexed asset, sorted by id.
    pub fn list(&self) -> Vec<AssetEntry> {
        self.inner
            .read()
            .expect("store rwlock poisoned")
            .by_id
            .values()
            .cloned()
            .collect()
    }

    /// Filter and paginate the index. Search first matches exact
    /// substrings, then falls back to typo-tolerant Levenshtein scoring
    /// over id, label, description and tags.
    ///
    /// * `kinds` — if non-empty, only entries whose kind is in the set
    ///   are kept.
    /// * `query` — case-insensitive substring matched against
    ///   `label`, `description`, `id` and `tags`.
    pub fn filtered(
        &self,
        kinds: &[AssetKind],
        query: Option<&str>,
        offset: u64,
        limit: u64,
    ) -> (u64, Vec<AssetEntry>) {
        let needle = query
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let guard = self.inner.read().expect("store rwlock poisoned");
        let mut scored: Vec<(u32, &AssetEntry)> = guard
            .by_id
            .values()
            .filter(|entry| kinds.is_empty() || kinds.contains(&entry.kind))
            .filter_map(|entry| {
                let score = match &needle {
                    None => 0,
                    Some(q) => relevance_score(entry, q)?,
                };
                Some((score, entry))
            })
            .collect();
        if needle.is_some() {
            scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
        }

        let total = scored.len() as u64;
        let page: Vec<AssetEntry> = scored
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|(_, entry)| entry.clone())
            .collect();
        (total, page)
    }
}

fn entry_from_primary(path: &Path, kind: AssetKind) -> Result<AssetEntry> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("{} has no valid UTF-8 stem", path.display()))?
        .to_string();
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    let size_bytes = metadata.len();

    let label = read_sidecar_string(path, "label").unwrap_or_else(|| stem.clone());
    let description = match kind {
        AssetKind::Text => std::fs::read_to_string(path).unwrap_or_default(),
        _ => read_sidecar_string(path, "txt").unwrap_or_default(),
    };
    let tags = read_sidecar_string(path, "tags")
        .map(parse_tags)
        .unwrap_or_default();
    let thumbnail = find_thumbnail(path, kind);
    let media_meta = read_media_meta(path).unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    Ok(AssetEntry {
        id: stem,
        kind,
        label,
        description,
        path: path.to_path_buf(),
        thumbnail,
        size_bytes,
        file_name,
        extension,
        duration_secs: media_meta.duration_secs,
        width: media_meta.width,
        height: media_meta.height,
        tags,
    })
}

fn relevance_score(entry: &AssetEntry, query: &str) -> Option<u32> {
    let fields = [
        entry.id.as_str(),
        entry.label.as_str(),
        entry.description.as_str(),
    ];
    let exact = fields
        .iter()
        .any(|field| field.to_lowercase().contains(query))
        || entry
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(query));
    if exact {
        return Some(0);
    }

    let mut best = u32::MAX;
    for token in searchable_tokens(entry) {
        let distance = levenshtein(&token, query) as u32;
        let max_len = token.chars().count().max(query.chars().count()) as u32;
        let allowed = (max_len / 3).max(1).min(4);
        if distance <= allowed {
            best = best.min(distance + 1);
        }
    }
    (best != u32::MAX).then_some(best)
}

fn searchable_tokens(entry: &AssetEntry) -> Vec<String> {
    let mut tokens = Vec::new();
    for raw in [&entry.id, &entry.label, &entry.description] {
        tokens.extend(split_search_tokens(raw));
    }
    for tag in &entry.tags {
        tokens.extend(split_search_tokens(tag));
    }
    tokens
}

fn split_search_tokens(raw: &str) -> impl Iterator<Item = String> + '_ {
    raw.split(|c: char| !c.is_alphanumeric())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s.chars().count() >= 3)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

fn is_primary_for_kind(path: &Path, kind: AssetKind) -> bool {
    // Skip the well-known sidecar suffixes so e.g. `clips/12.txt`
    // isn't picked up as its own asset.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.contains(".thumb.") || name.contains(".meta.") {
            return false;
        }
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if stem.ends_with(".thumb") || stem.ends_with(".meta") {
            return false;
        }
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    let ext = match ext {
        Some(e) => e,
        None => return false,
    };

    // Drop sidecar metadata files that would otherwise look like
    // legitimate `Text` or `Particle` payloads.
    if matches!(ext.as_str(), "tags" | "label") {
        return false;
    }

    // Anything inside a `thumbs/` directory is a thumbnail, not a
    // primary asset.
    if path.components().any(|c| c.as_os_str() == "thumbs") {
        return false;
    }

    kind.primary_extensions()
        .iter()
        .any(|allowed| *allowed == ext)
}

fn read_sidecar_string(primary: &Path, ext: &str) -> Option<String> {
    let sidecar = primary.with_extension(ext);
    let raw = std::fs::read_to_string(&sidecar).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn parse_tags(raw: String) -> Vec<String> {
    raw.split(|c: char| c == ',' || c == '\n' || c == '\r')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn find_thumbnail(primary: &Path, kind: AssetKind) -> Option<PathBuf> {
    let stem = primary.file_stem()?.to_str()?;
    let parent = primary.parent()?;

    // 1. Sibling `<stem>.thumb.<ext>` is the canonical sidecar.
    for ext in ["png", "jpg", "jpeg", "webp"] {
        let cand = parent.join(format!("{}.thumb.{}", stem, ext));
        if cand.exists() {
            return Some(cand);
        }
    }

    // 2. `<parent>/thumbs/<stem>.<ext>` keeps compatibility with older
    //    libraries that stored generated previews in a shared subfolder.
    let thumbs_dir = parent.join("thumbs");
    if thumbs_dir.is_dir() {
        for ext in ["jpg", "jpeg", "png", "webp"] {
            let cand = thumbs_dir.join(format!("{}.{}", stem, ext));
            if cand.exists() {
                return Some(cand);
            }
        }
    }

    // 3. Plain sibling `<stem>.png` / `<stem>.jpg`. We only allow
    //    these when the primary asset itself is *not* an image, to
    //    avoid pointing the thumbnail at a different image asset that
    //    happens to share a stem with this one.
    if kind != AssetKind::Image {
        for ext in ["png", "jpg", "jpeg", "webp"] {
            let cand = parent.join(format!("{}.{}", stem, ext));
            if cand.exists() && cand != primary {
                return Some(cand);
            }
        }
    }

    // 4. Image assets are their own thumbnails — the editor can scale
    //    the original on the client side. This way every image has a
    //    `preview_url` even if no sidecar is present.
    if kind == AssetKind::Image {
        return Some(primary.to_path_buf());
    }

    None
}

pub fn media_meta_path(primary: &Path) -> PathBuf {
    primary.with_extension("meta.json")
}

fn read_media_meta(primary: &Path) -> Option<AssetMediaMeta> {
    let raw = std::fs::read_to_string(media_meta_path(primary)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn ensure_asset_derivatives(primary: &Path, kind: AssetKind) -> Result<()> {
    if matches!(kind, AssetKind::Clip | AssetKind::Video) && find_thumbnail(primary, kind).is_none()
    {
        if let Err(e) = generate_video_thumbnail(primary) {
            warn!(
                path = %primary.display(),
                error = %e,
                "server thumbnail generation failed"
            );
        }
    }

    let meta_path = media_meta_path(primary);
    if !meta_path.exists() {
        match probe_media_meta(primary) {
            Ok(meta) => {
                let body = serde_json::to_vec_pretty(&meta)?;
                std::fs::write(&meta_path, body)
                    .with_context(|| format!("writing {}", meta_path.display()))?;
            }
            Err(e) => {
                warn!(
                    path = %primary.display(),
                    error = %e,
                    "server media metadata probe failed"
                );
            }
        }
    }
    Ok(())
}

fn generate_video_thumbnail(primary: &Path) -> Result<()> {
    let parent = primary
        .parent()
        .with_context(|| format!("{} has no parent", primary.display()))?;
    let stem = primary
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("{} has no stem", primary.display()))?;
    let thumb = parent.join(format!("{stem}.thumb.jpg"));
    let mut cmd = Command::new(ffmpeg_binary());
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-ss",
        "0.2",
        "-i",
    ])
    .arg(primary)
    .args(["-frames:v", "1", "-vf", "scale=360:-1", "-q:v", "4"])
    .arg(&thumb)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let status = cmd.status().context("spawn ffmpeg thumbnail")?;
    if status.success() && thumb.exists() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("ffmpeg exited with {status}"))
    }
}

fn probe_media_meta(primary: &Path) -> Result<AssetMediaMeta> {
    let output = Command::new(ffprobe_binary())
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=width,height",
            "-of",
            "json",
        ])
        .arg(primary)
        .stdin(Stdio::null())
        .output()
        .context("spawn ffprobe metadata")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!("ffprobe exited with {}", output.status));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse ffprobe json")?;
    let duration_secs = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|v| {
            v.as_f64()
                .map(|n| n as f32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<f32>().ok()))
        })
        .filter(|d| d.is_finite() && *d > 0.0);
    let mut width = None;
    let mut height = None;
    if let Some(streams) = json.get("streams").and_then(|v| v.as_array()) {
        for stream in streams {
            let w = stream
                .get("width")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok());
            let h = stream
                .get("height")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok());
            if w.is_some() || h.is_some() {
                width = w;
                height = h;
                break;
            }
        }
    }
    Ok(AssetMediaMeta {
        duration_secs,
        width,
        height,
    })
}

fn ffmpeg_binary() -> PathBuf {
    std::env::var_os("MEMSTROY_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            })
        })
}

fn ffprobe_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("MEMSTROY_FFPROBE") {
        return PathBuf::from(path);
    }
    let mut sibling = ffmpeg_binary();
    sibling.set_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    if sibling.exists() {
        sibling
    } else {
        PathBuf::from(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        })
    }
}

#[allow(dead_code)]
#[doc(hidden)]
pub fn _ensure_root_exists(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating asset root {}", root.display()))?;
    for k in AssetKind::ALL {
        let _ = std::fs::create_dir_all(root.join(k.subdir()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(path: &Path, body: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn indexes_files_per_kind() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        touch(&root.join("clips/abc.mp4"), b"hello");
        touch(&root.join("clips/abc.txt"), b"description text");
        touch(&root.join("clips/abc.tags"), b"meme,funny");
        touch(&root.join("images/cat.png"), b"\x89PNG");
        touch(&root.join("text/note.md"), b"# notes");

        let store = AssetStore::new();
        store.index_dir(root).unwrap();

        assert_eq!(store.count(), 3);
        let abc = store.get("abc").unwrap();
        assert_eq!(abc.kind, AssetKind::Clip);
        assert_eq!(abc.description, "description text");
        assert_eq!(abc.tags, vec!["meme".to_string(), "funny".to_string()]);

        let img = store.get("cat").unwrap();
        assert_eq!(img.kind, AssetKind::Image);
        assert!(img.thumbnail.is_some(), "image is its own thumbnail");

        let note = store.get("note").unwrap();
        assert_eq!(note.kind, AssetKind::Text);
        assert_eq!(note.description, "# notes");
    }

    #[test]
    fn filter_by_kind_and_query() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        touch(&root.join("clips/funny_cat.mp4"), b"a");
        touch(&root.join("clips/funny_cat.txt"), b"a cat being funny");
        touch(&root.join("images/sad_dog.png"), b"a");

        let store = AssetStore::new();
        store.index_dir(root).unwrap();
        let (total, items) = store.filtered(&[AssetKind::Clip], None, 0, 100);
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "funny_cat");

        let (total, items) = store.filtered(&[], Some("dog"), 0, 100);
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "sad_dog");

        let (total, items) = store.filtered(&[], Some("FUNNY"), 0, 100);
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "funny_cat");
    }
}
