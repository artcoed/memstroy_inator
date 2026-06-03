//! Scraper for Telegram public channel previews.
//!
//! Public channels are accessible without API/token via
//! `https://t.me/s/{channel}` and the `?before={id}` pagination query.
//! Each page contains up to ~16 messages with their inline videos
//! (direct CDN links in `<video src="...">`). This crate walks pages
//! backwards from the most recent post until the channel start, parses
//! each message and exposes the result.

pub mod download;
pub mod model;
pub mod scrape;

pub use download::*;
pub use model::*;
pub use scrape::*;
