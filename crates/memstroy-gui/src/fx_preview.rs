//! Live preview of FX zones on the canvas.
//!
//! An FX zone is an `Overlay::Image` with an empty `source` field — it
//! has no image content of its own and exists purely to apply its
//! `effects` stack to layers BELOW it that intersect its bounding box.
//! At export time the compositor (`memstroy-render`) handles this
//! cleanly via a multi-pass render. For the live canvas preview we run
//! a CPU-side approximation:
//!
//! 1. Identify each lower image overlay (in the `state.scene.overlays`
//!    Vec, BEFORE the FX zone) that intersects the FX bbox.
//! 2. Composite each one's decoded RGBA into a CPU buffer sized to
//!    the FX bbox at preview resolution.
//! 3. Apply the FX zone's effect stack to that buffer.
//! 4. Upload the result as a `TextureHandle` and draw it on top of
//!    the lower overlays inside the FX bbox.
//!
//! The result is cached per (fx_id, bbox_dims, effect_sig, lower_sig)
//! so a static composition only re-runs the CPU pipeline when something
//! actually changes.
//!
//! Limitations vs the export-time pipeline:
//!   * Image overlays only — actor frames live on the GPU and aren't
//!     readable from the egui side without copy-back. Actors below an
//!     FX zone show through unprocessed in the preview.
//!   * Fixed preview resolution (max 256px on the longer edge) so the
//!     CPU stage stays under ~5ms per recomputation.
//!   * Rotation / scale_y of lower overlays is approximated via a
//!     simple bilinear blit; the export-time compositor does proper
//!     trilinear + alpha blending.
//!
//! These are accepted trade-offs for live editing — the export still
//! produces the correct result.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use memstroy_core::{Overlay, OverlayState, effects::Effect, keyframe};

use crate::state::EditorState;

/// One cached FX preview, keyed by composition signature.
struct FxPreviewSlot {
    /// Cached uploaded texture.
    texture: TextureHandle,
    /// Pixel size of the cached buffer.
    #[allow(dead_code)]
    size: [u32; 2],
}

/// Per-FX cache. Stores the most recent baked result for each FX zone
/// id, keyed by a signature that hashes everything that could change
/// the output: bbox dims, effect stack, lower overlays' positions /
/// sizes / sources.
#[derive(Default)]
pub struct FxPreviewCache {
    inner: Mutex<HashMap<String, (u64, FxPreviewSlot)>>,
}

impl FxPreviewCache {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Look up a cached preview for `fx_id` if its signature matches.
    pub fn lookup(&self, fx_id: &str, sig: u64) -> Option<TextureHandle> {
        let g = self.inner.lock().ok()?;
        g.get(fx_id)
            .filter(|(prev_sig, _)| *prev_sig == sig)
            .map(|(_, slot)| slot.texture.clone())
    }

    /// Store a fresh preview, replacing any previous entry for `fx_id`.
    pub fn store(&self, fx_id: String, sig: u64, texture: TextureHandle, size: [u32; 2]) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.insert(fx_id, (sig, FxPreviewSlot { texture, size }));
    }

    /// Drop entries whose `fx_id` no longer exists in the scene.
    #[allow(dead_code)]
    pub fn prune(&self, alive_ids: &std::collections::HashSet<String>) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.retain(|id, _| alive_ids.contains(id));
    }
}

/// Compute the signature for an FX preview composition. Hashes the
/// bbox dimensions, effect stack, and a digest of every lower overlay
/// that contributes pixels (its source path + position + scale).
fn composition_signature(
    bbox_w: u32,
    bbox_h: u32,
    effects: &[Effect],
    lower_overlays: &[(PathBuf, [f32; 2], f32, f32)],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bbox_w.hash(&mut h);
    bbox_h.hash(&mut h);
    crate::image_effects::signature(effects).hash(&mut h);
    for (p, pos, w, ho) in lower_overlays {
        p.hash(&mut h);
        bits(pos[0]).hash(&mut h);
        bits(pos[1]).hash(&mut h);
        bits(*w).hash(&mut h);
        bits(*ho).hash(&mut h);
    }
    h.finish()
}

fn bits(v: f32) -> u32 {
    v.to_bits()
}

/// Identify image overlays that contribute to an FX zone's preview
/// at time `t`. Returns (idx_in_scene, source_path, world_pos,
/// world_w, world_h) for each contributing overlay.
///
/// "Contributes" means:
///   - Earlier in `scene.overlays` (lower in z-order by default)
///   - Source path is non-empty (skips other FX zones)
///   - Visible at time `t` (between t_in and t_out)
///   - World AABB intersects the FX zone's AABB
pub fn collect_contributors(
    state: &EditorState,
    fx_idx: usize,
    fx_bbox_min: [f32; 2],
    fx_bbox_max: [f32; 2],
    t: f32,
) -> Vec<(usize, PathBuf, [f32; 2], f32, f32)> {
    let mut out = Vec::new();
    let [out_w, out_h] = state.scene.render_frame.resolution;
    let world_w = out_w as f32;
    let world_h = out_h as f32;

    for (idx, ov) in state.scene.overlays.iter().enumerate() {
        if idx >= fx_idx {
            // Same level or above — doesn't contribute to FX zone
            // (the FX zone applies to elements drawn BEFORE it).
            break;
        }
        let im = match ov {
            Overlay::Image(im) => im,
            _ => continue, // only image overlays for the preview
        };
        if im.source.as_os_str().is_empty() {
            continue; // another FX zone, not content
        }
        // Visibility window check
        if t < im.t_in || t > im.t_out {
            continue;
        }
        // Sample the overlay's local state
        let local_t = (t - im.t_in).max(0.0);
        let ov_state: OverlayState = keyframe::sample(&im.layout, local_t)
            .unwrap_or_default();
        // World position (legacy normalised → world pixels)
        let center_x = ov_state.pos[0] * world_w;
        let center_y = ov_state.pos[1] * world_h;

        // Read source PNG dimensions from the texture cache if loaded
        let (sw, sh) = state.image_textures.lock().ok()
            .and_then(|m| match m.get(&im.source) {
                Some(crate::state::ImageTextureSlot::Loaded { size, .. }) => {
                    Some((size[0] as f32, size[1] as f32))
                }
                _ => None,
            })
            .unwrap_or((200.0, 200.0));
        let w = sw * ov_state.scale;
        let h = sh * ov_state.scale * ov_state.scale_y;
        // AABB
        let mn = [center_x - w * 0.5, center_y - h * 0.5];
        let mx = [center_x + w * 0.5, center_y + h * 0.5];
        // Intersection test with FX bbox
        if mn[0] >= fx_bbox_max[0] || mx[0] <= fx_bbox_min[0]
            || mn[1] >= fx_bbox_max[1] || mx[1] <= fx_bbox_min[1] {
            continue;
        }
        out.push((idx, im.source.clone(), [center_x, center_y], w, h));
    }
    out
}

/// Run the CPU-side composition + effect application and produce a
/// `ColorImage` ready for upload. Returns `None` when no contributors
/// were found (caller falls back to a tinted outline).
pub fn bake_fx_preview(
    state: &EditorState,
    fx_bbox_min: [f32; 2],
    fx_bbox_max: [f32; 2],
    contributors: &[(usize, PathBuf, [f32; 2], f32, f32)],
    effects: &[Effect],
    max_dim: u32,
) -> Option<ColorImage> {
    if contributors.is_empty() {
        return None;
    }
    let bbox_w_world = (fx_bbox_max[0] - fx_bbox_min[0]).max(1.0);
    let bbox_h_world = (fx_bbox_max[1] - fx_bbox_min[1]).max(1.0);

    // Determine preview pixel size, capping the longer edge at max_dim
    let scale = (max_dim as f32 / bbox_w_world.max(bbox_h_world)).min(1.0);
    let buf_w = (bbox_w_world * scale).clamp(8.0, max_dim as f32) as u32;
    let buf_h = (bbox_h_world * scale).clamp(8.0, max_dim as f32) as u32;

    // Allocate transparent RGBA buffer
    let mut rgba: Vec<u8> = vec![0u8; (buf_w * buf_h * 4) as usize];

    // Composite each contributor
    for (_, source, pos, w, h) in contributors {
        // Get decoded image (try cache first; if not, skip — the
        // image_fx_cache's worker will populate it after a few frames).
        let decoded = state.image_fx_cache.get_decoded(source);
        let (src_rgba, src_w, src_h) = match decoded {
            Some(d) => (d.rgba.clone(), d.width, d.height),
            None => {
                // Try to decode synchronously as a fallback. This is
                // only the FIRST time we see an image — subsequent
                // frames will hit the cache.
                let img = match image::open(source) {
                    Ok(i) => i.to_rgba8(),
                    Err(_) => continue,
                };
                let sw = img.width();
                let sh = img.height();
                let raw = img.into_raw();
                let arc = std::sync::Arc::new(raw);
                state.image_fx_cache.put_decoded(
                    source.clone(),
                    crate::image_fx_cache::DecodedImage {
                        rgba: arc.clone(),
                        width: sw,
                        height: sh,
                    },
                );
                (arc, sw, sh)
            }
        };
        // Compute target rect in buffer pixel space:
        // overlay centred at `pos` in world coords → translate to bbox-local
        // (pos - fx_bbox_min) → scale to buffer pixels (* scale)
        let local_cx = (pos[0] - fx_bbox_min[0]) * scale;
        let local_cy = (pos[1] - fx_bbox_min[1]) * scale;
        let dest_w = (w * scale) as i32;
        let dest_h = (h * scale) as i32;
        if dest_w < 1 || dest_h < 1 { continue; }
        let dest_x0 = (local_cx - (dest_w as f32) * 0.5) as i32;
        let dest_y0 = (local_cy - (dest_h as f32) * 0.5) as i32;

        // Bilinear-resample blit from src → dest with alpha blending
        for dy in 0..dest_h {
            let y_dest = dest_y0 + dy;
            if y_dest < 0 || y_dest >= buf_h as i32 { continue; }
            // Source v coordinate (0..1) maps to source pixels
            let v_src = (dy as f32 / dest_h.max(1) as f32) * src_h as f32;
            let sy = (v_src as i32).clamp(0, src_h as i32 - 1) as usize;
            for dx in 0..dest_w {
                let x_dest = dest_x0 + dx;
                if x_dest < 0 || x_dest >= buf_w as i32 { continue; }
                let u_src = (dx as f32 / dest_w.max(1) as f32) * src_w as f32;
                let sx = (u_src as i32).clamp(0, src_w as i32 - 1) as usize;

                let src_idx = (sy * src_w as usize + sx) * 4;
                if src_idx + 3 >= src_rgba.len() { continue; }
                let sr = src_rgba[src_idx];
                let sg = src_rgba[src_idx + 1];
                let sb = src_rgba[src_idx + 2];
                let sa = src_rgba[src_idx + 3];
                if sa == 0 { continue; }

                let dest_idx = (y_dest as usize * buf_w as usize + x_dest as usize) * 4;
                if dest_idx + 3 >= rgba.len() { continue; }
                // Standard alpha-over composite
                let dr = rgba[dest_idx];
                let dg = rgba[dest_idx + 1];
                let db = rgba[dest_idx + 2];
                let da = rgba[dest_idx + 3];
                let sa_f = sa as f32 / 255.0;
                let da_f = da as f32 / 255.0;
                let out_a = sa_f + da_f * (1.0 - sa_f);
                if out_a < 1e-6 { continue; }
                let out_r = (sr as f32 * sa_f + dr as f32 * da_f * (1.0 - sa_f)) / out_a;
                let out_g = (sg as f32 * sa_f + dg as f32 * da_f * (1.0 - sa_f)) / out_a;
                let out_b = (sb as f32 * sa_f + db as f32 * da_f * (1.0 - sa_f)) / out_a;
                rgba[dest_idx] = out_r.clamp(0.0, 255.0) as u8;
                rgba[dest_idx + 1] = out_g.clamp(0.0, 255.0) as u8;
                rgba[dest_idx + 2] = out_b.clamp(0.0, 255.0) as u8;
                rgba[dest_idx + 3] = (out_a * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }

    // Apply the FX effect stack to the composed buffer in place
    let _ = crate::image_effects::apply_effect_stack(&mut rgba, buf_w, buf_h, effects, 0.0);

    // Convert to ColorImage
    let pixels: Vec<Color32> = rgba.chunks_exact(4)
        .map(|c| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
        .collect();
    Some(ColorImage {
        size: [buf_w as usize, buf_h as usize],
        pixels,
    })
}

/// Public entry point used by the canvas preview. Returns a texture
/// handle for the FX preview when one is available (cache hit OR
/// fresh bake). The caller draws this texture over the FX bbox.
pub fn ensure_fx_preview(
    state: &EditorState,
    cache: &FxPreviewCache,
    fx_idx: usize,
    fx_id: &str,
    fx_bbox_min: [f32; 2],
    fx_bbox_max: [f32; 2],
    effects: &[Effect],
    t: f32,
    ctx: &egui::Context,
) -> Option<TextureHandle> {
    let bbox_w_world = (fx_bbox_max[0] - fx_bbox_min[0]).max(1.0);
    let bbox_h_world = (fx_bbox_max[1] - fx_bbox_min[1]).max(1.0);

    // Identify contributors
    let contributors = collect_contributors(state, fx_idx, fx_bbox_min, fx_bbox_max, t);
    if contributors.is_empty() {
        return None;
    }

    // Check if effects are actually active
    let active_effects: Vec<Effect> = effects.iter()
        .filter(|e| e.enabled && e.intensity > 0.001)
        .cloned()
        .collect();
    if active_effects.is_empty() {
        return None;
    }

    // Build signature
    let lower: Vec<(PathBuf, [f32; 2], f32, f32)> = contributors.iter()
        .map(|(_, p, pos, w, h)| (p.clone(), *pos, *w, *h))
        .collect();
    let sig = composition_signature(
        bbox_w_world as u32,
        bbox_h_world as u32,
        &active_effects,
        &lower,
    );

    // Cache hit → return cached texture
    if let Some(tex) = cache.lookup(fx_id, sig) {
        return Some(tex);
    }

    // Bake fresh
    let max_dim = 256_u32;
    let baked = bake_fx_preview(
        state,
        fx_bbox_min,
        fx_bbox_max,
        &contributors,
        &active_effects,
        max_dim,
    )?;
    let size = [baked.size[0] as u32, baked.size[1] as u32];
    let texture = ctx.load_texture(
        format!("fx_preview_{}", fx_id),
        baked,
        TextureOptions::LINEAR,
    );
    cache.store(fx_id.to_string(), sig, texture.clone(), size);
    Some(texture)
}
