//! Live effects: a rack of effects per track that you hear while playing (and that are
//! applied on export), without changing the audio until you choose to apply them.

use std::collections::HashSet;

use eframe::egui::{self, Color32, RichText};

use super::AudacityApp;
use crate::{dsp::realtime, ui_effects};

#[derive(Default)]
pub struct LiveFxState {
    /// Track id whose rack window is open.
    pub open_for: Option<u64>,
    add_choice: usize,
    /// Slots whose settings are shown.
    expanded: HashSet<u64>,
}

impl AudacityApp {
    pub(super) fn open_live_fx(&mut self, ti: usize) {
        if let Some(t) = self.project.tracks.get(ti) {
            self.live_fx.open_for = Some(t.id);
        }
    }

    /// The small "FX" toggle in a track's control panel.
    pub(super) fn live_fx_button(&mut self, ui: &mut egui::Ui, ti: usize) {
        let t = &self.project.tracks[ti];
        let on = t.live_fx.iter().filter(|f| f.enabled).count();
        let text = "FX";
        let open = self.live_fx.open_for == Some(t.id);
        let mut b = egui::Button::new(if on > 0 {
            RichText::new(text).color(Color32::from_rgb(120, 220, 140))
        } else {
            RichText::new(text)
        });
        b = b.selected(open);
        let tip = if t.live_fx.is_empty() {
            "Live effects: add EQ, compressor, reverb… that you hear while playing".to_string()
        } else {
            t.live_fx
                .iter()
                .map(|f| format!("{}{}", f.name(), if f.enabled { "" } else { " (off)" }))
                .collect::<Vec<_>>()
                .join(" → ")
        };
        if ui.add(b).on_hover_text(tip).clicked() {
            if open {
                self.live_fx.open_for = None;
            } else {
                self.open_live_fx(ti);
            }
        }
    }

    pub(super) fn live_fx_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.live_fx.open_for else {
            return;
        };
        let Some(ti) = self.project.tracks.iter().position(|t| t.id == id) else {
            self.live_fx.open_for = None;
            return;
        };
        let rate = self.project.rate;
        let mut open = true;
        let mut action: Option<Action> = None;
        let title = format!("Live Effects – {}", self.project.tracks[ti].name);
        egui::Window::new(title)
            .id(egui::Id::unique("live_fx_window"))
            .default_width(420.0)
            .default_pos([520.0, 110.0])
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "You hear these while playing, and they're applied when you export. \
                         The track's audio isn't changed unless you click Apply.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                let n = self.project.tracks[ti].live_fx.len();
                if n == 0 {
                    ui.label(RichText::new("No live effects on this track yet.").italics());
                }
                egui::ScrollArea::vertical().max_height(480.0).show(ui, |ui| {
                    for k in 0..n {
                        let slot = self.project.tracks[ti].live_fx[k].clone();
                        let expanded = self.live_fx.expanded.contains(&slot.id);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let mut on = slot.enabled;
                                if ui.checkbox(&mut on, "").on_hover_text("On / bypass").changed() {
                                    action = Some(Action::Toggle(k));
                                }
                                let name = RichText::new(format!("{}. {}", k + 1, slot.name())).strong();
                                if ui
                                    .add(egui::Label::new(if on { name } else { name.weak() }).sense(egui::Sense::click()))
                                    .on_hover_text("Click to show or hide its settings")
                                    .clicked()
                                {
                                    action = Some(Action::Expand(slot.id));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                        action = Some(Action::Remove(k));
                                    }
                                    if ui.add_enabled(k + 1 < n, egui::Button::new("⏷").small()).on_hover_text("Move down").clicked() {
                                        action = Some(Action::Swap(k, k + 1));
                                    }
                                    if ui.add_enabled(k > 0, egui::Button::new("⏶").small()).on_hover_text("Move up").clicked() {
                                        action = Some(Action::Swap(k - 1, k));
                                    }
                                    if ui
                                        .small_button(if expanded { "Hide" } else { "Edit" })
                                        .clicked()
                                    {
                                        action = Some(Action::Expand(slot.id));
                                    }
                                });
                            });
                            if expanded && slot.plugin.is_some() {
                                let title = format!("{} – {}", slot.name(), self.project.tracks[ti].name);
                                self.plugin_slot_ui(ui, slot.id, &title);
                            } else if expanded {
                                let mut fx = slot.effect;
                                ui_effects::params_ui(ui, ("live", slot.id), &mut fx, rate);
                                if fx != slot.effect {
                                    // Settings changes are heard immediately; they don't
                                    // create undo steps (adding/removing does).
                                    self.project.tracks[ti].live_fx[k].effect = fx;
                                    self.project.dirty = true;
                                }
                            }
                        });
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let cat = realtime::catalog();
                    let plugs: Vec<crate::plugins::PluginDesc> = self.plugins.effects().cloned().collect();
                    let n_builtin = cat.len();
                    let choice = self.live_fx.add_choice.min(n_builtin + plugs.len() - 1);
                    let label = |i: usize| -> String {
                        if i < n_builtin {
                            cat[i].name().to_string()
                        } else {
                            let d = &plugs[i - n_builtin];
                            format!("{} ({})", d.name, d.format.label())
                        }
                    };
                    egui::ComboBox::from_id_salt("live_add")
                        .selected_text(label(choice))
                        .width(240.0)
                        .height(420.0)
                        .show_ui(ui, |ui| {
                            ui.label(RichText::new("Built-in").small().weak());
                            for i in 0..n_builtin {
                                ui.selectable_value(&mut self.live_fx.add_choice, i, label(i));
                            }
                            ui.separator();
                            ui.label(RichText::new("Plugins (VST3 / CLAP)").small().weak());
                            if plugs.is_empty() {
                                ui.label(RichText::new("None found yet: Effect ▸ Plugins… ▸ Scan").small().italics());
                            }
                            for (i, p) in plugs.iter().enumerate() {
                                ui.selectable_value(&mut self.live_fx.add_choice, n_builtin + i, label(n_builtin + i))
                                    .on_hover_text(format!("{} · {}", p.vendor, p.category));
                            }
                        });
                    if ui.button("➕ Add").clicked() {
                        action = Some(if choice < n_builtin {
                            Action::Add(cat[choice])
                        } else {
                            Action::AddPlugin(plugs[choice - n_builtin].clone())
                        });
                    }
                });
                ui.label(
                    RichText::new(
                        "Effects that need the whole recording (Normalize, Noise Reduction, tempo/pitch…) \
                         are in the Effect menu instead.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
                let any_on = self.project.tracks[ti].live_fx.iter().any(|f| f.enabled);
                if ui
                    .add_enabled(any_on && !self.busy(), egui::Button::new("Apply to audio"))
                    .on_hover_text("Process the track's audio with these effects (one undo step) and empty the rack")
                    .clicked()
                {
                    action = Some(Action::Render);
                }
            });
        if !open {
            self.live_fx.open_for = None;
        }
        let Some(a) = action else { return };
        let t = &mut self.project.tracks[ti];
        match a {
            Action::Toggle(k) => {
                let label = if t.live_fx[k].enabled {
                    "Bypass Live Effect"
                } else {
                    "Enable Live Effect"
                };
                self.project.push_undo(label);
                let f = &mut self.project.tracks[ti].live_fx[k];
                f.enabled = !f.enabled;
            }
            Action::Expand(id) => {
                if !self.live_fx.expanded.remove(&id) {
                    self.live_fx.expanded.insert(id);
                }
            }
            Action::Remove(k) => {
                self.project.push_undo("Remove Live Effect");
                self.project.tracks[ti].live_fx.remove(k);
            }
            Action::Swap(a, b) => {
                self.project.push_undo("Reorder Live Effects");
                self.project.tracks[ti].live_fx.swap(a, b);
            }
            Action::Add(fx) => {
                if let Some(id) = self.project.add_live_fx(ti, fx) {
                    self.live_fx.expanded.insert(id);
                }
            }
            Action::AddPlugin(desc) => {
                if let Some(id) = self.project.add_live_plugin(ti, desc) {
                    self.live_fx.expanded.insert(id);
                }
            }
            Action::Render => {
                self.engine.stop_playback();
                self.capture_plugin_states();
                if self.project.render_live_fx(ti) {
                    self.status = "Applied live effects to the track's audio".into();
                }
            }
        }
    }
}

enum Action {
    Toggle(usize),
    Expand(u64),
    Remove(usize),
    Swap(usize, usize),
    Add(crate::effects::Effect),
    AddPlugin(crate::plugins::PluginDesc),
    Render,
}
