//! Edit ▸ Preferences: audio devices, buffer size, recording channels, input monitoring.

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, Stroke, pos2, vec2};

use super::{AudacityApp, recovery_ui};
use crate::{
    effects::{Generator, Waveform, lin_to_db},
    engine::{self, DeviceSettings},
    project::Project,
};

const KEY_DEVICES: &str = "audio_devices";
const BUFFERS: [Option<u32>; 7] = [
    None,
    Some(64),
    Some(128),
    Some(256),
    Some(512),
    Some(1024),
    Some(2048),
];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum PrefsTab {
    #[default]
    Audio,
    Projects,
    Appearance,
    Keyboard,
}

#[derive(Default)]
pub struct PrefsState {
    pub open: bool,
    pub tab: PrefsTab,
    outputs: Vec<String>,
    inputs: Vec<String>,
    /// Smoothed input level for the monitor meter.
    level: [f32; 2],
    hold: f32,
}

pub fn load_devices(storage: Option<&dyn eframe::Storage>) -> DeviceSettings {
    storage
        .and_then(|s| eframe::get_value(s, KEY_DEVICES))
        .unwrap_or_default()
}

pub fn save_devices(storage: &mut dyn eframe::Storage, d: &DeviceSettings) {
    eframe::set_value(storage, KEY_DEVICES, d);
}

fn buffer_label(b: Option<u32>, rate: u32) -> String {
    match b {
        None => "Driver default".into(),
        Some(n) => format!(
            "{n} frames (≈{:.1} ms)",
            n as f32 * 1000.0 / rate.max(1) as f32
        ),
    }
}

impl AudacityApp {
    pub(super) fn open_prefs(&mut self) {
        self.prefs.outputs = engine::output_device_names();
        self.prefs.inputs = engine::input_device_names();
        self.prefs.open = true;
    }

    pub(super) fn prefs_window(&mut self, ctx: &egui::Context) {
        if !self.prefs.open {
            if self.engine.is_monitoring() {
                self.engine.stop_recording();
            }
            return;
        }
        let mut open = true;
        let mut refresh = false;
        let mut test = false;
        let mut toggle_monitor = false;
        let before = self.engine.devices.clone();
        egui::Window::new("Preferences")
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (t, label) in [
                        (PrefsTab::Audio, "Audio"),
                        (PrefsTab::Projects, "Projects"),
                        (PrefsTab::Appearance, "Appearance"),
                        (PrefsTab::Keyboard, "Keyboard"),
                    ] {
                        ui.selectable_value(&mut self.prefs.tab, t, label);
                    }
                });
                ui.separator();
                match self.prefs.tab {
                    PrefsTab::Projects => {
                        self.prefs_projects(ui);
                        return;
                    }
                    PrefsTab::Appearance => {
                        self.prefs_appearance(ui);
                        return;
                    }
                    PrefsTab::Keyboard => {
                        self.prefs_keyboard(ui);
                        return;
                    }
                    PrefsTab::Audio => {}
                }
                let d = &mut self.engine.devices;
                egui::Grid::new("prefs_audio").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    let device_combo = |ui: &mut egui::Ui, id: &str, value: &mut Option<String>, list: &[String]| {
                        let text = value.clone().unwrap_or_else(|| "System default".into());
                        egui::ComboBox::from_id_salt(id)
                            .selected_text(text)
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(value, None, "System default");
                                for n in list {
                                    ui.selectable_value(value, Some(n.clone()), n);
                                }
                            });
                        if let Some(v) = value
                            && !list.contains(v)
                        {
                            ui.colored_label(Color32::from_rgb(230, 170, 80), "⚠")
                                .on_hover_text("This device isn't connected right now; the default will be used.");
                        }
                    };
                    ui.label("Playback device:");
                    ui.horizontal(|ui| device_combo(ui, "out_dev", &mut d.output, &self.prefs.outputs));
                    ui.end_row();
                    ui.label("Recording device:");
                    ui.horizontal(|ui| device_combo(ui, "in_dev", &mut d.input, &self.prefs.inputs));
                    ui.end_row();
                    ui.label("Record:");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut d.record_mono, true, "Mono (1 channel)");
                        ui.radio_value(&mut d.record_mono, false, "Stereo (2 channels)");
                    });
                    ui.end_row();
                    ui.label("Buffer size:");
                    let rate = self.project.rate;
                    egui::ComboBox::from_id_salt("buf")
                        .selected_text(buffer_label(d.buffer_frames, rate))
                        .show_ui(ui, |ui| {
                            for b in BUFFERS {
                                ui.selectable_value(&mut d.buffer_frames, b, buffer_label(b, rate));
                            }
                        })
                        .response
                        .on_hover_text("Smaller = lower latency when recording, but more risk of crackles. 256–512 suits most computers.");
                    ui.end_row();
                });

                ui.horizontal(|ui| {
                    if ui.button("⟳ Refresh devices").clicked() {
                        refresh = true;
                    }
                    if ui.button("▶ Test playback").on_hover_text("Plays a short tone on the playback device").clicked() {
                        test = true;
                    }
                });
                if self.prefs.outputs.is_empty() && self.prefs.inputs.is_empty() {
                    ui.colored_label(Color32::from_rgb(230, 170, 80), "No audio devices were found.");
                }
                if let Some(w) = &self.engine.device_warning {
                    ui.colored_label(Color32::from_rgb(230, 170, 80), w);
                }

                ui.add_space(8.0);
                ui.separator();
                ui.strong("Input level");
                ui.horizontal(|ui| {
                    let monitoring = self.engine.is_monitoring();
                    if ui
                        .add_enabled(
                            self.recording.is_none(),
                            egui::Button::new(if monitoring { "■ Stop monitoring" } else { "🎙 Monitor input" })
                                .selected(monitoring),
                        )
                        .on_hover_text("Shows the microphone level without recording, so you can set your gain")
                        .clicked()
                    {
                        toggle_monitor = true;
                    }
                    // Meter.
                    // The toolbar's record meter already reads and smooths the input peaks.
                    let dt = ui.input(|i| i.stable_dt).min(0.1);
                    self.prefs.level = [self.meters[2], self.meters[3]];
                    let peak_now = self.prefs.level[0].max(self.prefs.level[1]);
                    self.prefs.hold = if peak_now >= self.prefs.hold { peak_now } else { self.prefs.hold * (-dt * 0.7).exp() };
                    let (rect, _) = ui.allocate_exact_size(vec2(240.0, 18.0), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(30));
                    let frac = |v: f32| ((lin_to_db(v).max(-60.0) + 60.0) / 60.0).clamp(0.0, 1.0);
                    for c in 0..2 {
                        let v = self.prefs.level[c];
                        let r = Rect::from_min_size(
                            pos2(rect.left(), rect.top() + c as f32 * 9.0),
                            vec2(rect.width() * frac(v), 8.0),
                        );
                        let col = if v >= 0.99 {
                            Color32::RED
                        } else if lin_to_db(v) > -6.0 {
                            Color32::from_rgb(230, 200, 60)
                        } else {
                            Color32::from_rgb(220, 80, 70)
                        };
                        p.rect_filled(r, CornerRadius::ZERO, col);
                    }
                    let hx = rect.left() + rect.width() * frac(self.prefs.hold);
                    p.line_segment([pos2(hx, rect.top()), pos2(hx, rect.bottom())], Stroke::new(2.0, Color32::WHITE));
                    if monitoring {
                        ui.label(
                            RichText::new(format!("{:.1} dB", lin_to_db(self.prefs.hold).max(-60.0)))
                                .monospace()
                                .small(),
                        );
                        ui.ctx().request_repaint();
                    }
                });
                ui.label(
                    RichText::new("Aim for peaks around −12 to −6 dB while speaking or playing; red means clipping.")
                        .small()
                        .weak(),
                );
            });
        if refresh {
            self.prefs.outputs = engine::output_device_names();
            self.prefs.inputs = engine::input_device_names();
            self.engine.device_warning = None;
        }
        if self.engine.devices != before {
            self.engine.device_warning = None;
            self.plugins_follow_device_rate();
            // Re-open the monitor on the newly chosen input.
            if self.engine.is_monitoring() {
                self.engine.stop_recording();
                let r = self.engine.start_monitor();
                self.report(r);
            }
            if self.engine.is_playing() {
                self.engine.stop_playback();
            }
        }
        if toggle_monitor {
            if self.engine.is_monitoring() {
                self.engine.stop_recording();
            } else {
                let r = self.engine.start_monitor();
                self.report(r);
            }
        }
        if test && !self.busy() {
            let mut p = Project::default();
            p.rate = 44_100;
            let tone = Generator::Tone {
                wave: Waveform::Sine,
                freq: 660.0,
                amp: 0.25,
                secs: 0.8,
            }
            .render(p.rate);
            p.add_track("test", vec![tone]);
            self.previewing = true;
            let r = self.engine.play(&p, 0, None, false);
            self.report(r);
        }
        if !open {
            self.prefs.open = false;
            if self.engine.is_monitoring() {
                self.engine.stop_recording();
            }
        }
    }
}

impl AudacityApp {
    fn prefs_projects(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Autosave for crash recovery:");
            let r = &mut self.recovery;
            egui::ComboBox::from_id_salt("autosave")
                .selected_text(recovery_ui::interval_label(r.interval_secs))
                .show_ui(ui, |ui| {
                    for s in recovery_ui::INTERVALS {
                        ui.selectable_value(&mut r.interval_secs, s, recovery_ui::interval_label(s));
                    }
                })
                .response
                .on_hover_text(
                    "Unsaved changes are saved to a recovery file in the background. If the app crashes, \
                     you're offered to recover them next time.",
                );
        });
        ui.label(
            RichText::new(format!(
                "Recovery files: {}",
                crate::recovery::recovery_dir().display()
            ))
            .small()
            .weak(),
        );
        if let Some(e) = &self.recovery.error {
            ui.colored_label(Color32::from_rgb(230, 170, 80), e);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(format!("Recent files: {}", self.recovery.recent.0.len()));
            if ui.button("Clear").clicked() {
                self.recovery.recent.0.clear();
            }
        });
    }

    fn prefs_appearance(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Theme:");
            let before = self.theme;
            for t in super::theme::ThemeChoice::ALL {
                ui.radio_value(&mut self.theme, t, t.label());
            }
            if self.theme != before {
                self.theme.apply(ui.ctx());
            }
        });
        ui.label(
            RichText::new("“Same as system” follows your computer's light/dark setting.")
                .small()
                .weak(),
        );
    }

    fn prefs_keyboard(&mut self, ui: &mut egui::Ui) {
        use super::keys::Keymap;
        ui.label(
            RichText::new("Click Change, then press the new key combination (Esc cancels).")
                .small()
                .weak(),
        );
        if let Some(n) = &self.keys.note {
            ui.colored_label(Color32::from_rgb(230, 170, 80), n);
        }
        egui::ScrollArea::vertical()
            .max_height(380.0)
            .show(ui, |ui| {
                egui::Grid::new("keys")
                    .striped(true)
                    .num_columns(3)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        for (act, label) in Keymap::commands() {
                            ui.label(label);
                            let text = if self.keys.capturing == Some(act) {
                                RichText::new("Press keys…")
                                    .italics()
                                    .color(Color32::from_rgb(255, 210, 90))
                            } else {
                                let t = self.keys.labels(act);
                                RichText::new(if t.is_empty() { "—".to_string() } else { t })
                                    .monospace()
                            };
                            ui.label(text);
                            ui.horizontal(|ui| {
                                if ui.small_button("Change").clicked() {
                                    self.keys.capturing = Some(act);
                                    self.keys.note = None;
                                }
                                if ui
                                    .add_enabled(
                                        !self.keys.is_default(act),
                                        egui::Button::new("Reset").small(),
                                    )
                                    .clicked()
                                {
                                    self.keys.reset(act);
                                }
                                if ui
                                    .small_button("None")
                                    .on_hover_text("No key for this command")
                                    .clicked()
                                {
                                    self.keys.clear(act);
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        ui.separator();
        if ui.button("Reset all to defaults").clicked() {
            self.keys.reset_all();
        }
    }
}
