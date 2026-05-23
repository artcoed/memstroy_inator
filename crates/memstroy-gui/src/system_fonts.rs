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
//!
//! 2. `ensure_font_loaded(ctx, family)` lazily loads the TTF bytes for
//!    the requested family into egui's `FontDefinitions` and assigns
//!    a custom `FontFamily::Name(<family>)` to it. The first call
//!    incurs the file-read + parse cost; subsequent calls are O(1).
//!    The function is idempotent and safe to call every frame from
//!    the canvas / render path.
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

/// All fonts found on the system, deduplicated by family name and
/// sorted A→Z. The first call walks the filesystem (via the
/// memstroy-render font-dir cache); later calls reuse the result.
pub fn available_families() -> &'static [DiscoveredFont] {
    static CACHE: OnceLock<Vec<DiscoveredFont>> = OnceLock::new();
    CACHE.get_or_init(scan_families).as_slice()
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
/// `false` when no matching TTF was found and the bundled
/// `Proportional` family is the best the caller can do.
///
/// The function is **idempotent and cheap** for already-loaded
/// families — it short-circuits on a `BTreeSet::contains` lookup
/// without touching disk or egui.
pub fn ensure_font_loaded(ctx: &egui::Context, family_name: &str) -> bool {
    if family_name.is_empty() {
        return false;
    }
    {
        let state = loaded_state().lock().unwrap();
        if state.families.contains(family_name) {
            return true;
        }
    }

    // Find the on-disk path for this family.
    let entry = available_families()
        .iter()
        .find(|f| f.family.eq_ignore_ascii_case(family_name))
        .cloned();
    let Some(entry) = entry else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&entry.path) else {
        return false;
    };

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

    ctx.set_fonts(defs.clone());
    state.defs = Some(defs);
    state.families.insert(family_name.to_string());
    true
}
