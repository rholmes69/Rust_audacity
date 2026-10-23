//! Speech-to-text: the Transcribe dialog, the background Whisper job, and the transcript
//! panel with text-based editing (click words to select, delete words to cut audio).

use std::{
    sync::{
        Arc,
        mpsc::{Receiver, channel},
    },
    time::Instant,
};

use eframe::egui::{self, Align2, Color32, RichText, Sense, vec2};

use super::{AudacityApp, Dialog};
use crate::{
    dsp::separation::CancelHandle,
    project::Selection,
    transcript::{self, Transcript},
    whisper::{self, LANGUAGES, WhisperEvent, WhisperModel, WhisperOptions},
};

const KEY_PY: &str = "whisper_python";
const KEY_MODEL: &str = "whisper_model";
const KEY_LANG: &str = "whisper_language";
/// A pause this long starts a new paragraph in the panel.
const PARAGRAPH_GAP_SECS: f64 = 1.5;

struct Job {
    rx: Receiver<WhisperEvent>,
    cancel: CancelHandle,
    progress: Option<f32>,
    status: String,
    started: Instant,
    track_id: u64,
    track_name: String,
}

pub struct TranscriptState {
    pub show_panel: bool,
    pub dialog_open: bool,
    model: WhisperModel,
    language: String,
    python: String,
    /// Text edits cut every track (keeps music beds etc. in sync) or only the voice track.
    cut_all_tracks: bool,
    follow_playback: bool,
    search: String,
    /// Word index where a shift-click selection starts.
    anchor: Option<usize>,
    job: Option<Job>,
}

impl TranscriptState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let get = |k: &str| storage.and_then(|s| eframe::get_value::<String>(s, k));
        Self {
            show_panel: false,
            dialog_open: false,
            model: get(KEY_MODEL)
                .and_then(|m| WhisperModel::from_id(&m))
                .unwrap_or(WhisperModel::Small),
            language: get(KEY_LANG).unwrap_or_default(),
            python: get(KEY_PY).unwrap_or_else(|| whisper::default_python().into()),
            cut_all_tracks: true,
            follow_playback: true,
            search: String::new(),
            anchor: None,
            job: None,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_PY, &self.python);
        eframe::set_value(storage, KEY_MODEL, &self.model.id().to_string());
        eframe::set_value(storage, KEY_LANG, &self.language);
    }

    pub fn python(&self) -> &str {
        &self.python
    }

    pub fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl AudacityApp {
    /// The track a new transcription would use: the first selected track.
    fn transcribe_target(&self) -> Option<usize> {
        self.project.target_tracks().first().copied()
    }

    pub(super) fn open_transcribe_dialog(&mut self) {
        if self.project.tracks.is_empty() {
            self.dialog = Some(Dialog::Message(
                "Import or record some speech first.".into(),
            ));
            return;
        }
        self.transcript.dialog_open = true;
    }

    /// Start Whisper on a track (whole track). Used by the dialog and the agent bridge.
    pub(super) fn start_transcription(
        &mut self,
        track: usize,
        model: Option<WhisperModel>,
        language: Option<String>,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("Rust Audacity is busy with another job.".into());
        }
        let t = self.project.tracks.get(track).ok_or("no such track")?;
        if t.len() < self.project.rate as usize / 2 {
            return Err("That track is too short to transcribe.".into());
        }
        self.engine.stop_playback();
        if let Some(m) = model {
            self.transcript.model = m;
        }
        if let Some(l) = language {
            self.transcript.language = l;
        }
        let opts = WhisperOptions {
            python: self.transcript.python.clone(),
            model: self.transcript.model,
            language: self.transcript.language.clone(),
            env: Vec::new(),
        };
        let channels: Vec<Vec<f32>> = t.channels.iter().map(|c| c.to_vec()).collect();
        let rate = self.project.rate;
        let (tx, rx) = channel();
        let cancel = CancelHandle::default();
        let c2 = cancel.clone();
        std::thread::Builder::new()
            .name("whisper".into())
            .spawn(move || {
                let _ = tx.send(WhisperEvent::Status("Preparing audio…".into()));
                let audio = whisper::prepare_audio(&channels, rate);
                drop(channels);
                let r = whisper::run_whisper(&opts, &audio, &tx, &c2).map_err(|e| format!("{e:#}"));
                let _ = tx.send(WhisperEvent::Done(r));
            })
            .map_err(|e| e.to_string())?;
        self.transcript.job = Some(Job {
            rx,
            cancel,
            progress: None,
            status: "Starting Whisper…".into(),
            started: Instant::now(),
            track_id: t.id,
            track_name: t.name.clone(),
        });
        self.status = format!("Transcribing “{}”…", t.name);
        Ok(())
    }

    pub(super) fn poll_transcription(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.transcript.job else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let mut done = None;
        while let Ok(ev) = job.rx.try_recv() {
            match ev {
                WhisperEvent::Progress(p) => job.progress = Some(p),
                WhisperEvent::Status(s) => job.status = s,
                WhisperEvent::Done(r) => done = Some(r),
            }
        }
        let Some(result) = done else {
            return;
        };
        let job = self.transcript.job.take().unwrap();
        match result {
            Ok(t) => {
                let Some(idx) = self
                    .project
                    .tracks
                    .iter()
                    .position(|x| x.id == job.track_id)
                else {
                    self.status = "Transcription finished, but its track was removed".into();
                    return;
                };
                let n = t.words.len();
                let lang = t.language.clone();
                let tr = t.into_transcript(job.track_id, self.project.rate, 0);
                self.project.push_undo("Transcribe");
                self.project.transcript = Some(Arc::new(tr));
                self.transcript.show_panel = true;
                self.transcript.anchor = None;
                self.status = format!(
                    "Transcribed “{}”: {n} words, language {lang} ({:.0} s)",
                    self.project.tracks[idx].name,
                    job.started.elapsed().as_secs_f32()
                );
                if n == 0 {
                    self.dialog = Some(Dialog::Message(
                        "Whisper didn't hear any speech on that track.".into(),
                    ));
                }
            }
            Err(e) if e.contains("cancelled") => self.status = "Transcription cancelled".into(),
            Err(e) => {
                self.status = "Transcription failed".into();
                self.dialog = Some(Dialog::Message(format!("Transcription failed:\n{e}")));
            }
        }
    }

    pub(super) fn transcribe_windows(&mut self, ctx: &egui::Context) {
        // Progress window.
        if let Some(job) = &self.transcript.job {
            let mut cancel = false;
            egui::Window::new("Transcribing")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(380.0);
                    ui.heading(format!("Transcribing “{}”…", job.track_name));
                    ui.add_space(4.0);
                    let bar = match job.progress {
                        Some(p) => egui::ProgressBar::new(p).show_percentage(),
                        None => egui::ProgressBar::new(0.0).animate(true),
                    };
                    ui.add(bar.desired_width(360.0));
                    ui.label(RichText::new(&job.status).small().weak());
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{:.0} s elapsed",
                            job.started.elapsed().as_secs_f32()
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            if cancel {
                job.cancel.cancel();
            }
        }

        if !self.transcript.dialog_open {
            return;
        }
        let target = self.transcribe_target();
        let mut open = true;
        let mut start = false;
        let mut close = false;
        egui::Window::new("Transcribe Speech (Whisper)")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Turns speech into text with word timings. Click words to select audio, \
                         delete words to cut them, export subtitles.",
                    )
                    .weak(),
                );
                ui.add_space(6.0);
                egui::Grid::new("whisper").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Track:");
                    ui.label(match target {
                        Some(i) => self.project.tracks[i].name.clone(),
                        None => "—".into(),
                    });
                    ui.end_row();
                    ui.label("Model:");
                    egui::ComboBox::from_id_salt("wmodel")
                        .selected_text(self.transcript.model.label())
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for m in WhisperModel::ALL {
                                ui.selectable_value(&mut self.transcript.model, m, m.label());
                            }
                        });
                    ui.end_row();
                    ui.label("Language:");
                    let cur = LANGUAGES
                        .iter()
                        .find(|(c, _)| *c == self.transcript.language)
                        .map_or(self.transcript.language.as_str(), |(_, n)| n);
                    egui::ComboBox::from_id_salt("wlang")
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for (code, name) in LANGUAGES {
                                ui.selectable_value(&mut self.transcript.language, code.to_string(), name);
                            }
                        });
                    ui.end_row();
                    ui.label("Python:");
                    ui.add(egui::TextEdit::singleline(&mut self.transcript.python).desired_width(300.0))
                        .on_hover_text("The command that starts Python, or its full path, e.g.\n\"C:\\Python312\\python.exe\"");
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.label(RichText::new("One-time setup: install Python 3, then run").small());
                ui.code("pip install -U faster-whisper");
                ui.label(
                    RichText::new(
                        "The model downloads the first time you use it. An NVIDIA GPU is used \
                         automatically when available; on a CPU, “small” takes roughly as long as the audio.",
                    )
                    .small()
                    .weak(),
                );
                if self.project.transcript.is_some() {
                    ui.label(
                        RichText::new("This replaces the current transcript (you can undo).")
                            .small()
                            .color(Color32::from_rgb(230, 190, 90)),
                    );
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(target.is_some(), egui::Button::new("  Transcribe  "))
                        .clicked()
                    {
                        start = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if !open || close {
            self.transcript.dialog_open = false;
        }
        if start && let Some(t) = target {
            self.transcript.dialog_open = false;
            if let Err(e) = self.start_transcription(t, None, None) {
                self.dialog = Some(Dialog::Message(e));
            }
        }
    }

    // ------------------------------------------------------------ editing

    pub(super) fn transcript_cut_all(&mut self, all: bool) {
        self.transcript.cut_all_tracks = all;
    }

    /// Select the audio of words `a..=b` (and the tracks text edits apply to).
    pub(super) fn select_words(&mut self, a: usize, b: usize) {
        let Some(tr) = self.project.transcript.clone() else {
            return;
        };
        let (a, b) = (a.min(b), a.max(b).min(tr.words.len().saturating_sub(1)));
        let Some(first) = tr.words.get(a) else {
            return;
        };
        let last = &tr.words[b];
        self.project.selection = Selection::new(first.start, last.end);
        let tt = self.project.transcript_track();
        let all = self.transcript.cut_all_tracks;
        for (i, t) in self.project.tracks.iter_mut().enumerate() {
            t.selected = all || Some(i) == tt;
        }
    }

    /// Cut the words covered by the selection (snapped to whole words).
    pub(super) fn delete_selected_words(&mut self) -> Result<usize, String> {
        let tr = self
            .project
            .transcript
            .clone()
            .ok_or("There is no transcript.")?;
        let sel = self.project.selection;
        let idx = tr.words_in(sel.start, sel.end.max(sel.start + 1));
        let (Some(&a), Some(&b)) = (idx.first(), idx.last()) else {
            return Err("Select some words first (click a word, shift-click another).".into());
        };
        self.delete_word_range(a, b)
    }

    /// Cut words `a..=b` (by index). Returns how many words were removed.
    pub(super) fn delete_word_range(&mut self, a: usize, b: usize) -> Result<usize, String> {
        if self.busy() {
            return Err("Rust Audacity is busy.".into());
        }
        let tr = self
            .project
            .transcript
            .clone()
            .ok_or("There is no transcript.")?;
        if a > b || b >= tr.words.len() {
            return Err("word index out of range".into());
        }
        self.engine.stop_playback();
        let range = (tr.words[a].start, tr.words[b].end);
        let n = b - a + 1;
        let label = if n == 1 {
            format!("Delete “{}”", tr.words[a].text)
        } else {
            format!("Delete {n} words")
        };
        self.project
            .delete_ranges(&[range], self.transcript.cut_all_tracks, &label);
        self.transcript.anchor = None;
        self.status = label;
        Ok(n)
    }

    pub(super) fn remove_fillers(&mut self) -> usize {
        let Some(tr) = self.project.transcript.clone() else {
            return 0;
        };
        if self.busy() {
            return 0;
        }
        let ranges = tr.filler_ranges();
        let n = tr
            .words
            .iter()
            .filter(|w| transcript::is_filler(&w.text))
            .count();
        if ranges.is_empty() {
            self.status = "No filler words found".into();
            return 0;
        }
        self.engine.stop_playback();
        self.project.delete_ranges(
            &ranges,
            self.transcript.cut_all_tracks,
            "Remove Filler Words",
        );
        self.status = format!("Removed {n} filler words");
        n
    }

    fn export_transcript(&mut self, srt: bool) {
        let Some(tr) = self.project.transcript.clone() else {
            return;
        };
        let ext = if srt { "srt" } else { "txt" };
        let Some(mut path) = rfd::FileDialog::new()
            .add_filter(if srt { "SubRip subtitles" } else { "Text" }, &[ext])
            .set_file_name(format!("{}.{ext}", self.title))
            .save_file()
        else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension(ext);
        }
        let rate = self.project.rate;
        let text = if srt {
            tr.to_srt(rate, 42)
        } else {
            tr.to_text((PARAGRAPH_GAP_SECS * rate as f64) as usize)
        };
        match std::fs::write(&path, text) {
            Ok(()) => self.status = format!("Saved {}", path.display()),
            Err(e) => {
                self.dialog = Some(Dialog::Message(format!(
                    "Could not save {}:\n{e}",
                    path.display()
                )))
            }
        }
    }

    // ------------------------------------------------------------ panel

    pub(super) fn transcript_panel(&mut self, ui: &mut egui::Ui) {
        let Some(tr): Option<Arc<Transcript>> = self.project.transcript.clone() else {
            ui.add_space(8.0);
            ui.heading("Transcript");
            ui.label("No transcript yet.");
            ui.add_space(4.0);
            if ui
                .add_enabled(!self.busy(), egui::Button::new("🎙 Transcribe…"))
                .clicked()
            {
                self.open_transcribe_dialog();
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Select the track with speech, then transcribe it with Whisper. \
                     You can then edit the audio by deleting words.",
                )
                .small()
                .weak(),
            );
            return;
        };
        let rate = self.project.rate as f64;
        let track_name = self
            .project
            .transcript_track()
            .map(|i| self.project.tracks[i].name.clone())
            .unwrap_or_default();
        let idle = !self.busy();
        let sel = self.project.selection;
        let sel_words = if sel.is_empty() {
            Vec::new()
        } else {
            tr.words_in(sel.start, sel.end)
        };
        let fillers = tr
            .words
            .iter()
            .filter(|w| transcript::is_filler(&w.text))
            .count();

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Transcript");
            ui.label(
                RichText::new(format!(
                    "{track_name} · {} · {} words",
                    if tr.language.is_empty() {
                        "?"
                    } else {
                        &tr.language
                    },
                    tr.words.len()
                ))
                .weak(),
            );
        });
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(idle, egui::Button::new("🎙 Transcribe…"))
                .clicked()
            {
                self.open_transcribe_dialog();
            }
            if ui
                .add_enabled(
                    idle && !sel_words.is_empty(),
                    egui::Button::new(format!("✂ Delete {} word(s)", sel_words.len())),
                )
                .on_hover_text("Cut the selected words' audio (Delete key works too)")
                .clicked()
                && let Err(e) = self.delete_selected_words()
            {
                self.status = e;
            }
            if ui
                .add_enabled(
                    idle && fillers > 0,
                    egui::Button::new(format!("Remove fillers ({fillers})")),
                )
                .on_hover_text(format!(
                    "Cut every “{}”",
                    transcript::FILLERS[..6].join("”, “")
                ))
                .clicked()
            {
                self.remove_fillers();
            }
            ui.menu_button("Export ▾", |ui| {
                if ui.button("Subtitles (.srt)…").clicked() {
                    ui.close();
                    self.export_transcript(true);
                }
                if ui.button("Text (.txt)…").clicked() {
                    ui.close();
                    self.export_transcript(false);
                }
                if ui.button("Copy text").clicked() {
                    ui.close();
                    ui.ctx()
                        .copy_text(tr.to_text((PARAGRAPH_GAP_SECS * rate) as usize));
                    self.status = "Transcript copied".into();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.transcript.cut_all_tracks,
                "Cuts apply to all tracks",
            )
            .on_hover_text("Keep other tracks (music, second mic) in sync when you delete words");
            ui.checkbox(&mut self.transcript.follow_playback, "Follow playback");
        });
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.transcript.search)
                    .hint_text("Find words")
                    .desired_width(ui.available_width() - 8.0),
            );
        });
        ui.separator();

        let playing = self.engine.is_playing() && !self.previewing;
        let current = if playing {
            tr.word_at(self.engine.position() as usize)
        } else {
            None
        };
        let needle = self.transcript.search.trim().to_lowercase();
        let para_gap = (PARAGRAPH_GAP_SECS * rate) as usize;
        let mut clicked: Option<(usize, bool, bool)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                    ui.spacing_mut().button_padding = vec2(2.5, 1.0);
                    let mut prev_end: Option<usize> = None;
                    for (i, w) in tr.words.iter().enumerate() {
                        let new_para = match prev_end {
                            None => true,
                            Some(e) => w.start.saturating_sub(e) > para_gap,
                        };
                        if new_para {
                            if prev_end.is_some() {
                                ui.end_row();
                                ui.end_row();
                            }
                            let secs = w.start as f64 / rate;
                            ui.label(
                                RichText::new(format!(
                                    "{}:{:02}",
                                    (secs / 60.0) as u64,
                                    secs as u64 % 60
                                ))
                                .small()
                                .monospace()
                                .color(Color32::from_rgb(110, 150, 200)),
                            );
                        }
                        prev_end = Some(w.end);
                        let selected = sel_words.binary_search(&i).is_ok();
                        let mut text = RichText::new(&w.text).size(15.0);
                        if Some(i) == current {
                            text = text
                                .color(Color32::BLACK)
                                .background_color(Color32::from_rgb(255, 210, 90));
                        } else if transcript::is_filler(&w.text) {
                            text = text.color(Color32::from_rgb(200, 140, 90)).italics();
                        } else if w.prob < 0.45 {
                            text = text.color(Color32::from_rgb(220, 180, 120)).underline();
                        }
                        if !needle.is_empty() && w.text.to_lowercase().contains(&needle) {
                            text = text.background_color(Color32::from_rgb(70, 100, 60));
                        }
                        let r = ui
                            .add(egui::Button::selectable(selected, text).sense(Sense::click()))
                            .on_hover_text(format!(
                                "{:.2}–{:.2} s · confidence {:.0}%\nClick: select · Shift+click: extend · Double-click: play",
                                w.start as f64 / rate,
                                w.end as f64 / rate,
                                w.prob * 100.0
                            ));
                        if Some(i) == current && self.transcript.follow_playback {
                            r.scroll_to_me(Some(egui::Align::Center));
                        }
                        if r.double_clicked() {
                            clicked = Some((i, false, true));
                        } else if r.clicked() {
                            let shift = ui.input(|inp| inp.modifiers.shift);
                            clicked = Some((i, shift, false));
                        }
                    }
                });
            });
        if let Some((i, shift, play)) = clicked
            && idle
        {
            let a = match (shift, self.transcript.anchor) {
                (true, Some(a)) => a,
                _ => {
                    self.transcript.anchor = Some(i);
                    i
                }
            };
            self.select_words(a, i);
            if play || self.engine.is_playing() {
                let start = tr.words[i].start;
                self.play_from(start);
            }
        }
    }

    /// Word labels along the bottom of the transcribed track.
    pub(super) fn draw_word_labels(
        &self,
        painter: &egui::Painter,
        i: usize,
        rect: egui::Rect,
        left: f64,
    ) {
        let Some(tr) = &self.project.transcript else {
            return;
        };
        if self.project.transcript_track() != Some(i) {
            return;
        }
        let spp = self.view.spp;
        let first_visible = left.max(0.0) as usize;
        let last_visible = (left + rect.width() as f64 * spp) as usize;
        let from = tr.words.partition_point(|w| w.end < first_visible);
        let y = rect.bottom() - 3.0;
        let band =
            egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 18.0), rect.max);
        painter.rect_filled(band, 0.0, Color32::from_black_alpha(90));
        let mut last_text_x = f32::NEG_INFINITY;
        for w in tr.words[from..].iter() {
            if w.start > last_visible {
                break;
            }
            let x0 = rect.left() + ((w.start as f64 - left) / spp) as f32;
            let x1 = rect.left() + ((w.end as f64 - left) / spp) as f32;
            painter.line_segment(
                [egui::pos2(x0, band.top()), egui::pos2(x0, band.bottom())],
                egui::Stroke::new(1.0, Color32::from_white_alpha(40)),
            );
            let filler = transcript::is_filler(&w.text);
            let galley_w = w.text.chars().count() as f32 * 6.2;
            if x0 >= last_text_x + 3.0 && (x1 - x0) > 6.0 {
                painter.text(
                    egui::pos2(x0 + 2.0, y),
                    Align2::LEFT_BOTTOM,
                    &w.text,
                    egui::FontId::proportional(11.0),
                    if filler {
                        Color32::from_rgb(220, 150, 90)
                    } else {
                        Color32::from_white_alpha(200)
                    },
                );
                last_text_x = x0 + 2.0 + galley_w;
            }
        }
    }
}
