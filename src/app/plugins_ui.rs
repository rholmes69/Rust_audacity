//! VST3 / CLAP plugins in the app: scanning (Effect ▸ Plugins…), loading the plugins used
//! in track racks, keeping their settings saved, and their parameter sliders / windows.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{Receiver, channel},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};

use super::AudacityApp;
use crate::plugins::{self, LoadedPlugin, PluginDesc};

const KEY_LIST: &str = "plugins_scanned";
const KEY_DIRS: &str = "plugins_extra_dirs";

enum ScanMsg {
    File(usize, usize, String),
    Found(Vec<PluginDesc>),
    Failed(String, String),
    Done,
}

struct ScanJob {
    rx: Receiver<ScanMsg>,
    done: usize,
    total: usize,
    current: String,
    found: Vec<PluginDesc>,
    failed: Vec<(String, String)>,
}

#[derive(Default)]
pub struct PluginsState {
    /// Plugins found by the last scan.
    pub list: Vec<PluginDesc>,
    /// Extra folders to scan (besides the standard ones).
    pub dirs: Vec<PathBuf>,
    pub manager_open: bool,
    scan: Option<ScanJob>,
    last_failures: Vec<(String, String)>,
    /// Running plugins, by rack slot id.
    pub loaded: HashMap<u64, LoadedPlugin>,
    /// The state each loaded plugin was last set to / captured as (to notice undo/redo).
    applied: HashMap<u64, Vec<u8>>,
    /// Slots whose plugin couldn't be loaded (not retried every frame).
    pub failed: HashMap<u64, String>,
    filter: String,
}

impl PluginsState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            list: storage
                .and_then(|s| eframe::get_value(s, KEY_LIST))
                .unwrap_or_default(),
            dirs: storage
                .and_then(|s| eframe::get_value(s, KEY_DIRS))
                .unwrap_or_default(),
            ..Default::default()
        }
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_LIST, &self.list);
        eframe::set_value(storage, KEY_DIRS, &self.dirs);
    }
    pub fn effects(&self) -> impl Iterator<Item = &PluginDesc> {
        self.list.iter().filter(|d| !d.is_instrument)
    }
}

/// Run `rust_audacity --scan-plugin <file>` with a time limit.
fn scan_in_child(file: &std::path::Path) -> Result<Vec<PluginDesc>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("--scan-plugin")
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(30) => {
                let _ = child.kill();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('['))
        .unwrap_or("");
    match serde_json::from_str::<Vec<PluginDesc>>(line) {
        Ok(v) if !v.is_empty() => Ok(v),
        Ok(_) => Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .lines()
            .last()
            .unwrap_or("no effects in this file")
            .to_string()),
        Err(_) => Err(if out.status.success() {
            "unreadable scan result".into()
        } else {
            "the plugin crashed while loading".into()
        }),
    }
}

impl AudacityApp {
    pub(super) fn start_plugin_scan(&mut self) {
        if self.plugins.scan.is_some() {
            return;
        }
        let mut dirs = plugins::default_dirs();
        dirs.extend(self.plugins.dirs.iter().cloned());
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("plugin-scan".into())
            .spawn(move || {
                let files = plugins::plugin_files(&dirs);
                for (i, f) in files.iter().enumerate() {
                    let name = f
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    let _ = tx.send(ScanMsg::File(i, files.len(), name.clone()));
                    match scan_in_child(f) {
                        Ok(v) => {
                            let _ = tx.send(ScanMsg::Found(v));
                        }
                        Err(e) => {
                            let _ = tx.send(ScanMsg::Failed(f.display().to_string(), e));
                        }
                    }
                }
                let _ = tx.send(ScanMsg::Done);
            })
            .expect("spawn scan thread");
        self.plugins.scan = Some(ScanJob {
            rx,
            done: 0,
            total: 0,
            current: String::new(),
            found: Vec::new(),
            failed: Vec::new(),
        });
    }

    fn poll_plugin_scan(&mut self) {
        let Some(job) = &mut self.plugins.scan else {
            return;
        };
        self.egui_ctx
            .request_repaint_after(Duration::from_millis(100));
        let mut finished = false;
        while let Ok(m) = job.rx.try_recv() {
            match m {
                ScanMsg::File(i, n, name) => {
                    job.done = i;
                    job.total = n;
                    job.current = name;
                }
                ScanMsg::Found(v) => job.found.extend(v),
                ScanMsg::Failed(f, e) => job.failed.push((f, e)),
                ScanMsg::Done => finished = true,
            }
        }
        if finished {
            let job = self.plugins.scan.take().expect("checked above");
            let mut found = job.found;
            found.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then(a.format.label().cmp(b.format.label()))
            });
            self.status = format!(
                "Found {} plugin effect(s){}",
                found.iter().filter(|d| !d.is_instrument).count(),
                if job.failed.is_empty() {
                    String::new()
                } else {
                    format!(", {} file(s) couldn't be loaded", job.failed.len())
                }
            );
            self.plugins.list = found;
            self.plugins.last_failures = job.failed;
        }
    }

    /// Every frame: load the plugins the project's racks use, unload the ones no longer
    /// used, follow undo/redo of their settings, and give them main-thread time.
    pub(super) fn sync_plugins(&mut self) {
        self.poll_plugin_scan();
        let mut wanted: Vec<(u64, PluginDesc, Vec<u8>)> = Vec::new();
        for t in &self.project.tracks {
            for f in &t.live_fx {
                if let Some(p) = &f.plugin {
                    wanted.push((f.id, p.desc.clone(), p.state.clone()));
                }
            }
        }
        // Unload.
        let keep: Vec<u64> = wanted.iter().map(|w| w.0).collect();
        let gone: Vec<u64> = self
            .plugins
            .loaded
            .keys()
            .copied()
            .filter(|id| !keep.contains(id))
            .collect();
        for id in gone {
            self.engine.plugin_procs.remove(&id);
            self.plugins.applied.remove(&id);
            self.plugins.loaded.remove(&id);
        }
        self.plugins.failed.retain(|id, _| keep.contains(id));
        // Load / follow state changes.
        for (id, desc, state) in wanted {
            if self.plugins.failed.contains_key(&id) {
                continue;
            }
            if !self.plugins.loaded.contains_key(&id) {
                let rate = self.engine.output_rate() as f64;
                match LoadedPlugin::load(&desc, rate) {
                    Ok(mut p) => {
                        if !state.is_empty()
                            && let Err(e) = p.load_state(&state)
                        {
                            self.status = format!("{}: {e:#}", desc.name);
                        }
                        self.engine.plugin_procs.insert(id, p.processor());
                        self.plugins.applied.insert(id, state.clone());
                        self.plugins.loaded.insert(id, p);
                    }
                    Err(e) => {
                        self.plugins.failed.insert(id, format!("{e:#}"));
                        self.status = format!("Couldn't load {}: {e:#}", desc.name);
                    }
                }
                continue;
            }
            if self.plugins.applied.get(&id).is_some_and(|a| *a != state)
                && let Some(p) = self.plugins.loaded.get_mut(&id)
            {
                // Undo/redo brought back other settings.
                if !state.is_empty() {
                    let _ = p.load_state(&state);
                }
                self.plugins.applied.insert(id, state);
            }
        }
        let mut busy = false;
        for p in self.plugins.loaded.values_mut() {
            p.idle();
            busy |= p.editor_open();
        }
        if busy {
            // Plugin windows need regular main-thread time (timers, parameter sync).
            self.egui_ctx
                .request_repaint_after(Duration::from_millis(30));
        }
    }

    /// Store every running plugin's current settings in the project (before saving,
    /// exporting, rendering…). Not an undo step.
    pub(super) fn capture_plugin_states(&mut self) {
        let mut states: Vec<(u64, Vec<u8>)> = Vec::new();
        for (id, p) in self.plugins.loaded.iter_mut() {
            if let Ok(s) = p.save_state() {
                states.push((*id, s));
            }
        }
        for (id, s) in states {
            for t in self.project.tracks.iter_mut() {
                for f in t.live_fx.iter_mut() {
                    if f.id == id
                        && let Some(pl) = &mut f.plugin
                        && pl.state != s
                    {
                        pl.state = s.clone();
                        self.project.dirty = true;
                    }
                }
            }
            self.plugins.applied.insert(id, s);
        }
    }

    /// The output device changed: run plugins at its sample rate.
    pub(super) fn plugins_follow_device_rate(&mut self) {
        self.engine.devices_changed();
        let rate = self.engine.output_rate() as f64;
        for p in self.plugins.loaded.values_mut() {
            if let Err(e) = p.set_rate(rate) {
                self.status = format!("{}: {e:#}", p.desc().name);
            }
        }
    }

    /// Parameter sliders and the "plugin window" button for a rack slot.
    pub(super) fn plugin_slot_ui(&mut self, ui: &mut egui::Ui, slot_id: u64, title: &str) {
        if let Some(err) = self.plugins.failed.get(&slot_id) {
            ui.colored_label(
                Color32::from_rgb(235, 150, 120),
                format!("Not loaded: {err}"),
            );
            if ui.button("Try again").clicked() {
                self.plugins.failed.remove(&slot_id);
            }
            return;
        }
        let Some(p) = self.plugins.loaded.get_mut(&slot_id) else {
            ui.label(RichText::new("Loading…").weak());
            return;
        };
        ui.horizontal(|ui| {
            let d = p.desc().clone();
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}",
                    d.format.label(),
                    d.vendor,
                    d.category
                ))
                .small()
                .weak(),
            );
        });
        if p.has_editor() {
            ui.horizontal(|ui| {
                if p.editor_open() {
                    if ui.button("Close plugin window").clicked() {
                        p.close_editor();
                    }
                } else if ui.button("🗗 Open plugin window").clicked()
                    && let Err(e) = p.open_editor(title)
                {
                    self.status = format!("{e:#}");
                }
            });
        }
        let params = p.params();
        if params.is_empty() {
            ui.label(RichText::new("This plugin has no adjustable settings here.").weak());
            return;
        }
        let mut changed: Vec<(u32, f64)> = Vec::new();
        egui::Grid::new(("plugparams", slot_id))
            .num_columns(2)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                for q in params.iter().filter(|q| !q.is_bypass).take(64) {
                    ui.label(&q.name);
                    let mut v = q.value;
                    let text = p.param_text(q.id, q.value).unwrap_or_default();
                    let unit = if q.unit.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", q.unit)
                    };
                    let r = if q.steps > 0 && q.steps <= 64 && q.max > q.min {
                        let step = (q.max - q.min) / q.steps as f64;
                        ui.add(
                            egui::Slider::new(&mut v, q.min..=q.max)
                                .step_by(step)
                                .show_value(false),
                        )
                    } else {
                        ui.add(egui::Slider::new(&mut v, q.min..=q.max).show_value(false))
                    };
                    let r = r.on_hover_text("Double-click to reset");
                    if r.double_clicked() {
                        v = q.default;
                    }
                    ui.label(RichText::new(format!("{text}{unit}")).monospace().small());
                    ui.end_row();
                    if (v - q.value).abs() > f64::EPSILON {
                        changed.push((q.id, v));
                    }
                }
            });
        if params.len() > 64 {
            ui.label(
                RichText::new(format!(
                    "…and {} more (use the plugin window)",
                    params.len() - 64
                ))
                .small()
                .weak(),
            );
        }
        if !changed.is_empty() {
            for (id, v) in changed {
                p.set_param(id, v);
            }
            self.project.dirty = true;
        }
    }

    pub(super) fn plugin_manager_window(&mut self, ctx: &egui::Context) {
        if !self.plugins.manager_open {
            return;
        }
        let mut open = true;
        let mut rescan = false;
        egui::Window::new("Plugins")
            .default_width(620.0)
            .default_height(480.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "VST3 and CLAP effects can be added to any track's Live Effects (the FX button). \
                         Plugin windows open on Windows; elsewhere use the sliders.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal(|ui| {
                    match &self.plugins.scan {
                        Some(job) => {
                            ui.spinner();
                            ui.label(format!("Scanning {} of {}: {}", (job.done + 1).min(job.total.max(1)), job.total, job.current));
                        }
                        None => {
                            if ui.button("⟳ Scan for plugins").clicked() {
                                rescan = true;
                            }
                        }
                    }
                    if ui.button("Add folder…").clicked()
                        && let Some(d) = rfd::FileDialog::new().pick_folder()
                        && !self.plugins.dirs.contains(&d)
                    {
                        self.plugins.dirs.push(d);
                        rescan = true;
                    }
                });
                ui.collapsing("Folders searched", |ui| {
                    for d in plugins::default_dirs() {
                        ui.label(RichText::new(d.display().to_string()).small().monospace());
                    }
                    let mut remove = None;
                    for (i, d) in self.plugins.dirs.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(d.display().to_string()).small().monospace());
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove {
                        self.plugins.dirs.remove(i);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.text_edit_singleline(&mut self.plugins.filter);
                    ui.label(RichText::new(format!("{} effect(s)", self.plugins.effects().count())).weak());
                });
                let filter = self.plugins.filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    egui::Grid::new("pluglist").striped(true).num_columns(4).spacing([12.0, 4.0]).show(ui, |ui| {
                        ui.strong("Name");
                        ui.strong("Vendor");
                        ui.strong("Type");
                        ui.strong("Format");
                        ui.end_row();
                        for d in self.plugins.list.iter().filter(|d| {
                            filter.is_empty()
                                || d.name.to_lowercase().contains(&filter)
                                || d.vendor.to_lowercase().contains(&filter)
                                || d.category.to_lowercase().contains(&filter)
                        }) {
                            ui.label(&d.name).on_hover_text(d.path.display().to_string());
                            ui.label(&d.vendor);
                            ui.label(if d.is_instrument { "Instrument (not usable)".to_string() } else { d.category.clone() });
                            ui.label(d.format.label());
                            ui.end_row();
                        }
                    });
                });
                if !self.plugins.last_failures.is_empty() {
                    ui.collapsing(format!("{} file(s) couldn't be loaded", self.plugins.last_failures.len()), |ui| {
                        for (f, e) in &self.plugins.last_failures {
                            ui.label(RichText::new(format!("{f}: {e}")).small());
                        }
                    });
                }
            });
        if rescan {
            self.start_plugin_scan();
        }
        if !open {
            self.plugins.manager_open = false;
        }
    }
}
