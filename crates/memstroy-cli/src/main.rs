//! CLI entry point for memstroy_generator.
//!
//! All operations the GUI does are also exposed here for headless and
//! scripted use:
//!   `memstroy download`  — pull every "Имба" clip from the channel.
//!   `memstroy chroma`    — strip the green background from one clip.
//!   `memstroy remove-bg` — rembg-style ML matting (U²-Netp) on a still.
//!   `memstroy render`    — produce an MP4 from a scene description.
//!   `memstroy preview`   — render one PNG frame from a scene.
//!   `memstroy new`       — emit a starter scene.yaml.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use memstroy_core::Scene;
use memstroy_tg::{download_videos, fetch_all, ChannelCatalog};
use memstroy_render::{render_preview_frame, render_scene};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "memstroy", version, about = "Mellstroy meme generator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Scrape a Telegram public channel and download all videos.
    Download {
        /// Channel handle, e.g. `MELLSTROYfonz`.
        #[arg(long, default_value = "MELLSTROYfonz")]
        channel: String,
        /// Output directory. Defaults to `./assets/mellstroy`.
        #[arg(long, default_value = "assets/mellstroy")]
        out: PathBuf,
        /// Only download posts whose body contains this substring.
        /// Pass an empty string to disable filtering and download all.
        #[arg(long, default_value = "Имба")]
        filter: String,
        /// Maximum pages to crawl. The channel has ~600 posts at ~16
        /// per page, so 60 covers it with margin.
        #[arg(long, default_value_t = 80)]
        max_pages: usize,
        /// Re-download even if the file already exists.
        #[arg(long)]
        overwrite: bool,
        /// Concurrent downloads.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Skip the actual download; just write the catalog JSON.
        #[arg(long)]
        catalog_only: bool,
    },
    /// Strip the green background from a single clip into RGBA WebM.
    Chroma {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 0.20)]
        similarity: f32,
        #[arg(long, default_value_t = 0.10)]
        blend: f32,
    },
    /// Remove the background from a still image using the U²-Netp ONNX
    /// model (rembg-style alpha matting). Outputs an RGBA PNG.
    RemoveBg {
        /// Source image (PNG / JPEG / WebP).
        input: PathBuf,
        /// Destination RGBA PNG.
        #[arg(short, long)]
        output: PathBuf,
        /// Path to the U²-Netp ONNX model. Download from
        /// `https://github.com/danielgatis/rembg/releases` and place it
        /// at the default location to avoid passing this flag.
        #[arg(long, default_value = "assets/models/u2netp.onnx")]
        model: PathBuf,
        /// Optional saliency threshold in `[0, 1]`. When set above 0,
        /// produces a hard binary cutout instead of the soft matte.
        #[arg(long)]
        threshold: Option<f32>,
    },
    /// Render a scene file to MP4.
    Render {
        scene: PathBuf,
        #[arg(short, long, default_value = "out.mp4")]
        output: PathBuf,
        /// Asset root for resolving relative paths inside the scene.
        #[arg(long, default_value = ".")]
        assets: PathBuf,
    },
    /// Render a single PNG preview frame from a scene at time `t`.
    Preview {
        scene: PathBuf,
        #[arg(short, long, default_value = "preview.png")]
        output: PathBuf,
        #[arg(short, long, default_value_t = 0.0)]
        t: f32,
        #[arg(long, default_value = ".")]
        assets: PathBuf,
    },
    /// Emit a starter scene.yaml at the given path.
    New { path: PathBuf },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Download { channel, out, filter, max_pages, overwrite, concurrency, catalog_only } => {
            run_download(channel, out, filter, max_pages, overwrite, concurrency, catalog_only).await
        }
        Cmd::Chroma { input, output, similarity, blend } => {
            run_chroma(input, output, similarity, blend).await
        }
        Cmd::RemoveBg { input, output, model, threshold } => {
            run_remove_bg(input, output, model, threshold).await
        }
        Cmd::Render { scene, output, assets } => run_render(scene, output, assets).await,
        Cmd::Preview { scene, output, t, assets } => run_preview(scene, output, t, assets).await,
        Cmd::New { path } => run_new(path),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,memstroy_tg=info,memstroy_render=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn run_download(
    channel: String,
    out: PathBuf,
    filter: String,
    max_pages: usize,
    overwrite: bool,
    concurrency: usize,
    catalog_only: bool,
) -> Result<()> {
    info!(channel, max_pages, "scraping channel");
    let mut posts = fetch_all(&channel, max_pages).await?;
    let total = posts.len();
    if !filter.is_empty() {
        posts.retain(|p| p.body_contains(&filter));
    }
    info!(total, kept = posts.len(), "scrape done");

    tokio::fs::create_dir_all(&out).await.context("mkdir output")?;
    let catalog = ChannelCatalog {
        channel: channel.clone(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        posts: posts.clone(),
    };
    let catalog_path = out.join("catalog.json");
    tokio::fs::write(&catalog_path, serde_json::to_vec_pretty(&catalog)?).await?;
    info!(path = %catalog_path.display(), "wrote catalog");

    if catalog_only {
        return Ok(());
    }
    let stats = download_videos(&posts, &out, overwrite, concurrency).await?;
    info!(?stats, "download finished");
    Ok(())
}

async fn run_chroma(input: PathBuf, output: PathBuf, similarity: f32, blend: f32) -> Result<()> {
    // Use FFmpeg directly for whole-clip chromakey: faster than the
    // CPU implementation and produces broadcast-quality output.
    let bin = memstroy_render::ffmpeg_binary();
    let filter = format!(
        "chromakey=0x00B140:{}:{},despill=type=green:mix={}:expand=0,format=yuva420p",
        similarity, blend, 0.3
    );
    let status = tokio::process::Command::new(bin)
        .args([
            "-y",
            "-hide_banner",
            "-i",
        ])
        .arg(&input)
        .args(["-vf", &filter, "-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-b:v", "0", "-crf", "30"])
        .arg(&output)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("ffmpeg chromakey failed: {:?}", status);
    }
    Ok(())
}

async fn run_remove_bg(
    input: PathBuf,
    output: PathBuf,
    model: PathBuf,
    threshold: Option<f32>,
) -> Result<()> {
    use memstroy_vision::bgremove::{remove_background_to_file, BackgroundRemover, U2NetpBgRemover};

    let mut remover = U2NetpBgRemover::new(model.clone());
    if let Some(t) = threshold {
        remover = remover.with_threshold(t);
    }
    info!(
        backend = remover.id(),
        model = %model.display(),
        in_path = %input.display(),
        out_path = %output.display(),
        "running background removal"
    );
    remove_background_to_file(&remover, &input, &output).await?;
    info!(out = %output.display(), "background removal complete");
    Ok(())
}

async fn run_render(scene: PathBuf, output: PathBuf, assets: PathBuf) -> Result<()> {
    let scene = Scene::load(&scene)?;
    render_scene(&scene, &assets, &output, |line| {
        eprintln!("ffmpeg: {}", line);
    })
    .await?;
    info!(out = %output.display(), "render complete");
    Ok(())
}

async fn run_preview(scene: PathBuf, output: PathBuf, t: f32, assets: PathBuf) -> Result<()> {
    let scene = Scene::load(&scene)?;
    render_preview_frame(&scene, &assets, t, &output).await?;
    info!(out = %output.display(), t, "preview frame rendered");
    Ok(())
}

fn run_new(path: PathBuf) -> Result<()> {
    let scene = starter_scene();
    scene.save(&path)?;
    info!(path = %path.display(), "starter scene written");
    Ok(())
}

fn starter_scene() -> Scene {
    use memstroy_core::*;
    let bg = Background {
        id: "bg1".into(),
        source: MediaSource::SolidColor { color: [240, 240, 240] },
        start: 0.0,
        duration: 8.0,
        fit: Fit::Cover,
        transition: Transition::Cut,
    };
    let text = Overlay::Text(TextOverlay {
        id: "title".into(),
        text: "Имба".into(),
        t_in: 0.5,
        t_out: 7.5,
        style: TextStyle::default(),
        layout: vec![Keyframe::new(0.0, OverlayState { pos: [0.5, 0.15], scale: 1.0, scale_y: 1.0, rotation_deg: 0.0, opacity: 1.0, flip_x_anim: 1.0, flip_y_anim: 1.0 })],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        z_index: 100,
        behind_actors: false,
        effects: Vec::new(),
        animated_params: Default::default(),
        z_order: 0,
        parent_id: None,
    });
    Scene {
        format_version: 1,
        output: OutputSpec::default(),
        backgrounds: vec![bg],
        camera: Vec::new(),
        actors: Vec::new(),
        overlays: vec![text],
        audio: Vec::new(),
        render_frame: RenderFrame::default(),
        canvas_layouts: Vec::new(),
        skeleton_templates: Vec::new(),
        effect_layers: Vec::new(),
    }
}
