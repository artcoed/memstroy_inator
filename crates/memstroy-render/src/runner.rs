use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use memstroy_core::Scene;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::plan::{build_plan, FfmpegPlan};

/// Locate ffmpeg: env var `MEMSTROY_FFMPEG`, then PATH.
pub fn ffmpeg_binary() -> PathBuf {
    if let Ok(p) = std::env::var("MEMSTROY_FFMPEG") {
        return PathBuf::from(p);
    }
    PathBuf::from("ffmpeg")
}

/// Render `scene` into `output_path`. Streams FFmpeg's stderr line by
/// line through the optional progress callback so the GUI can show a
/// progress bar.
pub async fn render_scene<F>(
    scene: &Scene,
    assets_root: &Path,
    output_path: &Path,
    mut on_log: F,
) -> Result<()>
where
    F: FnMut(&str),
{
    let plan = build_plan(scene, output_path, assets_root)?;
    run_plan(&plan, &mut on_log).await
}

/// Render a single still frame at time `t` to `out_png`. Useful for
/// the GUI scrubber. Implementation: render the full scene to a
/// pipe-friendly intermediate is overkill for a preview; we instead
/// build a one-frame plan by overriding duration to a tiny window
/// and asking FFmpeg for one frame.
pub async fn render_preview_frame(
    scene: &Scene,
    assets_root: &Path,
    t: f32,
    out_png: &Path,
) -> Result<()> {
    let mut clipped = scene.clone();
    // Render a 1-frame window centred on t. Outputs with -frames:v 1.
    let frame_dur = 1.0 / scene.output.fps as f32;
    clipped.output.duration = (t + frame_dur).max(frame_dur);
    let plan = build_plan(&clipped, out_png, assets_root)?;

    let mut args = plan.to_args();
    // Replace x264 + duration with single-frame PNG output.
    // We strip codec/duration/pixfmt options and add -frames:v 1.
    let drops = ["-c:v", "-preset", "-crf", "-pix_fmt", "-t"];
    let mut cleaned: Vec<String> = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for a in args.drain(..) {
        if skip_next { skip_next = false; continue; }
        if drops.iter().any(|d| *d == a) { skip_next = true; continue; }
        cleaned.push(a);
    }
    // The output path was the last arg in the plan. Replace it with
    // -ss t -frames:v 1 -update 1 <out_png>.
    let output = cleaned.pop().unwrap();
    cleaned.push("-ss".into());
    cleaned.push(format!("{:.4}", t));
    cleaned.push("-frames:v".into());
    cleaned.push("1".into());
    cleaned.push("-update".into());
    cleaned.push("1".into());
    cleaned.push(output);

    let mut sink = |_: &str| {};
    let result = run_args(&cleaned, &mut sink).await;
    // Same cleanup story as `run_plan`: remove the mask alpha PNGs
    // (and any other auxiliary assets the builder produced) so a
    // long preview session doesn't leak temp files.
    for p in &plan.cleanup_paths {
        if let Err(e) = std::fs::remove_file(p) {
            warn!(path = %p.display(), error = %e, "failed to remove mask asset");
        }
    }
    result
}

async fn run_plan<F: FnMut(&str)>(plan: &FfmpegPlan, on_log: &mut F) -> Result<()> {
    let args = plan.to_args();
    let result = run_args(&args, on_log).await;
    // Best-effort cleanup of auxiliary files (alpha PNGs for masks
    // etc.) regardless of whether the run succeeded — leaking these
    // under `std::env::temp_dir()` would slowly accrete on busy
    // editors. We log a warning but don't propagate cleanup errors.
    for p in &plan.cleanup_paths {
        if let Err(e) = std::fs::remove_file(p) {
            warn!(path = %p.display(), error = %e, "failed to remove mask asset");
        }
    }
    result
}

async fn run_args<F: FnMut(&str)>(args: &[String], on_log: &mut F) -> Result<()> {
    let bin = ffmpeg_binary();
    info!(?bin, args_len = args.len(), "spawning ffmpeg");

    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Suppress the transient cmd.exe window that would otherwise pop
    // up every render on Windows. No-op on other platforms.
    crate::proc::hide_console_tokio(&mut cmd);

    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;

    // Keep a sliding window of the last few stderr lines so the error
    // we surface includes ffmpeg's actual complaint instead of just an
    // opaque exit code. The previous `ExitStatus(429)`-style errors
    // gave the user nothing to act on; this tail is what makes the
    // failure self-diagnosing.
    const TAIL_LINES: usize = 8;
    let mut tail: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(TAIL_LINES);

    let stderr = child.stderr.take().unwrap();
    // ffmpeg writes its periodic progress line ("frame=  120 fps=…
    // time=00:00:04.00 bitrate=…") and overwrites it in-place with a
    // bare carriage return — only emitting `\n` between phases. The
    // previous `BufReader::lines()` reader only fired on `\n`, which
    // meant the GUI saw zero progress events for the entire encode
    // and the bar jumped 0% → 100% at the end. We instead read raw
    // bytes and split on EITHER `\r` or `\n`, which surfaces every
    // progress refresh as its own log line.
    read_stderr_progress(stderr, |line| {
        on_log(line);
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
    })
    .await?;

    let status = child.wait().await?;
    if !status.success() {
        warn!(code = status.code(), "ffmpeg exited with non-zero status");
        // The `ExitStatus` Debug impl on Unix prints the raw wait
        // status (e.g. "ExitStatus(429)") which is meaningless to
        // most users. Prefer the cooked exit code or signal name when
        // available, falling back to the raw form only if neither
        // resolves.
        let status_str = match status.code() {
            Some(code) => format!("exit code {code}"),
            None => format!("{:?}", status),
        };
        let tail_str = if tail.is_empty() {
            String::new()
        } else {
            format!(" (last log: {})", tail.iter().cloned().collect::<Vec<_>>().join(" | "))
        };
        return Err(anyhow!("ffmpeg failed: {}{}", status_str, tail_str));
    }
    Ok(())
}

/// Stream ffmpeg's stderr, splitting on EITHER `\r` or `\n`.
///
/// ffmpeg's progress line ("frame=  120 fps= 30 q=28.0 size=… time=…
/// bitrate=… speed=…") is repeatedly overwritten using a bare
/// carriage return — never `\n` until the encode actually transitions
/// to a new phase. This means a `BufReader::lines()` reader buffers
/// the entire encode into a single line and `on_log` is invoked
/// exactly once at the end, leaving the GUI's progress bar stuck
/// at 0%. Splitting on both `\r` and `\n` surfaces every refresh
/// as its own logical "line", letting the GUI parse the embedded
/// `time=` token and update the bar at sub-second cadence.
///
/// `R: AsyncRead + Unpin` keeps the function generic over `tokio`'s
/// `ChildStderr` and `Stdin`/test streams without committing to a
/// concrete type.
async fn read_stderr_progress<R, F>(mut reader: R, mut emit: F) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(&str),
{
    let mut buf = [0u8; 1024];
    let mut acc: Vec<u8> = Vec::with_capacity(2048);
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            // Flush any trailing partial line so the very last
            // progress refresh of a clean encode is still emitted.
            if !acc.is_empty() {
                let s = String::from_utf8_lossy(&acc);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    emit(trimmed);
                }
                acc.clear();
            }
            return Ok(());
        }
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if !acc.is_empty() {
                    let s = String::from_utf8_lossy(&acc);
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        emit(trimmed);
                    }
                    acc.clear();
                }
            } else {
                acc.push(b);
            }
        }
    }
}
