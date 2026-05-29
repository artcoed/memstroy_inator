# Memstroy-inator — Agent Notes

## Build & Test
- `cargo check -p memstroy-gui` — fast compile check.
- `cargo test -p memstroy-gui --lib` — unit tests only (fast).
- `cargo test -p memstroy-gui` — includes slow integration tests; use only when needed.
- **Release builds with AVX2**: the SIMD kernel is compiled only when the compiler is allowed to emit vector instructions. To enable it pass `RUSTFLAGS` manually:
  ```
  set RUSTFLAGS=-C target-feature=+avx2,+fma
  cargo build --release -p memstroy-gui
  ```
  On older CPUs omit the flag — the scalar reference path (`apply_effects_cpu_scalar`) is pixel-identical and will be selected automatically.

## Performance Optimisation (Background Frame Pipeline)

### Phase 1 — Off-UI chroma-key / CC caching
`FrameCache` now holds two ring buffers:
- `buffer` — raw decoded JPEG frames (already existed).
- `processed_buffer` — chroma-keyed + colour-corrected + effects frames, produced by a background thread.

When `processed_frame_at_time` is called from the canvas preview:
1. Fast hit on `fx_texture` (single most-recent processed frame).
2. Look in `processed_buffer`.
3. If neither is ready, show the **raw (un-keyed) frame immediately** and dispatch a background `trigger_processed_preload` to fill `processed_buffer` for subsequent frames.

This eliminates the per-frame chroma-key CPU cost from the UI thread, keeping playback at 60 fps even with 5+ chroma-keyed actors. Raw frames may flash for 1–2 frames during rapid motion; this is acceptable per user spec.

### Phase 2 — SIMD fused chroma-key kernel
`apply_effects_cpu` now dispatches to:
- `apply_effects_cpu_simd` — AVX2 (x86_64) / NEON (aarch64) via the `wide` crate, processing 8 pixels per iteration for chroma-key + brightness + contrast + saturation + temperature.
- `apply_effects_cpu_scalar` — pixel-perfect reference implementation (±1). Used as fallback on non-vector targets or in debug builds without AVX flags.

LGG (lift/gain/gamma) and tone-curve LUTs remain scalar because `powf` and gather are harder to vectorise portably; they run as a lightweight second pass over the already SIMD-processed frame.

Key invariant: `simd_matches_scalar_pixel_perfect` test asserts that every pixel in the SIMD output differs from the scalar output by at most 1 in each channel.

## Critical Paths
- `video_cache.rs:356` `processed_frame_at_time` — called from `canvas_preview.rs:~1416` for every visible actor each frame.
- `video_cache.rs:apply_effects_cpu` / `apply_effects_cpu_simd` — hot path executed by both the UI thread (raw fallback) and background preload threads.
- `video_cache.rs:trigger_processed_preload` — background thread entry point; uses the same `apply_effects_cpu` as the old synchronous path, guaranteeing pixel consistency.

## Memory Budget Notes
- `processed_buffer` currently caps at 90 frames per cache (same as raw buffer). With 50 actors this is ~50 × 90 × (360p RGBA) ≈ 350 MB. If memory pressure becomes an issue, add an LRU eviction limit to `processed_buffer`.
- Adaptive `preview_dim` (180 / 240 / 360) still applies to extracted frames; lower resolutions reduce both raw and processed memory simultaneously.

## Chroma-Key Math
The canvas preview uses FFmpeg-faithful BT.601 YCbCr distance, identical to the export pipeline in `memstroy-render/filtergraph.rs`. This fixes the old "preview ≠ export" alpha-edge drift bug.

## Regression Checklist
- [ ] `cargo test -p memstroy-gui --lib video_cache::tests` passes (pixel-perfect SIMD).
- [ ] `cargo check -p memstroy-gui` clean.
- [ ] Scrubbing/playback with ≥5 chroma-keyed actors stays ≥60 fps in release build on AVX2 machine.
- [ ] Export still uses `memstroy-render` (not GUI cache); no change needed there.
