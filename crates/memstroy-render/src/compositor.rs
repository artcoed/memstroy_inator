//! CPU-side frame compositor — guaranteed pixel parity with the canvas.
//!
//! ## Why this exists
//!
//! The previous renderer leaned entirely on FFmpeg's filter graph
//! (`overlay`, `chromakey`, `eq`, `colorbalance`, `rotate`, …). It
//! produced output that frequently disagreed with the canvas preview
//! by enough that users couldn't trust what they were seeing — the
//! recurring complaint that "выходное изображение не похоже на
//! выделенную область на холсте". The two pipelines used different
//! sampling, easing approximations, and rotation models, and any
//! single inconsistency cascaded through the per-pixel result.
//!
//! This module composes every output frame on the CPU using the same
//! math the canvas snapshot rasterizer uses (`frame_snapshot` in
//! `memstroy-gui`). Frames are then piped into FFmpeg via stdin for
//! encoding to MP4 — FFmpeg sees only a raw RGBA stream, no filter
//! graph involved. Result: bit-exact match between
//! `frame_snapshot::compose_frame` (which is what the canvas paints)
//! and the rendered MP4.
//!
//! ## Pipeline shape
//!
//! 1. `extract_video_clips` — for every distinct video source in the
//!    scene, extract ONLY the frames we'll actually need (clipped to
//!    `[t_in, t_out]` and the source's `duration`) at the output's
//!    `fps`. Frames land in `<temp>/memstroy-cpu-cache-…/000001.jpg`.
//!    This step is the single largest time cost of a render; we
//!    parallelise across distinct sources and emit live progress so
//!    the UI never freezes at "0%" for ages.
//!
//! 2. `compose_frame` per output frame — sample every actor /
//!    overlay's animated state at scene-time `t`, world-pixel
//!    transform via `world_to_output` (mirrors `frame_snapshot`),
//!    composite onto a fresh `RgbaImage` of the output resolution.
//!
//! 3. Each composed frame is fed into a long-running FFmpeg encoder
//!    process via stdin (`-f rawvideo -pix_fmt rgba`). The encoder
//!    does the heavy YUV / x264 work in parallel with the next
//!    frame's compositing — the rendering loop and the encoder's
//!    pipeline overlap.
//!
//! 4. Audio is muxed in via a second FFmpeg invocation that uses the
//!    legacy filter-graph path (which was already working for
//!    audio); the two MP4 streams are then `-c copy`-muxed.
//!
//! ## Progress reporting
//!
//! `render_scene_cpu` invokes its `progress_cb` parameter at every
//! meaningful step (extraction start / per-source completion / per
//! composed frame / encoder finalisation). The closure itself must
//! be cheap because it's hit hundreds of times during a render; the
//! GUI converts the messages into a percentage + status line.
//!
//! ## What's covered
//!
//! - **Backgrounds** — solid color, image, video (all `Fit` modes)
//! - **Actor clips** — chromakey + color correction + opacity
//!   + scale + rotation + flips + effect stack, sampled at the
//!   correct scene-time
//! - **Image overlays** — full transform pipeline + chromakey mask
//!   + effect stack
//! - **Video overlays** — frame extraction + chromakey + transform
//!   + effect stack
//! - **Text overlays** — routed through `text_rasterize::rasterize_text_overlay`
//!   + effect stack
//! - **Render frame** — pos / zoom / rotation animated camera
//! - **Easing / modifiers** — uses `keyframe::sample` /
//!   `evaluate_modifiers` directly, no expression rewriting
//! - **z_order** — actors and overlays interleave by z_order
//!
//! ## Out of scope (deferred)
//!
//! - **Skeleton attachments** — pose-driven element positioning

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use image::{Rgba, RgbaImage};
use memstroy_core::{
    canvas::WorldPos,
    effects::{Effect, EffectKind, MaskShape},
    keyframe, ChromaKeyParams, ColorCorrection, Fit, MediaSource, Overlay, RenderFrame,
    RenderFrameState, Scene,
};
use tracing::{info, warn};

use crate::proc;

// ─── PROGRESS REPORTING ─────────────────────────────────────────────

/// Live progress events emitted by the compositor. The renderer
/// thread builds these and forwards them to the calling code via the
/// `progress_cb` closure. Keep variants small — every string here
/// gets re-encoded into a `JobEvent::RenderLog` on the GUI side.
#[derive(Debug, Clone)]
pub enum Progress {
    /// One-line status update with a free-form message and an
    /// optional 0..100 percentage.
    Stage { message: String, percent: f32 },
    /// One frame has been composed and pushed to the encoder.
    Frame { index: usize, total: usize },
}

impl Progress {
    pub fn to_log_line(&self) -> String {
        match self {
            Progress::Stage { message, percent } => {
                format!("[{:5.1}%] {}", percent, message)
            }
            Progress::Frame { index, total } => {
                let pct = (*index as f32 / (*total).max(1) as f32) * 100.0;
                format!("frame {}/{} ({:.1}%)", index, total, pct)
            }
        }
    }
}

// ─── ENTRY POINTS ────────────────────────────────────────────────────

/// Render the full scene to `output_path` as MP4 using the CPU
/// compositor.
///
/// `progress_cb` is called from the rendering thread on every
/// meaningful step (extraction stage, frame encoded, mux done). The
/// callback closure must be `Send` because it is dispatched from a
/// `spawn_blocking` worker; it should be cheap (forwarding to a
/// channel is the typical implementation).
pub fn render_scene_cpu<F>(
    scene: &Scene,
    assets_root: &Path,
    output_path: &Path,
    mut progress_cb: F,
) -> Result<()>
where
    F: FnMut(Progress) + Send,
{
    let canonical = canonicalise_scene(scene);
    let [out_w, out_h] = canonical.output.resolution;
    let fps = canonical.output.fps.max(1);
    let duration = canonical.output.duration.max(1.0 / fps as f32);
    let total_frames = ((duration * fps as f32).ceil() as usize).max(1);

    info!(
        out_w,
        out_h, fps, total_frames, "starting CPU render pipeline",
    );
    progress_cb(Progress::Stage {
        message: format!(
            "Rendering {}×{} @ {}fps, {} frames",
            out_w, out_h, fps, total_frames
        ),
        percent: 0.0,
    });

    // ── Stage 1: extract video frames ──
    let mut clip_caches: ClipCacheStore = ClipCacheStore::default();
    extract_video_clips(
        &canonical,
        assets_root,
        fps,
        &mut clip_caches,
        &mut progress_cb,
    )?;

    // ── Stage 2: compose + encode per frame ──
    progress_cb(Progress::Stage {
        message: format!("Encoding {} frames...", total_frames),
        percent: 5.0,
    });

    let mut encoder = spawn_encoder(out_w, out_h, fps, output_path)?;

    // Drain the encoder's stderr in a background thread so it never
    // fills its pipe buffer (which would deadlock the encoder when
    // it tries to flush a warning while we're blocked on stdin).
    // The buffered output is also surfaced if the encoder errors out
    // mid-render.
    let stderr_handle = if let Some(stderr) = encoder.stderr.take() {
        Some(spawn_stderr_drain(stderr))
    } else {
        None
    };

    // Buffer reused for every frame so we don't re-allocate 8 MB / frame.
    let frame_byte_count = (out_w as usize) * (out_h as usize) * 4;

    // ── Parallel frame compositing pipeline ──
    //
    // Compose frames on a thread pool and feed them to the encoder in
    // order. This overlaps CPU compositing work across multiple cores
    // with the encoder's x264 threading, keeping both saturated.
    //
    // Architecture:
    //   - N worker threads compose frames into `Vec<u8>` buffers
    //   - A bounded channel (capacity = N) delivers composed frames
    //     to the main thread IN ORDER
    //   - The main thread writes each frame to the encoder's stdin
    //
    // The ordering guarantee is maintained by pre-allocating slots
    // and joining workers in sequence (scoped threads).
    let compose_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(total_frames)
        .max(1);
    // Batch size: how many frames we compose in parallel per round.
    let batch_size = compose_threads;

    let canonical_ref = &canonical;
    let clip_caches_ref = &clip_caches;

    let mut frame_idx = 0usize;
    while frame_idx < total_frames {
        let batch_end = (frame_idx + batch_size).min(total_frames);

        // Compose this batch in parallel using scoped threads.
        let composed: Vec<Vec<u8>> = std::thread::scope(|s| {
            let handles: Vec<_> = (frame_idx..batch_end)
                .map(|fi| {
                    s.spawn(move || {
                        let t = (fi as f32) / (fps as f32);
                        let mut canvas = RgbaImage::from_pixel(
                            out_w,
                            out_h,
                            Rgba(rgba_from_color(canonical_ref.output.background_color, 255)),
                        );
                        compose_frame(canonical_ref, assets_root, clip_caches_ref, t, &mut canvas);
                        flatten_to_opaque(&mut canvas, canonical_ref.output.background_color);
                        canvas.into_raw()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("compose thread panicked"))
                .collect()
        });

        // Write composed frames to encoder in order.
        for (i, pixels) in composed.into_iter().enumerate() {
            debug_assert_eq!(pixels.len(), frame_byte_count);
            let stdin = encoder
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("ffmpeg encoder stdin closed unexpectedly"))?;
            if let Err(e) = stdin.write_all(&pixels) {
                let stderr_msg = collect_drained_stderr(&stderr_handle);
                return Err(anyhow!(
                    "ffmpeg encoder stdin write failed at frame {}/{}: {}.\n\
                     Encoder stderr (last lines):\n{}",
                    frame_idx + i,
                    total_frames,
                    e,
                    stderr_msg
                ));
            }
            progress_cb(Progress::Frame {
                index: frame_idx + i + 1,
                total: total_frames,
            });
        }

        frame_idx = batch_end;
    }

    // Close stdin so the encoder flushes.
    drop(encoder.stdin.take());
    progress_cb(Progress::Stage {
        message: "Finalising MP4 (encoder flush)...".into(),
        percent: 90.0,
    });
    let status = encoder.wait().context("wait for ffmpeg encoder process")?;
    if !status.success() {
        let stderr_msg = collect_drained_stderr(&stderr_handle);
        return Err(anyhow!(
            "ffmpeg encoder exited with status {:?}.\n\
             Encoder stderr (last lines):\n{}",
            status.code(),
            stderr_msg
        ));
    }
    // Drop the stderr handle so the drain thread shuts down.
    drop(stderr_handle);

    // ── Stage 3: mux audio (best-effort) ──
    //
    // Was previously gated on `!canonical.audio.is_empty()`, which
    // skipped the audio mux entirely whenever the scene had no
    // explicit `AudioTrack` rows — even if every actor video clip
    // carried an embedded soundtrack the GUI preview was happily
    // playing. The legacy filtergraph plan now folds actor source
    // soundtracks into the audio mix when no AudioTrack already
    // covers the same source path (mirroring `app.rs::build_sources`
    // for the engine), so we always attempt the mux when at least
    // one audio source exists. `build_plan` returns `map_audio = None`
    // when nothing was produced, which `mux_audio` short-circuits
    // on cheaply.
    let scene_has_audio_source =
        !canonical.audio.is_empty() || canonical.actors.iter().any(|a| a.visible);
    if scene_has_audio_source {
        progress_cb(Progress::Stage {
            message: "Muxing audio tracks...".into(),
            percent: 95.0,
        });
        if let Err(e) = mux_audio(&canonical, assets_root, output_path) {
            warn!(error = %e, "audio mux failed — output has no sound");
            progress_cb(Progress::Stage {
                message: format!("Audio mux skipped ({})", e),
                percent: 98.0,
            });
        }
    }

    clip_caches.cleanup();
    progress_cb(Progress::Stage {
        message: "Render complete.".into(),
        percent: 100.0,
    });
    Ok(())
}

/// Render a single frame at time `t` to a PNG file. Used by the GUI
/// scrubber.
pub fn render_preview_frame_cpu(
    scene: &Scene,
    assets_root: &Path,
    t: f32,
    out_png: &Path,
) -> Result<()> {
    let canonical = canonicalise_scene(scene);
    let [out_w, out_h] = canonical.output.resolution;
    let fps = canonical.output.fps.max(1);

    let mut clip_caches: ClipCacheStore = ClipCacheStore::default();
    let mut noop = |_| {};
    extract_video_clips(&canonical, assets_root, fps, &mut clip_caches, &mut noop)?;

    let mut canvas = RgbaImage::from_pixel(
        out_w,
        out_h,
        Rgba(rgba_from_color(canonical.output.background_color, 255)),
    );
    compose_frame(&canonical, assets_root, &clip_caches, t, &mut canvas);
    flatten_to_opaque(&mut canvas, canonical.output.background_color);
    canvas
        .save(out_png)
        .with_context(|| format!("save preview to {}", out_png.display()))?;

    clip_caches.cleanup();
    Ok(())
}

// ─── SCENE CANONICALISATION ──────────────────────────────────────────

fn canonicalise_scene(scene: &Scene) -> Scene {
    // Same contract as the FFmpeg-path renderer: the render-frame's
    // resolution is the single source of truth for output dimensions.
    // The canvas preview's world-pixel math also keys off
    // `render_frame.resolution`, so syncing here keeps the two
    // pipelines pointing at the same reference rectangle.
    let mut out = scene.clone();
    out.output.resolution = out.render_frame.resolution;
    out
}

fn audio_actor_overlap(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let actor_end = actor.t_out.unwrap_or(scene.output.duration);
    let audio_start = audio.t_in;
    let audio_end = audio.t_out.unwrap_or(scene.output.duration);
    (actor_end.min(audio_end) - actor_start.max(audio_start)).max(0.0)
}

fn audio_actor_gap(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let actor_end = actor.t_out.unwrap_or(scene.output.duration);
    let audio_start = audio.t_in;
    let audio_end = audio.t_out.unwrap_or(scene.output.duration);
    if audio_end < actor_start {
        actor_start - audio_end
    } else if actor_end < audio_start {
        audio_start - actor_end
    } else {
        0.0
    }
}

fn audio_actor_score(
    scene: &Scene,
    actor: &memstroy_core::Actor,
    audio: &memstroy_core::AudioTrack,
) -> f32 {
    let actor_start = actor.t_in.unwrap_or(0.0);
    let source_penalty = if actor.source == audio.source {
        0.0
    } else {
        10_000.0
    };
    let overlap_penalty = if audio_actor_overlap(scene, actor, audio) > 0.0 {
        0.0
    } else {
        1_000.0 + audio_actor_gap(scene, actor, audio)
    };
    source_penalty
        + overlap_penalty
        + (actor_start - audio.t_in).abs()
        + (actor.source_start - audio.source_start).abs() * 2.0
}

fn best_audio_actor_by_id(
    scene: &Scene,
    actor_id: &str,
    audio: &memstroy_core::AudioTrack,
) -> Option<usize> {
    scene
        .actors
        .iter()
        .enumerate()
        .filter(|(_, actor)| actor.id == actor_id)
        .min_by(|(_, a), (_, b)| {
            audio_actor_score(scene, a, audio).total_cmp(&audio_actor_score(scene, b, audio))
        })
        .map(|(idx, _)| idx)
}

fn infer_actor_for_audio_in_scene(scene: &Scene, audio_idx: usize) -> Option<usize> {
    let audio = scene.audio.get(audio_idx)?;
    if audio.deleted {
        return None;
    }

    if let Some(parent_id) = audio.parent_actor.as_deref() {
        if let Some(idx) = best_audio_actor_by_id(scene, parent_id, audio) {
            return Some(idx);
        }
    }

    if let Some(actor_id) = audio.id.strip_suffix("_audio") {
        if let Some(idx) = best_audio_actor_by_id(scene, actor_id, audio) {
            return Some(idx);
        }
    }

    scene
        .actors
        .iter()
        .enumerate()
        .filter(|(_, actor)| actor.source == audio.source)
        .filter(|(_, actor)| audio_actor_overlap(scene, actor, audio) > 0.0)
        .min_by(|(_, a), (_, b)| {
            audio_actor_score(scene, a, audio).total_cmp(&audio_actor_score(scene, b, audio))
        })
        .map(|(idx, _)| idx)
}

// ─── COMPOSITOR ──────────────────────────────────────────────────────

fn compose_frame(
    scene: &Scene,
    assets_root: &Path,
    clip_caches: &ClipCacheStore,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let rf = &scene.render_frame;
    let [rw, rh] = rf.resolution;
    let rf_state = sample_render_frame_eased(rf, t);

    // ── Backgrounds ──
    for bg in &scene.backgrounds {
        if t < bg.start || t > bg.start + bg.duration {
            continue;
        }
        match &bg.source {
            MediaSource::SolidColor { color } => {
                paint_solid_background(canvas, *color);
            }
            MediaSource::Image { path } => {
                let resolved = resolve_path(assets_root, path);
                if let Ok(layer) = image::open(&resolved).map(|i| i.to_rgba8()) {
                    composite_fit(canvas, &layer, bg.fit, rw, rh);
                } else {
                    warn!(path = %resolved.display(), "missing/undecodable bg image");
                }
            }
            MediaSource::Video {
                path,
                r#loop,
                start_at,
            } => {
                let local = (t - bg.start).max(0.0) + *start_at;
                let resolved = resolve_path(assets_root, path);
                if let Some(layer) = clip_caches.frame_at(&resolved, local, *r#loop) {
                    composite_fit(canvas, &layer, bg.fit, rw, rh);
                } else {
                    warn!(path = %resolved.display(), "video bg frame unavailable");
                }
            }
        }
    }

    // ── Actors and overlays interleaved by z_order ──
    let mut paint_ops = build_paint_ops(scene);
    paint_ops.sort_by(|a, b| {
        a.z_order
            .cmp(&b.z_order)
            .then(a.secondary.cmp(&b.secondary))
            .then(a.scene_index.cmp(&b.scene_index))
    });

    for op in paint_ops {
        match op.kind {
            PaintOpKind::Actor(idx) => {
                paint_actor(
                    scene,
                    idx,
                    assets_root,
                    clip_caches,
                    &rf_state,
                    rw,
                    rh,
                    t,
                    canvas,
                );
            }
            PaintOpKind::OverlayImage(idx) => {
                paint_image_overlay(scene, idx, assets_root, &rf_state, rw, rh, t, canvas);
            }
            PaintOpKind::OverlayText(idx) => {
                paint_text_overlay(scene, idx, &rf_state, rw, rh, t, canvas);
            }
            PaintOpKind::OverlayVideo(idx) => {
                paint_video_overlay(
                    scene,
                    idx,
                    assets_root,
                    clip_caches,
                    &rf_state,
                    rw,
                    rh,
                    t,
                    canvas,
                );
            }
        }
    }

    // ── Effect layers — applied AFTER all content layers ──
    //
    // Effect layers operate on the already-composited canvas within
    // their bounding box. They are sorted by z_order so the user can
    // control the order of effect application.
    apply_effect_layers(scene, &rf_state, rw, rh, t, canvas);
}

#[derive(Debug, Clone, Copy)]
struct PaintOp {
    z_order: i32,
    secondary: i32,
    scene_index: usize,
    kind: PaintOpKind,
}

#[derive(Debug, Clone, Copy)]
enum PaintOpKind {
    Actor(usize),
    OverlayImage(usize),
    OverlayText(usize),
    OverlayVideo(usize),
}

fn build_paint_ops(scene: &Scene) -> Vec<PaintOp> {
    let any_explicit = scene.actors.iter().any(|a| a.z_order != 0)
        || scene.overlays.iter().any(|ov| match ov {
            Overlay::Text(t) => t.z_order != 0,
            Overlay::Image(i) => i.z_order != 0,
            Overlay::Video(v) => v.z_order != 0,
        });

    let mut ops = Vec::with_capacity(scene.actors.len() + scene.overlays.len());

    for (i, a) in scene.actors.iter().enumerate() {
        let z = if any_explicit { a.z_order } else { 100 };
        ops.push(PaintOp {
            z_order: z,
            secondary: 0,
            scene_index: i,
            kind: PaintOpKind::Actor(i),
        });
    }
    for (i, ov) in scene.overlays.iter().enumerate() {
        let (z, sec, kind) = if any_explicit {
            let z = match ov {
                Overlay::Text(t) => t.z_order,
                Overlay::Image(o) => o.z_order,
                Overlay::Video(v) => v.z_order,
            };
            let sec = match ov {
                Overlay::Text(t) => t.z_index,
                _ => 100,
            };
            let kind = match ov {
                Overlay::Text(_) => PaintOpKind::OverlayText(i),
                Overlay::Image(_) => PaintOpKind::OverlayImage(i),
                Overlay::Video(_) => PaintOpKind::OverlayVideo(i),
            };
            (z, sec, kind)
        } else {
            // Legacy fallback for scenes without GUI-stamped z_order:
            // keep non-text media overlays below actors. The live canvas
            // defaults image/video overlays to the lower visual lane; putting
            // them above actors here hides clips in full-frame snapshots.
            match ov {
                Overlay::Text(t) if t.behind_actors => (0, t.z_index, PaintOpKind::OverlayText(i)),
                Overlay::Text(t) => (200, t.z_index, PaintOpKind::OverlayText(i)),
                Overlay::Image(_) => (50, 100, PaintOpKind::OverlayImage(i)),
                Overlay::Video(_) => (50, 100, PaintOpKind::OverlayVideo(i)),
            }
        };
        ops.push(PaintOp {
            z_order: z,
            secondary: sec,
            scene_index: i,
            kind,
        });
    }
    ops
}

// ─── HELPERS — RENDER FRAME / WORLD ─────────────────────────────────

fn sample_render_frame_eased(rf: &RenderFrame, t: f32) -> RenderFrameState {
    let mut s = memstroy_core::sample_render_frame_layout(&rf.layout, &rf.animated_params, t);
    if rf.modifiers.is_empty() {
        return s;
    }
    let delta = keyframe::evaluate_modifiers(&rf.modifiers, t);
    s.pos.x += delta.dx;
    s.pos.y += delta.dy;
    s.rotation_deg += delta.d_rotation_deg;
    if delta.d_scale.abs() > 1e-4 {
        let mult = (1.0 + delta.d_scale).max(1.0e-3);
        s.zoom = (s.zoom / mult).max(1.0e-3);
    }
    s
}

/// World → output mapping. Mirrors `frame_snapshot::world_to_output`.
fn world_to_output(world: WorldPos, rf_state: &RenderFrameState, rw: u32, rh: u32) -> (f32, f32) {
    let dx = world.x - rf_state.pos.x;
    let dy = world.y - rf_state.pos.y;
    let theta = -rf_state.rotation_deg.to_radians();
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    let rx = dx * cos_t - dy * sin_t;
    let ry = dx * sin_t + dy * cos_t;
    let zoom = rf_state.zoom.max(1.0e-3);
    let ox = (rw as f32) * 0.5 + rx * zoom;
    let oy = (rh as f32) * 0.5 + ry * zoom;
    (ox, oy)
}

fn resolve_path(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

// ─── PAINT — ACTOR ──────────────────────────────────────────────────

fn paint_actor(
    scene: &Scene,
    actor_idx: usize,
    assets_root: &Path,
    clip_caches: &ClipCacheStore,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let actor = match scene.actors.get(actor_idx) {
        Some(a) => a,
        None => return,
    };
    if !actor.visible {
        return;
    }
    let scene_dur = scene.output.duration;
    let t_in = actor.t_in.unwrap_or(0.0);
    let t_out = actor.t_out.unwrap_or(scene_dur);
    if t < t_in || t > t_out {
        return;
    }

    // Sample state with modifiers — same recipe as canvas_preview's actor pass.
    let mut actor_state =
        memstroy_core::sample_actor_layout(&actor.layout, &actor.animated_params, t);
    let mod_delta = keyframe::evaluate_modifiers(&actor.modifiers, t - t_in);
    actor_state.scale = (actor_state.scale + mod_delta.d_scale).max(0.001);
    actor_state.rotation_deg += mod_delta.d_rotation_deg;

    // Pull the frame for this scene-time from the pre-extracted cache.
    let speed = actor.speed.max(1.0e-4);
    let local_t = if actor.mellstroy_footage.edge_frame {
        actor.source_start
    } else {
        (t - t_in) * speed + actor.source_start
    };
    let resolved = resolve_path(assets_root, &actor.source);
    let mut layer = match clip_caches.frame_at(&resolved, local_t, actor.loop_source) {
        Some(img) => img,
        None => return,
    };
    let src_w = layer.width();
    let src_h = layer.height();
    if src_w == 0 || src_h == 0 {
        return;
    }

    // Apply chromakey + colour correction in place.
    let cc_local_t = (t - t_in).max(0.0);
    let cc = actor.color_correction.sampled_at(cc_local_t);
    apply_chroma_and_cc(&mut layer, &actor.chroma_key, &cc);

    // Apply the per-element effect stack (blur, glow, mask, etc.)
    if !actor.effects.is_empty() {
        apply_effect_stack_rgba(&mut layer, &actor.effects, cc_local_t);
    }

    // World position + parent rotation/scale — mirrors canvas_preview.
    let world_pos = memstroy_core::element_world_pos(scene, &actor.id, t);
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    if let Some(pid) = actor.parent_id.as_ref() {
        let mut visited = vec![actor.id.clone()];
        if let Some(pxf) = memstroy_core::resolve_parent_transform(scene, pid, t, &mut visited) {
            memstroy_core::apply_parent_inheritance_actor(
                &mut actor_state.rotation_deg,
                &mut actor_state.scale,
                &mut actor_state.scale_y,
                &pxf,
            );
        }
    }

    let combined_x = if actor.flip_horizontal {
        -actor_state.flip_x_anim
    } else {
        actor_state.flip_x_anim
    };
    let combined_y = actor_state.flip_y_anim;
    let flip_x = combined_x < 0.0;
    let flip_y = combined_y < 0.0;
    let abs_fx = combined_x.abs().max(0.02);
    let abs_fy = combined_y.abs().max(0.02);

    let out_w = (src_w as f32) * actor_state.scale * abs_fx * rf_state.zoom;
    let out_h = (src_h as f32) * actor_state.scale * actor_state.scale_y * abs_fy * rf_state.zoom;
    let rotation_rad = (actor_state.rotation_deg - rf_state.rotation_deg).to_radians();

    paint_layer_rgba(
        canvas,
        &layer,
        cx,
        cy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        actor_state.opacity,
    );
}

// ─── PAINT — IMAGE OVERLAY ──────────────────────────────────────────

fn paint_image_overlay(
    scene: &Scene,
    overlay_idx: usize,
    assets_root: &Path,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let img_ov = match scene.overlays.get(overlay_idx) {
        Some(Overlay::Image(o)) => o,
        _ => return,
    };
    if t < img_ov.t_in || t > img_ov.t_out {
        return;
    }
    let sample_t = t - img_ov.t_in;
    let ov_ref = &scene.overlays[overlay_idx];
    let mut ov_state = memstroy_core::overlay_visual_state(
        scene,
        ov_ref,
        &img_ov.id,
        img_ov.parent_id.as_ref(),
        t,
        sample_t,
        &img_ov.modifiers,
    );
    let mod_delta = keyframe::evaluate_modifiers(&img_ov.modifiers, sample_t);

    let resolved = resolve_path(assets_root, &img_ov.source);
    let mut layer = match image::open(&resolved) {
        Ok(img) => img.to_rgba8(),
        Err(_) => {
            warn!(path = %resolved.display(), "missing image overlay source");
            return;
        }
    };
    if let Some(ck) = &img_ov.chroma_key {
        apply_chroma_and_cc(&mut layer, ck, &ColorCorrection::default());
    }
    // Apply the per-element effect stack.
    if !img_ov.effects.is_empty() {
        let local_t = t - img_ov.t_in;
        apply_effect_stack_rgba(&mut layer, &img_ov.effects, local_t);
    }
    let src_w = layer.width();
    let src_h = layer.height();
    if src_w == 0 || src_h == 0 {
        return;
    }

    let world_pos = memstroy_core::element_world_pos(scene, &img_ov.id, t);
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let out_w = (src_w as f32) * ov_state.scale * abs_fx * rf_state.zoom;
    let out_h = (src_h as f32) * ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom;
    let rotation_rad = (ov_state.rotation_deg - rf_state.rotation_deg).to_radians();
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;

    paint_layer_rgba(
        canvas,
        &layer,
        cx,
        cy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );
}

// ─── PAINT — VIDEO OVERLAY ──────────────────────────────────────────

fn paint_video_overlay(
    scene: &Scene,
    overlay_idx: usize,
    assets_root: &Path,
    clip_caches: &ClipCacheStore,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let vid = match scene.overlays.get(overlay_idx) {
        Some(Overlay::Video(v)) => v,
        _ => return,
    };
    if t < vid.t_in || t > vid.t_out {
        return;
    }
    let sample_t = t - vid.t_in;
    let ov_ref = &scene.overlays[overlay_idx];
    let mut ov_state = memstroy_core::overlay_visual_state(
        scene,
        ov_ref,
        &vid.id,
        vid.parent_id.as_ref(),
        t,
        sample_t,
        &vid.modifiers,
    );
    let mod_delta = keyframe::evaluate_modifiers(&vid.modifiers, sample_t);

    let speed = vid.speed.max(1.0e-4);
    let local_t = sample_t * speed + vid.source_start;
    let resolved = resolve_path(assets_root, &vid.source);
    let mut layer = match clip_caches.frame_at(&resolved, local_t, vid.loop_source) {
        Some(img) => img,
        None => return,
    };
    if let Some(ck) = &vid.chroma_key {
        apply_chroma_and_cc(&mut layer, ck, &ColorCorrection::default());
    }
    // Apply the per-element effect stack.
    if !vid.effects.is_empty() {
        apply_effect_stack_rgba(&mut layer, &vid.effects, sample_t);
    }
    let src_w = layer.width();
    let src_h = layer.height();
    if src_w == 0 || src_h == 0 {
        return;
    }

    let world_pos = memstroy_core::element_world_pos(scene, &vid.id, t);
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let out_w = (src_w as f32) * ov_state.scale * abs_fx * rf_state.zoom;
    let out_h = (src_h as f32) * ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom;
    let rotation_rad = (ov_state.rotation_deg - rf_state.rotation_deg).to_radians();
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;

    paint_layer_rgba(
        canvas,
        &layer,
        cx,
        cy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );
}

// ─── PAINT — TEXT OVERLAY ───────────────────────────────────────────

fn paint_text_overlay(
    scene: &Scene,
    overlay_idx: usize,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    let txt = match scene.overlays.get(overlay_idx) {
        Some(Overlay::Text(t)) => t,
        _ => return,
    };
    if t < txt.t_in || t > txt.t_out {
        return;
    }
    let sample_t = t - txt.t_in;
    let ov_ref = &scene.overlays[overlay_idx];
    let mut ov_state = memstroy_core::overlay_visual_state(
        scene,
        ov_ref,
        &txt.id,
        txt.parent_id.as_ref(),
        t,
        sample_t,
        &txt.modifiers,
    );
    let mod_delta = keyframe::evaluate_modifiers(&txt.modifiers, sample_t);

    let raster = match crate::text_rasterize::rasterize_text_overlay(txt, rw, rh) {
        Ok(Some(r)) => r,
        Ok(None) => return,
        Err(e) => {
            warn!(error = %e, "text rasterise failed");
            return;
        }
    };
    let mut layer = match image::open(&raster.png_path).map(|i| i.to_rgba8()) {
        Ok(img) => img,
        Err(_) => {
            let _ = std::fs::remove_file(&raster.png_path);
            return;
        }
    };
    // Apply the per-element effect stack to text overlays.
    if !txt.effects.is_empty() {
        apply_effect_stack_rgba(&mut layer, &txt.effects, sample_t);
    }
    let png_w = raster.width as f32;
    let png_h = raster.height as f32;
    if png_w < 1.0 || png_h < 1.0 {
        let _ = std::fs::remove_file(&raster.png_path);
        return;
    }

    let world_pos = memstroy_core::element_world_pos(scene, &txt.id, t);
    let world_pos = WorldPos {
        x: world_pos.x + mod_delta.dx,
        y: world_pos.y + mod_delta.dy,
    };
    let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

    let abs_fx = ov_state.flip_x_anim.abs().max(0.02);
    let abs_fy = ov_state.flip_y_anim.abs().max(0.02);
    let scale_x = ov_state.scale * abs_fx * rf_state.zoom;
    let scale_y = ov_state.scale * ov_state.scale_y * abs_fy * rf_state.zoom;
    let out_w = png_w * scale_x;
    let out_h = png_h * scale_y;

    let mut local_dx = raster.anchor_dx_from_left - png_w * 0.5;
    let mut local_dy = raster.anchor_dy_from_top - png_h * 0.5;
    let flip_x = ov_state.flip_x_anim < 0.0;
    let flip_y = ov_state.flip_y_anim < 0.0;
    if flip_x {
        local_dx = -local_dx;
    }
    if flip_y {
        local_dy = -local_dy;
    }
    let out_dx = local_dx * scale_x;
    let out_dy = local_dy * scale_y;
    let rotation_rad = (ov_state.rotation_deg - rf_state.rotation_deg).to_radians();
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();
    let rot_dx = out_dx * cos_r - out_dy * sin_r;
    let rot_dy = out_dx * sin_r + out_dy * cos_r;

    paint_layer_rgba(
        canvas,
        &layer,
        cx - rot_dx,
        cy - rot_dy,
        out_w,
        out_h,
        rotation_rad,
        flip_x,
        flip_y,
        ov_state.opacity,
    );

    let _ = std::fs::remove_file(&raster.png_path);
}

// ─── BACKGROUND HELPERS ─────────────────────────────────────────────

fn paint_solid_background(canvas: &mut RgbaImage, color: [u8; 3]) {
    for px in canvas.pixels_mut() {
        *px = Rgba([color[0], color[1], color[2], 255]);
    }
}

fn composite_fit(canvas: &mut RgbaImage, layer: &RgbaImage, fit: Fit, rw: u32, rh: u32) {
    if rw == 0 || rh == 0 {
        return;
    }
    let lw = layer.width().max(1);
    let lh = layer.height().max(1);
    let aspect_layer = lw as f32 / lh as f32;
    let aspect_canvas = rw as f32 / rh as f32;

    let (target_w, target_h) = match fit {
        Fit::Stretch => (rw as f32, rh as f32),
        Fit::Cover => {
            if aspect_layer > aspect_canvas {
                (rh as f32 * aspect_layer, rh as f32)
            } else {
                (rw as f32, rw as f32 / aspect_layer)
            }
        }
        Fit::Contain => {
            if aspect_layer > aspect_canvas {
                (rw as f32, rw as f32 / aspect_layer)
            } else {
                (rh as f32 * aspect_layer, rh as f32)
            }
        }
        Fit::Original => (lw as f32, lh as f32),
    };
    let cx = rw as f32 * 0.5;
    let cy = rh as f32 * 0.5;
    paint_layer_rgba(
        canvas, layer, cx, cy, target_w, target_h, 0.0, false, false, 1.0,
    );
}

// ─── CHROMAKEY + COLOR CORRECTION (FFmpeg-faithful) ─────────────────

fn apply_chroma_and_cc(layer: &mut RgbaImage, ck: &ChromaKeyParams, cc: &ColorCorrection) {
    let similarity = if ck.similarity.is_finite() {
        ck.similarity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = if ck.blend.is_finite() {
        ck.blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let spill = if ck.spill.is_finite() {
        ck.spill.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let chroma_active = ck.is_active();
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(ck.key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;

    let cc_active = !cc.is_identity();
    let gain = [
        cc.gain[0].max(0.0),
        cc.gain[1].max(0.0),
        cc.gain[2].max(0.0),
    ];
    let inv_gamma = [
        1.0 / cc.gamma[0].max(0.05),
        1.0 / cc.gamma[1].max(0.05),
        1.0 / cc.gamma[2].max(0.05),
    ];

    for px in layer.pixels_mut() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;

        // Match `video_cache::apply_effects_cpu` semantics exactly:
        // when chromakey is disabled, `alpha = 1.0` (treat the frame
        // as fully opaque). When active, the alpha is the per-pixel
        // keep factor derived from the BT.601 Cb/Cr distance.
        let alpha = if !chroma_active {
            1.0
        } else {
            let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
            let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
            let du = cb - key_cb;
            let dv = cr - key_cr;
            let diff = (du * du + dv * dv).sqrt() / dist_norm;
            if diff < similarity {
                0.0
            } else if blend > 0.0 && diff < similarity + blend {
                ((diff - similarity) / blend).clamp(0.0, 1.0)
            } else {
                1.0
            }
        };

        let (mut or_, mut og, mut ob) = (r, g, b);
        if chroma_active && alpha > 0.0 && spill > 0.0 && g > (r + b) * 0.5 {
            let avg_rb = (r + b) * 0.5;
            og = g - (g - avg_rb) * spill;
        }

        if cc_active {
            // brightness/contrast/saturation/temperature
            or_ = (or_ + cc.brightness * 255.0).clamp(0.0, 255.0);
            og = (og + cc.brightness * 255.0).clamp(0.0, 255.0);
            ob = (ob + cc.brightness * 255.0).clamp(0.0, 255.0);
            or_ = ((or_ - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
            og = ((og - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
            ob = ((ob - 128.0) * cc.contrast + 128.0).clamp(0.0, 255.0);
            let gray = 0.299 * or_ + 0.587 * og + 0.114 * ob;
            or_ = (gray + (or_ - gray) * cc.saturation).clamp(0.0, 255.0);
            og = (gray + (og - gray) * cc.saturation).clamp(0.0, 255.0);
            ob = (gray + (ob - gray) * cc.saturation).clamp(0.0, 255.0);
            if cc.temperature != 0.0 {
                or_ = (or_ + cc.temperature * 30.0).clamp(0.0, 255.0);
                ob = (ob - cc.temperature * 30.0).clamp(0.0, 255.0);
            }

            // LGG (lift / gain / gamma)
            let mut nr = or_ / 255.0;
            let mut ng = og / 255.0;
            let mut nb = ob / 255.0;
            nr = nr + cc.lift[0] * (1.0 - nr);
            ng = ng + cc.lift[1] * (1.0 - ng);
            nb = nb + cc.lift[2] * (1.0 - nb);
            nr = (nr * gain[0]).max(0.0);
            ng = (ng * gain[1]).max(0.0);
            nb = (nb * gain[2]).max(0.0);
            nr = nr.powf(inv_gamma[0]);
            ng = ng.powf(inv_gamma[1]);
            nb = nb.powf(inv_gamma[2]);
            or_ = (nr * 255.0).clamp(0.0, 255.0);
            og = (ng * 255.0).clamp(0.0, 255.0);
            ob = (nb * 255.0).clamp(0.0, 255.0);
        }

        let a_out = (alpha * 255.0).clamp(0.0, 255.0) as u8;
        px.0[0] = or_ as u8;
        px.0[1] = og as u8;
        px.0[2] = ob as u8;
        px.0[3] = a_out;
    }
}

#[inline]
fn rgb_to_cbcr_bt601(rgb: [u8; 3]) -> (f32, f32) {
    let r = rgb[0] as f32;
    let g = rgb[1] as f32;
    let b = rgb[2] as f32;
    let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
    let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
    (cb, cr)
}

// ─── EFFECT STACK (mirrors video_cache::apply_effect_stack_cpu) ─────

/// Apply the full effect stack to an `RgbaImage` layer in-place.
/// This is the render-time equivalent of the preview's
/// `apply_effect_stack_cpu` — it runs every enabled effect in
/// declared order so the exported MP4 matches what the canvas shows.
fn apply_effect_stack_rgba(layer: &mut RgbaImage, effects: &[Effect], t_local: f32) {
    for eff in effects {
        if !eff.enabled {
            continue;
        }
        let sampled = eff.sampled_at(t_local);
        let intensity = sampled.intensity.clamp(0.0, 1.0);
        if intensity <= 0.001 {
            continue;
        }
        apply_single_effect_rgba(layer, &sampled.kind, intensity);
    }
}

fn apply_single_effect_rgba(layer: &mut RgbaImage, kind: &EffectKind, intensity: f32) {
    match kind {
        EffectKind::Blur { radius } => fx_blur(layer, (*radius * intensity).round() as u32),
        EffectKind::Sharpen { amount } => fx_sharpen(layer, *amount * intensity),
        EffectKind::Grayscale => fx_grayscale(layer, intensity),
        EffectKind::Sepia => fx_sepia(layer, intensity),
        EffectKind::Invert => fx_invert(layer, intensity),
        EffectKind::HueShift { degrees } => fx_hue_shift(layer, *degrees * intensity),
        EffectKind::Vignette { strength } => {
            fx_vignette(layer, (*strength * intensity).clamp(0.0, 1.0))
        }
        EffectKind::Pixelate { block_size } => {
            fx_pixelate(layer, (*block_size).max(1.0) as u32, intensity)
        }
        EffectKind::Posterize { levels } => fx_posterize(layer, *levels, intensity),
        EffectKind::Glow {
            radius,
            intensity: i2,
        } => fx_glow(layer, *radius, *i2 * intensity),
        EffectKind::Brightness { amount } => fx_brightness(layer, *amount * intensity),
        EffectKind::Contrast { amount } => fx_contrast(layer, *amount * intensity),
        EffectKind::Saturation { amount } => fx_saturation(layer, *amount * intensity),
        EffectKind::EdgeDetect { threshold } => fx_edge_detect(layer, *threshold, intensity),
        EffectKind::MirrorH => fx_mirror_h(layer, intensity),
        EffectKind::MirrorV => fx_mirror_v(layer, intensity),
        EffectKind::ChromaticAberration { offset } => {
            fx_chromatic_aberration(layer, *offset * intensity)
        }
        EffectKind::Noise { amount } => fx_noise(layer, *amount * intensity),
        EffectKind::Wave {
            amplitude,
            wavelength,
        } => fx_wave(layer, *amplitude * intensity, *wavelength),
        EffectKind::OldFilm => fx_old_film(layer, intensity),
        EffectKind::Vhs => fx_vhs(layer, intensity),
        EffectKind::Glitch { strength } => fx_glitch(layer, *strength * intensity),
        EffectKind::Bloom { radius } => fx_bloom(layer, *radius, intensity),
        EffectKind::Crop {
            left,
            top,
            right,
            bottom,
        } => fx_crop_alpha(
            layer,
            (*left * intensity).clamp(0.0, 0.49),
            (*top * intensity).clamp(0.0, 0.49),
            (*right * intensity).clamp(0.0, 0.49),
            (*bottom * intensity).clamp(0.0, 0.49),
        ),
        EffectKind::Mask {
            shape,
            feather,
            invert,
        } => fx_mask(layer, shape, *feather, *invert, intensity),
        EffectKind::ColorKey {
            color,
            similarity,
            blend,
            spill,
            invert,
        } => fx_color_key(
            layer,
            *color,
            *similarity,
            *blend,
            *spill,
            *invert,
            intensity,
        ),
    }
}

// ── Individual effect implementations on RgbaImage ──

fn fx_blur(img: &mut RgbaImage, radius: u32) {
    if radius == 0 {
        return;
    }
    let r = radius.min(50);
    let blurred = image::imageops::blur(img, r as f32);
    *img = blurred;
}

fn fx_sharpen(img: &mut RgbaImage, amount: f32) {
    if amount.abs() < 0.001 {
        return;
    }
    let blurred = image::imageops::blur(img, 1.5);
    let w = img.width();
    let h = img.height();
    for y in 0..h {
        for x in 0..w {
            let orig = img.get_pixel(x, y).0;
            let blur = blurred.get_pixel(x, y).0;
            let mut out = [0u8; 4];
            for c in 0..3 {
                let diff = orig[c] as f32 - blur[c] as f32;
                out[c] = (orig[c] as f32 + diff * amount).clamp(0.0, 255.0) as u8;
            }
            out[3] = orig[3];
            img.put_pixel(x, y, Rgba(out));
        }
    }
}

fn fx_grayscale(img: &mut RgbaImage, intensity: f32) {
    for px in img.pixels_mut() {
        let g = (0.299 * px.0[0] as f32 + 0.587 * px.0[1] as f32 + 0.114 * px.0[2] as f32)
            .clamp(0.0, 255.0);
        px.0[0] = lerp_f32(px.0[0] as f32, g, intensity) as u8;
        px.0[1] = lerp_f32(px.0[1] as f32, g, intensity) as u8;
        px.0[2] = lerp_f32(px.0[2] as f32, g, intensity) as u8;
    }
}

fn fx_sepia(img: &mut RgbaImage, intensity: f32) {
    for px in img.pixels_mut() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        let sr = (0.393 * r + 0.769 * g + 0.189 * b).clamp(0.0, 255.0);
        let sg = (0.349 * r + 0.686 * g + 0.168 * b).clamp(0.0, 255.0);
        let sb = (0.272 * r + 0.534 * g + 0.131 * b).clamp(0.0, 255.0);
        px.0[0] = lerp_f32(r, sr, intensity) as u8;
        px.0[1] = lerp_f32(g, sg, intensity) as u8;
        px.0[2] = lerp_f32(b, sb, intensity) as u8;
    }
}

fn fx_invert(img: &mut RgbaImage, intensity: f32) {
    for px in img.pixels_mut() {
        px.0[0] = lerp_f32(px.0[0] as f32, 255.0 - px.0[0] as f32, intensity) as u8;
        px.0[1] = lerp_f32(px.0[1] as f32, 255.0 - px.0[1] as f32, intensity) as u8;
        px.0[2] = lerp_f32(px.0[2] as f32, 255.0 - px.0[2] as f32, intensity) as u8;
    }
}

fn fx_hue_shift(img: &mut RgbaImage, degrees: f32) {
    let theta = degrees.to_radians();
    let c = theta.cos();
    let s = theta.sin();
    let m00 = 0.213 + 0.787 * c - 0.213 * s;
    let m01 = 0.213 - 0.213 * c + 0.413 * s;
    let m02 = 0.213 - 0.213 * c - 0.787 * s;
    let m10 = 0.715 - 0.715 * c - 0.715 * s;
    let m11 = 0.715 + 0.285 * c + 0.140 * s;
    let m12 = 0.715 - 0.715 * c + 0.715 * s;
    let m20 = 0.072 - 0.072 * c + 0.928 * s;
    let m21 = 0.072 - 0.072 * c - 0.283 * s;
    let m22 = 0.072 + 0.928 * c + 0.072 * s;
    for px in img.pixels_mut() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        px.0[0] = (m00 * r + m10 * g + m20 * b).clamp(0.0, 255.0) as u8;
        px.0[1] = (m01 * r + m11 * g + m21 * b).clamp(0.0, 255.0) as u8;
        px.0[2] = (m02 * r + m12 * g + m22 * b).clamp(0.0, 255.0) as u8;
    }
}

fn fx_vignette(img: &mut RgbaImage, strength: f32) {
    let w = img.width() as f32;
    let h = img.height() as f32;
    let cx = w * 0.5;
    let cy = h * 0.5;
    let max_dist = (cx * cx + cy * cy).sqrt();
    for (x, y, px) in img.enumerate_pixels_mut() {
        let dx = x as f32 - cx;
        let dy = y as f32 - cy;
        let dist = (dx * dx + dy * dy).sqrt() / max_dist;
        let factor = 1.0 - (dist * strength).clamp(0.0, 1.0);
        px.0[0] = (px.0[0] as f32 * factor).clamp(0.0, 255.0) as u8;
        px.0[1] = (px.0[1] as f32 * factor).clamp(0.0, 255.0) as u8;
        px.0[2] = (px.0[2] as f32 * factor).clamp(0.0, 255.0) as u8;
    }
}

fn fx_pixelate(img: &mut RgbaImage, block_size: u32, intensity: f32) {
    let bs = block_size.max(1).min(img.width().min(img.height()));
    let effective_bs = ((bs as f32 * intensity).round() as u32).max(1);
    if effective_bs <= 1 {
        return;
    }
    let w = img.width();
    let h = img.height();
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let bw = effective_bs.min(w - x);
            let bh = effective_bs.min(h - y);
            let mut sr = 0u32;
            let mut sg = 0u32;
            let mut sb = 0u32;
            let count = bw * bh;
            for by in 0..bh {
                for bx in 0..bw {
                    let p = img.get_pixel(x + bx, y + by).0;
                    sr += p[0] as u32;
                    sg += p[1] as u32;
                    sb += p[2] as u32;
                }
            }
            let ar = (sr / count) as u8;
            let ag = (sg / count) as u8;
            let ab = (sb / count) as u8;
            for by in 0..bh {
                for bx in 0..bw {
                    let p = img.get_pixel_mut(x + bx, y + by);
                    p.0[0] = ar;
                    p.0[1] = ag;
                    p.0[2] = ab;
                }
            }
            x += effective_bs;
        }
        y += effective_bs;
    }
}

fn fx_posterize(img: &mut RgbaImage, levels: u32, intensity: f32) {
    let levels = levels.max(2).min(256);
    let step = 255.0 / (levels - 1) as f32;
    for px in img.pixels_mut() {
        for c in 0..3 {
            let v = px.0[c] as f32;
            let q = ((v / step).round() * step).clamp(0.0, 255.0);
            px.0[c] = lerp_f32(v, q, intensity) as u8;
        }
    }
}

fn fx_glow(img: &mut RgbaImage, radius: f32, intensity: f32) {
    if intensity < 0.001 {
        return;
    }
    let blurred = image::imageops::blur(img, radius.max(1.0).min(30.0));
    let w = img.width();
    let h = img.height();
    for y in 0..h {
        for x in 0..w {
            let orig = img.get_pixel(x, y).0;
            let glow = blurred.get_pixel(x, y).0;
            let mut out = [0u8; 4];
            for c in 0..3 {
                let added = orig[c] as f32 + glow[c] as f32 * intensity;
                out[c] = added.clamp(0.0, 255.0) as u8;
            }
            out[3] = orig[3];
            img.put_pixel(x, y, Rgba(out));
        }
    }
}

fn fx_brightness(img: &mut RgbaImage, amount: f32) {
    let add = amount * 255.0;
    for px in img.pixels_mut() {
        px.0[0] = (px.0[0] as f32 + add).clamp(0.0, 255.0) as u8;
        px.0[1] = (px.0[1] as f32 + add).clamp(0.0, 255.0) as u8;
        px.0[2] = (px.0[2] as f32 + add).clamp(0.0, 255.0) as u8;
    }
}

fn fx_contrast(img: &mut RgbaImage, amount: f32) {
    let factor = (1.0 + amount).max(0.0);
    for px in img.pixels_mut() {
        px.0[0] = ((px.0[0] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        px.0[1] = ((px.0[1] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
        px.0[2] = ((px.0[2] as f32 - 128.0) * factor + 128.0).clamp(0.0, 255.0) as u8;
    }
}

fn fx_saturation(img: &mut RgbaImage, amount: f32) {
    let factor = (1.0 + amount).max(0.0);
    for px in img.pixels_mut() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        px.0[0] = (gray + (r - gray) * factor).clamp(0.0, 255.0) as u8;
        px.0[1] = (gray + (g - gray) * factor).clamp(0.0, 255.0) as u8;
        px.0[2] = (gray + (b - gray) * factor).clamp(0.0, 255.0) as u8;
    }
}

fn fx_edge_detect(img: &mut RgbaImage, _threshold: f32, intensity: f32) {
    let w = img.width();
    let h = img.height();
    if w < 3 || h < 3 {
        return;
    }
    let orig = img.clone();
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let mut edge = 0.0f32;
            for c in 0..3 {
                let tl = orig.get_pixel(x - 1, y - 1).0[c] as f32;
                let t = orig.get_pixel(x, y - 1).0[c] as f32;
                let tr = orig.get_pixel(x + 1, y - 1).0[c] as f32;
                let l = orig.get_pixel(x - 1, y).0[c] as f32;
                let r = orig.get_pixel(x + 1, y).0[c] as f32;
                let bl = orig.get_pixel(x - 1, y + 1).0[c] as f32;
                let b = orig.get_pixel(x, y + 1).0[c] as f32;
                let br = orig.get_pixel(x + 1, y + 1).0[c] as f32;
                let gx = -tl - 2.0 * l - bl + tr + 2.0 * r + br;
                let gy = -tl - 2.0 * t - tr + bl + 2.0 * b + br;
                edge += (gx * gx + gy * gy).sqrt();
            }
            let e = (edge / 3.0).clamp(0.0, 255.0);
            let px = img.get_pixel_mut(x, y);
            let orig_px = orig.get_pixel(x, y).0;
            px.0[0] = lerp_f32(orig_px[0] as f32, e, intensity) as u8;
            px.0[1] = lerp_f32(orig_px[1] as f32, e, intensity) as u8;
            px.0[2] = lerp_f32(orig_px[2] as f32, e, intensity) as u8;
        }
    }
}

fn fx_mirror_h(img: &mut RgbaImage, intensity: f32) {
    if intensity < 0.5 {
        return;
    }
    image::imageops::flip_horizontal_in_place(img);
}

fn fx_mirror_v(img: &mut RgbaImage, intensity: f32) {
    if intensity < 0.5 {
        return;
    }
    image::imageops::flip_vertical_in_place(img);
}

fn fx_chromatic_aberration(img: &mut RgbaImage, offset: f32) {
    let off = offset.round() as i32;
    if off == 0 {
        return;
    }
    let w = img.width() as i32;
    let h = img.height() as i32;
    let orig = img.clone();
    for y in 0..h {
        for x in 0..w {
            let px = img.get_pixel_mut(x as u32, y as u32);
            // Shift red channel left, blue channel right
            let rx = (x - off).clamp(0, w - 1) as u32;
            let bx = (x + off).clamp(0, w - 1) as u32;
            px.0[0] = orig.get_pixel(rx, y as u32).0[0];
            px.0[2] = orig.get_pixel(bx, y as u32).0[2];
        }
    }
}

fn fx_noise(img: &mut RgbaImage, amount: f32) {
    if amount < 0.001 {
        return;
    }
    let sigma = amount * 50.0;
    // Simple deterministic noise based on pixel position (no rand crate needed)
    for (x, y, px) in img.enumerate_pixels_mut() {
        let seed = (x as u32)
            .wrapping_mul(1103515245)
            .wrapping_add(y as u32 * 12345);
        let noise_val = ((seed >> 16) as f32 / 32768.0 - 1.0) * sigma;
        px.0[0] = (px.0[0] as f32 + noise_val).clamp(0.0, 255.0) as u8;
        px.0[1] = (px.0[1] as f32 + noise_val).clamp(0.0, 255.0) as u8;
        px.0[2] = (px.0[2] as f32 + noise_val).clamp(0.0, 255.0) as u8;
    }
}

fn fx_wave(img: &mut RgbaImage, amplitude: f32, wavelength: f32) {
    if amplitude < 0.5 || wavelength < 1.0 {
        return;
    }
    let w = img.width();
    let h = img.height();
    let orig = img.clone();
    for y in 0..h {
        let offset =
            (amplitude * (2.0 * std::f32::consts::PI * y as f32 / wavelength).sin()).round() as i32;
        for x in 0..w {
            let sx = (x as i32 + offset).clamp(0, w as i32 - 1) as u32;
            *img.get_pixel_mut(x, y) = *orig.get_pixel(sx, y);
        }
    }
}

fn fx_old_film(img: &mut RgbaImage, intensity: f32) {
    fx_sepia(img, 0.6 * intensity);
    fx_vignette(img, 0.4 * intensity);
    fx_noise(img, 0.08 * intensity);
}

fn fx_vhs(img: &mut RgbaImage, intensity: f32) {
    fx_chromatic_aberration(img, 3.0 * intensity);
    // Slight desaturation
    fx_saturation(img, -0.3 * intensity);
}

fn fx_glitch(img: &mut RgbaImage, strength: f32) {
    if strength < 0.01 {
        return;
    }
    let w = img.width();
    let h = img.height();
    let block_h = (h as f32 * 0.05).max(2.0) as u32;
    let max_shift = (w as f32 * strength * 0.1).round() as i32;
    if max_shift == 0 {
        return;
    }
    let orig = img.clone();
    let mut seed = 42u32;
    let mut y = 0;
    while y < h {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let shift = ((seed >> 16) as i32 % (max_shift * 2 + 1)) - max_shift;
        let bh = block_h.min(h - y);
        for dy in 0..bh {
            for x in 0..w {
                let sx = (x as i32 + shift).clamp(0, w as i32 - 1) as u32;
                *img.get_pixel_mut(x, y + dy) = *orig.get_pixel(sx, y + dy);
            }
        }
        y += bh;
    }
}

fn fx_bloom(img: &mut RgbaImage, radius: f32, intensity: f32) {
    fx_glow(img, radius, intensity * 0.5);
}

fn fx_crop_alpha(img: &mut RgbaImage, left: f32, top: f32, right: f32, bottom: f32) {
    let w = img.width();
    let h = img.height();
    let lx = (left * w as f32).round() as u32;
    let ty = (top * h as f32).round() as u32;
    let rx = w.saturating_sub((right * w as f32).round() as u32);
    let by = h.saturating_sub((bottom * h as f32).round() as u32);
    for y in 0..h {
        for x in 0..w {
            if x < lx || x >= rx || y < ty || y >= by {
                let px = img.get_pixel_mut(x, y);
                px.0[3] = 0;
            }
        }
    }
}

fn fx_mask(img: &mut RgbaImage, shape: &MaskShape, feather: f32, invert: bool, intensity: f32) {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return;
    }
    let inv_w = 1.0 / w as f32;
    let inv_h = 1.0 / h as f32;
    for y in 0..h {
        let v = (y as f32 + 0.5) * inv_h;
        for x in 0..w {
            let u = (x as f32 + 0.5) * inv_w;
            let keep = if feather > 0.001 {
                let margin = shape.signed_margin_uv(u, v);
                let raw = (margin / feather + 0.5).clamp(0.0, 1.0);
                if invert {
                    1.0 - raw
                } else {
                    raw
                }
            } else {
                let inside = shape.contains_uv(u, v);
                if invert {
                    if inside {
                        0.0
                    } else {
                        1.0
                    }
                } else {
                    if inside {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            let px = img.get_pixel_mut(x, y);
            let orig_a = px.0[3] as f32;
            let target_a = orig_a * keep;
            px.0[3] = (orig_a + (target_a - orig_a) * intensity).clamp(0.0, 255.0) as u8;
        }
    }
}

fn fx_color_key(
    img: &mut RgbaImage,
    key_color: [u8; 3],
    similarity: f32,
    blend: f32,
    _spill: f32,
    invert: bool,
    intensity: f32,
) {
    let similarity = if similarity.is_finite() {
        similarity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let blend = if blend.is_finite() {
        blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if similarity < 1.0e-5 && !invert {
        return;
    }
    let (key_cb, key_cr) = rgb_to_cbcr_bt601(key_color);
    let dist_norm = 255.0 * std::f32::consts::SQRT_2;
    for px in img.pixels_mut() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        let cb = -0.169 * r - 0.331 * g + 0.500 * b + 128.0;
        let cr = 0.500 * r - 0.419 * g - 0.081 * b + 128.0;
        let du = cb - key_cb;
        let dv = cr - key_cr;
        let diff = (du * du + dv * dv).sqrt() / dist_norm;
        let mut alpha_keep = if diff < similarity {
            0.0
        } else if blend > 0.0 && diff < similarity + blend {
            ((diff - similarity) / blend).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if invert {
            alpha_keep = 1.0 - alpha_keep;
        }
        let orig_a = px.0[3] as f32;
        let target_a = orig_a * alpha_keep;
        px.0[3] = (orig_a + (target_a - orig_a) * intensity).clamp(0.0, 255.0) as u8;
    }
}

#[inline]
fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ─── LAYER COMPOSITOR (inverse-mapping affine + bilinear) ──────────

fn paint_layer_rgba(
    canvas: &mut RgbaImage,
    layer: &RgbaImage,
    cx: f32,
    cy: f32,
    out_w: f32,
    out_h: f32,
    rotation_rad: f32,
    flip_x: bool,
    flip_y: bool,
    opacity: f32,
) {
    let lw = layer.width();
    let lh = layer.height();
    if lw == 0 || lh == 0 || out_w < 0.5 || out_h < 0.5 || opacity <= 1.0e-3 {
        return;
    }
    let cw = canvas.width() as i32;
    let ch = canvas.height() as i32;
    if cw == 0 || ch == 0 {
        return;
    }

    let half_w = out_w * 0.5;
    let half_h = out_h * 0.5;
    let cos_r = rotation_rad.cos();
    let sin_r = rotation_rad.sin();

    let extent_x = half_w * cos_r.abs() + half_h * sin_r.abs();
    let extent_y = half_w * sin_r.abs() + half_h * cos_r.abs();
    let x_min = ((cx - extent_x).floor() as i32).max(0);
    let x_max = ((cx + extent_x).ceil() as i32).min(cw - 1);
    let y_min = ((cy - extent_y).floor() as i32).max(0);
    let y_max = ((cy + extent_y).ceil() as i32).min(ch - 1);
    if x_min > x_max || y_min > y_max {
        return;
    }

    let inv_half_w = 1.0 / half_w;
    let inv_half_h = 1.0 / half_h;
    let lw_f = lw as f32;
    let lh_f = lh as f32;
    let opacity = opacity.clamp(0.0, 1.0);

    for y in y_min..=y_max {
        for x in x_min..=x_max {
            let dx = (x as f32 + 0.5) - cx;
            let dy = (y as f32 + 0.5) - cy;
            let lx = dx * cos_r + dy * sin_r;
            let ly = -dx * sin_r + dy * cos_r;
            let mut nx = lx * inv_half_w;
            let mut ny = ly * inv_half_h;
            if flip_x {
                nx = -nx;
            }
            if flip_y {
                ny = -ny;
            }
            if nx < -1.0 || nx > 1.0 || ny < -1.0 || ny > 1.0 {
                continue;
            }
            let u = (nx + 1.0) * 0.5;
            let v = (ny + 1.0) * 0.5;
            let src_x = u * lw_f - 0.5;
            let src_y = v * lh_f - 0.5;
            let sample = sample_bilinear(layer, src_x, src_y);
            let src_a = (sample[3] as f32) * opacity;
            if src_a < 0.5 {
                continue;
            }
            let p = canvas.get_pixel_mut(x as u32, y as u32);
            let dst_a = p[3] as f32;
            let src_a_n = src_a / 255.0;
            let inv_n = 1.0 - src_a_n;
            let out_a = src_a + dst_a * inv_n;
            if out_a < 1.0e-3 {
                continue;
            }
            let out_r = (sample[0] as f32 * src_a_n * 255.0 + p[0] as f32 * dst_a * inv_n) / out_a;
            let out_g = (sample[1] as f32 * src_a_n * 255.0 + p[1] as f32 * dst_a * inv_n) / out_a;
            let out_b = (sample[2] as f32 * src_a_n * 255.0 + p[2] as f32 * dst_a * inv_n) / out_a;
            *p = Rgba([
                out_r.clamp(0.0, 255.0) as u8,
                out_g.clamp(0.0, 255.0) as u8,
                out_b.clamp(0.0, 255.0) as u8,
                out_a.clamp(0.0, 255.0) as u8,
            ]);
        }
    }
}

fn sample_bilinear(img: &RgbaImage, x: f32, y: f32) -> [u8; 4] {
    let w = img.width() as i32;
    let h = img.height() as i32;
    if w == 0 || h == 0 {
        return [0, 0, 0, 0];
    }
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = x0 + 1;
    let y1 = y0 + 1;
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let get = |xx: i32, yy: i32| -> [f32; 4] {
        let cx = xx.clamp(0, w - 1) as u32;
        let cy = yy.clamp(0, h - 1) as u32;
        let p = img.get_pixel(cx, cy).0;
        [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
    };
    let p00 = get(x0, y0);
    let p10 = get(x1, y0);
    let p01 = get(x0, y1);
    let p11 = get(x1, y1);
    let w00 = (1.0 - fx) * (1.0 - fy);
    let w10 = fx * (1.0 - fy);
    let w01 = (1.0 - fx) * fy;
    let w11 = fx * fy;
    let blend = |i: usize| -> u8 {
        (p00[i] * w00 + p10[i] * w10 + p01[i] * w01 + p11[i] * w11).clamp(0.0, 255.0) as u8
    };
    [blend(0), blend(1), blend(2), blend(3)]
}

fn flatten_to_opaque(canvas: &mut RgbaImage, bg_color: [u8; 3]) {
    for px in canvas.pixels_mut() {
        let a = px.0[3] as f32 / 255.0;
        if a >= 0.999 {
            px.0[3] = 255;
            continue;
        }
        let r = px.0[0] as f32 * a + bg_color[0] as f32 * (1.0 - a);
        let g = px.0[1] as f32 * a + bg_color[1] as f32 * (1.0 - a);
        let b = px.0[2] as f32 * a + bg_color[2] as f32 * (1.0 - a);
        px.0[0] = r.clamp(0.0, 255.0) as u8;
        px.0[1] = g.clamp(0.0, 255.0) as u8;
        px.0[2] = b.clamp(0.0, 255.0) as u8;
        px.0[3] = 255;
    }
}

#[inline]
fn rgba_from_color(c: [u8; 3], a: u8) -> [u8; 4] {
    [c[0], c[1], c[2], a]
}

// ─── EFFECT LAYERS ──────────────────────────────────────────────────

/// Apply all active effect layers to the canvas. Each effect layer
/// defines a spatial region; pixels within that region are extracted,
/// the effect stack is applied, and the result is written back.
///
/// Effect layers are sorted by `z_order` so the user can control
/// application order. Layers listed in an effect layer's `exclude_ids`
/// are NOT handled here (exclusion is a future enhancement that would
/// require a multi-pass compositor with per-layer buffers).
fn apply_effect_layers(
    scene: &Scene,
    rf_state: &RenderFrameState,
    rw: u32,
    rh: u32,
    t: f32,
    canvas: &mut RgbaImage,
) {
    if scene.effect_layers.is_empty() {
        return;
    }

    // Sort by z_order (lower = applied first).
    let mut sorted: Vec<(i32, usize)> = scene
        .effect_layers
        .iter()
        .enumerate()
        .map(|(i, e)| (e.z_order, i))
        .collect();
    sorted.sort_by_key(|&(z, i)| (z, i));

    for (_, idx) in sorted {
        let fx_ov = &scene.effect_layers[idx];
        if t < fx_ov.t_in || t > fx_ov.t_out {
            continue;
        }
        if fx_ov.effects.is_empty() {
            continue;
        }
        let sample_t = t - fx_ov.t_in;
        let mut ov_state =
            memstroy_core::sample_overlay_layout(&fx_ov.layout, &fx_ov.animated_params, sample_t);
        let mod_delta = keyframe::evaluate_modifiers(&fx_ov.modifiers, sample_t);
        ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
        ov_state.rotation_deg += mod_delta.d_rotation_deg;

        // Effect layer bounding box. The default "intrinsic size" is
        // 200×200 world pixels — the user resizes via scale on the
        // canvas. This matches how image overlays use src_w × scale.
        let base_size = 200.0_f32;
        let world_w = base_size * ov_state.scale;
        let world_h = base_size * ov_state.scale * ov_state.scale_y;

        let world_pos = memstroy_core::element_world_pos(scene, &fx_ov.id, t);
        let world_pos = memstroy_core::canvas::WorldPos {
            x: world_pos.x + mod_delta.dx,
            y: world_pos.y + mod_delta.dy,
        };
        let (cx, cy) = world_to_output(world_pos, rf_state, rw, rh);

        let half_w = world_w * rf_state.zoom * 0.5;
        let half_h = world_h * rf_state.zoom * 0.5;
        let x_min = ((cx - half_w).floor() as i32).max(0) as u32;
        let y_min = ((cy - half_h).floor() as i32).max(0) as u32;
        let x_max = ((cx + half_w).ceil() as i32).min(rw as i32 - 1).max(0) as u32;
        let y_max = ((cy + half_h).ceil() as i32).min(rh as i32 - 1).max(0) as u32;

        let region_w = x_max.saturating_sub(x_min) + 1;
        let region_h = y_max.saturating_sub(y_min) + 1;
        if region_w == 0 || region_h == 0 {
            continue;
        }

        // Extract the region from the canvas.
        let mut region = RgbaImage::new(region_w, region_h);
        for y in 0..region_h {
            for x in 0..region_w {
                let px = canvas.get_pixel(x_min + x, y_min + y);
                region.put_pixel(x, y, *px);
            }
        }

        // Apply the effect stack.
        apply_effect_stack_rgba(&mut region, &fx_ov.effects, sample_t);

        // Write back.
        for y in 0..region_h {
            for x in 0..region_w {
                let px = region.get_pixel(x, y);
                canvas.put_pixel(x_min + x, y_min + y, *px);
            }
        }
    }
}

// ─── CLIP CACHE — FFMPEG-EXTRACTED FRAMES ───────────────────────────

/// Shared (path → cache directory + metadata) store used during a
/// single render pass. Frames are extracted lazily *per source* into
/// per-source temp directories; on Drop / `cleanup` every directory
/// is removed best-effort.
#[derive(Default)]
pub(crate) struct ClipCacheStore {
    caches: std::collections::HashMap<PathBuf, ClipCacheEntry>,
}

struct ClipCacheEntry {
    cache_dir: PathBuf,
    fps: u32,
    /// Number of frames ACTUALLY in `cache_dir`. The first frame is
    /// always `000001.jpg`; the last is `cache_dir/{frame_count:06}.jpg`.
    frame_count: usize,
    /// Absolute scene-time of the first cached frame. The cache only
    /// holds frames inside the source's `[seek_start, seek_end]`
    /// window; values outside this clamp to first / last.
    seek_start: f32,
}

impl ClipCacheStore {
    /// Look up a frame for the source at `path`, given a clip-local
    /// time `local_t`. Returns `None` only when the cache entry is
    /// missing or the clip is empty; otherwise clamps to the nearest
    /// extracted frame.
    fn frame_at(&self, path: &Path, local_t: f32, looping: bool) -> Option<RgbaImage> {
        let entry = self.caches.get(path)?;
        if entry.frame_count == 0 {
            return None;
        }
        let extracted_span = (entry.frame_count as f32) / (entry.fps as f32);
        let mut local = local_t;
        if looping && extracted_span > 1.0e-3 {
            // We may have only extracted a window of the looping
            // source. Wrap on the EXTRACTED span, since beyond that
            // we'd just see the wrap-around again. This works
            // because for looping sources we always extract from
            // `seek_start = 0`.
            local = local.rem_euclid(extracted_span);
        }
        if local < entry.seek_start {
            local = entry.seek_start;
        }
        let rel = (local - entry.seek_start).max(0.0);
        let frame_idx =
            ((rel * entry.fps as f32).floor() as usize).min(entry.frame_count.saturating_sub(1));
        let p = entry.cache_dir.join(format!("{:06}.jpg", frame_idx + 1));
        match image::open(&p) {
            Ok(img) => Some(img.to_rgba8()),
            Err(e) => {
                warn!(
                    path = %p.display(),
                    error = %e,
                    "failed to load cached video frame"
                );
                None
            }
        }
    }

    fn cleanup(self) {
        for (_, e) in self.caches {
            let _ = std::fs::remove_dir_all(&e.cache_dir);
        }
    }
}

/// Build the list of (source path → time-window we'll need) from
/// every actor / overlay / background, deduplicating by source path.
/// Returns the windows in `[start, end]` scene-time relative to the
/// SOURCE clip (post-`source_start`, pre-loop).
fn collect_source_windows(
    scene: &Scene,
    assets_root: &Path,
) -> std::collections::HashMap<PathBuf, (f32, f32, bool)> {
    use std::collections::HashMap;
    let mut by_path: HashMap<PathBuf, (f32, f32, bool)> = HashMap::new();
    let scene_dur = scene.output.duration;

    let mut add = |path: &Path, src_start: f32, span: f32, speed: f32, loop_source: bool| {
        let speed = speed.max(1.0e-4);
        let lo = src_start.max(0.0);
        let hi = (src_start + span * speed).max(lo + 0.1);
        let resolved = resolve_path(assets_root, path);
        let entry = by_path.entry(resolved).or_insert((lo, hi, loop_source));
        entry.0 = entry.0.min(lo);
        entry.1 = entry.1.max(hi);
        entry.2 |= loop_source;
    };

    for a in &scene.actors {
        if !a.visible {
            continue;
        }
        let t_in = a.t_in.unwrap_or(0.0);
        let t_out = a.t_out.unwrap_or(scene_dur);
        let span = (t_out - t_in).max(0.1);
        add(&a.source, a.source_start, span, a.speed, a.loop_source);
    }
    for ov in &scene.overlays {
        if let Overlay::Video(v) = ov {
            let span = (v.t_out - v.t_in).max(0.1);
            add(&v.source, v.source_start, span, v.speed, v.loop_source);
        }
    }
    for bg in &scene.backgrounds {
        if let MediaSource::Video {
            path,
            r#loop,
            start_at,
        } = &bg.source
        {
            add(path, *start_at, bg.duration.max(0.1), 1.0, *r#loop);
        }
    }
    by_path
}

fn extract_video_clips<F>(
    scene: &Scene,
    assets_root: &Path,
    fps: u32,
    store: &mut ClipCacheStore,
    progress_cb: &mut F,
) -> Result<()>
where
    F: FnMut(Progress),
{
    let windows = collect_source_windows(scene, assets_root);
    if windows.is_empty() {
        return Ok(());
    }
    let total_sources = windows.len();

    progress_cb(Progress::Stage {
        message: format!("Extracting frames from {} source(s)...", total_sources),
        percent: 0.0,
    });

    // ── Parallel extraction ──
    //
    // Each source is extracted by its own ffmpeg process. We spawn up
    // to `max_parallel` processes concurrently so multi-source scenes
    // (7+ actors) finish in a fraction of the sequential time. The
    // bottleneck is disk I/O and ffmpeg's internal decode, both of
    // which benefit from overlapping across independent sources.
    let max_parallel = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(total_sources)
        .max(1);

    // Prepare extraction jobs.
    struct ExtractionJob {
        src: PathBuf,
        cache_dir: PathBuf,
        seek_start: f32,
        span: f32,
        fps: u32,
    }
    let scene_dur = scene.output.duration.max(0.1);
    let mut jobs: Vec<ExtractionJob> = Vec::with_capacity(total_sources);
    for (src, (lo, hi, looping)) in &windows {
        if !src.exists() {
            warn!(path = %src.display(), "video source missing — skipping");
            continue;
        }
        let cache_dir = make_temp_dir("memstroy-cpu-cache")?;
        let probed_duration = probe_duration(src).unwrap_or(0.0);
        let pad = 1.0 / fps as f32;
        let (seek_start, span) = if *looping && probed_duration > 1.0e-3 {
            let span = probed_duration.min(scene_dur + pad);
            (0.0_f32, span)
        } else {
            let lo = (lo - pad).max(0.0);
            let hi_clamped = if probed_duration > 1.0e-3 {
                hi.min(probed_duration)
            } else {
                *hi
            };
            let span = (hi_clamped - lo + 2.0 * pad).max(1.0 / fps as f32);
            (lo, span)
        };
        jobs.push(ExtractionJob {
            src: src.clone(),
            cache_dir,
            seek_start,
            span,
            fps,
        });
    }

    // Run extraction in parallel batches.
    let extracted_count = Arc::new(AtomicUsize::new(0));
    let results: Vec<Result<(PathBuf, PathBuf, f32, usize)>> = {
        let chunks: Vec<&[ExtractionJob]> = jobs.chunks(max_parallel).collect();
        let mut all_results = Vec::with_capacity(jobs.len());
        for chunk in chunks {
            let handles: Vec<_> = chunk
                .iter()
                .map(|job| {
                    let src = job.src.clone();
                    let cache_dir = job.cache_dir.clone();
                    let seek_start = job.seek_start;
                    let span = job.span;
                    let fps = job.fps;
                    std::thread::spawn(move || {
                        let frames = extract_frames(&src, fps, seek_start, span, &cache_dir)?;
                        Ok((src, cache_dir, seek_start, frames))
                    })
                })
                .collect();
            for handle in handles {
                let result = handle
                    .join()
                    .map_err(|_| anyhow!("extraction thread panicked"))?;
                all_results.push(result);
                let done = extracted_count.fetch_add(1, Ordering::Relaxed) + 1;
                progress_cb(Progress::Stage {
                    message: format!("Extracted {}/{} sources", done, total_sources),
                    percent: (done as f32 / total_sources as f32) * 5.0,
                });
            }
        }
        all_results
    };

    for result in results {
        match result {
            Ok((src, cache_dir, seek_start, frames)) => {
                info!(
                    src = %src.display(),
                    frames,
                    seek_start,
                    cache = %cache_dir.display(),
                    "extracted clip frames"
                );
                store.caches.insert(
                    src,
                    ClipCacheEntry {
                        cache_dir,
                        fps,
                        frame_count: frames,
                        seek_start,
                    },
                );
            }
            Err(e) => {
                warn!(error = %e, "frame extraction failed for a source — skipping");
            }
        }
    }
    Ok(())
}

fn make_temp_dir(prefix: &str) -> Result<PathBuf> {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("{}-{}-{}-{}", prefix, pid, nanos, counter));
    std::fs::create_dir_all(&dir).context("create temp dir for clip cache")?;
    Ok(dir)
}

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn extract_frames(
    src: &Path,
    fps: u32,
    seek_start: f32,
    span: f32,
    out_dir: &Path,
) -> Result<usize> {
    let bin = crate::ffmpeg_binary();
    let mut cmd = Command::new(&bin);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
    if seek_start > 1.0e-3 {
        cmd.args(["-ss", &format!("{:.3}", seek_start)]);
    }
    cmd.args(["-i"]).arg(src);
    if span > 0.0 {
        cmd.args(["-t", &format!("{:.3}", span)]);
    }
    // Frames are extracted at the SAME downscaled resolution the GUI's
    // `FrameCache` uses (`scale=480:-1`). This is the size the canvas
    // preview reads as `source_width`/`source_height`, and every
    // `actor.scale` value the user authored on the canvas was sized
    // against this 480-px-wide reference. If we extract at native
    // resolution instead, the rendered output would show every actor
    // ~2.25× too large compared to the canvas (`1080 / 480`) — which
    // is exactly the user-reported "rendered output doesn't look like
    // what's inside the render frame on the canvas" bug.
    //
    // Quality is fine because:
    //   * the canvas already locks the user's effective resolution at
    //     480 wide, so any `actor.scale` they dialed in is calibrated
    //     for that size,
    //   * the output frame is up-scaled into the render-frame's
    //     1080×1920 (or whatever) rectangle via bilinear sampling,
    //     which preserves the subjective look of the canvas.
    cmd.args(["-vf", &format!("fps={},scale=480:-1", fps), "-q:v", "3"])
        .arg(out_dir.join("%06d.jpg"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    proc::hide_console_std(&mut cmd);

    let output = cmd.output().context("spawn ffmpeg for frame extraction")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "ffmpeg frame extraction failed for {}:\n{}",
            src.display(),
            stderr.lines().take(20).collect::<Vec<_>>().join("\n")
        ));
    }

    let mut n = 0;
    for entry in std::fs::read_dir(out_dir).context("read clip cache dir")? {
        let entry = entry?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            n += 1;
        }
    }
    Ok(n)
}

fn probe_duration(src: &Path) -> Option<f32> {
    let probe = crate::ffprobe_binary();
    let mut cmd = Command::new(&probe);
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ])
    .arg(src)
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    proc::hide_console_std(&mut cmd);
    let out = cmd.output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<f32>().ok()
}

// ─── ENCODER (raw RGBA → MP4 via ffmpeg stdin) ──────────────────────

fn spawn_encoder(w: u32, h: u32, fps: u32, output_path: &Path) -> Result<std::process::Child> {
    let bin = crate::ffmpeg_binary();
    let mut cmd = Command::new(&bin);
    // Determine thread count for x264. Use all available cores.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "0".to_string());
    cmd.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        &format!("{}x{}", w, h),
        "-r",
        &fps.to_string(),
        "-thread_queue_size",
        "512",
        "-i",
        "-",
        "-c:v",
        "libx264",
        // ── Encoder speed/quality trade-off ──
        //
        // `faster` + `crf=20` roughly halves encode wall-clock vs
        // `medium` + `crf=19` while keeping perceptual quality
        // essentially identical for short-form overlay-heavy footage.
        // A +1 CRF step is below the visible-difference threshold;
        // the bitrate goes up ~10% to compensate but the time savings
        // are far larger.
        "-preset",
        "faster",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        // Use all CPU cores for x264's internal threading.
        "-threads",
        &threads,
        // Tune for fast-cut / animated content: disables psy-rdo
        // tweaks tuned for dark grain, saves 10-15% encode time.
        "-tune",
        "fastdecode",
        "-movflags",
        "+faststart",
    ])
    .arg(output_path)
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    // Capture stderr so we can include it in error messages when the
    // encoder dies mid-stream. Without this, a stdin write failure
    // surfaced as a generic "broken pipe" with no actionable detail.
    .stderr(Stdio::piped());
    proc::hide_console_std(&mut cmd);
    cmd.spawn().context("spawn ffmpeg encoder")
}

/// Handle to a background thread reading the encoder's stderr into a
/// shared `Mutex<String>`. Dropping the handle does NOT kill the
/// thread — it just lets the caller stop holding a reference. The
/// thread terminates naturally when the encoder closes its stderr
/// pipe (i.e. the encoder process exits).
struct StderrDrainHandle {
    buffer: Arc<std::sync::Mutex<String>>,
    _join: std::thread::JoinHandle<()>,
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr) -> StderrDrainHandle {
    use std::io::Read;
    let buffer = Arc::new(std::sync::Mutex::new(String::with_capacity(8 * 1024)));
    let buffer_inner = buffer.clone();
    let join = std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&chunk[..n]).to_string();
                    if let Ok(mut buf) = buffer_inner.lock() {
                        buf.push_str(&s);
                        // Cap the buffer so a chatty encoder doesn't
                        // grow it unboundedly. We keep the LAST 32 KB
                        // since recent lines are the most useful for
                        // diagnostics.
                        const MAX: usize = 32 * 1024;
                        if buf.len() > MAX {
                            let drop_n = buf.len() - MAX;
                            buf.drain(..drop_n);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    StderrDrainHandle {
        buffer,
        _join: join,
    }
}

/// Snapshot the current contents of the drained stderr buffer for
/// inclusion in error messages. Returns the LAST few non-empty lines,
/// since FFmpeg prints the actual error message right before exiting.
fn collect_drained_stderr(handle: &Option<StderrDrainHandle>) -> String {
    let Some(h) = handle else {
        return "(stderr unavailable)".into();
    };
    let Ok(buf) = h.buffer.lock() else {
        return "(stderr lock poisoned)".into();
    };
    let lines: Vec<&str> = buf.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail: Vec<&str> = lines.iter().rev().take(15).copied().collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

// ─── AUDIO MUX (delegate to FFmpeg-path renderer) ───────────────────

/// Decompose an arbitrary tempo factor into a chain of `atempo=…`
/// filters whose product equals the factor. ffmpeg's `atempo` filter
/// requires its argument in [0.5, 2.0]; values outside that range
/// have to be split across multiple filter invocations.
///
/// E.g. tempo=4.0 → ["atempo=2.0", "atempo=2.0"], tempo=0.25 →
/// ["atempo=0.5", "atempo=0.5"]. Tempo=1.0 returns an empty Vec so
/// callers can skip emitting any filter at all in the common case.
#[allow(dead_code)]
fn atempo_chain(mut tempo: f32) -> Vec<String> {
    let mut out = Vec::new();
    if (tempo - 1.0).abs() < 1e-4 {
        return out;
    }
    while tempo > 2.0 + 1e-4 {
        out.push("atempo=2.0".to_string());
        tempo /= 2.0;
    }
    while tempo < 0.5 - 1e-4 {
        out.push("atempo=0.5".to_string());
        tempo *= 2.0;
    }
    if (tempo - 1.0).abs() > 1e-4 {
        out.push(format!("atempo={:.6}", tempo.clamp(0.5, 2.0)));
    }
    out
}

fn mux_audio(scene: &Scene, assets_root: &Path, output_path: &Path) -> Result<()> {
    let temp_audio = output_path.with_extension("audio.m4a");

    // ── Audio-only filter graph ──
    //
    // Earlier we delegated to `crate::plan::build_plan` and ran the
    // whole video+audio plan with `-vn` to skip the video output.
    // That worked when the scene was simple but had two failure
    // modes that surfaced as "video has no sound":
    //
    //   1. The full plan added every video / image / overlay input
    //      to the cmdline. `-loop 1` on an image input requires a
    //      paired `-t`, which the audio sub-call didn't supply, so
    //      ffmpeg hung waiting for the looped image to terminate
    //      (it never does without `-t`), the parent gave up after
    //      the next stage, and the audio file was never written.
    //   2. The video filter chain references generated `[base]`
    //      labels that depend on side-tables (e.g. mask PNG exports)
    //      the audio sub-call doesn't actually need but still has
    //      to evaluate at graph-init time. A single rasterisation
    //      failure in any of those nodes would abort the whole
    //      filter graph and ffmpeg would exit with no audio output.
    //
    // The fix is to build an AUDIO-ONLY filter graph: enumerate the
    // explicit `Scene::audio` rows + every visible actor whose
    // source has an audio stream (mirrors `app.rs::build_sources` so
    // the rendered audio matches what plays in the canvas preview),
    // wire each through the same per-track normalisation chain that
    // `emit_audio` uses for graph-init stability, and feed it
    // straight to the AAC encoder. Everything video-related stays
    // out of the cmdline so the audio render can't be poisoned by
    // an unrelated overlay / mask glitch.

    struct AudioJob {
        source: PathBuf,
        t_in: f32,
        t_out: f32,
        source_start: f32,
        volume: f32,
        mute: bool,
        fade_in: f32,
        fade_out: f32,
        /// Playback rate multiplier: 1.0 = unchanged, 0.5 = half
        /// speed (sounds twice as long), 2.0 = double speed.
        speed: f32,
        /// Pitch shift in semitones. 0 = no shift.
        pitch_semitones: f32,
        /// Stereo pan. -1.0 = full left, 0.0 = centre, +1.0 = full right.
        pan: f32,
        /// Low-pass filter cutoff in Hz. None = disabled.
        low_pass_hz: Option<u32>,
        /// High-pass filter cutoff in Hz. None = disabled.
        high_pass_hz: Option<u32>,
        /// Reverb wet mix (0..1). 0 = dry.
        reverb: f32,
    }

    let scene_dur = scene.output.duration.max(1.0 / 60.0);
    let mut jobs: Vec<AudioJob> = Vec::new();
    // Actor ids that have an explicit AudioTrack row. This mirrors
    // `app.rs::build_sources`: a row bound to a specific actor is the
    // source of truth for that clip's audio (including mute/volume and
    // split windows). Do not dedupe by source path: after a split or a
    // copy, multiple actor clips can legitimately use the same file and
    // must each schedule their own audio window.
    let explicit_actor_audio: std::collections::HashSet<usize> = scene
        .audio
        .iter()
        .enumerate()
        .filter(|(_, tr)| !tr.deleted)
        .filter_map(|(idx, _)| infer_actor_for_audio_in_scene(scene, idx))
        .collect();
    for tr in &scene.audio {
        // Skip deleted audio tracks (marked as deleted instead of
        // removed from the array to prevent index shifts in the UI).
        if tr.deleted {
            continue;
        }
        let path = if tr.source.is_absolute() {
            tr.source.clone()
        } else {
            assets_root.join(&tr.source)
        };
        let t_in = tr.t_in;
        let t_out = tr.t_out.unwrap_or(scene_dur);
        let clip_dur = (t_out.max(t_in) - t_in).max(1.0 / 60.0);
        let mid = clip_dur * 0.5;
        jobs.push(AudioJob {
            source: path,
            t_in,
            t_out,
            source_start: tr.source_start,
            volume: tr.volume_at(mid),
            mute: tr.mute,
            fade_in: tr.fade_in,
            fade_out: tr.fade_out,
            speed: tr.speed_at(mid),
            pitch_semitones: tr.pitch_at(mid),
            pan: tr.pan_at(mid),
            low_pass_hz: tr.low_pass_at(mid),
            high_pass_hz: tr.high_pass_at(mid),
            reverb: tr.reverb_at(mid),
        });
    }
    for (actor_idx, actor) in scene.actors.iter().enumerate() {
        if !actor.visible {
            continue;
        }
        if actor.mute_audio {
            continue;
        }
        let path = if actor.source.is_absolute() {
            actor.source.clone()
        } else {
            assets_root.join(&actor.source)
        };
        if explicit_actor_audio.contains(&actor_idx) {
            continue;
        }
        let t_in = actor.t_in.unwrap_or(0.0);
        let t_out = actor.t_out.unwrap_or(scene_dur);
        jobs.push(AudioJob {
            source: path,
            t_in,
            t_out,
            source_start: actor.source_start,
            volume: 1.0,
            mute: false,
            fade_in: 0.0,
            fade_out: 0.0,
            // Mirror the actor's speed onto its embedded soundtrack so
            // a slowed-down video clip's audio also slows down (and
            // its rendered window stays in sync). Actors don't expose
            // a pitch shift, so leave it neutral.
            speed: actor.speed,
            pitch_semitones: 0.0,
            pan: 0.0,
            low_pass_hz: None,
            high_pass_hz: None,
            reverb: 0.0,
        });
    }

    if jobs.is_empty() {
        return Ok(());
    }

    // Drop tracks with missing files / no audio stream BEFORE we
    // build the cmdline so each `[idx:a]` reference resolves.
    jobs.retain(|j| {
        if !j.source.exists() {
            warn!(
                path = %j.source.display(),
                "audio mux: source missing, skipping",
            );
            return false;
        }
        if !crate::proc::probe_has_audio_stream(&j.source) {
            warn!(
                path = %j.source.display(),
                "audio mux: source has no audio stream, skipping",
            );
            return false;
        }
        true
    });
    if jobs.is_empty() {
        return Ok(());
    }

    const SR: u32 = 44_100;
    const FMT: &str = "fltp";
    const LAYOUT: &str = "stereo";

    // Build the cmdline: every job becomes a `-i <path>` followed by
    // a per-track normalise-trim-delay chain. Tracks are mixed over
    // an explicit scene-length silent bed, then the bus is trimmed
    // and fed straight to the AAC encoder.
    let bin = crate::ffmpeg_binary();
    let mut cmd = Command::new(&bin);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);

    for job in &jobs {
        cmd.args(["-i"]).arg(&job.source);
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for (idx, job) in jobs.iter().enumerate() {
        let t_in = job.t_in.max(0.0).min(scene_dur);
        let t_out = job.t_out.clamp(t_in, scene_dur);
        let clip_dur = (t_out - t_in).max(1.0 / 60.0);
        let volume = if job.mute { 0.0 } else { job.volume.max(0.0) };
        let speed = job.speed.max(0.05);
        let pitch_semis = job.pitch_semitones;

        let mut filters: Vec<String> = Vec::with_capacity(12);
        filters.push(format!("aresample={sr}", sr = SR));
        filters.push(format!(
            "aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}",
            fmt = FMT,
            sr = SR,
            ch = LAYOUT,
        ));
        if job.source_start > 0.0 {
            filters.push(format!("atrim=start={:.6}", job.source_start));
            filters.push("asetpts=PTS-STARTPTS".into());
        }

        // ── Pitch + speed (resample model) ──
        //
        // The canvas preview implements speed+pitch as a single
        // resample: `effective_rate = speed * 2^(pitch/12)`. This is
        // a pure "tape speed" model — changing speed also changes
        // pitch, and vice versa. The ffmpeg equivalent is:
        //
        //   asetrate=SR*rate  — re-label the sample rate so playback
        //                       is faster/slower AND pitch shifts.
        //   aresample=SR      — resample back to the bus rate so
        //                       downstream filters + amix see a
        //                       uniform sample rate.
        //
        // This matches `filtergraph.rs::emit_audio` and produces the
        // same audible result as the live preview.
        let pitch_factor = 2f32.powf(pitch_semis / 12.0);
        let rate = (speed * pitch_factor).max(0.05);
        if (rate - 1.0).abs() > 1e-4 {
            filters.push(format!("asetrate={:.6}", SR as f32 * rate));
            filters.push(format!("aresample={sr}", sr = SR));
            filters.push(format!(
                "aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}",
                fmt = FMT,
                sr = SR,
                ch = LAYOUT,
            ));
        }

        filters.push(format!("atrim=duration={:.6}", clip_dur));
        filters.push("asetpts=PTS-STARTPTS".into());

        // ── Per-track DSP effects (mirrors the preview's chain) ──
        //
        // Order: high-pass → low-pass → reverb → fades → pan → volume.
        // This matches the live preview's `audio_engine::load_sinks`
        // pipeline so the rendered audio sounds identical to what the
        // user heard in the editor.

        // High-pass filter.
        if let Some(hp) = job.high_pass_hz {
            if hp > 0 {
                filters.push(format!("highpass=f={}", hp));
            }
        }
        // Low-pass filter.
        if let Some(lp) = job.low_pass_hz {
            if lp > 0 {
                filters.push(format!("lowpass=f={}", lp));
            }
        }
        // Reverb (feedback comb approximation via aecho). The preview
        // uses a single-tap feedback comb at ~120 ms with
        // feedback = mix * 0.55 (capped at 0.7). ffmpeg's `aecho`
        // with a single delay tap and decay < 1 produces a similar
        // decaying echo tail.
        if job.reverb > 1e-3 {
            let mix = job.reverb.clamp(0.0, 1.0);
            let decay = (mix * 0.55).min(0.7);
            filters.push(format!("aecho=1.0:{:.6}:120:{:.6}", mix, decay));
        }

        if job.fade_in > 0.0 {
            let fi = job.fade_in.min(clip_dur);
            if fi > 0.0 {
                filters.push(format!("afade=t=in:st=0:d={:.6}:curve=tri", fi));
            }
        }
        if job.fade_out > 0.0 {
            let fo = job.fade_out.min(clip_dur);
            if fo > 0.0 {
                let st = (clip_dur - fo).max(0.0);
                filters.push(format!("afade=t=out:st={:.6}:d={:.6}:curve=tri", st, fo));
            }
        }
        // Stereo pan — same equal-power law as the preview's
        // `dsp::Stereo` and `filtergraph.rs::emit_audio`. Skipped
        // when pan == 0 (centre = identity).
        if job.pan.abs() > 1e-4 {
            let pan = job.pan.clamp(-1.0, 1.0);
            let theta = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            let lg = theta.cos() * std::f32::consts::SQRT_2 * 0.5;
            let rg = theta.sin() * std::f32::consts::SQRT_2 * 0.5;
            let l_coef = lg * 0.5;
            let r_coef = rg * 0.5;
            filters.push(format!(
                "pan=stereo|c0={l:.6}*c0+{l:.6}*c1|c1={r:.6}*c0+{r:.6}*c1",
                l = l_coef,
                r = r_coef,
            ));
        }
        if (volume - 1.0).abs() > 1e-4 {
            filters.push(format!("volume={:.6}", volume));
        }
        let delay_ms = (t_in * 1000.0).round().max(0.0) as u64;
        if delay_ms > 0 {
            filters.push(format!("adelay={d}:all=1", d = delay_ms));
            // `adelay` shifts the stream timestamps as well as the
            // samples. Reset immediately so amix aligns every delayed
            // clip at timeline zero; otherwise a split clip can land
            // late and sound like it restarted from the wrong source
            // position in the exported MP4.
            filters.push("asetpts=PTS-STARTPTS".into());
        }
        filters.push(format!("atrim=duration={:.6}", scene_dur));
        filters.push("asetpts=PTS-STARTPTS".into());

        let lbl = format!("[a{}]", idx);
        chunks.push(format!(
            "[{idx}:a]{filters}{out}",
            idx = idx,
            filters = filters.join(","),
            out = lbl,
        ));
        labels.push(lbl);
    }

    let silence_label = "[asilence]";
    chunks.push(format!(
        "anullsrc=channel_layout={ch}:sample_rate={sr}:d={dur:.6}{out}",
        ch = LAYOUT,
        sr = SR,
        dur = scene_dur,
        out = silence_label,
    ));

    let mix_label = "[amix]".to_string();
    let mut mix_inputs = Vec::with_capacity(labels.len() + 1);
    mix_inputs.push(silence_label.to_string());
    mix_inputs.extend(labels);
    let inputs = mix_inputs.join("");
    let raw = "[amixraw]";
    chunks.push(format!(
        "{inputs}amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0{out}",
        inputs = inputs,
        n = mix_inputs.len(),
        out = raw,
    ));
    chunks.push(format!(
        "{raw}atrim=duration={dur:.6},asetpts=PTS-STARTPTS,\
         aformat=sample_fmts={fmt}:sample_rates={sr}:channel_layouts={ch}{out}",
        raw = raw,
        dur = scene_dur,
        fmt = FMT,
        sr = SR,
        ch = LAYOUT,
        out = mix_label,
    ));

    let filter_complex = chunks.join(";");

    cmd.args(["-filter_complex", &filter_complex])
        .args(["-map", &mix_label])
        .args([
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-ar",
            "44100",
            "-ac",
            "2",
            "-t",
            &format!("{:.3}", scene.output.duration),
            "-vn",
        ])
        .arg(&temp_audio)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    proc::hide_console_std(&mut cmd);

    let output = cmd.output().context("ffmpeg audio render")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            stderr = %stderr.lines().take(20).collect::<Vec<_>>().join("\n"),
            "audio render returned non-zero, skipping mux"
        );
        let _ = std::fs::remove_file(&temp_audio);
        return Ok(());
    }
    // Sanity check: an empty m4a means the encode silently produced
    // nothing. Fail-soft so the silent video still ships.
    let audio_meta = match std::fs::metadata(&temp_audio) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "audio mux: temp file vanished after encode");
            return Ok(());
        }
    };
    if audio_meta.len() < 1024 {
        warn!(
            size = audio_meta.len(),
            path = %temp_audio.display(),
            "audio mux: temp file too small, skipping mux step",
        );
        let _ = std::fs::remove_file(&temp_audio);
        return Ok(());
    }

    // Mux audio onto the silent MP4 by stream-copying both.
    let muxed = output_path.with_extension("muxed.mp4");
    let mut cmd = Command::new(&bin);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-i"])
        .arg(output_path)
        .args(["-i"])
        .arg(&temp_audio)
        .args([
            "-c:v",
            "copy",
            "-c:a",
            "copy",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-shortest",
        ])
        .arg(&muxed)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    proc::hide_console_std(&mut cmd);

    let output = cmd.output().context("ffmpeg audio mux")?;
    let _ = std::fs::remove_file(&temp_audio);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            stderr = %stderr.lines().take(20).collect::<Vec<_>>().join("\n"),
            "audio mux returned non-zero, output has no audio"
        );
        let _ = std::fs::remove_file(&muxed);
        return Ok(());
    }

    std::fs::rename(&muxed, output_path).context("move muxed file into place")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_chroma_key_does_not_key_or_despill_cpu_render() {
        let mut img = RgbaImage::from_pixel(1, 1, Rgba([0, 177, 64, 255]));
        let ck = ChromaKeyParams {
            enabled: false,
            key_color: [0, 177, 64],
            similarity: 1.0,
            blend: 1.0,
            spill: 1.0,
        };
        apply_chroma_and_cc(&mut img, &ck, &ColorCorrection::default());
        assert_eq!(
            img.get_pixel(0, 0).0,
            [0, 177, 64, 255],
            "disabled chromakey must leave RGBA untouched"
        );
    }
}
