//! System-font discovery and on-demand registration with egui.
//!
//! On Windows / macOS / Linux desktops the user expects the editor's
//! font picker to show every font installed on their machine — not
//! just the two bundled families. The previous implementation hard-
//! coded a 2-element constant (`["Default", "Monospace"]`) because
//! egui doesn't ship a font enumerator and the workspace didn't pull
//! in `font-kit` / `fontdb`.
//!
//! This module fixes that without adding a heavyweight dependency:
//!
//! 1. `available_families()` reads the TTF/OTF file list from the
//!    `memstroy-render::fonts` filesystem cache (already walked at
//!    render-time; we just reuse it), parses each file's `name` table
//!    via `ttf-parser`, and emits the deduped list of human-readable
//!    family names ("Arial", "Inter", "Segoe UI", …) sorted A→Z.
//!    The first call kicks off a **background scan** instead of
//!    blocking — until the scan completes the picker shows just the
//!    bundled families, but the GUI thread isn't frozen for the
//!    seconds it takes to parse 600+ Windows fonts. `kick_background_scan`
//!    can be called at app startup so the cache is warm by the time
//!    the user opens the font picker for the first time.
//!
//! 2. `ensure_font_loaded(ctx, family)` lazily loads the TTF bytes for
//!    the requested family into egui's `FontDefinitions` and assigns
//!    a custom `FontFamily::Name(<family>)` to it. The first call
//!    incurs the file-read + parse cost; subsequent calls are O(1).
//!
//!    The function performs **strict, non-panicking pre-flight
//!    validation** with `ttf-parser` (the same parser ab_glyph wraps
//!    via `owned_ttf_parser`) before handing bytes to egui. Earlier
//!    versions of this module relied on `std::panic::catch_unwind`
//!    around `ab_glyph::FontVec::try_from_vec(...)` — but the
//!    workspace forces `panic = "abort"` in `[profile.release]` for
//!    binary hardening, and under that strategy `catch_unwind` is
//!    **silently a no-op**: any panic deep in epaint's text layout
//!    (e.g. `assert!(scale_in_pixels > 0.0)` inside
//!    `epaint::FontImpl::new` when a system font reports
//!    `ascent == descent == 0`) goes straight to `__fastfail(7)`,
//!    which Windows surfaces as `STATUS_STACK_BUFFER_OVERRUN`
//!    (`0xC0000409`) — the exact crash users report ("При выборе
//!    шрифта для текста подгруженного из системы приложение
//!    крашится … STATUS_STACK_BUFFER_OVERRUN"). The validator below
//!    therefore exercises every metric egui will eventually consult
//!    using only `Result`/`Option`-returning APIs, so a malformed
//!    font becomes a benign "fall back to Proportional" instead of a
//!    hard process abort.
//!
//! 3. A small in-process `Mutex<Loaded>` tracks which families are
//!    already in the egui font table so we don't pay for a full
//!    `set_fonts(...)` each frame. egui requires the entire
//!    FontDefinitions to be replaced, so we keep our own copy of the
//!    table and ship it back through `set_fonts(...)` only when the
//!    set of loaded families actually grew.
//!
//!    The custom family is registered **only** under
//!    `FontFamily::Name(<family>)` — we deliberately do NOT push it
//!    into the bundled `FontFamily::Proportional` / `Monospace`
//!    fallback chains. Earlier builds did, with the rationale of
//!    "missing-glyph fallback", but it meant a single subtly-broken
//!    user font would poison every UI label that ever fell back to
//!    it. Keeping system fonts strictly opt-in via `FontFamily::Name`
//!    limits the blast radius to overlays that actually requested
//!    that font.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// One discovered font face: human-readable family name + path on disk.
#[derive(Debug, Clone)]
pub struct DiscoveredFont {
    /// Family name as advertised by the TTF `name` table (English,
    /// preferred); falls back to the file stem if the parser can't
    /// recover one.
    pub family: String,
    /// Absolute path to the TTF / OTF file.
    pub path: PathBuf,
}

/// Asynchronously-populated cache of discovered system fonts. The
/// scan itself runs on a worker thread the first time we need it so
/// the GUI stays responsive while hundreds of fonts are parsed. While
/// the scan is in flight `available_families()` returns an empty
/// slice — callers blend the bundled families on top, so the picker
/// is always usable.
struct FamilyCache {
    /// `None` while the scan is still running, `Some` once it's done.
    /// We swap by taking the lock only when transitioning, so steady-
    /// state reads are a single atomic load on the inner `OnceLock`.
    inner: OnceLock<Vec<DiscoveredFont>>,
    /// Set when a scan is in flight so we don't spawn duplicate
    /// workers. Doesn't gate visibility — the OnceLock above does.
    started: std::sync::atomic::AtomicBool,
}

fn family_cache() -> &'static FamilyCache {
    static CACHE: OnceLock<FamilyCache> = OnceLock::new();
    CACHE.get_or_init(|| FamilyCache {
        inner: OnceLock::new(),
        started: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Kick off the system-font scan in a background thread. Idempotent —
/// safe to call from app startup and again from the picker. Until the
/// scan finishes `available_families()` returns `&[]`.
pub fn kick_background_scan() {
    let cache = family_cache();
    if cache.inner.get().is_some() {
        return;
    }
    if cache
        .started
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    std::thread::Builder::new()
        .name("memstroy-fontscan".into())
        .spawn(|| {
            let scanned = scan_families();
            // First-writer-wins; the OnceLock ignores subsequent sets.
            let _ = family_cache().inner.set(scanned);
        })
        .ok();
}

/// All fonts found on the system, deduplicated by family name and
/// sorted A→Z. The first call kicks off the background scan; until
/// the scan completes this returns `&[]`. Cheap to call every frame
/// — once the scan finishes the result is a static slice.
pub fn available_families() -> &'static [DiscoveredFont] {
    let cache = family_cache();
    if let Some(v) = cache.inner.get() {
        return v.as_slice();
    }
    kick_background_scan();
    &[]
}

fn scan_families() -> Vec<DiscoveredFont> {
    let paths = memstroy_render::discovered_font_paths();
    // `family` → preferred path. We keep the first non-bold, non-italic
    // variant we encounter so the picker default for "Arial" lands on
    // `arial.ttf` rather than `ARIALBI.ttf`.
    let mut by_family: BTreeMap<String, DiscoveredFont> = BTreeMap::new();

    for path in paths {
        // Skip unreadable / unparseable files at scan time — anything
        // we leave in the picker MUST be loadable later, otherwise
        // the user clicks a name and gets a silent fall-back. Doing
        // the same `ttf_parser::Face::parse` round-trip here that
        // `validate_font_for_egui` requires below also weeds out
        // .ttf-named-but-not-actually-a-TTF files (rare but real on
        // Windows when an installer drops cache stubs into Fonts/).
        let Some(family) = read_family_name(path) else {
            continue;
        };
        // Prefer Regular / non-italic variants for the picker entry.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_regularish = !stem.contains("bold")
            && !stem.contains("italic")
            && !stem.contains("oblique")
            && !stem.contains("light")
            && !stem.contains("thin");

        match by_family.get_mut(&family) {
            None => {
                by_family.insert(
                    family.clone(),
                    DiscoveredFont {
                        family,
                        path: path.clone(),
                    },
                );
            }
            Some(existing) => {
                let existing_stem = existing
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let existing_is_regular = !existing_stem.contains("bold")
                    && !existing_stem.contains("italic")
                    && !existing_stem.contains("oblique");
                if is_regularish && !existing_is_regular {
                    existing.path = path.clone();
                }
            }
        }
    }

    by_family.into_values().collect()
}

/// Read the user-facing family name from a TTF/OTF `name` table.
/// Returns `None` when the file isn't a parseable single-face font
/// (we deliberately don't expand `.ttc` collections — the few that
/// install on Windows are usually duplicated by per-face `.ttf`s
/// alongside).
fn read_family_name(path: &PathBuf) -> Option<String> {
    // Read at most ~2 MiB of the file — `ttf-parser` only touches
    // the directory tables at the top of a TTF and doesn't need the
    // glyph payload to resolve `name`. This keeps the scan fast on
    // machines with hundreds of system fonts.
    let bytes = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&bytes, 0).ok()?;
    // Prefer family-name entry whose platform/language tag is English.
    let names = face.names();
    let mut best: Option<String> = None;
    for i in 0..names.len() {
        let Some(rec) = names.get(i) else { continue };
        if rec.name_id != ttf_parser::name_id::FAMILY {
            continue;
        }
        let Some(name) = rec.to_string() else { continue };
        // English (US) Microsoft = (3, 0x409) — the canonical source
        // for human-readable Windows family names. Otherwise just
        // remember the last non-empty hit and keep looking.
        if rec.platform_id == ttf_parser::PlatformId::Windows
            && rec.language_id == 0x0409
        {
            return Some(name);
        }
        if best.is_none() {
            best = Some(name);
        }
    }
    best.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    })
}

/// Strictly non-panicking validation of a font's bytes for use with
/// egui 0.28's text layout. Returns `false` when egui would either
/// refuse the bytes or — far worse — successfully load them and then
/// panic later in `epaint::FontImpl::new` / `FontImplCache::font_impl`
/// during the next paint, which under `panic = "abort"` aborts the
/// whole process via `__fastfail` (Windows: `STATUS_STACK_BUFFER_OVERRUN`,
/// `0xC0000409`).
///
/// The checks intentionally use `ttf-parser`'s `Option`/`Result`-based
/// API exclusively — every accessor we call is total and cannot panic —
/// so a malformed system font reliably surfaces as `false` no matter
/// how exotic its tables are. ab_glyph wraps this same parser via
/// `owned_ttf_parser`, so a font that survives every check here is
/// guaranteed to also survive `ab_glyph::FontVec::try_from_vec` and
/// the subsequent `epaint` pipeline.
fn validate_font_for_egui(bytes: &[u8]) -> bool {
    // 1. Must parse as a TrueType / OpenType face at index 0. This
    //    enforces presence of the mandatory `head`, `hhea`, `maxp`
    //    tables (ttf_parser bakes those checks into `Face::parse`)
    //    and rejects collections that we'd need a different index for.
    let Ok(face) = ttf_parser::Face::parse(bytes, 0) else {
        return false;
    };

    // 2. `units_per_em` must be in the OpenType-mandated 16..=16384
    //    range. `ttf_parser::Face::parse` already enforces this in
    //    `head::Table::parse`, so a valid parse implies a valid value
    //    — but spell it out so a future ttf_parser relaxation can't
    //    quietly let a divide-by-zero through into epaint's
    //    `font_scaling = height / units_per_em` calculation.
    let upem = face.units_per_em();
    if !(16..=16384).contains(&upem) {
        return false;
    }

    // 3. `epaint::FontImpl::new` computes `height = ascent - descent`
    //    and then `font_scaling = height / units_per_em`, then
    //        scale_in_pixels = pixels_per_point * pt_size * font_scaling
    //        assert!(scale_in_pixels > 0.0);
    //    If both the hhea and OS/2 metrics are zero — which happens
    //    on a handful of Windows symbol / dingbat fonts (`Marlett`,
    //    `MS Outlook`, certain corrupt `%LOCALAPPDATA%\…\Fonts` files)
    //    — `height` collapses to 0, `scale_in_pixels` to 0, and the
    //    assert panics. With `panic = "abort"` that becomes
    //    `STATUS_STACK_BUFFER_OVERRUN`. Reject those fonts here so
    //    the picker silently falls back to Proportional instead.
    let ascender = face.ascender();
    let descender = face.descender();
    if ascender == 0 && descender == 0 {
        return false;
    }

    // 4. Must have at least one glyph and a cmap that maps common
    //    ASCII codepoints. A font where every `glyph_index(c)`
    //    returns `None` would render every char as the replacement
    //    box — a worse-than-useless picker entry — and indicates
    //    something is structurally wrong even when the directory
    //    tables look intact.
    if face.number_of_glyphs() == 0 {
        return false;
    }
    let has_basic_glyphs = ['A', 'a', '0', '.', ' ']
        .iter()
        .any(|c| face.glyph_index(*c).is_some());
    if !has_basic_glyphs {
        return false;
    }

    // 5. Glyph outlines must be present. ab_glyph reads from `glyf`
    //    (TrueType) or `CFF`/`CFF2` (PostScript). Bitmap-only fonts
    //    (`sbix`/`CBDT`-only, e.g. some emoji fonts on macOS) are
    //    handled by epaint as zero-sized rects, which we tolerate,
    //    but a font with NEITHER outlines NOR bitmap data is a
    //    malformed file the picker shouldn't expose.
    let tables = face.tables();
    let has_outlines = tables.glyf.is_some()
        || tables.cff.is_some()
        || tables.cff2.is_some()
        || tables.sbix.is_some()
        || tables.cbdt.is_some();
    if !has_outlines {
        return false;
    }

    true
}

/// In-process record of which custom font families have already been
/// loaded into egui via `ctx.set_fonts(...)`. Egui's font definitions
/// are owned wholesale by the context, so we keep a parallel copy
/// here that we mutate and re-ship whenever a new family is requested.
#[derive(Default)]
struct Loaded {
    families: BTreeSet<String>,
    /// Families we've already attempted to load and rejected (e.g.
    /// the TTF failed `validate_font_for_egui`). Caching these means
    /// `ensure_font_loaded` short-circuits on subsequent calls
    /// instead of re-reading and re-rejecting the same broken file
    /// every frame from the inspector / canvas painter.
    rejected: BTreeSet<String>,
    /// Cached FontDefinitions to extend. We keep the most recent
    /// version we shipped to egui so the "add another family" path
    /// is just `defs.font_data.insert + defs.families.insert` and
    /// one `ctx.set_fonts(defs.clone())` call.
    defs: Option<egui::FontDefinitions>,
}

fn loaded_state() -> &'static Mutex<Loaded> {
    static STATE: OnceLock<Mutex<Loaded>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Loaded::default()))
}

/// Register the built-in `egui::FontDefinitions::default()` set with
/// the context so subsequent on-demand additions can extend it. Idempotent
/// — call this once at app startup before the first `ensure_font_loaded`.
pub fn install_default_definitions(ctx: &egui::Context) {
    let mut state = loaded_state().lock().unwrap();
    if state.defs.is_none() {
        let defs = egui::FontDefinitions::default();
        ctx.set_fonts(defs.clone());
        state.defs = Some(defs);
    }
}

/// Lazily ensure a custom font family identified by `family_name` is
/// available in `ctx`. After this call, paint code can reference it
/// via `egui::FontFamily::Name(family_name.into())`. Returns `true`
/// when the family resolved successfully (or was already loaded),
/// `false` when no matching TTF was found, when its bytes failed
/// validation, or when `available_families()` hasn't completed its
/// background scan yet — in all of those cases callers fall back to
/// the bundled `Proportional` family.
///
/// The function is **idempotent and cheap** for already-loaded (or
/// already-rejected) families — it short-circuits on a `BTreeSet`
/// lookup without touching disk or egui.
pub fn ensure_font_loaded(ctx: &egui::Context, family_name: &str) -> bool {
    if family_name.is_empty() {
        return false;
    }
    {
        let state = loaded_state().lock().unwrap();
        if state.families.contains(family_name) {
            return true;
        }
        if state.rejected.contains(family_name) {
            return false;
        }
    }

    // Find the on-disk path for this family. While the background
    // scan is still running this returns an empty slice and we bail
    // out quickly — the picker will retry on the next frame.
    let entry = available_families()
        .iter()
        .find(|f| f.family.eq_ignore_ascii_case(family_name))
        .cloned();
    let Some(entry) = entry else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&entry.path) else {
        // Mark as rejected so we don't retry hot.
        let mut state = loaded_state().lock().unwrap();
        state.rejected.insert(family_name.to_string());
        return false;
    };

    // ── Strict, non-panicking pre-flight ──
    //
    // Under `panic = "abort"` (set in `[profile.release]` for client
    // distribution hardening) `std::panic::catch_unwind` is silently
    // a no-op, so the only way to keep a malformed system font from
    // crashing the editor is to refuse it BEFORE handing the bytes
    // to egui. `validate_font_for_egui` walks every metric epaint
    // will eventually consult using only total accessors from
    // ttf-parser; anything that survives is safe for the next paint.
    if !validate_font_for_egui(&bytes) {
        let mut state = loaded_state().lock().unwrap();
        state.rejected.insert(family_name.to_string());
        tracing::warn!(
            font = %family_name,
            path = %entry.path.display(),
            "rejected system font: failed pre-flight validation \
             (corrupt tables, zero metrics, or no outline data)"
        );
        return false;
    }

    let mut state = loaded_state().lock().unwrap();
    let mut defs = state
        .defs
        .clone()
        .unwrap_or_else(egui::FontDefinitions::default);

    let key = format!("user_{}", family_name);
    defs.font_data
        .insert(key.clone(), egui::FontData::from_owned(bytes));

    // Register the font under its own `FontFamily::Name(...)` ONLY.
    //
    // Earlier versions also pushed `key` onto the bundled
    // `FontFamily::Proportional` chain "as a missing-glyph fallback",
    // but that wired every UI label in the editor (status bar, file
    // dialogs, the font picker dropdown itself) through whatever
    // system font the user just clicked. A subtly-broken font then
    // panicked epaint deep inside text layout — which under
    // `panic = "abort"` aborts the whole process via `__fastfail`
    // and surfaces as `STATUS_STACK_BUFFER_OVERRUN` (`0xC0000409`).
    //
    // Strict scoping keeps the blast radius to overlays that
    // explicitly opted into `FontFamily::Name(<family>)`; the rest
    // of the UI continues to render in the bundled defaults even if
    // a system font sneaks past `validate_font_for_egui`.
    let fam = egui::FontFamily::Name(family_name.to_string().into());
    let entry_list = defs.families.entry(fam).or_default();
    if !entry_list.contains(&key) {
        entry_list.insert(0, key.clone());
    }

    ctx.set_fonts(defs.clone());
    state.defs = Some(defs);
    state.families.insert(family_name.to_string());
    true
}
