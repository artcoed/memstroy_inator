//! Cross-platform child-process spawn helpers.
//!
//! On Windows, spawning `ffmpeg.exe` or `ffprobe.exe` from a GUI
//! application pops up a transient console window for the lifetime of
//! the child — every time we render a scene, generate a thumbnail, or
//! probe a clip's duration the user sees a black `cmd`-style flash. The
//! fix is to OR `CREATE_NO_WINDOW` (`0x0800_0000`) into the child's
//! creation flags before `spawn()` / `output()`, which keeps the child
//! console-less while leaving stdout/stderr piping intact.
//!
//! Both `std::process::Command` and `tokio::process::Command` expose
//! `creation_flags()` on Windows (via `CommandExt`); on other platforms
//! the helpers below are no-ops so callers don't need their own
//! `#[cfg]` blocks. Use them for every external-process spawn the
//! editor performs.

/// Hide the spawned child's console window on Windows. No-op on other
/// platforms so callers can use this unconditionally.
#[cfg(windows)]
pub fn hide_console_std(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW from winbase.h. Avoids pulling in `winapi` /
    // `windows-sys` just for one constant.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub fn hide_console_std(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

/// Same as [`hide_console_std`] but for the tokio variant. Tokio's
/// `Command` re-implements `creation_flags` via its own `CommandExt`
/// equivalent on Windows.
#[cfg(windows)]
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd
}
