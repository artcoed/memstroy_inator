use std::path::{Path, PathBuf};

use anyhow::Result;
use memstroy_core::Scene;

use crate::filtergraph::FilterGraphBuilder;

/// Concrete FFmpeg invocation derived from a [`Scene`]. Keep all the
/// flag/arg construction in this struct so renderer callers (CLI, GUI)
/// can introspect or pretty-print the command before running it.
#[derive(Debug, Clone)]
pub struct FfmpegPlan {
    pub inputs: Vec<FfmpegInput>,
    pub filter_complex: String,
    pub map_video: String,
    pub map_audio: Option<String>,
    pub output: PathBuf,
    pub fps: u32,
    pub resolution: [u32; 2],
    pub duration: f32,
    /// Auxiliary files the builder generated on disk that should be
    /// deleted after FFmpeg finishes — currently used for the alpha
    /// PNGs that back `EffectKind::Mask` exports. The runner walks
    /// this list at the end of the render (success or failure) and
    /// best-effort removes each entry. Empty for plans that don't
    /// rely on generated assets.
    pub cleanup_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct FfmpegInput {
    pub path: PathBuf,
    pub kind: InputKind,
    /// Optional `-stream_loop -1` for looping image-as-video.
    pub r#loop: bool,
    /// Optional `-ss` seek into the source.
    pub seek: Option<f32>,
    /// Optional `-t` duration limit.
    pub t: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Image,
    Video,
    Audio,
}

impl FfmpegPlan {
    /// Convert into argv form ready for `Command::args`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["-y".into(), "-hide_banner".into()];
        for inp in &self.inputs {
            // Image inputs are looped at the FPS of the output and
            // duration is constrained by the global -t at the end.
            if inp.kind == InputKind::Image {
                args.push("-loop".into());
                args.push("1".into());
                args.push("-framerate".into());
                args.push(self.fps.to_string());
            } else if inp.r#loop {
                args.push("-stream_loop".into());
                args.push("-1".into());
            }
            if let Some(s) = inp.seek {
                args.push("-ss".into());
                args.push(format!("{:.3}", s));
            }
            args.push("-i".into());
            args.push(inp.path.to_string_lossy().to_string());
        }
        args.push("-filter_complex".into());
        args.push(self.filter_complex.clone());
        args.push("-map".into());
        args.push(self.map_video.clone());
        if let Some(a) = &self.map_audio {
            args.push("-map".into());
            args.push(a.clone());
            // ── AAC encoder hardening ──
            // Pinning sample rate (`-ar`) and channel count (`-ac`)
            // alongside the codec belt-and-braces forces the encoder
            // to negotiate one specific format with the filter
            // graph. Without these the encoder used to defer init
            // until the first frame arrived from `amix`, and when
            // the filter graph collapsed (mismatched sample rates
            // across sources, see `emit_audio`) the encoder
            // surfaced as "Could not open encoder before EOF" with
            // no diagnostic lead-in. Aligning these values with the
            // post-mix `aformat` filter means the encoder opens at
            // graph-init time and any failure is now the actual
            // filter error.
            args.push("-c:a".into());
            args.push("aac".into());
            args.push("-b:a".into());
            args.push("192k".into());
            args.push("-ar".into());
            args.push("44100".into());
            args.push("-ac".into());
            args.push("2".into());
        }
        args.push("-r".into());
        args.push(self.fps.to_string());
        // `-fps_mode cfr` (the modern replacement for `-vsync cfr`)
        // tells ffmpeg to emit a constant-frame-rate stream by
        // duplicating / dropping frames as needed. Without it the
        // x264 encoder occasionally received frames with non-
        // monotonic timestamps from the more elaborate overlay
        // chains (rotate + per-frame scale), which surfaced as the
        // "-22 Invalid argument" libx264 task failure even when the
        // filter graph itself was fine.
        args.push("-fps_mode".into());
        args.push("cfr".into());
        args.push("-t".into());
        args.push(format!("{:.3}", self.duration));
        args.push("-pix_fmt".into());
        args.push("yuv420p".into());
        args.push("-c:v".into());
        args.push("libx264".into());
        args.push("-preset".into());
        args.push("medium".into());
        args.push("-crf".into());
        args.push("19".into());
        // `+faststart` relocates the moov atom to the front of the
        // file after encoding finishes, so players (and the editor
        // itself when re-importing the result) can start playback
        // without seeking to the end first. Cheap to add and a
        // strict win for the user.
        args.push("-movflags".into());
        args.push("+faststart".into());
        args.push(self.output.to_string_lossy().to_string());
        args
    }
}

/// Build (but do not run) an FFmpeg plan for the given scene + output
/// path. Resolves all relative asset paths against `assets_root`.
pub fn build_plan(scene: &Scene, output: &Path, assets_root: &Path) -> Result<FfmpegPlan> {
    // ── Render-frame ⇄ output canonicalisation ────────────────────
    //
    // The canvas preview converts an element's legacy `[0..1]`
    // normalised position to world pixels via
    // `pos * render_frame.resolution`. The renderer's
    // `expr::build_element_transform` does the same conversion via
    // `pos * output.resolution`. As long as the two resolution
    // fields agree, preview and render see every overlay at the
    // same world position; when they diverge — e.g. because a
    // scene was loaded from disk with one value and the inspector
    // panel (which used to be the only place that re-synced them)
    // never opened — every overlay's world position drifts in the
    // export, the rf-camera's centre crop chops them off-canvas
    // and the user sees only the actor (or the actor at the wrong
    // size) instead of the composed scene. We canonicalise the two
    // fields here so EVERY render path — full export, single-frame
    // preview, CLI, GUI — is immune to that class of bug regardless
    // of what set the scene up.
    //
    // Policy: `render_frame.resolution` IS the output resolution
    // (per its own doc-comment in `core::canvas::RenderFrame`).
    // We point both at the same value so the preview's
    // `world_w = rf.resolution[0]` formula and the renderer's
    // `world_w = output.resolution[0]` formula agree by
    // construction.
    let mut canonical: Scene;
    let scene_ref: &Scene = if scene.render_frame.resolution != scene.output.resolution {
        canonical = scene.clone();
        canonical.render_frame.resolution = canonical.output.resolution;
        &canonical
    } else {
        scene
    };

    let mut builder = FilterGraphBuilder::new(scene_ref, assets_root);
    builder.build()?;
    let (filter, inputs, map_video, map_audio, cleanup_paths) = builder.finish();
    Ok(FfmpegPlan {
        inputs,
        filter_complex: filter,
        map_video,
        map_audio,
        output: output.to_path_buf(),
        fps: scene_ref.output.fps,
        resolution: scene_ref.output.resolution,
        duration: scene_ref.output.duration,
        cleanup_paths,
    })
}
