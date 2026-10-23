//! File ▸ Batch Process…: run an effect chain over many files and export them, in the
//! background (you can keep editing meanwhile).

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, channel},
    },
    time::Instant,
};

use eframe::egui::{self, Color32, RichText};

use super::AudacityApp;
use crate::{
    batch::{self, BatchSettings, FileResult},
    fileio::ExportFormat,
};

enum Msg {
    File(usize, usize, String),
    Result(FileResult),
    Done,
}

struct Run {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    done: usize,
    total: usize,
    current: String,
}

pub struct BatchState {
    pub open: bool,
    files: Vec<PathBuf>,
    /// Index into the chain list (None = no processing, just convert).
    chain: Option<usize>,
    fmt: ExportFormat,
    out_dir: Option<PathBuf>,
    suffix: String,
    overwrite: bool,
    title_from_file: bool,
    use_project_tags: bool,
    recursive: bool,
    run: Option<Run>,
    results: Vec<FileResult>,
}

impl Default for BatchState {
    fn default() -> Self {
        Self {
            open: false,
            files: Vec::new(),
            chain: Some(0),
            fmt: ExportFormat::Mp3 { kbps: 192 },
            out_dir: None,
            suffix: "_processed".into(),
            overwrite: false,
            title_from_file: true,
            use_project_tags: false,
            recursive: false,
            run: None,
            results: Vec::new(),
        }
    }
}

fn short(p: &std::path::Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

impl AudacityApp {
    fn start_batch(&mut self) {
        let chains = self.chains.all();
        let chain = self
            .batch
            .chain
            .and_then(|i| chains.get(i))
            .map(|(c, _)| c.steps.clone())
            .unwrap_or_default();
        let settings = BatchSettings {
            chain,
            format: self.batch.fmt,
            out_dir: self.batch.out_dir.clone(),
            suffix: self.batch.suffix.clone(),
            overwrite: self.batch.overwrite,
            meta: self
                .batch
                .use_project_tags
                .then(|| self.project.metadata.clone()),
            title_from_file: self.batch.title_from_file,
        };
        let files = self.batch.files.clone();
        let total = files.len();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::Builder::new()
            .name("batch".into())
            .spawn(move || {
                let tx2 = tx.clone();
                batch::run(
                    &files,
                    &settings,
                    &c2,
                    |i, n, p| {
                        let _ = tx.send(Msg::File(i, n, short(p)));
                    },
                    |r| {
                        let _ = tx2.send(Msg::Result(r.clone()));
                    },
                );
                let _ = tx.send(Msg::Done);
            })
            .expect("spawn batch thread");
        self.batch.results.clear();
        self.batch.run = Some(Run {
            rx,
            cancel,
            started: Instant::now(),
            done: 0,
            total,
            current: String::new(),
        });
    }

    pub(super) fn poll_batch(&mut self, ctx: &egui::Context) {
        let Some(run) = &mut self.batch.run else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(150));
        let mut finished = false;
        while let Ok(m) = run.rx.try_recv() {
            match m {
                Msg::File(i, n, name) => {
                    run.done = i;
                    run.total = n;
                    run.current = name;
                }
                Msg::Result(r) => {
                    run.done += 1;
                    self.batch.results.push(r);
                }
                Msg::Done => finished = true,
            }
        }
        if finished {
            let ok = self
                .batch
                .results
                .iter()
                .filter(|r| r.error.is_none())
                .count();
            let secs = run.started.elapsed().as_secs_f32();
            let cancelled = run.cancel.load(Ordering::Relaxed);
            self.status = format!(
                "Batch {}: {ok} of {} file(s) processed in {secs:.0} s",
                if cancelled { "cancelled" } else { "finished" },
                run.total
            );
            self.batch.run = None;
            self.batch.open = true;
        }
    }

    pub(super) fn batch_window(&mut self, ctx: &egui::Context) {
        if !self.batch.open {
            return;
        }
        let mut open = true;
        let chains = self.chains.all();
        let mut start = false;
        egui::Window::new("Batch Process")
            .default_width(620.0)
            .open(&mut open)
            .show(ctx, |ui| {
                let b = &mut self.batch;
                let running = b.run.is_some();
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("Files ({})", b.files.len()));
                        if ui.button("Add files…").clicked()
                            && let Some(fs) = rfd::FileDialog::new()
                                .add_filter("Audio", &batch::AUDIO_EXTS)
                                .pick_files()
                        {
                            for f in fs {
                                if !b.files.contains(&f) {
                                    b.files.push(f);
                                }
                            }
                        }
                        if ui.button("Add folder…").clicked()
                            && let Some(d) = rfd::FileDialog::new().pick_folder()
                        {
                            for f in batch::audio_files_in(&d, b.recursive) {
                                if !b.files.contains(&f) {
                                    b.files.push(f);
                                }
                            }
                        }
                        ui.checkbox(&mut b.recursive, "include sub-folders");
                        if !b.files.is_empty() && ui.button("Clear").clicked() {
                            b.files.clear();
                        }
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("batch_files")
                        .max_height(140.0)
                        .show(ui, |ui| {
                            if b.files.is_empty() {
                                ui.label(RichText::new("Add audio files or a whole folder.").weak());
                            }
                            let mut remove = None;
                            for (i, f) in b.files.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.small_button("✕").clicked() {
                                        remove = Some(i);
                                    }
                                    ui.label(short(f)).on_hover_text(f.display().to_string());
                                });
                            }
                            if let Some(i) = remove {
                                b.files.remove(i);
                            }
                        });
                    ui.separator();
                    egui::Grid::new("batch_opts").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Process with:");
                        let name = b
                            .chain
                            .and_then(|i| chains.get(i))
                            .map(|(c, _)| c.name.clone())
                            .unwrap_or_else(|| "Nothing (just convert)".into());
                        egui::ComboBox::from_id_salt("batch_chain")
                            .selected_text(name)
                            .width(280.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut b.chain, None, "Nothing (just convert)");
                                for (i, (c, builtin)) in chains.iter().enumerate() {
                                    let label = if *builtin { format!("★ {}", c.name) } else { c.name.clone() };
                                    ui.selectable_value(&mut b.chain, Some(i), label);
                                }
                            });
                        ui.end_row();
                        if let Some((c, _)) = b.chain.and_then(|i| chains.get(i)) {
                            ui.label("");
                            ui.label(
                                RichText::new(c.steps.iter().map(|s| s.name()).collect::<Vec<_>>().join(" → "))
                                    .small()
                                    .weak(),
                            );
                            ui.end_row();
                        }
                        Self::export_format_ui(ui, &mut b.fmt);
                        ui.label("Save to:");
                        ui.horizontal(|ui| {
                            match &b.out_dir {
                                Some(d) => {
                                    ui.label(d.display().to_string());
                                }
                                None => {
                                    ui.label("next to each file");
                                }
                            }
                            if ui.button("Choose folder…").clicked()
                                && let Some(d) = rfd::FileDialog::new().pick_folder()
                            {
                                b.out_dir = Some(d);
                            }
                            if b.out_dir.is_some() && ui.small_button("Next to each file").clicked() {
                                b.out_dir = None;
                            }
                        });
                        ui.end_row();
                        ui.label("Name:");
                        ui.horizontal(|ui| {
                            ui.label("original name +");
                            ui.add(egui::TextEdit::singleline(&mut b.suffix).desired_width(110.0));
                            ui.label(format!(".{}", b.fmt.extension()));
                        });
                        ui.end_row();
                        ui.label("");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut b.overwrite, "Overwrite existing outputs (otherwise “name (2)”)");
                            ui.checkbox(&mut b.title_from_file, "Set each file's title tag to its file name");
                            ui.checkbox(
                                &mut b.use_project_tags,
                                "Also tag with this project's artist / album / cover (File ▸ Metadata)",
                            );
                        });
                        ui.end_row();
                    });
                });
                ui.separator();
                match &b.run {
                    Some(run) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{} of {}: {}", (run.done + 1).min(run.total), run.total, run.current));
                        });
                        ui.add(
                            egui::ProgressBar::new(run.done as f32 / run.total.max(1) as f32)
                                .show_percentage(),
                        );
                        if ui.button("Cancel").clicked() {
                            run.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    None => {
                        let label = format!("▶ Process {} file(s)", b.files.len());
                        if ui
                            .add_enabled(!b.files.is_empty(), egui::Button::new(RichText::new(label).strong()))
                            .clicked()
                        {
                            start = true;
                        }
                    }
                }
                if !b.results.is_empty() {
                    ui.separator();
                    let ok = b.results.iter().filter(|r| r.error.is_none()).count();
                    ui.label(format!("{ok} done, {} failed", b.results.len() - ok));
                    egui::ScrollArea::vertical()
                        .id_salt("batch_results")
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for r in &b.results {
                                match (&r.output, &r.error) {
                                    (Some(o), _) => {
                                        ui.horizontal(|ui| {
                                            ui.colored_label(Color32::from_rgb(110, 210, 120), "✓");
                                            ui.label(format!("{} → {}", short(&r.input), short(o)))
                                                .on_hover_text(o.display().to_string());
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} · {:.1} s",
                                                    r.lufs_after.map_or("—".into(), |l| format!("{l:.1} LUFS")),
                                                    r.secs
                                                ))
                                                .small()
                                                .weak(),
                                            );
                                        });
                                    }
                                    (None, e) => {
                                        ui.horizontal(|ui| {
                                            ui.colored_label(Color32::from_rgb(235, 110, 90), "✗");
                                            ui.label(short(&r.input));
                                            ui.label(
                                                RichText::new(e.clone().unwrap_or_default())
                                                    .small()
                                                    .color(Color32::from_rgb(235, 150, 120)),
                                            );
                                        });
                                    }
                                }
                            }
                        });
                }
            });
        if start {
            self.start_batch();
        }
        if !open {
            // Closing the window doesn't stop a running batch; it reopens when done.
            self.batch.open = false;
        }
    }
}
