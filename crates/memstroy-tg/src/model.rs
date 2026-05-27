use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One scraped message from a public Telegram channel preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TgPost {
    /// Channel name + post id, e.g. `"MELLSTROYfonz/919"`.
    pub data_post: String,
    /// Just the numeric id parsed out of `data_post`.
    pub id: u64,
    /// Best-effort plain-text contents of the message body.
    pub text: String,
    /// All `<video src=...>` URLs found in the message (Telegram CDN).
    /// Usually the high-quality variant is the second occurrence; we
    /// keep the de-duplicated list and treat the last one as best.
    pub videos: Vec<String>,
    /// All `<img>` thumbnails or photo URLs found in the message.
    pub images: Vec<String>,
    /// Original publication date if present (ISO 8601 string).
    pub date: Option<String>,
    /// View count text as displayed (e.g. "1.21K").
    pub views: Option<String>,
}

impl TgPost {
    pub fn primary_video(&self) -> Option<&str> {
        self.videos.last().map(|s| s.as_str())
    }

    /// True when the body of the message contains the substring (case-
    /// insensitive). Useful to filter posts that carry the meme tag
    /// "Имба" — the channel's convention for share-worthy clips.
    pub fn body_contains(&self, needle: &str) -> bool {
        let lower = self.text.to_lowercase();
        lower.contains(&needle.to_lowercase())
    }

    /// Extract the "useful" part of the post text: everything before
    /// the footer boilerplate (voting markers, chat links, boost links).
    /// 
    /// The typical structure is:
    /// ```
    /// [emoji]—Main description text
    /// [emoji]—Имба/Топчик/Хрень
    /// Наш чатик— https://t.me/...
    /// Голосовать для продвижения канала—https://t.me/...?boost
    /// ```
    /// 
    /// We want to extract only the main description text.
    pub fn clean_description(&self) -> String {
        let text = &self.text;

        // Footer markers that indicate the start of boilerplate
        let footer_markers = [
            "Наш чатик",
            "Голосовать для продвижения",
            "https://t.me/+",
            "?boost",
        ];

        // Find the earliest footer marker
        let mut footer_start = text.len();
        for marker in &footer_markers {
            if let Some(pos) = text.find(marker) {
                footer_start = footer_start.min(pos);
            }
        }

        // Take text before footer
        let before_footer = &text[..footer_start];

        // Split by em-dash lines (format: "—Text")
        let lines: Vec<&str> = before_footer
            .split('—')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        // Voting markers that should be excluded
        let voting_markers = [
            "Имба",
            "Топчик", 
            "Топ",
            "Хрень",
            "Херня",
            "Такое себе",
            "Танцует с бутылкой в руках",
        ];

        // Find the main description line (first non-voting line)
        let main_line = lines
            .iter()
            .find(|line| {
                // Skip lines that are just voting markers
                !voting_markers.iter().any(|marker| line.starts_with(marker))
            })
            .copied()
            .unwrap_or("");

        // Clean up emojis and special characters from the beginning
        let cleaned = main_line
            .trim_start_matches(|c: char| {
                // Remove leading emojis, custom emoji placeholders, and whitespace
                !c.is_alphanumeric() && c != '(' && c != ')' && c != '"' && c != '\''
            })
            .trim();

        if cleaned.is_empty() {
            // Fallback: return first 60 chars of original text
            text.chars()
                .take(60)
                .collect::<String>()
                .trim()
                .to_string()
        } else {
            cleaned.to_string()
        }
    }
}

/// Persistent state for incremental downloads. Stored as
/// `assets/mellstroy/state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DownloadState {
    pub channel: String,
    /// Sorted list of all clip entries we know about (downloaded or not).
    pub clips: BTreeMap<u64, ClipEntry>,
    /// Timestamp of the last successful refresh.
    pub last_refresh: Option<String>,
}

/// Metadata for one downloaded clip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipEntry {
    pub id: u64,
    /// Clean description (before the boilerplate).
    pub description: String,
    /// Filename relative to the clips directory (e.g. `"919.mp4"`).
    pub filename: String,
    /// Whether the video file has been successfully downloaded.
    pub downloaded: bool,
    /// Original full text for reference.
    pub full_text: String,
    /// Publication date if available.
    pub date: Option<String>,
}

impl DownloadState {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, json)?;
        Ok(())
    }

    /// IDs that are in the catalog but not yet downloaded.
    pub fn pending_ids(&self) -> Vec<u64> {
        self.clips
            .values()
            .filter(|c| !c.downloaded)
            .map(|c| c.id)
            .collect()
    }

    /// Total downloaded count.
    pub fn downloaded_count(&self) -> usize {
        self.clips.values().filter(|c| c.downloaded).count()
    }

    /// Get all clips sorted by id ascending.
    pub fn all_clips_sorted(&self) -> Vec<&ClipEntry> {
        self.clips.values().collect()
    }
}

/// Catalog file written next to the downloaded clips (legacy compat).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelCatalog {
    pub channel: String,
    pub fetched_at: String,
    pub posts: Vec<TgPost>,
}
