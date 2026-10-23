//! Stems ▸ Separate Stems… dialog and background job handling.

use std::{
    sync::mpsc::{Receiver, channel},
    time::Instant,
};

use eframe::egui::{self, Align2, RichText, vec2};

use super::{AudacityApp, Dialog};
use crate::{
    dsp::separation::{self, CancelHandle, DemucsModel, Stem, StemEvent},
    fileio::{self, WavFormat},
    track::resample,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Builtin,
    Demucs,
}

struct Job {
    rx: Receiver<StemEvent>,
    cancel: CancelHandle,
    progress: Option<f32>,
    last_log: String,
    started: Instant,
    start_pos: usize,
    base_name: String,
    sources: Vec<u64>,
    engine: Engine,
}

pub struct StemsState {
    pub dialog_open: bool,
    engine: Engine,
    low_hz: f32,
    high_hz: f32,
    strength: f32,
    model: DemucsModel,
    two_stems: bool,
    command: String,
    mute_original: bool,
    job: Option<Job>,
}

const KEY_CMD: &str = "demucs_command";

fn default_command() -> String {
    if cfg!(windows) {
        "python -m demucs".into()
    } else {
        "demucs".into()
    }
}

impl StemsState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            dialog_open: false,
            engine: Engine::Demucs,
            low_hz: 150.0,
            high_hz: 9000.0,
            strength: 2.0,
            model: DemucsModel::HtDemucs,
            two_stems: false,
            command: storage
                .and_then(|s| eframe::get_value(s, KEY_CMD))
                .unwrap_or_else(default_command),
            mute_original: true,
            job: None,
        }
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_CMD, &self.command);
    }

    pub fn demucs_command(&self) -> &str {
        &self.command
    }
}

fn title_case(s: &str) -> String {
    match s {
        "no_vocals" => "Instrumental".into(),
        _ => {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

impl AudacityApp {
    fn start_stem_job(&mut self) {
        if self.stems.job.is_some() || self.busy() {
            return;
        }
        let (start, end) = self.project.effect_range();
        if self.project.tracks.is_empty() || end <= start {
            self.dialog = Some(Dialog::Message("There is no audio to separate.".into()));
            return;
        }
        self.engine.stop_playback();
        let targets = self.project.target_tracks();
        let mix = self.project.mixdown(start, end, true);
        let base_name = if targets.len() == 1 {
            self.project.tracks[targets[0]].name.clone()
        } else {
            "Mix".into()
        };
        let rate = self.project.rate;
        let (tx, rx) = channel();
        let cancel = CancelHandle::default();
        let engine = self.stems.engine;
        match engine {
            Engine::Builtin => {
                if mix.len() < 2 {
                    self.dialog = Some(Dialog::Message(
                        "The built-in separator needs a stereo mix (it uses the difference between left and right).\nUse the Demucs AI engine for mono audio.".into(),
                    ));
                    return;
                }
                let (lo, hi, strength) =
                    (self.stems.low_hz, self.stems.high_hz, self.stems.strength);
                std::thread::spawn(move || {
                    let (voc, inst) =
                        separation::centre_extract(&mix[0], &mix[1], rate, lo, hi, strength);
                    let _ = tx.send(StemEvent::Done(Ok(vec![
                        Stem {
                            name: "vocals".into(),
                            channels: voc,
                            rate,
                        },
                        Stem {
                            name: "instrumental".into(),
                            channels: inst,
                            rate,
                        },
                    ])));
                });
            }
            Engine::Demucs => {
                let (command, model, two) = (
                    self.stems.command.clone(),
                    self.stems.model,
                    self.stems.two_stems,
                );
                let cancel2 = cancel.clone();
                let safe_name: String = base_name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                std::thread::spawn(move || {
                    let dir = std::env::temp_dir().join(format!(
                        "rust_audacity_stems_{}_{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_nanos())
                    ));
                    let result = (|| -> anyhow::Result<Vec<Stem>> {
                        std::fs::create_dir_all(&dir)?;
                        let input = dir.join(format!("{safe_name}.wav"));
                        fileio::export_wav(&input, &mix, rate, WavFormat::Float32)?;
                        let _ = tx.send(StemEvent::Log(format!(
                            "Running {command} -n {} …",
                            model.id()
                        )));
                        let files = separation::run_demucs(
                            &command,
                            model,
                            two,
                            &input,
                            &dir.join("out"),
                            &tx,
                            &cancel2,
                        )?;
                        files
                            .into_iter()
                            .map(|(name, path)| {
                                let a = fileio::import(&path)?;
                                Ok(Stem {
                                    name,
                                    channels: a.channels,
                                    rate: a.rate,
                                })
                            })
                            .collect()
                    })();
                    let _ = std::fs::remove_dir_all(&dir);
                    let _ = tx.send(StemEvent::Done(result.map_err(|e| format!("{e:#}"))));
                });
            }
        }
        self.stems.job = Some(Job {
            rx,
            cancel,
            progress: None,
            last_log: String::new(),
            started: Instant::now(),
            start_pos: start,
            base_name,
            sources: targets.iter().map(|&i| self.project.tracks[i].id).collect(),
            engine,
        });
        self.stems.dialog_open = false;
    }

    pub(super) fn poll_stems(&mut self, ctx: &egui::Context) {
        let Some(job) = self.stems.job.as_mut() else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let mut done = None;
        while let Ok(ev) = job.rx.try_recv() {
            match ev {
                StemEvent::Progress(p) => job.progress = Some(p),
                StemEvent::Log(l) => job.last_log = l,
                StemEvent::Done(r) => done = Some(r),
            }
        }
        let Some(result) = done else { return };
        let job = self.stems.job.take().unwrap();
        match result {
            Ok(stems) => {
                self.project.push_undo("Separate Stems");
                if self.stems.mute_original {
                    for t in &mut self.project.tracks {
                        if job.sources.contains(&t.id) {
                            t.mute = true;
                        }
                    }
                }
                for t in &mut self.project.tracks {
                    t.selected = false;
                }
                let n = stems.len();
                for stem in stems {
                    let mut chans = resample(&stem.channels, stem.rate, self.project.rate);
                    for c in &mut chans {
                        c.splice(0..0, std::iter::repeat_n(0.0, job.start_pos));
                    }
                    let idx = self.project.add_track(
                        format!("{} – {}", job.base_name, title_case(&stem.name)),
                        chans,
                    );
                    self.project.tracks[idx].selected = true;
                }
                self.status = format!(
                    "Separated into {n} stems in {:.1} s",
                    job.started.elapsed().as_secs_f32()
                );
            }
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!("Stem separation failed.\n\n{e}")))
            }
        }
    }

    pub(super) fn stems_windows(&mut self, ctx: &egui::Context) {
        // Progress window while a job runs.
        if let Some(job) = &self.stems.job {
            let mut cancel = false;
            egui::Window::new("Separating stems…")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_width(420.0);
                    let secs = job.started.elapsed().as_secs();
                    ui.label(match job.engine {
                        Engine::Builtin => "Built-in centre extraction".to_string(),
                        Engine::Demucs => {
                            "Demucs AI model (this can take as long as the song on a CPU)"
                                .to_string()
                        }
                    });
                    match job.progress {
                        Some(p) => ui.add(egui::ProgressBar::new(p).show_percentage()),
                        None => ui.add(egui::ProgressBar::new(0.0).animate(true).text("working…")),
                    };
                    ui.label(format!("Elapsed: {}:{:02}", secs / 60, secs % 60));
                    if !job.last_log.is_empty() {
                        ui.label(
                            RichText::new(job.last_log.chars().take(120).collect::<String>())
                                .small()
                                .weak(),
                        );
                    }
                    if job.engine == Engine::Demucs && ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            if cancel {
                job.cancel.cancel();
            }
            return;
        }
        if !self.stems.dialog_open {
            return;
        }
        let mut open = true;
        let mut go = false;
        egui::Window::new("Separate Stems").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.set_width(470.0);
            ui.label(RichText::new("Splits the selection (or whole project) into separate tracks, e.g. vocals, drums, bass and other instruments.").weak());
            ui.separator();
            let s = &mut self.stems;
            ui.radio_value(&mut s.engine, Engine::Demucs, RichText::new("Demucs AI (best quality, 2/4/6 stems)").strong());
            ui.indent("demucs", |ui| {
                ui.add_enabled_ui(s.engine == Engine::Demucs, |ui| {
                    egui::Grid::new("demucs_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label("Model:");
                        egui::ComboBox::from_id_salt("demucs_model").width(280.0).selected_text(s.model.label()).show_ui(ui, |ui| {
                            for m in DemucsModel::ALL {
                                ui.selectable_value(&mut s.model, m, m.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Stems:");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut s.two_stems, false, if s.model == DemucsModel::HtDemucs6s { "All 6" } else { "All 4" });
                            ui.radio_value(&mut s.two_stems, true, "Vocals + Instrumental");
                        });
                        ui.end_row();
                        ui.label("Command:");
                        ui.text_edit_singleline(&mut s.command);
                        ui.end_row();
                    });
                    ui.label(
                        RichText::new(
                            "Requires Python 3 with Demucs installed once:   pip install -U demucs\n\
                             The model (~80 MB) downloads automatically on first use. A GPU makes it much faster.",
                        )
                        .small()
                        .weak(),
                    );
                });
            });
            ui.add_space(4.0);
            ui.radio_value(&mut s.engine, Engine::Builtin, RichText::new("Built-in (fast, no install – Vocals + Instrumental)").strong());
            ui.indent("builtin", |ui| {
                ui.add_enabled_ui(s.engine == Engine::Builtin, |ui| {
                    egui::Grid::new("builtin_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        ui.label("Vocal range:");
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut s.low_hz).range(40.0..=1000.0).suffix(" Hz"));
                            ui.label("to");
                            ui.add(egui::DragValue::new(&mut s.high_hz).range(2000.0..=20000.0).suffix(" Hz"));
                        });
                        ui.end_row();
                        ui.label("Strictness:");
                        ui.add(egui::Slider::new(&mut s.strength, 0.5..=6.0)).on_hover_text("Higher = only strongly centred sound goes to the vocal stem");
                        ui.end_row();
                    });
                    ui.label(RichText::new("Works on stereo mixes where the vocal is panned centre. Quality is below Demucs.").small().weak());
                });
            });
            ui.separator();
            ui.checkbox(&mut s.mute_original, "Mute the original track(s) afterwards");
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Separate").strong()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    s.dialog_open = false;
                }
            });
        });
        if !open {
            self.stems.dialog_open = false;
        }
        if go {
            self.start_stem_job();
        }
    }
}
