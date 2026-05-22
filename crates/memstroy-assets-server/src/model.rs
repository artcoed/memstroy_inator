//! Data types used both internally and on the wire.
//!
//! `AssetEntry` is what we store in the in-memory index and what we
//! return from `GET /api/assets/:id`. `AssetSummary` is the trimmed
//! variant we return from the listing endpoint to keep payloads small.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Coarse classification of an asset, derived from which subdirectory
/// of the asset root the file was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Clip,
    Video,
    Image,
    Sound,
    Particle,
    Text,
}

impl AssetKind {
    /// Every supported kind, in display order.
    pub const ALL: [AssetKind; 6] = [
        AssetKind::Clip,
        AssetKind::Video,
        AssetKind::Image,
        AssetKind::Sound,
        AssetKind::Particle,
        AssetKind::Text,
    ];

    /// Subdirectory name (relative to the asset root) that holds
    /// assets of this kind.
    pub fn subdir(self) -> &'static str {
        match self {
            AssetKind::Clip => "clips",
            AssetKind::Video => "videos",
            AssetKind::Image => "images",
            AssetKind::Sound => "sounds",
            AssetKind::Particle => "particles",
            AssetKind::Text => "text",
        }
    }

    /// Map a subdirectory name back to a kind.
    pub fn from_subdir(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.subdir().eq_ignore_ascii_case(name))
    }

    /// Parse a kind from a query-string token (case-insensitive,
    /// matches the `snake_case` serde representation).
    pub fn parse_token(token: &str) -> Option<Self> {
        let token = token.trim().to_ascii_lowercase();
        match token.as_str() {
            "clip" => Some(AssetKind::Clip),
            "video" => Some(AssetKind::Video),
            "image" => Some(AssetKind::Image),
            "sound" => Some(AssetKind::Sound),
            "particle" => Some(AssetKind::Particle),
            "text" => Some(AssetKind::Text),
            _ => None,
        }
    }

    /// Allow-list of file extensions that count as the *primary* asset
    /// of this kind. Anything else found in the kind's subdirectory is
    /// treated as a sidecar (description, tags, thumbnail, …).
    pub fn primary_extensions(self) -> &'static [&'static str] {
        match self {
            AssetKind::Clip | AssetKind::Video => {
                &["mp4", "webm", "mov", "mkv", "m4v", "avi"]
            }
            AssetKind::Image => &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff"],
            AssetKind::Sound => &["wav", "mp3", "ogg", "flac", "aac", "m4a", "opus"],
            AssetKind::Particle => &["json", "yaml", "yml", "ron", "pex", "particle"],
            AssetKind::Text => &["txt", "md", "json", "yaml", "yml"],
        }
    }
}

/// Full record of one asset, as stored in the in-memory index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetEntry {
    /// Stable id derived from the file stem (no extension).
    pub id: String,
    /// Coarse classification.
    pub kind: AssetKind,
    /// Human-readable name, taken from the `<stem>.label` sidecar if
    /// present, otherwise the file stem itself.
    pub label: String,
    /// Free-form description. For `kind == Text` this is the file's
    /// contents; for everything else it is the contents of the
    /// `<stem>.txt` sidecar (or empty).
    pub description: String,
    /// Absolute path to the asset file on disk.
    pub path: PathBuf,
    /// Path to a thumbnail file, if one was found. The server uses
    /// this for `GET /api/assets/:id/preview`.
    pub thumbnail: Option<PathBuf>,
    /// Size of the primary asset file in bytes.
    pub size_bytes: u64,
    /// Tags from the `<stem>.tags` sidecar (one tag per line, or
    /// comma-separated).
    pub tags: Vec<String>,
}

/// Trimmed representation returned by the listing endpoint. Mostly
/// identical to [`AssetEntry`] but with `description` truncated and
/// `path`/`thumbnail` replaced by HTTP URLs.
#[derive(Debug, Clone, Serialize)]
pub struct AssetSummary {
    pub id: String,
    pub kind: AssetKind,
    pub label: String,
    /// Truncated to 240 chars (codepoints) with an ellipsis if the
    /// original was longer.
    pub description: String,
    /// Either `/api/assets/<id>/preview` if a thumbnail is available
    /// for this asset, or `null` if the editor should fall back to a
    /// generic icon.
    pub preview_url: Option<String>,
    pub size_bytes: u64,
    pub tags: Vec<String>,
}

impl AssetSummary {
    /// Maximum length of `description` in the summary (counted in
    /// Unicode scalar values, not bytes).
    pub const DESCRIPTION_LIMIT: usize = 240;

    pub fn from_entry(entry: &AssetEntry) -> Self {
        let description = truncate_chars(&entry.description, Self::DESCRIPTION_LIMIT);
        let preview_url = entry
            .thumbnail
            .as_ref()
            .map(|_| format!("/api/assets/{}/preview", entry.id));
        AssetSummary {
            id: entry.id.clone(),
            kind: entry.kind,
            label: entry.label.clone(),
            description,
            preview_url,
            size_bytes: entry.size_bytes,
            tags: entry.tags.clone(),
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}'); // ellipsis
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_chars("hello", 240), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let long = "a".repeat(500);
        let out = truncate_chars(&long, 240);
        assert_eq!(out.chars().count(), 240);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn kind_round_trip() {
        for k in AssetKind::ALL {
            let token = serde_json::to_string(&k).unwrap();
            // serde encodes as a quoted string e.g. "\"clip\""
            let parsed: AssetKind = serde_json::from_str(&token).unwrap();
            assert_eq!(parsed, k);
        }
    }
}
