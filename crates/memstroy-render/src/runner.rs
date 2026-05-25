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

/// Render `scene` into `output_path`. Switches between the CPU
/// compositor (default) and the legacy FFmpeg filtergraph path
/// based on the `MEMSTROY_RENDER_BACKEND` env var.
///
/// The CPU compositor mirrors the canvas snapshot rasterizer pixel
/// for pixel — every transform (world→output, camera, modifiers,
/// chromakey, color correction) uses the same math `frame_snapshot`
/// uses on the canvas. This is the only way to guarantee that what
/// the user sees in the editor matches the rendered MP4.
///
/// `on_log` is called for every diagnostic line (or progress update)
/// produced by the renderer in real time. The GUI converts these
/// into status-bar / progress-bar updates. The closure must be
/// `FnMut(&str) + Send` because the CPU path runs on a blocking
/// worker thread; we marshal the progress events back to the caller
/// via a `std::sync::mpsc` channel and replay them as `&str` calls
/// from the async context.
///
/// `MEMSTROY_RENDER_BACKEND=ffmpeg` falls back to the legacy
/// filtergraph path for users who hit a regression in the CPU path.
pub async fn render_scene<F>(
    scene: &Scene,
    assets_root: &Path,
    output_path: &Path,
    mut on_log: F,
) -> Result<()>
where
    F: FnMut(&str) + Send,
{
    if use_legacy_filtergraph_backend() {
        let plan = build_plan(scene, output_path, assets_root)?;
        return run_plan(&plan, &mut on_log).await;
    }

    // CPU path. We stream progress back to the caller in real time
    // through a `std::sync::mpsc` channel — the worker thread sends
    // events as soon as they happen, the async side polls them at a
    // tight cadence so the GUI sees frame-by-frame progress instead
    // of "0% for 5 minutes, then jump to 100%". Without the channel
    // the user reported "бесконечный 0%" because we only flushed
    // log events AFTER the worker returned.
    on_log("CPU compositor: preparing scene...");

    let scene_owned = scene.clone();
    let assets_root_owned = assets_root.to_path_buf();
    let output_path_owned = output_path.to_path_buf();
    let (prog_tx, prog_rx) = std::sync::mpsc::channel::<crate::compositor::Progress>();

    let join_handle = tokio::task::spawn_blocking(move || {
        crate::compositor::render_scene_cpu(
            &scene_owned,
            &assets_root_owned,
            &output_path_owned,
            |progress| {
                let _ = prog_tx.send(progress);
            },
        )
    });

    // Drain the progress channel while the worker runs. We poll on a
    // 100 ms cadence so the async runtime stays responsive but we
    // don't burn CPU. The channel is closed (i.e. `recv` returns
    // `Disconnected`) once the worker has completed and its
    // `prog_tx` has dropped — at which point we await the join handle
    // for the actual `Result`.
    loop {
        match prog_rx.try_recv() {
            Ok(p) => on_log(&p.to_log_line()),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
    // Worker is done — drain anything left in the channel before we
    // surface its result.
    while let Ok(p) = prog_rx.try_recv() {
        on_log(&p.to_log_line());
    }
    join_handle.await.context("CPU compositor task panicked")?
}

fn use_legacy_filtergraph_backend() -> bool {
    matches!(
        std::env::var("MEMSTROY_RENDER_BACKEND")
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Ok("ffmpeg") | Ok("filtergraph") | Ok("legacy")
    )
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
    if use_legacy_filtergraph_backend() {
        return render_preview_frame_filtergraph(scene, assets_root, t, out_png).await;
    }

    // CPU path — runs on a blocking worker so async callers don't
    // stall their runtime.
    let scene = scene.clone();
    let assets_root = assets_root.to_path_buf();
    let out_png = out_png.to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::compositor::render_preview_frame_cpu(&scene, &assets_root, t, &out_png)
    })
    .await
    .context("CPU preview task panicked")?
}

async fn render_preview_frame_filtergraph(
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
    if result.is_err() {
        // Surface the filter complex string into the structured log so
        // a developer triaging a user's bug report can reproduce the
        // exact graph the renderer fed to ffmpeg. Capped at a generous
        // length so a pathological scene doesn't blow up the log file.
        const MAX_DUMP: usize = 8192;
        let fc: String = if plan.filter_complex.chars().count() > MAX_DUMP {
            let mut s: String = plan.filter_complex.chars().take(MAX_DUMP).collect();
            s.push_str("…<truncated>");
            s
        } else {
            plan.filter_complex.clone()
        };
        tracing::error!(
            inputs = plan.inputs.len(),
            map_video = %plan.map_video,
            map_audio = ?plan.map_audio,
            filter_complex = %fc,
            "ffmpeg failed; dumping filter graph for diagnostics",
        );
    }
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

    // Keep BOTH a head and a sliding tail of stderr so the error we
    // surface includes the actual ffmpeg complaint regardless of
    // whether it appeared at filter-graph init time (HEAD) or near the
    // end of the encode (TAIL).
    //
    // Filter-graph configuration errors — "Stream specifier ':a'
    // matches no streams", "Output dimension wrong", missing input
    // streams — print at the BEGINNING of stderr, well before any
    // progress line. The previous tail-only buffer dropped these once
    // ffmpeg's progress output started rolling, so the user got an
    // opaque "exit code -22" with no actionable detail.
    //
    // The tail is still useful for mid-encode failures (encoder error
    // codes, muxer warnings, the "Conversion failed!" footer). Keeping
    // both at modest sizes covers every failure mode without bloating
    // the error message.
    const HEAD_LINES: usize = 12;
    const TAIL_LINES: usize = 24;
    let mut head: Vec<String> = Vec::with_capacity(HEAD_LINES);
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
        if head.len() < HEAD_LINES {
            head.push(line.to_string());
        }
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
        // Compose a diagnostic summary that includes BOTH the head
        // and tail of stderr. Filter-graph init failures (the most
        // common cause of the libx264 -22 / AAC EOF double-failure)
        // print at the beginning; encode-phase errors print near the
        // end. Prioritising lines that look like ffmpeg errors makes
        // the message useful even when the buffers contain a lot of
        // routine info / warning chatter.
        let log_str = format_diagnostic_log(&head, &tail);
        return Err(anyhow!("ffmpeg failed: {}{}", status_str, log_str));
    }
    Ok(())
}

/// Format the captured head + tail of ffmpeg's stderr into a single
/// human-readable suffix appended to the error message. Lines that
/// match ffmpeg's `[Error]` / `Invalid` / `failed` prefixes get
/// promoted to the front so the actual cause appears first regardless
/// of where it lived in the stream.
fn format_diagnostic_log(head: &[String], tail: &std::collections::VecDeque<String>) -> String {
    if head.is_empty() && tail.is_empty() {
        return String::new();
    }
    // Deduplicate: if the encode was so short the head and tail
    // overlap, keep only one copy of each line in original order.
    let mut seen = std::collections::HashSet::<&str>::new();
    let mut combined: Vec<&str> = Vec::with_capacity(head.len() + tail.len());
    for line in head.iter().chain(tail.iter()) {
        if seen.insert(line.as_str()) {
            combined.push(line.as_str());
        }
    }
    // Promote error-shaped lines to the front. We look for tokens
    // ffmpeg uses for actual diagnostic output rather than progress
    // updates: `Error`, `Invalid`, `Unable`, `Could not`, `failed`,
    // `No such`, `Stream specifier`. Anything else falls through in
    // capture order so the user sees the surrounding context too.
    let is_error_line = |line: &&str| {
        let s = line.to_ascii_lowercase();
        s.contains("error")
            || s.contains("invalid")
            || s.contains("unable")
            || s.contains("could not")
            || s.contains("failed")
            || s.contains("no such")
            || s.contains("stream specifier")
            || s.contains("matches no streams")
    };
    let (errors, rest): (Vec<&str>, Vec<&str>) =
        combined.iter().copied().partition(is_error_line);

    let mut all: Vec<&str> = Vec::with_capacity(errors.len() + rest.len());
    all.extend(errors);
    all.extend(rest);

    // Cap each line and the total length so a runaway log doesn't
    // overflow whatever the host UI uses to display the error.
    const MAX_LINE: usize = 240;
    const MAX_TOTAL: usize = 1500;
    let mut acc = String::with_capacity(MAX_TOTAL.min(2048));
    acc.push_str(" (last log: ");
    let mut first = true;
    for line in all {
        let truncated: String = if line.chars().count() > MAX_LINE {
            let mut t: String = line.chars().take(MAX_LINE - 3).collect();
            t.push_str("...");
            t
        } else {
            line.to_string()
        };
        if !first {
            if acc.len() + truncated.len() + 3 > MAX_TOTAL {
                acc.push_str(" | ...");
                break;
            }
            acc.push_str(" | ");
        }
        acc.push_str(&truncated);
        first = false;
    }
    acc.push(')');
    acc
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
