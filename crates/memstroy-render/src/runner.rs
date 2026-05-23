use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use memstroy_core::Scene;
use tokio::io::{AsyncBufReadExt, BufReader};
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

    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;

    let stderr = child.stderr.take().unwrap();
    let mut reader = BufReader::new(stderr).lines();
    while let Some(line) = reader.next_line().await? {
        on_log(&line);
    }

    let status = child.wait().await?;
    if !status.success() {
        warn!(code = status.code(), "ffmpeg exited with non-zero status");
        return Err(anyhow!("ffmpeg failed: {:?}", status));
    }
    Ok(())
}
