//! Audio playback engine using rodio.
//! Decodes audio tracks and plays them in sync with the timeline playhead.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use rodio::Source;
use tracing::{info, warn};

/// Audio engine state shared between UI and audio thread.
pub struct AudioEngine {
    /// Whether audio is currently playing
    playing: bool,
    /// Current playback position (seconds)
    position: f32,
    /// Volume (0.0 - 1.0)
    volume: f32,
    /// Audio output stream (kept alive to prevent dropping)
    _stream: Option<rodio::OutputStream>,
    /// Stream handle for creating sinks
    stream_handle: Option<rodio::OutputStreamHandle>,
    /// Sink for controlling playback
    sink: Option<rodio::Sink>,
}

impl AudioEngine {
    /// Create a new audio engine. Initializes the output stream and sink.
    pub fn new() -> Self {
        let (stream, handle, sink) = match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => {
                let sink = rodio::Sink::try_new(&handle).ok();
                info!("Audio engine initialized successfully");
                (Some(stream), Some(handle), sink)
            }
            Err(e) => {
                warn!("Failed to initialize audio output: {}", e);
                (None, None, None)
            }
        };

        Self {
            playing: false,
            position: 0.0,
            volume: 1.0,
            _stream: stream,
            stream_handle: handle,
            sink,
        }
    }

    /// Play audio sources. Each tuple is (file_path, t_in, volume).
    /// Opens each file, decodes it, seeks to current position, applies volume.
    pub fn play(&mut self, sources: &[(PathBuf, f32, f32)]) {
        self.play_from(sources, 0.0);
    }

    /// Play audio sources from a specific time offset.
    pub fn play_from(&mut self, sources: &[(PathBuf, f32, f32)], playhead: f32) {
        // Stop any current playback first
        if let Some(sink) = &self.sink {
            sink.stop();
        }

        // Create a new sink for this playback session
        let Some(handle) = &self.stream_handle else {
            warn!("No audio stream handle available");
            return;
        };

        let sink = match rodio::Sink::try_new(handle) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to create audio sink: {}", e);
                return;
            }
        };

        sink.set_volume(self.volume);

        for (path, t_in, vol) in sources {
            let file = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to open audio file {:?}: {}", path, e);
                    continue;
                }
            };

            let reader = BufReader::new(file);
            match rodio::Decoder::new(reader) {
                Ok(source) => {
                    // Skip to the correct position: playhead relative to track start
                    let skip_secs = (playhead - t_in).max(0.0);
                    let skip_duration = std::time::Duration::from_secs_f32(skip_secs);
                    let skipped = source.skip_duration(skip_duration);
                    let amplified = skipped.amplify(*vol);
                    sink.append(amplified);
                    info!("Added audio source: {:?} (skip {:.2}s)", path, skip_secs);
                }
                Err(e) => {
                    warn!("Failed to decode audio file {:?}: {}", path, e);
                }
            }
        }

        sink.play();
        self.sink = Some(sink);
        self.playing = true;
    }

    /// Pause audio playback.
    pub fn pause(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
        self.playing = false;
    }

    /// Seek to a new position. Since rodio doesn't support seeking natively,
    /// we stop current playback. The caller should re-invoke play() with
    /// the updated time offset if audio should resume from the new position.
    pub fn seek(&mut self, t: f32) {
        self.position = t;
        // Stop current playback - caller should re-trigger play() if needed
        if let Some(sink) = &self.sink {
            sink.stop();
        }
        // Re-create a fresh sink for next play
        if let Some(handle) = &self.stream_handle {
            self.sink = rodio::Sink::try_new(handle).ok();
        }
        self.playing = false;
    }

    /// Set playback volume (0.0 - 1.0).
    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(sink) = &self.sink {
            sink.set_volume(self.volume);
        }
    }

    /// Returns whether audio is currently playing.
    pub fn is_playing(&self) -> bool {
        self.playing
    }
}
