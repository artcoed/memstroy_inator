//! Entry point for the desktop editor.

mod app;
mod audio_engine;
mod clip_editor;
mod curve_editor;
mod gpu_preview;
mod jobs;
mod node_editor;
mod panels;
mod state;
mod undo;
mod video_cache;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,memstroy_gui=debug")),
        )
        .with_target(false)
        .try_init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("memstroy-bg")
        .build()?;

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1480.0, 900.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_title("Memstroy Generator"),
        ..Default::default()
    };

    eframe::run_native(
        "memstroy-gui",
        opts,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app::App::new(runtime)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
