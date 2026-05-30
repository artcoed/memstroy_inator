//! Open the system file manager and highlight a path.

use std::path::{Path, PathBuf};

/// Reveal `path` in the OS file manager, selecting the file when it exists.
pub fn reveal_path_in_file_manager(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(crate::i18n::t("Path not found").to_string());
    }

    if path.is_file() {
        reveal_existing_file(path)
    } else if let Ok(abs) = resolve_path(path) {
        if abs.is_file() {
            reveal_existing_file(&abs)
        } else if abs.is_dir() {
            open_directory(&abs)
        } else if let Some(parent) = abs.parent().filter(|p| p.is_dir()) {
            open_directory(parent)
        } else {
            Err(format!(
                "{}: {}",
                crate::i18n::t("Path not found"),
                path.display()
            ))
        }
    } else if path.is_dir() {
        open_directory(path)
    } else if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
        open_directory(parent)
    } else {
        Err(format!(
            "{}: {}",
            crate::i18n::t("Path not found"),
            path.display()
        ))
    }
}

fn io_err(e: std::io::Error) -> String {
    format!("{}: {e}", crate::i18n::t("Could not open folder"))
}

/// Resolve `path` to an absolute on-disk path and verify the file exists.
fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(io_err)?
            .join(path)
    };
    let abs = std::fs::canonicalize(&abs).map_err(io_err)?;
    Ok(strip_verbatim_prefix(&abs))
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(target_os = "windows")]
fn reveal_existing_file(path: &Path) -> Result<(), String> {
    let abs = resolve_path(path)?;
    if reveal_via_shell_com(&abs).is_ok() {
        return Ok(());
    }
    explorer_select(&abs)
}

#[cfg(target_os = "windows")]
fn reveal_via_shell_com(path: &Path) -> Result<(), String> {
    let path_literal = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$p='{path_literal}'; \
         if (-not (Test-Path -LiteralPath $p)) {{ exit 1 }}; \
         $shell = New-Object -ComObject Shell.Application; \
         $folder = Split-Path -LiteralPath $p; \
         $file = Split-Path -LiteralPath $p -Leaf; \
         $item = $shell.NameSpace($folder).ParseName($file); \
         if ($null -eq $item) {{ exit 2 }}; \
         $item.InvokeVerb('select')"
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .map_err(io_err)?;
    if status.success() {
        Ok(())
    } else {
        Err(io_err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Shell select exited with {status}"),
        )))
    }
}

#[cfg(target_os = "windows")]
fn explorer_select(path: &Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();
    let arg = format!("/select,{path_str}");
    std::process::Command::new("explorer.exe")
        .arg(arg)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_directory(path: &Path) -> Result<(), String> {
    let abs = resolve_path(path).unwrap_or_else(|_| path.to_path_buf());
    std::process::Command::new("explorer.exe")
        .arg(abs)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn reveal_existing_file(path: &Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_directory(path: &Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_existing_file(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        open_directory(parent)
    } else {
        Err(crate::i18n::t("Path not found").to_string())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_directory(path: &Path) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn reveal_existing_file(_path: &Path) -> Result<(), String> {
    Err(crate::i18n::t("Could not open folder").to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn open_directory(_path: &Path) -> Result<(), String> {
    Err(crate::i18n::t("Could not open folder").to_string())
}
