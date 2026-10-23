//! Project files (Open / Save / Save As), the "save changes?" prompt and the export dialog.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Align2, vec2};

use super::{AudacityApp, Dialog, View};
use crate::{
    fileio::{ExportFormat, MP3_BITRATES, WavFormat},
    project::Project,
    projectfile::{self, ViewState},
};

/// What to do once the user has dealt with unsaved changes.
#[derive(Clone, Debug)]
pub(super) enum Pending {
    New,
    /// Open a project; `None` shows the file picker.
    Open(Option<PathBuf>),
    Exit,
}

impl Pending {
    fn verb(&self) -> &'static str {
        match self {
            Pending::New => "starting a new project",
            Pending::Open(_) => "opening another project",
            Pending::Exit => "closing",
        }
    }
}

const STORAGE_EXPORT: &str = "export_format";

impl AudacityApp {
    pub(super) fn has_unsaved_changes(&self) -> bool {
        self.project.dirty && !self.project.tracks.is_empty()
    }

    /// Run `action`, first asking to save if there are unsaved changes.
    pub(super) fn confirm_then(&mut self, action: Pending) {
        if self.has_unsaved_changes() {
            self.dialog = Some(Dialog::ConfirmUnsaved(action));
        } else {
            self.run_pending(action);
        }
    }

    fn run_pending(&mut self, action: Pending) {
        match action {
            Pending::New => self.new_project(),
            Pending::Open(Some(path)) => self.open_project(&path),
            Pending::Open(None) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Rust Audacity project", &[projectfile::EXTENSION])
                    .pick_file()
                {
                    self.open_project(&path);
                }
            }
            Pending::Exit => {
                self.close_confirmed = true;
                self.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    pub(super) fn open_project(&mut self, path: &Path) {
        if self.busy() {
            return;
        }
        match projectfile::load(path) {
            Ok(loaded) => {
                self.stop();
                self.project = loaded.project;
                self.title = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Project".into());
                self.project_path = Some(path.to_path_buf());
                self.add_recent(path);
                match loaded.view {
                    Some(v) if v.samples_per_pixel > 0.0 => {
                        self.view = View {
                            left: v.left.max(0.0),
                            spp: v.samples_per_pixel,
                        }
                    }
                    _ => self.fit_pending = true,
                }
                self.status = format!("Opened {}", path.display());
            }
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!(
                    "Could not open the project:\n{e:#}"
                )))
            }
        }
    }

    /// Save to the current project file, or ask for one. Returns true if saved.
    pub(super) fn save_project(&mut self, save_as: bool) -> bool {
        if self.busy() {
            return false;
        }
        self.capture_plugin_states();
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let Some(mut p) = rfd::FileDialog::new()
                    .add_filter("Rust Audacity project", &[projectfile::EXTENSION])
                    .set_file_name(format!("{}.{}", self.title, projectfile::EXTENSION))
                    .save_file()
                else {
                    return false;
                };
                if !projectfile::is_project_path(&p) {
                    p.set_extension(projectfile::EXTENSION);
                }
                p
            }
        };
        let view = ViewState {
            left: self.view.left,
            samples_per_pixel: self.view.spp,
        };
        match projectfile::save(&path, &self.project, Some(view)) {
            Ok(()) => {
                self.project.dirty = false;
                self.project_path = Some(path.clone());
                self.add_recent(&path);
                self.title = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.title.clone());
                self.status = format!("Saved {}", path.display());
                true
            }
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!(
                    "Could not save the project:\n{e:#}"
                )));
                false
            }
        }
    }

    /// The "Save changes?" prompt.
    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context, action: Pending) {
        #[derive(PartialEq)]
        enum Choice {
            None,
            Save,
            Discard,
            Cancel,
        }
        let mut choice = Choice::None;
        let mut open = true;
        egui::Window::new("Save changes?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Save changes to “{}” before {}?",
                    self.title,
                    action.verb()
                ));
                ui.label(
                    egui::RichText::new("If you don't save, your changes will be lost.").weak(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("  Save  ").clicked() {
                        choice = Choice::Save;
                    }
                    if ui.button("Don't Save").clicked() {
                        choice = Choice::Discard;
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Choice::Cancel;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    choice = Choice::Cancel;
                }
            });
        if !open {
            choice = Choice::Cancel;
        }
        match choice {
            Choice::None => self.dialog = Some(Dialog::ConfirmUnsaved(action)),
            Choice::Cancel => self.dialog = None,
            Choice::Discard => {
                self.dialog = None;
                self.run_pending(action);
            }
            Choice::Save => {
                self.dialog = None;
                if self.save_project(false) {
                    self.run_pending(action);
                }
            }
        }
    }

    // ----------------------------------------------------------------- export

    pub(super) fn open_export_dialog(&mut self, selection_only: bool) {
        self.dialog = Some(Dialog::Export {
            fmt: self.export_fmt,
            selection_only,
        });
    }

    /// Format controls for the export dialog (inside a 2-column grid).
    pub(super) fn export_format_ui(ui: &mut egui::Ui, fmt: &mut ExportFormat) {
        ui.label("Format:");
        egui::ComboBox::from_id_salt("fmt_family")
            .selected_text(fmt.family_name())
            .show_ui(ui, |ui| {
                for f in ExportFormat::families() {
                    if ui
                        .selectable_label(fmt.same_family(&f), f.family_name())
                        .clicked()
                        && !fmt.same_family(&f)
                    {
                        *fmt = f;
                    }
                }
            });
        ui.end_row();
        match fmt {
            ExportFormat::Wav(w) => {
                ui.label("Encoding:");
                egui::ComboBox::from_id_salt("wavfmt")
                    .selected_text(w.label())
                    .show_ui(ui, |ui| {
                        for f in [WavFormat::Pcm16, WavFormat::Pcm24, WavFormat::Float32] {
                            ui.selectable_value(w, f, f.label());
                        }
                    });
            }
            ExportFormat::Flac { bits } => {
                ui.label("Bit depth:");
                ui.horizontal(|ui| {
                    ui.selectable_value(bits, 16, "16-bit");
                    ui.selectable_value(bits, 24, "24-bit");
                });
            }
            ExportFormat::Mp3 { kbps } => {
                ui.label("Bitrate:");
                egui::ComboBox::from_id_salt("mp3rate")
                    .selected_text(format!("{kbps} kbps"))
                    .show_ui(ui, |ui| {
                        for r in MP3_BITRATES {
                            let hint = match r {
                                128 => " (small)",
                                192 => " (good)",
                                320 => " (best)",
                                _ => "",
                            };
                            ui.selectable_value(kbps, r, format!("{r} kbps{hint}"));
                        }
                    });
            }
            ExportFormat::Ogg { quality } => {
                ui.label("Quality:");
                let hint = match *quality {
                    0..=2 => "small",
                    3..=5 => "good",
                    6..=8 => "very good",
                    _ => "best",
                };
                ui.add(egui::Slider::new(quality, 0..=10).text(hint));
            }
        }
        ui.end_row();
        ui.label("");
        ui.label(
            egui::RichText::new(match fmt {
                ExportFormat::Wav(_) => "Uncompressed. Largest files, perfect quality.",
                ExportFormat::Flac { .. } => "Lossless compression, about half the size of WAV.",
                ExportFormat::Mp3 { .. } => "Lossy. Plays everywhere.",
                ExportFormat::Ogg { .. } => {
                    "Lossy. Open format, better quality than MP3 at the same size."
                }
            })
            .small()
            .weak(),
        );
        ui.end_row();
    }

    pub(super) fn export(&mut self, fmt: ExportFormat, selection_only: bool) {
        self.export_fmt = fmt;
        let (s, e) = if selection_only && !self.project.selection.is_empty() {
            (self.project.selection.start, self.project.selection.end)
        } else {
            (0, self.project.len())
        };
        if e <= s {
            self.dialog = Some(Dialog::Message("There is no audio to export.".into()));
            return;
        }
        let ext = fmt.extension();
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter(format!("{} audio", fmt.family_name()), &[ext])
            .set_file_name(format!("{}.{ext}", self.title))
            .save_file()
        else {
            return;
        };
        let typed = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if typed.as_deref() != Some(ext) && !(ext == "ogg" && typed.as_deref() == Some("oga")) {
            path.set_extension(ext);
        }
        self.start_export_job(path, fmt, selection_only);
    }

    pub(super) fn load_export_format(storage: Option<&dyn eframe::Storage>) -> ExportFormat {
        storage
            .and_then(|s| eframe::get_value(s, STORAGE_EXPORT))
            .unwrap_or_default()
    }

    pub(super) fn save_export_format(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_EXPORT, &self.export_fmt);
    }

    pub(super) fn reset_to_new_project(&mut self) {
        self.project = Project::default();
        self.project_path = None;
    }
}
