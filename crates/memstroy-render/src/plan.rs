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
            args.push("-c:a".into());
            args.push("aac".into());
            args.push("-b:a".into());
            args.push("192k".into());
        }
        args.push("-r".into());
        args.push(self.fps.to_string());
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
        args.push(self.output.to_string_lossy().to_string());
        args
    }
}

/// Build (but do not run) an FFmpeg plan for the given scene + output
/// path. Resolves all relative asset paths against `assets_root`.
pub fn build_plan(scene: &Scene, output: &Path, assets_root: &Path) -> Result<FfmpegPlan> {
    let mut builder = FilterGraphBuilder::new(scene, assets_root);
    builder.build()?;
    let (filter, inputs, map_video, map_audio) = builder.finish();
    Ok(FfmpegPlan {
        inputs,
        filter_complex: filter,
        map_video,
        map_audio,
        output: output.to_path_buf(),
        fps: scene.output.fps,
        resolution: scene.output.resolution,
        duration: scene.output.duration,
    })
}
