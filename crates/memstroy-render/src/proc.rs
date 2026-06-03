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
//!
//! This module also hosts a small `probe_has_audio_stream()` helper
//! used by the renderer to skip `AudioTrack`s whose source file
//! actually contains no audio. The GUI auto-pushes an audio track for
//! every dropped actor (so the embedded soundtrack shows up on the
//! audio lanes) — but when the source clip is silent (GIF→MP4
//! conversion, screen recordings without mic, etc.) the renderer's
//! `[idx:a]aresample=…` reference resolves to nothing, ffmpeg aborts
//! filter-graph configuration with "Stream specifier ':a' matches no
//! streams", and BOTH encoder threads die in lockstep:
//!
//!   * `vost#0:0/libx264 Task finished with error code: -22 (Invalid
//!     argument)`,
//!   * `aost#0:1/aac Could not open encoder before EOF`,
//!
//! producing a `frame=0 Lsize=0KiB` output. The probe ffprobes the
//! source once at plan-build time and lets `emit_audio` bail out
//! gracefully when no audio stream is present.

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

// ─── ffprobe-based audio stream detection ─────────────────────────────

use std::path::Path;

/// Locate ffprobe alongside the configured ffmpeg binary, falling back
/// to a bare `ffprobe` on `PATH`. Mirrors the lookup pattern several
/// other crates use (frame_snapshot, video_cache, state) so an
/// `MEMSTROY_FFMPEG=/opt/ffmpeg/bin/ffmpeg` override automatically
/// picks up `/opt/ffmpeg/bin/ffprobe` next to it.
fn ffprobe_binary() -> std::path::PathBuf {
    crate::runner::ffprobe_binary()
}

/// Return `true` when `path` is confirmed to contain at least one
/// audio stream, OR when the probe couldn't run (ffprobe missing /
/// permission denied / file unreadable). The pessimistic `false`
/// branch fires ONLY when ffprobe successfully ran AND reported zero
/// audio streams — that's the precise failure mode that triggers
/// "Stream specifier ':a' matches no streams" in the filter graph.
///
/// We default to `true` on probe failure so that:
///   * builds without ffprobe on PATH (CI, sandboxes) keep working
///     verbatim — the audio track is included and ffmpeg will surface
///     a clearer error if the stream really is missing;
///   * a transient ffprobe glitch never silently drops a track the
///     user actually wanted.
pub fn probe_has_audio_stream(path: &Path) -> bool {
    let bin = ffprobe_binary();
    let mut cmd = std::process::Command::new(&bin);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a",
        "-show_entries",
        "stream=codec_type",
        "-of",
        "csv=p=0",
    ])
    .arg(path)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null());
    hide_console_std(&mut cmd);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            // ffprobe prints the literal token `audio` once per
            // matched stream; an empty stdout means the file has no
            // audio streams (the "silent video" case the GUI's
            // auto-AudioTrack logic doesn't filter for).
            let stdout = String::from_utf8_lossy(&out.stdout);
            ffprobe_stdout_has_audio_stream(&stdout)
        }
        // Probe failure (binary missing, file not found, etc.) — be
        // optimistic. Render-time errors are still surfaced through
        // ffmpeg's own stderr if the assumption was wrong.
        _ => true,
    }
}

fn ffprobe_stdout_has_audio_stream(stdout: &str) -> bool {
    stdout.lines().any(|line| {
        line.split(',')
            .any(|field| field.trim().eq_ignore_ascii_case("audio"))
    })
}

#[cfg(test)]
mod tests {
    use super::ffprobe_stdout_has_audio_stream;

    #[test]
    fn ffprobe_csv_audio_with_trailing_field_counts_as_audio() {
        assert!(ffprobe_stdout_has_audio_stream("audio,\n"));
    }

    #[test]
    fn ffprobe_empty_audio_selection_counts_as_silent() {
        assert!(!ffprobe_stdout_has_audio_stream(""));
    }
}
