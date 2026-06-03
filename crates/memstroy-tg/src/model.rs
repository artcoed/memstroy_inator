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

/// True when the URL can be fetched with a plain HTTP GET (not Telegram
/// `stream/` embed tokens or `blob:` placeholders).
pub fn is_downloadable_video_url(url: &str) -> bool {
    let u = url.trim();
    if u.is_empty() || u.starts_with("blob:") {
        return false;
    }
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return false;
    }
    let lower = u.to_ascii_lowercase();
    !(lower.contains("/stream/") || lower.contains("stream%2f"))
}

/// Score a downloadable URL by how confidently we believe it points at
/// a real video file vs a thumbnail.
///
/// Telegram's `cdnN.telesco.pe/file/...` namespace serves both the
/// full-quality clip and the JPEG poster from the same host with the
/// same path shape. The clip URLs we want look like
/// `.../file/<hash>.mp4?token=...`; thumbnail URLs either end in
/// `.jpg` (filtered out earlier by the scraper) or are extensionless
/// token blobs whose response body is secretly `image/jpeg`. Without
/// this extra signal, `downloadable_video()` used to fall back to
/// "last URL wins", which on real channels picks the extensionless
/// thumbnail blob — so we end up downloading 5 KB JPEGs renamed as
/// `.mp4`.
fn url_video_extension_score(url: &str) -> u8 {
    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".mp4")
        || path.ends_with(".mov")
        || path.ends_with(".webm")
        || path.ends_with(".m4v")
    {
        2
    } else if path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".png")
        || path.ends_with(".webp")
        || path.ends_with(".gif")
    {
        0
    } else {
        // No extension. Could be a real video that Telegram serves
        // under a token-named blob, but in practice on the public
        // preview pages it's almost always a thumbnail. Treat as
        // weak fallback.
        1
    }
}

impl TgPost {
    pub fn primary_video(&self) -> Option<&str> {
        self.videos.last().map(|s| s.as_str())
    }

    /// Best direct CDN URL suitable for `reqwest` download (skips stream/blob).
    ///
    /// Prefers URLs that explicitly carry a video file extension (`.mp4`,
    /// `.mov`, `.webm`, `.m4v`). Falls back to the original "later URL
    /// wins" tie-breaker only among candidates with the same extension
    /// score — the high-quality clip URL on Telegram tends to appear
    /// after low-quality variants in the HTML, so within a score tier
    /// the last hit is usually best.
    pub fn downloadable_video(&self) -> Option<&str> {
        self.videos
            .iter()
            .filter(|u| is_downloadable_video_url(u))
            .enumerate()
            .max_by_key(|(idx, u)| (url_video_extension_score(u), *idx))
            .map(|(_, s)| s.as_str())
    }

    /// Like [`downloadable_video`](Self::downloadable_video) but only
    /// returns a URL when we're *confident* the candidate is an actual
    /// video file — i.e. the URL path ends in `.mp4`, `.mov`, `.webm`,
    /// or `.m4v`.
    ///
    /// Use this from ingest pipelines to filter out posts that have no
    /// real video on the preview/?single pages. Telegram serves both
    /// the full-quality clip and the poster thumbnail under the same
    /// `cdnN.telesco.pe/file/...` shape, and the thumbnail URLs are
    /// extensionless token blobs whose body is secretly `image/jpeg`.
    /// `downloadable_video()` will happily return such a thumbnail URL
    /// as a last resort; this method won't.
    pub fn confidently_downloadable_video(&self) -> Option<&str> {
        self.videos
            .iter()
            .filter(|u| is_downloadable_video_url(u) && url_video_extension_score(u) >= 2)
            .enumerate()
            .max_by_key(|(idx, _)| *idx)
            .map(|(_, s)| s.as_str())
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

        // Take text before footer and split into human lines.
        let before_footer = text[..footer_start].replace("\r\n", "\n");
        let mut lines: Vec<&str> = before_footer
            .split('\n')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        // Telegram часто форматирует как "—Текст". Срежем ведущую тире-обвязку.
        for l in &mut lines {
            *l = l.trim_start_matches(|c: char| c == '—' || c == '-' || c.is_whitespace());
        }

        // Явные «голосовалки»/маркеры — их надо игнорировать, но не трогать
        // реальный текст описания.
        let is_vote_line = |s: &str| {
            let low = s.to_lowercase();
            low.starts_with("имба")
                || low.starts_with("топчик")
                || low.starts_with("топ")
                || low.starts_with("хрень")
                || low.starts_with("херн")
                || low.starts_with("такое себе")
                || low.starts_with("👍")
                || low.starts_with("👎")
        };

        let main_line = lines.into_iter().find(|l| !is_vote_line(l)).unwrap_or("");

        // Clean up leading emojis/punct from the beginning but keep quotes/brackets.
        let cleaned = main_line
            .trim_start_matches(|c: char| {
                !c.is_alphanumeric()
                    && c != '('
                    && c != ')'
                    && c != '"'
                    && c != '\''
                    && c != '«'
                    && c != '»'
            })
            .trim();

        if cleaned.is_empty() {
            // Fallback: return first 60 chars of original text
            text.chars().take(60).collect::<String>().trim().to_string()
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
