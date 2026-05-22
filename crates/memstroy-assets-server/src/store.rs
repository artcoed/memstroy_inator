//! In-memory index of the asset library on disk.
//!
//! [`AssetStore`] is cheap to clone (it's a thin handle around an
//! `Arc<RwLock<Inner>>`) so it can be shared with axum handlers via
//! `with_state(...)` and with background ingest tasks via
//! `tokio::spawn`. Callers re-run [`AssetStore::index_dir`] whenever
//! the on-disk library changes (e.g. after a TG ingest job finishes)
//! to refresh the cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::model::{AssetEntry, AssetKind};

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
        self.inner.read().expect("store rwlock poisoned").root.clone()
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
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let metadata = match ent.metadata() {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "metadata read failed");
                        continue;
                    }
                };
                let size_bytes = metadata.len();

                let label = read_sidecar_string(&path, "label").unwrap_or_else(|| stem.clone());
                let description = match kind {
                    AssetKind::Text => std::fs::read_to_string(&path).unwrap_or_default(),
                    _ => read_sidecar_string(&path, "txt").unwrap_or_default(),
                };
                let tags = read_sidecar_string(&path, "tags")
                    .map(parse_tags)
                    .unwrap_or_default();
                let thumbnail = find_thumbnail(&path, kind);

                by_id.insert(
                    stem.clone(),
                    AssetEntry {
                        id: stem,
                        kind,
                        label,
                        description,
                        path,
                        thumbnail,
                        size_bytes,
                        tags,
                    },
                );
            }
        }

        let mut inner = self.inner.write().expect("store rwlock poisoned");
        inner.root = root.clone();
        inner.by_id = by_id;
        info!(count = inner.by_id.len(), root = %root.display(), "asset index rebuilt");
        Ok(())
    }

    /// Total number of indexed assets.
    pub fn count(&self) -> usize {
        self.inner.read().expect("store rwlock poisoned").by_id.len()
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

    /// Filter and paginate the index. Filtering is performed against
    /// a fresh snapshot of the index so the read lock is held only
    /// briefly.
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
        let snapshot = self.list();
        let needle = query.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
        let filtered: Vec<AssetEntry> = snapshot
            .into_iter()
            .filter(|entry| kinds.is_empty() || kinds.contains(&entry.kind))
            .filter(|entry| match &needle {
                None => true,
                Some(q) => {
                    entry.label.to_lowercase().contains(q)
                        || entry.description.to_lowercase().contains(q)
                        || entry.id.to_lowercase().contains(q)
                        || entry
                            .tags
                            .iter()
                            .any(|t| t.to_lowercase().contains(q))
                }
            })
            .collect();

        let total = filtered.len() as u64;
        let page: Vec<AssetEntry> = filtered
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        (total, page)
    }
}

fn is_primary_for_kind(path: &Path, kind: AssetKind) -> bool {
    // Skip the well-known sidecar suffixes so e.g. `clips/12.txt`
    // isn't picked up as its own asset.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.contains(".thumb.") {
            return false;
        }
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if stem.ends_with(".thumb") {
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
    // primary asset (this matches the convention used by `memstroy-tg`
    // when it generates per-clip preview images).
    if path.components().any(|c| c.as_os_str() == "thumbs") {
        return false;
    }

    kind.primary_extensions().iter().any(|allowed| *allowed == ext)
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

    // 2. `<parent>/thumbs/<stem>.<ext>` (matches what `memstroy-tg`
    //     produces when generating ffmpeg-based clip thumbnails).
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
