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
mod autosave;
mod build_info;
mod canvas_preview;
mod curve_editor;

mod frame_snapshot;
mod fx_preview;
mod gpu_preview;
mod i18n;
mod image_effects;
mod image_fx_cache;
mod image_fx_worker;
mod jobs;
mod kf_anim;
mod panels;
mod settings;
mod skeleton_editor;
mod split_crop;
mod state;
mod system_fonts;
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
        .on_thread_stop(|| {
            tracing::debug!("tokio worker thread stopping");
        })
        .build()?;

    // ── Window / taskbar icon ───────────────────────────────────────
    // Branding logo: assets/internal_images/catost.png. We bake the
    // PNG bytes into the binary via include_bytes! so a deployed
    // bundle never depends on the source-tree path at runtime —
    // important because client builds ship without an `assets/`
    // directory next to the executable.
    //
    // Decoding is best-effort: if the PNG ever becomes malformed we
    // emit a tracing warning and fall back to the OS default icon
    // rather than crashing the whole editor on startup.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1480.0, 900.0])
        .with_min_inner_size([1100.0, 700.0])
        .with_title("Memstroy-inator");
    const APP_ICON_PNG: &[u8] =
        include_bytes!("../../../assets/internal_images/catost.png");
    match image::load_from_memory(APP_ICON_PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            viewport = viewport.with_icon(egui::IconData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
            });
        }
        Err(err) => {
            tracing::warn!("failed to decode embedded app icon (catost.png): {err}");
        }
    }

    let opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "memstroy-gui",
        opts,
        Box::new(move |cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // Seed the egui font table with the default bundled set
            // so subsequent on-demand `ensure_font_loaded` calls can
            // extend it instead of clobbering the built-in
            // Proportional / Monospace families. The actual scan of
            // system fonts happens lazily on first picker open — the
            // cost is otherwise paid for every cold start even when
            // the user never opens a text overlay.
            system_fonts::install_default_definitions(&cc.egui_ctx);
            // Kick off the system-font discovery scan in a background
            // thread RIGHT NOW so the picker is populated by the time
            // the user clicks "+T" and opens the font dropdown. The
            // scan walks every TTF/OTF in the OS font directories
            // and parses each one for its display name — on a typical
            // Windows machine that's 600+ files, several seconds of
            // work that absolutely must not block the UI thread.
            // Without this prefetch the very first text overlay had
            // a multi-second hang ("при создании текста пролаг
            // долгий") while the inspector waited for the scan to
            // finish synchronously.
            system_fonts::kick_background_scan();
            Ok(Box::new(app::App::new(runtime)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
