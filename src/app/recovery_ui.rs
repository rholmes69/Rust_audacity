//! Autosave in the background, the "recover unsaved work?" window, and File ▸ Open Recent.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, channel},
    time::{Duration, Instant},
};

use eframe::egui::{self, RichText};

use super::{AudacityApp, Dialog, file_ui::Pending};
use crate::{
    projectfile::{self, ViewState},
    recovery::{self, Info, Orphan, RecentFiles, Session},
};

const KEY_RECENT: &str = "recent_files";
const KEY_INTERVAL: &str = "autosave_secs";
/// Choices for Preferences ▸ Autosave (seconds; 0 = off).
pub const INTERVALS: [u32; 5] = [0, 30, 60, 120, 300];

pub struct RecoveryState {
    session: Option<Session>,
    pub interval_secs: u32,
    last_rev: u64,
    last_save: Instant,
    saving: Option<Receiver<Result<(), String>>>,
    pub orphans: Vec<Orphan>,
    pub show_orphans: bool,
    pub recent: RecentFiles,
    pub error: Option<String>,
}

impl RecoveryState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let dir = recovery::recovery_dir();
        // Look for crashed sessions before starting our own.
        let orphans = recovery::find_orphans(&dir);
        let (session, error) = match Session::start(&dir) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("Autosave is off: {e:#}"))),
        };
        Self {
            session,
            interval_secs: storage
                .and_then(|s| eframe::get_value(s, KEY_INTERVAL))
                .unwrap_or(60),
            last_rev: u64::MAX,
            last_save: Instant::now(),
            saving: None,
            show_orphans: !orphans.is_empty(),
            orphans,
            recent: storage
                .and_then(|s| eframe::get_value(s, KEY_RECENT))
                .unwrap_or_default(),
            error,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_RECENT, &self.recent);
        eframe::set_value(storage, KEY_INTERVAL, &self.interval_secs);
    }

    /// Clean exit: remove our autosave.
    pub fn shutdown(&mut self) {
        // Let a write in progress finish so we don't delete under it.
        if let Some(rx) = self.saving.take() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
        if let Some(s) = self.session.take() {
            s.finish();
        }
    }
}

pub fn interval_label(secs: u32) -> String {
    match secs {
        0 => "Off".into(),
        s if s < 60 => format!("Every {s} seconds"),
        60 => "Every minute".into(),
        s => format!("Every {} minutes", s / 60),
    }
}

fn when(unix: u64) -> String {
    let ago = recovery::now().saturating_sub(unix);
    match ago {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", ago / 60),
        3600..=86_399 => format!("{} h ago", ago / 3600),
        _ => format!("{} days ago", ago / 86_400),
    }
}

impl AudacityApp {
    /// Called every frame: start an autosave when the interval has passed and something
    /// changed; forget the autosave once the project has been saved normally.
    pub(super) fn tick_autosave(&mut self) {
        let r = &mut self.recovery;
        if let Some(rx) = &r.saving {
            match rx.try_recv() {
                Ok(res) => {
                    r.saving = None;
                    r.error = res.err().map(|e| format!("Autosave failed: {e}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.egui_ctx
                        .request_repaint_after(Duration::from_millis(200));
                    return;
                }
                Err(_) => r.saving = None,
            }
        }
        let Some(session) = &r.session else { return };
        if !self.project.dirty || self.project.tracks.is_empty() {
            if session.has_autosave() {
                session.clear();
            }
            r.last_rev = self.project.revision;
            return;
        }
        if r.interval_secs == 0 || self.project.revision == r.last_rev {
            return;
        }
        let due = Duration::from_secs(r.interval_secs as u64);
        let elapsed = r.last_save.elapsed();
        if elapsed < due {
            // The UI only redraws on input, so ask to be woken when the autosave is due.
            self.egui_ctx.request_repaint_after(due - elapsed);
            return;
        }
        if self.recording.is_some() || self.job.is_some() {
            self.egui_ctx.request_repaint_after(Duration::from_secs(2));
            return;
        }
        self.capture_plugin_states();
        let r = &mut self.recovery;
        let Some(session) = &r.session else { return };
        // Tracks share their sample buffers, so this copy is cheap; the file is written
        // on a background thread.
        let p = self.project.save_copy();
        let info = Info {
            title: self.title.clone(),
            project_path: self.project_path.clone(),
            saved: 0,
            tracks: 0,
            seconds: 0.0,
        }
        .describe(&p);
        let view = ViewState {
            left: self.view.left,
            samples_per_pixel: self.view.spp,
        };
        let target = session.target();
        let (tx, rx) = channel();
        r.saving = Some(rx);
        r.last_rev = self.project.revision;
        r.last_save = Instant::now();
        std::thread::Builder::new()
            .name("autosave".into())
            .spawn(move || {
                let res = target
                    .write(&p, Some(view), &info)
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(res);
            })
            .expect("spawn autosave thread");
    }

    pub(super) fn add_recent(&mut self, p: &Path) {
        self.recovery.recent.add(p);
    }

    /// File ▸ Open Recent.
    pub(super) fn recent_menu(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.add_enabled_ui(idle, |ui| {
            ui.menu_button("Open Recent", |ui| {
                if self.recovery.recent.0.is_empty() {
                    ui.label(RichText::new("No recent files").weak());
                    return;
                }
                let mut open = None;
                for p in &self.recovery.recent.0 {
                    let name = p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let icon = if projectfile::is_project_path(p) {
                        "📁"
                    } else {
                        "🎵"
                    };
                    if ui
                        .button(format!("{icon} {name}"))
                        .on_hover_text(p.display().to_string())
                        .clicked()
                    {
                        open = Some(p.clone());
                    }
                }
                ui.separator();
                if ui.button("Clear list").clicked() {
                    self.recovery.recent.0.clear();
                    ui.close();
                }
                if let Some(p) = open {
                    ui.close();
                    self.open_recent(p);
                }
            });
        });
    }

    fn open_recent(&mut self, p: PathBuf) {
        if !p.exists() {
            self.recovery.recent.remove(&p);
            self.dialog = Some(Dialog::Message(format!(
                "{} no longer exists, so it was removed from the list.",
                p.display()
            )));
        } else if projectfile::is_project_path(&p) {
            self.confirm_then(Pending::Open(Some(p)));
        } else {
            self.import_path(p);
        }
    }

    fn recover(&mut self, i: usize) {
        if self.has_unsaved_changes() {
            self.dialog = Some(Dialog::Message(
                "Save or close the current project first (File ▸ New), then recover.".into(),
            ));
            return;
        }
        let o = self.recovery.orphans[i].clone();
        match projectfile::load(&o.rap) {
            Ok(loaded) => {
                self.stop();
                self.project = loaded.project;
                self.project.dirty = true;
                self.project.revision += 1;
                self.title = o.info.title.clone();
                self.project_path = o.info.project_path.clone();
                self.fit_pending = true;
                if let Some(v) = loaded.view
                    && v.samples_per_pixel > 0.0
                {
                    self.view.left = v.left.max(0.0);
                    self.view.spp = v.samples_per_pixel;
                    self.fit_pending = false;
                }
                o.discard();
                self.recovery.orphans.remove(i);
                self.status = format!("Recovered “{}”. Save it (Ctrl+S) to keep it.", o.info.title);
            }
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!(
                    "Could not recover this autosave:\n{e:#}"
                )))
            }
        }
    }

    pub(super) fn recovery_window(&mut self, ctx: &egui::Context) {
        if !self.recovery.show_orphans || self.recovery.orphans.is_empty() {
            return;
        }
        let mut recover = None;
        let mut discard = None;
        let mut later = false;
        egui::Window::new("Recover unsaved work")
            .collapsible(false)
            .resizable(false)
            .min_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Rust Audacity didn't close properly last time. These autosaves can be recovered:");
                ui.add_space(6.0);
                egui::Grid::new("orphans").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                    for (i, o) in self.recovery.orphans.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.set_min_width(300.0);
                            ui.strong(&o.info.title);
                            let secs = o.info.seconds;
                            ui.label(
                                RichText::new(format!(
                                    "{} track(s), {}:{:04.1} · autosaved {}",
                                    o.info.tracks,
                                    (secs / 60.0).floor() as u64,
                                    secs % 60.0,
                                    when(o.info.saved)
                                ))
                                .small()
                                .weak(),
                            );
                            if let Some(p) = &o.info.project_path {
                                ui.label(RichText::new(p.display().to_string()).small().weak());
                            }
                        });
                        if ui.button("Recover").clicked() {
                            recover = Some(i);
                        }
                        if ui.button("🗑 Discard").clicked() {
                            discard = Some(i);
                        }
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.separator();
                if ui
                    .button("Decide later")
                    .on_hover_text("Keep the autosaves; you'll be asked again next time")
                    .clicked()
                {
                    later = true;
                }
            });
        if let Some(i) = recover {
            self.recover(i);
        }
        if let Some(i) = discard {
            self.recovery.orphans.remove(i).discard();
        }
        if later {
            self.recovery.show_orphans = false;
        }
    }
}
