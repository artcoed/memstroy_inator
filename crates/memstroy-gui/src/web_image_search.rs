//! In-app **Web Image Search** panel.
//!
//! Shipped as a small self-contained "browser-lite" so the user can
//! find an image on the open web without leaving the editor and drop
//! it straight onto the canvas. The interaction model intentionally
//! mirrors the existing **Library → Images** tab so muscle memory
//! carries over:
//!
//! * type a query, press Enter — `spawn_web_image_search` runs in the
//!   tokio runtime and posts results back over the App's `JobEvent`
//!   channel;
//! * click a result thumbnail → `spawn_web_image_download` saves the
//!   full-resolution file into the project's `assets/images/` and the
//!   pump-events handler in [`crate::app`] both refreshes the Images
//!   tab and adds an `Overlay::Image` at the playhead;
//! * drag a result thumbnail onto the canvas / timeline — once the
//!   file has landed locally, the row populates [`crate::state::AssetDrag`]
//!   exactly the way a normal library card does, so the existing
//!   `canvas_preview::handle_canvas_asset_drag` and timeline drop
//!   handlers pick it up unchanged.
//!
//! No new HTTP / GUI / parsing crates are added — the module reuses
//! `reqwest`, `serde_json`, `image` and `egui_extras` that the GUI
//! crate already depends on. The search backend is **DuckDuckGo's
//! image endpoint** (`/i.js?o=json`), which doesn't require an API
//! key. The HTML scrape that extracts the `vqd` token is intentionally
//! tolerant of multiple known shapes; if DuckDuckGo changes its
//! markup the module surfaces a clear error string in `status` rather
//! than crashing.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use egui::{Color32, RichText, Rounding, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};

use crate::jobs::{spawn_web_image_download, spawn_web_image_search, JobEvent};
use crate::state::{AssetDragKind, EditorState, LibraryTab};

/// Cap on how many results we ingest from a single DDG page response.
/// DuckDuckGo's first page tends to return up to ~100 hits; this cap
/// guards against unbounded growth while leaving headroom for the
/// occasional response with more entries.
pub const MAX_RESULTS_PER_PAGE: usize = 200;

/// Hard cap on the total number of cached results across all loaded
/// pages. Past this we stop accepting "Load more" clicks. ~1000 cards
/// is already past the point where the panel becomes scroll-bound;
/// going higher mostly hurts paint cost.
pub const MAX_TOTAL_RESULTS: usize = 1_000;

/// One image hit returned by the search backend.
///
/// `Serialize`/`Deserialize` are derived so future "save the last
/// query with the project" features get them for free; today the
/// struct is purely in-memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebImageHit {
    /// Direct URL of the full-resolution image (PNG / JPEG / WebP).
    /// This is what we download when the user clicks "Add" / drops
    /// the row on the canvas.
    pub image_url: String,
    /// URL of a small thumbnail used in the panel grid. DuckDuckGo
    /// proxies these via its own CDN so they always return CORS-free
    /// and resolve quickly even when the source page is slow.
    pub thumbnail_url: String,
    /// Free-form alt-text / page title the search engine returned.
    pub title: String,
    /// Page the image originally came from. Surfaced as a small grey
    /// hint under the title so the user can sanity-check attribution.
    pub source_page: String,
    pub width: u32,
    pub height: u32,
    /// Local path on disk once the image has been downloaded. Until
    /// then drag-to-canvas is disabled for the row (we'd have nothing
    /// to put in `state.asset_drag.dragging`). `Some` after a
    /// successful download — set by the pump-events handler.
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    /// True while a download for this row is in flight. Cleared by
    /// the pump-events handler regardless of success / failure so the
    /// UI doesn't get stuck with a permanent spinner.
    #[serde(default, skip)]
    pub downloading: bool,
    /// Error string from the most recent download attempt, if any.
    /// Rendered as a tooltip on the row so failures don't disappear
    /// silently.
    #[serde(default, skip)]
    pub last_error: Option<String>,
}

impl WebImageHit {
    pub fn new(
        image_url: String,
        thumbnail_url: String,
        title: String,
        source_page: String,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            image_url,
            thumbnail_url,
            title,
            source_page,
            width,
            height,
            local_path: None,
            downloading: false,
            last_error: None,
        }
    }
}

/// Panel-local state. Lives inside `EditorState` (see `state.rs`) so
/// it's kept across show/hide cycles of the floating window.
#[derive(Default)]
pub struct WebImageSearchState {
    /// Current text in the search box (live-edited).
    pub query: String,
    /// Last query that was actually sent to the backend, so retyping
    /// the same string + Enter doesn't refire the search uselessly.
    pub last_sent_query: String,
    /// Results from the latest successful search, accumulated across
    /// paginated page fetches.
    pub results: Vec<WebImageHit>,
    /// Free-form status string shown above the grid (errors, "no
    /// results", "downloading 'foo.jpg'…", …).
    pub status: String,
    /// True while a search request is in flight. Drives a spinner
    /// next to the search box.
    pub searching: bool,
    /// Monotonic id used to associate a download response with the
    /// hit that triggered it. We pass this in to `spawn_web_image_download`
    /// and the matching `JobEvent::WebImageDownloaded` carries it back.
    pub next_request_id: u64,
    /// DuckDuckGo `s=` start-offset for the next page; `None` means
    /// "no more pages" (initial state, end-of-results, or after an
    /// error). The "Load more" button is only shown when this is
    /// `Some(_)`.
    pub next_offset: Option<u32>,
    /// 1-indexed page counter for status display only ("Page 2: 100
    /// more results"). Resets to 0 on a fresh search.
    pub page_count: u32,
    /// Last outer size of the floating window (remembered so opening /
    /// searching does not reset the user's layout).
    pub panel_size: Option<egui::Vec2>,
}

/// Mark the matching hit as "no longer downloading" and update its
/// local_path / last_error fields based on the download outcome.
/// Returns `Some(asset.path)` and the "place on canvas?" flag the
/// caller asked for so the App-side handler can do the canvas
/// insertion outside the borrow of `state.web_image_search`.
pub fn ingest_download_result(
    state: &mut WebImageSearchState,
    image_url: &str,
    result: &Result<crate::state::LibraryAsset, String>,
) {
    for hit in state.results.iter_mut() {
        if hit.image_url == image_url {
            hit.downloading = false;
            match result {
                Ok(asset) => {
                    hit.local_path = Some(asset.path.clone());
                    hit.last_error = None;
                }
                Err(e) => {
                    hit.last_error = Some(e.clone());
                }
            }
            return;
        }
    }
}

/// Render the floating "Web Image Search" window. Called every frame
/// from `App::update` while `state.web_image_search_open` is true; the
/// `&mut bool` lets egui's window close button toggle the field on
/// the editor state.
pub fn show_window(
    ctx: &egui::Context,
    state: &mut EditorState,
    tx: &Sender<JobEvent>,
) {
    let mut open = state.web_image_search_open;
    let window_id = egui::Id::new("web_image_search_window");

    let window_size = state
        .web_image_search
        .panel_size
        .unwrap_or(egui::vec2(560.0, 640.0));

    let mut window = egui::Window::new(format!("\u{1F310} {}", crate::i18n::t("Web Image Search")))
        .id(window_id)
        .open(&mut open)
        .default_size(window_size)
        .min_width(300.0)
        .max_width(1200.0)
        .min_height(280.0)
        .max_height(1200.0)
        .resizable([true, true])
        .collapsible(true)
        .scroll(false);
    
    // Preserve window position if it exists
    if let Some(pos) = ctx.memory(|m| m.area_rect(window_id).map(|r| r.left_top())) {
        window = window.current_pos(pos);
    }
    
    let response = window.show(ctx, |ui| {
            // Use the saved window size to calculate body width instead of clip_rect,
            // which can fluctuate and cause unwanted expansion.
            let window_size = state.web_image_search.panel_size.unwrap_or(egui::vec2(560.0, 640.0));
            let margins = ui.spacing().window_margin.sum().x;
            let body_w = (window_size.x - margins).max(280.0);
            
            // Hard-clamp the content width to prevent expansion.
            ui.set_max_width(body_w);
            ui.set_width(body_w);

            // Claim the full window body for pointer hits so clicks on
            // padding / gaps between widgets don't fall through to the
            // timeline / layers panel underneath.
            let block_rect = ui.max_rect();
            ui.interact(
                block_rect,
                ui.id().with("web_image_search_pointer_block"),
                Sense::click(),
            );

            window_body(ui, state, tx, body_w);
        });
    
    // Only update saved size when user is actively resizing (dragging edges)
    if let Some(inner_response) = response {
        if inner_response.response.dragged() || inner_response.response.drag_stopped() {
            if let Some(new_rect) = ctx.memory(|m| m.area_rect(window_id)) {
                let new_size = new_rect.size();
                if new_size.x >= 300.0 && new_size.y >= 280.0 && new_size.x <= 1200.0 && new_size.y <= 1200.0 {
                    state.web_image_search.panel_size = Some(new_size);
                }
            }
        }
    }
    
    state.web_image_search_open = open;
}

fn window_body(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    tx: &Sender<JobEvent>,
    body_w: f32,
) {
    use crate::i18n::t;

    // ── Search bar ─────────────────────────────────────────────────
    ui.allocate_ui_with_layout(
        egui::vec2(body_w, 30.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_max_width(body_w);
            
            let text_width = (body_w - 110.0).max(80.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.web_image_search.query)
                    .hint_text(t("Search images..."))
                    .desired_width(text_width)
                    .clip_text(true),
            );
            let enter = resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let go = ui.button(format!("\u{1F50D} {}", t("Search"))).clicked() || enter;
            if go {
                kick_search(state, tx, /*append=*/ false);
                // Refocus the box so the user can immediately edit the
                // query without clicking back into it.
                resp.request_focus();
            }
            if state.web_image_search.searching {
                ui.spinner();
                ui.label(
                    RichText::new(t("searching..."))
                        .size(10.0)
                        .color(Color32::from_rgb(255, 200, 80)),
                );
            }
        },
    );

    // ── Status / hint line ─────────────────────────────────────────
    let status = &state.web_image_search.status;
    let is_error = status.starts_with('\u{274C}');
    if is_error {
        // Error state — show prominently with a retry button
        ui.add_space(4.0);
        ui.allocate_ui_with_layout(
            egui::vec2(body_w, 20.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_max_width(body_w);
                ui.label(
                    RichText::new(status.as_str())
                        .size(12.0)
                        .color(Color32::from_rgb(255, 100, 100))
                        .strong(),
                );
            },
        );
        ui.add_space(4.0);
        ui.allocate_ui_with_layout(
            egui::vec2(body_w, 30.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_max_width(body_w);
                if ui.button(format!("\u{1F504} {}", t("Retry"))).clicked() {
                    kick_search(state, tx, /*append=*/ false);
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(t("Check your internet connection or try a different query."))
                            .size(10.0)
                            .italics()
                            .color(Color32::from_rgb(160, 160, 180)),
                    )
                    .wrap(),
                );
            },
        );
    } else {
        let hint = if !status.is_empty() {
            status.clone()
        } else if state.web_image_search.results.is_empty() {
            t("Type a query and press Enter. Click a result to add it to the project.").to_string()
        } else {
            format!(
                "{} {}",
                t("Found"),
                state.web_image_search.results.len(),
            )
        };
        ui.add(
            egui::Label::new(
                RichText::new(hint)
                    .size(10.5)
                    .italics()
                    .color(Color32::from_rgb(160, 160, 180)),
            )
            .wrap(),
        );
    }
    ui.add_space(4.0);

    ui.separator();
    ui.add_space(2.0);

    // ── Results grid ───────────────────────────────────────────────
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Constrain the scroll area content to prevent expansion
            ui.set_max_width(body_w);
            
            // When searching and no results yet, show a centered loading state
            if state.web_image_search.searching && state.web_image_search.results.is_empty() {
                ui.add_space(40.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(body_w, 100.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.set_max_width(body_w);
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(t("Searching DuckDuckGo..."))
                                .size(13.0)
                                .color(Color32::from_rgb(200, 200, 220)),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(t("This may take a few seconds."))
                                .size(10.0)
                                .italics()
                            .color(Color32::from_rgb(140, 140, 160)),
                    );
                });
                return;
            }

            results_grid(ui, state, tx);

            // ── Pagination control ──────────────────────────────
            let has_more = state.web_image_search.next_offset.is_some()
                && !state.web_image_search.searching
                && state.web_image_search.results.len() < MAX_TOTAL_RESULTS;
            if has_more {
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 40.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        let next_pg = state.web_image_search.page_count + 1;
                        let label = format!(
                            "\u{2B07} {} (page {})",
                            t("Load more"),
                            next_pg + 1,
                        );
                        if ui
                            .button(RichText::new(label).size(12.0))
                            .on_hover_text(t(
                                "Fetch the next page of results.",
                            ))
                            .clicked()
                        {
                            kick_search(state, tx, /*append=*/ true);
                        }
                    },
                );
                ui.add_space(8.0);
            } else if state.web_image_search.searching
                && !state.web_image_search.results.is_empty()
            {
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 40.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new(t("Loading more..."))
                                .size(10.0)
                                .color(Color32::from_rgb(200, 200, 220)),
                        );
                    },
                );
                ui.add_space(8.0);
            } else if state.web_image_search.next_offset.is_none()
                && state.web_image_search.results.len() >= MAX_RESULTS_PER_PAGE
            {
                ui.add_space(6.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new(t("(end of results)"))
                                .size(10.0)
                                .italics()
                                .color(Color32::from_rgb(120, 120, 140)),
                        );
                    },
                );
                ui.add_space(6.0);
            }
        });
}

fn results_grid(ui: &mut egui::Ui, state: &mut EditorState, tx: &Sender<JobEvent>) {
    // Subtract a safety margin equal to the vertical scrollbar width
    // BEFORE we compute how many cards fit per row. The first paint
    // (no scrollbar yet) and subsequent paints (scrollbar present)
    // would otherwise compute different `cols` values, which makes
    // the grid jitter / overflow / "разъезжается" — the exact symptom
    // the user reported.
    const VBAR_WIDTH: f32 = 16.0;
    const HORIZONTAL_MARGIN: f32 = 8.0;
    let avail_w = (ui.available_width() - VBAR_WIDTH - HORIZONTAL_MARGIN).max(140.0);
    let card_w = 150.0_f32;
    let card_h = 180.0_f32;
    let gap = 6.0_f32;
    // Floor of how many cards fit; never less than 1 so a narrow
    // panel still renders one column.
    let cols = ((avail_w + gap) / (card_w + gap)).floor().max(1.0) as usize;

    if state.web_image_search.results.is_empty() {
        ui.add_space(12.0);
        ui.allocate_ui_with_layout(
            egui::vec2(avail_w, 80.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.label(
                    RichText::new("\u{1F50D}")
                        .size(40.0)
                        .color(Color32::from_rgb(62, 60, 42)),
                );
                ui.label(
                    RichText::new(crate::i18n::t("No results yet."))
                        .size(11.0)
                        .color(Color32::from_rgb(140, 140, 160)),
                );
            },
        );
        return;
    }

    // We mutate state.web_image_search.results inside the loop (to
    // flip `downloading`), and we kick downloads through a borrow of
    // `state` as a whole. Rather than fight the borrow checker, we
    // drive the loop by index and look up the hit fresh on each pass.
    let n = state.web_image_search.results.len();
    let mut i = 0;
    while i < n {
        // Allocate the row with an explicit max width so a stale
        // `cols` value (e.g. one frame after the scrollbar appears)
        // can't cause `ui.horizontal` to measure wider than the
        // panel and pump the parent window wider on the next frame.
        let row_max_w = avail_w;
        ui.allocate_ui_with_layout(
            egui::vec2(row_max_w, card_h),
            egui::Layout::left_to_right(egui::Align::TOP),
            |ui| {
                ui.set_max_width(row_max_w);
                ui.set_width(row_max_w);
                for col in 0..cols {
                    let idx = i + col;
                    if idx >= n {
                        break;
                    }
                    draw_card(ui, state, tx, idx, card_w, card_h);
                    if col + 1 < cols {
                        ui.add_space(gap);
                    }
                }
            },
        );
        ui.add_space(gap);
        i += cols;
    }
}

/// Draw one result card and wire its click / drag interactions.
fn draw_card(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    tx: &Sender<JobEvent>,
    idx: usize,
    card_w: f32,
    card_h: f32,
) {
    // Read-only snapshot of the row so the rest of the function can
    // mutate state freely. The row is small (~12 fields) so the clone
    // is essentially free.
    let hit = state.web_image_search.results[idx].clone();

    let frame = egui::Frame::none()
        .fill(Color32::from_rgb(32, 30, 18))
        .rounding(Rounding::same(5.0))
        .inner_margin(egui::Margin::same(4.0))
        .stroke(Stroke::new(
            1.0,
            if hit.local_path.is_some() {
                // Subtly highlight rows we've already cached locally
                // — they're the ones that support drag.
                Color32::from_rgb(70, 110, 70)
            } else {
                Color32::from_rgb(54, 52, 36)
            },
        ));

    let resp = frame.show(ui, |ui| {
        ui.set_width(card_w);
        ui.set_height(card_h);
        ui.vertical(|ui| {
            // Thumbnail box ─── fixed height, image stretched to fit.
            let thumb_size = Vec2::new(card_w - 8.0, card_h - 60.0);
            let (rect, _) = ui.allocate_exact_size(thumb_size, Sense::hover());
            ui.painter().rect_filled(
                rect,
                Rounding::same(3.0),
                Color32::from_rgb(22, 21, 12),
            );
            // Use the thumbnail URL as-is — egui_extras' image
            // loaders fetch HTTPS resources transparently because
            // `install_image_loaders` is called in `main.rs`.
            let img = egui::Image::from_uri(hit.thumbnail_url.clone())
                .fit_to_exact_size(thumb_size)
                .maintain_aspect_ratio(true)
                .rounding(Rounding::same(3.0));
            img.paint_at(ui, rect);

            if hit.downloading {
                // Draw a translucent overlay + spinner-ish dot pattern
                // so the user knows that row is busy.
                ui.painter().rect_filled(
                    rect,
                    Rounding::same(3.0),
                    Color32::from_rgba_premultiplied(0, 0, 0, 140),
                );
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "\u{2B6F}",
                    egui::FontId::proportional(22.0),
                    Color32::from_rgb(255, 220, 80),
                );
            } else if hit.local_path.is_some() {
                // Tiny green dot in the corner to confirm the file
                // is locally cached and drag-ready.
                let dot = rect.right_top() + egui::vec2(-6.0, 6.0);
                ui.painter().circle_filled(
                    dot,
                    3.0,
                    Color32::from_rgb(120, 220, 120),
                );
            }

            // Title (one line, ellipsised by egui automatically)
            ui.label(
                RichText::new(if hit.title.is_empty() { crate::i18n::t("(untitled)") } else { hit.title.as_str() })
                    .size(10.5)
                    .color(Color32::from_rgb(220, 220, 240))
                    .strong(),
            );
            // Source / dimensions
            let dim = format!("{}\u{00D7}{}", hit.width, hit.height);
            let host = host_of(&hit.source_page);
            ui.label(
                RichText::new(format!("{}  \u{00B7}  {}", dim, host))
                    .size(9.0)
                    .color(Color32::from_rgb(140, 140, 160)),
            );
        });
    }).response;

    let resp = resp.interact(Sense::click_and_drag());

    // Tooltips: errors live here so they stay out of the grid.
    if let Some(err) = &hit.last_error {
        resp.clone().on_hover_text(format!("\u{26A0} {}", err));
    } else {
        resp.clone().on_hover_text(format!(
            "{}\n{}",
            hit.title,
            hit.source_page,
        ));
    }

    // ── Click ⇒ download + place on canvas ──
    if resp.clicked() && !hit.downloading {
        kick_download(state, tx, idx, /*place_on_canvas=*/ true);
    }

    // ── Drag ⇒ if locally cached, populate asset_drag like any
    //          library row; otherwise kick a background download
    //          so the file is ready by the time the next gesture
    //          lands. ──
    if resp.dragged() {
        if let Some(local) = hit.local_path.clone() {
            state.asset_drag.dragging = Some(local.clone());
            state.asset_drag.kind = AssetDragKind::Image;
            state.asset_drag.label = if hit.title.is_empty() {
                crate::i18n::t("web image").to_string()
            } else {
                hit.title.clone()
            };
            state.asset_drag.thumbnail = Some(local);
            if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                state.asset_drag.pos = [pos.x, pos.y];
            }
        } else if !hit.downloading {
            // Pre-fetch on drag-start so a re-attempt picks up the
            // local file. We DON'T flip place_on_canvas here, because
            // the canvas drop handler will commit the placement once
            // the file lands.
            kick_download(state, tx, idx, /*place_on_canvas=*/ false);
            state.web_image_search.status = format!(
                "\u{2B6F} {} \u{2014} {}",
                crate::i18n::t("Downloading"),
                short_url(&hit.image_url),
            );
        }
    }
}

// ── Action helpers ────────────────────────────────────────────────────

/// Kick a fresh search (when `append == false`) or fetch the next page
/// (when `append == true`). The append path keeps the existing
/// `results` and the cached `next_offset` is consumed as the
/// search engine `s=` start offset.
fn kick_search(state: &mut EditorState, tx: &Sender<JobEvent>, append: bool) {
    let q = state.web_image_search.query.trim().to_string();
    if q.is_empty() {
        state.web_image_search.status = crate::i18n::t("Type a search query first.").to_string();
        return;
    }
    if state.web_image_search.searching {
        // Already running a search/page fetch; ignore the click so we
        // don't spawn a second background request.
        return;
    }
    let Some(handle) = state.tokio_handle.clone() else {
        state.web_image_search.status =
            crate::i18n::t("Tokio runtime missing — search disabled.").to_string();
        return;
    };

    // For the "append" path, only proceed when we actually have a
    // saved offset from the previous response. Otherwise treat the
    // call as a fresh search to avoid an HTTP request that would
    // immediately return zero hits.
    let offset = if append {
        match state.web_image_search.next_offset {
            Some(o) => o,
            None => return,
        }
    } else {
        0
    };

    if !append {
        state.web_image_search.last_sent_query = q.clone();
        state.web_image_search.results.clear();
        state.web_image_search.next_offset = None;
        state.web_image_search.page_count = 0;
        state.web_image_search.status = format!("{} \"{}\"...", crate::i18n::t("\u{1F50D} Searching for"), q);
    } else {
        state.web_image_search.status = format!(
            "{} {}...",
            crate::i18n::t("\u{2B07} Loading page"),
            state.web_image_search.page_count + 1,
        );
    }
    state.web_image_search.searching = true;
    spawn_web_image_search(&handle, tx.clone(), q, offset);
}

fn kick_download(
    state: &mut EditorState,
    tx: &Sender<JobEvent>,
    hit_idx: usize,
    place_on_canvas: bool,
) {
    if hit_idx >= state.web_image_search.results.len() {
        return;
    }
    // Mark the row as in-flight so the UI shows the spinner overlay
    // and we don't kick duplicate fetches if the user spams clicks.
    if state.web_image_search.results[hit_idx].downloading {
        return;
    }
    state.web_image_search.results[hit_idx].downloading = true;
    state.web_image_search.results[hit_idx].last_error = None;

    let hit = state.web_image_search.results[hit_idx].clone();
    let dest = state.images_dir();

    // Switch the library tab to Images so the downloaded file is
    // visible the moment it lands — matches the Ctrl+V paste flow.
    state.library_tab = LibraryTab::Images;

    let Some(handle) = state.tokio_handle.clone() else {
        state.web_image_search.results[hit_idx].downloading = false;
        state.web_image_search.results[hit_idx].last_error =
            Some(crate::i18n::t("Tokio runtime missing").to_string());
        return;
    };

    let request_id = state.web_image_search.next_request_id;
    state.web_image_search.next_request_id =
        state.web_image_search.next_request_id.wrapping_add(1);

    state.web_image_search.status = format!(
        "\u{2B6F} {} \u{2192} {}",
        crate::i18n::t("Downloading"),
        short_url(&hit.image_url),
    );

    spawn_web_image_download(
        &handle,
        tx.clone(),
        hit.image_url.clone(),
        hit.title.clone(),
        dest,
        request_id,
        place_on_canvas,
    );
}

// ── tiny formatting helpers ───────────────────────────────────────────

/// `https://example.com/foo/bar.jpg?baz=1` → `example.com`.
fn host_of(url: &str) -> &str {
    let s = url.trim_start_matches("http://").trim_start_matches("https://");
    let end = s.find('/').unwrap_or(s.len());
    &s[..end]
}

/// Very short URL summary for the status line ("filename.jpg").
fn short_url(url: &str) -> String {
    let trimmed = url.split('?').next().unwrap_or(url);
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if last.is_empty() {
        trimmed.to_string()
    } else if last.len() > 60 {
        format!("{}\u{2026}", &last[..60])
    } else {
        last.to_string()
    }
}
