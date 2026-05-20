//! Main eframe application: wires panels together and dispatches jobs.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use egui::{Color32, RichText, ViewportCommand, Rounding, Stroke, Vec2};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::jobs::{spawn_preview, spawn_refresh, spawn_render, JobEvent};
use crate::panels;
use crate::state::{EditorState, Selection};

pub struct App {
    rt: Runtime,
    state: EditorState,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
}

impl App {
    pub fn new(rt: Runtime) -> Self {
        let (tx, rx) = channel();
        let mut state = EditorState::new();
        state.reload_library();
        Self { rt, state, tx, rx }
    }

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                JobEvent::Status(s) => self.state.status = s,
                JobEvent::PreviewReady(p) => {
                    self.state.last_preview = Some(p);
                    ctx.forget_all_images();
                }
                JobEvent::PreviewFailed(e) => {
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
                    ui.label(RichText::new("refreshing...").color(Color32::from_rgb(255, 200, 50)));
                } else if let Some(rp) = &self.state.render_progress {
                    if !rp.done {
                        ui.spinner();
                        ui.label(RichText::new("rendering...").color(Color32::from_rgb(100, 200, 255)));
                    }
                }
            });
        });
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let modifiers = ctx.input(|i| i.modifiers);
        let ctrl = modifiers.ctrl || modifiers.mac_cmd;

        ctx.input(|i| {
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

        // Keyboard shortcuts
        self.handle_shortcuts(ctx);

        // Apply modern dark style
        apply_style(ctx);

        // Top menu bar
        egui::TopBottomPanel::top("menu")
            .frame(egui::Frame::none().fill(Color32::from_rgb(25, 25, 35)).inner_margin(6.0))
            .show(ctx, |ui| self.menu(ctx, ui));

        // Status bar at bottom
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(30, 30, 42))
                    .inner_margin(egui::Margin::symmetric(12.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&self.state.status)
                            .color(Color32::from_rgb(200, 200, 220))
                            .size(13.0),
                    );
                });
            });

        // Left panel: Library + Refresh button
        egui::SidePanel::left("library")
            .resizable(true)
            .default_width(320.0)
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

        // Right panel: Inspector
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(380.0)
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
            .resizable(true)
            .default_height(240.0)
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(18, 18, 28))
                    .inner_margin(10.0),
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

        // Keep refreshing UI while jobs are running
        if self.state.refreshing
            || self.state.render_progress.as_ref().is_some_and(|p| !p.done)
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
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
