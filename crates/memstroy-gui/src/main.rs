//! Entry point for the desktop editor.
//
// On Windows release builds we suppress the OS-allocated console
// window: the GUI is a normal end-user app, not a CLI tool, and the
// flicker of `cmd.exe` opening behind the editor is noisy and
// confuses non-technical users (it also looks unprofessional in a
// packaged installer build).
//
// Debug builds keep the console so `tracing` output is visible
// during development; toggling on `not(debug_assertions)` means the
// console only disappears in `cargo build --release` artefacts —
// which is exactly what `scripts/package-client.ps1` ships.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio_engine;
mod build_info;
mod canvas_preview;
mod curve_editor;

mod gpu_preview;
mod i18n;
mod image_editor;
mod image_effects;
mod image_fx_cache;
mod image_fx_worker;
mod jobs;
mod kf_anim;
mod panels;
mod settings;
mod skeleton_editor;
mod state;
mod title_templates;
mod undo;
mod video_cache;
mod web_image_search;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(
                    // Default: app at debug, everything else at info, but
                    // silence the symphonia container/codec parsers since they
                    // spam "skipping junk" / "invalid mpeg audio header" warnings
                    // for clips that simply have no decodable audio stream.
                    "info,memstroy_gui=debug,\
                     symphonia=error,\
                     symphonia_core=error,\
                     symphonia_format_mp3=error,\
                     symphonia_format_wav=error,\
                     symphonia_format_isomp4=error,\
                     symphonia_codec_pcm=error,\
                     symphonia_bundle_mp3=error,\
                     symphonia_bundle_flac=error,\
                     lewton=error"
                )),
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
            .with_title("memstroy-inator"),
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
