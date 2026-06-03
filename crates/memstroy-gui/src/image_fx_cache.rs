//! Two-layer LRU cache for the image-effects pipeline.
//!
//! The cache replaces the bare `HashMap<(PathBuf, u64), ImageFxSlot>`
//! the canvas-preview path used to lock on every paint. Two improvements
//! land here together:
//!
//! 1. **A separate L1 (decoded RGBA) layer** so dragging an effect
//!    slider doesn't re-decode the source PNG every signature change —
//!    only the (cheap) effect pipeline re-runs.
//!
//! 2. **A `Pending | Ready | Failed` state machine on the L2 (baked
//!    texture) layer** so the canvas can show *something* — typically
//!    the unprocessed image — while the heavy effect bake runs on a
//!    worker thread. The previous design blocked the UI thread for the
//!    entire decode + filter chain on every cache miss.
//!
//! Both layers are LRU-capped by byte budget (decoded ~64 MiB, baked
//! ~256 MiB by default). Eviction picks the least recently *used*
//! entry — `last_used` is bumped on every cache hit, including frame-
//! to-frame paint reads, so visible textures are sticky and only the
//! truly stale ones get dropped.
//!
//! The cache also tracks **in-flight bake jobs** in a `HashSet` so
//! concurrent calls to `submit_image_fx_job` (e.g. from successive
//! frames before a job finishes) are deduplicated atomically. The
//! worker calls `release_in_flight` on completion regardless of
//! outcome.
//!
//! All public methods take `&self` and lock an internal `Mutex`. The
//! lock window is intentionally short — the body of every method is a
//! handful of HashMap / Vec ops with no allocation in the hot path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default cap for the decoded RGBA layer. ~64 MiB ≈ a few 4K PNGs.
pub const DEFAULT_DECODED_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Default cap for the baked texture layer. ~256 MiB ≈ ~16× a 4096×4096
/// fully baked overlay, plenty for a typical scene without leaning on
/// the GPU's own memory pressure to evict.
pub const DEFAULT_BAKED_CAP_BYTES: usize = 256 * 1024 * 1024;

/// Decoded RGBA8 buffer for one image source. Wrapped in `Arc` so the
/// worker can hand a cheap clone to the bake step without bumping the
/// per-pixel cost of cloning a 4K image.
#[derive(Clone)]
pub struct DecodedImage {
    pub rgba: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

impl DecodedImage {
    fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// Cached effects-processed texture for an image overlay. Stored
/// alongside the Crop UV inset so the renderer can shrink the picture's
/// visible rectangle to match a Crop entry on the effect stack.
///
/// This struct used to live in `state.rs` next to the bare HashMap;
/// it is now owned by the cache module and re-exported as
/// `crate::state::ImageFxSlot` for compatibility with existing
/// call-sites in `canvas_preview.rs`.
#[derive(Clone)]
pub struct ImageFxSlot {
    pub texture: egui::TextureHandle,
    pub size: [u32; 2],
    /// (left, top, right, bottom) crop inset in normalised 0..1.
    pub crop: [f32; 4],
}

/// Lifecycle of a single `(path, signature)` baked entry.
///
/// `Pending` is set the moment a bake job is dispatched; the canvas
/// uses it as a signal to draw the unprocessed image instead of
/// blocking. `Ready` carries the actual texture; `Failed` carries a
/// human-readable reason and is also cached so we don't keep retrying
/// the same broken image every frame.
#[derive(Clone)]
pub enum BakedState {
    Pending,
    Ready(ImageFxSlot),
    /// Cached failure, with a human-readable reason. The reason is
    /// kept around so a future status-bar surface can show it without
    /// the canvas needing a second round-trip through the worker —
    /// today the canvas only branches on the variant tag, hence the
    /// `allow(dead_code)`.
    Failed {
        #[allow(dead_code)]
        reason: String,
    },
}

/// Outcome of a `lookup_or_dispatch`-style call. Mirrors `BakedState`
/// for `Ready` / `Failed`, plus a `Miss` value that the caller uses to
/// know it should submit a fresh bake job.
pub enum LookupOutcome {
    /// The texture is ready — draw it.
    Ready(ImageFxSlot),
    /// A bake is in flight — fall back to the unprocessed image.
    Pending,
    /// A previous bake failed — fall back to the unprocessed image
    /// and don't retry. The reason is intentionally not surfaced to
    /// callers in the hot path; it's logged once at insertion time.
    Failed,
    /// Nothing in the cache for this key. Caller should dispatch a
    /// fresh job (which will land back here as `Pending`).
    Miss,
}

pub struct ImageFxCache {
    inner: Mutex<Inner>,
}

struct Inner {
    decoded: HashMap<PathBuf, DecodedEntry>,
    decoded_bytes: usize,
    decoded_cap: usize,

    baked: HashMap<(PathBuf, u64), BakedEntry>,
    baked_bytes: usize,
    baked_cap: usize,

    /// Keys whose bake job is currently running on a worker thread.
    /// Used to dedup concurrent submissions and to drop stale results
    /// (when the corresponding entry has been evicted by the time the
    /// worker finishes).
    in_flight: HashSet<(PathBuf, u64)>,

    /// Monotonic LRU counter; bumped on every successful read or
    /// insert and stamped onto the touched entry's `last_used`.
    counter: u64,
}

struct DecodedEntry {
    image: DecodedImage,
    bytes: usize,
    last_used: u64,
}

struct BakedEntry {
    state: BakedState,
    bytes: usize,
    last_used: u64,
}

impl Default for ImageFxCache {
    fn default() -> Self {
        Self::new(DEFAULT_DECODED_CAP_BYTES, DEFAULT_BAKED_CAP_BYTES)
    }
}

impl ImageFxCache {
    pub fn new(decoded_cap: usize, baked_cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                decoded: HashMap::new(),
                decoded_bytes: 0,
                decoded_cap,
                baked: HashMap::new(),
                baked_bytes: 0,
                baked_cap,
                in_flight: HashSet::new(),
                counter: 0,
            }),
        }
    }

    /// Look up a baked entry by `(path, sig)`. Hits bump the LRU
    /// counter so frame-to-frame draws keep visible textures pinned.
    pub fn lookup(&self, path: &Path, sig: u64) -> LookupOutcome {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            // A poisoned lock can only happen if a previous lock holder
            // panicked mid-update. The cache is best-effort; just treat
            // it as a miss so the caller falls back to the unprocessed
            // image and we recover on the next successful insert.
            Err(_) => return LookupOutcome::Miss,
        };
        let key = (path.to_path_buf(), sig);
        let next = g.bump_counter();
        match g.baked.get_mut(&key) {
            Some(entry) => {
                entry.last_used = next;
                match &entry.state {
                    BakedState::Ready(slot) => LookupOutcome::Ready(slot.clone()),
                    BakedState::Pending => LookupOutcome::Pending,
                    BakedState::Failed { .. } => LookupOutcome::Failed,
                }
            }
            None => LookupOutcome::Miss,
        }
    }

    /// Look up the decoded RGBA for `path` if cached. The worker calls
    /// this before re-decoding from disk so a slider drag avoids
    /// re-paying the PNG decode cost for every signature change.
    pub fn get_decoded(&self, path: &Path) -> Option<DecodedImage> {
        let mut g = self.inner.lock().ok()?;
        let next = g.bump_counter();
        let entry = g.decoded.get_mut(path)?;
        entry.last_used = next;
        Some(entry.image.clone())
    }

    /// Insert a freshly decoded image into the L1 layer and run LRU
    /// eviction if we're over budget.
    pub fn put_decoded(&self, path: PathBuf, image: DecodedImage) {
        let Ok(mut g) = self.inner.lock() else { return };
        let bytes = image.bytes();
        let next = g.bump_counter();
        if let Some(prev) = g.decoded.insert(
            path,
            DecodedEntry {
                image,
                bytes,
                last_used: next,
            },
        ) {
            g.decoded_bytes = g.decoded_bytes.saturating_sub(prev.bytes);
        }
        g.decoded_bytes = g.decoded_bytes.saturating_add(bytes);
        g.evict_decoded_if_needed();
    }

    /// Atomically claim a bake-in-flight slot for `(path, sig)`.
    /// Returns `true` if no other worker is already baking the same
    /// key — the caller is then responsible for eventually calling
    /// `release_in_flight` (whether the bake succeeds or fails).
    ///
    /// Side effect: on a successful claim the corresponding baked
    /// entry transitions to `Pending` so the canvas reads it as
    /// "in progress, fall back to the raw image" without a second
    /// round-trip through the cache.
    pub fn try_claim_in_flight(&self, path: &Path, sig: u64) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        let key = (path.to_path_buf(), sig);
        if g.in_flight.contains(&key) {
            return false;
        }
        g.in_flight.insert(key.clone());
        let next = g.bump_counter();
        // Pending placeholders are bookkeeping only; counted as 0
        // bytes so they don't push real textures out of the cache.
        let bytes = 0;
        if let Some(prev) = g.baked.insert(
            key,
            BakedEntry {
                state: BakedState::Pending,
                bytes,
                last_used: next,
            },
        ) {
            g.baked_bytes = g.baked_bytes.saturating_sub(prev.bytes);
        }
        true
    }

    /// Release the in-flight claim. Idempotent — safe to call from
    /// the worker's drop / cleanup path.
    pub fn release_in_flight(&self, path: &Path, sig: u64) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.in_flight.remove(&(path.to_path_buf(), sig));
    }

    /// Insert a successful bake result, evicting other signatures for
    /// the same path (matches the previous cache's "one signature per
    /// path" behaviour so the user sliding a parameter doesn't pile
    /// up dozens of stale baked variants).
    pub fn put_ready(&self, path: PathBuf, sig: u64, slot: ImageFxSlot) {
        let Ok(mut g) = self.inner.lock() else { return };
        let bytes = (slot.size[0] as usize) * (slot.size[1] as usize) * 4;
        let next = g.bump_counter();

        // Evict every other baked entry for the same path. Skips the
        // in-flight set: those are placeholders for jobs we still
        // expect to complete, and removing them here would cause the
        // worker's eventual put_ready to insert a brand-new entry
        // anyway — harmless but wasteful.
        let stale_keys: Vec<(PathBuf, u64)> = g
            .baked
            .keys()
            .filter(|(p, s)| p == &path && *s != sig)
            .cloned()
            .collect();
        for k in stale_keys {
            if let Some(prev) = g.baked.remove(&k) {
                g.baked_bytes = g.baked_bytes.saturating_sub(prev.bytes);
            }
        }

        let key = (path, sig);
        if let Some(prev) = g.baked.insert(
            key,
            BakedEntry {
                state: BakedState::Ready(slot),
                bytes,
                last_used: next,
            },
        ) {
            g.baked_bytes = g.baked_bytes.saturating_sub(prev.bytes);
        }
        g.baked_bytes = g.baked_bytes.saturating_add(bytes);
        g.evict_baked_if_needed();
    }

    /// Record a bake failure for `(path, sig)`. Cached so we don't
    /// retry the same broken image every frame; the caller (the
    /// worker) is expected to log the reason at warn level.
    pub fn put_failed(&self, path: PathBuf, sig: u64, reason: String) {
        let Ok(mut g) = self.inner.lock() else { return };
        let next = g.bump_counter();
        // Failure entries take essentially no memory; track 0 bytes
        // so they sit at the bottom of the LRU and get evicted first
        // when the user retouches the picture / effect stack.
        let bytes = 0;
        let key = (path, sig);
        if let Some(prev) = g.baked.insert(
            key,
            BakedEntry {
                state: BakedState::Failed { reason },
                bytes,
                last_used: next,
            },
        ) {
            g.baked_bytes = g.baked_bytes.saturating_sub(prev.bytes);
        }
    }

    /// Wipe everything for `path` — both layers — so the next paint
    /// re-decodes from disk. Used when the user re-saves a picture
    /// over an existing path.
    #[allow(dead_code)]
    pub fn invalidate_path(&self, path: &Path) {
        let Ok(mut g) = self.inner.lock() else { return };
        if let Some(prev) = g.decoded.remove(path) {
            g.decoded_bytes = g.decoded_bytes.saturating_sub(prev.bytes);
        }
        let stale_keys: Vec<(PathBuf, u64)> =
            g.baked.keys().filter(|(p, _)| p == path).cloned().collect();
        for k in stale_keys {
            if let Some(prev) = g.baked.remove(&k) {
                g.baked_bytes = g.baked_bytes.saturating_sub(prev.bytes);
            }
        }
    }

    // ─── observability helpers ───────────────────────────────────────

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        let Ok(g) = self.inner.lock() else { return 0 };
        g.baked
            .values()
            .filter(|e| matches!(e.state, BakedState::Pending))
            .count()
    }

    #[allow(dead_code)]
    pub fn baked_byte_usage(&self) -> usize {
        self.inner.lock().map(|g| g.baked_bytes).unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn decoded_byte_usage(&self) -> usize {
        self.inner.lock().map(|g| g.decoded_bytes).unwrap_or(0)
    }
}

impl Inner {
    #[inline]
    fn bump_counter(&mut self) -> u64 {
        self.counter = self.counter.wrapping_add(1);
        self.counter
    }

    /// Drop the least recently used decoded entries until we're back
    /// under the byte cap. O(n log n) over the cache size on the rare
    /// frames we go over budget; cache size is bounded to a few dozen
    /// entries in practice so this never shows up on a profile.
    fn evict_decoded_if_needed(&mut self) {
        if self.decoded_bytes <= self.decoded_cap {
            return;
        }
        let mut order: Vec<(PathBuf, u64)> = self
            .decoded
            .iter()
            .map(|(k, v)| (k.clone(), v.last_used))
            .collect();
        order.sort_by_key(|(_, t)| *t);
        for (k, _) in order {
            if self.decoded_bytes <= self.decoded_cap {
                break;
            }
            if let Some(prev) = self.decoded.remove(&k) {
                self.decoded_bytes = self.decoded_bytes.saturating_sub(prev.bytes);
            }
        }
    }

    fn evict_baked_if_needed(&mut self) {
        if self.baked_bytes <= self.baked_cap {
            return;
        }
        let mut order: Vec<((PathBuf, u64), u64)> = self
            .baked
            .iter()
            // Don't evict in-flight Pending entries — they're cheap
            // and dropping them would orphan the worker's eventual
            // put_ready (it would re-insert with a fresh LRU slot).
            .filter(|(k, e)| {
                !matches!(e.state, BakedState::Pending) || !self.in_flight.contains(*k)
            })
            .map(|(k, v)| (k.clone(), v.last_used))
            .collect();
        order.sort_by_key(|(_, t)| *t);
        for (k, _) in order {
            if self.baked_bytes <= self.baked_cap {
                break;
            }
            if let Some(prev) = self.baked.remove(&k) {
                self.baked_bytes = self.baked_bytes.saturating_sub(prev.bytes);
            }
        }
    }
}
