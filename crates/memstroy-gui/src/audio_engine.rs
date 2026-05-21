//! Audio playback engine using rodio.
//!
//! The engine manages a set of parallel `rodio::Sink`s — one per active
//! audio source — so multiple tracks (scene audio + actor video soundtracks)
//! play together in sync with the timeline playhead. Each source is opened,
//! decoded, seeked into via `skip_duration`, optionally trimmed via
//! `take_duration`, optionally delayed via `delay`, and amplified to its
//! per-track volume.
//!
//! On play-start or after a seek, the engine tears down all current sinks
//! and rebuilds them from scratch at the new playhead.
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
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use rodio::Source;
use tracing::{debug, info, warn};

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
                // Audio worker hit a decoder panic — already handled by
                // catch_unwind in load_sinks, no need to scare the user.
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
    /// Per-track playback rate (1.0 = normal, 2.0 = double speed +
    /// octave-up pitch). Applied via `Source::speed`.
    pub speed: f32,
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
        // Quiet down the rodio/symphonia decoder panics that we already
        // catch_unwind on the worker thread.
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
        // Tear down whatever was playing instantly so the user perceives
        // the seek/restart as snappy.
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
            // Compensate for time elapsed during background decode so the
            // already-running visual playhead doesn't drift away from audio.
            let elapsed = started_at.elapsed().as_secs_f32();
            let live_playhead = playhead + elapsed;

            if let Some(t_out) = spec.t_out {
                if live_playhead >= t_out { continue; }
            }
            if !spec.path.exists() { continue; }

            let file = match File::open(&spec.path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to open audio file {}: {}", spec.path.display(), e);
                    continue;
                }
            };
            // Reject obviously empty / tiny files outright. rodio 0.19 + symphonia
            // 0.5 will sometimes panic on init when the underlying source is too
            // short to probe (the panic site is `unreachable!("Seek errors should
            // not occur during initialization")` in rodio's symphonia adapter).
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
            // `rodio::Decoder::new` can panic — not just return Err — when
            // symphonia hits an unexpected internal seek error during probe.
            // Wrap the call in `catch_unwind` so a single bad attachment
            // (e.g. a video file with no audio stream, a partial download,
            // or an unsupported codec) doesn't take down the whole audio
            // worker thread.
            let decoder_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                rodio::Decoder::new(reader)
            }));
            let decoder = match decoder_result {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    // Many image / unsupported files end up here when an
                    // "actor" path doesn't actually have an audio stream.
                    debug!("No decodable audio in {}: {}", spec.path.display(), e);
                    continue;
                }
                Err(_) => {
                    // Decoder panicked while probing the source. Skip it
                    // gracefully instead of crashing the worker thread.
                    warn!(
                        "Audio decoder panicked while probing {}; skipping.",
                        spec.path.display()
                    );
                    continue;
                }
            };

            let stream = decoder.convert_samples::<f32>();
            // Apply per-track speed at the source level. `speed` divides
            // every internal duration we hand the source (skip, take,
            // delay) so time still maps to the user's scene timeline:
            // the audio plays N× faster but starts/ends at the same
            // visible scene-times.
            let speed = spec.speed.max(0.05);
            let stream = stream.speed(speed);
            let skip_secs = (spec.source_start
                + (live_playhead - spec.t_in).max(0.0))
                .max(0.0);
            // skip / take / delay are all stated in *scene-time* seconds
            // by the caller; multiply by `speed` to recover the
            // wall-clock seconds the now-faster source needs to consume.
            let stream = stream.skip_duration(Duration::from_secs_f32(skip_secs * speed));
            let stream = stream.amplify(spec.volume.max(0.0));
            let take_secs = spec.t_out.map(|end| {
                let visible_start = live_playhead.max(spec.t_in);
                (end - visible_start).max(0.0)
            });
            let delay_secs = (spec.t_in - live_playhead).max(0.0);

            let sink = match rodio::Sink::try_new(handle) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to create audio sink: {}", e);
                    continue;
                }
            };
            sink.set_volume(master_volume);
            sink.pause(); // start paused so the UI thread starts us at the right moment

            match (take_secs, delay_secs > 0.0) {
                (Some(td), true) => sink.append(
                    stream
                        .take_duration(Duration::from_secs_f32(td * speed))
                        .delay(Duration::from_secs_f32(delay_secs)),
                ),
                (Some(td), false) => sink.append(
                    stream.take_duration(Duration::from_secs_f32(td * speed)),
                ),
                (None, true) => sink.append(
                    stream.delay(Duration::from_secs_f32(delay_secs)),
                ),
                (None, false) => sink.append(stream),
            }

            // If the receiver was dropped (next play_sources arrived), bail.
            if tx.send(ReadySink { sink }).is_err() {
                return;
            }
        }
    }

    /// Drain any sinks the worker thread has finished building and start
    /// them. Call this every UI frame.
    pub fn poll_pending(&mut self) {
        // Snapshot the current generation up front so we don't hold a borrow
        // on `self.pending` while we mutate `self.sinks`.
        let cur_gen = self.generation;
        let Some((gen, rx)) = self.pending.take() else { return };
        if gen != cur_gen {
            // A newer play_sources call superseded this one: drop everything.
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
    /// load that hasn't yet attached.
    pub fn stop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.stop_all_sinks();
        self.playing = false;
    }

    /// Set the master volume (0.0 .. 1.0). Per-track volumes still apply.
    #[allow(dead_code)]
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
