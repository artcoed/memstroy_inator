//! Tiny DSP module with custom `rodio::Source` adapters used by the
//! audio engine. Each adapter is a no-op when its parameter sits at the
//! neutral value, so untouched tracks pay essentially no extra cost.
//!
//! Filters are first-order IIRs (one-pole). They're not "audiophile"
//! grade but are stable, cheap, and behave reasonably across the
//! audible range — which is exactly what the editor preview needs.
//!
//! All filters operate on each channel independently and assume the
//! upstream source is stereo OR mono (anything else still works but
//! per-channel state may smear into other channels). The decoder
//! pipeline upstream is set up to deliver f32 samples; we propagate
//! `sample_rate()`, `channels()`, `current_frame_len()`, and
//! `total_duration()` through faithfully.

use rodio::Source;
use std::time::Duration;

/// Concrete wrapper around a `Box<dyn Source<Item = f32> + Send>` so we
/// can call methods that require `Self: Sized` (e.g. `take_duration`,
/// `delay`, custom adapters) on it. Pure delegation.
pub struct DynSource {
    inner: Box<dyn Source<Item = f32> + Send>,
}

impl DynSource {
    pub fn new(inner: Box<dyn Source<Item = f32> + Send>) -> Self {
        Self { inner }
    }
}

impl Iterator for DynSource {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl Source for DynSource {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

// ─── HIGH-PASS FILTER ────────────────────────────────────────────────

/// One-pole high-pass IIR. Coefficient form:
///   `y[n] = α · (y[n-1] + x[n] − x[n-1])`
///   `α    = RC / (RC + dt) ; RC = 1 / (2π·fc)`
///
/// `cutoff_hz = None` (or 0) makes this a passthrough.
pub struct HighPass<S: Source<Item = f32>> {
    inner: S,
    enabled: bool,
    alpha: f32,
    channels: usize,
    ch_idx: usize,
    prev_in: [f32; 8],
    prev_out: [f32; 8],
}

impl<S: Source<Item = f32>> HighPass<S> {
    pub fn new(inner: S, cutoff_hz: Option<u32>) -> Self {
        let channels = inner.channels() as usize;
        let sr = inner.sample_rate() as f32;
        let (enabled, alpha) = match cutoff_hz {
            Some(fc) if fc > 0 => {
                let dt = 1.0 / sr;
                let rc = 1.0 / (2.0 * std::f32::consts::PI * fc as f32);
                (true, rc / (rc + dt))
            }
            _ => (false, 0.0),
        };
        Self {
            inner,
            enabled,
            alpha,
            channels: channels.min(8).max(1),
            ch_idx: 0,
            prev_in: [0.0; 8],
            prev_out: [0.0; 8],
        }
    }
}

impl<S: Source<Item = f32>> Iterator for HighPass<S> {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let x = self.inner.next()?;
        if !self.enabled {
            return Some(x);
        }
        let c = self.ch_idx;
        self.ch_idx += 1;
        if self.ch_idx >= self.channels {
            self.ch_idx = 0;
        }
        let y = self.alpha * (self.prev_out[c] + x - self.prev_in[c]);
        self.prev_in[c] = x;
        self.prev_out[c] = y;
        Some(y)
    }
}

impl<S: Source<Item = f32>> Source for HighPass<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

// ─── LOW-PASS FILTER ─────────────────────────────────────────────────

/// One-pole low-pass IIR.
///   `y[n] = y[n-1] + α · (x[n] − y[n-1])`
///   `α    = dt / (RC + dt) ; RC = 1 / (2π·fc)`
pub struct LowPass<S: Source<Item = f32>> {
    inner: S,
    enabled: bool,
    alpha: f32,
    channels: usize,
    ch_idx: usize,
    prev_out: [f32; 8],
}

impl<S: Source<Item = f32>> LowPass<S> {
    pub fn new(inner: S, cutoff_hz: Option<u32>) -> Self {
        let channels = inner.channels() as usize;
        let sr = inner.sample_rate() as f32;
        let (enabled, alpha) = match cutoff_hz {
            Some(fc) if fc > 0 => {
                let dt = 1.0 / sr;
                let rc = 1.0 / (2.0 * std::f32::consts::PI * fc as f32);
                (true, dt / (rc + dt))
            }
            _ => (false, 1.0),
        };
        Self {
            inner,
            enabled,
            alpha,
            channels: channels.min(8).max(1),
            ch_idx: 0,
            prev_out: [0.0; 8],
        }
    }
}

impl<S: Source<Item = f32>> Iterator for LowPass<S> {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let x = self.inner.next()?;
        if !self.enabled {
            return Some(x);
        }
        let c = self.ch_idx;
        self.ch_idx += 1;
        if self.ch_idx >= self.channels {
            self.ch_idx = 0;
        }
        let y = self.prev_out[c] + self.alpha * (x - self.prev_out[c]);
        self.prev_out[c] = y;
        Some(y)
    }
}

impl<S: Source<Item = f32>> Source for LowPass<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

// ─── REVERB (FEEDBACK COMB) ──────────────────────────────────────────

/// Single-tap feedback comb filter that approximates a small room. Cheap
/// (one delay line per channel) but gets the "moisten the dry signal"
/// vibe across without any FFTs or convolution.
///
/// `mix` is the wet mix in 0..1. Internally chooses a sane delay length
/// (~120 ms) and a feedback level scaled with `mix` so even at full wet
/// the loop remains stable.
pub struct Reverb<S: Source<Item = f32>> {
    inner: S,
    enabled: bool,
    mix: f32,
    feedback: f32,
    delay: Vec<f32>,
    write: usize,
    channels: usize,
    ch_idx: usize,
}

impl<S: Source<Item = f32>> Reverb<S> {
    pub fn new(inner: S, mix: f32) -> Self {
        let channels = (inner.channels() as usize).min(8).max(1);
        let mix = mix.clamp(0.0, 1.0);
        let sr = inner.sample_rate().max(1) as usize;
        // ~120 ms per channel → buffer length = sr * 0.12 * channels
        let per_channel = (sr as f32 * 0.12).round().max(1.0) as usize;
        let delay = vec![0.0; per_channel.saturating_mul(channels)];
        let feedback = (mix * 0.55).min(0.7); // capped to stay stable
        Self {
            inner,
            enabled: mix > 1e-3,
            mix,
            feedback,
            delay,
            write: 0,
            channels,
            ch_idx: 0,
        }
    }
}

impl<S: Source<Item = f32>> Iterator for Reverb<S> {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let x = self.inner.next()?;
        if !self.enabled {
            return Some(x);
        }
        // Per-channel circular buffers, interleaved as L0,R0,L1,R1,...
        // To keep this a single Vec, we map (write_index, channel) →
        // index = write * channels + channel. Each channel sees its
        // own slot at the same `write` index across iterations.
        let idx = self.write * self.channels + self.ch_idx;
        let delayed = self.delay[idx];
        let wet = delayed;
        let out = x + self.mix * wet;
        // Feedback: store dry + scaled-back delayed sample so the line
        // decays naturally instead of ringing forever.
        self.delay[idx] = x + self.feedback * delayed;
        // Advance channel/write pointers.
        self.ch_idx += 1;
        if self.ch_idx >= self.channels {
            self.ch_idx = 0;
            self.write += 1;
            if self.write * self.channels >= self.delay.len() {
                self.write = 0;
            }
        }
        Some(out)
    }
}

impl<S: Source<Item = f32>> Source for Reverb<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

// ─── STEREO BUS (pan + volume + fades) ───────────────────────────────

/// Forces a stereo (2-channel) output and folds together the
/// final-stage parameters: pan, volume, optional fade-in, optional
/// fade-out, and an implicit mute (volume = 0). Mono input is duplicated
/// to L/R; multi-channel input is downmixed by averaging into stereo.
///
/// Sample timing is preserved: one *frame* of input (i.e. `channels()`
/// samples) becomes exactly two output samples (one L, one R), so the
/// sample rate stays identical and downstream timing of `delay`,
/// `take_duration`, etc. is unaffected.
pub struct Stereo<S: Source<Item = f32>> {
    inner: S,
    in_channels: u16,
    sample_rate: u32,
    /// Equal-power pan gain for L and R.
    left: f32,
    right: f32,
    fade_in_samples: u64, // 0 = no fade-in
    fade_out_samples: u64, // 0 = no fade-out
    total_samples: u64,    // 0 = unknown total; fade-out is then disabled
    /// Cached frame buffer (one input frame's worth of samples, mixed
    /// down to a single mono value).
    pending_right: Option<f32>,
    pos_samples: u64, // counts emitted *output* sample pairs
}

impl<S: Source<Item = f32>> Stereo<S> {
    pub fn new(
        inner: S,
        volume: f32,
        pan: f32,
        fade_in_samples: Option<u64>,
        fade_out_samples: Option<u64>,
        total_samples: Option<u64>,
    ) -> Self {
        // Equal-power pan law: theta = (pan + 1) * π/4 ∈ [0, π/2]
        let pan = pan.clamp(-1.0, 1.0);
        let theta = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let left = theta.cos() * volume * std::f32::consts::SQRT_2 * 0.5;
        let right = theta.sin() * volume * std::f32::consts::SQRT_2 * 0.5;
        let in_channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate();
        Self {
            inner,
            in_channels,
            sample_rate,
            left,
            right,
            fade_in_samples: fade_in_samples.unwrap_or(0),
            fade_out_samples: fade_out_samples.unwrap_or(0),
            total_samples: total_samples.unwrap_or(0),
            pending_right: None,
            pos_samples: 0,
        }
    }

    /// Compute the fade gain for the current output position. 1.0 when
    /// no fades are active or we're in the steady region.
    #[inline]
    fn fade_gain(&self) -> f32 {
        // Both fade segments are expressed in "output stereo frames"
        // (i.e. one fade sample corresponds to one L+R pair).
        let pos = self.pos_samples;
        let mut g = 1.0_f32;
        if self.fade_in_samples > 0 {
            if pos < self.fade_in_samples {
                g *= pos as f32 / self.fade_in_samples as f32;
            }
        }
        if self.fade_out_samples > 0 && self.total_samples > self.fade_out_samples {
            let fade_start = self.total_samples - self.fade_out_samples;
            if pos >= fade_start {
                let into_fade = pos - fade_start;
                let g_out =
                    1.0 - (into_fade as f32 / self.fade_out_samples as f32).clamp(0.0, 1.0);
                g *= g_out;
            }
        }
        g
    }

    /// Pull one frame from the input and downmix it to a single mono
    /// sample (averaging all channels). Returns None if the input is
    /// exhausted partway through a frame.
    fn next_mono(&mut self) -> Option<f32> {
        let mut acc = 0.0_f32;
        let mut got = 0_u32;
        for _ in 0..self.in_channels {
            match self.inner.next() {
                Some(v) => {
                    acc += v;
                    got += 1;
                }
                None => {
                    if got == 0 {
                        return None;
                    } else {
                        // Partial frame: average what we got.
                        break;
                    }
                }
            }
        }
        if got == 0 {
            None
        } else {
            Some(acc / got as f32)
        }
    }
}

impl<S: Source<Item = f32>> Iterator for Stereo<S> {
    type Item = f32;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(r) = self.pending_right.take() {
            // Right channel emission — apply the same fade gain that
            // was used for the matching left sample (we increment
            // `pos_samples` only after the *pair* is emitted).
            let g = self.fade_gain();
            self.pos_samples = self.pos_samples.saturating_add(1);
            return Some(r * self.right * g);
        }

        let mono = self.next_mono()?;
        let g = self.fade_gain();
        let l = mono * self.left * g;
        let r = mono; // raw mono cached; right gain + fade applied above
        self.pending_right = Some(r);
        Some(l)
    }
}

impl<S: Source<Item = f32>> Source for Stereo<S> {
    fn current_frame_len(&self) -> Option<usize> {
        // Output is always stereo, so report frame length in *output*
        // samples (which is roughly 2 × the input frame's mono frames).
        self.inner.current_frame_len().map(|n| {
            let frames = n / self.in_channels.max(1) as usize;
            frames.saturating_mul(2)
        })
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}
