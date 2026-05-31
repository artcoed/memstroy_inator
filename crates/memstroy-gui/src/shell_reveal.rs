//! Open the system file manager and highlight a path.

use std::path::{Path, PathBuf};

/// Reveal `path` in the OS file manager, selecting the file when it exists.
pub fn reveal_path_in_file_manager(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(crate::i18n::t("Path not found").to_string());
    }

    if path.is_file() {
        reveal_existing_file(path)
    } else if let Ok(abs) = resolve_existing_file(path) {
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

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Resolve to an absolute path; prefer canonical paths but fall back
/// when the OS refuses (OneDrive placeholders, long paths, …).
fn resolve_existing_file(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io_err)?.join(path)
    };
    if !abs.exists() {
        return Err(format!(
            "{}: {}",
            crate::i18n::t("Path not found"),
            path.display()
        ));
    }
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return Ok(strip_verbatim_prefix(&canon));
    }
    Ok(strip_verbatim_prefix(&abs))
}

#[cfg(target_os = "windows")]
fn reveal_existing_file(path: &Path) -> Result<(), String> {
    let abs = resolve_existing_file(path)?;
    if reveal_via_shell_api(&abs).is_ok() {
        return Ok(());
    }
    explorer_select(&abs)
}

#[cfg(target_os = "windows")]
fn reveal_via_shell_api(path: &Path) -> Result<(), String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: Win32 shell COM APIs; `wide` is null-terminated UTF-16.
    unsafe {
        #[link(name = "ole32")]
        extern "system" {
            fn CoInitializeEx(pvReserved: *const c_void, dwCoInit: u32) -> i32;
        }
        #[link(name = "shell32")]
        extern "system" {
            fn ILCreateFromPathW(pszPath: *const u16) -> *mut c_void;
            fn ILClone(pidl: *const c_void) -> *mut c_void;
            fn ILFindLastID(pidl: *const c_void) -> *mut c_void;
            fn ILRemoveLastID(pidl: *mut c_void) -> i32;
            fn ILFree(pidl: *const c_void);
            fn SHOpenFolderAndSelectItems(
                pidlFolder: *const c_void,
                cidl: u32,
                apidl: *const *const c_void,
                dwFlags: u32,
            ) -> i32;
        }

        let _ = CoInitializeEx(std::ptr::null(), 0x2); // COINIT_APARTMENTTHREADED

        let pidl_full = ILCreateFromPathW(wide.as_ptr());
        if pidl_full.is_null() {
            return Err(crate::i18n::t("Could not open folder").to_string());
        }

        let pidl_item = ILFindLastID(pidl_full);
        let pidl_folder = ILClone(pidl_full);
        if pidl_folder.is_null() {
            ILFree(pidl_full);
            return Err(crate::i18n::t("Could not open folder").to_string());
        }

        let _ = ILRemoveLastID(pidl_folder);

        let apidl = [pidl_item as *const c_void];
        let hr = SHOpenFolderAndSelectItems(pidl_folder, 1, apidl.as_ptr(), 0);

        ILFree(pidl_folder);
        ILFree(pidl_full);

        if hr < 0 {
            return Err(format!(
                "{}: Shell error 0x{:08X}",
                crate::i18n::t("Could not open folder"),
                hr as u32
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn explorer_select(path: &Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();
    let arg = format!("/select,\"{path_str}\"");
    std::process::Command::new("explorer.exe")
        .arg(arg)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_directory(path: &Path) -> Result<(), String> {
    let abs = if path.is_dir() {
        path.to_path_buf()
    } else if let Ok(p) = resolve_existing_file(path) {
        p
    } else {
        path.to_path_buf()
    };
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
