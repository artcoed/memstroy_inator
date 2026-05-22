//! Shared asset server panel.
//!
//! Talks to a local `memstroy-assets-server` instance (default
//! `http://127.0.0.1:8765`) and renders a lazy, paginated, searchable
//! library tab next to the local ones. Previews and asset bodies are
//! fetched on demand (never the whole catalogue at once) so the UI
//! stays snappy even with thousands of clips on the server.
//!
//! ## High-level architecture
//!
//! - [`SharedLibraryState`] keeps the user's connection settings, the
//!   most recently loaded page of summaries, the in-flight HTTP job
//!   handle, and a small thumbnail-bytes cache keyed by `(server, id)`.
//! - [`spawn_list`] / [`spawn_thumbnail`] / [`spawn_download`] /
//!   [`spawn_tg_ingest`] kick off background tokio tasks against the
//!   server. Each task delivers its result through an `Arc<Mutex<...>>`
//!   slot the UI polls every frame.
//! - [`shared_library_panel`] is the egui widget the panel host calls
//!   inside the Library section.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, Sense, Vec2};
use serde::Deserialize;
use tokio::runtime::Handle;

use crate::state::{AssetDragKind, EditorState, LibraryAsset};

/// Default URL the panel connects to on first paint. Mirrors the
/// default `--addr 0.0.0.0:8765` from the server binary, swapped for
/// the loopback alias so it works out of the box on a dev machine.
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8765";

const PAGE_SIZE: u64 = 24;

/// Summary returned by `GET /api/assets`.
#[derive(Clone, Debug, Deserialize)]
pub struct ServerAssetSummary {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerListResponse {
    total: u64,
    #[serde(default)]
    offset: u64,
    items: Vec<ServerAssetSummary>,
}

/// Persistent state for the "Shared" library tab.
pub struct SharedLibraryState {
    /// Base URL of the server (e.g. `http://127.0.0.1:8765`). Editable.
    pub server_url: String,
    /// Free-form search string. Empty matches everything.
    pub query: String,
    /// Optional kind filter (`"clip"`, `"image"`, …). Empty = all.
    pub kind_filter: String,
    /// Latest set of items returned by the server.
    pub items: Vec<ServerAssetSummary>,
    /// Total entries the server knows about for the active filter.
    pub total: u64,
    /// Currently visible offset (incremented as the user requests more).
    pub offset: u64,
    /// Status string surfaced in the panel footer.
    pub status: String,
    /// Whether a refresh has been triggered at least once. Stops the
    /// initial empty-state nagging from looking like an error.
    pub initialised: bool,
    /// In-flight list job. The slot is set to `Some(result)` by the
    /// background task on completion.
    list_job: Arc<Mutex<Option<Result<ServerListResponse, String>>>>,
    /// In-flight thumbnail fetches keyed by asset id. We never fetch
    /// the same thumbnail twice.
    thumbnails: std::collections::HashMap<String, Arc<Mutex<ThumbState>>>,
    /// In-flight download (apply-to-project) jobs.
    download_jobs: Vec<Arc<Mutex<Option<Result<DownloadResult, String>>>>>,
    /// In-flight TG ingest job, if any.
    ingest_job: Arc<Mutex<Option<Result<String, String>>>>,
    /// Channel name typed into the TG ingest input.
    pub tg_channel: String,
    /// How many posts to pull when the user clicks Ingest.
    pub tg_limit: u32,
}

#[derive(Default, Clone)]
enum ThumbState {
    #[default]
    Pending,
    Ready(PathBuf),
    Failed,
}

#[derive(Clone, Debug)]
struct DownloadResult {
    id: String,
    kind: String,
    path: PathBuf,
}

impl Default for SharedLibraryState {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER_URL.to_string(),
            query: String::new(),
            kind_filter: String::new(),
            items: Vec::new(),
            total: 0,
            offset: 0,
            status: "Click Refresh to fetch the asset list.".to_string(),
            initialised: false,
            list_job: Arc::new(Mutex::new(None)),
            thumbnails: std::collections::HashMap::new(),
            download_jobs: Vec::new(),
            ingest_job: Arc::new(Mutex::new(None)),
            tg_channel: String::new(),
            tg_limit: 32,
        }
    }
}

impl SharedLibraryState {
    /// Compute where downloaded files for `kind` should land in the
    /// local project's `assets/` tree.
    fn local_dir_for_kind(state: &EditorState, kind: &str) -> PathBuf {
        match kind {
            "image" => state.images_dir(),
            "sound" => state.sounds_dir(),
            "particle" => state.particles_dir(),
            "video" => state.videos_dir(),
            "clip" => state.clips_dir(),
            _ => state.assets_root.join("assets").join("shared"),
        }
    }

    /// Drop kind for an applied asset — used when registering the
    /// asset into the local library so the editor knows which row
    /// kind to surface it as.
    fn drag_kind_for(kind: &str) -> AssetDragKind {
        match kind {
            "image" => AssetDragKind::Image,
            "sound" => AssetDragKind::Sound,
            "particle" => AssetDragKind::Particle,
            "video" => AssetDragKind::Video,
            "clip" => AssetDragKind::Clip,
            _ => AssetDragKind::None,
        }
    }
}

/// Public entry point for the panel host. Mirrors the
/// `library_clips_tab` etc. functions in `panels.rs`.
pub fn shared_library_panel(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    rt: &Handle,
) {
    // Pump previously-spawned jobs.
    pump_jobs(state);

    // ── Connection bar ──
    ui.horizontal(|ui| {
        ui.label(RichText::new("Server:").size(10.0));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.shared_library.server_url)
                .hint_text("http://host:port")
                .desired_width(200.0),
        );
        let refresh = ui
            .small_button("Refresh")
            .on_hover_text("Fetch the asset list from the server")
            .clicked();
        if refresh || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
            state.shared_library.offset = 0;
            state.shared_library.items.clear();
            spawn_list(rt, &mut state.shared_library, 0);
        }
    });

    // ── Search / filter bar ──
    ui.horizontal(|ui| {
        ui.label(RichText::new("Search:").size(10.0));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.shared_library.query)
                .hint_text("label, tags, …")
                .desired_width(160.0),
        );
        let mut kind = state.shared_library.kind_filter.clone();
        egui::ComboBox::from_id_source("shared_kind_filter")
            .selected_text(if kind.is_empty() { "All kinds".to_string() } else { kind.clone() })
            .width(110.0)
            .show_ui(ui, |ui| {
                for k in ["", "clip", "video", "image", "sound", "particle", "text"] {
                    let label = if k.is_empty() { "All kinds".to_string() } else { k.to_string() };
                    ui.selectable_value(&mut kind, k.to_string(), label);
                }
            });
        if kind != state.shared_library.kind_filter {
            state.shared_library.kind_filter = kind;
            state.shared_library.offset = 0;
            state.shared_library.items.clear();
            spawn_list(rt, &mut state.shared_library, 0);
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            state.shared_library.offset = 0;
            state.shared_library.items.clear();
            spawn_list(rt, &mut state.shared_library, 0);
        }
    });

    ui.add_space(4.0);

    // ── List + lazy-load ──
    let server_url = state.shared_library.server_url.clone();
    let avail_h = ui.available_height().max(120.0) - 80.0;
    egui::ScrollArea::vertical()
        .id_source("shared_library_scroll")
        .max_height(avail_h.max(80.0))
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            // Take a snapshot of the items so the closure can iterate
            // while still letting `draw_item_row` mutably borrow state.
            let items: Vec<ServerAssetSummary> = state.shared_library.items.clone();
            for item in items {
                draw_item_row(ui, state, &server_url, &item, rt);
            }

            // Auto-load next page when the user scrolls to the bottom.
            // Showing a simple "Load more" button keeps the trigger
            // explicit so the user knows what's happening.
            let len = state.shared_library.items.len() as u64;
            let total = state.shared_library.total;
            if len < total {
                ui.horizontal(|ui| {
                    if ui.button("\u{2B73} Load more").clicked() {
                        let next = state.shared_library.offset + PAGE_SIZE;
                        spawn_list(rt, &mut state.shared_library, next);
                    }
                    ui.label(
                        RichText::new(format!("{} / {}", len, total))
                            .size(9.0)
                            .color(Color32::from_rgb(160, 160, 180)),
                    );
                });
            }
        });

    ui.add_space(4.0);

    // ── TG ingest controls ──
    ui.collapsing("\u{1F4E1} Pull from Telegram channel", |ui| {
        ui.horizontal(|ui| {
            ui.label("@");
            ui.add(
                egui::TextEdit::singleline(&mut state.shared_library.tg_channel)
                    .hint_text("channel_name")
                    .desired_width(140.0),
            );
            ui.add(
                egui::DragValue::new(&mut state.shared_library.tg_limit)
                    .range(1..=500)
                    .speed(1.0)
                    .prefix("limit: "),
            );
            let chan = state.shared_library.tg_channel.trim().to_string();
            let enabled = !chan.is_empty();
            let resp = ui.add_enabled(enabled, egui::Button::new("Ingest"));
            if resp.clicked() {
                spawn_tg_ingest(
                    rt,
                    &state.shared_library.server_url,
                    &chan,
                    state.shared_library.tg_limit,
                    state.shared_library.ingest_job.clone(),
                );
                state.shared_library.status =
                    format!("Ingest started for @{}", chan);
            }
        });
        ui.label(
            RichText::new(
                "Server-side: scrapes the channel and adds new clips to the shared library.",
            )
            .size(9.0)
            .color(Color32::from_rgb(160, 160, 180)),
        );
    });

    // ── Footer status ──
    ui.add_space(2.0);
    ui.label(
        RichText::new(state.shared_library.status.as_str())
            .size(9.0)
            .color(Color32::from_rgb(160, 160, 180)),
    );
}

fn draw_item_row(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    server_url: &str,
    item: &ServerAssetSummary,
    rt: &Handle,
) {
    let card_h = 44.0;
    let card_size = Vec2::new(ui.available_width(), card_h);
    let (rect, resp) = ui.allocate_exact_size(card_size, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::Rounding::same(4.0),
        Color32::from_rgb(28, 28, 38),
    );

    // ── Thumbnail ──
    let thumb_size = Vec2::new(36.0, 36.0);
    let thumb_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x + 4.0, rect.min.y + 4.0),
        thumb_size,
    );

    let needs_thumb = item.preview_url.is_some()
        && !state.shared_library.thumbnails.contains_key(&item.id);
    if needs_thumb {
        let slot = Arc::new(Mutex::new(ThumbState::Pending));
        state.shared_library.thumbnails.insert(item.id.clone(), slot.clone());
        spawn_thumbnail(rt, server_url, &item.id, slot);
    }
    let thumb_path = state
        .shared_library
        .thumbnails
        .get(&item.id)
        .and_then(|slot| match &*slot.lock().unwrap() {
            ThumbState::Ready(p) => Some(p.clone()),
            _ => None,
        });

    if let Some(p) = thumb_path {
        let uri = format!("file://{}", p.display());
        let img = egui::Image::from_uri(uri)
            .fit_to_exact_size(thumb_size)
            .rounding(egui::Rounding::same(2.0));
        img.paint_at(ui, thumb_rect);
    } else {
        painter.rect_filled(
            thumb_rect,
            egui::Rounding::same(2.0),
            Color32::from_rgb(40, 40, 56),
        );
        painter.text(
            thumb_rect.center(),
            egui::Align2::CENTER_CENTER,
            kind_glyph(&item.kind),
            egui::FontId::proportional(18.0),
            Color32::from_rgb(140, 140, 200),
        );
    }

    // ── Label / description ──
    let text_x = thumb_rect.max.x + 8.0;
    let text_top = rect.min.y + 4.0;
    painter.text(
        egui::pos2(text_x, text_top),
        egui::Align2::LEFT_TOP,
        &item.label,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(220, 220, 240),
    );
    let descr = if item.description.is_empty() {
        format!("{} · {} bytes", item.kind, item.size_bytes)
    } else {
        let trim = item.description.replace('\n', " ");
        if trim.len() > 70 {
            format!("{}…", &trim.chars().take(70).collect::<String>())
        } else {
            trim
        }
    };
    painter.text(
        egui::pos2(text_x, text_top + 14.0),
        egui::Align2::LEFT_TOP,
        descr,
        egui::FontId::proportional(9.0),
        Color32::from_rgb(160, 160, 180),
    );

    // ── Apply button (rightmost) ──
    let btn_w = 70.0_f32;
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(rect.max.x - btn_w - 4.0, rect.min.y + 8.0),
        Vec2::new(btn_w, card_h - 16.0),
    );
    let apply_resp = ui.put(
        btn_rect,
        egui::Button::new(RichText::new("Apply").size(10.0)).small(),
    );
    let apply_clicked = apply_resp.clicked();
    if apply_resp.hovered() {
        painter.rect_stroke(
            rect,
            egui::Rounding::same(4.0),
            egui::Stroke::new(1.0, Color32::from_rgb(255, 200, 80)),
        );
    } else if resp.hovered() {
        painter.rect_stroke(
            rect,
            egui::Rounding::same(4.0),
            egui::Stroke::new(1.0, Color32::from_rgb(80, 100, 140)),
        );
    }

    if apply_clicked {
        let dest_dir = SharedLibraryState::local_dir_for_kind(state, &item.kind);
        let slot = Arc::new(Mutex::new(None));
        state.shared_library.download_jobs.push(slot.clone());
        spawn_download(rt, server_url, &item.id, &item.kind, dest_dir, slot);
        state.shared_library.status =
            format!("Downloading \"{}\"…", item.label);
    }
}

fn kind_glyph(kind: &str) -> &'static str {
    match kind {
        "clip" | "video" => "\u{1F3AC}",
        "image" => "\u{1F5BC}",
        "sound" => "\u{1F50A}",
        "particle" => "\u{2728}",
        "text" => "\u{1F4DD}",
        _ => "?",
    }
}

// ─── HTTP JOB SPAWNERS ───────────────────────────────────────────────

fn spawn_list(rt: &Handle, shared: &mut SharedLibraryState, offset: u64) {
    let url = build_list_url(
        &shared.server_url,
        &shared.kind_filter,
        &shared.query,
        offset,
        PAGE_SIZE,
    );
    let slot = shared.list_job.clone();
    *slot.lock().unwrap() = None;
    shared.status = format!("Loading offset={offset}…");
    rt.spawn(async move {
        let res = match reqwest::get(&url).await {
            Ok(resp) if resp.status().is_success() => match resp.json::<ServerListResponse>().await
            {
                Ok(j) => Ok(j),
                Err(e) => Err(format!("decode error: {e}")),
            },
            Ok(resp) => Err(format!("server error: HTTP {}", resp.status())),
            Err(e) => Err(format!("network error: {e}")),
        };
        *slot.lock().unwrap() = Some(res);
    });
}

fn build_list_url(server: &str, kind: &str, q: &str, offset: u64, limit: u64) -> String {
    let mut url = format!("{}/api/assets?offset={}&limit={}", server.trim_end_matches('/'), offset, limit);
    if !kind.is_empty() {
        url.push_str(&format!("&kind={}", urlencode(kind)));
    }
    if !q.is_empty() {
        url.push_str(&format!("&q={}", urlencode(q)));
    }
    url
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect()
}

fn spawn_thumbnail(rt: &Handle, server_url: &str, id: &str, slot: Arc<Mutex<ThumbState>>) {
    let url = format!(
        "{}/api/assets/{}/preview",
        server_url.trim_end_matches('/'),
        id
    );
    let id = id.to_string();
    rt.spawn(async move {
        let res = reqwest::get(&url).await;
        match res {
            Ok(resp) if resp.status().is_success() => {
                let ext = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|ct| {
                        if ct.contains("png") {
                            "png"
                        } else if ct.contains("jpeg") || ct.contains("jpg") {
                            "jpg"
                        } else if ct.contains("webp") {
                            "webp"
                        } else {
                            "bin"
                        }
                    })
                    .unwrap_or("bin");
                let bytes = match resp.bytes().await {
                    Ok(b) => b,
                    Err(_) => {
                        *slot.lock().unwrap() = ThumbState::Failed;
                        return;
                    }
                };
                // Cache thumbnails under the OS temp dir so the
                // application gets fresh fetches on every run instead
                // of relying on a persistent cache directory the user
                // hasn't asked us to create.
                let cache_dir = std::env::temp_dir().join("memstroy-shared-thumbs");
                let _ = std::fs::create_dir_all(&cache_dir);
                let path = cache_dir.join(format!("{}.{}", sanitise_id(&id), ext));
                if std::fs::write(&path, &bytes).is_ok() {
                    *slot.lock().unwrap() = ThumbState::Ready(path);
                } else {
                    *slot.lock().unwrap() = ThumbState::Failed;
                }
            }
            _ => *slot.lock().unwrap() = ThumbState::Failed,
        }
    });
}

fn spawn_download(
    rt: &Handle,
    server_url: &str,
    id: &str,
    kind: &str,
    dest_dir: PathBuf,
    slot: Arc<Mutex<Option<Result<DownloadResult, String>>>>,
) {
    let url = format!(
        "{}/api/assets/{}/download",
        server_url.trim_end_matches('/'),
        id
    );
    let id = id.to_string();
    let kind = kind.to_string();
    rt.spawn(async move {
        let res: Result<DownloadResult, String> = async {
            let _ = tokio::fs::create_dir_all(&dest_dir).await;
            let resp = reqwest::get(&url)
                .await
                .map_err(|e| format!("network error: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("server error: HTTP {}", resp.status()));
            }
            let filename = resp
                .headers()
                .get("content-disposition")
                .and_then(|v| v.to_str().ok())
                .and_then(filename_from_disposition)
                .unwrap_or_else(|| format!("{}.bin", sanitise_id(&id)));
            let path = dest_dir.join(filename);
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("read error: {e}"))?;
            tokio::fs::write(&path, &bytes)
                .await
                .map_err(|e| format!("write error: {e}"))?;
            Ok(DownloadResult { id: id.clone(), kind: kind.clone(), path })
        }
        .await;
        *slot.lock().unwrap() = Some(res);
    });
}

fn spawn_tg_ingest(
    rt: &Handle,
    server_url: &str,
    channel: &str,
    limit: u32,
    slot: Arc<Mutex<Option<Result<String, String>>>>,
) {
    let url = format!("{}/api/ingest/tg", server_url.trim_end_matches('/'));
    let body = serde_json::json!({ "channel": channel, "limit": limit });
    rt.spawn(async move {
        let client = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(e) => {
                *slot.lock().unwrap() = Some(Err(format!("client error: {e}")));
                return;
            }
        };
        let res = match client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => Ok(format!(
                "Ingest started (HTTP {})",
                resp.status().as_u16()
            )),
            Ok(resp) => Err(format!("server error: HTTP {}", resp.status())),
            Err(e) => Err(format!("network error: {e}")),
        };
        *slot.lock().unwrap() = Some(res);
    });
}

fn filename_from_disposition(value: &str) -> Option<String> {
    // Find `filename="..."` in the header value. Tolerant of extra
    // whitespace and case differences.
    let lower = value.to_ascii_lowercase();
    let key = "filename=";
    let idx = lower.find(key)?;
    let rest = &value[idx + key.len()..];
    let rest = rest.trim();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest.find(';').unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn sanitise_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

// ─── BACKGROUND JOB PUMP ─────────────────────────────────────────────

fn pump_jobs(state: &mut EditorState) {
    // List: replace items / append next page.
    let list_result = state
        .shared_library
        .list_job
        .lock()
        .ok()
        .and_then(|mut g| g.take());
    if let Some(res) = list_result {
        match res {
            Ok(resp) => {
                if resp.offset == 0 {
                    state.shared_library.items = resp.items;
                } else {
                    state.shared_library.items.extend(resp.items);
                }
                state.shared_library.offset = resp.offset;
                state.shared_library.total = resp.total;
                state.shared_library.initialised = true;
                state.shared_library.status = format!(
                    "Loaded {} / {} from {}",
                    state.shared_library.items.len(),
                    state.shared_library.total,
                    state.shared_library.server_url
                );
            }
            Err(e) => {
                state.shared_library.status = format!("Server error: {e}");
            }
        }
    }

    // Downloads: register the saved file as a local LibraryAsset so
    // the rest of the editor immediately sees it.
    //
    // We can't iterate the slot vec while also calling
    // `register_downloaded_asset(&mut state)` (it touches the same
    // EditorState), so we drain finished jobs into a temp buffer first
    // and process them afterwards.
    let mut finished: Vec<DownloadResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut completed: Vec<usize> = Vec::new();
    for (i, slot) in state.shared_library.download_jobs.iter().enumerate() {
        let mut g = match slot.lock() { Ok(g) => g, Err(_) => continue };
        if let Some(res) = g.take() {
            completed.push(i);
            match res {
                Ok(dl) => finished.push(dl),
                Err(e) => errors.push(e),
            }
        }
    }
    // Drop completed slots from the back to keep indices stable.
    for &i in completed.iter().rev() {
        state.shared_library.download_jobs.swap_remove(i);
    }
    for dl in finished {
        state.shared_library.status =
            format!("Saved \"{}\" to {}", dl.id, dl.path.display());
        register_downloaded_asset(state, &dl);
    }
    if let Some(e) = errors.into_iter().last() {
        state.shared_library.status = format!("Download failed: {e}");
    }

    // TG ingest: surface the result as a toast.
    let ingest_result = state
        .shared_library
        .ingest_job
        .lock()
        .ok()
        .and_then(|mut g| g.take());
    if let Some(res) = ingest_result {
        state.shared_library.status = match res {
            Ok(msg) => msg,
            Err(e) => format!("Ingest error: {e}"),
        };
    }
}

fn register_downloaded_asset(state: &mut EditorState, dl: &DownloadResult) {
    let id = dl
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("asset")
        .to_string();
    let asset = LibraryAsset {
        id: id.clone(),
        path: dl.path.clone(),
        label: id.clone(),
        thumbnail: None,
    };
    let kind = SharedLibraryState::drag_kind_for(&dl.kind);
    match kind {
        AssetDragKind::Image => state.library.images.push(asset),
        AssetDragKind::Sound => state.library.sounds.push(asset),
        AssetDragKind::Particle => state.library.particles.push(asset),
        AssetDragKind::Video | AssetDragKind::Clip => state.library.videos.push(asset),
        _ => state.library.images.push(asset),
    }
}
