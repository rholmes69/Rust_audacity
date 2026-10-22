//! The main application window: menus, transport, track panels and waveform editor.

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender, channel},
};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Key, KeyboardShortcut, Modifiers, Rect, Sense,
    Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::{
    effects::{self, Category, Effect, Generator, NoiseKind, Waveform, db_to_lin, lin_to_db},
    engine::{Engine, Shared},
    fileio::{self, DecodedAudio, ExportFormat},
    project::{Project, Selection},
    projectfile,
    spectrogram::{self, SpectrogramCache},
    track::{Track, TrackView, adapt_channels},
    waveform::{self, SummaryCache},
};

const PANEL_W: f32 = 168.0;
const RULER_H: f32 = 26.0;
const MIN_TRACK_H: f32 = 120.0;

/// Horizontal view: which sample is at the left edge and how many samples one pixel covers.
#[derive(Clone, Copy)]
struct View {
    left: f64,
    spp: f64,
}

impl View {
    fn x_to_sample(&self, area: Rect, x: f32) -> f64 {
        self.left + (x - area.left()) as f64 * self.spp
    }
    fn sample_to_x(&self, area: Rect, s: f64) -> f32 {
        area.left() + ((s - self.left) / self.spp) as f32
    }
}

mod analyze_ui;
mod bridge_ui;
mod file_ui;
mod fx_ui;
mod jobs_ui;
mod stems_ui;
mod transcript_ui;

use file_ui::Pending;

/// Mouse tool for the track area.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    /// Click and drag to select time ranges.
    Select,
    /// Drag tracks or the selected audio along the timeline.
    TimeShift,
}

/// An in-progress Time Shift drag.
struct ShiftDrag {
    /// Track the drag started on.
    track: usize,
    start_x: f32,
    /// Moving the selected audio (true) or the whole track (false).
    clip: bool,
    /// Current offset in samples (already clamped).
    delta: i64,
    /// Track under the pointer (for moving a clip to another track).
    dest: Option<usize>,
}

#[derive(Clone)]
enum Dialog {
    /// Parameter dialog for any effect.
    Effect(Effect),
    Tone {
        wave: Waveform,
        freq: f32,
        amp: f32,
        secs: f32,
    },
    Noise {
        kind: NoiseKind,
        amp: f32,
        secs: f32,
    },
    InsertSilence {
        secs: f32,
    },
    Export {
        fmt: ExportFormat,
        selection_only: bool,
    },
    ConfirmUnsaved(Pending),
    Shortcuts,
    About,
    Message(String),
}

struct DragState {
    anchor: usize,
    start_y: f32,
    additive: bool,
}

struct Recording {
    track_id: u64,
    start: usize,
    resampler: StreamResampler,
}

/// Streaming linear-interpolation resampler for live recording.
struct StreamResampler {
    ratio: f64,
    t: f64,
    pending: Vec<Vec<f32>>,
}

impl StreamResampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        Self {
            ratio: from as f64 / to as f64,
            t: 0.0,
            pending: vec![Vec::new(); channels],
        }
    }
    fn process(&mut self, input: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        if (self.ratio - 1.0).abs() < 1e-12 {
            return input;
        }
        for (p, i) in self.pending.iter_mut().zip(input) {
            p.extend(i);
        }
        let n = self.pending[0].len();
        let mut out = vec![Vec::new(); self.pending.len()];
        let mut t = self.t;
        while t + 1.0 < n as f64 {
            let i0 = t as usize;
            let fr = (t - i0 as f64) as f32;
            for (o, p) in out.iter_mut().zip(&self.pending) {
                o.push(p[i0] + (p[i0 + 1] - p[i0]) * fr);
            }
            t += self.ratio;
        }
        let consumed = (t as usize).min(n);
        for p in &mut self.pending {
            p.drain(..consumed);
        }
        self.t = t - consumed as f64;
        out
    }
}

pub struct AudacityApp {
    project: Project,
    engine: Engine,
    view: View,
    vzoom: f32,
    cache: SummaryCache,
    drag: Option<DragState>,
    dialog: Option<Dialog>,
    status: String,
    recording: Option<Recording>,
    meters: [f32; 4],
    looping: bool,
    last_track_area: Rect,
    track_rects: Vec<(usize, Rect)>,
    import_tx: Sender<(PathBuf, anyhow::Result<DecodedAudio>)>,
    import_rx: Receiver<(PathBuf, anyhow::Result<DecodedAudio>)>,
    importing: usize,
    title: String,
    last_title: String,
    spec_cache: SpectrogramCache,
    /// Amplify dialog: allow clipping.
    allow_clip: bool,
    /// True while an effect preview is playing.
    previewing: bool,
    preview_error: Option<String>,
    chains: fx_ui::ChainsState,
    analysis: Option<analyze_ui::AnalysisWindow>,
    stems: stems_ui::StemsState,
    /// Where the project was last saved / opened from.
    project_path: Option<PathBuf>,
    /// Set once the user has agreed to close (so the close request isn't intercepted again).
    close_confirmed: bool,
    egui_ctx: egui::Context,
    /// Effect or export running in the background.
    job: Option<jobs_ui::RunningJob>,
    /// Last export format used (remembered between sessions).
    export_fmt: ExportFormat,
    tool: Tool,
    shift: Option<ShiftDrag>,
    /// Track rectangles from the previous frame (for hit-testing during drags).
    prev_track_rects: Vec<(usize, Rect)>,
    /// Zoom to fit once the track area's size is known (after opening a project).
    fit_pending: bool,
    transcript: transcript_ui::TranscriptState,
    bridge: bridge_ui::BridgeState,
}

impl AudacityApp {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (import_tx, import_rx) = channel();
        let mut app = Self {
            project: Project::default(),
            engine: Engine::default(),
            view: View {
                left: 0.0,
                spp: 400.0,
            },
            vzoom: 1.0,
            cache: SummaryCache::default(),
            drag: None,
            dialog: None,
            status: "Welcome! Drop audio files here, use File ▸ Import, or press R to record."
                .into(),
            recording: None,
            meters: [0.0; 4],
            looping: false,
            last_track_area: Rect::NOTHING,
            track_rects: Vec::new(),
            import_tx,
            import_rx,
            importing: 0,
            title: "Untitled".into(),
            last_title: String::new(),
            spec_cache: SpectrogramCache::default(),
            allow_clip: false,
            previewing: false,
            preview_error: None,
            chains: fx_ui::ChainsState::load(cc.storage),
            analysis: None,
            stems: stems_ui::StemsState::load(cc.storage),
            project_path: None,
            close_confirmed: false,
            egui_ctx: cc.egui_ctx.clone(),
            job: None,
            export_fmt: Self::load_export_format(cc.storage),
            tool: Tool::Select,
            shift: None,
            prev_track_rects: Vec::new(),
            fit_pending: false,
            transcript: transcript_ui::TranscriptState::load(cc.storage),
            bridge: bridge_ui::BridgeState::load(cc.storage),
        };
        for f in files {
            app.import_path(f);
        }
        app
    }

    // ------------------------------------------------------------------ helpers

    fn busy(&self) -> bool {
        self.recording.is_some() || self.job.is_some() || self.transcript.running()
    }

    fn rate(&self) -> f64 {
        self.project.rate as f64
    }

    fn fmt_time(&self, samples: f64) -> String {
        let secs = samples / self.rate();
        let h = (secs / 3600.0).floor();
        let m = ((secs - h * 3600.0) / 60.0).floor();
        let s = secs - h * 3600.0 - m * 60.0;
        format!("{:02}:{:02}:{:06.3}", h as u64, m as u64, s.max(0.0))
    }

    fn report<T>(&mut self, r: anyhow::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!("{e:#}")));
                None
            }
        }
    }

    fn import_path(&mut self, path: PathBuf) {
        if projectfile::is_project_path(&path) {
            self.confirm_then(Pending::Open(Some(path)));
            return;
        }
        let tx = self.import_tx.clone();
        self.importing += 1;
        self.status = format!("Importing {}…", path.display());
        std::thread::spawn(move || {
            let r = fileio::import(&path);
            let _ = tx.send((path, r));
        });
    }

    fn poll_imports(&mut self, track_area_w: f32) {
        while let Ok((path, res)) = self.import_rx.try_recv() {
            self.importing = self.importing.saturating_sub(1);
            match res {
                Ok(audio) => {
                    let was_empty = self.project.tracks.is_empty();
                    self.project.push_undo("Import");
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or("Imported".into());
                    if was_empty {
                        self.title = name.clone();
                    }
                    for t in &mut self.project.tracks {
                        t.selected = false;
                    }
                    self.project.add_audio(name, audio.channels, audio.rate);
                    if was_empty {
                        self.zoom_fit(track_area_w);
                    }
                    self.status = format!("Imported {}", path.display());
                }
                Err(e) => {
                    self.dialog = Some(Dialog::Message(format!(
                        "Could not import {}:\n{e:#}",
                        path.display()
                    )))
                }
            }
        }
    }

    // --------------------------------------------------------------- transport

    fn play(&mut self) {
        if self.busy() {
            return;
        }
        let sel = self.project.selection;
        let (start, end) = if sel.is_empty() {
            (sel.start, None)
        } else {
            (sel.start, Some(sel.end))
        };
        let start = if start >= self.project.len() {
            0
        } else {
            start
        };
        let r = self
            .engine
            .play(&self.project, start, end, self.looping && end.is_some());
        self.report(r);
    }

    fn play_from(&mut self, pos: usize) {
        let r = self.engine.play(&self.project, pos, None, false);
        self.report(r);
    }

    fn stop(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        }
        self.engine.stop_playback();
    }

    fn toggle_play(&mut self) {
        if self.engine.is_playing() || self.recording.is_some() {
            self.stop();
        } else {
            self.play();
        }
    }

    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }
        self.engine.stop_playback();
        let started = self.engine.start_recording();
        if let Some(()) = self.report(started) {
            let start = self.project.selection.start;
            self.project.push_undo("Record");
            if self.project.tracks.is_empty() {
                self.project.rate = self.engine.rec_rate;
            }
            let chans = self.engine.rec_channels.min(2);
            let n = self
                .project
                .tracks
                .iter()
                .filter(|t| t.name.starts_with("Audio"))
                .count()
                + 1;
            let id = self.project.alloc_id();
            let mut track = Track::empty(id, format!("Audio {n}"), chans);
            track.pad_to(start);
            for t in &mut self.project.tracks {
                t.selected = false;
            }
            // Overdub: play the existing tracks while recording.
            if self.project.len() > start {
                let _ = self.engine.play(&self.project, start, None, false);
            }
            self.project.tracks.push(track);
            self.recording = Some(Recording {
                track_id: id,
                start,
                resampler: StreamResampler::new(self.engine.rec_rate, self.project.rate, chans),
            });
            self.status = "Recording…".into();
        }
    }

    fn pump_recording(&mut self) {
        let Some(rec) = self.recording.as_mut() else {
            return;
        };
        let raw = self.engine.take_recorded();
        if raw.is_empty() || raw[0].is_empty() {
            return;
        }
        let data = rec.resampler.process(raw);
        if let Some(t) = self
            .project
            .tracks
            .iter_mut()
            .find(|t| t.id == rec.track_id)
        {
            let at = t.len();
            t.insert(at, &data);
        }
    }

    fn stop_recording(&mut self) {
        self.pump_recording();
        self.engine.stop_recording();
        self.engine.stop_playback();
        if let Some(rec) = self.recording.take() {
            if let Some(t) = self
                .project
                .tracks
                .iter_mut()
                .find(|t| t.id == rec.track_id)
            {
                t.selected = true;
                self.project.selection = Selection::new(rec.start, t.len());
            }
            self.project.touch();
            self.status = format!(
                "Recorded {}",
                self.fmt_time(self.project.selection.len() as f64)
            );
        }
    }

    // -------------------------------------------------------------------- view

    fn zoom_fit(&mut self, width: f32) {
        let len = self.project.len().max(1) as f64;
        self.view.left = 0.0;
        self.view.spp = (len / (width.max(100.0) as f64 - 10.0)).max(0.01);
    }

    fn zoom_by(&mut self, factor: f64, anchor_sample: f64, anchor_px: f32) {
        self.view.spp = (self.view.spp / factor).clamp(0.01, 1.0e6);
        self.view.left = (anchor_sample - anchor_px as f64 * self.view.spp).max(0.0);
    }

    fn zoom_center(&mut self, factor: f64) {
        let w = self.last_track_area.width().max(100.0);
        let center = self.view.left + w as f64 * 0.5 * self.view.spp;
        self.zoom_by(factor, center, w * 0.5);
    }

    fn zoom_selection(&mut self) {
        let sel = self.project.selection;
        if sel.is_empty() {
            return;
        }
        let w = self.last_track_area.width().max(100.0) as f64;
        self.view.spp = (sel.len() as f64 / (w * 0.9)).max(0.01);
        self.view.left = (sel.start as f64 - w * 0.05 * self.view.spp).max(0.0);
    }

    // ----------------------------------------------------------------- actions

    fn do_edit(&mut self, f: impl FnOnce(&mut Project) -> bool, what: &str) {
        if self.busy() {
            return;
        }
        self.engine.stop_playback();
        if !f(&mut self.project) {
            self.status = format!("{what}: select some audio first");
        } else {
            self.status = what.to_string();
        }
    }

    /// Apply an effect in the background (one undo step).
    fn apply_effect(&mut self, fx: &Effect) {
        self.start_effect_job(fx.name().to_string(), vec![*fx]);
    }

    /// Menu entry for an effect: open its dialog, or apply directly if it has no settings.
    fn open_effect(&mut self, fx: Effect) {
        if !fx.has_params() {
            self.apply_effect(&fx);
            return;
        }
        let fx = match fx {
            Effect::Amplify { .. } => {
                let peak = effects::selection_peak(&self.project);
                let db = if peak > 0.0 { -lin_to_db(peak) } else { 0.0 };
                self.allow_clip = false;
                Effect::Amplify {
                    db: (db * 10.0).floor() / 10.0,
                }
            }
            other => self.chains.last_used(&other).unwrap_or(other),
        };
        self.dialog = Some(Dialog::Effect(fx));
    }

    fn new_project(&mut self) {
        self.stop();
        self.reset_to_new_project();
        self.title = "Untitled".into();
        self.view = View {
            left: 0.0,
            spp: 400.0,
        };
        self.status = "New project".into();
    }

    fn stereo_to_mono(&mut self) {
        if self.busy() {
            return;
        }
        let targets: Vec<usize> = self
            .project
            .target_tracks()
            .into_iter()
            .filter(|&i| self.project.tracks[i].num_channels() > 1)
            .collect();
        if targets.is_empty() {
            self.status = "No stereo tracks selected".into();
            return;
        }
        self.project.push_undo("Stereo to Mono");
        for i in targets {
            let t = &mut self.project.tracks[i];
            let data: Vec<Vec<f32>> = t.channels.iter().map(|c| c.to_vec()).collect();
            t.channels = adapt_channels(&data, 1)
                .into_iter()
                .map(std::sync::Arc::new)
                .collect();
        }
    }

    fn add_empty_track(&mut self, chans: usize) {
        self.project.push_undo("New Track");
        let id = self.project.alloc_id();
        let n = self.project.tracks.len() + 1;
        self.project
            .tracks
            .push(Track::empty(id, format!("Track {n}"), chans));
    }

    fn remove_selected_tracks(&mut self) {
        if self.busy() {
            return;
        }
        if !self.project.tracks.iter().any(|t| t.selected) {
            return;
        }
        self.engine.stop_playback();
        self.project.push_undo("Remove Tracks");
        self.project.tracks.retain(|t| !t.selected);
        self.project.drop_orphan_transcript();
    }

    // --------------------------------------------------------------- shortcuts

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.dialog.is_some() {
            return;
        }
        let sc = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)))
        };
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;

        if (sc(cmd_shift, Key::Z) || sc(cmd, Key::Y)) && !self.busy() && self.project.redo() {
            self.engine.stop_playback();
        }
        if sc(cmd, Key::Z) && !self.busy() && self.project.undo() {
            self.engine.stop_playback();
        }
        if sc(cmd_shift, Key::I) {
            self.import_dialog();
        }
        if sc(cmd, Key::O) {
            self.confirm_then(Pending::Open(None));
        }
        if sc(cmd_shift, Key::S) {
            self.save_project(true);
        }
        if sc(cmd, Key::S) {
            self.save_project(false);
        }
        if sc(cmd_shift, Key::E) {
            self.open_export_dialog(false);
        }
        if sc(cmd, Key::N) {
            self.confirm_then(Pending::New);
        }
        if sc(cmd_shift, Key::T) {
            self.transcript.show_panel ^= true;
        }
        if sc(Modifiers::NONE, Key::F1) {
            self.tool = Tool::Select;
        }
        if sc(Modifiers::NONE, Key::F5) {
            self.tool = Tool::TimeShift;
        }
        if sc(cmd, Key::X) {
            self.do_edit(Project::cut, "Cut");
        }
        if sc(cmd, Key::C) {
            self.do_edit(Project::copy, "Copy");
        }
        if sc(cmd, Key::V) {
            self.do_edit(Project::paste, "Paste");
        }
        if sc(cmd, Key::L) {
            self.do_edit(Project::silence_selection, "Silence");
        }
        if sc(cmd, Key::T) {
            self.do_edit(Project::trim, "Trim");
        }
        if sc(cmd, Key::D) {
            self.do_edit(Project::duplicate, "Duplicate");
        }
        if sc(cmd, Key::A) {
            self.project.select_all();
        }
        if sc(cmd, Key::Num1) {
            self.zoom_center(2.0);
        }
        if sc(cmd, Key::Num3) {
            self.zoom_center(0.5);
        }
        if sc(cmd, Key::E) {
            self.zoom_selection();
        }
        if sc(cmd, Key::F) {
            self.zoom_fit(self.last_track_area.width());
        }
        if sc(Modifiers::NONE, Key::Delete) || sc(Modifiers::NONE, Key::Backspace) {
            self.do_edit(Project::delete, "Delete");
        }
        if sc(Modifiers::NONE, Key::Space) {
            self.toggle_play();
        }
        if sc(Modifiers::NONE, Key::P) && self.engine.is_playing() {
            self.engine.toggle_pause();
        }
        if sc(Modifiers::NONE, Key::R) {
            if self.recording.is_some() {
                self.stop_recording();
            } else {
                self.start_recording();
            }
        }
        if sc(Modifiers::NONE, Key::L) {
            self.looping = !self.looping;
        }
        let shift = ctx.input(|i| i.modifiers.shift);
        let len = self.project.len();
        if sc(Modifiers::NONE, Key::Home)
            || sc(Modifiers::SHIFT, Key::Home)
            || sc(Modifiers::NONE, Key::J)
        {
            let end = self.project.selection.end;
            self.project.selection = if shift {
                Selection::new(0, end)
            } else {
                Selection::cursor(0)
            };
            self.view.left = 0.0;
        }
        if sc(Modifiers::NONE, Key::End)
            || sc(Modifiers::SHIFT, Key::End)
            || sc(Modifiers::NONE, Key::K)
        {
            let st = self.project.selection.start;
            self.project.selection = if shift {
                Selection::new(st, len)
            } else {
                Selection::cursor(len)
            };
        }
        let step = (self.view.spp * 10.0).max(1.0) as usize;
        if sc(Modifiers::NONE, Key::ArrowLeft) {
            let p = self.project.selection.start.saturating_sub(step);
            self.project.selection = Selection::cursor(p);
        }
        if sc(Modifiers::NONE, Key::ArrowRight) {
            let p = (self.project.selection.end + step).min(len);
            self.project.selection = Selection::cursor(p);
        }
    }

    fn import_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .add_filter(
                "Audio files",
                &[
                    "wav", "mp3", "flac", "ogg", "oga", "m4a", "aac", "aif", "aiff", "caf", "mka",
                    "webm",
                ],
            )
            .add_filter("All files", &["*"])
            .pick_files()
        {
            for f in files {
                self.import_path(f);
            }
        }
    }

    // -------------------------------------------------------------------- menus

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                let idle = !self.busy();
                let item = |ui: &mut Ui, label: &str, key: &str, enabled: bool| {
                    ui.add_enabled(enabled, egui::Button::new(label).shortcut_text(key))
                        .clicked()
                };
                if item(ui, "New", "Ctrl+N", idle) {
                    ui.close();
                    self.confirm_then(Pending::New);
                }
                if item(ui, "Open Project…", "Ctrl+O", idle) {
                    ui.close();
                    self.confirm_then(Pending::Open(None));
                }
                let has_audio = !self.project.tracks.is_empty();
                if item(ui, "Save Project", "Ctrl+S", idle && has_audio) {
                    ui.close();
                    self.save_project(false);
                }
                if item(ui, "Save Project As…", "Ctrl+Shift+S", idle && has_audio) {
                    ui.close();
                    self.save_project(true);
                }
                ui.separator();
                if item(ui, "Import Audio…", "Ctrl+Shift+I", true) {
                    ui.close();
                    self.import_dialog();
                }
                ui.separator();
                if item(ui, "Export Audio…", "Ctrl+Shift+E", idle && has_audio) {
                    ui.close();
                    self.open_export_dialog(false);
                }
                if item(
                    ui,
                    "Export Selection…",
                    "",
                    idle && !self.project.selection.is_empty(),
                ) {
                    ui.close();
                    self.open_export_dialog(true);
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.close();
                    self.confirm_then(Pending::Exit);
                }
            });

            ui.menu_button("Edit", |ui| {
                let undo = match self.project.undo_label() {
                    Some(l) => format!("Undo {l}"),
                    None => "Undo".into(),
                };
                if ui
                    .add_enabled(
                        self.project.can_undo() && !self.busy(),
                        egui::Button::new(undo).shortcut_text("Ctrl+Z"),
                    )
                    .clicked()
                {
                    self.engine.stop_playback();
                    self.project.undo();
                    ui.close();
                }
                let redo = match self.project.redo_label() {
                    Some(l) => format!("Redo {l}"),
                    None => "Redo".into(),
                };
                if ui
                    .add_enabled(
                        self.project.can_redo() && !self.busy(),
                        egui::Button::new(redo).shortcut_text("Ctrl+Y"),
                    )
                    .clicked()
                {
                    self.engine.stop_playback();
                    self.project.redo();
                    ui.close();
                }
                ui.separator();
                type Op = fn(&mut Project) -> bool;
                let items: [(&str, &str, Op); 8] = [
                    ("Cut", "Ctrl+X", Project::cut),
                    ("Copy", "Ctrl+C", Project::copy),
                    ("Paste", "Ctrl+V", Project::paste),
                    ("Delete", "Del", Project::delete),
                    ("Silence Audio", "Ctrl+L", Project::silence_selection),
                    ("Trim Audio", "Ctrl+T", Project::trim),
                    ("Duplicate", "Ctrl+D", Project::duplicate),
                    ("Split New", "", Project::split_new),
                ];
                for (label, key, op) in items {
                    if ui
                        .add(egui::Button::new(label).shortcut_text(key))
                        .clicked()
                    {
                        self.do_edit(op, label);
                        ui.close();
                    }
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Select All").shortcut_text("Ctrl+A"))
                    .clicked()
                {
                    self.project.select_all();
                    ui.close();
                }
                if ui.button("Select None").clicked() {
                    self.project.selection = Selection::cursor(self.project.selection.start);
                    ui.close();
                }
            });

            ui.menu_button("View", |ui| {
                if ui
                    .add(egui::Button::new("Zoom In").shortcut_text("Ctrl+1"))
                    .clicked()
                {
                    self.zoom_center(2.0);
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Zoom Out").shortcut_text("Ctrl+3"))
                    .clicked()
                {
                    self.zoom_center(0.5);
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Zoom to Selection").shortcut_text("Ctrl+E"))
                    .clicked()
                {
                    self.zoom_selection();
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Fit to Width").shortcut_text("Ctrl+F"))
                    .clicked()
                {
                    self.zoom_fit(self.last_track_area.width());
                    ui.close();
                }
                ui.separator();
                if ui
                    .add(
                        egui::Button::selectable(self.transcript.show_panel, "Transcript Panel")
                            .shortcut_text("Ctrl+Shift+T"),
                    )
                    .clicked()
                {
                    self.transcript.show_panel ^= true;
                    ui.close();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Vertical zoom");
                    ui.add(egui::Slider::new(&mut self.vzoom, 0.5..=16.0).logarithmic(true));
                });
            });

            ui.menu_button("Transport", |ui| {
                if ui
                    .add(egui::Button::new("Play / Stop").shortcut_text("Space"))
                    .clicked()
                {
                    self.toggle_play();
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Pause").shortcut_text("P"))
                    .clicked()
                {
                    self.engine.toggle_pause();
                    ui.close();
                }
                if ui
                    .add(egui::Button::new("Record").shortcut_text("R"))
                    .clicked()
                {
                    self.start_recording();
                    ui.close();
                }
                ui.checkbox(&mut self.looping, "Loop selection (L)");
            });

            ui.menu_button("Tracks", |ui| {
                if ui.button("Add New ▸ Mono Track").clicked() {
                    self.add_empty_track(1);
                    ui.close();
                }
                if ui.button("Add New ▸ Stereo Track").clicked() {
                    self.add_empty_track(2);
                    ui.close();
                }
                ui.separator();
                if ui.button("Mix and Render").clicked() {
                    self.do_edit(Project::mix_and_render, "Mix and Render");
                    ui.close();
                }
                if ui.button("Stereo to Mono").clicked() {
                    self.stereo_to_mono();
                    ui.close();
                }
                if ui.button("Remove Selected Tracks").clicked() {
                    self.remove_selected_tracks();
                    ui.close();
                }
            });

            ui.menu_button("Generate", |ui| {
                if ui.button("Tone…").clicked() {
                    self.dialog = Some(Dialog::Tone {
                        wave: Waveform::Sine,
                        freq: 440.0,
                        amp: 0.8,
                        secs: 5.0,
                    });
                    ui.close();
                }
                if ui.button("Noise…").clicked() {
                    self.dialog = Some(Dialog::Noise {
                        kind: NoiseKind::White,
                        amp: 0.5,
                        secs: 5.0,
                    });
                    ui.close();
                }
                if ui.button("Silence…").clicked() {
                    self.dialog = Some(Dialog::InsertSilence { secs: 1.0 });
                    ui.close();
                }
            });

            ui.menu_button("Effect", |ui| {
                for cat in Category::ALL {
                    ui.menu_button(cat.label(), |ui| {
                        for fx in Effect::catalog()
                            .into_iter()
                            .filter(|f| f.category() == cat)
                        {
                            let label = if fx.has_params() {
                                format!("{}…", fx.name())
                            } else {
                                fx.name().to_string()
                            };
                            if ui.button(label).clicked() {
                                ui.close();
                                self.open_effect(fx);
                            }
                        }
                    });
                }
                ui.separator();
                if ui.button("Effect Chains…").clicked() {
                    self.chains.open = true;
                    ui.close();
                }
            });

            ui.menu_button("Analyze", |ui| {
                if ui.button("Plot Spectrum & Loudness Stats…").clicked() {
                    ui.close();
                    self.open_analysis();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        !self.busy(),
                        egui::Button::new("Transcribe Speech (Whisper)…"),
                    )
                    .clicked()
                {
                    ui.close();
                    self.open_transcribe_dialog();
                }
            });

            ui.menu_button("Stems", |ui| {
                if ui.button("Separate Stems…").clicked() {
                    self.stems.dialog_open = true;
                    ui.close();
                }
                if ui.button("Quick Vocal Remover (centre cancel)…").clicked() {
                    ui.close();
                    self.open_effect(Effect::VocalRemover {
                        keep_bass_hz: 150.0,
                    });
                }
            });

            ui.menu_button("Agents", |ui| {
                if ui
                    .checkbox(
                        &mut self.bridge.enabled,
                        "Allow AI agents to control this window",
                    )
                    .changed()
                {
                    self.bridge.error = None;
                }
                ui.label(
                    egui::RichText::new(
                        "Agents connected through the Rust Audacity MCP server\n\
                         (Claude, Cursor, Ollama via rust_audacity_chat…) can use the\n\
                         app_* tools to edit what is open here. Only programs on\n\
                         this computer, running as you, can connect.",
                    )
                    .small()
                    .weak(),
                );
                if let Some(e) = &self.bridge.error {
                    ui.colored_label(Color32::from_rgb(230, 120, 90), e);
                }
                if let Some(t) = self.bridge.last_request {
                    ui.label(
                        egui::RichText::new(format!(
                            "Last agent command: {} ({:.0} s ago)",
                            self.bridge.last_command,
                            t.elapsed().as_secs_f32()
                        ))
                        .small(),
                    );
                }
            });

            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard Shortcuts").clicked() {
                    self.dialog = Some(Dialog::Shortcuts);
                    ui.close();
                }
                if ui.button("About").clicked() {
                    self.dialog = Some(Dialog::About);
                    ui.close();
                }
            });
        });
    }

    // ----------------------------------------------------------------- toolbar

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().button_padding = vec2(10.0, 6.0);
            let big = |s: &str| egui::RichText::new(s).size(18.0);
            let playing = self.engine.is_playing();
            let recording = self.recording.is_some();

            if ui
                .add(egui::Button::new(big("⏸")).selected(self.engine.is_paused()))
                .on_hover_text("Pause (P)")
                .clicked()
            {
                self.engine.toggle_pause();
            }
            if ui
                .add(
                    egui::Button::new(big("▶").color(Color32::from_rgb(90, 200, 90)))
                        .selected(playing && !recording),
                )
                .on_hover_text("Play (Space)")
                .clicked()
                && !playing
            {
                self.play();
            }
            if ui
                .add(egui::Button::new(big("⏹")))
                .on_hover_text("Stop (Space)")
                .clicked()
            {
                self.stop();
            }
            if ui
                .add(egui::Button::new(big("⏮")))
                .on_hover_text("Skip to Start (Home)")
                .clicked()
            {
                self.project.selection = Selection::cursor(0);
                self.view.left = 0.0;
            }
            if ui
                .add(egui::Button::new(big("⏭")))
                .on_hover_text("Skip to End (End)")
                .clicked()
            {
                self.project.selection = Selection::cursor(self.project.len());
            }
            if ui
                .add(
                    egui::Button::new(big("⏺").color(Color32::from_rgb(230, 60, 60)))
                        .selected(recording),
                )
                .on_hover_text("Record (R)")
                .clicked()
            {
                if recording {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }
            if ui
                .add(egui::Button::new(big("🔁")).selected(self.looping))
                .on_hover_text("Loop selection (L)")
                .clicked()
            {
                self.looping = !self.looping;
            }

            ui.separator();
            if ui
                .add(egui::Button::new(big("I")).selected(self.tool == Tool::Select))
                .on_hover_text("Selection tool (F1): drag to select audio")
                .clicked()
            {
                self.tool = Tool::Select;
            }
            if ui
                .add(egui::Button::new(big("↔")).selected(self.tool == Tool::TimeShift))
                .on_hover_text(
                    "Time Shift tool (F5): drag a track to move it in time,\n\
                     or drag inside the selection to move that audio (also onto another track)",
                )
                .clicked()
            {
                self.tool = Tool::TimeShift;
            }
            ui.separator();
            let pos = if playing || recording {
                if recording {
                    self.project
                        .tracks
                        .iter()
                        .find(|t| Some(t.id) == self.recording.as_ref().map(|r| r.track_id))
                        .map_or(0.0, |t| t.len() as f64)
                } else {
                    self.engine.position()
                }
            } else {
                self.project.selection.start as f64
            };
            ui.label(
                egui::RichText::new(self.fmt_time(pos))
                    .monospace()
                    .size(22.0)
                    .color(Color32::from_rgb(120, 200, 255)),
            );
            ui.separator();
            self.meters_ui(ui);
            ui.separator();
            if ui.button("🔍+").on_hover_text("Zoom In (Ctrl+1)").clicked() {
                self.zoom_center(2.0);
            }
            if ui
                .button("🔍−")
                .on_hover_text("Zoom Out (Ctrl+3)")
                .clicked()
            {
                self.zoom_center(0.5);
            }
            if ui
                .button("⬌")
                .on_hover_text("Fit project (Ctrl+F)")
                .clicked()
            {
                self.zoom_fit(self.last_track_area.width());
            }
            if ui
                .button("⬍sel")
                .on_hover_text("Zoom to selection (Ctrl+E)")
                .clicked()
            {
                self.zoom_selection();
            }
        });
    }

    fn meters_ui(&mut self, ui: &mut Ui) {
        let play = Shared::take_peak(&self.engine.shared.play_peak);
        let rec = Shared::take_peak(&self.engine.shared.rec_peak);
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let decay = (-dt * 6.0).exp();
        let vals = [play[0], play[1], rec[0], rec[1]];
        for (m, v) in self.meters.iter_mut().zip(vals) {
            *m = (*m * decay).max(v);
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (label, idx, color) in [
                ("Play", 0, Color32::from_rgb(70, 190, 90)),
                ("Rec ", 2, Color32::from_rgb(220, 80, 70)),
            ] {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(label).small().monospace());
                    let (rect, _) = ui.allocate_exact_size(vec2(180.0, 14.0), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(30));
                    for ch in 0..2 {
                        let v = self.meters[idx + ch];
                        let db = lin_to_db(v).max(-60.0);
                        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                        let r = Rect::from_min_size(
                            pos2(rect.left(), rect.top() + ch as f32 * 7.0),
                            vec2(rect.width() * frac, 6.0),
                        );
                        let c = if v >= 1.0 {
                            Color32::RED
                        } else if db > -6.0 {
                            Color32::from_rgb(230, 200, 60)
                        } else {
                            color
                        };
                        p.rect_filled(r, CornerRadius::ZERO, c);
                    }
                    for db in [-48.0, -36.0, -24.0, -12.0, -6.0] {
                        let x = rect.left() + rect.width() * ((db + 60.0) / 60.0);
                        p.line_segment(
                            [pos2(x, rect.top()), pos2(x, rect.bottom())],
                            Stroke::new(1.0, Color32::from_black_alpha(120)),
                        );
                    }
                });
            }
        });
        if self.meters.iter().any(|&m| m > 0.001) {
            ui.ctx().request_repaint();
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Project Rate: {} Hz", self.project.rate));
            ui.separator();
            let sel = self.project.selection;
            ui.label("Selection:");
            ui.monospace(self.fmt_time(sel.start as f64));
            ui.label("to");
            ui.monospace(self.fmt_time(sel.end as f64));
            ui.label("length");
            ui.monospace(self.fmt_time(sel.len() as f64));
            ui.separator();
            if self.importing > 0 {
                ui.spinner();
            }
            if self.bridge.active_recently() {
                ui.label(
                    egui::RichText::new(format!("🤖 Agent: {}", self.bridge.last_command))
                        .color(Color32::from_rgb(140, 200, 255)),
                );
                ui.separator();
            }
            ui.label(&self.status);
        });
    }

    // ------------------------------------------------------------------ tracks

    fn tracks_area(&mut self, ui: &mut Ui) {
        let full = ui.available_rect_before_wrap();
        let wave_left = full.left() + PANEL_W;
        let wave_w = (full.width() - PANEL_W).max(10.0);

        // Wheel: ctrl = zoom around pointer, shift / horizontal = scroll.
        let hover = ui.rect_contains_pointer(full);
        if hover {
            let (zoom, scroll, ptr) =
                ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
            if zoom != 1.0
                && let Some(p) = ptr
            {
                let px = (p.x - wave_left).max(0.0);
                let anchor = self.view.left + px as f64 * self.view.spp;
                self.zoom_by(zoom as f64, anchor, px);
            }
            if scroll.x != 0.0 {
                self.view.left = (self.view.left - scroll.x as f64 * self.view.spp).max(0.0);
            }
        }

        // Ruler.
        let ruler = Rect::from_min_size(pos2(wave_left, full.top()), vec2(wave_w, RULER_H));
        self.ruler(ui, ruler);

        // Horizontal scrollbar at bottom.
        let sb_h = 14.0;
        let sb = Rect::from_min_size(pos2(wave_left, full.bottom() - sb_h), vec2(wave_w, sb_h));
        self.hscrollbar(ui, sb);

        let tracks_rect = Rect::from_min_max(
            pos2(full.left(), ruler.bottom()),
            pos2(full.right(), sb.top()),
        );
        self.last_track_area =
            Rect::from_min_max(pos2(wave_left, tracks_rect.top()), tracks_rect.max);
        if self.fit_pending {
            self.fit_pending = false;
            self.zoom_fit(self.last_track_area.width());
        }

        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(tracks_rect));
        if self.project.tracks.is_empty() {
            child.painter().text(
                tracks_rect.center(),
                Align2::CENTER_CENTER,
                "Drop audio files here, choose File ▸ Import Audio, or press R to record",
                FontId::proportional(18.0),
                Color32::from_gray(140),
            );
            // Clicking in empty space still sets the cursor.
            return;
        }

        self.prev_track_rects = std::mem::take(&mut self.track_rects);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut remove: Option<usize> = None;
                for i in 0..self.project.tracks.len() {
                    let h = self.project.tracks[i].height;
                    let (row, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), h + 6.0), Sense::hover());
                    let panel = Rect::from_min_size(row.min, vec2(PANEL_W - 4.0, h));
                    let wave = Rect::from_min_max(
                        pos2(row.left() + PANEL_W, row.top()),
                        pos2(row.right(), row.top() + h),
                    );
                    if self.track_panel(ui, i, panel) {
                        remove = Some(i);
                    }
                    self.track_wave(ui, i, wave);
                    // Resize handle.
                    let handle = Rect::from_min_max(pos2(row.left(), row.bottom() - 6.0), row.max);
                    let hr = ui.interact(handle, ui.scope_id().with(("resize", i)), Sense::drag());
                    if hr.hovered() || hr.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }
                    if hr.dragged() {
                        let t = &mut self.project.tracks[i];
                        t.height = (t.height + hr.drag_delta().y).clamp(MIN_TRACK_H, 600.0);
                    }
                    ui.painter().line_segment(
                        [
                            pos2(row.left(), row.bottom() - 3.0),
                            pos2(row.right(), row.bottom() - 3.0),
                        ],
                        Stroke::new(1.0, Color32::from_gray(55)),
                    );
                    self.track_rects.push((i, wave));
                }
                if let Some(i) = remove
                    && !self.busy()
                {
                    self.engine.stop_playback();
                    self.project.remove_track(i);
                }
            });

        // Playhead + auto-scroll.
        if (self.engine.is_playing() && !self.previewing) || self.recording.is_some() {
            let pos = if let Some(rec) = &self.recording {
                self.project
                    .tracks
                    .iter()
                    .find(|t| t.id == rec.track_id)
                    .map_or(0.0, |t| t.len() as f64)
            } else {
                self.engine.position()
            };
            let area = self.last_track_area;
            let x = self.view.sample_to_x(area, pos);
            if x > area.right() - 5.0 || x < area.left() {
                self.view.left = pos - 0.1 * area.width() as f64 * self.view.spp;
                self.view.left = self.view.left.max(0.0);
            }
            let x = self.view.sample_to_x(area, pos);
            let p = ui.painter_at(Rect::from_min_max(pos2(area.left(), full.top()), area.max));
            let col = if self.recording.is_some() {
                Color32::from_rgb(255, 80, 80)
            } else {
                Color32::from_rgb(80, 255, 120)
            };
            p.line_segment(
                [pos2(x, full.top()), pos2(x, area.bottom())],
                Stroke::new(1.5, col),
            );
            p.add(egui::Shape::convex_polygon(
                vec![
                    pos2(x - 6.0, full.top()),
                    pos2(x + 6.0, full.top()),
                    pos2(x, full.top() + 10.0),
                ],
                col,
                Stroke::NONE,
            ));
        }
    }

    fn ruler(&mut self, ui: &mut Ui, rect: Rect) {
        let resp = ui.interact(rect, ui.scope_id().with("ruler"), Sense::click());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, CornerRadius::ZERO, Color32::from_gray(38));
        let px_per_sec = self.rate() / self.view.spp;
        let steps = [
            0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0,
            60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
        ];
        let major = steps
            .iter()
            .copied()
            .find(|s| s * px_per_sec >= 90.0)
            .unwrap_or(3600.0);
        let minor = major / 5.0;
        let t0 = self.view.left / self.rate();
        let t1 = t0 + rect.width() as f64 / px_per_sec;
        let mut t = (t0 / minor).floor() * minor;
        let mut k = (t0 / minor).floor() as i64;
        while t <= t1 {
            let x = rect.left() + ((t - t0) * px_per_sec) as f32;
            let is_major = k % 5 == 0;
            let h = if is_major { 12.0 } else { 5.0 };
            p.line_segment(
                [pos2(x, rect.bottom() - h), pos2(x, rect.bottom())],
                Stroke::new(1.0, Color32::from_gray(150)),
            );
            if is_major && t >= 0.0 {
                let label = if major >= 1.0 {
                    let tot = t.round() as u64;
                    if tot >= 3600 {
                        format!("{}:{:02}:{:02}", tot / 3600, tot / 60 % 60, tot % 60)
                    } else {
                        format!("{}:{:02}", tot / 60, tot % 60)
                    }
                } else {
                    format!("{:.3}", t)
                };
                p.text(
                    pos2(x + 3.0, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::proportional(11.0),
                    Color32::from_gray(200),
                );
            }
            k += 1;
            t = k as f64 * minor;
        }
        // Selection indicator on ruler.
        let sel = self.project.selection;
        if !sel.is_empty() {
            let x0 = self.view.sample_to_x(rect, sel.start as f64);
            let x1 = self.view.sample_to_x(rect, sel.end as f64);
            p.rect_filled(
                Rect::from_min_max(pos2(x0, rect.bottom() - 4.0), pos2(x1, rect.bottom())),
                CornerRadius::ZERO,
                Color32::from_rgb(90, 140, 220),
            );
        }
        // Quick-Play: click on the ruler to play from there.
        if resp.clicked()
            && !self.busy()
            && !self.project.tracks.is_empty()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let s = self.view.x_to_sample(rect, pos.x).max(0.0) as usize;
            self.play_from(s.min(self.project.len()));
        }
        resp.on_hover_text("Click to play from here");
    }

    fn hscrollbar(&mut self, ui: &mut Ui, rect: Rect) {
        let total = (self.project.len() as f64).max(1.0);
        let visible = rect.width() as f64 * self.view.spp;
        let content = total.max(self.view.left + visible);
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(3), Color32::from_gray(35));
        let x0 = rect.left() + (self.view.left / content) as f32 * rect.width();
        let w = ((visible / content) as f32 * rect.width()).clamp(20.0, rect.width());
        let thumb = Rect::from_min_size(pos2(x0, rect.top() + 2.0), vec2(w, rect.height() - 4.0));
        let resp = ui.interact(rect, ui.scope_id().with("hscroll"), Sense::click_and_drag());
        let col = if resp.dragged() || resp.hovered() {
            Color32::from_gray(140)
        } else {
            Color32::from_gray(100)
        };
        p.rect_filled(thumb, CornerRadius::same(3), col);
        if resp.dragged() {
            self.view.left = (self.view.left
                + resp.drag_delta().x as f64 / rect.width() as f64 * content)
                .max(0.0);
        } else if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let frac = ((pos.x - rect.left()) / rect.width()) as f64;
            self.view.left = (frac * content - visible / 2.0).max(0.0);
        }
    }

    /// Returns true if the track's close button was clicked.
    fn track_panel(&mut self, ui: &mut Ui, i: usize, rect: Rect) -> bool {
        let selected = self.project.tracks[i].selected;
        let bg = if selected {
            Color32::from_rgb(52, 58, 72)
        } else {
            Color32::from_gray(42)
        };
        ui.painter().rect_filled(rect, CornerRadius::same(3), bg);
        let bg_resp = ui.interact(rect, ui.scope_id().with(("panel", i)), Sense::click());
        if bg_resp.clicked() {
            let ctrl = ui.input(|inp| inp.modifiers.command);
            if ctrl {
                self.project.tracks[i].selected ^= true;
            } else {
                for (j, t) in self.project.tracks.iter_mut().enumerate() {
                    t.selected = j == i;
                }
            }
        }
        let mut close = false;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(4.0)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing = vec2(4.0, 3.0);
        let t = &mut self.project.tracks[i];
        ui.horizontal(|ui| {
            if ui.small_button("✕").on_hover_text("Remove track").clicked() {
                close = true;
            }
            ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(PANEL_W - 40.0));
        });
        ui.horizontal(|ui| {
            ui.toggle_value(&mut t.mute, "Mute");
            ui.toggle_value(&mut t.solo, "Solo");
        });
        let mut db = lin_to_db(t.gain).max(-36.0);
        ui.horizontal(|ui| {
            ui.label("−");
            if ui
                .add(egui::Slider::new(&mut db, -36.0..=36.0).show_value(false))
                .on_hover_text(format!("Gain: {db:+.1} dB (double-click to reset)"))
                .double_clicked()
            {
                db = 0.0;
            }
            ui.label("+");
        });
        t.gain = if db <= -36.0 { 0.0 } else { db_to_lin(db) };
        ui.horizontal(|ui| {
            ui.label("L");
            let pan_text = if t.pan.abs() < 0.01 {
                "Pan: center".to_string()
            } else {
                format!(
                    "Pan: {:.0}% {}",
                    t.pan.abs() * 100.0,
                    if t.pan < 0.0 { "left" } else { "right" }
                )
            };
            if ui
                .add(egui::Slider::new(&mut t.pan, -1.0..=1.0).show_value(false))
                .on_hover_text(pan_text)
                .double_clicked()
            {
                t.pan = 0.0;
            }
            ui.label("R");
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut t.view, TrackView::Waveform, "Wave");
            ui.selectable_value(&mut t.view, TrackView::Spectrogram, "Spectrum");
        });
        if t.height >= 130.0 {
            let chans = if t.num_channels() == 1 {
                "Mono"
            } else {
                "Stereo"
            };
            ui.label(
                egui::RichText::new(format!("{chans}, {} Hz\n32-bit float", self.project.rate))
                    .small()
                    .weak(),
            );
        }
        close
    }

    fn track_wave(&mut self, ui: &mut Ui, i: usize, rect: Rect) {
        let resp = ui.interact(
            rect,
            ui.scope_id().with(("wave", i)),
            Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);
        let track = &self.project.tracks[i];
        let selected = track.selected;
        painter.rect_filled(
            rect,
            CornerRadius::ZERO,
            if selected {
                Color32::from_rgb(34, 38, 48)
            } else {
                Color32::from_gray(28)
            },
        );

        let sel = self.project.selection;
        // While a whole track is being time-shifted, draw it at its new position.
        let shift_preview = match &self.shift {
            Some(sd) if !sd.clip && sd.track == i => sd.delta as f64,
            _ => 0.0,
        };
        let draw_left = self.view.left - shift_preview;
        // End-of-audio shading.
        let end_x = self
            .view
            .sample_to_x(rect, track.len() as f64 + shift_preview);
        if end_x < rect.right() {
            painter.rect_filled(
                Rect::from_min_max(pos2(end_x.max(rect.left()), rect.top()), rect.max),
                CornerRadius::ZERO,
                Color32::from_gray(22),
            );
        }

        let colors = waveform::Colors {
            wave: Color32::from_rgb(100, 140, 230),
            rms: Color32::from_rgb(150, 185, 255),
            clip: Color32::from_rgb(240, 60, 60),
            center: Color32::from_gray(70),
        };
        let nch = track.num_channels();
        let lane_h = rect.height() / nch as f32;
        let id = track.id;
        let rev = self.project.revision;
        for (c, data) in track.channels.iter().enumerate() {
            let lane = Rect::from_min_size(
                pos2(rect.left(), rect.top() + c as f32 * lane_h),
                vec2(rect.width(), lane_h),
            );
            if track.view == TrackView::Spectrogram {
                let lane_px = lane.shrink2(vec2(0.0, 1.0));
                let (w, h) = (
                    lane_px.width().max(1.0) as usize,
                    lane_px.height().max(1.0) as usize,
                );
                let tex = self.spec_cache.texture(
                    ui.ctx(),
                    id,
                    c,
                    rev,
                    data,
                    self.project.rate,
                    draw_left,
                    self.view.spp,
                    w,
                    h,
                );
                painter.image(
                    tex.id(),
                    lane_px,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
                // Frequency labels.
                for f in [100.0, 1000.0, 10000.0] {
                    let y = lane_px.top()
                        + spectrogram::frac_of(f, self.project.rate) * lane_px.height();
                    if y > lane_px.top() + 8.0 && y < lane_px.bottom() - 4.0 {
                        let label = if f >= 1000.0 {
                            format!("{}k", f / 1000.0)
                        } else {
                            format!("{f}")
                        };
                        painter.text(
                            pos2(lane_px.left() + 3.0, y),
                            Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(10.0),
                            Color32::from_white_alpha(170),
                        );
                    }
                }
            } else {
                let blocks = self.cache.get(id, c, rev, data);
                waveform::draw_channel(
                    &painter,
                    lane.shrink2(vec2(0.0, 2.0)),
                    data,
                    blocks,
                    draw_left,
                    self.view.spp,
                    self.vzoom,
                    &colors,
                );
            }
            if c > 0 {
                painter.line_segment(
                    [
                        pos2(rect.left(), lane.top()),
                        pos2(rect.right(), lane.top()),
                    ],
                    Stroke::new(1.0, Color32::from_gray(50)),
                );
            }
        }

        self.draw_word_labels(&painter, i, rect, draw_left);

        // Selection highlight.
        if selected && !sel.is_empty() {
            let x0 = self
                .view
                .sample_to_x(rect, sel.start as f64)
                .max(rect.left());
            let x1 = self
                .view
                .sample_to_x(rect, sel.end as f64)
                .min(rect.right());
            if x1 > x0 {
                painter.rect_filled(
                    Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.bottom())),
                    CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(170, 190, 255, 45),
                );
            }
        }

        // Cursor line.
        if sel.is_empty() && selected {
            let x = self.view.sample_to_x(rect, sel.start as f64);
            painter.line_segment(
                [pos2(x, rect.top()), pos2(x, rect.bottom())],
                Stroke::new(1.0, Color32::WHITE),
            );
        }
        if selected {
            painter.rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::from_rgb(90, 120, 190)),
                StrokeKind::Inside,
            );
        }

        // Ghost of the selected audio while it is being dragged with the Time Shift tool.
        if let Some(sd) = &self.shift
            && sd.clip
        {
            let n_sel = self.project.tracks.iter().filter(|t| t.selected).count();
            let on_this = match sd.dest {
                Some(d) if n_sel == 1 => d == i,
                _ => selected,
            };
            if on_this {
                let s0 = (sel.start as i64 + sd.delta).max(0) as f64;
                let x0 = self.view.sample_to_x(rect, s0);
                let x1 = self.view.sample_to_x(rect, s0 + sel.len() as f64);
                let ghost =
                    Rect::from_min_max(pos2(x0, rect.top() + 2.0), pos2(x1, rect.bottom() - 2.0));
                painter.rect_filled(
                    ghost,
                    CornerRadius::same(3),
                    Color32::from_rgba_unmultiplied(255, 210, 90, 40),
                );
                painter.rect_stroke(
                    ghost,
                    CornerRadius::same(3),
                    Stroke::new(1.5, Color32::from_rgb(255, 210, 90)),
                    StrokeKind::Inside,
                );
            }
        }

        // Interaction.
        if self.busy() {
            return;
        }
        if self.tool == Tool::TimeShift {
            self.time_shift_interaction(ui, i, rect, &resp);
            return;
        }
        let ptr = ui
            .input(|i| i.pointer.press_origin())
            .or(resp.interact_pointer_pos());
        if (resp.drag_started() || (resp.clicked() && self.drag.is_none()))
            && let Some(p) = ptr
        {
            let s = self.view.x_to_sample(rect, p.x).max(0.0) as usize;
            let (shift, ctrl) = ui.input(|inp| (inp.modifiers.shift, inp.modifiers.command));
            if shift {
                // Extend existing selection.
                let anchor =
                    if (s as i64 - sel.start as i64).abs() < (s as i64 - sel.end as i64).abs() {
                        sel.end
                    } else {
                        sel.start
                    };
                self.project.selection = Selection::new(anchor, s);
                self.project.tracks[i].selected = true;
                self.drag = Some(DragState {
                    anchor,
                    start_y: p.y,
                    additive: true,
                });
            } else {
                self.project.selection = Selection::cursor(s);
                if ctrl {
                    self.project.tracks[i].selected = true;
                } else {
                    for (j, t) in self.project.tracks.iter_mut().enumerate() {
                        t.selected = j == i;
                    }
                }
                self.drag = Some(DragState {
                    anchor: s,
                    start_y: p.y,
                    additive: ctrl,
                });
            }
            if self.engine.is_playing() && resp.clicked() {
                self.play_from(s);
            }
        }
        if resp.dragged()
            && let (Some(p), Some(d)) = (ui.input(|inp| inp.pointer.interact_pos()), &self.drag)
        {
            let s = self.view.x_to_sample(rect, p.x).max(0.0) as usize;
            self.project.selection = Selection::new(d.anchor, s);
            // Extend track selection vertically.
            let (y0, y1) = (d.start_y.min(p.y), d.start_y.max(p.y));
            let additive = d.additive;
            for (ti, r) in &self.track_rects {
                if r.bottom() >= y0 && r.top() <= y1 {
                    self.project.tracks[*ti].selected = true;
                } else if !additive {
                    self.project.tracks[*ti].selected = false;
                }
            }
            // Auto-scroll while dragging past the edges.
            let area = self.last_track_area;
            if p.x > area.right() {
                self.view.left += (p.x - area.right()) as f64 * self.view.spp * 0.2;
            } else if p.x < area.left() {
                self.view.left =
                    (self.view.left - (area.left() - p.x) as f64 * self.view.spp * 0.2).max(0.0);
            }
        }
        if resp.drag_stopped() || resp.clicked() {
            self.drag = None;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }
    }

    /// Time Shift tool: drag a whole track, or the selected audio, along the timeline.
    fn time_shift_interaction(&mut self, ui: &Ui, i: usize, rect: Rect, resp: &egui::Response) {
        let sel = self.project.selection;
        if resp.drag_started()
            && let Some(p) = ui.input(|inp| inp.pointer.press_origin())
        {
            let s = self.view.x_to_sample(rect, p.x).max(0.0) as usize;
            let t = &self.project.tracks[i];
            let clip = t.selected && !sel.is_empty() && s >= sel.start && s < sel.end;
            self.shift = Some(ShiftDrag {
                track: i,
                start_x: p.x,
                clip,
                delta: 0,
                dest: None,
            });
        }
        if resp.dragged()
            && let Some(p) = ui.input(|inp| inp.pointer.interact_pos())
            && let Some(sd) = &self.shift
        {
            let raw = ((p.x - sd.start_x) as f64 * self.view.spp).round() as i64;
            let (clip, track) = (sd.clip, sd.track);
            let delta = if clip {
                raw.max(-(sel.start as i64))
            } else {
                self.project.clamp_track_shift(track, raw)
            };
            let dest = self
                .prev_track_rects
                .iter()
                .find(|(_, r)| p.y >= r.top() && p.y <= r.bottom())
                .map(|(ti, _)| *ti);
            if let Some(sd) = &mut self.shift {
                sd.delta = delta;
                sd.dest = dest;
            }
            let what = if clip { "selection" } else { "track" };
            self.status = format!(
                "Moving {what} by {:+.3} s{}",
                delta as f64 / self.rate(),
                match dest {
                    Some(d) if clip && d != track =>
                        format!(" → “{}”", self.project.tracks[d].name),
                    _ => String::new(),
                }
            );
        }
        if resp.drag_stopped()
            && let Some(sd) = self.shift.take()
        {
            self.engine.stop_playback();
            let secs = sd.delta as f64 / self.rate();
            if sd.clip {
                let dest = sd.dest.filter(|&d| d != sd.track);
                if self.project.move_selection(sd.delta, dest) {
                    self.status = format!("Moved selection by {secs:+.3} s");
                } else {
                    self.status = "Nothing moved".into();
                }
            } else {
                let applied = self.project.shift_track(sd.track, sd.delta);
                self.status = if applied == 0 && sd.delta == 0 {
                    "Nothing moved".into()
                } else {
                    format!("Moved track by {:+.3} s", applied as f64 / self.rate())
                };
            }
        }
        if resp.clicked() {
            // A plain click selects the track (Ctrl adds to the selection).
            let ctrl = ui.input(|inp| inp.modifiers.command);
            if ctrl {
                self.project.tracks[i].selected ^= true;
            } else {
                for (j, t) in self.project.tracks.iter_mut().enumerate() {
                    t.selected = j == i;
                }
            }
        }
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    // ----------------------------------------------------------------- dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.clone() else {
            return;
        };
        if let Dialog::Effect(fx) = d {
            self.effect_dialog(ctx, fx);
            return;
        }
        if let Dialog::ConfirmUnsaved(action) = d {
            self.confirm_dialog(ctx, action);
            return;
        }
        let title = match &d {
            Dialog::Effect(_) | Dialog::ConfirmUnsaved(_) => "Effect",
            Dialog::Tone { .. } => "Generate Tone",
            Dialog::Noise { .. } => "Generate Noise",
            Dialog::InsertSilence { .. } => "Generate Silence",
            Dialog::Export { .. } => "Export Audio",
            Dialog::Shortcuts => "Keyboard Shortcuts",
            Dialog::About => "About Rust Audacity",
            Dialog::Message(_) => "Rust Audacity",
        };
        let mut ok = false;
        let mut cancel = false;
        let mut open = true;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("dlg")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| match &mut d {
                        Dialog::Effect(_) | Dialog::ConfirmUnsaved(_) => {}
                        Dialog::Tone {
                            wave,
                            freq,
                            amp,
                            secs,
                        } => {
                            ui.label("Waveform:");
                            egui::ComboBox::from_id_salt("wave")
                                .selected_text(format!("{wave:?}"))
                                .show_ui(ui, |ui| {
                                    for w in [
                                        Waveform::Sine,
                                        Waveform::Square,
                                        Waveform::Sawtooth,
                                        Waveform::Triangle,
                                    ] {
                                        ui.selectable_value(wave, w, format!("{w:?}"));
                                    }
                                });
                            ui.end_row();
                            ui.label("Frequency:");
                            ui.add(
                                egui::DragValue::new(freq)
                                    .speed(1.0)
                                    .range(1.0..=(self.project.rate as f32 * 0.5))
                                    .suffix(" Hz"),
                            );
                            ui.end_row();
                            ui.label("Amplitude (0–1):");
                            ui.add(egui::Slider::new(amp, 0.0..=1.0));
                            ui.end_row();
                            ui.label("Duration:");
                            ui.add(
                                egui::DragValue::new(secs)
                                    .speed(0.1)
                                    .range(0.0..=3600.0)
                                    .suffix(" s"),
                            );
                            ui.end_row();
                        }
                        Dialog::Noise { kind, amp, secs } => {
                            ui.label("Noise type:");
                            egui::ComboBox::from_id_salt("noise")
                                .selected_text(format!("{kind:?}"))
                                .show_ui(ui, |ui| {
                                    for k in [NoiseKind::White, NoiseKind::Pink, NoiseKind::Brown] {
                                        ui.selectable_value(kind, k, format!("{k:?}"));
                                    }
                                });
                            ui.end_row();
                            ui.label("Amplitude (0–1):");
                            ui.add(egui::Slider::new(amp, 0.0..=1.0));
                            ui.end_row();
                            ui.label("Duration:");
                            ui.add(
                                egui::DragValue::new(secs)
                                    .speed(0.1)
                                    .range(0.0..=3600.0)
                                    .suffix(" s"),
                            );
                            ui.end_row();
                        }
                        Dialog::InsertSilence { secs } => {
                            ui.label("Duration:");
                            ui.add(
                                egui::DragValue::new(secs)
                                    .speed(0.1)
                                    .range(0.0..=3600.0)
                                    .suffix(" s"),
                            );
                            ui.end_row();
                        }
                        Dialog::Export {
                            fmt,
                            selection_only,
                        } => {
                            Self::export_format_ui(ui, fmt);
                            ui.label("");
                            ui.checkbox(selection_only, "Selected audio only");
                            ui.end_row();
                        }
                        Dialog::Shortcuts => {
                            for (k, v) in [
                                ("Space", "Play / Stop"),
                                ("P", "Pause"),
                                ("R", "Record / stop recording"),
                                ("L", "Toggle loop"),
                                ("Click ruler", "Play from that point"),
                                ("Ctrl+Z / Ctrl+Y", "Undo / Redo"),
                                ("Ctrl+X / C / V", "Cut / Copy / Paste"),
                                ("Delete", "Delete selection"),
                                ("Ctrl+L", "Silence selection"),
                                ("Ctrl+T", "Trim to selection"),
                                ("Ctrl+D", "Duplicate selection"),
                                ("Ctrl+A", "Select all"),
                                ("Shift+click", "Extend selection"),
                                ("Ctrl+click", "Add track to selection"),
                                ("Home / End", "Cursor to start / end"),
                                ("← / →", "Nudge cursor"),
                                ("Ctrl+1 / Ctrl+3", "Zoom in / out"),
                                ("Ctrl+E / Ctrl+F", "Zoom to selection / fit"),
                                ("Ctrl+wheel", "Zoom at pointer"),
                                ("Shift+wheel", "Scroll horizontally"),
                                ("F1 / F5", "Selection tool / Time Shift tool"),
                                ("Ctrl+Shift+T", "Show / hide the transcript panel"),
                                ("Ctrl+N", "New project"),
                                ("Ctrl+O", "Open project"),
                                ("Ctrl+S / Ctrl+Shift+S", "Save project / Save As"),
                                ("Ctrl+Shift+I", "Import audio"),
                                ("Ctrl+Shift+E", "Export audio (WAV/FLAC/MP3/OGG)"),
                            ] {
                                ui.monospace(k);
                                ui.label(v);
                                ui.end_row();
                            }
                        }
                        Dialog::About => {
                            ui.label(
                            "Rust Audacity 0.4\nAn Audacity-style audio editor written in Rust.\n\n\
                             egui/eframe · cpal · symphonia · hound · realfft · ebur128\n\
                             flacenc · LAME (MP3) · libvorbis/aoTuV (Ogg)\n\
                             AI stem separation via Meta's Demucs (optional, external).",
                        );
                            ui.end_row();
                        }
                        Dialog::Message(m) => {
                            ui.label(m.as_str());
                            ui.end_row();
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    let info = matches!(d, Dialog::Shortcuts | Dialog::About | Dialog::Message(_));
                    if info {
                        if ui.button("OK").clicked() {
                            cancel = true;
                        }
                    } else {
                        if ui.button("  OK  ").clicked() {
                            ok = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });

        if !open || cancel {
            self.dialog = None;
            return;
        }
        if !ok {
            self.dialog = Some(d);
            return;
        }
        self.dialog = None;
        match d {
            Dialog::Tone {
                wave,
                freq,
                amp,
                secs,
            } => self.generate(Generator::Tone {
                wave,
                freq,
                amp,
                secs,
            }),
            Dialog::Noise { kind, amp, secs } => {
                self.generate(Generator::Noise { kind, amp, secs })
            }
            Dialog::InsertSilence { secs } => self.generate(Generator::Silence { secs }),
            Dialog::Export {
                fmt,
                selection_only,
            } => self.export(fmt, selection_only),
            _ => {}
        }
    }

    fn generate(&mut self, g: Generator) {
        if self.busy() {
            return;
        }
        self.engine.stop_playback();
        let was_empty = self.project.tracks.is_empty();
        effects::generate(&mut self.project, g);
        if was_empty {
            self.zoom_fit(self.last_track_area.width());
        }
        self.status = format!("Generated {}", g.name());
    }
}

impl eframe::App for AudacityApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.engine.poll();
        self.pump_recording();
        self.poll_imports(self.last_track_area.width().max(600.0));
        self.poll_stems(&ctx);
        self.poll_job(&ctx);
        self.poll_transcription(&ctx);
        self.poll_bridge(&ctx);
        if ctx.input(|i| i.viewport().close_requested())
            && !self.close_confirmed
            && self.has_unsaved_changes()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::ConfirmUnsaved(Pending::Exit));
        }
        if self.previewing && !self.engine.is_playing() {
            self.previewing = false;
        }
        if self.engine.is_playing() {
            self.engine.update_gains(&self.project);
        }
        if self.engine.is_playing() || self.recording.is_some() || self.importing > 0 {
            ctx.request_repaint();
        }

        // Drag-and-drop import.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for p in dropped {
            self.import_path(p);
        }

        self.handle_shortcuts(&ctx);

        let ids: Vec<u64> = self.project.tracks.iter().map(|t| t.id).collect();
        self.cache.retain_tracks(&ids);
        self.spec_cache.retain_tracks(&ids);

        let title = format!(
            "{}{} – Rust Audacity",
            if self.project.dirty { "*" } else { "" },
            self.title
        );
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.transcript.show_panel {
            egui::Panel::right("transcript")
                .resizable(true)
                .default_size(380.0)
                .min_size(240.0)
                .show(ui, |ui| self.transcript_panel(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.tracks_area(ui));

        // Show a hint while files are being dragged over the window.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let p = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("drop"),
            ));
            p.rect_filled(screen, CornerRadius::ZERO, Color32::from_black_alpha(160));
            p.text(
                screen.center(),
                Align2::CENTER_CENTER,
                "Drop to import",
                FontId::proportional(28.0),
                Color32::WHITE,
            );
        }

        self.dialogs(&ctx);
        self.chains_window(&ctx);
        self.analysis_window(&ctx);
        self.stems_windows(&ctx);
        self.job_window(&ctx);
        self.transcribe_windows(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.chains.save(storage);
        self.stems.save(storage);
        self.save_export_format(storage);
        self.transcript.save(storage);
        self.bridge.save(storage);
    }

    fn on_exit(&mut self) {
        self.bridge.shutdown();
        self.engine.stop_recording();
        self.engine.stop_playback();
    }
}
