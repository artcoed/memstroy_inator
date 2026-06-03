//! Background worker for the image-effects pipeline.
//!
//! `submit_image_fx_job` dispatches a (decode + effect stack) bake
//! onto the editor's tokio runtime via `spawn_blocking`. The pipeline
//! itself stays in `crate::image_effects::apply_effect_stack` — this
//! module is just the plumbing that gets the work off the UI thread:
//!
//! 1. **Dedup**: claim an in-flight slot on `ImageFxCache`. If another
//!    worker is already baking the same `(path, sig)`, the call is a
//!    no-op. The cache transitions the corresponding entry to
//!    `Pending` as part of the claim, so the canvas knows to draw the
//!    unprocessed image while the bake runs.
//!
//! 2. **L1 reuse**: try to read the decoded RGBA from the cache before
//!    re-decoding the source PNG. The common slider-drag case keeps
//!    the same source path with a different effect signature each
//!    frame — the L1 hit makes that path very cheap (no PNG decode,
//!    no Vec<u8> allocation for the input buffer).
//!
//! 3. **Spawn blocking**: `tokio::task::spawn_blocking` runs the work
//!    on tokio's blocking pool, sharing the runtime that's already
//!    powering ffmpeg / refresh / web-search jobs (no extra thread
//!    pool). The result is posted back through the existing
//!    `JobEvent` mpsc channel to the App's `pump_events` drain.
//!
//! 4. **Wake the UI**: `egui::Context::request_repaint()` is called
//!    from the worker after sending the result so the App's event
//!    loop drains the message and uploads the texture promptly even
//!    when the user is sitting idle (no mouse motion). egui's Context
//!    is `Send + Sync` and cheap to clone.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use memstroy_core::effects::Effect;
use tokio::runtime::Handle;
use tracing::warn;

use crate::image_fx_cache::{DecodedImage, ImageFxCache};
use crate::jobs::JobEvent;

/// Result posted by a finished bake job. The `outcome` carries either
/// a fully-baked RGBA buffer ready to be uploaded as a texture, or a
/// human-readable error string. The cache is updated by the App
/// drain (texture upload requires the UI thread / `egui::Context`).
#[derive(Debug)]
pub struct ImageFxResult {
    pub path: PathBuf,
    pub sig: u64,
    pub outcome: Result<ImageFxBaked, String>,
}

/// Successfully baked output handed back to the UI thread for upload.
#[derive(Debug)]
pub struct ImageFxBaked {
    /// RGBA8 buffer with the full effect stack applied, ready for
    /// `egui::ColorImage::from_rgba_unmultiplied`.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Crop UV inset accumulated by the stack (left, top, right, bottom).
    pub crop: [f32; 4],
}

/// Dispatch a fresh bake. Idempotent — concurrent calls for the same
/// `(path, sig)` after the first one are silently dropped via the
/// cache's in-flight set. Caller does NOT need to check whether a job
/// is already running.
///
/// `egui_ctx` is cloned and moved into the worker so the worker can
/// wake the UI on completion. The runtime handle is borrowed; the
/// caller is expected to keep the `Runtime` alive for the lifetime
/// of the editor (the App owns it).
pub fn submit_image_fx_job(
    rt: &Handle,
    tx: &Sender<JobEvent>,
    cache: &Arc<ImageFxCache>,
    egui_ctx: &egui::Context,
    path: PathBuf,
    effects: Vec<Effect>,
    sig: u64,
) {
    if !cache.try_claim_in_flight(&path, sig) {
        // A worker is already baking this exact (path, sig) pair.
        // The canvas will read the existing Pending state on its
        // next paint and fall back to the unprocessed image.
        return;
    }

    let cache = cache.clone();
    let tx = tx.clone();
    let ctx = egui_ctx.clone();

    rt.spawn_blocking(move || {
        let outcome = run_bake(&cache, &path, &effects);

        // Always release the in-flight claim, success or failure.
        cache.release_in_flight(&path, sig);

        if let Err(ref e) = outcome {
            // One log per failure — the cache also stores the reason
            // so the canvas can surface it without spamming the log
            // every frame.
            warn!(
                path = %path.display(),
                sig = format!("{:x}", sig),
                "image-fx bake failed: {e}",
            );
        }

        let _ = tx.send(JobEvent::ImageFxReady(ImageFxResult { path, sig, outcome }));
        // Nudge egui so the App's pump_events drain runs even when
        // the user is idle (no input -> no implicit repaint).
        ctx.request_repaint();
    });
}

/// Run the actual bake on the worker thread. Returns the baked RGBA
/// buffer and the accumulated Crop UV inset.
fn run_bake(
    cache: &ImageFxCache,
    path: &std::path::Path,
    effects: &[Effect],
) -> Result<ImageFxBaked, String> {
    // L1 hit: skip the PNG decode entirely.
    let decoded = match cache.get_decoded(path) {
        Some(d) => d,
        None => {
            let img = image::open(path)
                .map_err(|e| format!("decode failed for {}: {e}", path.display()))?
                .to_rgba8();
            let w = img.width();
            let h = img.height();
            let raw = img.into_raw();
            let d = DecodedImage {
                rgba: Arc::new(raw),
                width: w,
                height: h,
            };
            cache.put_decoded(path.to_path_buf(), d.clone());
            d
        }
    };

    // The effect pipeline mutates the buffer in place; we own a fresh
    // copy so the L1 entry stays intact for the next signature change.
    let mut buf: Vec<u8> = (*decoded.rgba).clone();
    let crop = crate::image_effects::apply_effect_stack(
        &mut buf,
        decoded.width,
        decoded.height,
        effects,
        0.0,
    );

    Ok(ImageFxBaked {
        rgba: buf,
        width: decoded.width,
        height: decoded.height,
        crop: [crop.0, crop.1, crop.2, crop.3],
    })
}

/// Look up an effect-baked texture for `(path, effects)`, dispatching
/// a background bake on a cache miss. Mirrors the helper that the
/// canvas-preview path uses so secondary surfaces (e.g. the image
/// editor's preview) can reuse the *same* baked texture without
/// re-implementing the lookup-or-dispatch dance.
///
/// Returns `Some((handle, crop_inset))` when the cache holds a
/// `Ready` slot for this exact `(path, sig)`. Returns `None` for
/// `Pending` / `Failed` / `Miss` — in those cases the caller is
/// expected to fall back to the unprocessed image while the worker
/// completes (or after a permanent failure).
///
/// The `crop_inset` is `(left, top, right, bottom)` in normalised
/// 0..1 — the caller's mesh / preview rect should shrink to this
/// rectangle so the user sees the same crop the export will produce.
#[allow(dead_code)]
pub fn lookup_or_dispatch_image_fx(
    state: &crate::state::EditorState,
    path: &std::path::Path,
    effects: &[Effect],
    egui_ctx: &egui::Context,
) -> Option<(egui::TextureHandle, [f32; 4])> {
    use crate::image_fx_cache::LookupOutcome;

    let sig = crate::image_effects::signature(effects);

    match state.image_fx_cache.lookup(path, sig) {
        LookupOutcome::Ready(slot) => {
            return Some((slot.texture, slot.crop));
        }
        LookupOutcome::Pending => {
            // A worker is already baking this exact (path, sig).
            // Caller will fall back to the unprocessed image.
            egui_ctx.request_repaint_after(std::time::Duration::from_millis(33));
            return None;
        }
        LookupOutcome::Failed => {
            // Bake previously failed; don't keep retrying every frame.
            return None;
        }
        LookupOutcome::Miss => {
            // Fall through and dispatch a fresh bake.
        }
    }

    if let (Some(handle), Some(tx)) = (state.tokio_handle.as_ref(), state.image_fx_tx.as_ref()) {
        submit_image_fx_job(
            handle,
            tx,
            &state.image_fx_cache,
            egui_ctx,
            path.to_path_buf(),
            effects.to_vec(),
            sig,
        );
        egui_ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
    None
}
