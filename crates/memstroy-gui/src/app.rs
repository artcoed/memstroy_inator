//! Main eframe application: wires panels together and dispatches jobs.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, ViewportCommand, Rounding, Stroke, Vec2};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::jobs::{spawn_preview, spawn_refresh, spawn_render, JobEvent};
use crate::node_editor::NodeEditor;
use crate::panels;
use crate::state::{EditorState, Selection};

pub struct App {
    rt: Runtime,
    state: EditorState,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
    node_editor: NodeEditor,
    /// Per-actor extraction results. Key = actor index.
    frame_extract_results: Vec<Arc<Mutex<Option<(f32, usize, std::path::PathBuf)>>>>,
    /// Per-audio-track waveform extraction results.
    waveform_extract_results: Vec<Arc<Mutex<Option<(Vec<f32>, f32)>>>>,
}

impl App {
    pub fn new(rt: Runtime) -> Self {
        let (tx, rx) = channel();
        let mut state = EditorState::new();
        state.reload_library();
        Self {
            rt,
            state,
            tx,
            rx,
            node_editor: NodeEditor::default(),
            frame_extract_results: Vec::new(),
            waveform_extract_results: Vec::new(),
        }
    }

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                JobEvent::Status(s) => self.state.status = s,
                JobEvent::PreviewReady(p) => {
                    self.state.last_preview = Some(p);
                    self.state.preview_rendering = false;
                    ctx.forget_all_images();
                }
                JobEvent::PreviewFailed(e) => {
                    self.state.preview_rendering = false;
                    self.state.status = format!("\u{274C} Preview failed: {}", e);
                }
                JobEvent::RenderLog(line) => {
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.last_log = line;
                    }
                }
                JobEvent::RenderFinished(Ok(p)) => {
                    self.state.status = format!("\u{2705} Rendered: {}", p.display());
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                    }
                }
                JobEvent::RenderFinished(Err(e)) => {
                    self.state.status = format!("\u{274C} Render failed: {}", e);
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                        rp.error = Some(e);
                    }
                }
                JobEvent::RefreshProgress(msg) => {
                    self.state.status = format!("\u{1F504} {}", msg);
                }
                JobEvent::RefreshFinished(Ok(summary)) => {
                    self.state.refreshing = false;
                    self.state.reload_library();
                    self.state.status = format!(
                        "\u{1F389} Refresh done! {} new clips, {} total in library",
                        summary.new_clips, summary.total_clips
                    );
                    if summary.failed > 0 {
                        self.state.status.push_str(&format!(
                            " ({} failed)",
                            summary.failed
                        ));
                    }
                }
                JobEvent::RefreshFinished(Err(e)) => {
                    self.state.refreshing = false;
                    self.state.status = format!("\u{274C} Refresh failed: {}", e);
                }
            }
        }
    }

    fn menu(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button(RichText::new("\u{1F4C1} File").strong(), |ui| {
                if ui.button("\u{2728} New scene").clicked() {
                    self.state.scene = Scene::default();
                    self.state.scene_path = None;
                    self.state.status = "\u{2728} New scene created.".into();
                    ui.close_menu();
                }
                if ui.button("\u{1F4C2} Open scene...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Scene", &["yaml", "yml", "json"])
                        .pick_file()
                    {
                        match Scene::load(&path) {
                            Ok(s) => {
                                self.state.scene = s;
                                self.state.scene_path = Some(path);
                                self.state.status = "\u{2705} Scene loaded.".into();
                            }
                            Err(e) => self.state.status = format!("\u{274C} Open failed: {e}"),
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("\u{1F4BE} Save scene").clicked() {
                    self.save_scene();
                    ui.close_menu();
                }
                if ui.button("\u{1F4BE} Save scene as...").clicked() {
                    self.save_as();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("\u{1F6AA} Exit").clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });

            ui.menu_button(RichText::new("\u{1F3AC} Render").strong(), |ui| {
                if ui.button("\u{1F5BC} Preview frame").clicked() {
                    self.run_preview();
                    ui.close_menu();
                }
                if ui.button("\u{1F3A5} Render full clip...").clicked() {
                    self.run_render();
                    ui.close_menu();
                }
            });

            ui.menu_button(RichText::new("\u{1F9E0} Tools").strong(), |ui| {
                if ui.button("\u{1F9CD} Detect anchors (pose)...").clicked() {
                    self.state.status =
                        "\u{1F6A7} Pose detection: ONNX backend coming in next iteration."
                            .into();
                    ui.close_menu();
                }
            });

            // Status indicator on the right
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.state.refreshing {
                    ui.spinner();
                    ui.label(RichText::new("refreshing...").color(Color32::from_rgb(255, 200, 50)).size(11.0));
                } else if let Some(rp) = &self.state.render_progress {
                    if !rp.done {
                        ui.spinner();
                        ui.label(RichText::new("rendering...").color(Color32::from_rgb(100, 200, 255)).size(11.0));
                    }
                }
                // Show status text (trimmed)
                let status = &self.state.status;
                if !status.is_empty() && !status.starts_with("__") {
                    let display = if status.chars().count() > 60 {
                        let t: String = status.chars().take(57).collect();
                        format!("{}...", t)
                    } else {
                        status.clone()
                    };
                    ui.label(
                        RichText::new(display)
                            .size(11.0)
                            .color(Color32::from_rgb(160, 160, 180)),
                    );
                }
            });
        });
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let modifiers = ctx.input(|i| i.modifiers);
        let ctrl = modifiers.ctrl || modifiers.mac_cmd;

        ctx.input(|i| {
            // Space = Play/Pause
            if i.key_pressed(egui::Key::Space) {
                self.state.playing = !self.state.playing;
                if self.state.playing {
                    self.state.status = "\u{25B6} Playing".into();
                } else {
                    self.state.status = "\u{23F8} Paused".into();
                }
            }
            // Ctrl+Z = Undo
            if ctrl && i.key_pressed(egui::Key::Z) && !modifiers.shift {
                self.state.undo();
            }
            // Ctrl+Shift+Z or Ctrl+Y = Redo
            if ctrl && ((i.key_pressed(egui::Key::Z) && modifiers.shift) || i.key_pressed(egui::Key::Y)) {
                self.state.redo();
            }
            // Delete key = remove selected element
            if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace) {
                self.delete_selected();
            }
            // Ctrl+D = duplicate selected
            if ctrl && i.key_pressed(egui::Key::D) {
                self.duplicate_selected();
            }
        });
    }

    fn delete_selected(&mut self) {
        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                self.state.mutate(|s| { s.actors.remove(i); });
                self.state.selection = Selection::None;
                self.state.status = "\u{1F5D1} Actor deleted.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                self.state.mutate(|s| { s.overlays.remove(i); });
                self.state.selection = Selection::None;
                self.state.status = "\u{1F5D1} Overlay deleted.".into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                self.state.mutate(|s| { s.backgrounds.remove(i); });
                self.state.selection = Selection::None;
                self.state.status = "\u{1F5D1} Background deleted.".into();
            }
            _ => {}
        }
    }

    fn duplicate_selected(&mut self) {
        match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let mut dup = self.state.scene.actors[i].clone();
                dup.id = format!("{}_copy", dup.id);
                let new_idx = self.state.scene.actors.len();
                self.state.mutate(move |s| { s.actors.push(dup); });
                self.state.selection = Selection::Actor(new_idx);
                self.state.status = "\u{1F4CB} Actor duplicated.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let mut dup = self.state.scene.overlays[i].clone();
                match &mut dup {
                    memstroy_core::Overlay::Text(t) => t.id = format!("{}_copy", t.id),
                    memstroy_core::Overlay::Image(im) => im.id = format!("{}_copy", im.id),
                    memstroy_core::Overlay::Video(v) => v.id = format!("{}_copy", v.id),
                }
                let new_idx = self.state.scene.overlays.len();
                self.state.mutate(move |s| { s.overlays.push(dup); });
                self.state.selection = Selection::Overlay(new_idx);
                self.state.status = "\u{1F4CB} Overlay duplicated.".into();
            }
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                let mut dup = self.state.scene.backgrounds[i].clone();
                dup.id = format!("{}_copy", dup.id);
                let new_idx = self.state.scene.backgrounds.len();
                self.state.mutate(move |s| { s.backgrounds.push(dup); });
                self.state.selection = Selection::Background(new_idx);
                self.state.status = "\u{1F4CB} Background duplicated.".into();
            }
            _ => {}
        }
    }

    /// Split the selected element at the current playhead position.
    /// Creates two adjacent elements: [original_start..playhead] and [playhead..original_end].
    fn split_at_playhead(&mut self) {
        let t = self.state.playhead;
        match self.state.selection {
            Selection::Background(i) if i < self.state.scene.backgrounds.len() => {
                let bg = &self.state.scene.backgrounds[i];
                let start = bg.start;
                let end = bg.start + bg.duration;
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this background's range.".into();
                    return;
                }
                let mut right = bg.clone();
                right.id = format!("{}_R", right.id);
                right.start = t;
                right.duration = end - t;
                let left_dur = t - start;
                self.state.mutate(move |s| {
                    s.backgrounds[i].duration = left_dur;
                    s.backgrounds.insert(i + 1, right);
                });
                self.state.status = "\u{2702} Background split at playhead.".into();
            }
            Selection::Actor(i) if i < self.state.scene.actors.len() => {
                let a = &self.state.scene.actors[i];
                let start = a.t_in.unwrap_or(0.0);
                let end = a.t_out.unwrap_or(self.state.scene.output.duration);
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this actor's range.".into();
                    return;
                }
                let mut right = a.clone();
                right.id = format!("{}_R", right.id);
                right.t_in = Some(t);
                right.t_out = Some(end);
                // Correct source_start for the right half
                right.source_start = a.source_start + (t - start);
                // Keep only keyframes in each half (by time).
                right.layout.retain(|kf| kf.t >= t);
                self.state.mutate(move |s| {
                    s.actors[i].t_out = Some(t);
                    s.actors[i].layout.retain(|kf| kf.t <= t);
                    s.actors.insert(i + 1, right);
                });
                self.state.status = "\u{2702} Actor split at playhead.".into();
            }
            Selection::Overlay(i) if i < self.state.scene.overlays.len() => {
                let ov = &self.state.scene.overlays[i];
                let (start, end) = match ov {
                    memstroy_core::Overlay::Text(txt) => (txt.t_in, txt.t_out),
                    memstroy_core::Overlay::Image(im) => (im.t_in, im.t_out),
                    memstroy_core::Overlay::Video(v) => (v.t_in, v.t_out),
                };
                if t <= start || t >= end {
                    self.state.status = "\u{26A0} Playhead is outside this overlay's range.".into();
                    return;
                }
                let mut right = ov.clone();
                match &mut right {
                    memstroy_core::Overlay::Text(txt) => {
                        txt.id = format!("{}_R", txt.id);
                        txt.t_in = t;
                        txt.layout.retain(|kf| kf.t >= t);
                    }
                    memstroy_core::Overlay::Image(im) => {
                        im.id = format!("{}_R", im.id);
                        im.t_in = t;
                        im.layout.retain(|kf| kf.t >= t);
                    }
                    memstroy_core::Overlay::Video(v) => {
                        v.id = format!("{}_R", v.id);
                        v.t_in = t;
                        v.layout.retain(|kf| kf.t >= t);
                    }
                }
                self.state.mutate(move |s| {
                    match &mut s.overlays[i] {
                        memstroy_core::Overlay::Text(txt) => {
                            txt.t_out = t;
                            txt.layout.retain(|kf| kf.t <= t);
                        }
                        memstroy_core::Overlay::Image(im) => {
                            im.t_out = t;
                            im.layout.retain(|kf| kf.t <= t);
                        }
                        memstroy_core::Overlay::Video(v) => {
                            v.t_out = t;
                            v.layout.retain(|kf| kf.t <= t);
                        }
                    }
                    s.overlays.insert(i + 1, right);
                });
                self.state.status = "\u{2702} Overlay split at playhead.".into();
            }
            _ => {
                self.state.status = "\u{26A0} Select an element to split.".into();
            }
        }
    }

    /// Merge the selected element with its next sibling of the same kind.
    /// The merged result spans from the selected's start to the sibling's end.
    fn merge_next(&mut self) {
        match self.state.selection {
            Selection::Background(i) if i + 1 < self.state.scene.backgrounds.len() => {
                let next_end = self.state.scene.backgrounds[i + 1].start
                    + self.state.scene.backgrounds[i + 1].duration;
                let start = self.state.scene.backgrounds[i].start;
                self.state.mutate(move |s| {
                    s.backgrounds[i].duration = next_end - start;
                    s.backgrounds.remove(i + 1);
                });
                self.state.status = "\u{1F517} Backgrounds merged.".into();
            }
            Selection::Actor(i) if i + 1 < self.state.scene.actors.len() => {
                let next = self.state.scene.actors[i + 1].clone();
                self.state.mutate(move |s| {
                    let end = next.t_out.unwrap_or(s.output.duration);
                    s.actors[i].t_out = Some(end);
                    // Merge keyframes from the next actor.
                    for kf in &next.layout {
                        if !s.actors[i].layout.iter().any(|k| (k.t - kf.t).abs() < 0.01) {
                            s.actors[i].layout.push(kf.clone());
                        }
                    }
                    s.actors[i].layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
                    s.actors.remove(i + 1);
                });
                self.state.status = "\u{1F517} Actors merged.".into();
            }
            Selection::Overlay(i) if i + 1 < self.state.scene.overlays.len() => {
                let next_end = match &self.state.scene.overlays[i + 1] {
                    memstroy_core::Overlay::Text(t) => t.t_out,
                    memstroy_core::Overlay::Image(im) => im.t_out,
                    memstroy_core::Overlay::Video(v) => v.t_out,
                };
                self.state.mutate(move |s| {
                    match &mut s.overlays[i] {
                        memstroy_core::Overlay::Text(t) => t.t_out = next_end,
                        memstroy_core::Overlay::Image(im) => im.t_out = next_end,
                        memstroy_core::Overlay::Video(v) => v.t_out = next_end,
                    }
                    s.overlays.remove(i + 1);
                });
                self.state.status = "\u{1F517} Overlays merged.".into();
            }
            _ => {
                self.state.status = "\u{26A0} Select an element with a next sibling to merge.".into();
            }
        }
    }

    // ─── AUDIO PLAYBACK (TODO) ────────────────────────────────────────
    // Architecture:
    // 1. Add `rodio` or `cpal` dependency
    // 2. Create AudioEngine struct with output stream + sink
    // 3. On play: decode audio tracks active at playhead, mix, send to sink
    // 4. On seek: flush sink, re-decode from new position
    // 5. On pause: pause sink
    // Integration point: in the `update()` method after playhead advance

    /// Start waveform extraction for all audio tracks that don't yet have waveform data.
    /// Spawns background tasks (similar to frame extraction pattern) that call
    /// `AudioWaveform::extract_peaks()` and store results in shared slots.
    fn start_waveform_extraction(&mut self) {
        let num_audio = self.state.scene.audio.len();
        if num_audio == 0 {
            return;
        }

        // Ensure audio_waveforms vec is sized to match audio tracks
        while self.state.audio_waveforms.len() < num_audio {
            self.state.audio_waveforms.push(crate::state::AudioWaveform::default());
        }

        for audio_idx in 0..num_audio {
            let wf = &self.state.audio_waveforms[audio_idx];
            if wf.ready || wf.extracting {
                continue;
            }

            let source = self.state.scene.audio[audio_idx].source.clone();
            if !source.exists() {
                continue;
            }

            // Mark as extracting
            self.state.audio_waveforms[audio_idx].extracting = true;

            let source_clone = source.clone();
            // Use a shared slot to communicate results back
            let result_slot: Arc<Mutex<Option<(Vec<f32>, f32)>>> = Arc::new(Mutex::new(None));
            let slot_clone = result_slot.clone();

            self.rt.spawn(async move {
                let peaks = crate::state::AudioWaveform::extract_peaks(&source_clone, 512);
                if let Ok(mut slot) = slot_clone.lock() {
                    *slot = peaks;
                }
            });

            // Store the result slot for polling — we reuse audio_waveforms to check later
            // Since we can't easily add another vec, we'll poll inline.
            // Instead, store via a simpler approach: poll on next frame via the waveform's fields.
            // We'll use a dedicated polling vec similar to frame_extract_results.
            // For simplicity, store in a field on the waveform struct itself isn't possible (no Arc).
            // Use the existing pattern: store slots in a parallel vec.
            if self.waveform_extract_results.len() <= audio_idx {
                while self.waveform_extract_results.len() <= audio_idx {
                    self.waveform_extract_results.push(Arc::new(Mutex::new(None)));
                }
            }
            self.waveform_extract_results[audio_idx] = result_slot;
        }

        self.state.status = "\u{1F3B5} Extracting audio waveforms...".into();
    }

    /// Poll for waveform extraction completion across all audio tracks.
    fn poll_waveform_extraction(&mut self) {
        for audio_idx in 0..self.waveform_extract_results.len() {
            if audio_idx >= self.state.audio_waveforms.len() { break; }
            if self.state.audio_waveforms[audio_idx].ready { continue; }

            if let Ok(mut slot) = self.waveform_extract_results[audio_idx].lock() {
                if let Some((peaks, duration)) = slot.take() {
                    self.state.audio_waveforms[audio_idx].peaks = peaks;
                    self.state.audio_waveforms[audio_idx].duration = duration;
                    self.state.audio_waveforms[audio_idx].ready = true;
                    self.state.audio_waveforms[audio_idx].extracting = false;
                    self.state.status = format!(
                        "\u{2705} Waveform ready (audio {}): {:.1}s",
                        audio_idx, duration
                    );
                }
            }
        }
    }

    /// Start frame extraction for ALL actors in the scene.
    fn start_frame_extraction(&mut self) {
        let num_actors = self.state.scene.actors.len();
        if num_actors == 0 {
            return;
        }

        // Ensure frame_caches and frame_extract_results are sized to match actors
        while self.state.frame_caches.len() < num_actors {
            let idx = self.state.frame_caches.len();
            let source = self.state.scene.actors[idx].source.clone();
            self.state.frame_caches.push(crate::video_cache::FrameCache::new(source, idx));
        }
        while self.frame_extract_results.len() < num_actors {
            self.frame_extract_results.push(Arc::new(Mutex::new(None)));
        }

        for actor_idx in 0..num_actors {
            let source = self.state.scene.actors[actor_idx].source.clone();

            if !source.exists() {
                continue;
            }

            // Skip if this actor's cache is already ready or extracting
            if let Some(fc) = self.state.frame_caches.get(actor_idx) {
                if (fc.is_ready() || fc.extracting) && fc.source == source {
                    continue;
                }
            }

            // Initialize or re-initialize the cache for this actor
            let mut cache = crate::video_cache::FrameCache::new(source.clone(), actor_idx);
            cache.extracting = true;
            self.state.frame_caches[actor_idx] = cache;

            let result_slot = self.frame_extract_results[actor_idx].clone();
            // Clear any previous result
            if let Ok(mut slot) = result_slot.lock() {
                *slot = None;
            }

            crate::video_cache::FrameCache::start_extraction(
                source,
                self.rt.handle(),
                move |duration, frame_count, cache_dir| {
                    if let Ok(mut slot) = result_slot.lock() {
                        *slot = Some((duration, frame_count, cache_dir));
                    }
                },
            );
        }

        self.state.status = "\u{1F3AC} Extracting preview frames...".into();
    }

    /// Poll for frame extraction completion across all actors.
    fn poll_frame_extraction(&mut self) {
        for actor_idx in 0..self.frame_extract_results.len() {
            if let Ok(mut slot) = self.frame_extract_results[actor_idx].lock() {
                if let Some((duration, frame_count, cache_dir)) = slot.take() {
                    if let Some(fc) = self.state.frame_caches.get_mut(actor_idx) {
                        fc.set_ready(duration, frame_count, cache_dir);
                        self.state.status = format!(
                            "\u{2705} Preview ready (actor {}): {} frames ({:.1}s)",
                            actor_idx, frame_count, duration
                        );
                    }
                }
            }
        }
    }

    fn save_scene(&mut self) {
        if let Some(path) = self.state.scene_path.clone() {
            match self.state.scene.save(&path) {
                Ok(()) => self.state.status = "\u{2705} Saved.".into(),
                Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
            }
        } else {
            self.save_as();
        }
    }

    fn save_as(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Scene YAML", &["yaml", "yml"])
            .add_filter("Scene JSON", &["json"])
            .save_file()
        {
            match self.state.scene.save(&path) {
                Ok(()) => {
                    self.state.scene_path = Some(path);
                    self.state.status = "\u{2705} Saved.".into();
                }
                Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
            }
        }
    }

    fn run_preview(&mut self) {
        let out = std::env::temp_dir().join(format!(
            "memstroy_preview_{}.png",
            chrono::Utc::now().timestamp_millis()
        ));
        spawn_preview(
            self.rt.handle(),
            self.tx.clone(),
            self.state.scene.clone(),
            self.state.assets_root.clone(),
            self.state.playhead,
            out,
        );
        self.state.status = "\u{1F5BC} Rendering preview...".into();
    }

    fn run_render(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MP4", &["mp4"])
            .save_file()
        else {
            return;
        };
        self.state.render_progress = Some(crate::state::RenderProgress {
            started: std::time::Instant::now(),
            last_log: String::new(),
            done: false,
            error: None,
        });
        spawn_render(
            self.rt.handle(),
            self.tx.clone(),
            self.state.scene.clone(),
            self.state.assets_root.clone(),
            path,
        );
        self.state.status = "\u{1F3A5} Rendering...".into();
    }

    fn run_refresh(&mut self) {
        if self.state.refreshing {
            return;
        }
        self.state.refreshing = true;
        self.state.status = "\u{1F504} Refreshing clips from Telegram...".into();
        spawn_refresh(
            self.rt.handle(),
            self.tx.clone(),
            "MELLSTROYfonz".into(),
            self.state.clips_dir(),
            self.state.state_path(),
            "\u{0418}\u{043C}\u{0431}\u{0430}".into(), // "Имба"
            80,
            4,
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events(ctx);
        self.poll_frame_extraction();
        self.poll_waveform_extraction();

        // Keyboard shortcuts
        self.handle_shortcuts(ctx);

        // Play/pause: advance playhead
        if self.state.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1); // cap at 100ms
            self.state.playhead += dt * self.state.playback_speed;
            if self.state.playhead >= self.state.scene.output.duration {
                self.state.playhead = 0.0; // loop
            }
            ctx.request_repaint(); // keep animating
        }

        // Auto-preview: only if ffmpeg available, not playing, playhead was manually moved,
        // and frame cache is NOT active (frame cache provides real-time preview instead).
        let frame_cache_active = self.state.frame_caches.iter().any(|fc| fc.is_ready());
        if self.state.ffmpeg_available && !self.state.playing && !frame_cache_active {
            let playhead_delta = (self.state.playhead - self.state.last_rendered_playhead).abs();
            if playhead_delta > 0.1 && !self.state.preview_rendering {
                self.state.preview_rendering = true;
                self.state.last_rendered_playhead = self.state.playhead;
                self.run_preview();
            }
        }

        // Apply modern dark style
        apply_style(ctx);

        // Top menu bar
        egui::TopBottomPanel::top("menu")
            .frame(egui::Frame::none().fill(Color32::from_rgb(25, 25, 35)).inner_margin(6.0))
            .show(ctx, |ui| self.menu(ctx, ui));

        // Left panel: Library + Refresh button
        egui::SidePanel::left("library")
            .resizable(false)
            .default_width(300.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 22, 32))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                panels::library(ui, &mut self.state, || {
                    // This closure doesn't have access to self, so we use a flag
                });

                // Refresh button at top of library
            });

        // Check if refresh was requested via flag
        if self.state.status == "__REFRESH_REQUESTED__" {
            self.state.status = String::new();
            self.run_refresh();
        }
        if self.state.status == "__DELETE_SELECTED__" {
            self.state.status = String::new();
            self.delete_selected();
        }
        if self.state.status == "__DUPLICATE_SELECTED__" {
            self.state.status = String::new();
            self.duplicate_selected();
        }
        if self.state.status == "__SPLIT_AT_PLAYHEAD__" {
            self.state.status = String::new();
            self.split_at_playhead();
        }
        if self.state.status == "__MERGE_NEXT__" {
            self.state.status = String::new();
            self.merge_next();
        }

        // Handle frame extraction request
        if self.state.status == "__EXTRACT_FRAMES__" {
            self.state.status = String::new();
            self.start_frame_extraction();
            self.start_waveform_extraction();
        }

        // Handle eyedropper activation
        if self.state.status == "__EYEDROPPER_ON__" {
            self.state.status = String::new();
            self.state.eyedropper_active = true;
        }

        // Right panel: Inspector
        egui::SidePanel::right("inspector")
            .resizable(false)
            .default_width(350.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(22, 22, 32))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                panels::inspector(ui, &mut self.state);
            });

        // Bottom panel: Timeline
        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(false)
            .exact_height(280.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(18, 18, 28))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                panels::timeline(ui, &mut self.state);
            });

        // Central panel: Preview
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(15, 15, 22))
                    .inner_margin(10.0),
            )
            .show(ctx, |ui| {
                panels::preview(ui, &mut self.state);
            });

        // Node editor floating window (scaffold)
        self.node_editor.show(ctx, &mut self.state.node_editor_open);

        // Repaint scheduling:
        // - When playing with ready frame cache: 16ms (~60fps)
        // - When playing without frame cache: 33ms (~30fps)
        // - When idle/paused: only repaint if jobs are running (reactive mode)
        if self.state.playing {
            if self.state.frame_caches.iter().any(|fc| fc.is_ready()) {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
            }
        } else if self.state.refreshing
            || self.state.render_progress.as_ref().is_some_and(|p| !p.done)
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // When idle/paused with no jobs: don't request repaint (reactive mode)
    }
}

/// Apply a modern dark theme with accent colors.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut visuals = egui::Visuals::dark();

    // Background colors
    visuals.panel_fill = Color32::from_rgb(20, 20, 30);
    visuals.window_fill = Color32::from_rgb(28, 28, 40);
    visuals.extreme_bg_color = Color32::from_rgb(12, 12, 18);

    // Widget colors
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(35, 35, 50);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(40, 40, 58);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(60, 60, 90);
    visuals.widgets.active.bg_fill = Color32::from_rgb(80, 60, 180);

    // Accent colors
    visuals.selection.bg_fill = Color32::from_rgb(100, 60, 200);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(180, 140, 255));
    visuals.hyperlink_color = Color32::from_rgb(140, 100, 255);

    // Rounded corners
    visuals.widgets.noninteractive.rounding = Rounding::same(6.0);
    visuals.widgets.inactive.rounding = Rounding::same(6.0);
    visuals.widgets.hovered.rounding = Rounding::same(6.0);
    visuals.widgets.active.rounding = Rounding::same(6.0);
    visuals.window_rounding = Rounding::same(10.0);

    // Text
    visuals.override_text_color = Some(Color32::from_rgb(220, 220, 240));

    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);

    ctx.set_style(style);
}
