//! Speech-to-text with OpenAI's Whisper model.
//!
//! Like Demucs, Whisper runs as an external Python program, so the app stays small and can
//! use a GPU when one is available. A small runner script (embedded below) is written to a
//! temporary folder and started with the configured Python. It uses
//! [faster-whisper](https://github.com/SYSTRAN/faster-whisper) if installed (recommended:
//! fast, word timestamps, no ffmpeg needed) and falls back to the original
//! `openai-whisper` package. It prints one JSON object per line on stdout.

use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::Sender,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    dsp::separation::{CancelHandle, parse_progress, split_command},
    track::{adapt_channels, resample},
};

/// Whisper's native sample rate.
pub const WHISPER_RATE: u32 = 16_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhisperModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3,
    Turbo,
}

impl WhisperModel {
    pub const ALL: [WhisperModel; 6] = [
        WhisperModel::Tiny,
        WhisperModel::Base,
        WhisperModel::Small,
        WhisperModel::Medium,
        WhisperModel::Turbo,
        WhisperModel::LargeV3,
    ];
    pub fn id(self) -> &'static str {
        match self {
            WhisperModel::Tiny => "tiny",
            WhisperModel::Base => "base",
            WhisperModel::Small => "small",
            WhisperModel::Medium => "medium",
            WhisperModel::LargeV3 => "large-v3",
            WhisperModel::Turbo => "turbo",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            WhisperModel::Tiny => "tiny – fastest, rough (75 MB)",
            WhisperModel::Base => "base – fast, OK (145 MB)",
            WhisperModel::Small => "small – good balance (480 MB)",
            WhisperModel::Medium => "medium – accurate, slow on CPU (1.5 GB)",
            WhisperModel::Turbo => "turbo – near-best, fast on a GPU (1.6 GB)",
            WhisperModel::LargeV3 => "large-v3 – best, needs a GPU (3 GB)",
        }
    }
    pub fn from_id(id: &str) -> Option<WhisperModel> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }
}

/// Languages offered in the UI (Whisper supports ~100; any code can be typed via the API).
pub const LANGUAGES: [(&str, &str); 16] = [
    ("", "Detect automatically"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("tr", "Turkish"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
];

/// Default Python command for this platform.
pub fn default_python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[derive(Clone, Debug)]
pub struct WhisperOptions {
    /// Command that starts Python, e.g. `python` or `"C:\Python312\python.exe"`.
    pub python: String,
    pub model: WhisperModel,
    /// Language code, or empty to detect.
    pub language: String,
    /// Extra environment variables for the Python process (used by tests).
    pub env: Vec<(String, String)>,
}

impl Default for WhisperOptions {
    fn default() -> Self {
        Self {
            python: default_python().into(),
            model: WhisperModel::Base,
            language: String::new(),
            env: Vec::new(),
        }
    }
}

/// A recognised word with times in seconds.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedWord {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub prob: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Transcription {
    pub language: String,
    pub engine: String,
    pub words: Vec<TimedWord>,
}

impl Transcription {
    /// Convert to a [`crate::transcript::Transcript`] for a track at `rate`.
    pub fn into_transcript(
        self,
        track_id: u64,
        rate: u32,
        offset: usize,
    ) -> crate::transcript::Transcript {
        let r = rate as f64;
        crate::transcript::Transcript {
            track_id,
            language: self.language,
            words: self
                .words
                .into_iter()
                .map(|w| crate::transcript::Word {
                    start: offset + (w.start * r).round().max(0.0) as usize,
                    end: offset + (w.end * r).round().max(0.0) as usize,
                    text: w.text,
                    prob: w.prob,
                })
                .filter(|w| w.end > w.start)
                .collect(),
        }
    }
}

/// Messages from a running transcription.
pub enum WhisperEvent {
    /// 0–1.
    Progress(f32),
    /// A status line for the user ("Loading model…", download progress, …).
    Status(String),
    Done(std::result::Result<Transcription, String>),
}

/// Prepare audio for Whisper: mono, 16 kHz.
pub fn prepare_audio(channels: &[Vec<f32>], rate: u32) -> Vec<f32> {
    let mono = adapt_channels(channels, 1).pop().unwrap_or_default();
    resample(&[mono], rate, WHISPER_RATE)
        .pop()
        .unwrap_or_default()
}

const RUNNER: &str = include_str!("whisper_runner.py");

/// Transcribe 16 kHz mono audio. Blocks; run it on a worker thread.
pub fn run_whisper(
    opts: &WhisperOptions,
    audio_16k: &[f32],
    events: &Sender<WhisperEvent>,
    cancel: &CancelHandle,
) -> Result<Transcription> {
    if audio_16k.len() < WHISPER_RATE as usize / 10 {
        bail!("there is too little audio to transcribe");
    }
    let dir = std::env::temp_dir().join(format!(
        "rust_audacity_whisper_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    let result = run_in(&dir, opts, audio_16k, events, cancel);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn run_in(
    dir: &Path,
    opts: &WhisperOptions,
    audio: &[f32],
    events: &Sender<WhisperEvent>,
    cancel: &CancelHandle,
) -> Result<Transcription> {
    let script = dir.join("rust_audacity_whisper.py");
    std::fs::write(&script, RUNNER)?;
    let raw = dir.join("audio.f32");
    let mut bytes = Vec::with_capacity(audio.len() * 4);
    for s in audio {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(&raw, bytes)?;

    let parts = split_command(&opts.python);
    let (prog, pre) = parts
        .split_first()
        .ok_or_else(|| anyhow!("no Python command configured"))?;
    let mut cmd = Command::new(prog);
    cmd.args(pre)
        .arg("-u")
        .arg(&script)
        .arg("--audio")
        .arg(&raw)
        .arg("--model")
        .arg(opts.model.id());
    if !opts.language.trim().is_empty() {
        cmd.arg("--language").arg(opts.language.trim());
    }
    cmd.env("PYTHONIOENCODING", "utf-8")
        .envs(opts.env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "could not start Python (`{}`).\nInstall Python 3 from python.org, then run:  pip install -U faster-whisper\n\
             If Python is installed somewhere else, put its full path in the Python box.",
            opts.python
        )
    })?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    *cancel.lock() = Some(child);

    // stderr: model-download / tqdm progress and errors.
    let ev = events.clone();
    let err_thread = std::thread::spawn(move || {
        let mut log = String::new();
        let mut buf = [0u8; 1024];
        let mut pending = String::new();
        let handle = |line: &str, log: &mut String| {
            let line = line.trim();
            if line.is_empty() {
                return;
            }
            if parse_progress(line).is_some() {
                let _ = ev.send(WhisperEvent::Status(format!(
                    "Working: {}",
                    line.chars().take(60).collect::<String>()
                )));
            } else {
                log.push_str(line);
                log.push('\n');
            }
        };
        loop {
            let n = stderr.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(pos) = pending.find(['\r', '\n']) {
                let line: String = pending.drain(..=pos).collect();
                handle(&line, &mut log);
            }
        }
        handle(&pending, &mut log);
        log
    });

    let mut out = Transcription::default();
    let mut error: Option<String> = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("status") => {
                let text = msg["message"].as_str().unwrap_or("").to_string();
                let _ = events.send(WhisperEvent::Status(text));
            }
            Some("info") => {
                out.language = msg["language"].as_str().unwrap_or("").into();
                out.engine = msg["engine"].as_str().unwrap_or("").into();
                let _ = events.send(WhisperEvent::Status(format!(
                    "Transcribing ({}, language: {})…",
                    out.engine, out.language
                )));
            }
            Some("progress") => {
                let p = msg["value"].as_f64().unwrap_or(0.0) as f32;
                let _ = events.send(WhisperEvent::Progress(p.clamp(0.0, 1.0)));
            }
            Some("words") => {
                if let Some(ws) = msg["words"].as_array() {
                    for w in ws {
                        let a = w.as_array().map(Vec::as_slice).unwrap_or(&[]);
                        if a.len() < 3 {
                            continue;
                        }
                        let text = a[2].as_str().unwrap_or("").trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        out.words.push(TimedWord {
                            start: a[0].as_f64().unwrap_or(0.0),
                            end: a[1].as_f64().unwrap_or(0.0),
                            text,
                            prob: a.get(3).and_then(Value::as_f64).unwrap_or(1.0) as f32,
                        });
                    }
                }
            }
            Some("error") => error = msg["message"].as_str().map(str::to_string),
            _ => {}
        }
    }
    let log = err_thread.join().unwrap_or_default();
    let status = cancel.lock().take().map(|mut c| c.wait()).transpose()?;
    match status {
        None => bail!("Transcription was cancelled"),
        Some(_) if error.is_some() => bail!("{}", error.unwrap()),
        Some(s) if !s.success() => {
            let tail: Vec<&str> = log.lines().rev().take(8).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            bail!("Whisper failed ({s}).\n{}", tail.join("\n"));
        }
        Some(_) => {}
    }
    out.words.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `faster_whisper` package that returns fixed words, so the whole
    /// pipeline (script, JSON protocol, progress) is tested without the real model.
    pub(crate) fn fake_faster_whisper(dir: &Path) {
        let pkg = dir.join("faster_whisper");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("__init__.py"),
            r#"
class _W:
    def __init__(s, start, end, word, probability=0.9):
        s.start, s.end, s.word, s.probability = start, end, word, probability
class _S:
    def __init__(s, start, end, words):
        s.start, s.end, s.words = start, end, words
        s.text = "".join(w.word for w in words)
class _Info:
    language = "en"
    duration = 3.0
class WhisperModel:
    def __init__(self, name, device="auto", compute_type="default"):
        self.name = name
    def transcribe(self, audio, language=None, word_timestamps=False, **kw):
        assert word_timestamps
        assert len(audio) > 1000, len(audio)
        segs = [
            _S(0.0, 1.0, [_W(0.10, 0.50, " Hello"), _W(0.55, 0.70, " um,")]),
            _S(1.0, 3.0, [_W(1.20, 1.60, " world."), _W(2.0, 2.5, " Again")]),
        ]
        return iter(segs), _Info()
"#,
        )
        .unwrap();
    }

    #[test]
    fn whisper_plumbing_with_fake_package() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let has_numpy = Command::new("python3")
            .args(["-c", "import numpy"])
            .status()
            .is_ok_and(|s| s.success());
        if !has_numpy {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ra_fake_whisper_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        fake_faster_whisper(&dir);
        let opts = WhisperOptions {
            python: "python3".into(),
            model: WhisperModel::Tiny,
            language: String::new(),
            env: vec![("PYTHONPATH".into(), dir.display().to_string())],
        };
        let audio = prepare_audio(&[vec![0.1; 44100 * 3], vec![0.1; 44100 * 3]], 44100);
        assert_eq!(audio.len(), 48000);
        let (tx, rx) = std::sync::mpsc::channel();
        let t = run_whisper(&opts, &audio, &tx, &CancelHandle::default()).unwrap();
        assert_eq!(t.language, "en");
        assert_eq!(t.engine, "faster-whisper");
        let words: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, vec!["Hello", "um,", "world.", "Again"]);
        assert!((t.words[2].start - 1.2).abs() < 1e-9);
        let progress: Vec<f32> = rx
            .try_iter()
            .filter_map(|e| match e {
                WhisperEvent::Progress(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(progress.last().copied(), Some(1.0));
        let tr = t.into_transcript(7, 1000, 50);
        assert_eq!((tr.words[0].start, tr.words[0].end), (150, 550));

        // Without any whisper package installed the error explains what to install.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let opts2 = WhisperOptions {
            env: vec![
                ("PYTHONPATH".into(), empty.display().to_string()),
                ("RUST_AUDACITY_WHISPER_ENGINES".into(), "none".into()),
            ],
            ..opts.clone()
        };
        let err = run_whisper(&opts2, &audio, &tx, &CancelHandle::default()).unwrap_err();
        assert!(format!("{err:#}").contains("pip install"), "{err:#}");

        // A missing Python gives a helpful error.
        let opts3 = WhisperOptions {
            python: "definitely-not-python".into(),
            ..opts
        };
        let err = run_whisper(&opts3, &audio, &tx, &CancelHandle::default()).unwrap_err();
        assert!(format!("{err:#}").contains("faster-whisper"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
