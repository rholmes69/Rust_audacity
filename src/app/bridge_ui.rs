//! Agent bridge, app side: answers commands from the MCP server (see `crate::bridge`) by
//! driving this window exactly as a user would, so you can watch an AI agent work.

use std::{path::PathBuf, time::Instant};

use eframe::egui;
use serde_json::{Value, json};

use super::{AudacityApp, View};
use crate::{
    bridge::Server,
    dsp::{analysis, denoise},
    effects::{Effect, lin_to_db},
    fileio::{ExportFormat, WavFormat},
    project::{Project, Selection},
    projectfile,
    whisper::WhisperModel,
};

const KEY_ENABLED: &str = "agent_bridge_enabled";

pub struct BridgeState {
    pub enabled: bool,
    server: Option<Server>,
    pub error: Option<String>,
    /// When the last agent command arrived (for the status-bar indicator).
    pub last_request: Option<Instant>,
    pub last_command: String,
}

impl BridgeState {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            enabled: storage
                .and_then(|s| eframe::get_value(s, KEY_ENABLED))
                .unwrap_or(true),
            server: None,
            error: None,
            last_request: None,
            last_command: String::new(),
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY_ENABLED, &self.enabled);
    }

    /// Start or stop the listener to match `enabled`.
    pub fn sync(&mut self, ctx: &egui::Context) {
        match (self.enabled, self.server.is_some()) {
            (true, false) if self.error.is_none() => {
                let ctx = ctx.clone();
                match Server::start(move || ctx.request_repaint()) {
                    Ok(s) => self.server = Some(s),
                    Err(e) => self.error = Some(format!("Agent bridge could not start: {e}")),
                }
            }
            (false, true) => {
                self.server = None; // Drop shuts it down and removes the discovery file.
            }
            _ => {}
        }
    }

    pub fn shutdown(&mut self) {
        self.server = None;
    }

    /// Port and token of the running bridge (for the built-in MCP server).
    pub fn info(&self) -> Option<crate::bridge::BridgeInfo> {
        self.server.as_ref().map(|s| s.info.clone())
    }

    pub fn active_recently(&self) -> bool {
        self.last_request
            .is_some_and(|t| t.elapsed().as_secs_f32() < 4.0)
    }
}

fn clips_json(r: u32, t: &crate::track::Track) -> Vec<Value> {
    t.clips
        .iter()
        .enumerate()
        .map(|(i, c)| json!({ "clip": i, "name": c.name, "start": secs(r, c.start), "end": secs(r, c.end) }))
        .collect()
}

fn secs(rate: u32, samples: usize) -> f64 {
    (samples as f64 / rate as f64 * 1000.0).round() / 1000.0
}

fn f(a: &Value, k: &str) -> Option<f64> {
    a.get(k).and_then(Value::as_f64)
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

fn b(a: &Value, k: &str) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn db_json(x: f32) -> Value {
    if x.is_finite() {
        json!((x as f64 * 10.0).round() / 10.0)
    } else {
        Value::Null
    }
}

/// Quietest half-second across the target tracks, as a noise sample.
fn auto_noise_profile(p: &Project) -> Option<denoise::NoiseProfile> {
    let (s, e) = p.effect_range();
    let mix = p.mixdown(s, e, true);
    let m = crate::track::adapt_channels(&mix, 1).pop()?;
    let win = p.rate as usize / 2;
    if m.len() < win {
        return None;
    }
    let mut best: Option<(f32, usize)> = None;
    let mut i = 0;
    while i + win <= m.len() {
        let e: f32 = m[i..i + win].iter().map(|x| x * x).sum::<f32>() / win as f32;
        if e > 1e-12 && best.is_none_or(|(be, _)| e < be) {
            best = Some((e, i));
        }
        i += win / 4;
    }
    let (_, at) = best?;
    let mut chans = Vec::new();
    for t in p.target_tracks() {
        chans.extend(p.tracks[t].extract(s + at, s + at + win));
    }
    let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
    denoise::learn_profile(&refs, p.rate)
}

impl AudacityApp {
    /// Handle any agent commands that have arrived. Called every frame.
    pub(super) fn poll_bridge(&mut self, ctx: &egui::Context) {
        self.bridge.sync(ctx);
        let Some(server) = &self.bridge.server else {
            return;
        };
        let requests = server.pending();
        for req in requests {
            self.bridge.last_request = Some(Instant::now());
            self.bridge.last_command = req.cmd.clone();
            let result = self.bridge_command(&req.cmd.clone(), &req.args.clone());
            req.respond(result);
            ctx.request_repaint();
        }
        if self.bridge.active_recently() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn sel_secs(&self) -> Value {
        let r = self.project.rate;
        let sel = self.project.selection;
        json!({ "start": secs(r, sel.start), "end": secs(r, sel.end), "length": secs(r, sel.len()) })
    }

    fn bridge_status(&self) -> Value {
        let r = self.project.rate;
        let tracks: Vec<Value> = self
            .project
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "index": i,
                    "name": t.name,
                    "channels": t.num_channels(),
                    "duration": secs(r, t.len()),
                    "gain_db": db_json(lin_to_db(t.gain)),
                    "pan": (t.pan * 100.0).round() / 100.0,
                    "mute": t.mute,
                    "solo": t.solo,
                    "selected": t.selected,
                    "envelope": t.envelope.points.iter().map(|p| json!([secs(r, p.pos), (p.db * 10.0).round() / 10.0])).collect::<Vec<_>>(),
                    "clips": clips_json(r, t),
                    "live_fx": t.live_fx.iter().map(|f| format!("{}{}", f.name(), if f.enabled { "" } else { " (bypassed)" })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let transcript = self.project.transcript.as_ref().map(|tr| {
            json!({
                "track": self.project.transcript_track(),
                "language": tr.language,
                "words": tr.words.len(),
            })
        });
        json!({
            "title": self.title,
            "project_file": self.project_path.as_ref().map(|p| p.display().to_string()),
            "unsaved_changes": self.has_unsaved_changes(),
            "sample_rate": r,
            "duration": secs(r, self.project.len()),
            "selection": self.sel_secs(),
            "tracks": tracks,
            "busy": self.busy() || self.importing > 0,
            "importing": self.importing,
            "job": self.job_status(),
            "transcribing": self.transcript.running(),
            "playing": self.engine.is_playing(),
            "position": if self.engine.is_playing() { json!(secs(r, self.engine.position() as usize)) } else { Value::Null },
            "status_message": self.status,
            "undo": self.project.undo_label(),
            "redo": self.project.redo_label(),
            "transcript": transcript,
            "labels": self.project.labels.len(),
            "dialog_open": self.dialog.is_some(),
            "dialog_message": match &self.dialog {
                Some(super::Dialog::Message(m)) => json!(m),
                _ => Value::Null,
            },
        })
    }

    fn to_samples(&self, secs: f64) -> usize {
        ((secs.max(0.0) * self.project.rate as f64).round() as usize).min(self.project.len())
    }

    fn require_idle(&self) -> Result<(), String> {
        if self.busy() || self.importing > 0 {
            Err("Rust Audacity is busy (an effect, export, import or transcription is running). Wait and try again.".into())
        } else {
            Ok(())
        }
    }

    fn bridge_command(&mut self, cmd: &str, a: &Value) -> Result<Value, String> {
        let r = self.project.rate;
        match cmd {
            "ping" => Ok(json!({ "app": "Rust Audacity", "version": env!("CARGO_PKG_VERSION") })),
            "status" => {
                let mut st = self.bridge_status();
                if b(a, "measure") && !self.project.tracks.is_empty() {
                    let (s, e) = self.project.effect_range();
                    let mix = self.project.mixdown(s, e, true);
                    let m = analysis::measure(&mix, r);
                    st["levels"] = json!({
                        "range": [secs(r, s), secs(r, e)],
                        "peak_dbfs": db_json(m.peak_db),
                        "true_peak_dbtp": db_json(m.true_peak_db),
                        "rms_dbfs": db_json(m.rms_db),
                        "lufs": m.lufs.map(|l| (l * 10.0).round() / 10.0),
                        "lra": m.lra.map(|l| (l * 10.0).round() / 10.0),
                    });
                }
                Ok(st)
            }
            "open" => {
                self.require_idle()?;
                let path = PathBuf::from(s(a, "path").ok_or("missing `path`")?);
                if !path.exists() {
                    return Err(format!("{} does not exist", path.display()));
                }
                if projectfile::is_project_path(&path) {
                    if self.has_unsaved_changes() && !b(a, "discard_changes") {
                        return Err("The open project has unsaved changes. Save it first (app_save_project) or pass discard_changes=true.".into());
                    }
                    self.open_project(&path);
                    if self.project_path.as_deref() != Some(path.as_path()) {
                        return Err(format!("Could not open {}", path.display()));
                    }
                    self.dialog = None;
                    Ok(json!({ "opened": path.display().to_string() }))
                } else {
                    self.import_path(path.clone());
                    Ok(json!({ "importing": path.display().to_string() }))
                }
            }
            "new_project" => {
                self.require_idle()?;
                if self.has_unsaved_changes() && !b(a, "discard_changes") {
                    return Err("The open project has unsaved changes. Save it first or pass discard_changes=true.".into());
                }
                self.new_project();
                Ok(json!({ "new_project": true }))
            }
            "save_project" => {
                self.require_idle()?;
                if let Some(p) = s(a, "path") {
                    let mut p = PathBuf::from(p);
                    if !projectfile::is_project_path(&p) {
                        p.set_extension(projectfile::EXTENSION);
                    }
                    self.project_path = Some(p);
                }
                if self.project_path.is_none() {
                    return Err(
                        "This project has never been saved: pass a `path` ending in .rap".into(),
                    );
                }
                if self.save_project(false) {
                    self.dialog = None;
                    Ok(
                        json!({ "saved": self.project_path.as_ref().map(|p| p.display().to_string()) }),
                    )
                } else {
                    Err("Saving failed".into())
                }
            }
            "select" => {
                let start = f(a, "start").map_or(0, |t| self.to_samples(t));
                let end = f(a, "end").map_or(self.project.len(), |t| self.to_samples(t));
                self.project.selection = Selection::new(start, end);
                match a.get("tracks") {
                    Some(Value::Array(list)) => {
                        let idx: Vec<usize> = list
                            .iter()
                            .filter_map(Value::as_u64)
                            .map(|x| x as usize)
                            .collect();
                        if idx.iter().any(|&i| i >= self.project.tracks.len()) {
                            return Err("track index out of range".into());
                        }
                        for (i, t) in self.project.tracks.iter_mut().enumerate() {
                            t.selected = idx.contains(&i);
                        }
                    }
                    Some(Value::String(s)) if s == "all" => self
                        .project
                        .tracks
                        .iter_mut()
                        .for_each(|t| t.selected = true),
                    _ => {}
                }
                if b(a, "zoom") {
                    self.zoom_selection();
                }
                Ok(json!({ "selection": self.sel_secs() }))
            }
            "zoom" => {
                match (f(a, "start"), f(a, "end")) {
                    (Some(st), Some(en)) if en > st => {
                        let w = self.last_track_area.width().max(100.0) as f64;
                        let (s0, s1) = (st * r as f64, en * r as f64);
                        self.view = View {
                            left: s0,
                            spp: ((s1 - s0) / w).max(0.01),
                        };
                    }
                    _ => self.zoom_fit(self.last_track_area.width()),
                }
                Ok(json!({ "zoomed": true }))
            }
            "edit" => {
                self.require_idle()?;
                let op = s(a, "op").ok_or("missing `op`")?;
                type Op = fn(&mut Project) -> bool;
                let f: Option<Op> = match op {
                    "cut" => Some(Project::cut),
                    "copy" => Some(Project::copy),
                    "paste" => Some(Project::paste),
                    "delete" => Some(Project::delete),
                    "silence" => Some(Project::silence_selection),
                    "trim" => Some(Project::trim),
                    "duplicate" => Some(Project::duplicate),
                    "split_new" => Some(Project::split_new),
                    "mix_and_render" => Some(Project::mix_and_render),
                    "undo" => Some(Project::undo),
                    "redo" => Some(Project::redo),
                    _ => None,
                };
                let ok = match (op, f) {
                    (_, Some(f)) => {
                        self.engine.stop_playback();
                        f(&mut self.project)
                    }
                    ("select_all", _) => {
                        self.project.select_all();
                        true
                    }
                    ("remove_selected_tracks", _) => {
                        let any = self.project.tracks.iter().any(|t| t.selected);
                        self.remove_selected_tracks();
                        any
                    }
                    _ => return Err(format!("unknown op `{op}`")),
                };
                if !ok {
                    return Err(format!(
                        "`{op}` did nothing (does it need a selection or clipboard?)"
                    ));
                }
                self.status = format!("Agent: {op}");
                Ok(
                    json!({ "done": op, "selection": self.sel_secs(), "duration": secs(r, self.project.len()) }),
                )
            }
            "apply_effects" => {
                self.require_idle()?;
                let effects: Vec<Effect> =
                    serde_json::from_value(a.get("effects").cloned().unwrap_or(json!([])))
                        .map_err(|e| format!("bad effects: {e}"))?;
                if effects.is_empty() {
                    return Err("no effects given".into());
                }
                let mut note = None;
                if effects
                    .iter()
                    .any(|e| matches!(e, Effect::NoiseReduction { .. }))
                    && self
                        .project
                        .noise_profile
                        .as_ref()
                        .is_none_or(|p| p.rate != r)
                {
                    let p = auto_noise_profile(&self.project)
                        .ok_or("Noise reduction needs a noise sample and none could be found automatically.")?;
                    self.project.noise_profile = Some(p);
                    note = Some("noise profile taken from the quietest half-second");
                }
                let label = s(a, "label").map(str::to_string).unwrap_or_else(|| {
                    if effects.len() <= 3 {
                        effects
                            .iter()
                            .map(|e| e.name())
                            .collect::<Vec<_>>()
                            .join(" + ")
                    } else {
                        format!("{} effects", effects.len())
                    }
                });
                self.start_effect_job(label.clone(), effects);
                if self.job.is_none() {
                    // Starting failed; the reason is in the message dialog.
                    let msg = match self.dialog.take() {
                        Some(super::Dialog::Message(m)) => m,
                        _ => "could not start".into(),
                    };
                    return Err(msg);
                }
                Ok(json!({ "started": label, "note": note }))
            }
            "noise_profile" => {
                let (st, en) = (
                    f(a, "start").ok_or("missing `start`")?,
                    f(a, "end").ok_or("missing `end`")?,
                );
                let (s0, s1) = (self.to_samples(st), self.to_samples(en));
                let mut chans = Vec::new();
                for t in self.project.target_tracks() {
                    chans.extend(self.project.tracks[t].extract(s0, s1));
                }
                let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
                self.project.noise_profile =
                    Some(denoise::learn_profile(&refs, r).ok_or("noise sample is too short")?);
                Ok(json!({ "noise_profile": [st, en] }))
            }
            "track" => {
                let i = a
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or("missing `index`")? as usize;
                let t = self
                    .project
                    .tracks
                    .get_mut(i)
                    .ok_or("track index out of range")?;
                if let Some(n) = s(a, "name") {
                    t.name = n.to_string();
                }
                if let Some(g) = f(a, "gain_db") {
                    t.gain = crate::effects::db_to_lin(g.clamp(-60.0, 36.0) as f32);
                }
                if let Some(p) = f(a, "pan") {
                    t.pan = p.clamp(-1.0, 1.0) as f32;
                }
                if let Some(m) = a.get("mute").and_then(Value::as_bool) {
                    t.mute = m;
                }
                if let Some(m) = a.get("solo").and_then(Value::as_bool) {
                    t.solo = m;
                }
                if let Some(m) = a.get("selected").and_then(Value::as_bool) {
                    t.selected = m;
                }
                self.project.dirty = true;
                if self.engine.is_playing() {
                    self.engine.update_gains(&self.project);
                }
                Ok(self.bridge_status()["tracks"][i].clone())
            }
            "play" => {
                if self.busy() {
                    return Err("busy".into());
                }
                let start =
                    f(a, "start").map_or(self.project.selection.start, |t| self.to_samples(t));
                let end = f(a, "end").map(|t| self.to_samples(t));
                self.engine.stop_playback();
                self.engine
                    .play(&self.project, start, end, false)
                    .map_err(|e| format!("{e:#}"))?;
                Ok(json!({ "playing_from": secs(r, start) }))
            }
            "stop" => {
                self.stop();
                Ok(json!({ "stopped": true }))
            }
            "export" => {
                self.require_idle()?;
                let path = PathBuf::from(s(a, "path").ok_or("missing `path`")?);
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .ok_or("the path needs an extension: .wav, .flac, .mp3 or .ogg")?;
                let mut fmt = ExportFormat::from_extension(ext)
                    .ok_or("unsupported extension (use .wav, .flac, .mp3 or .ogg)")?;
                match &mut fmt {
                    ExportFormat::Wav(w) => {
                        *w = match s(a, "wav_format").unwrap_or("pcm16") {
                            "pcm24" => WavFormat::Pcm24,
                            "float32" => WavFormat::Float32,
                            _ => WavFormat::Pcm16,
                        }
                    }
                    ExportFormat::Flac { bits } => {
                        *bits = if s(a, "wav_format") == Some("pcm24") {
                            24
                        } else {
                            16
                        }
                    }
                    ExportFormat::Mp3 { kbps } => {
                        *kbps = f(a, "mp3_kbps").map_or(192, |k| k as u32)
                    }
                    ExportFormat::Ogg { quality } => {
                        *quality = f(a, "ogg_quality").map_or(6, |q| q.clamp(0.0, 10.0) as u8)
                    }
                }
                if self.project.is_empty() {
                    return Err("There is no audio to export.".into());
                }
                self.start_export_job(path.clone(), fmt, b(a, "selection_only"));
                Ok(json!({ "exporting": path.display().to_string(), "format": fmt.label() }))
            }
            "transcribe" => {
                let track = match a.get("track").and_then(Value::as_u64) {
                    Some(i) => i as usize,
                    None => self
                        .project
                        .target_tracks()
                        .first()
                        .copied()
                        .ok_or("no tracks")?,
                };
                let model = match s(a, "model") {
                    Some(m) => Some(WhisperModel::from_id(m).ok_or(format!("unknown model {m}"))?),
                    None => None,
                };
                self.start_transcription(track, model, s(a, "language").map(str::to_string))?;
                Ok(json!({ "transcribing_track": track }))
            }
            "transcript" => {
                let tr = self
                    .project
                    .transcript
                    .clone()
                    .ok_or("There is no transcript yet: run transcribe first.")?;
                let from = f(a, "start").map_or(0, |t| self.to_samples(t));
                let to = f(a, "end").map_or(usize::MAX, |t| self.to_samples(t));
                let words: Vec<Value> = tr
                    .words
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.end > from && w.start < to)
                    .map(|(i, w)| json!([i, secs(r, w.start), secs(r, w.end), w.text]))
                    .collect();
                Ok(json!({
                    "track": self.project.transcript_track(),
                    "language": tr.language,
                    "word_count": tr.words.len(),
                    "words_format": "[index, start_secs, end_secs, text]",
                    "words": words,
                    "text": tr.to_text((1.5 * r as f64) as usize),
                }))
            }
            "delete_words" => {
                self.require_idle()?;
                let tr = self
                    .project
                    .transcript
                    .clone()
                    .ok_or("There is no transcript.")?;
                let (from, to) = match (
                    a.get("from").and_then(Value::as_u64),
                    a.get("to").and_then(Value::as_u64),
                ) {
                    (Some(x), Some(y)) => (x as usize, y as usize),
                    (Some(x), None) => (x as usize, x as usize),
                    _ => return Err("pass word indices `from` and `to` (inclusive)".into()),
                };
                if let Some(all) = a.get("all_tracks").and_then(Value::as_bool) {
                    self.transcript_cut_all(all);
                }
                let removed: Vec<String> = tr
                    .words
                    .get(from..=to.min(tr.words.len().saturating_sub(1)))
                    .unwrap_or(&[])
                    .iter()
                    .map(|w| w.text.clone())
                    .collect();
                let n = self.delete_word_range(from, to)?;
                Ok(
                    json!({ "deleted_words": n, "text": removed.join(" "), "duration": secs(r, self.project.len()) }),
                )
            }
            "remove_fillers" => {
                self.require_idle()?;
                if self.project.transcript.is_none() {
                    return Err("There is no transcript.".into());
                }
                if let Some(all) = a.get("all_tracks").and_then(Value::as_bool) {
                    self.transcript_cut_all(all);
                }
                let n = self.remove_fillers();
                Ok(json!({ "removed": n, "duration": secs(r, self.project.len()) }))
            }
            "auto_duck" => {
                self.require_idle()?;
                let n = self.project.tracks.len();
                let voice = match a.get("voice_track").and_then(Value::as_u64) {
                    Some(v) => v as usize,
                    None => self
                        .project
                        .transcript_track()
                        .ok_or("pass `voice_track` (index of the speech track)")?,
                };
                if voice >= n {
                    return Err("voice_track out of range".into());
                }
                let targets: Vec<usize> = match a.get("duck_tracks").and_then(Value::as_array) {
                    Some(list) => list
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|x| x as usize)
                        .collect(),
                    None => (0..n).filter(|&i| i != voice).collect(),
                };
                let mut params = crate::dsp::duck::DuckParams::default();
                if let Some(v) = f(a, "amount_db") {
                    params.amount_db = -(v.abs() as f32);
                }
                if let Some(v) = f(a, "threshold_db") {
                    params.threshold_db = v as f32;
                }
                if let Some(v) = f(a, "fade_down_ms") {
                    params.fade_down_ms = v as f32;
                }
                if let Some(v) = f(a, "fade_up_ms") {
                    params.fade_up_ms = v as f32;
                }
                if let Some(v) = f(a, "hold_ms") {
                    params.hold_ms = v as f32;
                }
                let use_words = a
                    .get("use_transcript")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let regions = self.run_auto_duck(voice, &targets, &params, use_words)?;
                Ok(
                    json!({ "voice_track": voice, "ducked_tracks": targets, "speech_regions": regions, "amount_db": params.amount_db }),
                )
            }
            "envelope" => {
                let i = a
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or("missing `index`")? as usize;
                if i >= self.project.tracks.len() {
                    return Err("track index out of range".into());
                }
                let pts = a
                    .get("points")
                    .and_then(Value::as_array)
                    .ok_or("pass `points`: [[seconds, dB], …] (empty to clear)")?;
                let mut points = Vec::new();
                for pt in pts {
                    let (Some(t), Some(db)) = (
                        pt.get(0).and_then(Value::as_f64),
                        pt.get(1).and_then(Value::as_f64),
                    ) else {
                        return Err("each point must be [seconds, dB]".into());
                    };
                    points.push(crate::envelope::EnvPoint {
                        pos: (t.max(0.0) * r as f64).round() as usize,
                        db: db as f32,
                    });
                }
                self.project.push_undo("Envelope");
                self.project.tracks[i].envelope = crate::envelope::Envelope::from_points(points);
                Ok(json!({ "index": i, "points": self.project.tracks[i].envelope.points.len() }))
            }
            "spectral" => {
                self.require_idle()?;
                let lo = f(a, "low_hz").ok_or("missing `low_hz`")? as f32;
                let hi = f(a, "high_hz").ok_or("missing `high_hz`")? as f32;
                if hi <= lo {
                    return Err("high_hz must be above low_hz".into());
                }
                let start = self.to_samples(f(a, "start").ok_or("missing `start` (seconds)")?);
                let end = self.to_samples(f(a, "end").ok_or("missing `end` (seconds)")?);
                if end <= start {
                    return Err("end must be after start".into());
                }
                if let Some(ti) = a.get("track").and_then(Value::as_u64) {
                    let ti = ti as usize;
                    if ti >= self.project.tracks.len() {
                        return Err(format!("no track {ti}"));
                    }
                    for (j, t) in self.project.tracks.iter_mut().enumerate() {
                        t.selected = j == ti;
                    }
                }
                let op = match s(a, "action").unwrap_or("delete") {
                    "delete" => crate::dsp::spectral::SpectralOp::Delete,
                    "reduce" => {
                        crate::dsp::spectral::SpectralOp::Reduce(f(a, "db").unwrap_or(12.0) as f32)
                    }
                    "heal" => crate::dsp::spectral::SpectralOp::Heal,
                    "isolate" => crate::dsp::spectral::SpectralOp::Isolate,
                    o => return Err(format!("unknown action {o}")),
                };
                self.project.selection = Selection::new(start, end);
                self.spectral.sel = Some((lo, hi));
                self.engine.stop_playback();
                if !self.project.spectral_edit(lo, hi, op) {
                    return Err("nothing to edit there".into());
                }
                self.status = format!("Agent: {}", op.name());
                Ok(
                    json!({ "applied": op.name(), "start": f(a, "start"), "end": f(a, "end"), "low_hz": lo, "high_hz": hi }),
                )
            }
            "live_fx" => {
                let ti = a
                    .get("track")
                    .and_then(Value::as_u64)
                    .map_or(0, |x| x as usize);
                if ti >= self.project.tracks.len() {
                    return Err(format!("no track {ti}"));
                }
                let slot = a.get("slot").and_then(Value::as_u64).map(|x| x as usize);
                let n = self.project.tracks[ti].live_fx.len();
                let need_slot = || -> Result<usize, String> {
                    let k = slot.ok_or("missing `slot` (see action=list)")?;
                    if k >= n {
                        return Err(format!("track {ti} has no live effect slot {k}"));
                    }
                    Ok(k)
                };
                let effect = || -> Result<Effect, String> {
                    serde_json::from_value(a.get("effect").cloned().ok_or("missing `effect`")?)
                        .map_err(|e| format!("bad effect: {e}"))
                };
                match s(a, "action").unwrap_or("list") {
                    "list" => {}
                    "add" if a.get("plugin").is_some() => {
                        let want = s(a, "plugin")
                            .ok_or("`plugin` must be a name")?
                            .to_lowercase();
                        let fmt = s(a, "format").map(str::to_lowercase);
                        let found = self
                            .plugins
                            .effects()
                            .filter(|d| {
                                fmt.as_deref()
                                    .is_none_or(|f| d.format.label().to_lowercase() == f)
                            })
                            .find(|d| d.name.to_lowercase() == want)
                            .or_else(|| {
                                self.plugins
                                    .effects()
                                    .find(|d| d.name.to_lowercase().contains(&want))
                            })
                            .cloned();
                        let Some(desc) = found else {
                            let names: Vec<String> = self
                                .plugins
                                .effects()
                                .map(|d| format!("{} ({})", d.name, d.format.label()))
                                .collect();
                            return Err(if names.is_empty() {
                                "no plugins found yet: the user can scan in Effect ▸ Plugins…"
                                    .into()
                            } else {
                                format!("no plugin named “{want}”. Installed: {}", names.join(", "))
                            });
                        };
                        self.project.add_live_plugin(ti, desc);
                        // Load it now so its parameters can be listed right away.
                        self.sync_plugins();
                    }
                    "set_param" => {
                        let k = need_slot()?;
                        let id = self.project.tracks[ti].live_fx[k].id;
                        let p = self
                            .plugins
                            .loaded
                            .get_mut(&id)
                            .ok_or("that slot isn't a loaded plugin")?;
                        let params = p.params();
                        let key = a.get("param").ok_or("missing `param` (name or id)")?;
                        let q = params
                            .iter()
                            .find(|q| match key {
                                Value::Number(n) => n.as_u64() == Some(q.id as u64),
                                Value::String(n) => q.name.eq_ignore_ascii_case(n),
                                _ => false,
                            })
                            .ok_or("no such parameter (see action=list)")?;
                        let v = f(a, "value").ok_or("missing `value`")?.clamp(q.min, q.max);
                        p.set_param(q.id, v);
                        self.project.dirty = true;
                    }
                    "add" => {
                        let fx = effect()?;
                        if !crate::dsp::realtime::supports(&fx) {
                            return Err(format!(
                                "{} can't run live; use app_apply_effects for it",
                                fx.name()
                            ));
                        }
                        self.project.add_live_fx(ti, fx);
                    }
                    "set" => {
                        let k = need_slot()?;
                        let fx = effect()?;
                        let slot = &mut self.project.tracks[ti].live_fx[k];
                        if slot.plugin.is_some() {
                            return Err(format!("slot {k} is a plugin: use action=set_param"));
                        }
                        if std::mem::discriminant(&fx) != std::mem::discriminant(&slot.effect) {
                            return Err(format!("slot {k} is {}, not {}", slot.name(), fx.name()));
                        }
                        slot.effect = fx;
                        self.project.dirty = true;
                    }
                    "enable" | "bypass" => {
                        let k = need_slot()?;
                        self.project.push_undo("Toggle Live Effect");
                        self.project.tracks[ti].live_fx[k].enabled =
                            s(a, "action") == Some("enable");
                    }
                    "remove" => {
                        let k = need_slot()?;
                        self.project.push_undo("Remove Live Effect");
                        self.project.tracks[ti].live_fx.remove(k);
                    }
                    "apply" => {
                        self.require_idle()?;
                        self.engine.stop_playback();
                        if !self.project.render_live_fx(ti) {
                            return Err("no enabled live effects on that track".into());
                        }
                    }
                    o => return Err(format!("unknown action {o}")),
                }
                let slots = self.project.tracks[ti].live_fx.clone();
                let mut list: Vec<Value> = Vec::new();
                for (k, f) in slots.iter().enumerate() {
                    match &f.plugin {
                        Some(pl) => {
                            let mut params = Vec::new();
                            if let Some(p) = self.plugins.loaded.get_mut(&f.id) {
                                for q in p.params().iter().filter(|q| !q.is_bypass) {
                                    let text = p.param_text(q.id, q.value).unwrap_or_default();
                                    params.push(json!({ "id": q.id, "name": q.name, "value": q.value, "min": q.min, "max": q.max, "display": text }));
                                }
                            }
                            let error = self.plugins.failed.get(&f.id).cloned();
                            list.push(json!({ "slot": k, "name": f.name(), "enabled": f.enabled,
                                "plugin": { "format": pl.desc.format.label(), "vendor": pl.desc.vendor, "params": params, "error": error } }));
                        }
                        None => list.push(json!({ "slot": k, "name": f.name(), "enabled": f.enabled, "effect": f.effect })),
                    }
                }
                Ok(json!({ "track": ti, "live_fx": list }))
            }
            "clips" => {
                let action = s(a, "action").unwrap_or("list");
                let track = a.get("track").and_then(Value::as_u64).map(|x| x as usize);
                let clip = a.get("clip").and_then(Value::as_u64).map(|x| x as usize);
                let need = |what: Option<usize>, name: &str| {
                    what.ok_or(format!("missing `{name}` (see action=list)"))
                };
                let check = |p: &Project, ti: usize, ci: usize| -> Result<(), String> {
                    let t = p.tracks.get(ti).ok_or(format!("no track {ti}"))?;
                    t.clips
                        .get(ci)
                        .map(|_| ())
                        .ok_or(format!("track {ti} has no clip {ci}"))
                };
                match action {
                    "list" => {}
                    "move" => {
                        self.require_idle()?;
                        let (ti, ci) = (need(track, "track")?, need(clip, "clip")?);
                        check(&self.project, ti, ci)?;
                        let c = self.project.tracks[ti].clips[ci].clone();
                        let to = match (f(a, "to"), f(a, "by")) {
                            (Some(t), _) => self.to_samples(t) as i64,
                            (None, Some(by)) => {
                                c.start as i64 + (by * self.project.rate as f64).round() as i64
                            }
                            _ => {
                                return Err(
                                    "give `to` (new start, seconds) or `by` (seconds)".into()
                                );
                            }
                        };
                        let dest = a
                            .get("to_track")
                            .and_then(Value::as_u64)
                            .map(|x| x as usize);
                        if let Some(d) = dest
                            && d >= self.project.tracks.len()
                        {
                            return Err(format!("no track {d}"));
                        }
                        self.engine.stop_playback();
                        if !self.project.move_clip(ti, ci, to - c.start as i64, dest) {
                            return Err("nothing moved: there is no room there without overlapping another clip \
                                        (clips stop next to each other). Move or delete the other clip first, or pick a gap that fits."
                                .into());
                        }
                    }
                    "rename" => {
                        let (ti, ci) = (need(track, "track")?, need(clip, "clip")?);
                        check(&self.project, ti, ci)?;
                        self.project
                            .rename_clip(ti, ci, s(a, "name").ok_or("missing `name`")?);
                    }
                    "delete" => {
                        self.require_idle()?;
                        let (ti, ci) = (need(track, "track")?, need(clip, "clip")?);
                        check(&self.project, ti, ci)?;
                        self.engine.stop_playback();
                        self.project.delete_clip(ti, ci);
                    }
                    "split" | "join" => {
                        self.require_idle()?;
                        if let Some(at) = f(a, "at") {
                            self.project.selection = Selection::cursor(self.to_samples(at));
                        }
                        if let Some(ti) = track {
                            for (j, t) in self.project.tracks.iter_mut().enumerate() {
                                t.selected = j == ti;
                            }
                        }
                        let ok = if action == "split" {
                            self.project.split_clips()
                        } else {
                            self.project.join_clips()
                        };
                        if !ok {
                            return Err(format!("nothing to {action} at the cursor/selection"));
                        }
                    }
                    o => return Err(format!("unknown action {o}")),
                }
                let r = self.project.rate;
                let list: Vec<Value> = self
                    .project
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| json!({ "track": i, "name": t.name, "clips": clips_json(r, t) }))
                    .collect();
                Ok(json!({ "tracks": list }))
            }
            "labels" => {
                let action = s(a, "action").unwrap_or("list");
                match action {
                    "list" => {}
                    "add" => {
                        let st = self.to_samples(f(a, "start").ok_or("missing `start` (seconds)")?);
                        let en = f(a, "end").map_or(st, |t| self.to_samples(t));
                        let text = s(a, "text").unwrap_or("Label").to_string();
                        self.project.add_label(st, en, text);
                    }
                    "rename" | "delete" => {
                        let id = a
                            .get("id")
                            .and_then(Value::as_u64)
                            .ok_or("missing label `id` (see action=list)")?;
                        if !self.project.labels.iter().any(|l| l.id == id) {
                            return Err(format!("no label with id {id}"));
                        }
                        if action == "delete" {
                            self.delete_label(id);
                        } else {
                            let text = s(a, "text").ok_or("missing `text`")?.to_string();
                            self.project.push_undo("Rename Label");
                            if let Some(l) = self.project.labels.iter_mut().find(|l| l.id == id) {
                                l.text = text;
                            }
                        }
                    }
                    "clear" => {
                        if !self.project.labels.is_empty() {
                            self.project.push_undo("Clear Labels");
                            self.project.labels.clear();
                        }
                    }
                    "export_files" => {
                        self.require_idle()?;
                        let dir = PathBuf::from(s(a, "dir").ok_or("missing `dir`")?);
                        std::fs::create_dir_all(&dir)
                            .map_err(|e| format!("{}: {e}", dir.display()))?;
                        let mut fmt = ExportFormat::from_extension(s(a, "format").unwrap_or("mp3"))
                            .ok_or("format must be wav, flac, mp3 or ogg")?;
                        if let ExportFormat::Mp3 { kbps } = &mut fmt {
                            *kbps = f(a, "mp3_kbps").map_or(192, |k| k as u32);
                        }
                        let n = self.start_export_many(
                            dir.clone(),
                            fmt,
                            b(a, "include_before_first"),
                            a.get("numbered").and_then(Value::as_bool).unwrap_or(true),
                        )?;
                        return Ok(
                            json!({ "exporting_files": n, "dir": dir.display().to_string() }),
                        );
                    }
                    o => return Err(format!("unknown action {o}")),
                }
                let list: Vec<Value> = self
                    .project
                    .labels
                    .iter()
                    .map(|l| json!({ "id": l.id, "start": secs(r, l.start), "end": secs(r, l.end), "text": l.text }))
                    .collect();
                Ok(json!({ "labels": list }))
            }
            "metadata" => {
                let m = &mut self.project.metadata;
                let mut changed = false;
                for (k, slot) in [
                    ("title", &mut m.title),
                    ("artist", &mut m.artist),
                    ("album", &mut m.album),
                    ("album_artist", &mut m.album_artist),
                    ("year", &mut m.year),
                    ("genre", &mut m.genre),
                    ("track", &mut m.track),
                    ("comment", &mut m.comment),
                ] {
                    if let Some(v) = a.get(k) {
                        *slot = match v {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        changed = true;
                    }
                }
                if let Some(p) = s(a, "cover_path") {
                    m.cover = Some(
                        crate::metadata::load_cover(std::path::Path::new(p))
                            .map_err(|e| format!("{e:#}"))?,
                    );
                    changed = true;
                }
                if b(a, "remove_cover") {
                    m.cover = None;
                    changed = true;
                }
                if changed {
                    self.project.dirty = true;
                }
                let m = &self.project.metadata;
                Ok(json!({
                    "title": m.title, "artist": m.artist, "album": m.album, "album_artist": m.album_artist,
                    "year": m.year, "genre": m.genre, "track": m.track, "comment": m.comment,
                    "cover": m.cover.as_ref().map(|c| json!({ "mime": c.mime, "bytes": c.data.len() })),
                    "chapters_from_labels": self.project.labels.len(),
                }))
            }
            "dismiss_dialog" => {
                let had = self.dialog.take().is_some();
                Ok(json!({ "dismissed": had }))
            }
            "show_transcript" => {
                self.transcript.show_panel = a.get("show").and_then(Value::as_bool).unwrap_or(true);
                Ok(json!({ "shown": self.transcript.show_panel }))
            }
            other => Err(format!("unknown command `{other}`")),
        }
    }
}
