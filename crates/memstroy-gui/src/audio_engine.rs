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

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;

use rodio::Source;
use tracing::{info, warn};

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
}

impl AudioEngine {
    /// Create a new audio engine. Initialises the output stream.
    pub fn new() -> Self {
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
    pub fn play_sources(&mut self, sources: &[AudioSourceSpec], playhead: f32) {
        self.stop_all_sinks();

        let Some(handle) = self.stream_handle.clone() else {
            warn!("No audio stream handle available");
            return;
        };

        for spec in sources {
            // Skip sources that have already ended.
            if let Some(t_out) = spec.t_out {
                if playhead >= t_out { continue; }
            }
            // Skip nonexistent files cleanly.
            if !spec.path.exists() {
                continue;
            }

            let file = match File::open(&spec.path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to open audio file {}: {}", spec.path.display(), e);
                    continue;
                }
            };
            let reader = BufReader::new(file);
            let decoder = match rodio::Decoder::new(reader) {
                Ok(d) => d,
                Err(e) => {
                    // Many image / unsupported files end up here when an
                    // "actor" path doesn't actually have an audio stream.
                    // Log at debug level and skip silently.
                    info!("No decodable audio in {}: {}", spec.path.display(), e);
                    continue;
                }
            };

            // Convert to f32 samples so adapters compose uniformly.
            let stream = decoder.convert_samples::<f32>();

            // Skip into the source: source_start + (how much of t_in we missed).
            let skip_secs = (spec.source_start + (playhead - spec.t_in).max(0.0)).max(0.0);
            let stream = stream.skip_duration(Duration::from_secs_f32(skip_secs));

            // Per-track gain. Engine master volume is applied at the sink level.
            let stream = stream.amplify(spec.volume.max(0.0));

            // Trim to the visible window if `t_out` is set.
            let take_secs = spec.t_out.map(|end| {
                let visible_start = playhead.max(spec.t_in);
                (end - visible_start).max(0.0)
            });

            // Delay if the source begins later than the current playhead.
            let delay_secs = (spec.t_in - playhead).max(0.0);

            let sink = match rodio::Sink::try_new(&handle) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to create audio sink: {}", e);
                    continue;
                }
            };
            sink.set_volume(self.volume);

            // Compose the optional adapters; each branch ends with a single
            // `sink.append(...)` call so type inference stays simple.
            match (take_secs, delay_secs > 0.0) {
                (Some(td), true) => sink.append(
                    stream
                        .take_duration(Duration::from_secs_f32(td))
                        .delay(Duration::from_secs_f32(delay_secs))
                ),
                (Some(td), false) => sink.append(
                    stream.take_duration(Duration::from_secs_f32(td))
                ),
                (None, true) => sink.append(
                    stream.delay(Duration::from_secs_f32(delay_secs))
                ),
                (None, false) => sink.append(stream),
            }

            sink.play();
            self.sinks.push(sink);
        }

        self.playing = !self.sinks.is_empty();
    }

    /// Pause every sink without dropping it (so resume picks up where it left off).
    pub fn pause(&mut self) {
        for sink in &self.sinks {
            sink.pause();
        }
        self.playing = false;
    }

    /// Stop all playback and release every sink.
    pub fn stop(&mut self) {
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
