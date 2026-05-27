use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use scraper::{Html, Selector};
use tracing::{debug, info, warn};

use crate::model::TgPost;

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
            // The href points to the post, not the video
            // But we can extract video from nested video tags
            let nested_video_sel = Selector::parse("video").unwrap();
            for v in player.select(&nested_video_sel) {
                if let Some(src) = v.value().attr("src") {
                    videos.push(src.to_string());
                }
            }
        }
        
        // De-duplicate while preserving order.
        let mut seen = std::collections::HashSet::new();
        videos.retain(|u| seen.insert(u.clone()));

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
    // Collapse runs of whitespace but keep meaningful structure.
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}
