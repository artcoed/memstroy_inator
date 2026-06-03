//! Logical-name to TTF-path resolution.
//!
//! FFmpeg's `drawtext` filter wants either fontconfig (rare in headless
//! sandboxes / portable builds) or an explicit `fontfile=`. We pick the
//! latter and resolve a logical family name to a concrete file:
//!
//! 1. Honour the `MEMSTROY_FONT_DIR` environment variable.
//! 2. Walk a list of OS-standard font directories (Linux/macOS/Windows).
//! 3. Best-effort substring match on the family name and weight.
//!
//! Result is memoised so later text overlays don't re-walk the FS.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Resolve `family` (e.g. "DejaVuSans") to a TTF path, optionally
/// preferring a bold variant. Returns `None` when nothing matched.
pub fn find_font(family: &str, bold: bool) -> Option<PathBuf> {
    let cache = font_cache();
    // Family substring match, case-insensitive, ASCII alnum only.
    let fam_norm = normalise(family);
    let prefer_bold = bold;

    // Two-pass: first the preferred weight, then any.
    for want_bold in [prefer_bold, !prefer_bold] {
        if let Some(p) = cache.iter().find(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let stem_norm = normalise(stem);
            let is_bold = stem_norm.contains("bold");
            stem_norm.contains(&fam_norm) && is_bold == want_bold && !stem_norm.contains("italic")
        }) {
            return Some(p.clone());
        }
    }
    cache
        .iter()
        .find(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            normalise(stem).contains(&fam_norm)
        })
        .cloned()
}

/// Public access to the discovered TTF/OTF file list.
///
/// Used by the GUI's `system_fonts` module to enumerate every font on
/// disk and surface them in the font picker — without re-walking the
/// filesystem (the cache is populated once on first call and shared
/// across the workspace).
pub fn discovered_font_paths() -> &'static [PathBuf] {
    font_cache()
}

fn font_cache() -> &'static [PathBuf] {
    static CACHE: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(scan_font_dirs)
}

fn scan_font_dirs() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("MEMSTROY_FONT_DIR") {
        roots.push(PathBuf::from(d));
    }
    for s in [
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        "/Library/Fonts",
        "/System/Library/Fonts",
        "C:/Windows/Fonts",
    ] {
        roots.push(PathBuf::from(s));
    }
    if let Some(home) = dirs_home() {
        roots.push(home.join(".fonts"));
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join("Library/Fonts"));
    }
    // Windows 11 stores per-user fonts under
    // `%LOCALAPPDATA%\Microsoft\Windows\Fonts` — that's where the
    // "Install for me only" path of the Settings → Personalization
    // → Fonts page drops a TTF, and is exactly the directory the
    // user complains about ("шрифты не подгружаются системные, на
    // Win 11 отображается только 2"). The legacy `C:/Windows/Fonts`
    // root above only catches the `installforAllUsers` path.
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }

    let mut out = Vec::new();
    for r in roots {
        if !r.exists() {
            continue;
        }
        walk_ttfs(&r, &mut out);
    }
    out
}

fn walk_ttfs(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_ttfs(&path, out);
        } else if matches!(
            path.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase())
                .as_deref(),
            Some("ttf" | "otf")
        ) {
            out.push(path);
        }
    }
}

fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
