//! Editor-wide settings (language, master volume, autosave interval).
//!
//! Persisted to `~/.memstroy/settings.json`. The struct is intentionally
//! tiny — every field has a `serde(default)` so adding new options later
//! never breaks an existing settings file.
//!
//! ## Lifecycle
//!
//! - `EditorSettings::load()` is called once at app startup. It also
//!   pushes the loaded language into the global i18n flag so the very
//!   first frame is already translated.
//! - The settings dialog ([`show_settings_dialog`]) edits a copy of
//!   the live settings; "Apply" / "Done" writes back to disk and
//!   propagates side-effects (audio master volume, language flag).
//! - The dialog is intentionally minimal: a language picker, a
//!   master-volume slider, an autosave-interval slider, and a snap
//!   toggle. No tabs, no sub-panes, no surprises.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::audio_engine::AudioEngine;
use crate::i18n::{self, Lang};
use crate::state::EditorState;

/// Persistent editor preferences. Stored as JSON for human readability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorSettings {
    /// UI language. Defaults to English.
    #[serde(default)]
    pub language: Lang,

    /// Master output gain, multiplied with each track's per-track volume.
    /// Range 0.0 .. 1.0. Default 1.0.
    #[serde(default = "default_volume")]
    pub master_volume: f32,

    /// Autosave interval in seconds. Default 30.
    #[serde(default = "default_autosave")]
    pub autosave_interval: f32,

    /// Whether timeline snapping is enabled by default.
    #[serde(default = "default_true")]
    pub snap_enabled: bool,

    /// UI scale factor (pixels per point). Default 0.0 means "use system
    /// DPI". Values like 1.0, 1.25, 1.5, 2.0 override the system setting
    /// so users on different monitors can scale the interface to taste.
    #[serde(default)]
    pub ui_scale: f32,
}

fn default_volume() -> f32 {
    1.0
}
fn default_autosave() -> f32 {
    30.0
}
fn default_true() -> bool {
    true
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            language: Lang::default(),
            master_volume: 1.0,
            autosave_interval: 30.0,
            snap_enabled: true,
            ui_scale: 0.0,
        }
    }
}

impl EditorSettings {
    /// Path of the persisted settings file. Lives next to the autosave
    /// directory so a user wiping `~/.memstroy` clears both at once.
    pub fn settings_path() -> PathBuf {
        EditorState::autosave_dir().join("settings.json")
    }

    /// Load settings from disk. Returns `Default::default()` on any
    /// failure (no file, malformed JSON, etc.) so the editor always
    /// starts with a valid settings struct.
    pub fn load() -> Self {
        let path = Self::settings_path();
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                let s = Self::default();
                i18n::set_global(s.language);
                return s;
            }
        };
        let parsed: Self = serde_json::from_str(&s).unwrap_or_default();
        // Apply language to the global flag immediately so even the
        // very first painted frame is already translated.
        i18n::set_global(parsed.language);
        parsed
    }

    /// Save settings to disk. Errors are logged but never fatal.
    pub fn save(&self) {
        let path = Self::settings_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::warn!("Failed to write settings to {}: {}", path.display(), e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialise settings: {}", e);
            }
        }
    }
}

/// Render the modal Settings window. Call from the App update loop on
/// every frame; the dialog is only shown while `state.settings_open`
/// is true.
///
/// Side-effects when the user picks a different language / volume:
/// - The global i18n flag is updated so subsequent UI reflects the
///   change on the next frame.
/// - The audio engine's master volume is updated immediately.
pub fn show_settings_dialog(
    ctx: &egui::Context,
    state: &mut EditorState,
    audio_engine: &mut AudioEngine,
) {
    if !state.settings_open {
        return;
    }

    let mut open = state.settings_open;
    let mut close_via_button = false;
    let mut reset_via_button = false;

    egui::Window::new(i18n::t("Editor settings"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(380.0)
        .show(ctx, |ui| {
            // Reuse a tight, minimal layout: every row is "label |
            // control" with consistent vertical spacing. No tabs, no
            // sub-headers — that's the whole "max simple" promise.
            ui.add_space(4.0);

            // ── Language picker ────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(i18n::t("Language")).strong());
                ui.add_space(8.0);
                let current_lbl = state.settings.language.display_name();
                egui::ComboBox::from_id_source("settings_lang")
                    .selected_text(current_lbl)
                    .show_ui(ui, |ui| {
                        for lang in Lang::all() {
                            if ui
                                .selectable_label(
                                    state.settings.language == *lang,
                                    lang.display_name(),
                                )
                                .clicked()
                            {
                                state.settings.language = *lang;
                                i18n::set_global(*lang);
                            }
                        }
                    });
            });

            ui.add_space(8.0);

            // ── Master volume ──────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(i18n::t("Master volume")).strong());
            });
            let mut vol = state.settings.master_volume;
            if ui
                .add(
                    egui::Slider::new(&mut vol, 0.0..=1.0)
                        .show_value(true)
                        .fixed_decimals(2),
                )
                .changed()
            {
                state.settings.master_volume = vol.clamp(0.0, 1.0);
                audio_engine.set_master_volume(state.settings.master_volume);
            }

            ui.add_space(8.0);

            // ── Autosave interval ──────────────────────────────
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(i18n::t("Auto-save interval")).strong());
            });
            let mut iv = state.settings.autosave_interval;
            if ui
                .add(
                    egui::Slider::new(&mut iv, 5.0..=300.0)
                        .suffix(" s")
                        .fixed_decimals(0),
                )
                .changed()
            {
                state.settings.autosave_interval = iv.max(5.0);
                state.autosave_interval = state.settings.autosave_interval;
            }

            ui.add_space(8.0);

            // ── UI Scale ───────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(i18n::t("UI Scale")).strong());
                let scale = state.settings.ui_scale;
                let label = if scale < 0.01 {
                    "Auto".to_string()
                } else {
                    format!("{:.0}%", scale * 100.0)
                };
                ui.label(egui::RichText::new(label).size(13.0).strong());
            });
            ui.horizontal(|ui| {
                if ui
                    .button(egui::RichText::new(" \u{2212} ").size(14.0))
                    .on_hover_text(i18n::t("Decrease scale"))
                    .clicked()
                {
                    let cur = if state.settings.ui_scale < 0.01 {
                        1.0_f32
                    } else {
                        state.settings.ui_scale
                    };
                    state.settings.ui_scale = (cur - 0.1).max(0.5);
                }
                if ui
                    .button(egui::RichText::new(" + ").size(14.0))
                    .on_hover_text(i18n::t("Increase scale"))
                    .clicked()
                {
                    let cur = if state.settings.ui_scale < 0.01 {
                        1.0_f32
                    } else {
                        state.settings.ui_scale
                    };
                    state.settings.ui_scale = (cur + 0.1).min(3.0);
                }
                if ui
                    .button(egui::RichText::new("Auto").size(11.0))
                    .on_hover_text(i18n::t("Reset to system default"))
                    .clicked()
                {
                    state.settings.ui_scale = 0.0;
                }
                if ui
                    .button(egui::RichText::new("100%").size(11.0))
                    .on_hover_text(i18n::t("Set to 100%"))
                    .clicked()
                {
                    state.settings.ui_scale = 1.0;
                }
            });

            ui.add_space(8.0);

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(i18n::t("Restart not required — changes apply immediately."))
                    .size(10.0)
                    .italics()
                    .color(egui::Color32::from_rgb(160, 160, 180)),
            );

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui.button(i18n::t("Reset to defaults")).clicked() {
                    reset_via_button = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(i18n::t("Done")).strong(),
                        ))
                        .clicked()
                    {
                        close_via_button = true;
                    }
                });
            });
        });

    if reset_via_button {
        state.settings = EditorSettings::default();
        i18n::set_global(state.settings.language);
        state.snap_enabled = state.settings.snap_enabled;
        state.autosave_interval = state.settings.autosave_interval;
        audio_engine.set_master_volume(state.settings.master_volume);
    }

    if close_via_button {
        open = false;
    }

    // ── Persist whenever the dialog closes (X-button or Done). ──
    // If the user closed by clicking the window X, `open` already
    // flipped via `.open(&mut open)`. Save in either case so changes
    // survive even if they don't click "Done".
    if state.settings_open && !open {
        state.settings.save();
    }
    state.settings_open = open;
}
