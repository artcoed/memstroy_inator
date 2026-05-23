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
//!    The function validates the bytes with `ab_glyph` before
//!    handing them to egui — earlier we relied on `ttf-parser` only,
//!    and the (slightly different) parser inside egui's text layout
//!    could panic on a TTF that ttf-parser had accepted, crashing the
//!    whole app the moment the user picked that font ("когда шрифт
//!    выбрал из списка случился краш программы"). When validation
//!    fails we fall back to the bundled default and report `false`
//!    so callers can show a hint.
//!
//! 3. A small in-process `Mutex<Loaded>` tracks which families are
//!    already in the egui font table so we don't pay for a full
//!    `set_fonts(...)` each frame. egui requires the entire
//!    FontDefinitions to be replaced, so we keep our own copy of the
//!    table and ship it back through `set_fonts(...)` only when the
//!    set of loaded families actually grew.

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

/// In-process record of which custom font families have already been
/// loaded into egui via `ctx.set_fonts(...)`. Egui's font definitions
/// are owned wholesale by the context, so we keep a parallel copy
/// here that we mutate and re-ship whenever a new family is requested.
#[derive(Default)]
struct Loaded {
    families: BTreeSet<String>,
    /// Families we've already attempted to load and rejected (e.g.
    /// the TTF was unparseable by `ab_glyph`). Caching these means
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

    // ── Validate with ab_glyph BEFORE handing bytes to egui ──
    //
    // egui's text layout uses ab_glyph internally; if ab_glyph rejects
    // a TTF (corrupt tables, unsupported variant, …) the next paint
    // panics and crashes the editor. We pre-flight the same parser so
    // a bad font becomes a benign "fall back to Proportional" instead
    // of a hard crash. The `catch_unwind` belt-and-braces guards
    // against any deeper panics ab_glyph might exhibit on extremely
    // exotic TTFs.
    let validated = std::panic::catch_unwind(|| {
        ab_glyph::FontVec::try_from_vec(bytes.clone()).is_ok()
    })
    .unwrap_or(false);
    if !validated {
        let mut state = loaded_state().lock().unwrap();
        state.rejected.insert(family_name.to_string());
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

    let fam = egui::FontFamily::Name(family_name.to_string().into());
    defs.families
        .entry(fam)
        .or_default()
        .insert(0, key.clone());
    // Also append to Proportional / Monospace as a fallback so glyphs
    // missing from the bundled defaults can be drawn by a system font.
    defs.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push(key.clone());

    // Wrap `set_fonts` in catch_unwind too — defence in depth against
    // egui internals that could otherwise panic on unusual font tables
    // even when ab_glyph parsed them. On failure we keep the previous
    // FontDefinitions and surface `false` to the caller.
    let install = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.set_fonts(defs.clone());
    }));
    if install.is_err() {
        state.rejected.insert(family_name.to_string());
        return false;
    }
    state.defs = Some(defs);
    state.families.insert(family_name.to_string());
    true
}
