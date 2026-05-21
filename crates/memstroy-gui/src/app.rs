//! Main eframe application: wires panels together and dispatches jobs.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use egui::{Color32, RichText, ViewportCommand, Rounding, Stroke, Vec2};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::jobs::{spawn_refresh, spawn_render, JobEvent};
use crate::node_editor::NodeEditor;
use crate::panels;
use crate::state::{EditorState, Selection};
use crate::clip_editor;
use crate::curve_editor;
use crate::audio_engine::AudioEngine;
use memstroy_vision::PoseEstimator;

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
    /// Audio playback engine
    audio_engine: AudioEngine,
    /// Previous playing state (for detecting transitions)
    was_playing: bool,
    /// Previous playhead for detecting seeks
    prev_playhead: f32,
    /// Previous count of audio sources used by the engine, so we can rebuild
    /// the engine when the user adds/removes a track mid-playback.
    prev_audio_source_count: usize,
}

impl App {
    pub fn new(rt: Runtime) -> Self {
        let (tx, rx) = channel();
        let mut state = EditorState::new();
        state.reload_library();

        // Recovery: if an autosave from a previous session exists and is newer
        // than the user's current scene file, surface a recovery dialog.
        let autosave_path = EditorState::autosave_path();
        if autosave_path.exists() {
            let autosave_modified = std::fs::metadata(&autosave_path)
                .and_then(|m| m.modified())
                .ok();
            // If there is no scene file, any autosave is a candidate.
            // If there is one, only show recovery when the autosave is newer.
            let should_offer = match (&state.scene_path, autosave_modified) {
                (None, Some(_)) => true,
                (Some(p), Some(am)) => match std::fs::metadata(p).and_then(|m| m.modified()) {
                    Ok(scene_modified) => am > scene_modified,
                    Err(_) => true,
                },
                _ => true,
            };
            if should_offer {
                state.recovery_pending = Some(autosave_path);
                state.recovery_dialog_open = true;
            }
        }

        Self {
            rt,
            state,
            tx,
            rx,
            node_editor: NodeEditor::default(),
            frame_extract_results: Vec::new(),
            waveform_extract_results: Vec::new(),
            audio_engine: AudioEngine::new(),
            was_playing: false,
            prev_playhead: 0.0,
            prev_audio_source_count: 0,
        }
    }

    fn pump_events(&mut self, _ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                JobEvent::Status(s) => self.state.status = s,
                JobEvent::RenderLog(line) => {
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.last_log = line.clone();
                        // Parse ffmpeg progress from log lines
                        // Look for "frame= 120" or "time=00:00:04.00"
                        if let Some(time_progress) = parse_ffmpeg_time(&line) {
                            let total = self.state.scene.output.duration;
                            if total > 0.0 {
                                rp.progress = (time_progress / total).clamp(0.0, 1.0);
                            }
                        } else if let Some(frame_num) = parse_ffmpeg_frame(&line) {
                            let total_frames = (self.state.scene.output.duration
                                * self.state.scene.output.fps as f32) as u32;
                            if total_frames > 0 {
                                rp.progress = (frame_num as f32 / total_frames as f32).clamp(0.0, 1.0);
                            }
                        }
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
                                self.state.scene_path = Some(path.clone());
                                self.state.status = "\u{2705} Scene loaded.".into();
                                // Load layout alongside scene
                                let layout_path = path.with_extension("layout.json");
                                self.state.load_layout(&layout_path);
                                // Update tab name
                                let name = path.file_stem().and_then(|s| s.to_str())
                                    .unwrap_or("Scene").to_string();
                                if self.state.active_tab < self.state.scene_tabs.len() {
                                    self.state.scene_tabs[self.state.active_tab].name = name;
                                    self.state.scene_tabs[self.state.active_tab].path = Some(path.clone());
                                    self.state.scene_tabs[self.state.active_tab].scene = self.state.scene.clone();
                                }
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
                if ui.button("\u{1F3A5} Render full clip...").clicked() {
                    self.run_render();
                    ui.close_menu();
                }
            });

            ui.menu_button(RichText::new("\u{1F9E0} Tools").strong(), |ui| {
                if ui.button("\u{1F9B4} Skeleton Constructor...").clicked() {
                    self.state.skeleton_editor.open = true;
                    // Pre-select the source clip from the currently selected
                    // actor (so the editor is ready to edit a familiar clip),
                    // but the editor itself is now clip-centric — see
                    // `skeleton_editor::on_clip_changed`.
                    if let Selection::Actor(i) = self.state.selection {
                        if i < self.state.scene.actors.len() {
                            let path = self.state.scene.actors[i].source.clone();
                            crate::skeleton_editor::select_clip(&mut self.state, &path);
                        }
                    }
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
                        ui.add(egui::ProgressBar::new(rp.progress).text(format!("{:.0}%", rp.progress * 100.0)));
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
                // Keep frame_caches and frame_extract_results in lock-step with actors
                // so subsequent actors don't display the wrong cached frames.
                if i < self.state.frame_caches.len() {
                    self.state.frame_caches.remove(i);
                }
                if i < self.frame_extract_results.len() {
                    self.frame_extract_results.remove(i);
                }
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
            Selection::Audio(i) if i < self.state.scene.audio.len() => {
                self.state.mutate(|s| { s.audio.remove(i); });
                if i < self.state.audio_waveforms.len() {
                    self.state.audio_waveforms.remove(i);
                }
                if i < self.waveform_extract_results.len() {
                    self.waveform_extract_results.remove(i);
                }
                self.state.selection = Selection::None;
                self.state.status = "\u{1F5D1} Audio deleted.".into();
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
                // Keep only keyframes in each half (by LOCAL time relative to actor start).
                let local_split = t - start;
                // Right half: keep keyframes at or after the split point, shift them to start from 0
                right.layout.retain(|kf| kf.t >= local_split);
                for kf in right.layout.iter_mut() {
                    kf.t -= local_split;
                }
                // If right half has no keyframes, add one at t=0 with the last known state
                if right.layout.is_empty() {
                    let last_state = a.layout.last().map(|k| k.value).unwrap_or_default();
                    right.layout.push(memstroy_core::Keyframe::new(0.0, last_state));
                }
                let local_split_for_left = local_split;
                self.state.mutate(move |s| {
                    s.actors[i].t_out = Some(t);
                    // Left half: keep keyframes at or before the split point
                    s.actors[i].layout.retain(|kf| kf.t <= local_split_for_left);
                    // If left half has no keyframes, add one at t=0 with default state
                    if s.actors[i].layout.is_empty() {
                        s.actors[i].layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::ActorState::default()));
                    }
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
                let local_split = t - start;
                match &mut right {
                    memstroy_core::Overlay::Text(txt) => {
                        txt.id = format!("{}_R", txt.id);
                        txt.t_in = t;
                        txt.layout.retain(|kf| kf.t >= local_split);
                        for kf in txt.layout.iter_mut() { kf.t -= local_split; }
                        if txt.layout.is_empty() {
                            txt.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                    memstroy_core::Overlay::Image(im) => {
                        im.id = format!("{}_R", im.id);
                        im.t_in = t;
                        im.layout.retain(|kf| kf.t >= local_split);
                        for kf in im.layout.iter_mut() { kf.t -= local_split; }
                        if im.layout.is_empty() {
                            im.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                    memstroy_core::Overlay::Video(v) => {
                        v.id = format!("{}_R", v.id);
                        v.t_in = t;
                        v.layout.retain(|kf| kf.t >= local_split);
                        for kf in v.layout.iter_mut() { kf.t -= local_split; }
                        if v.layout.is_empty() {
                            v.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                        }
                    }
                }
                let local_split_left = local_split;
                self.state.mutate(move |s| {
                    match &mut s.overlays[i] {
                        memstroy_core::Overlay::Text(txt) => {
                            txt.t_out = t;
                            txt.layout.retain(|kf| kf.t <= local_split_left);
                            if txt.layout.is_empty() {
                                txt.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
                        }
                        memstroy_core::Overlay::Image(im) => {
                            im.t_out = t;
                            im.layout.retain(|kf| kf.t <= local_split_left);
                            if im.layout.is_empty() {
                                im.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
                        }
                        memstroy_core::Overlay::Video(v) => {
                            v.t_out = t;
                            v.layout.retain(|kf| kf.t <= local_split_left);
                            if v.layout.is_empty() {
                                v.layout.push(memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default()));
                            }
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
                Ok(()) => {
                    self.state.status = "\u{2705} Saved.".into();
                    // Save layout alongside scene
                    let layout_path = path.with_extension("layout.json");
                    self.state.save_layout(&layout_path);
                }
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
                    self.state.scene_path = Some(path.clone());
                    self.state.status = "\u{2705} Saved.".into();
                    // Save layout alongside scene
                    let layout_path = path.with_extension("layout.json");
                    self.state.save_layout(&layout_path);
                }
                Err(e) => self.state.status = format!("\u{274C} Save failed: {e}"),
            }
        }
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
            progress: 0.0,
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

    /// Run pose detection on the current frame of the selected actor.
    /// Uses memstroy-vision's pose estimation. Falls back to dummy points
    /// if ONNX runtime is not available.
    fn run_pose_detection(&mut self) {
        let actor_idx = match self.state.selection {
            Selection::Actor(i) if i < self.state.scene.actors.len() => i,
            _ => {
                self.state.status = "Select an actor first for pose detection.".into();
                return;
            }
        };

        let actor = &self.state.scene.actors[actor_idx];
        let source = actor.source.clone();

        // Check if we can load anchor data from existing file
        if let Some(anchors_path) = &actor.anchors {
            if anchors_path.exists() {
                if let Some(track) = memstroy_vision::load_anchor_track(&source) {
                    // Extract points from the sample nearest to current time
                    let t_in = actor.t_in.unwrap_or(0.0);
                    let local_t = self.state.playhead - t_in + actor.source_start;
                    let points: Vec<[f32; 2]> = track.samples.iter()
                        .min_by(|a, b| (a.t - local_t).abs().partial_cmp(&(b.t - local_t).abs()).unwrap())
                        .map(|sample| {
                            sample.points.values()
                                .map(|kp| [kp.x, kp.y])
                                .collect()
                        })
                        .unwrap_or_default();
                    self.state.detected_points = points;
                    self.state.status = format!(
                        "Loaded {} pose points from anchors file.",
                        self.state.detected_points.len()
                    );
                    return;
                }
            }
        }

        // Try to run pose detection; gracefully degrade if ONNX isn't available
        let model_path = self.state.assets_root.join("assets/models/yolov8n-pose.onnx");
        if !model_path.exists() {
            // Provide dummy detection points as a graceful degradation
            self.state.detected_points = vec![
                [0.50, 0.15], // head
                [0.50, 0.35], // body center
                [0.40, 0.30], // left shoulder
                [0.60, 0.30], // right shoulder
                [0.35, 0.50], // left elbow
                [0.65, 0.50], // right elbow
                [0.30, 0.65], // left wrist
                [0.70, 0.65], // right wrist
                [0.43, 0.55], // left hip
                [0.57, 0.55], // right hip
                [0.40, 0.75], // left knee
                [0.60, 0.75], // right knee
                [0.38, 0.90], // left ankle
                [0.62, 0.90], // right ankle
            ];
            self.state.status =
                "Pose detection requires ONNX runtime (model not found). Showing placeholder points."
                    .into();
            return;
        }

        // Attempt real detection via spawned task
        let tx = self.tx.clone();
        let source_clone = source.clone();
        let model_clone = model_path.clone();

        self.rt.spawn(async move {
            let estimator = memstroy_vision::OnnxPoseEstimator::new(model_clone);
            match estimator.estimate(&source_clone, 1.0).await {
                Ok(track) => {
                    if let Some(sample) = track.samples.first() {
                        let points: Vec<[f32; 2]> = sample.points.values()
                            .map(|kp| [kp.x, kp.y])
                            .collect();
                        let msg = format!("Detected {} pose points.", points.len());
                        let _ = tx.send(JobEvent::Status(msg));
                    } else {
                        let _ = tx.send(JobEvent::Status("No pose detected in frame.".into()));
                    }
                }
                Err(e) => {
                    let msg = format!("Pose detection requires ONNX runtime: {}", e);
                    let _ = tx.send(JobEvent::Status(msg));
                }
            }
        });

        // Show placeholder while waiting
        self.state.status = "Running pose detection...".into();
    }

    // ─── AUTO-SAVE / RECOVERY ────────────────────────────────────────

    /// Periodically saves the current scene to `~/.memstroy/autosave.scene.yaml`.
    /// Triggered from `update()`. Updates `last_autosave` and shows a 2 s toast.
    fn tick_autosave(&mut self) {
        let interval = self.state.autosave_interval;
        let due = match self.state.last_autosave {
            Some(t) => t.elapsed().as_secs_f32() > interval,
            None => true, // schedule the first autosave shortly after launch
        };
        if !due {
            return;
        }

        // First call (None): just stamp the timer so we don't autosave on the very
        // first frame; we'll wait the configured interval before writing.
        if self.state.last_autosave.is_none() {
            self.state.last_autosave = Some(std::time::Instant::now());
            return;
        }

        let path = EditorState::autosave_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match self.state.scene.save(&path) {
            Ok(()) => {
                self.state.last_autosave = Some(std::time::Instant::now());
                self.state.autosave_toast_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                self.state.status = "\u{1F4BE} Auto-saved".into();
            }
            Err(e) => {
                self.state.status = format!("\u{26A0} Autosave failed: {e}");
            }
        }
    }

    /// Render the recovery modal when an autosave from a previous launch was
    /// detected. Lets the user restore, discard, or postpone the decision.
    fn show_recovery_dialog(&mut self, ctx: &egui::Context) {
        if !self.state.recovery_dialog_open {
            return;
        }
        let Some(autosave_path) = self.state.recovery_pending.clone() else {
            self.state.recovery_dialog_open = false;
            return;
        };

        let mut close = false;
        let mut decision: Option<&'static str> = None;

        egui::Window::new("Recover scene?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("\u{26A0} A recovered scene was found.")
                        .size(14.0)
                        .strong()
                        .color(Color32::from_rgb(255, 200, 80)),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(autosave_path.display().to_string())
                        .size(10.0)
                        .color(Color32::from_rgb(160, 160, 180)),
                );
                ui.add_space(8.0);
                ui.label("Restore the auto-saved scene?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let yes = egui::Button::new(RichText::new("Yes, restore").color(Color32::WHITE))
                        .fill(Color32::from_rgb(60, 160, 80));
                    if ui.add(yes).clicked() {
                        decision = Some("yes");
                        close = true;
                    }
                    let no = egui::Button::new(RichText::new("No, discard").color(Color32::WHITE))
                        .fill(Color32::from_rgb(200, 60, 60));
                    if ui.add(no).clicked() {
                        decision = Some("no");
                        close = true;
                    }
                    if ui.button("Later").clicked() {
                        decision = Some("later");
                        close = true;
                    }
                });
            });

        if !close {
            return;
        }

        match decision {
            Some("yes") => match Scene::load(&autosave_path) {
                Ok(scene) => {
                    self.state.scene = scene;
                    self.state.scene_path = None;
                    self.state.status = "\u{2705} Recovered scene loaded.".into();
                }
                Err(e) => {
                    self.state.status = format!("\u{274C} Recovery failed: {e}");
                }
            },
            Some("no") => {
                let _ = std::fs::remove_file(&autosave_path);
                self.state.status = "\u{1F5D1} Recovery discarded.".into();
            }
            Some("later") => {
                self.state.status = "Recovery postponed.".into();
            }
            _ => {}
        }
        self.state.recovery_dialog_open = false;
        self.state.recovery_pending = None;
    }

    // ─── TITLE TEMPLATES PICKER ──────────────────────────────────────

    /// Modal-style egui Window listing built-in title templates as cards.
    /// Clicking a card adds an overlay at the playhead with a 3-second window.
    fn show_title_picker(&mut self, ctx: &egui::Context) {
        if !self.state.title_picker_open {
            return;
        }

        let mut open = self.state.title_picker_open;
        let playhead = self.state.playhead;
        let scene_dur = self.state.scene.output.duration;
        let mut chosen: Option<usize> = None;

        egui::Window::new("Add Title")
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .default_height(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Pick a title template")
                        .strong()
                        .size(14.0)
                        .color(Color32::from_rgb(180, 140, 255)),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Adds a 3-second text overlay at the playhead. \
                        Edit text/style afterwards in the Inspector.",
                    )
                    .size(11.0)
                    .color(Color32::from_rgb(160, 160, 180)),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for (i, tpl) in crate::title_templates::TEMPLATES.iter().enumerate() {
                            let frame = egui::Frame::none()
                                .fill(Color32::from_rgb(32, 32, 48))
                                .rounding(Rounding::same(8.0))
                                .stroke(Stroke::new(1.0, Color32::from_rgb(60, 60, 80)))
                                .inner_margin(egui::Margin::same(8.0));

                            let resp = frame
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(tpl.icon).size(22.0));
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(tpl.name).strong().size(13.0),
                                            );
                                            ui.label(
                                                RichText::new(tpl.description)
                                                    .size(10.0)
                                                    .color(Color32::from_rgb(160, 160, 180)),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "\u{201C}{}\u{201D}",
                                                    tpl.default_text
                                                ))
                                                .size(10.0)
                                                .italics()
                                                .color(Color32::from_rgb(200, 200, 220)),
                                            );
                                        });
                                    });
                                })
                                .response;
                            if resp.interact(egui::Sense::click()).clicked() {
                                chosen = Some(i);
                            }
                            ui.add_space(4.0);
                        }
                    });
            });

        self.state.title_picker_open = open;

        if let Some(idx) = chosen {
            let templates = crate::title_templates::TEMPLATES;
            if idx < templates.len() {
                let tpl: &'static crate::title_templates::TitleTemplate = &templates[idx];
                let t_in = playhead;
                let t_out = (t_in + 3.0).min(scene_dur.max(t_in + 0.1));
                let mut new_idx_out: usize = 0;
                self.state.mutate(|scene| {
                    new_idx_out = crate::title_templates::add_template_to_scene(
                        scene, tpl, t_in, t_out,
                    );
                });
                self.state.selection = Selection::Overlay(new_idx_out);
                self.state.status = format!("\u{2728} Added title: {}", tpl.name);
                self.state.title_picker_open = false;
            }
        }
    }
}

/// Parse ffmpeg time output like "time=00:00:04.00" and return seconds.
fn parse_ffmpeg_time(line: &str) -> Option<f32> {
    let time_prefix = "time=";
    let idx = line.find(time_prefix)?;
    let after = &line[idx + time_prefix.len()..];
    let time_str = after.split_whitespace().next().unwrap_or("");
    // Parse HH:MM:SS.xx format
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 3 {
        let hours: f32 = parts[0].parse().ok()?;
        let minutes: f32 = parts[1].parse().ok()?;
        let seconds: f32 = parts[2].parse().ok()?;
        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    } else {
        None
    }
}

/// Parse ffmpeg frame output like "frame=  120" and return frame number.
fn parse_ffmpeg_frame(line: &str) -> Option<u32> {
    let frame_prefix = "frame=";
    let idx = line.find(frame_prefix)?;
    let after = &line[idx + frame_prefix.len()..];
    let num_str = after.trim_start().split_whitespace().next().unwrap_or("");
    num_str.parse().ok()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events(ctx);
        self.poll_frame_extraction();
        self.poll_waveform_extraction();

        // Auto-start frame extraction for actors that have source files
        if !self.state.scene.actors.is_empty() {
            let needs_extraction = self.state.scene.actors.iter().enumerate().any(|(i, actor)| {
                actor.source.exists()
                    && self.state.frame_caches.get(i).map(|fc| !fc.is_ready() && !fc.extracting).unwrap_or(true)
            });
            if needs_extraction {
                self.start_frame_extraction();
            }
        }

        // Auto-start waveform extraction for any audio tracks that don't yet
        // have a ready waveform — fixes "audio doesn't load from waveform"
        // when a track is dropped on the timeline.
        if !self.state.scene.audio.is_empty() {
            let needs_wf = self.state.scene.audio.iter().enumerate().any(|(i, au)| {
                au.source.exists()
                    && self.state.audio_waveforms.get(i)
                        .map(|wf| !wf.ready && !wf.extracting)
                        .unwrap_or(true)
            });
            if needs_wf {
                self.start_waveform_extraction();
            }
        }

        // Keyboard shortcuts
        self.handle_shortcuts(ctx);

        // Play/pause: advance playhead
        if self.state.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1); // cap at 100ms
            self.state.playhead += dt * self.state.playback_speed;

            // Loop preview: clamp playhead within the loop region.
            if self.state.loop_mode {
                if let Some((ls, le)) = self.state.loop_region {
                    let (ls, le) = if ls <= le { (ls, le) } else { (le, ls) };
                    if self.state.playhead > le || self.state.playhead < ls {
                        self.state.playhead = ls;
                    }
                }
            }

            // Wrap when the playhead reaches the end of the longest layer
            // (which `panels::timeline()` keeps in sync with `output.duration`).
            // No "+1 second" buffer — once the last clip finishes we restart
            // immediately so loop playback doesn't sit on dead air.
            if self.state.playhead >= self.state.scene.output.duration
                || self.state.playhead < 0.0
            {
                self.state.playhead = 0.0;
            }
            // Repaint at the scene's output FPS (capped) so we don't burn
            // CPU/GPU at 120+ Hz when the output is e.g. 30 fps.
            let target_fps = (self.state.scene.output.fps as f32).max(15.0).min(120.0);
            let dt_target = std::time::Duration::from_secs_f32(1.0 / target_fps);
            ctx.request_repaint_after(dt_target);
        }

        // Auto-preview via ffmpeg has been replaced by the canvas frame
        // caches, which give a much faster preview. We deliberately keep the
        // pipeline disabled here to avoid spawning ffmpeg every time the
        // playhead moves — that was the dominant source of editor lag.

        // Only request continuous repaints while playing. When paused, repaint
        // happens on user input (mouse / keyboard / drag) automatically.
        // (request_repaint_after above handles the scheduling.)

        // Apply modern dark style
        apply_style(ctx);

        // Top menu bar
        egui::TopBottomPanel::top("menu")
            .frame(egui::Frame::none().fill(Color32::from_rgb(25, 25, 35)).inner_margin(6.0))
            .show(ctx, |ui| self.menu(ctx, ui));

        // ── Tab bar for multiple scenes ──
        egui::TopBottomPanel::top("tab_bar")
            .frame(egui::Frame::none().fill(Color32::from_rgb(22, 22, 30)).inner_margin(egui::Margin::symmetric(6.0, 2.0)))
            .exact_height(26.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let num_tabs = self.state.scene_tabs.len();
                    let mut switch_to: Option<usize> = None;
                    let mut close_tab: Option<usize> = None;

                    for i in 0..num_tabs {
                        let is_active = i == self.state.active_tab;
                        let tab_name = self.state.scene_tabs[i].name.clone();
                        let fill = if is_active { Color32::from_rgb(40, 40, 60) } else { Color32::from_rgb(28, 28, 38) };
                        let text_col = if is_active { Color32::from_rgb(255, 255, 255) } else { Color32::from_rgb(140, 140, 160) };

                        let tab_frame = egui::Frame::none()
                            .fill(fill)
                            .rounding(Rounding { nw: 4.0, ne: 4.0, sw: 0.0, se: 0.0 })
                            .inner_margin(egui::Margin::symmetric(8.0, 2.0));

                        tab_frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui.selectable_label(is_active, RichText::new(&tab_name).size(11.0).color(text_col)).clicked() {
                                    switch_to = Some(i);
                                }
                                if num_tabs > 1 {
                                    if ui.small_button("x").clicked() {
                                        close_tab = Some(i);
                                    }
                                }
                            });
                        });
                        ui.add_space(2.0);
                    }

                    // "+" button to add new tab
                    if ui.button(RichText::new("+").size(12.0).color(Color32::from_rgb(100, 200, 100))).clicked() {
                        self.state.new_tab();
                    }

                    if let Some(idx) = switch_to {
                        self.state.switch_tab(idx);
                    }
                    if let Some(idx) = close_tab {
                        self.state.close_tab(idx);
                    }
                });
            });

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

        // ── OS Drag-and-Drop: accept files from Windows Explorer ──
        let dropped_files: Vec<_> = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped_files.is_empty() {
            for file in &dropped_files {
                if let Some(path) = &file.path {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    if ["mp4", "mov", "webm", "avi", "mkv"].contains(&ext.as_str()) {
                        // `add_actor_from_clip` already creates a matching AudioTrack
                        // and pre-loads any per-clip chroma/skeleton sidecars.
                        crate::panels::add_actor_from_clip(&mut self.state, &path.to_path_buf());
                    } else if ["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str()) {
                        let id = path.file_stem().and_then(|s| s.to_str())
                            .map(|s| format!("img_{}", s))
                            .unwrap_or_else(|| format!("img_{}", self.state.scene.overlays.len() + 1));
                        let overlay = memstroy_core::Overlay::Image(memstroy_core::ImageOverlay {
                            id: id.clone(),
                            source: path.to_path_buf(),
                            t_in: self.state.playhead,
                            t_out: (self.state.playhead + 3.0).min(self.state.scene.output.duration),
                            layout: vec![memstroy_core::Keyframe::new(0.0, memstroy_core::OverlayState::default())],
                        });
                        self.state.scene.overlays.push(overlay);
                        self.state.selection = Selection::Overlay(self.state.scene.overlays.len() - 1);
                        self.state.status = format!("Dropped image: {}", id);
                    } else if ["mp3", "wav", "ogg", "flac", "aac", "m4a"].contains(&ext.as_str()) {
                        let id = path.file_stem().and_then(|s| s.to_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("audio_{}", self.state.scene.audio.len() + 1));
                        self.state.scene.audio.push(memstroy_core::AudioTrack {
                            id: id.clone(),
                            source: path.to_path_buf(),
                            t_in: self.state.playhead,
                            t_out: None,
                            source_start: 0.0,
                            volume: 1.0,
                        });
                        self.state.selection = Selection::Audio(self.state.scene.audio.len() - 1);
                        self.state.status = format!("Dropped audio: {}", id);
                    }
                }
            }
        }

        // Handle eyedropper activation
        if self.state.status == "__EYEDROPPER_ON__" {
            self.state.status = String::new();
            self.state.eyedropper_active = true;
        }

        // Handle pose detection request
        if self.state.status == "__DETECT_POSE__" {
            self.state.status = String::new();
            self.run_pose_detection();
        }

        // ── Audio engine synchronisation ──
        // Build the list of audio sources currently scheduled in the scene:
        //   - explicit AudioTrack entries (state.scene.audio)
        //   - actor video clips (their embedded audio streams) — but only
        //     when no AudioTrack already references the same source path,
        //     because otherwise we'd hear the clip's audio twice.
        // The engine ignores anything without a decodable audio stream, so
        // including every actor unconditionally is safe.
        let build_sources = |state: &EditorState| -> Vec<crate::audio_engine::AudioSourceSpec> {
            let mut out = Vec::new();
            let mut seen: std::collections::HashSet<std::path::PathBuf> =
                std::collections::HashSet::new();
            for a in &state.scene.audio {
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: a.source.clone(),
                    t_in: a.t_in,
                    t_out: a.t_out,
                    source_start: a.source_start,
                    volume: a.volume,
                });
                seen.insert(a.source.clone());
            }
            for actor in &state.scene.actors {
                if !actor.visible { continue; }
                if seen.contains(&actor.source) { continue; }
                out.push(crate::audio_engine::AudioSourceSpec {
                    path: actor.source.clone(),
                    t_in: actor.t_in.unwrap_or(0.0),
                    t_out: actor.t_out,
                    source_start: actor.source_start,
                    volume: 1.0,
                });
            }
            out
        };

        // Detect a seek (playhead jumped further than a sane frame's worth).
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let expected_step = if self.state.playing { dt * self.state.playback_speed.abs() } else { 0.0 };
        let actual_delta = self.state.playhead - self.prev_playhead;
        let seeked = (actual_delta - expected_step).abs() > 0.15
            || actual_delta < -0.05;

        if self.state.playing && !self.was_playing {
            // Transition: paused → playing. Start playback at the current playhead.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.audio_engine.play_sources(&sources, self.state.playhead);
        } else if !self.state.playing && self.was_playing {
            // Transition: playing → paused.
            self.audio_engine.pause();
        } else if self.state.playing && seeked {
            // Seek while playing — restart from the new position so audio stays in sync.
            let sources = build_sources(&self.state);
            self.prev_audio_source_count = sources.len();
            self.audio_engine.play_sources(&sources, self.state.playhead);
        } else if self.state.playing {
            // Detect new/removed sources mid-playback (e.g., user just dropped
            // an audio clip on the timeline). Rebuild so the new track is heard.
            let sources = build_sources(&self.state);
            if sources.len() != self.prev_audio_source_count {
                self.prev_audio_source_count = sources.len();
                self.audio_engine.play_sources(&sources, self.state.playhead);
            }
        }

        self.was_playing = self.state.playing;
        self.prev_playhead = self.state.playhead;

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
                crate::canvas_preview::canvas_preview(ui, &mut self.state);
            });

        // Node editor floating window (scaffold)
        self.node_editor.show(ctx, &mut self.state.node_editor_open);

        // Curve editor floating window
        if self.state.curve_editor_open {
            let mut curve_open = self.state.curve_editor_open;
            egui::Window::new("Curve Editor")
                .open(&mut curve_open)
                .default_size([600.0, 200.0])
                .resizable(true)
                .collapsible(true)
                .anchor(egui::Align2::LEFT_BOTTOM, [10.0, -10.0])
                .show(ctx, |ui| {
                    match self.state.selection {
                        Selection::Actor(i) if i < self.state.scene.actors.len() => {
                            let duration = self.state.scene.output.duration;
                            let playhead = self.state.playhead;
                            let keyframes = &mut self.state.scene.actors[i].layout;
                            curve_editor::curve_editor_panel(
                                ui,
                                keyframes,
                                duration,
                                &mut self.state.curve_editor_property,
                                playhead,
                            );
                        }
                        _ => {
                            ui.label(egui::RichText::new("Select an actor to edit curves.")
                                .italics()
                                .color(Color32::from_rgb(140, 140, 160)));
                        }
                    }
                });
            self.state.curve_editor_open = curve_open;
        }

        // Clip editor floating window
        if self.state.clip_editor_open {
            self.state.clip_editor_open = clip_editor::clip_editor_window(ctx, &mut self.state);
        }

        // Skeleton editor floating window
        crate::skeleton_editor::skeleton_editor_window(ctx, &mut self.state);

        // Title-templates picker (popup grid of preset captions)
        self.show_title_picker(ctx);

        // Auto-save tick + recovery modal
        self.tick_autosave();
        self.show_recovery_dialog(ctx);

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
