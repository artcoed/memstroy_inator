//! Audio playback engine using rodio.
//!
//! The engine manages a set of parallel `rodio::Sink`s — one per active
//! audio source — so multiple tracks (scene audio + actor video soundtracks)
//! play together in sync with the timeline playhead.
//!
//! ## Per-track effect chain
//!
//! Each scheduled source flows through this chain (in order):
//!
//! 1. **Decode** to f32 samples.
//! 2. **Speed × pitch resample** — both knobs are folded into one rate
//!    factor: `effective_rate = speed * 2^(pitch_semitones / 12)`. We
//!    don't do time-stretching, so changing pitch shortens/lengthens
//!    the clip in wall-clock time the same way speed does. That's the
//!    classic editor "scrub speed + Mickey-Mouse" feel.
//! 3. **Skip into source_start** + the live playhead offset.
//! 4. **Take** only the visible duration.
//! 5. **High-pass filter** (one-pole IIR), if `high_pass_hz` is set.
//! 7. **Low-pass filter** (one-pole IIR), if `low_pass_hz` is set.
//! 8. **Reverb** (single-tap feedback comb), if `reverb > 0`.
//! 9. **Stereo splitter + pan + volume + fade in/out + mute** in a
//!    single combined adapter (see `dsp::Stereo`). Always forces
//!    output to 2 channels so panning is meaningful.
//! 10. **Delay** so the source becomes audible at the correct
//!     `t_in - playhead` offset.
//!
//! All custom adapters are no-ops when their parameter is at neutral,
//! so untouched tracks pay no extra DSP cost.
//!
//! ## Background loading
//!
//! Decoding into MP3 / AAC / WAV containers and `skip_duration`-ing into
//! mid-clip is **expensive** and was previously done synchronously on the
//! UI thread, which froze the editor for hundreds of ms (sometimes seconds
//! with multiple clips) every time the user hit Play. We now offload the
//! whole pipeline to a worker thread; sinks are constructed there and
//! shipped back to the UI through an mpsc channel. The UI calls
//! `poll_pending()` every frame to attach freshly-built sinks. A
//! `generation` counter lets stale loads (e.g. user hit Play, seeked, then
//! Play again before the first load finished) be discarded automatically.

use std::fs::File;
use std::io::BufReader;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use rodio::Source;
use tracing::{debug, info, warn};

mod dsp;

/// Global one-shot panic-hook installer. Suppresses the stderr "thread
/// 'memstroy-audio-load-N' panicked at ..." spam that rodio 0.19's
/// symphonia adapter emits when probing some MP4s with no/short audio.
/// We already `catch_unwind` those panics in the worker, but the default
/// hook still prints the panic message to stderr before the unwind starts.
/// Filter those out (route them through `tracing::debug!` instead) while
/// leaving every other panic untouched.
static AUDIO_PANIC_HOOK: Once = Once::new();

fn install_audio_panic_hook() {
    AUDIO_PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let on_audio_thread = thread::current()
                .name()
                .map(|n| n.starts_with("memstroy-audio-load-"))
                .unwrap_or(false);
            if on_audio_thread {
                debug!(
                    "Suppressed audio-worker panic ({}): {}",
                    thread::current().name().unwrap_or("?"),
                    info
                );
                return;
            }
            prev(info);
        }));
    });
}

/// One scheduled audio source on the timeline. Used by the editor to
/// describe scene-level audio tracks AND actor video soundtracks.
#[derive(Debug, Clone)]
pub struct AudioSourceSpec {
    pub path: PathBuf,
    /// When in scene time this source becomes audible.
    pub t_in: f32,
    /// When in scene time this source ends. `None` = play to end of file.
    pub t_out: Option<f32>,
    /// Offset into the source file at which `t_in` should map.
    pub source_start: f32,
    /// Per-track linear gain (0.0..N).
    pub volume: f32,
    /// Per-track playback rate (1.0 = normal, 2.0 = double speed).
    /// Combined with `pitch_semitones` to form the effective resample
    /// rate (`speed * 2^(pitch/12)`).
    pub speed: f32,
    /// Pitch shift in semitones (12 = +1 octave). 0 = neutral.
    pub pitch_semitones: f32,
    /// Stereo pan, -1.0 = full left, +1.0 = full right.
    pub pan: f32,
    /// One-pole low-pass cutoff in Hz; `None` disables.
    pub low_pass_hz: Option<u32>,
    /// One-pole high-pass cutoff in Hz; `None` disables.
    pub high_pass_hz: Option<u32>,
    /// Linear fade-in length in scene seconds. 0 = no fade.
    pub fade_in: f32,
    /// Linear fade-out length in scene seconds. 0 = no fade.
    /// Only effective when `t_out` is set.
    pub fade_out: f32,
    /// Mute the source without removing it from the schedule.
    pub mute: bool,
    /// Reverb mix (0..1). 0 = dry.
    pub reverb: f32,
}

impl Default for AudioSourceSpec {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            t_in: 0.0,
            t_out: None,
            source_start: 0.0,
            volume: 1.0,
            speed: 1.0,
            pitch_semitones: 0.0,
            pan: 0.0,
            low_pass_hz: None,
            high_pass_hz: None,
            fade_in: 0.0,
            fade_out: 0.0,
            mute: false,
            reverb: 0.0,
        }
    }
}

impl AudioSourceSpec {
    /// Stable, hash-friendly fingerprint of every field that influences
    /// the rodio sink. Used by `app.rs` to decide whether the running
    /// playback needs to be rebuilt because the user just touched an
    /// audio inspector slider mid-playback (volume, pan, pitch, …).
    /// Two specs with the same `signature` are guaranteed to produce
    /// the same audible output, so we can short-circuit the rebuild.
    pub fn signature(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.path.hash(&mut h);
        self.t_in.to_bits().hash(&mut h);
        match self.t_out {
            Some(v) => {
                1u8.hash(&mut h);
                v.to_bits().hash(&mut h);
            }
            None => 0u8.hash(&mut h),
        }
        self.source_start.to_bits().hash(&mut h);
        self.volume.to_bits().hash(&mut h);
        self.speed.to_bits().hash(&mut h);
        self.pitch_semitones.to_bits().hash(&mut h);
        self.pan.to_bits().hash(&mut h);
        self.low_pass_hz.hash(&mut h);
        self.high_pass_hz.hash(&mut h);
        self.fade_in.to_bits().hash(&mut h);
        self.fade_out.to_bits().hash(&mut h);
        self.mute.hash(&mut h);
        self.reverb.to_bits().hash(&mut h);
        h.finish()
    }
}

/// A pre-built sink that the worker thread hands back, ready to be
/// `play()`-ed by the UI thread. Wrapped so we can ferry through a
/// `mpsc::Sender` on a thread with no `Sync` requirements.
struct ReadySink {
    sink: rodio::Sink,
}

/// Audio playback engine. Owns the rodio output stream + a fleet of sinks.
pub struct AudioEngine {
    /// Whether playback is currently running.
    playing: bool,
    /// Master gain (multiplied with each source's per-track volume).
    volume: f32,
    /// Audio output stream (kept alive to prevent dropping).
    _stream: Option<rodio::OutputStream>,
    /// Stream handle for creating sinks.
    stream_handle: Option<rodio::OutputStreamHandle>,
    /// One sink per currently scheduled source (so they play in parallel).
    sinks: Vec<rodio::Sink>,
    /// Bumped on every `play_sources`/`stop`/`pause` so background workers
    /// know whether their results are still wanted.
    generation: u64,
    /// Receiver for sinks built on the worker thread. Tagged with the
    /// generation that started the load so we can discard stale sinks.
    pending: Option<(u64, mpsc::Receiver<ReadySink>)>,
}

impl AudioEngine {
    /// Create a new audio engine. Initialises the output stream.
    pub fn new() -> Self {
        install_audio_panic_hook();
        let (stream, handle) = match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => {
                info!("Audio engine initialised");
                (Some(stream), Some(handle))
            }
            Err(e) => {
                warn!("Failed to initialise audio output: {}", e);
                (None, None)
            }
        };
        Self {
            playing: false,
            volume: 1.0,
            _stream: stream,
            stream_handle: handle,
            sinks: Vec::new(),
            generation: 0,
            pending: None,
        }
    }

    /// Stop and drop every currently active sink.
    fn stop_all_sinks(&mut self) {
        for sink in self.sinks.drain(..) {
            sink.stop();
        }
    }

    /// Start (or restart) playback from `playhead` mixing every source in
    /// `sources` whose timing window overlaps the future timeline.
    ///
    /// **This call returns immediately.** The actual decode + sink
    /// construction happens on a worker thread; sinks are attached
    /// progressively as they become ready (see [`Self::poll_pending`]).
    pub fn play_sources(&mut self, sources: &[AudioSourceSpec], playhead: f32) {
        self.stop_all_sinks();
        self.generation = self.generation.wrapping_add(1);
        let gen = self.generation;
        self.pending = None;

        let Some(handle) = self.stream_handle.clone() else {
            warn!("No audio stream handle available");
            return;
        };
        let master_volume = self.volume;
        let specs: Vec<AudioSourceSpec> = sources.to_vec();
        let any_specs = !specs.is_empty();

        let (tx, rx) = mpsc::channel::<ReadySink>();
        let started_at = Instant::now();
        let _ = thread::Builder::new()
            .name(format!("memstroy-audio-load-{}", gen))
            .spawn(move || {
                Self::load_sinks(&handle, &specs, playhead, master_volume, started_at, tx);
            });

        self.pending = Some((gen, rx));
        self.playing = any_specs;
    }

    /// Background worker entry: build each spec's sink in turn and send it
    /// back through `tx`. We send sinks one-by-one so the *first* track
    /// becomes audible as soon as it's ready instead of waiting for the
    /// whole batch.
    fn load_sinks(
        handle: &rodio::OutputStreamHandle,
        specs: &[AudioSourceSpec],
        playhead: f32,
        master_volume: f32,
        started_at: Instant,
        tx: mpsc::Sender<ReadySink>,
    ) {
        for spec in specs {
            // Mute = skip entirely so we don't waste a sink on silence.
            if spec.mute {
                continue;
            }
            // Compensate for time elapsed during background decode so the
            // already-running visual playhead doesn't drift away from audio.
            let elapsed = started_at.elapsed().as_secs_f32();
            let live_playhead = playhead + elapsed;

            if let Some(t_out) = spec.t_out {
                if live_playhead >= t_out {
                    continue;
                }
            }
            if !spec.path.exists() {
                continue;
            }

            // Container fallback: extract audio via ffmpeg for files that
            // rodio's symphonia adapter can't safely probe.
            let resolved_path = match resolve_audio_source(&spec.path) {
                Some(p) => p,
                None => {
                    debug!(
                        "Skipping audio source (no decodable stream): {}",
                        spec.path.display()
                    );
                    continue;
                }
            };

            let file = match File::open(&resolved_path) {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        "Failed to open audio file {}: {}",
                        resolved_path.display(),
                        e
                    );
                    continue;
                }
            };
            if let Ok(meta) = file.metadata() {
                if meta.len() < 64 {
                    debug!(
                        "Skipping audio file (too small to decode): {}",
                        spec.path.display()
                    );
                    continue;
                }
            }
            let reader = BufReader::new(file);
            let decoder_result =
                std::panic::catch_unwind(AssertUnwindSafe(|| rodio::Decoder::new(reader)));
            let decoder = match decoder_result {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    debug!("No decodable audio in {}: {}", spec.path.display(), e);
                    continue;
                }
                Err(_) => {
                    warn!(
                        "Audio decoder panicked while probing {}; skipping.",
                        spec.path.display()
                    );
                    continue;
                }
            };

            let sink = match rodio::Sink::try_new(handle) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to create audio sink: {}", e);
                    continue;
                }
            };
            sink.set_volume(master_volume);
            sink.pause();

            // ── Build the per-track effect chain. ─────────────────────
            let stream = decoder.convert_samples::<f32>();

            // (1) Resample factor combines speed and pitch.
            let pitch_factor = 2.0_f32.powf(spec.pitch_semitones / 12.0);
            let rate = (spec.speed * pitch_factor).max(0.05);
            let stream = stream.speed(rate);

            // (2) Skip into the source. After `speed(rate)`, the stream's
            // time axis is in wall-clock (output) seconds:
            //   - `skip_duration(d)` skips `d` seconds of output time.
            //   - `source_start` is in SOURCE seconds; at the modified
            //     rate, those source seconds occupy `source_start / rate`
            //     output seconds.
            //   - `(live_playhead - t_in)` is already in scene/output time.
            let elapsed_scene = (live_playhead - spec.t_in).max(0.0);
            let skip_output_secs = (spec.source_start / rate + elapsed_scene).max(0.0);
            let stream = stream.skip_duration(Duration::from_secs_f32(skip_output_secs));

            // (3) Take only the visible duration. The audio engine no
            // longer loops sources — when the visible window outlasts
            // the file the tail is silent, which matches the inspector
            // (the loop toggle was removed).
            let stream: Box<dyn Source<Item = f32> + Send> = Box::new(stream);
            let stream = dsp::DynSource::new(stream);

            // (4) Take only the visible duration.
            // After `speed(rate)`, `take_duration(d)` takes `d` seconds
            // of output/wall-clock time. The visible window is already
            // in scene time (= output time), so use it directly.
            let take_secs = spec.t_out.map(|end| {
                let visible_start = live_playhead.max(spec.t_in);
                (end - visible_start).max(0.0)
            });

            let stream: Box<dyn Source<Item = f32> + Send> = match take_secs {
                Some(td) if td > 0.0 => {
                    Box::new(stream.take_duration(Duration::from_secs_f32(td)))
                }
                _ => Box::new(stream),
            };
            let stream = dsp::DynSource::new(stream);

            // (5) High-pass filter.
            let stream = dsp::HighPass::new(stream, spec.high_pass_hz);
            // (6) Low-pass filter.
            let stream = dsp::LowPass::new(stream, spec.low_pass_hz);
            // (7) Reverb.
            let stream = dsp::Reverb::new(stream, spec.reverb);

            // (8) Final stereo bus: pan + volume + fade in/out.
            // After `speed(rate)`, `stream.sample_rate()` reports the
            // modified rate (original_sr * rate). Fade durations are in
            // scene-time seconds; converting to sample counts at the
            // stream's reported rate gives the correct number of samples
            // that will flow through the Stereo adapter during that
            // wall-clock interval.
            let sr = stream.sample_rate() as f32;
            let fade_in_samples = if spec.fade_in > 0.0 {
                Some((spec.fade_in * sr) as u64)
            } else {
                None
            };
            let total_samples = take_secs
                .map(|td| (td * sr) as u64);
            let fade_out_samples = if spec.fade_out > 0.0 && total_samples.is_some() {
                Some((spec.fade_out * sr) as u64)
            } else {
                None
            };

            let stream = dsp::Stereo::new(
                stream,
                spec.volume.max(0.0),
                spec.pan,
                fade_in_samples,
                fade_out_samples,
                total_samples,
            );

            // (9) Delay until t_in (only when scheduling something that
            // hasn't started yet).
            let delay_secs = (spec.t_in - live_playhead).max(0.0);
            if delay_secs > 0.0 {
                sink.append(stream.delay(Duration::from_secs_f32(delay_secs)));
            } else {
                sink.append(stream);
            }

            if tx.send(ReadySink { sink }).is_err() {
                return;
            }
        }
    }

    /// Drain any sinks the worker thread has finished building and start
    /// them. Call this every UI frame.
    pub fn poll_pending(&mut self) {
        let cur_gen = self.generation;
        let Some((gen, rx)) = self.pending.take() else { return };
        if gen != cur_gen {
            for ready in rx.try_iter() {
                ready.sink.stop();
            }
            return;
        }
        let mut still_open = true;
        loop {
            match rx.try_recv() {
                Ok(ready) => {
                    ready.sink.play();
                    self.sinks.push(ready.sink);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    still_open = false;
                    break;
                }
            }
        }
        if still_open {
            self.pending = Some((gen, rx));
        }
    }

    /// Pause every sink without dropping it (so resume picks up where it left off).
    pub fn pause(&mut self) {
        for sink in &self.sinks {
            sink.pause();
        }
        self.playing = false;
    }

    /// Stop all playback and release every sink, including any background
    /// load that hasn't yet attached. Currently unused: the editor pauses
    /// rather than stops on every transport interaction; kept in the
    /// public API in case a future "rewind to t=0 and reset state"
    /// shortcut wants it.
    #[allow(dead_code)]
    pub fn stop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.stop_all_sinks();
        self.playing = false;
    }

    /// Set the master volume (0.0 .. 1.0). Per-track volumes still apply.
    pub fn set_master_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        for sink in &self.sinks {
            sink.set_volume(self.volume);
        }
    }

    /// Whether any sink is currently active and not paused.
    #[allow(dead_code)]
    pub fn is_playing(&self) -> bool {
        self.playing
    }
}

/// Resolve a scene audio path to something rodio can decode without
/// panicking.
///
/// Policy: **whitelist** the codecs whose `symphonia` adapters we trust
/// to probe + decode in-process without panicking, and route everything
/// else (every MP4-family container — `m4a` / `mp4` / `mov` / `m4v` /
/// `3gp` / `3g2` — plus matroska, webm, AVI, FLV, opus, aiff, caf, and
/// any unknown extension) through ffmpeg → WAV first.
///
/// Why a whitelist rather than a blacklist: rodio 0.19's
/// `symphonia-isomp4` adapter is known to panic during *probe* (and
/// occasionally mid-decode) on some valid MP4 files — and `m4a` is just
/// MP4 with an audio-only track, so it hits the same code path. Because
/// the release profile is built with `panic = "abort"`, any panic that
/// escapes the worker thread (including any panic raised on rodio's
/// own internal playback thread, which we cannot wrap in
/// `catch_unwind`) takes the whole editor down. Pre-extracting to PCM
/// WAV via ffmpeg sidesteps the buggy probe path entirely and gives
/// every downstream stage a stable, well-formed source.
///
/// The extracted WAV is cached under the OS temp dir keyed on
/// (stem, size, mtime), so the cost is paid only on the first import
/// of each file.
fn resolve_audio_source(src: &Path) -> Option<PathBuf> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    // Stable native decoders: raw single-stream containers whose
    // symphonia adapters have been reliable across our test corpus.
    // Anything outside this list — notably `m4a` and other MP4-family
    // audio — falls through to the ffmpeg pre-extract below.
    let safe_native =
        matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg" | "oga" | "aac");
    if safe_native {
        return Some(src.to_path_buf());
    }

    let meta = std::fs::metadata(src).ok()?;
    let size = meta.len();
    if size < 64 {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio")
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let cache_dir = std::env::temp_dir().join("memstroy_audio_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let cache_name = format!("{}_{}_{}.wav", stem, size, mtime);
    let cache_path = cache_dir.join(cache_name);

    if cache_path.exists()
        && std::fs::metadata(&cache_path)
            .map(|m| m.len() > 1024)
            .unwrap_or(false)
    {
        return Some(cache_path);
    }

    let ffmpeg = memstroy_render::ffmpeg_binary();
    let status = {
        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(src)
            .args(["-vn", "-ac", "2", "-ar", "44100", "-sn", "-dn", "-f", "wav"])
            .arg(&cache_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        memstroy_render::hide_console_std(&mut cmd).status()
    };

    match status {
        Ok(s) if s.success() => {
            let len = std::fs::metadata(&cache_path).map(|m| m.len()).unwrap_or(0);
            if len > 1024 {
                debug!(
                    "Extracted audio cache for {} -> {} ({} bytes)",
                    src.display(),
                    cache_path.display(),
                    len
                );
                Some(cache_path)
            } else {
                let _ = std::fs::remove_file(&cache_path);
                None
            }
        }
        Ok(_) | Err(_) => {
            let _ = std::fs::remove_file(&cache_path);
            None
        }
    }
}
