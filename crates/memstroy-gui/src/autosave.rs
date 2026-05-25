//! Per-project autosave management.
//!
//! Each open scene tab gets its own slot in `~/.memstroy/autosaves/`.
//! A slot is a pair of files:
//!
//! * `<slot_id>.scene.yaml` — the scene snapshot itself (what
//!   `Scene::save` would write).
//! * `<slot_id>.meta.json` — sidecar metadata describing what the
//!   scene belongs to: original on-disk path (when the user has saved
//!   the project at least once), display name, and timestamp. The
//!   timestamp also lives on the scene file's mtime; we duplicate it
//!   here so the menu can format ages without one extra `metadata`
//!   syscall per row.
//!
//! `slot_id` is derived deterministically from the project's identity:
//!
//! * If the tab has been saved, it's a stable hash of the canonical
//!   path. The same project always autosaves into the same slot, so
//!   restarting the editor does not pile up duplicates.
//! * Otherwise (Untitled tab), the editor allocates a fresh slot id
//!   the first time the tab is autosaved and remembers it on the tab
//!   for the rest of the session, so a never-saved tab still updates
//!   one consistent slot instead of leaking a new file every interval.
//!
//! The `File > Open last autosave` menu lists every slot found on
//! disk, sorted newest-first, and lets the user pick which project to
//! recover.

use std::path::{Path, PathBuf};

/// Sidecar metadata persisted alongside an autosave file.
///
/// Versioned with `serde(default)` on every field so future additions
/// can be made without invalidating older `.meta.json` files left over
/// from previous editor versions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AutosaveMeta {
    /// Display name (typically the tab name at autosave time).
    #[serde(default)]
    pub name: String,
    /// Original scene file the tab is bound to, if the user has ever
    /// saved it. `None` for never-saved (Untitled) tabs.
    #[serde(default)]
    pub original_path: Option<PathBuf>,
    /// Unix-millis timestamp when this autosave was written. Used by
    /// the menu to format ages without a per-row `mtime` syscall.
    #[serde(default)]
    pub saved_at_ms: u64,
}

/// One row to render in the `Open last autosave` submenu.
#[derive(Debug, Clone)]
pub struct AutosaveEntry {
    /// Path to the scene snapshot file (the one to load).
    pub scene_path: PathBuf,
    /// Parsed metadata, if the sidecar exists and parsed cleanly.
    pub meta: Option<AutosaveMeta>,
    /// Modification time pulled from the filesystem; falls back to
    /// `meta.saved_at_ms` when the syscall fails.
    pub mtime: std::time::SystemTime,
}

/// Directory that holds every per-project autosave slot.
pub fn autosaves_dir() -> PathBuf {
    crate::state::EditorState::autosave_dir().join("autosaves")
}

/// Old single-file autosave path. We keep this around so an upgrade
/// from a previous editor version surfaces the legacy snapshot in the
/// new menu instead of silently orphaning it.
pub fn legacy_autosave_path() -> PathBuf {
    crate::state::EditorState::autosave_dir().join("autosave.scene.yaml")
}

/// Compute a stable slot id for a tab.
///
/// * `path = Some(...)` → deterministic hash of the canonical path,
///   so the same project always reuses the same slot across restarts.
/// * `path = None`      → falls back to a hash derived from `seed`,
///   which the caller is expected to keep stable for the lifetime of
///   the tab (e.g. a per-tab uuid). A pure-content hash would create
///   a fresh slot on every keystroke, defeating the point of "one
///   slot per logical project".
pub fn slot_id(path: Option<&Path>, seed: u64) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match path {
        Some(p) => {
            // Canonicalise when possible so `./scene.yaml` and the
            // absolute form collapse onto the same slot. Falls back to
            // the literal path when the file does not yet exist (a
            // freshly-typed Save As target before the first write).
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            "path".hash(&mut h);
            canon.hash(&mut h);
        }
        None => {
            "tab".hash(&mut h);
            seed.hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

/// Path to the scene file for a given slot id.
/// Uses `.memstroy` format to preserve full editor state (layout, track
/// assignments, zoom, etc.) — not just the bare scene YAML.
pub fn slot_scene_path(slot: &str) -> PathBuf {
    autosaves_dir().join(format!("{slot}.memstroy"))
}

/// Legacy `.scene.yaml` slot path. Used by the entry scanner to surface
/// autosaves written by older editor versions that used bare YAML.
#[allow(dead_code)]
pub fn slot_scene_path_legacy(slot: &str) -> PathBuf {
    autosaves_dir().join(format!("{slot}.scene.yaml"))
}

/// Path to the sidecar metadata for a given slot id.
pub fn slot_meta_path(slot: &str) -> PathBuf {
    autosaves_dir().join(format!("{slot}.meta.json"))
}

/// Write the sidecar `.meta.json` for a slot. Failures are swallowed
/// (logged via `tracing`); a missing or unreadable sidecar just falls
/// back to filename-only display in the menu.
pub fn write_meta(slot: &str, meta: &AutosaveMeta) {
    let path = slot_meta_path(slot);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(meta) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(?path, error = %e, "autosave: failed to write meta sidecar");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "autosave: failed to serialise meta sidecar");
        }
    }
}

/// Enumerate every autosave slot currently on disk, newest-first.
///
/// * Skips files whose name doesn't match the `*.scene.yaml` pattern
///   so unrelated YAML files dropped into the directory don't leak
///   into the menu.
/// * Reads the sidecar `.meta.json` if present; missing / malformed
///   sidecars yield an entry with `meta = None` so the menu can still
///   offer to load the bare scene.
/// * Includes the legacy single-file autosave (if any) at the end so
///   first-launch-after-upgrade users don't lose access to it.
pub fn list_entries() -> Vec<AutosaveEntry> {
    let mut out: Vec<AutosaveEntry> = Vec::new();

    let dir = autosaves_dir();
    if let Ok(read_dir) = std::fs::read_dir(&dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Accept both new `.memstroy` and legacy `.scene.yaml` slots.
            let slot = if name.ends_with(".memstroy") {
                &name[..name.len() - ".memstroy".len()]
            } else if name.ends_with(".scene.yaml") {
                &name[..name.len() - ".scene.yaml".len()]
            } else {
                continue;
            };
            let meta_path = slot_meta_path(slot);
            let meta = std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<AutosaveMeta>(&raw).ok());
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .or_else(|| {
                    meta.as_ref().map(|m| {
                        std::time::UNIX_EPOCH
                            + std::time::Duration::from_millis(m.saved_at_ms)
                    })
                })
                .unwrap_or(std::time::UNIX_EPOCH);
            out.push(AutosaveEntry {
                scene_path: path,
                meta,
                mtime,
            });
        }
    }

    // Surface the legacy single-file autosave too, but only if it's
    // not already shadowed by a per-project slot for the same tab.
    let legacy = legacy_autosave_path();
    if legacy.exists() {
        let mtime = std::fs::metadata(&legacy)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push(AutosaveEntry {
            scene_path: legacy,
            meta: None,
            mtime,
        });
    }

    out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    out
}

/// Format a `SystemTime` as a short relative age suitable for menu
/// rows: "12s", "5m", "3h", "2d". Returns a localisable English
/// string; the menu wraps it in `i18n::t` for Russian display.
pub fn format_age(mtime: std::time::SystemTime) -> String {
    let elapsed = mtime.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Current Unix-millis timestamp; used to stamp `meta.saved_at_ms`
/// when writing an autosave. Returns 0 on the rare clock-before-epoch
/// case so callers don't need to plumb errors.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
