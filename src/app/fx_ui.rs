//! Effect dialogs (with Preview) and the Effect Chains editor.

use std::mem::discriminant;

use eframe::egui::{self, Align2, Color32, Key, RichText, vec2};

use super::{AudacityApp, Dialog};
use crate::{
    dsp::denoise,
    effects::{self, Effect, EffectChain, builtin_chains, lin_to_db},
    ui_effects,
};

const PREVIEW_SECS: f32 = 6.0;

#[derive(Default)]
pub struct ChainsState {
    pub open: bool,
    user: Vec<EffectChain>,
    /// Index into builtins ++ user.
    selected: usize,
    add_choice: usize,
    /// Last-used settings for each effect type.
    last_params: Vec<Effect>,
}

const KEY_CHAINS: &str = "effect_chains";
const KEY_LAST: &str = "effect_last_params";

impl ChainsState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let mut s = Self::default();
        if let Some(st) = storage {
            s.user = eframe::get_value(st, KEY_CHAINS).unwrap_or_default();
            s.last_params = eframe::get_value(st, KEY_LAST).unwrap_or_default();
        }
        s
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_CHAINS, &self.user);
        eframe::set_value(storage, KEY_LAST, &self.last_params);
    }

    pub fn last_used(&self, fx: &Effect) -> Option<Effect> {
        self.last_params
            .iter()
            .find(|p| discriminant(*p) == discriminant(fx))
            .copied()
    }

    fn remember(&mut self, fx: &Effect) {
        self.last_params
            .retain(|p| discriminant(p) != discriminant(fx));
        self.last_params.push(*fx);
    }

    pub(super) fn all(&self) -> Vec<(EffectChain, bool)> {
        builtin_chains()
            .into_iter()
            .map(|c| (c, true))
            .chain(self.user.iter().cloned().map(|c| (c, false)))
            .collect()
    }
}

impl AudacityApp {
    pub(super) fn start_preview(&mut self, chain: &[Effect]) {
        self.engine.stop_playback();
        match effects::preview(&self.project, chain, PREVIEW_SECS) {
            Ok(tmp) => match self.engine.play(&tmp, 0, None, false) {
                Ok(()) => {
                    self.previewing = true;
                    self.preview_error = None;
                }
                Err(e) => self.preview_error = Some(format!("Preview failed: {e:#}")),
            },
            Err(e) => self.preview_error = Some(format!("Preview: {e}")),
        }
    }

    pub(super) fn stop_preview(&mut self) {
        if self.previewing {
            self.engine.stop_playback();
            self.previewing = false;
        }
    }

    fn grab_noise_profile(&mut self) {
        let sel = self.project.selection;
        if sel.is_empty() {
            self.status = "Select a few seconds of noise-only audio first".into();
            return;
        }
        let mut chans: Vec<Vec<f32>> = Vec::new();
        for i in self.project.target_tracks() {
            chans.extend(self.project.tracks[i].extract(sel.start, sel.end));
        }
        let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        match denoise::learn_profile(&refs, self.project.rate) {
            Some(p) => {
                self.status = format!(
                    "Noise profile captured from {:.2} s",
                    sel.len() as f64 / self.project.rate as f64
                );
                self.project.noise_profile = Some(p);
            }
            None => self.status = "Noise selection is too short (needs at least 25 ms)".into(),
        }
    }

    pub(super) fn effect_dialog(&mut self, ctx: &egui::Context, mut fx: Effect) {
        let mut ok = false;
        let mut cancel = false;
        let mut open = true;
        let mut preview = false;
        let mut grab_profile = false;
        let rate = self.project.rate;
        let peak = effects::selection_peak(&self.project);
        egui::Window::new(fx.name())
            .id(egui::Id::unique("effect_dialog"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                if !fx.help().is_empty() {
                    ui.label(RichText::new(fx.help()).weak());
                    ui.add_space(4.0);
                }
                if let Effect::NoiseReduction { .. } = fx {
                    ui.horizontal(|ui| {
                        if ui
                            .button("Get Noise Profile")
                            .on_hover_text("Uses the current selection as the noise sample")
                            .clicked()
                        {
                            grab_profile = true;
                        }
                        match &self.project.noise_profile {
                            Some(p) => ui.colored_label(
                                Color32::from_rgb(120, 210, 120),
                                format!("✔ profile ready ({} frames)", p.frames),
                            ),
                            None => ui.colored_label(Color32::YELLOW, "no profile yet"),
                        };
                    });
                    ui.separator();
                }
                ui_effects::params_ui(ui, "dialog", &mut fx, rate);
                let mut blocked = false;
                if let Effect::Amplify { db } = fx {
                    let new_peak = lin_to_db(peak) + db;
                    ui.horizontal(|ui| {
                        ui.label(format!("New peak: {new_peak:.2} dBFS"));
                        ui.checkbox(&mut self.allow_clip, "Allow clipping");
                    });
                    if !self.allow_clip && new_peak > 0.0 {
                        ui.colored_label(Color32::YELLOW, "This would clip – OK is disabled.");
                        blocked = true;
                    }
                }
                if let Some(err) = &self.preview_error {
                    ui.colored_label(Color32::from_rgb(255, 140, 90), err);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let label = if self.previewing {
                        "⏹ Stop Preview"
                    } else {
                        "▶ Preview"
                    };
                    if ui
                        .button(label)
                        .on_hover_text(format!(
                            "Plays the first {PREVIEW_SECS:.0} s of the selection with this effect"
                        ))
                        .clicked()
                    {
                        preview = true;
                    }
                    ui.add_space(20.0);
                    if ui
                        .add_enabled(!blocked, egui::Button::new("   OK   "))
                        .clicked()
                    {
                        ok = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });

        if grab_profile {
            self.grab_noise_profile();
        }
        if preview {
            if self.previewing {
                self.stop_preview();
            } else {
                self.start_preview(&[fx]);
            }
        }
        if !open || cancel {
            self.stop_preview();
            self.preview_error = None;
            self.dialog = None;
            return;
        }
        if ok {
            self.stop_preview();
            self.preview_error = None;
            self.chains.remember(&fx);
            self.dialog = None;
            self.apply_effect(&fx);
            return;
        }
        self.dialog = Some(Dialog::Effect(fx));
    }

    pub(super) fn chains_window(&mut self, ctx: &egui::Context) {
        if !self.chains.open {
            return;
        }
        let all = self.chains.all();
        if self.chains.selected >= all.len() {
            self.chains.selected = 0;
        }
        let n_builtin = builtin_chains().len();
        let rate = self.project.rate;
        let mut open = true;
        let mut apply: Option<EffectChain> = None;
        let mut preview: Option<Vec<Effect>> = None;
        egui::Window::new("Effect Chains")
            .default_size(vec2(760.0, 480.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(RichText::new("Run several effects in order with one click. Built-in chains (★) can be duplicated and edited.").weak());
                ui.separator();
                ui.horizontal_top(|ui| {
                    // Chain list.
                    ui.vertical(|ui| {
                        ui.set_width(230.0);
                        egui::ScrollArea::vertical().id_salt("chain_list").max_height(360.0).show(ui, |ui| {
                            for (i, (c, builtin)) in all.iter().enumerate() {
                                let label = if *builtin { format!("★ {}", c.name) } else { c.name.clone() };
                                if ui.selectable_label(self.chains.selected == i, label).clicked() {
                                    self.chains.selected = i;
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("➕ New chain").clicked() {
                            self.chains.user.push(EffectChain { name: format!("My Chain {}", self.chains.user.len() + 1), steps: Vec::new() });
                            self.chains.selected = n_builtin + self.chains.user.len() - 1;
                        }
                    });
                    ui.separator();
                    // Chain details.
                    ui.vertical(|ui| {
                        let sel = self.chains.selected;
                        let (chain, builtin) = all[sel].clone();
                        if builtin {
                            ui.heading(&chain.name);
                            for (k, st) in chain.steps.iter().enumerate() {
                                ui.label(format!("{}. {}", k + 1, st.name()));
                            }
                            ui.add_space(6.0);
                            if ui.button("Duplicate to edit").clicked() {
                                let mut c = chain.clone();
                                c.name = format!("{} (copy)", c.name);
                                self.chains.user.push(c);
                                self.chains.selected = n_builtin + self.chains.user.len() - 1;
                            }
                        } else {
                            let ui_idx = sel - n_builtin;
                            let mut remove_chain = false;
                            {
                                let c = &mut self.chains.user[ui_idx];
                                ui.horizontal(|ui| {
                                    ui.label("Name:");
                                    ui.text_edit_singleline(&mut c.name);
                                    if ui.button("🗑 Delete chain").clicked() {
                                        remove_chain = true;
                                    }
                                });
                                let mut action: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 remove)
                                egui::ScrollArea::vertical().id_salt("chain_steps").max_height(300.0).show(ui, |ui| {
                                    let n = c.steps.len();
                                    for (k, st) in c.steps.iter_mut().enumerate() {
                                        ui.horizontal(|ui| {
                                            if ui.add_enabled(k > 0, egui::Button::new("↑").small()).clicked() {
                                                action = Some((k, -1));
                                            }
                                            if ui.add_enabled(k + 1 < n, egui::Button::new("↓").small()).clicked() {
                                                action = Some((k, 1));
                                            }
                                            if ui.small_button("✕").clicked() {
                                                action = Some((k, 0));
                                            }
                                            egui::CollapsingHeader::new(format!("{}. {}", k + 1, st.name())).id_salt(("step", ui_idx, k)).show(ui, |ui| {
                                                ui_effects::params_ui(ui, ("chain", ui_idx, k), st, rate);
                                            });
                                        });
                                    }
                                    if n == 0 {
                                        ui.label(RichText::new("No steps yet – add one below.").weak());
                                    }
                                });
                                if let Some((k, dir)) = action {
                                    match dir {
                                        0 => {
                                            c.steps.remove(k);
                                        }
                                        -1 => c.steps.swap(k, k - 1),
                                        _ => c.steps.swap(k, k + 1),
                                    }
                                }
                                ui.horizontal(|ui| {
                                    let cat = Effect::catalog();
                                    let choice = self.chains.add_choice.min(cat.len() - 1);
                                    egui::ComboBox::from_id_salt("add_step").width(220.0).selected_text(cat[choice].name()).show_ui(ui, |ui| {
                                        for (i, f) in cat.iter().enumerate() {
                                            ui.selectable_value(&mut self.chains.add_choice, i, format!("{} – {}", f.category().label(), f.name()));
                                        }
                                    });
                                    if ui.button("➕ Add step").clicked() {
                                        c.steps.push(cat[choice]);
                                    }
                                });
                            }
                            if remove_chain {
                                self.chains.user.remove(ui_idx);
                                self.chains.selected = 0;
                                return;
                            }
                        }
                        ui.separator();
                        let chain_now = self.chains.all()[self.chains.selected.min(self.chains.all().len() - 1)].0.clone();
                        if let Some(err) = &self.preview_error {
                            ui.colored_label(Color32::from_rgb(255, 140, 90), err);
                        }
                        ui.horizontal(|ui| {
                            let label = if self.previewing { "⏹ Stop Preview" } else { "▶ Preview" };
                            if ui.button(label).clicked() {
                                preview = Some(chain_now.steps.clone());
                            }
                            if ui.button(RichText::new("Apply to selection").strong()).clicked() {
                                apply = Some(chain_now.clone());
                            }
                        });
                    });
                });
            });
        if let Some(steps) = preview {
            if self.previewing {
                self.stop_preview();
            } else {
                self.start_preview(&steps);
            }
        }
        if let Some(chain) = apply {
            self.stop_preview();
            self.start_effect_job(format!("Chain “{}”", chain.name), chain.steps.clone());
        }
        if !open {
            self.stop_preview();
            self.chains.open = false;
        }
    }
}
