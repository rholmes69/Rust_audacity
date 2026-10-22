//! Background jobs: effects, effect chains and exports run on a worker thread so the
//! window stays responsive, with a progress bar and a Cancel button.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, channel},
    },
    time::{Duration, Instant},
};

use eframe::egui::{self, Align2, vec2};

use super::{AudacityApp, Dialog};
use crate::{
    effects::{self, Effect, EffectJob, JobResult},
    fileio::{self, ExportFormat},
};

/// Don't flash a progress window for jobs that finish almost instantly.
const SHOW_AFTER: Duration = Duration::from_millis(250);

enum Output {
    Effect(Result<JobResult, String>),
    Export {
        path: PathBuf,
        result: Result<(), String>,
        peak: f32,
        fmt: ExportFormat,
    },
}

pub(super) struct RunningJob {
    label: String,
    started: Instant,
    /// Progress as fraction × 1e6 (atomics don't do floats).
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Output>,
    /// Some jobs can only report "working…" (a single long step).
    determinate: bool,
    /// Effects can be cancelled; exports are quick enough to just finish.
    cancellable: bool,
}

impl RunningJob {
    fn fraction(&self) -> f32 {
        self.progress.load(Ordering::Relaxed) as f32 / 1e6
    }
}

impl AudacityApp {
    /// Run an effect chain on the current selection in the background.
    pub(super) fn start_effect_job(&mut self, label: String, chain: Vec<Effect>) {
        if self.busy() {
            return;
        }
        self.engine.stop_playback();
        self.previewing = false;
        let job = match EffectJob::prepare(&self.project, label.clone(), chain) {
            Ok(j) => j,
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!("{label}: {e}")));
                return;
            }
        };
        let determinate = job.steps() > 1;
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p, c) = (progress.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("effect".into())
            .spawn(move || {
                let r = job.run(|done, total| {
                    p.store((done as f32 / total as f32 * 1e6) as u32, Ordering::Relaxed);
                    !c.load(Ordering::Relaxed)
                });
                let _ = tx.send(Output::Effect(r));
            })
            .expect("spawn effect thread");
        self.job = Some(RunningJob {
            label,
            started: Instant::now(),
            progress,
            cancel,
            rx,
            determinate,
            cancellable: true,
        });
        self.status = "Processing…".into();
    }

    /// Mix down and encode in the background.
    pub(super) fn start_export_job(
        &mut self,
        path: PathBuf,
        fmt: ExportFormat,
        selection_only: bool,
    ) {
        if self.busy() {
            return;
        }
        let (s, e) = if selection_only && !self.project.selection.is_empty() {
            (self.project.selection.start, self.project.selection.end)
        } else {
            (0, self.project.len())
        };
        let mix = self.project.mixdown(s, e, selection_only);
        let rate = self.project.rate;
        let (tx, rx) = channel();
        let p = path.clone();
        std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                let peak = mix.iter().flatten().fold(0f32, |m, x| m.max(x.abs()));
                let result = fileio::export(&p, &mix, rate, fmt).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Output::Export {
                    path: p,
                    result,
                    peak,
                    fmt,
                });
            })
            .expect("spawn export thread");
        self.job = Some(RunningJob {
            label: format!("Exporting {}", fmt.family_name()),
            started: Instant::now(),
            progress: Arc::new(AtomicU32::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            rx,
            determinate: false,
            cancellable: false,
        });
        self.status = format!("Exporting {}…", path.display());
    }

    /// Running job as JSON (for the agent bridge).
    pub(super) fn job_status(&self) -> serde_json::Value {
        match &self.job {
            None => serde_json::Value::Null,
            Some(j) => serde_json::json!({
                "label": j.label,
                "progress": if j.determinate { Some((j.fraction() * 100.0).round()) } else { None },
                "elapsed_secs": j.started.elapsed().as_secs(),
            }),
        }
    }

    /// Check for a finished job; called every frame.
    pub(super) fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(50));
        let out = match job.rx.try_recv() {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let label = job.label.clone();
                self.job = None;
                self.dialog = Some(Dialog::Message(format!("{label} stopped unexpectedly.")));
                return;
            }
        };
        let job = self.job.take().unwrap();
        let secs = job.started.elapsed().as_secs_f32();
        match out {
            Output::Effect(Ok(result)) => {
                if job.cancel.load(Ordering::Relaxed) {
                    self.status = format!("Cancelled {}", job.label);
                } else if result.commit(&mut self.project) {
                    self.status = format!("Applied {} ({secs:.1} s)", job.label);
                } else {
                    self.status =
                        format!("{}: the tracks were removed while processing", job.label);
                }
            }
            Output::Effect(Err(e)) if e == effects::CANCELLED => {
                self.status = format!("Cancelled {}", job.label);
            }
            Output::Effect(Err(e)) => {
                self.dialog = Some(Dialog::Message(format!("{}: {e}", job.label)));
                self.status = format!("{} failed", job.label);
            }
            Output::Export {
                path,
                result,
                peak,
                fmt,
            } => match result {
                Ok(()) => {
                    self.status = format!("Exported {} ({secs:.1} s)", path.display());
                    if peak > 1.0 && fmt.clips_above_full_scale() {
                        self.dialog = Some(Dialog::Message(format!(
                            "Exported, but the mix peaked at {:+.1} dBFS and was clipped.\n\
                             Try Effect ▸ Dynamics ▸ Limiter or Normalize first.",
                            effects::lin_to_db(peak)
                        )));
                    }
                }
                Err(e) => {
                    self.dialog = Some(Dialog::Message(format!(
                        "Could not export {}:\n{e}",
                        path.display()
                    )));
                    self.status = "Export failed".into();
                }
            },
        }
    }

    /// Progress window shown while a job runs.
    pub(super) fn job_window(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else {
            return;
        };
        let elapsed = job.started.elapsed();
        if elapsed < SHOW_AFTER {
            return;
        }
        let mut cancel = false;
        let can_cancel = job.cancellable;
        egui::Window::new("Working")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(340.0);
                ui.heading(format!("{}…", job.label));
                ui.add_space(6.0);
                let bar = if job.determinate {
                    let f = job.fraction();
                    egui::ProgressBar::new(f).show_percentage()
                } else {
                    egui::ProgressBar::new(0.0).animate(true)
                };
                ui.add(bar.desired_width(320.0));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(format!("{:.0} s elapsed", elapsed.as_secs_f32()));
                    if can_cancel {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    }
                });
                if can_cancel && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if cancel {
            // Signal the worker and forget the job immediately: the window becomes usable
            // right away and whatever the worker produces is discarded.
            job.cancel.store(true, Ordering::Relaxed);
            self.status = format!("Cancelled {}", job.label);
            self.job = None;
        }
    }
}
