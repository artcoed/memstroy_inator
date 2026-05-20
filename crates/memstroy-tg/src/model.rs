use serde::{Deserialize, Serialize};

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
}

/// Catalog file written next to the downloaded clips.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelCatalog {
    pub channel: String,
    pub fetched_at: String,
    pub posts: Vec<TgPost>,
}
