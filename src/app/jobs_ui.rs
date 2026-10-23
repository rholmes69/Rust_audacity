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
    ExportMany {
        dir: PathBuf,
        results: Vec<(String, Result<(), String>)>,
    },
}

/// What goes into exported files besides the audio.
#[derive(Clone, Copy)]
pub(super) struct TagOptions {
    /// Write the project's metadata (title, artist, cover…).
    pub tags: bool,
    /// Write labels as chapters.
    pub chapters: bool,
}

impl Default for TagOptions {
    fn default() -> Self {
        Self {
            tags: true,
            chapters: true,
        }
    }
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
        let meta = (self.tag_opts.tags && !self.project.metadata.is_empty())
            .then(|| self.project.metadata.clone());
        // Chapters inside the exported range, relative to its start.
        let off = (s as f64 * 1000.0 / rate as f64) as u32;
        let end_ms = (e as f64 * 1000.0 / rate as f64) as u32;
        let chapters: Vec<crate::metadata::Chapter> = if self.tag_opts.chapters {
            self.project
                .chapters()
                .into_iter()
                .filter(|c| c.start_ms < end_ms && c.end_ms > off)
                .map(|c| crate::metadata::Chapter {
                    start_ms: c.start_ms.max(off) - off,
                    end_ms: c.end_ms.min(end_ms) - off,
                    title: c.title,
                })
                .collect()
        } else {
            Vec::new()
        };
        let (tx, rx) = channel();
        let p = path.clone();
        std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                let peak = mix.iter().flatten().fold(0f32, |m, x| m.max(x.abs()));
                let result = fileio::export_tagged(&p, &mix, rate, fmt, meta.as_ref(), &chapters)
                    .map_err(|e| format!("{e:#}"));
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

    /// Export every label region as its own file ("Export Multiple").
    /// Returns the number of files that will be written.
    pub(super) fn start_export_many(
        &mut self,
        dir: PathBuf,
        fmt: ExportFormat,
        before_first: bool,
        numbered: bool,
    ) -> Result<usize, String> {
        if self.busy() {
            return Err("Rust Audacity is busy.".into());
        }
        let regions =
            crate::labels::export_regions(&self.project.labels, self.project.len(), before_first);
        if regions.is_empty() {
            return Err("There are no labels to export. Add labels with Ctrl+B first.".into());
        }
        // A lightweight copy of the tracks (audio is shared, not copied).
        let mut snap = crate::project::Project::default();
        snap.rate = self.project.rate;
        snap.tracks = self.project.tracks.clone();
        let base = self.project.metadata.clone();
        let tags = self.tag_opts.tags;
        let n = regions.len();
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p2, c2, d2) = (progress.clone(), cancel.clone(), dir.clone());
        std::thread::Builder::new()
            .name("export-many".into())
            .spawn(move || {
                let mut results = Vec::new();
                for (i, (s, e, name)) in regions.into_iter().enumerate() {
                    if c2.load(Ordering::Relaxed) {
                        break;
                    }
                    let stem = crate::labels::file_name_part(&name);
                    let stem = if numbered {
                        format!("{:02} - {stem}", i + 1)
                    } else {
                        stem
                    };
                    let mut path = d2.join(format!("{stem}.{}", fmt.extension()));
                    let mut k = 2;
                    while path.exists() {
                        path = d2.join(format!("{stem} ({k}).{}", fmt.extension()));
                        k += 1;
                    }
                    let mix = snap.mixdown(s, e, false);
                    let mut meta = base.clone();
                    meta.title = name.clone();
                    meta.track = format!("{}/{n}", i + 1);
                    let r = fileio::export_tagged(
                        &path,
                        &mix,
                        snap.rate,
                        fmt,
                        tags.then_some(&meta),
                        &[],
                    )
                    .map_err(|e| format!("{e:#}"));
                    results.push((
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        r,
                    ));
                    p2.store(((i + 1) as f32 / n as f32 * 1e6) as u32, Ordering::Relaxed);
                }
                let _ = tx.send(Output::ExportMany { dir: d2, results });
            })
            .map_err(|e| e.to_string())?;
        self.job = Some(RunningJob {
            label: format!("Exporting {n} files"),
            started: Instant::now(),
            progress,
            cancel,
            rx,
            determinate: true,
            cancellable: true,
        });
        Ok(n)
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
            Output::ExportMany { dir, results } => {
                let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
                let failed: Vec<String> = results
                    .iter()
                    .filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}")))
                    .collect();
                self.status = format!("Exported {ok} file(s) to {} ({secs:.1} s)", dir.display());
                if !failed.is_empty() {
                    self.dialog = Some(Dialog::Message(format!(
                        "{} file(s) could not be exported:\n{}",
                        failed.len(),
                        failed.join("\n")
                    )));
                }
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
