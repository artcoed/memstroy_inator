use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use scraper::{Html, Selector};
use tracing::{debug, info, warn};

use crate::model::TgPost;

static TELESCO_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://cdn\d*\.telesco\.pe/file/[^\s"'<>\\]+"#).unwrap()
});

static TG_CDN_FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Telegram preview occasionally serves files from telegram-cdn* domains.
    Regex::new(r#"https://[^\s"'<>\\]*telegram-cdn[^\s"'<>\\]*"#).unwrap()
});

static OG_VIDEO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"property="og:video(?::url)?"\s+content="([^"]+)""#).unwrap()
});

fn is_likely_image_cdn_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".webp")
        || lower.contains("thumb")
        || lower.contains("/photo")
}

/// Pull direct CDN links from raw HTML (list page or `?single` embed).
fn extract_cdn_video_urls(html: &str) -> Vec<String> {
    let telesco = TELESCO_FILE_RE
        .find_iter(html)
        .map(|m| m.as_str().trim_end_matches('\\').to_string());
    let tgcdn = TG_CDN_FILE_RE
        .find_iter(html)
        .map(|m| m.as_str().trim_end_matches('\\').to_string());
    telesco
        .chain(tgcdn)
        .filter(|u| !is_likely_image_cdn_url(u))
        .collect()
}

fn merge_video_urls(into: &mut Vec<String>, more: impl IntoIterator<Item = String>) {
    let mut seen: HashSet<String> = into.iter().cloned().collect();
    for u in more {
        if seen.insert(u.clone()) {
            into.push(u);
        }
    }
}

/// If the channel preview page had no `<video src>`, try the public
/// single-post embed (`t.me/{channel}/{id}?single`).
pub async fn enrich_post_videos(client: &reqwest::Client, post: &mut TgPost) -> Result<()> {
    if post.downloadable_video().is_some() {
        return Ok(());
    }
    let url = format!("https://t.me/{}?single", post.data_post);
    debug!(url = %url, id = post.id, "enriching post videos");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} for {}", resp.status(), url));
    }
    let html = resp.text().await?;
    for cap in OG_VIDEO_RE.captures_iter(&html) {
        if let Some(u) = cap.get(1) {
            post.videos.push(u.as_str().to_string());
        }
    }
    let doc = Html::parse_document(&html);
    let video_sel = Selector::parse("video").unwrap();
    for v in doc.select(&video_sel) {
        if let Some(src) = v.value().attr("src") {
            post.videos.push(src.to_string());
        }
    }
    merge_video_urls(&mut post.videos, extract_cdn_video_urls(&html));
    Ok(())
}

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0 Safari/537.36";

/// HTTP client used by the scraper. Configured for resilient long
/// crawls (gzip, sane timeouts, browser-like UA).
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .gzip(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        // Re-validate every redirect hop so an attacker-controlled public
        // origin can't 30x-redirect a scrape/download onto loopback / LAN /
        // cloud-metadata addresses (SSRF). Legitimate t.me / CDN redirects are
        // all public and pass unchanged.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if crate::model::is_public_http_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("redirect to non-public host blocked")
            }
        }))
        .build()
        .context("building reqwest client")
}

/// Fetch one page of the channel preview. `before` corresponds to the
/// `?before=N` pagination param; `None` returns the most recent page.
pub async fn fetch_page(
    client: &reqwest::Client,
    channel: &str,
    before: Option<u64>,
) -> Result<String> {
    let mut url = format!("https://t.me/s/{}", channel);
    if let Some(b) = before {
        url.push_str(&format!("?before={}", b));
    }
    debug!(url = %url, "GET");
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} for {}", resp.status(), url));
    }
    let body = resp.text().await?;
    Ok(body)
}

/// Extract every `tgme_widget_message_wrap` block from a page and
/// convert it into a `TgPost`.
pub fn parse_posts(html: &str) -> Vec<TgPost> {
    let doc = Html::parse_document(html);
    let msg_sel = Selector::parse(".tgme_widget_message_wrap .tgme_widget_message").unwrap();
    let body_sel = Selector::parse(".tgme_widget_message_text").unwrap();
    let video_sel = Selector::parse("video").unwrap();
    let grouped_sel = Selector::parse(".tgme_widget_message_grouped_layer").unwrap();
    let img_sel = Selector::parse("img").unwrap();
    let date_sel = Selector::parse("time").unwrap();
    let views_sel = Selector::parse(".tgme_widget_message_views").unwrap();

    let mut out = Vec::new();
    for node in doc.select(&msg_sel) {
        let Some(data_post) = node.value().attr("data-post") else { continue };
        let id = data_post
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);

        let text = node
            .select(&body_sel)
            .next()
            .map(|t| extract_text(&t))
            .unwrap_or_default();

        let mut videos: Vec<String> = Vec::new();
        
        // Try to find video src from <video src="..."> tags
        for v in node.select(&video_sel) {
            if let Some(src) = v.value().attr("src") {
                videos.push(src.to_string());
            }
        }
        
        // Also try to find video URLs from background-image in video player wrappers
        // Telegram sometimes embeds video URLs in data attributes or as background thumbnails
        let video_player_sel = Selector::parse(".tgme_widget_message_video_player").unwrap();
        for player in node.select(&video_player_sel) {
            // The href points to the post, not the video. Still, some
            // variants nest another <video src="..."> in here.
            let nested_video_sel = Selector::parse("video").unwrap();
            for v in player.select(&nested_video_sel) {
                if let Some(src) = v.value().attr("src") {
                    videos.push(src.to_string());
                }
            }
        }
        
        merge_video_urls(&mut videos, extract_cdn_video_urls(&node.html()));
        for layer in node.select(&grouped_sel) {
            merge_video_urls(&mut videos, extract_cdn_video_urls(&layer.html()));
            for v in layer.select(&video_sel) {
                if let Some(src) = v.value().attr("src") {
                    merge_video_urls(&mut videos, std::iter::once(src.to_string()));
                }
            }
        }

        let images: Vec<String> = node
            .select(&img_sel)
            .filter_map(|v| v.value().attr("src").map(|s| s.to_string()))
            .filter(|s| !s.contains("/file/sticker.tgs"))
            .collect();

        let date = node
            .select(&date_sel)
            .find_map(|d| d.value().attr("datetime").map(|s| s.to_string()));

        let views = node
            .select(&views_sel)
            .next()
            .map(|v| v.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty());

        out.push(TgPost {
            data_post: data_post.to_string(),
            id,
            text,
            videos,
            images,
            date,
            views,
        });
    }
    if !out.is_empty() {
        return out;
    }

    // web.telegram.org bubble HTML (no `data-post`).
    let bubble_sel = Selector::parse("div.bubble.channel-post").unwrap();
    let bubble_text_sel = Selector::parse(".translatable-message").unwrap();
    let bubble_video_sel = Selector::parse("video.media-video, video").unwrap();
    for node in doc.select(&bubble_sel) {
        let data_mid = node.value().attr("data-mid").unwrap_or("0");
        let id = data_mid.parse::<u64>().unwrap_or(0);
        let peer = node.value().attr("data-peer-id").unwrap_or("unknown");
        let data_post = format!("{}_{}", peer, data_mid);

        let text = node
            .select(&bubble_text_sel)
            .next()
            .map(|t| extract_text(&t))
            .unwrap_or_default();

        let mut videos: Vec<String> = node
            .select(&bubble_video_sel)
            .filter_map(|v| v.value().attr("src").map(|s| s.to_string()))
            .collect();
        merge_video_urls(&mut videos, extract_cdn_video_urls(&node.html()));

        let images: Vec<String> = node
            .select(&img_sel)
            .filter_map(|v| v.value().attr("src").map(|s| s.to_string()))
            .filter(|s| !s.contains("/file/sticker.tgs"))
            .collect();

        out.push(TgPost {
            data_post,
            id,
            text,
            videos,
            images,
            date: None,
            views: None,
        });
    }
    out
}

/// Crawl a public channel from the most recent post to the very first,
/// following `prev` pagination via `?before=`. Returns posts ordered by
/// id ascending (oldest -> newest), de-duplicated.
pub async fn fetch_all(channel: &str, max_pages: usize) -> Result<Vec<TgPost>> {
    let client = build_client()?;
    let mut by_id: BTreeMap<u64, TgPost> = BTreeMap::new();
    let mut before: Option<u64> = None;
    let mut pages = 0usize;

    loop {
        if pages >= max_pages {
            warn!(pages, "max_pages reached, stopping early");
            break;
        }
        pages += 1;

        let html = match fetch_page(&client, channel, before).await {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "page fetch failed; retrying once after 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
                fetch_page(&client, channel, before).await?
            }
        };
        let posts = parse_posts(&html);
        if posts.is_empty() {
            info!("page yielded 0 posts; stopping");
            break;
        }
        let lowest_on_page = posts.iter().map(|p| p.id).min().unwrap_or(0);
        let count = posts.len();
        for p in posts {
            by_id.insert(p.id, p);
        }
        info!(
            channel = %channel,
            page = pages,
            page_count = count,
            total = by_id.len(),
            lowest_id = lowest_on_page,
            "page fetched"
        );

        // Termination conditions: reached the very first post (id <= 1),
        // or no progress between pages (sentinel).
        if lowest_on_page <= 1 {
            break;
        }
        let next_before = lowest_on_page;
        if Some(next_before) == before {
            warn!("pagination stalled; stopping");
            break;
        }
        before = Some(next_before);
        // Polite delay between page fetches.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Ok(by_id.into_values().collect())
}

fn extract_text(node: &scraper::ElementRef<'_>) -> String {
    // `descendants()` walks the subtree in document order. We pull text
    // nodes verbatim and emit a newline whenever we cross a <br>, which
    // is how Telegram formats line breaks in the preview HTML.
    let mut buf = String::new();
    for n in node.descendants() {
        match n.value() {
            scraper::Node::Text(t) => buf.push_str(t),
            scraper::Node::Element(el) if el.name() == "br" => buf.push('\n'),
            _ => {}
        }
    }
    // Keep meaningful line structure but trim noise.
    let lines: Vec<String> = buf
        .replace("\r\n", "\n")
        .split('\n')
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    lines.join("\n")
}
