//! Main eframe application: wires panels together and dispatches jobs.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use egui::{Color32, RichText, ViewportCommand};
use memstroy_core::Scene;
use tokio::runtime::Runtime;

use crate::jobs::{
    spawn_download, spawn_preview, spawn_render, JobEvent,
};
use crate::panels;
use crate::state::EditorState;

pub struct App {
    rt: Runtime,
    state: EditorState,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
    download_form: DownloadForm,
}

struct DownloadForm {
    open: bool,
    channel: String,
    out: PathBuf,
    filter: String,
    max_pages: usize,
    overwrite: bool,
    concurrency: usize,
}

impl Default for DownloadForm {
    fn default() -> Self {
        Self {
            open: false,
            channel: "MELLSTROYfonz".into(),
            out: PathBuf::from("assets/mellstroy"),
            filter: "Имба".into(),
            max_pages: 80,
            overwrite: false,
            concurrency: 4,
        }
    }
}

impl App {
    pub fn new(rt: Runtime) -> Self {
        let (tx, rx) = channel();
        let mut state = EditorState::new();
        rescan_library(&mut state);
        Self {
            rt,
            state,
            tx,
            rx,
            download_form: DownloadForm::default(),
        }
    }

    fn pump_events(&mut self, ctx: &egui::Context) {
        // Drain all pending background events.
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                JobEvent::Status(s) => self.state.status = s,
                JobEvent::PreviewReady(p) => {
                    self.state.last_preview = Some(p);
                    ctx.forget_all_images();
                }
                JobEvent::PreviewFailed(e) => {
                    self.state.status = format!("Preview failed: {}", e);
                }
                JobEvent::RenderLog(line) => {
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.last_log = line;
                    }
                }
                JobEvent::RenderFinished(Ok(p)) => {
                    self.state.status = format!("Rendered {}", p.display());
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                    }
                }
                JobEvent::RenderFinished(Err(e)) => {
                    self.state.status = format!("Render failed: {}", e);
                    if let Some(rp) = self.state.render_progress.as_mut() {
                        rp.done = true;
                        rp.error = Some(e);
                    }
                }
                JobEvent::DownloadFinished(Ok(s)) => {
                    self.state.status = format!(
                        "Download done: {}/{} kept, {} new, {} skipped, {} failed",
                        s.kept, s.total, s.downloaded, s.skipped, s.failed
                    );
                    rescan_library(&mut self.state);
                }
                JobEvent::DownloadFinished(Err(e)) => {
                    self.state.status = format!("Download failed: {}", e);
                }
            }
        }
    }

    fn menu(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New scene").clicked() {
                    self.state.scene = Scene::default();
                    self.state.scene_path = None;
                    self.state.status = "New scene.".into();
                    ui.close_menu();
                }
                if ui.button("Open scene…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Scene", &["yaml", "yml", "json"])
                        .pick_file()
                    {
                        match Scene::load(&path) {
                            Ok(s) => {
                                self.state.scene = s;
                                self.state.scene_path = Some(path);
                                self.state.status = "Loaded scene.".into();
                            }
                            Err(e) => self.state.status = format!("Open failed: {e}"),
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("Save scene").clicked() {
                    if let Some(path) = self.state.scene_path.clone() {
                        if let Err(e) = self.state.scene.save(&path) {
                            self.state.status = format!("Save failed: {e}");
                        } else {
                            self.state.status = "Saved.".into();
                        }
                    } else {
                        self.save_as();
                    }
                    ui.close_menu();
                }
                if ui.button("Save scene as…").clicked() {
                    self.save_as();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });

            ui.menu_button("Channel", |ui| {
                if ui.button("Download from Telegram…").clicked() {
                    self.download_form.open = true;
                    ui.close_menu();
                }
                if ui.button("Rescan library").clicked() {
                    rescan_library(&mut self.state);
                    ui.close_menu();
                }
            });

            ui.menu_button("Render", |ui| {
                if ui.button("Render preview frame").clicked() {
                    self.run_preview();
                    ui.close_menu();
                }
                if ui.button("Render full clip…").clicked() {
                    self.run_render();
                    ui.close_menu();
                }
            });

            ui.menu_button("Tools", |ui| {
                if ui.button("Detect anchors (pose)…").clicked() {
                    self.state.status =
                        "Pose detection: ONNX backend not yet wired in. Coming next iteration."
                            .into();
                    ui.close_menu();
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(rp) = &self.state.render_progress {
                    let label = if rp.done {
                        if rp.error.is_some() { "render: error" } else { "render: done" }
                    } else {
                        "render: running…"
                    };
                    ui.label(label);
                }
            });
        });
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
                    self.state.status = "Saved.".into();
                }
                Err(e) => self.state.status = format!("Save failed: {e}"),
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
        self.state.status = "Rendering preview…".into();
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
        self.state.status = "Rendering…".into();
    }

    fn download_modal(&mut self, ctx: &egui::Context) {
        if !self.download_form.open {
            return;
        }
        let mut open = self.download_form.open;
        egui::Window::new("Download from Telegram")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Public channel handle:");
                ui.text_edit_singleline(&mut self.download_form.channel);
                ui.label("Output directory:");
                ui.horizontal(|ui| {
                    let mut s = self.download_form.out.display().to_string();
                    if ui.text_edit_singleline(&mut s).changed() {
                        self.download_form.out = PathBuf::from(s);
                    }
                    if ui.button("…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.download_form.out = p;
                        }
                    }
                });
                ui.label("Body must contain (empty = all posts):");
                ui.text_edit_singleline(&mut self.download_form.filter);
                ui.add(
                    egui::Slider::new(&mut self.download_form.max_pages, 1..=400)
                        .text("max pages"),
                );
                ui.add(
                    egui::Slider::new(&mut self.download_form.concurrency, 1..=16)
                        .text("concurrency"),
                );
                ui.checkbox(&mut self.download_form.overwrite, "Overwrite existing files");
                ui.separator();
                if ui.button("Start").clicked() {
                    spawn_download(
                        self.rt.handle(),
                        self.tx.clone(),
                        self.download_form.channel.clone(),
                        self.download_form.out.clone(),
                        self.download_form.filter.clone(),
                        self.download_form.max_pages,
                        self.download_form.overwrite,
                        self.download_form.concurrency,
                    );
                    self.state.status = "Download started…".into();
                    self.download_form.open = false;
                }
            });
        self.download_form.open = open;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events(ctx);

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu(ctx, ui));

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.state.status).color(Color32::LIGHT_GRAY));
            });
        });

        egui::SidePanel::left("library")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                panels::library(ui, &mut self.state);
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                panels::inspector(ui, &mut self.state);
            });

        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(true)
            .default_height(260.0)
            .show(ctx, |ui| {
                panels::timeline(ui, &mut self.state);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            panels::preview(ui, &mut self.state);
        });

        self.download_modal(ctx);

        // Animate the status bar / progress while a job is running.
        if self.state.render_progress.as_ref().is_some_and(|p| !p.done) {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
}

fn rescan_library(state: &mut EditorState) {
    state.library.mellstroy_clips = scan_dir(&state.assets_root.join("assets/mellstroy"), &["mp4", "mov", "webm"]);
    state.library.backgrounds = scan_dir(&state.assets_root.join("assets/backgrounds"), &["mp4", "mov", "webm", "jpg", "jpeg", "png", "webp"]);
    state.library.props = scan_dir(&state.assets_root.join("assets/props"), &["png", "webp", "svg"]);
}

fn scan_dir(dir: &std::path::Path, exts: &[&str]) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            if exts.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
