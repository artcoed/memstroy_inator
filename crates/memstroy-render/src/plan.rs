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
    /// Image sequence consumed at the output frame rate. Used by the
    /// snapshot-based render path (see `build_image_sequence_plan`).
    /// The `path` for this input is an ffmpeg-style C-printf pattern
    /// like `/tmp/render-1234/%06d.png` and ffmpeg is told to read
    /// frames in order at the output fps, NOT to loop a single still.
    ImageSequence,
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
            } else if inp.kind == InputKind::ImageSequence {
                // Snapshot-based render: read a sequential PNG dump
                // at the output frame rate. `-start_number 1` matches
                // the 1-based naming the snapshot writer uses
                // (`000001.png`, `000002.png`, …); `-framerate` ties
                // the demuxer's per-frame PTS to the scene's fps.
                // Note: NO `-loop 1` here — looping a sequence demuxer
                // collides with `-framerate` and produces a 1-frame
                // output regardless of how many PNGs are on disk.
                args.push("-framerate".into());
                args.push(self.fps.to_string());
                args.push("-start_number".into());
                args.push("1".into());
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



/// Build (but do not run) an FFmpeg plan that consumes a pre-rendered
/// PNG sequence as its video input and mixes the scene's audio tracks
/// alongside.
///
/// The picture is composed by `frame_snapshot::render_full_frame_at`
/// (one PNG per output frame, written to `image_dir` with 1-based
/// 6-digit names — `000001.png`, `000002.png`, …). FFmpeg only has to
/// remux the sequence into MP4 and add the AAC-encoded audio bus.
///
/// This is the back-end of the snapshot-based render path that gives
/// the user pixel-for-pixel parity with the canvas preview. The
/// existing `build_plan` route (filter_complex video composition)
/// stays available for the CLI / preview frame extraction; a future
/// PR can deprecate it once the snapshot path proves itself.
///
/// `assets_root` is used for resolving relative audio source paths,
/// matching the contract of `build_plan`. `image_dir` should be the
/// directory that `frame_snapshot::render_full_frame_at` wrote into;
/// callers are responsible for cleaning it up after the encode (the
/// returned `cleanup_paths` is empty so we don't accidentally delete
/// it on success — the GUI's render-job cleanup task removes the
/// whole tree once ffmpeg finishes).
pub fn build_image_sequence_plan(
    scene: &Scene,
    image_dir: &Path,
    output: &Path,
    assets_root: &Path,
) -> Result<FfmpegPlan> {
    // Same render-frame ↔ output canonicalisation `build_plan` does,
    // for the same reason: the snapshot painter uses
    // `render_frame.resolution` as its world-to-output scale, while
    // the encoder we're targeting expects `output.resolution`. Force
    // the two to agree at the renderer boundary so the snapshot's
    // `rw × rh` frames line up with the expected output dimensions.
    let mut canonical: Scene;
    let scene_ref: &Scene = if scene.render_frame.resolution != scene.output.resolution {
        canonical = scene.clone();
        canonical.render_frame.resolution = canonical.output.resolution;
        &canonical
    } else {
        scene
    };

    // Audio-only filtergraph — no video filters, just the
    // per-track normalisation chain that feeds amix.
    let mut builder = crate::filtergraph::FilterGraphBuilder::new(scene_ref, assets_root);
    builder.build_audio_only()?;
    let (audio_filter, audio_inputs, _map_video_dummy, map_audio, cleanup_paths) =
        builder.finish();

    // Slot 0 = the image sequence. The path is the C-printf pattern
    // ffmpeg's image2 demuxer expects; the to_args path attaches
    // `-framerate <fps> -start_number 1` automatically when it sees
    // `InputKind::ImageSequence`.
    let mut inputs: Vec<FfmpegInput> = Vec::with_capacity(audio_inputs.len() + 1);
    inputs.push(FfmpegInput {
        path: image_dir.join("%06d.png"),
        kind: InputKind::ImageSequence,
        r#loop: false,
        seek: None,
        t: None,
    });
    // Audio inputs are appended AFTER the image sequence. The audio
    // filter chunks the builder produced refer to inputs by their
    // `[N:a]` index — and those indices were allocated assuming slot
    // 0 was the FIRST audio input. Now that slot 0 is our image
    // sequence, every audio reference is off-by-one. We rewrite the
    // filter complex by bumping `[N:a]` → `[N+1:a]` for every audio
    // input slot the builder claimed.
    let audio_input_count = audio_inputs.len();
    inputs.extend(audio_inputs);
    let filter_complex = if audio_input_count == 0 {
        // No audio at all → empty filter_complex. The plan still
        // works because we map video straight from `[0:v]` (no
        // filter graph required for a passthrough).
        String::new()
    } else {
        bump_audio_input_indices(&audio_filter, audio_input_count)
    };

    // Map video directly from the image-sequence input (slot 0).
    // The image2 demuxer exposes its single video stream as
    // `[0:v]`, which the encoder consumes with no extra filters.
    let map_video = "[0:v]".to_string();

    Ok(FfmpegPlan {
        inputs,
        filter_complex,
        map_video,
        map_audio,
        output: output.to_path_buf(),
        fps: scene_ref.output.fps,
        resolution: scene_ref.output.resolution,
        duration: scene_ref.output.duration,
        cleanup_paths,
    })
}

/// Rewrite a filter_complex string to bump every `[N:a]` audio-input
/// reference up by one slot. Used by `build_image_sequence_plan` when
/// it inserts the image sequence as input slot 0 — the audio builder
/// allocated indices starting at 0 and we need to shift them all so
/// they still point at the right `-i` after the prepend.
///
/// The matcher is intentionally narrow: it only rewrites tokens of
/// the form `[<digits>:a]`, leaving alone:
///   * `[<digits>:v]` — there are no video inputs in the audio-only
///     filter graph, but if a future change adds one this helper
///     stays safe.
///   * Generated labels like `[a_3]`, `[amix_5]` — they don't have
///     the `:a` suffix.
fn bump_audio_input_indices(filter: &str, count: usize) -> String {
    if count == 0 {
        return filter.to_string();
    }
    let mut out = String::with_capacity(filter.len());
    let bytes = filter.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Try to parse `[<digits>:a]`.
            let j = i + 1;
            let mut digits_end = j;
            while digits_end < bytes.len() && bytes[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end > j
                && digits_end + 2 < bytes.len()
                && bytes[digits_end] == b':'
                && bytes[digits_end + 1] == b'a'
                && bytes[digits_end + 2] == b']'
            {
                // Parse the slot number, increment, write the
                // rewritten token. Safe-ish: if the number is bigger
                // than usize::MAX (it won't be) we fall through and
                // copy verbatim.
                let s = &filter[j..digits_end];
                if let Ok(n) = s.parse::<usize>() {
                    out.push('[');
                    out.push_str(&(n + 1).to_string());
                    out.push_str(":a]");
                    i = digits_end + 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod image_sequence_tests {
    use super::*;

    #[test]
    fn bump_rewrites_audio_input_references() {
        let in_filter = "[0:a]aresample=44100[a_1];[1:a]aresample=44100[a_2];\
                         [a_1][a_2]amix=inputs=2[amix_3]";
        let out = bump_audio_input_indices(in_filter, 2);
        assert!(out.contains("[1:a]aresample"));
        assert!(out.contains("[2:a]aresample"));
        // Generated labels untouched.
        assert!(out.contains("[a_1]"));
        assert!(out.contains("[a_2]"));
        assert!(out.contains("[amix_3]"));
        // Original `[0:a]` / `[1:a]` shifted away.
        assert!(!out.contains("[0:a]"));
    }

    #[test]
    fn bump_is_no_op_on_empty_filter() {
        assert_eq!(bump_audio_input_indices("", 3), "");
    }

    #[test]
    fn bump_skips_non_audio_brackets() {
        let in_filter = "[a_1]volume=0.5[a_2]";
        assert_eq!(bump_audio_input_indices(in_filter, 1), in_filter);
    }
}
