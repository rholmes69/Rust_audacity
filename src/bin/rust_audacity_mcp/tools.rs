//! MCP tool definitions and implementations.

use std::path::{Path, PathBuf};

use rust_audacity::{
    dsp::{
        analysis::{self, Stats},
        denoise,
        separation::{self, CancelHandle, DemucsModel, StemEvent},
        specgram,
    },
    effects::{self, Effect, Generator, NoiseKind, Waveform, builtin_chains},
    fileio::{self, WavFormat},
    project::{Project, Selection},
    track::{Track, adapt_channels, resample},
};
use serde_json::{Map, Value, json};

use crate::{Config, Progress};

type R<T> = Result<T, String>;

/// What a tool returns: a human-readable summary, machine-readable JSON, and optionally a PNG.
pub struct Output {
    pub text: String,
    pub data: Value,
    pub png: Option<Vec<u8>>,
}

const TOOL_NAMES: [&str; 10] = [
    "audio_info",
    "analyze_audio",
    "list_effects",
    "apply_effects",
    "edit_audio",
    "convert_audio",
    "mix_audio",
    "generate_audio",
    "separate_stems",
    "render_spectrogram",
];

pub fn exists(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

// ---------------------------------------------------------------- definitions

fn tool(
    name: &str,
    title: &str,
    description: &str,
    props: Value,
    required: &[&str],
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
        "annotations": { "title": title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": true, "openWorldHint": false },
    })
}

fn p_path(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}
fn p_num(desc: &str) -> Value {
    json!({ "type": "number", "description": desc })
}
fn p_format() -> Value {
    json!({ "type": "string", "enum": ["pcm16", "pcm24", "float32"], "description": "WAV sample format of the output (default pcm24)." })
}

pub fn definitions() -> Vec<Value> {
    let range = |extra: Value| {
        let mut m = extra.as_object().cloned().unwrap_or_default();
        m.insert(
            "start".into(),
            p_num("Start of the time range in seconds (default: beginning)."),
        );
        m.insert(
            "end".into(),
            p_num("End of the time range in seconds (default: end of file)."),
        );
        Value::Object(m)
    };
    let effect_item = json!({
        "oneOf": [
            { "type": "string", "description": "Effect id with default settings, e.g. \"fade_in\"." },
            { "type": "object", "properties": {
                "effect": { "type": "string", "description": "Effect id or name from list_effects, e.g. \"compressor\", \"noise_reduction\", \"graphic_eq\"." },
                "params": { "type": "object", "description": "Parameters to override; omitted ones use defaults. See list_effects." }
              }, "required": ["effect"] }
        ]
    });
    vec![
        tool(
            "audio_info",
            "Audio file info",
            "Read an audio file (WAV, MP3, FLAC, OGG, M4A/AAC, AIFF…) and report duration, sample rate, channels, peak, true peak, RMS and loudness (LUFS).",
            json!({ "path": p_path("Audio file to inspect.") }),
            &["path"],
            true,
            false,
        ),
        tool(
            "analyze_audio",
            "Analyze audio",
            "In-depth analysis of an audio file (or a time range): loudness (LUFS, LRA, true peak), RMS, clipping, DC offset, noise floor, \
             silent regions, frequency-band balance, dominant frequencies with musical notes, mains-hum detection, plus a list of detected \
             issues and suggested effects (ready to pass to apply_effects).",
            range(json!({
                "path": p_path("Audio file to analyze."),
                "top_peaks": { "type": "integer", "description": "How many dominant frequencies to list (default 8)." },
                "silence_threshold_db": p_num("Level below which audio counts as silence (default -50 dBFS)."),
            })),
            &["path"],
            true,
            false,
        ),
        tool(
            "list_effects",
            "List effects",
            "List every effect with its id, category, description and default parameters, plus the built-in effect chains. Use the ids and parameter names with apply_effects.",
            json!({}),
            &[],
            true,
            false,
        ),
        tool(
            "apply_effects",
            "Apply effects",
            "Apply one or more effects in order (or a built-in chain) to an audio file and write the result to a new WAV file. \
             Covers cleanup (noise_reduction, click_removal, hum_removal, de_esser, clip_repair), dynamics (compressor, limiter, noise_gate, \
             loudness_normalize), EQ & tone (graphic_eq, bass_treble, low_pass, high_pass, distortion), time & pitch (change_tempo, change_pitch, \
             change_speed), space (reverb, stereo_widener, vocal_remover) and basics (amplify, normalize, fade_in, fade_out, reverse, invert, echo). \
             Optionally limit to a time range. For noise_reduction a noise profile is taken from noise_profile (a range of the same file) or, \
             if omitted, from the quietest half-second automatically. Returns before/after loudness.",
            range(json!({
                "input": p_path("Source audio file."),
                "output": p_path("Output WAV path (default: <input>_processed.wav next to the input)."),
                "effects": { "type": "array", "items": effect_item, "description": "Effects to apply in order." },
                "chain": { "type": "string", "description": "Name of a built-in chain from list_effects (e.g. \"Podcast Voice\"). Applied before `effects`." },
                "noise_profile": { "type": "object", "properties": { "start": p_num("seconds"), "end": p_num("seconds") }, "description": "Noise-only range of the input used by noise_reduction." },
                "format": p_format(),
                "overwrite": { "type": "boolean", "description": "Allow writing over the input file (default false)." },
            })),
            &["input"],
            false,
            false,
        ),
        tool(
            "edit_audio",
            "Edit audio",
            "Cut-and-splice editing, writing a new WAV file. Operations run in order; times refer to the audio as it is at that step. \
             ops: trim {start,end} keep only that range · delete {start,end} remove a range · silence {start,end} · insert_silence {at,duration} · \
             fade_in {duration} · fade_out {duration} · gain {db, start?, end?} · reverse {start?, end?} · \
             remove_silence {threshold_db=-50, min_duration=0.5, keep=0.2} shortens long pauses.",
            json!({
                "input": p_path("Source audio file."),
                "output": p_path("Output WAV path (default: <input>_edited.wav)."),
                "operations": { "type": "array", "items": { "type": "object", "properties": {
                    "op": { "type": "string", "enum": ["trim", "delete", "silence", "insert_silence", "fade_in", "fade_out", "gain", "reverse", "remove_silence"] },
                    "start": p_num("seconds"), "end": p_num("seconds"), "at": p_num("seconds"), "duration": p_num("seconds"),
                    "db": p_num("gain in dB"), "threshold_db": p_num("silence threshold (remove_silence)"),
                    "min_duration": p_num("shortest pause to shorten, seconds"), "keep": p_num("seconds of pause to keep")
                  }, "required": ["op"] } },
                "format": p_format(),
                "overwrite": { "type": "boolean" },
            }),
            &["input", "operations"],
            false,
            false,
        ),
        tool(
            "convert_audio",
            "Convert audio",
            "Convert any supported audio file (MP3, FLAC, OGG, M4A, AIFF, WAV…) to WAV, optionally changing sample rate, channel count, bit depth, or normalizing loudness.",
            json!({
                "input": p_path("Source audio file."),
                "output": p_path("Output WAV path (default: <input>.wav or <input>_converted.wav)."),
                "sample_rate": { "type": "integer", "description": "New sample rate in Hz, e.g. 44100 or 48000." },
                "channels": { "type": "integer", "enum": [1, 2], "description": "1 = mono, 2 = stereo." },
                "normalize_lufs": p_num("Normalize integrated loudness to this LUFS value (e.g. -14), with a -1 dB peak limiter."),
                "format": p_format(),
                "overwrite": { "type": "boolean" },
            }),
            &["input"],
            false,
            false,
        ),
        tool(
            "mix_audio",
            "Mix audio files",
            "Mix several audio files into one stereo WAV (e.g. recombine stems at new levels, or lay music under a voice-over). Each input can have a gain, pan and start offset.",
            json!({
                "inputs": { "type": "array", "items": { "type": "object", "properties": {
                    "path": p_path("Audio file."), "gain_db": p_num("Level change in dB (default 0)."),
                    "pan": p_num("-1 left … 0 centre … 1 right (default 0)."), "offset": p_num("Start time in the mix, seconds (default 0)."),
                    "mute": { "type": "boolean" }
                  }, "required": ["path"] } },
                "output": p_path("Output WAV path."),
                "normalize_peak_db": p_num("If set, scale the mix so its peak is this many dBFS (e.g. -1)."),
                "format": p_format(),
            }),
            &["inputs", "output"],
            false,
            false,
        ),
        tool(
            "generate_audio",
            "Generate audio",
            "Create a test tone, noise or silence as a WAV file.",
            json!({
                "output": p_path("Output WAV path."),
                "kind": { "type": "string", "enum": ["tone", "noise", "silence"] },
                "duration": p_num("Length in seconds."),
                "frequency": p_num("Tone frequency in Hz (default 440)."),
                "waveform": { "type": "string", "enum": ["sine", "square", "sawtooth", "triangle"] },
                "noise_type": { "type": "string", "enum": ["white", "pink", "brown"] },
                "amplitude": p_num("0–1 (default 0.5)."),
                "sample_rate": { "type": "integer", "description": "Default 44100." },
                "channels": { "type": "integer", "enum": [1, 2] },
                "format": p_format(),
            }),
            &["output", "kind", "duration"],
            false,
            false,
        ),
        tool(
            "separate_stems",
            "Separate stems",
            "Split a song into stems as separate WAV files. engine \"demucs\" (default) uses Meta's Demucs AI model: 4 stems (vocals, drums, bass, other), \
             6 stems with model htdemucs_6s (adds guitar, piano), or two_stems=true for vocals + no_vocals; it needs `pip install -U demucs` on this computer \
             and can take about as long as the song on a CPU (progress is reported). engine \"builtin\" is instant and needs nothing installed: \
             vocals + instrumental by spectral centre extraction (stereo input with a centre-panned vocal).",
            range(json!({
                "input": p_path("Song to separate."),
                "output_dir": p_path("Folder for the stem files (default: <input>_stems next to the input)."),
                "engine": { "type": "string", "enum": ["demucs", "builtin"] },
                "model": { "type": "string", "enum": ["htdemucs", "htdemucs_ft", "htdemucs_6s"], "description": "Demucs model (default htdemucs)." },
                "two_stems": { "type": "boolean", "description": "Demucs: only vocals + no_vocals." },
                "demucs_command": { "type": "string", "description": "Override the command used to run Demucs, e.g. \"python -m demucs\"." },
                "format": p_format(),
            })),
            &["input"],
            false,
            false,
        ),
        tool(
            "render_spectrogram",
            "Render spectrogram",
            "Render a spectrogram (log-frequency heat map) of an audio file or time range as a PNG, returned as an image you can look at and saved to disk. \
             x = time, y = frequency from 40 Hz (bottom) to Nyquist (top) with faint guide lines at 100 Hz, 1 kHz and 10 kHz; brighter = louder.",
            range(json!({
                "input": p_path("Audio file."),
                "output": p_path("PNG path (default: <input>_spectrogram.png)."),
                "width": { "type": "integer", "description": "Pixels, default 1200." },
                "height": { "type": "integer", "description": "Pixels, default 400." },
                "channel": { "type": "string", "enum": ["mix", "left", "right"], "description": "Default mix." },
            })),
            &["input"],
            false,
            false,
        ),
    ]
}

pub fn to_result(r: R<Output>) -> Value {
    match r {
        Ok(o) => {
            let mut content = vec![json!({ "type": "text", "text": o.text })];
            if let Some(png) = o.png {
                content.push(
                    json!({ "type": "image", "data": base64(&png), "mimeType": "image/png" }),
                );
            }
            json!({ "content": content, "structuredContent": o.data, "isError": false })
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

pub fn call(
    name: &str,
    args: &Value,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancelHandle,
) -> R<Output> {
    match name {
        "audio_info" => audio_info(args, cfg),
        "analyze_audio" => analyze_audio(args, cfg),
        "list_effects" => list_effects(),
        "apply_effects" => apply_effects(args, cfg, progress),
        "edit_audio" => edit_audio(args, cfg),
        "convert_audio" => convert_audio(args, cfg),
        "mix_audio" => mix_audio(args, cfg),
        "generate_audio" => generate_audio(args, cfg),
        "separate_stems" => separate_stems(args, cfg, progress, cancel),
        "render_spectrogram" => render_spectrogram(args, cfg),
        _ => Err(format!("unknown tool {name}")),
    }
}

// ---------------------------------------------------------------- helpers

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}
fn f(a: &Value, k: &str) -> Option<f64> {
    a.get(k).and_then(Value::as_f64)
}
fn b(a: &Value, k: &str) -> bool {
    a.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn req_str<'a>(a: &'a Value, k: &str) -> R<&'a str> {
    s(a, k).ok_or_else(|| format!("missing required argument `{k}`"))
}

fn check_root(cfg: &Config, p: &Path) -> R<()> {
    if let Some(root) = &cfg.root
        && !p.starts_with(root)
    {
        return Err(format!(
            "{} is outside the allowed folder {}",
            p.display(),
            root.display()
        ));
    }
    Ok(())
}

fn absolutize(cfg: &Config, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else if let Some(root) = &cfg.root {
        root.join(p)
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn input_path(cfg: &Config, raw: &str) -> R<PathBuf> {
    let p = absolutize(cfg, raw);
    let p = std::fs::canonicalize(&p).map_err(|e| format!("cannot open {}: {e}", p.display()))?;
    check_root(cfg, &p)?;
    if !p.is_file() {
        return Err(format!("{} is not a file", p.display()));
    }
    Ok(p)
}

fn output_path(cfg: &Config, raw: &str, ext: &str) -> R<PathBuf> {
    let mut p = absolutize(cfg, raw);
    if p.extension().is_none() {
        p.set_extension(ext);
    }
    let parent = p.parent().map(Path::to_path_buf).unwrap_or_default();
    if !parent.exists() {
        // Only create folders inside the allowed root.
        let mut existing = parent.clone();
        while !existing.exists() {
            existing = existing
                .parent()
                .map(Path::to_path_buf)
                .ok_or("invalid output path")?;
        }
        check_root(
            cfg,
            &std::fs::canonicalize(&existing).map_err(|e| e.to_string())?,
        )?;
        std::fs::create_dir_all(&parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let parent =
        std::fs::canonicalize(&parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let full = parent.join(p.file_name().ok_or("output path needs a file name")?);
    check_root(cfg, &full)?;
    Ok(full)
}

/// Resolve `output` (or a default next to the input) and refuse to clobber the input by accident.
fn out_for(cfg: &Config, args: &Value, input: &Path, suffix: &str, ext: &str) -> R<PathBuf> {
    let out = match s(args, "output") {
        Some(o) => output_path(cfg, o, ext)?,
        None => {
            let stem = input.file_stem().unwrap_or_default().to_string_lossy();
            input.with_file_name(format!("{stem}{suffix}.{ext}"))
        }
    };
    if out == input && !b(args, "overwrite") {
        return Err(
            "output is the same file as the input; pass overwrite=true to replace it".into(),
        );
    }
    Ok(out)
}

fn wav_format(args: &Value) -> R<WavFormat> {
    match s(args, "format").unwrap_or("pcm24") {
        "pcm16" => Ok(WavFormat::Pcm16),
        "pcm24" => Ok(WavFormat::Pcm24),
        "float32" => Ok(WavFormat::Float32),
        o => Err(format!("unknown format {o} (use pcm16, pcm24 or float32)")),
    }
}

struct Audio {
    chans: Vec<Vec<f32>>,
    rate: u32,
}

impl Audio {
    fn len(&self) -> usize {
        self.chans.first().map_or(0, Vec::len)
    }
    fn secs(&self) -> f64 {
        self.len() as f64 / self.rate as f64
    }
}

fn load(path: &Path) -> R<Audio> {
    let d = fileio::import(path).map_err(|e| format!("{e:#}"))?;
    Ok(Audio {
        chans: d.channels,
        rate: d.rate,
    })
}

fn save(path: &Path, chans: &[Vec<f32>], rate: u32, fmt: WavFormat) -> R<()> {
    fileio::export_wav(path, chans, rate, fmt).map_err(|e| format!("{e:#}"))
}

/// Seconds → sample range, clamped. Errors if the range is empty.
fn range(args: &Value, rate: u32, len: usize) -> R<(usize, usize)> {
    let to = |t: f64| ((t.max(0.0) * rate as f64).round() as usize).min(len);
    let start = f(args, "start").map_or(0, to);
    let end = f(args, "end").map_or(len, to);
    if end <= start {
        return Err(format!(
            "empty time range (start {:.3}s, end {:.3}s, file is {:.3}s)",
            start as f64 / rate as f64,
            end as f64 / rate as f64,
            len as f64 / rate as f64
        ));
    }
    Ok((start, end))
}

fn db(x: f32) -> Value {
    if x.is_finite() {
        json!((x * 100.0).round() / 100.0)
    } else {
        Value::Null
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn stats_json(st: &Stats) -> Value {
    json!({
        "duration_secs": round3(st.duration_secs),
        "peak_dbfs": db(st.peak_db),
        "true_peak_dbtp": db(st.true_peak_db),
        "rms_dbfs": db(st.rms_db),
        "integrated_lufs": st.lufs.map(|v| (v * 100.0).round() / 100.0),
        "loudness_range_lu": st.lra.map(|v| (v * 100.0).round() / 100.0),
        "crest_factor_db": db(st.crest_db),
        "dc_offset": (st.dc_offset * 1e5).round() / 1e5,
    })
}

fn summary_line(st: &Stats) -> String {
    format!(
        "{:.2}s, peak {:.1} dBFS, true peak {:.1} dBTP, RMS {:.1} dBFS, loudness {}",
        st.duration_secs,
        st.peak_db,
        st.true_peak_db,
        st.rms_db,
        st.lufs
            .map_or("n/a (too short/quiet)".into(), |v| format!("{v:.1} LUFS"))
    )
}

fn mono(chans: &[Vec<f32>]) -> Vec<f32> {
    adapt_channels(chans, 1)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn note_name(freq: f64) -> String {
    if freq < 16.0 {
        return String::new();
    }
    const N: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let midi = (69.0 + 12.0 * (freq / 440.0).log2()).round() as i32;
    format!("{}{}", N[midi.rem_euclid(12) as usize], midi / 12 - 1)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

// ---------------------------------------------------------------- effect specs

fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn snake(variant: &str) -> String {
    let mut out = String::new();
    for (i, c) in variant.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn variant_and_params(fx: &Effect) -> (String, Map<String, Value>) {
    match serde_json::to_value(fx).unwrap_or(Value::Null) {
        Value::String(v) => (v, Map::new()),
        Value::Object(m) => {
            let (k, v) = m.into_iter().next().unwrap();
            (k, v.as_object().cloned().unwrap_or_default())
        }
        _ => (String::new(), Map::new()),
    }
}

fn effect_id(fx: &Effect) -> String {
    snake(&variant_and_params(fx).0)
}

fn describe(fx: &Effect) -> &'static str {
    match fx {
        Effect::Amplify { .. } => "Change the level by `db` decibels.",
        Effect::Normalize { .. } => {
            "Scale so the highest peak reaches `peak_db` dBFS; optionally remove DC offset."
        }
        Effect::FadeIn => "Fade in linearly over the range.",
        Effect::FadeOut => "Fade out linearly over the range.",
        Effect::Reverse => "Play the range backwards.",
        Effect::Invert => "Flip the polarity.",
        Effect::Echo { .. } => "Repeating echo with delay and decay (0–0.95).",
        Effect::LowPass { .. } => "Remove content above the cutoff (12 dB/octave).",
        Effect::HighPass { .. } => {
            "Remove content below the cutoff, e.g. 80 Hz rumble (12 dB/octave)."
        }
        other => other.help(),
    }
}

fn parse_effect(spec: &Value) -> R<Effect> {
    let (name, params) = match spec {
        Value::String(n) => (n.clone(), Map::new()),
        Value::Object(m) => {
            let name = m
                .get("effect")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .ok_or("each effect needs an `effect` name")?
                .to_string();
            let mut params = m
                .get("params")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Also accept parameters given directly next to `effect`.
            for (k, v) in m {
                if k != "effect" && k != "name" && k != "params" {
                    params.insert(k.clone(), v.clone());
                }
            }
            (name, params)
        }
        _ => return Err("effects must be strings or objects".into()),
    };
    let wanted = norm(&name);
    let base = Effect::catalog()
        .into_iter()
        .find(|fx| norm(&effect_id(fx)) == wanted || norm(fx.name()) == wanted)
        .ok_or_else(|| {
            format!(
                "unknown effect `{name}`. Valid ids: {}",
                Effect::catalog()
                    .iter()
                    .map(effect_id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let (variant, mut defaults) = variant_and_params(&base);
    if defaults.is_empty() {
        if !params.is_empty() {
            return Err(format!("{} takes no parameters", effect_id(&base)));
        }
        return Ok(base);
    }
    for (k, v) in params {
        if !defaults.contains_key(&k) {
            return Err(format!(
                "unknown parameter `{k}` for {}. Valid: {}",
                effect_id(&base),
                defaults.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        let v = match (k.as_str(), v) {
            // Accept "soft"/"SOFT" for enum values.
            ("kind", Value::String(s)) => {
                let mut c = s.to_lowercase();
                if let Some(first) = c.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                Value::String(c)
            }
            (_, v) => v,
        };
        defaults.insert(k, v);
    }
    let mut obj = Map::new();
    obj.insert(variant, Value::Object(defaults));
    serde_json::from_value(Value::Object(obj))
        .map_err(|e| format!("invalid parameters for {}: {e}", effect_id(&base)))
}

// ---------------------------------------------------------------- tools

fn audio_info(args: &Value, cfg: &Config) -> R<Output> {
    let path = input_path(cfg, req_str(args, "path")?)?;
    let a = load(&path)?;
    let st = analysis::measure(&a.chans, a.rate);
    let mut data = stats_json(&st);
    let m = data.as_object_mut().unwrap();
    m.insert("path".into(), json!(path.display().to_string()));
    m.insert(
        "format".into(),
        json!(path.extension().map(|e| e.to_string_lossy().to_lowercase())),
    );
    m.insert("sample_rate".into(), json!(a.rate));
    m.insert("channels".into(), json!(a.chans.len()));
    m.insert("frames".into(), json!(a.len()));
    m.insert(
        "file_bytes".into(),
        json!(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)),
    );
    let text = format!(
        "{}: {} Hz, {} channel(s), {}",
        path.display(),
        a.rate,
        a.chans.len(),
        summary_line(&st)
    );
    Ok(Output {
        text,
        data,
        png: None,
    })
}

/// RMS (dBFS) of consecutive windows.
fn window_levels(m: &[f32], win: usize) -> Vec<f32> {
    m.chunks(win.max(1))
        .map(|c| {
            10.0 * ((c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).max(1e-12)).log10()
        })
        .collect()
}

/// Silent regions (start, end) in samples.
fn silent_regions(m: &[f32], rate: u32, threshold_db: f32, min_secs: f64) -> Vec<(usize, usize)> {
    let win = (rate as f64 * 0.02) as usize; // 20 ms
    let levels = window_levels(m, win);
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &l) in levels.iter().chain(std::iter::once(&0.0)).enumerate() {
        let silent = l < threshold_db && i < levels.len();
        match (silent, start) {
            (true, None) => start = Some(i),
            (false, Some(s0)) => {
                let (a, b) = (s0 * win, (i * win).min(m.len()));
                if (b - a) as f64 / rate as f64 >= min_secs {
                    out.push((a, b));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

fn analyze_audio(args: &Value, cfg: &Config) -> R<Output> {
    let path = input_path(cfg, req_str(args, "path")?)?;
    let a = load(&path)?;
    let (s0, e0) = range(args, a.rate, a.len())?;
    let chans: Vec<Vec<f32>> = a.chans.iter().map(|c| c[s0..e0].to_vec()).collect();
    let rate = a.rate;
    let st = analysis::measure(&chans, rate);
    let m = mono(&chans);
    let silence_db = f(args, "silence_threshold_db").unwrap_or(-50.0) as f32;

    // Clipping.
    let total = (chans.len() * m.len()).max(1);
    let clipped = chans.iter().flatten().filter(|v| v.abs() >= 0.999).count();
    // Noise floor: 10th percentile of 50 ms window levels (ignoring digital silence).
    let mut levels: Vec<f32> = window_levels(&m, (rate as f64 * 0.05) as usize)
        .into_iter()
        .filter(|l| *l > -110.0)
        .collect();
    levels.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let noise_floor = levels.get(levels.len() / 10).copied();
    let silences = silent_regions(&m, rate, silence_db, 0.3);
    let silent_secs: f64 = silences
        .iter()
        .map(|(x, y)| (y - x) as f64 / rate as f64)
        .sum();

    // Spectrum, bands and peaks.
    let n = 8192;
    let spec = analysis::spectrum(&m, n);
    let bin_hz = rate as f64 / n as f64;
    let bands = [
        ("sub_20_60", 20.0, 60.0),
        ("bass_60_250", 60.0, 250.0),
        ("low_mid_250_500", 250.0, 500.0),
        ("mid_500_2k", 500.0, 2000.0),
        ("high_mid_2k_4k", 2000.0, 4000.0),
        ("presence_4k_6k", 4000.0, 6000.0),
        ("brilliance_6k_20k", 6000.0, 20000.0),
    ];
    let power = |lo: f64, hi: f64| -> f64 {
        let (b0, b1) = (
            (lo / bin_hz).ceil() as usize,
            ((hi / bin_hz).floor() as usize).min(spec.len() - 1),
        );
        (b0..=b1.max(b0))
            .map(|i| 10f64.powf(spec[i.min(spec.len() - 1)] as f64 / 10.0))
            .sum()
    };
    let total_p: f64 = bands
        .iter()
        .map(|(_, lo, hi)| power(*lo, *hi))
        .sum::<f64>()
        .max(1e-20);
    let mut band_json = Map::new();
    let mut band_pct = Vec::new();
    for (name, lo, hi) in bands {
        let pct = power(lo, hi) / total_p * 100.0;
        band_pct.push((name, pct));
        band_json.insert(name.into(), json!((pct * 10.0).round() / 10.0));
    }
    // Local maxima, at least 6 dB above neighbours ±8 bins.
    let mut peaks: Vec<(usize, f32)> = (9..spec.len().saturating_sub(9))
        .filter(|&i| {
            let v = spec[i];
            v > -80.0 && (i - 8..=i + 8).all(|j| j == i || spec[j] <= v) && {
                let nb = (spec[i - 8] + spec[i + 8]) / 2.0;
                v - nb > 6.0
            }
        })
        .map(|i| (i, spec[i]))
        .collect();
    peaks.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap());
    let top = args.get("top_peaks").and_then(Value::as_u64).unwrap_or(8) as usize;
    let peaks_json: Vec<Value> = peaks.iter().take(top).map(|&(i, v)| {
        let hz = i as f64 * bin_hz;
        json!({ "hz": hz.round(), "note": note_name(hz), "level_db": (v * 10.0).round() / 10.0 })
    }).collect();
    // Mains hum: narrow peak at 50/60 Hz (+ harmonic) standing out from neighbours.
    // High-resolution spectrum (≈1.3 Hz bins at 44.1 kHz) for narrow mains-hum lines.
    let hn = 32768;
    let hspec = if m.len() >= hn / 2 {
        analysis::spectrum(&m, hn)
    } else {
        Vec::new()
    };
    let hbin = rate as f64 / hn as f64;
    let prominence = |hz: f64| -> f32 {
        let i = (hz / hbin).round() as usize;
        if hspec.is_empty() || i + 13 >= hspec.len() || i < 13 {
            return 0.0;
        }
        let peak = hspec[i - 1..=i + 1]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        if peak < -80.0 {
            return 0.0;
        }
        let mut around: Vec<f32> = (i - 12..i - 4)
            .chain(i + 5..i + 13)
            .map(|j| hspec[j])
            .collect();
        around.sort_by(|x, y| x.partial_cmp(y).unwrap());
        peak - around[around.len() / 2]
    };
    // A narrow peak at the mains frequency; a matching 2nd harmonic makes it more certain.
    let hum_score = |base: f64| {
        let (p1, p2) = (prominence(base), prominence(base * 2.0));
        if p2 > 6.0 { p1 } else { p1 - 10.0 }
    };
    let hum50 = hum_score(50.0);
    let hum60 = hum_score(60.0);

    // Issues and suggestions.
    let mut issues = Vec::new();
    let mut suggest: Vec<Value> = Vec::new();
    let clip_pct = clipped as f64 / total as f64 * 100.0;
    if clip_pct > 0.005 {
        issues.push(format!(
            "clipping: {clipped} samples ({clip_pct:.3}%) at full scale"
        ));
        suggest.push(
            json!({ "effect": "clip_repair", "params": { "threshold": 0.98, "reduce_db": 6.0 } }),
        );
    }
    let hum = if hum60 > 15.0 && hum60 >= hum50 {
        Some(60.0)
    } else if hum50 > 15.0 {
        Some(50.0)
    } else {
        None
    };
    if let Some(h) = hum {
        issues.push(format!(
            "mains hum at {h} Hz ({:.0} dB above surrounding spectrum)",
            hum60.max(hum50)
        ));
        suggest.push(json!({ "effect": "hum_removal", "params": { "base_hz": h } }));
    }
    if st.dc_offset.abs() > 0.005 {
        issues.push(format!("DC offset {:.4}", st.dc_offset));
        suggest.push(
            json!({ "effect": "normalize", "params": { "peak_db": -1.0, "remove_dc": true } }),
        );
    }
    if let Some(nf) = noise_floor
        && nf > -60.0
        && nf < st.rms_db - 6.0
    {
        issues.push(format!(
            "audible background noise (noise floor ≈ {nf:.1} dBFS)"
        ));
        suggest.push(json!({ "effect": "noise_reduction", "params": { "reduction_db": 12.0 } }));
    }
    let sub = band_pct
        .iter()
        .find(|b| b.0.starts_with("sub"))
        .map_or(0.0, |b| b.1);
    if sub > 25.0 {
        issues.push(format!(
            "a lot of sub-bass/rumble below 60 Hz ({sub:.0}% of energy)"
        ));
        suggest.push(json!({ "effect": "high_pass", "params": { "cutoff_hz": 60.0 } }));
    }
    if let Some(l) = st.lufs {
        if l < -20.0 {
            issues.push(format!(
                "quiet: {l:.1} LUFS (streaming ≈ -14, podcasts ≈ -16)"
            ));
            suggest.push(json!({ "effect": "loudness_normalize", "params": { "target_lufs": -16.0, "limit_peaks": true } }));
        } else if l > -9.0 {
            issues.push(format!("very loud: {l:.1} LUFS – likely over-compressed"));
        }
    }
    if st.true_peak_db > -1.0 && clip_pct <= 0.005 {
        issues.push(format!(
            "true peak {:.1} dBTP exceeds -1 dBTP (may clip after MP3/AAC encoding)",
            st.true_peak_db
        ));
        suggest.push(json!({ "effect": "limiter", "params": { "ceiling_db": -1.0 } }));
    }

    let data = json!({
        "path": path.display().to_string(),
        "range_secs": [round3(s0 as f64 / rate as f64), round3(e0 as f64 / rate as f64)],
        "sample_rate": rate,
        "channels": chans.len(),
        "levels": stats_json(&st),
        "clipping": { "clipped_samples": clipped, "percent": (clip_pct * 1000.0).round() / 1000.0 },
        "noise_floor_dbfs": noise_floor.map(|v| (v * 10.0).round() / 10.0),
        "silence": {
            "threshold_dbfs": silence_db,
            "total_secs": round3(silent_secs),
            "regions": silences.iter().take(50).map(|(x, y)| json!([round3((x + s0) as f64 / rate as f64), round3((y + s0) as f64 / rate as f64)])).collect::<Vec<_>>(),
        },
        "band_energy_percent": band_json,
        "dominant_frequencies": peaks_json,
        "hum_prominence_db": { "50hz": (hum50 * 10.0).round() / 10.0, "60hz": (hum60 * 10.0).round() / 10.0 },
        "issues": issues,
        "suggested_effects": suggest,
    });
    let text = format!(
        "{} [{:.2}s–{:.2}s]\n{}\nIssues: {}\nSuggested effects: {}\n\nFull analysis:\n{}",
        path.display(),
        s0 as f64 / rate as f64,
        e0 as f64 / rate as f64,
        summary_line(&st),
        if issues.is_empty() {
            "none detected".into()
        } else {
            issues.join("; ")
        },
        if suggest.is_empty() {
            "none".into()
        } else {
            serde_json::to_string(&suggest).unwrap_or_default()
        },
        pretty(&data)
    );
    Ok(Output {
        text,
        data,
        png: None,
    })
}

fn list_effects() -> R<Output> {
    let effects: Vec<Value> = Effect::catalog()
        .iter()
        .map(|fx| {
            let (_, params) = variant_and_params(fx);
            json!({
                "id": effect_id(fx),
                "name": fx.name(),
                "category": fx.category().label(),
                "description": describe(fx),
                "default_params": params,
                "changes_length": fx.length_policy() == effects::LengthPolicy::Changes,
                "stereo_only": fx.needs_stereo(),
            })
        })
        .collect();
    let chains: Vec<Value> = builtin_chains()
        .iter()
        .map(|c| json!({ "name": c.name, "steps": c.steps.iter().map(|s| json!({ "effect": effect_id(s), "params": variant_and_params(s).1 })).collect::<Vec<_>>() }))
        .collect();
    let data = json!({
        "effects": effects,
        "chains": chains,
        "notes": "Units are in the parameter names (_db, _ms, _hz, _secs). graphic_eq gains are 10 values in dB for 31,62,125,250,500,1k,2k,4k,8k,16k Hz. distortion kind: soft|hard|tube|fuzz. change_speed factor 1.25 = 25% faster (pitch rises); change_tempo percent; change_pitch semitones.",
    });
    let mut text = String::from("Effects (id – description; default params):\n");
    for e in &effects {
        text.push_str(&format!(
            "• {} [{}] – {} {}\n",
            e["id"].as_str().unwrap_or(""),
            e["category"].as_str().unwrap_or(""),
            e["description"].as_str().unwrap_or(""),
            e["default_params"]
        ));
    }
    text.push_str("\nBuilt-in chains:\n");
    for c in builtin_chains() {
        text.push_str(&format!(
            "• {} = {}\n",
            c.name,
            c.steps
                .iter()
                .map(effect_id)
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }
    text.push_str(&format!("\n{}", data["notes"].as_str().unwrap_or("")));
    Ok(Output {
        text,
        data,
        png: None,
    })
}

/// Pick the quietest half-second as a noise sample.
fn auto_noise_range(m: &[f32], rate: u32) -> Option<(usize, usize)> {
    let win = (rate as f64 * 0.5) as usize;
    if m.len() < win {
        return None;
    }
    let hop = win / 4;
    let mut best: Option<(f32, usize)> = None;
    let mut i = 0;
    while i + win <= m.len() {
        let l = window_levels(&m[i..i + win], win)[0];
        if l > -100.0 && best.is_none_or(|(bl, _)| l < bl) {
            best = Some((l, i));
        }
        i += hop;
    }
    best.map(|(_, i)| (i, i + win))
}

fn apply_effects(args: &Value, cfg: &Config, progress: &Progress) -> R<Output> {
    let input = input_path(cfg, req_str(args, "input")?)?;
    let out = out_for(cfg, args, &input, "_processed", "wav")?;
    let fmt = wav_format(args)?;
    let mut steps: Vec<Effect> = Vec::new();
    if let Some(chain) = s(args, "chain") {
        let c = builtin_chains()
            .into_iter()
            .find(|c| norm(&c.name) == norm(chain) || norm(&c.name).starts_with(&norm(chain)))
            .ok_or_else(|| {
                format!(
                    "unknown chain `{chain}`. Available: {}",
                    builtin_chains()
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        steps.extend(c.steps);
    }
    if let Some(list) = args.get("effects") {
        let list = list.as_array().ok_or("`effects` must be an array")?;
        for (i, spec) in list.iter().enumerate() {
            steps.push(parse_effect(spec).map_err(|e| format!("effects[{i}]: {e}"))?);
        }
    }
    if steps.is_empty() {
        return Err("nothing to do: pass `effects` and/or `chain` (see list_effects)".into());
    }
    let a = load(&input)?;
    let before = analysis::measure(&a.chans, a.rate);
    let (start, end) = range(args, a.rate, a.len())?;
    let mut p = Project::default();
    p.add_audio(
        input.file_stem().unwrap_or_default().to_string_lossy(),
        a.chans,
        a.rate,
    );
    let rate = p.rate;

    let mut notes = Vec::new();
    if steps
        .iter()
        .any(|s| matches!(s, Effect::NoiseReduction { .. }))
    {
        let m = mono(&p.tracks[0].extract(0, p.len()));
        let (ns, ne) = match args.get("noise_profile") {
            Some(np) => range(np, rate, m.len()).map_err(|e| format!("noise_profile: {e}"))?,
            None => {
                let r = auto_noise_range(&m, rate)
                    .ok_or("audio too short to find a noise sample; pass noise_profile")?;
                notes.push(format!(
                    "noise profile taken automatically from the quietest 0.5 s at {:.2}–{:.2} s",
                    r.0 as f64 / rate as f64,
                    r.1 as f64 / rate as f64
                ));
                r
            }
        };
        let chans = p.tracks[0].extract(ns, ne);
        let refs: Vec<&[f32]> = chans.iter().map(Vec::as_slice).collect();
        p.noise_profile =
            Some(denoise::learn_profile(&refs, rate).ok_or("noise_profile range is too short")?);
    }

    p.selection = if start == 0 && end == p.len() {
        Selection::cursor(0)
    } else {
        Selection::new(start, end)
    };
    let total = steps.len() as f64;
    for (i, fx) in steps.iter().enumerate() {
        progress.report(i as f64, Some(total), fx.name());
        effects::apply(&mut p, fx).map_err(|e| format!("{}: {e}", fx.name()))?;
        if fx.needs_stereo() && p.tracks[0].num_channels() < 2 {
            notes.push(format!("{} skipped: input is mono", fx.name()));
        }
    }
    progress.report(total, Some(total), "writing file");
    let chans = p.tracks[0].extract(0, p.tracks[0].len());
    save(&out, &chans, rate, fmt)?;
    let after = analysis::measure(&chans, rate);
    let applied: Vec<String> = steps.iter().map(effect_id).collect();
    let data = json!({
        "output": out.display().to_string(),
        "applied": applied,
        "notes": notes,
        "before": stats_json(&before),
        "after": stats_json(&after),
    });
    let text = format!(
        "Wrote {}\nApplied: {}\n{}Before: {}\nAfter:  {}",
        out.display(),
        applied.join(" → "),
        notes
            .iter()
            .map(|n| format!("Note: {n}\n"))
            .collect::<String>(),
        summary_line(&before),
        summary_line(&after)
    );
    Ok(Output {
        text,
        data,
        png: None,
    })
}

fn edit_audio(args: &Value, cfg: &Config) -> R<Output> {
    let input = input_path(cfg, req_str(args, "input")?)?;
    let out = out_for(cfg, args, &input, "_edited", "wav")?;
    let fmt = wav_format(args)?;
    let ops = args
        .get("operations")
        .and_then(Value::as_array)
        .ok_or("`operations` must be an array")?;
    let a = load(&input)?;
    let rate = a.rate;
    let before = a.secs();
    let mut t = Track::new(1, "edit", a.chans);
    let secs = |v: f64| (v.max(0.0) * rate as f64).round() as usize;
    let mut log = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let name = s(op, "op").ok_or_else(|| format!("operations[{i}] needs `op`"))?;
        let len = t.len();
        let rng = || range(op, rate, len).map_err(|e| format!("operations[{i}] {name}: {e}"));
        match name {
            "trim" => {
                let (a0, b0) = rng()?;
                t.remove(b0, len);
                t.remove(0, a0);
            }
            "delete" => {
                let (a0, b0) = rng()?;
                t.remove(a0, b0);
            }
            "silence" => {
                let (a0, b0) = rng()?;
                t.silence(a0, b0);
            }
            "insert_silence" => {
                let at = secs(f(op, "at").ok_or("insert_silence needs `at`")?).min(len);
                let d = secs(f(op, "duration").ok_or("insert_silence needs `duration`")?);
                t.insert(at, &vec![vec![0.0; d]; t.num_channels()]);
            }
            "fade_in" | "fade_out" => {
                let d = secs(f(op, "duration").ok_or_else(|| format!("{name} needs `duration`"))?)
                    .min(len);
                let (a0, b0) = if name == "fade_in" {
                    (0, d)
                } else {
                    (len - d, len)
                };
                let fx = if name == "fade_in" {
                    Effect::FadeIn
                } else {
                    Effect::FadeOut
                };
                let data = effects::render(&fx, t.extract(a0, b0), rate, None)?;
                t.replace(a0, b0, &data);
            }
            "gain" => {
                let g = effects::db_to_lin(f(op, "db").ok_or("gain needs `db`")? as f32);
                let (a0, b0) = rng()?;
                t.process(a0, b0, |buf| buf.iter_mut().for_each(|x| *x *= g));
            }
            "reverse" => {
                let (a0, b0) = rng()?;
                t.process(a0, b0, |buf| buf.reverse());
            }
            "remove_silence" => {
                let thr = f(op, "threshold_db").unwrap_or(-50.0) as f32;
                let min = f(op, "min_duration").unwrap_or(0.5);
                let keep = secs(f(op, "keep").unwrap_or(0.2));
                let m = mono(&t.extract(0, len));
                let regions = silent_regions(&m, rate, thr, min);
                let mut removed = 0usize;
                for &(x, y) in regions.iter().rev() {
                    if y - x > keep {
                        let cut0 = x + keep / 2;
                        let cut1 = y - (keep - keep / 2);
                        t.remove(cut0, cut1);
                        removed += cut1 - cut0;
                    }
                }
                log.push(format!(
                    "remove_silence: shortened {} pauses, removed {:.2}s",
                    regions.len(),
                    removed as f64 / rate as f64
                ));
                continue;
            }
            other => return Err(format!("operations[{i}]: unknown op `{other}`")),
        }
        log.push(name.to_string());
    }
    let chans = t.extract(0, t.len());
    save(&out, &chans, rate, fmt)?;
    let after = chans.first().map_or(0, Vec::len) as f64 / rate as f64;
    let data = json!({ "output": out.display().to_string(), "operations": log, "duration_before_secs": round3(before), "duration_after_secs": round3(after) });
    Ok(Output {
        text: format!(
            "Wrote {} ({:.2}s → {:.2}s)\nOperations: {}",
            out.display(),
            before,
            after,
            log.join(", ")
        ),
        data,
        png: None,
    })
}

fn convert_audio(args: &Value, cfg: &Config) -> R<Output> {
    let input = input_path(cfg, req_str(args, "input")?)?;
    let is_wav = input
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    let out = out_for(
        cfg,
        args,
        &input,
        if is_wav { "_converted" } else { "" },
        "wav",
    )?;
    let fmt = wav_format(args)?;
    let a = load(&input)?;
    let mut rate = a.rate;
    let mut chans = a.chans;
    if let Some(sr) = args.get("sample_rate").and_then(Value::as_u64) {
        if !(8000..=384_000).contains(&sr) {
            return Err("sample_rate must be between 8000 and 384000".into());
        }
        chans = resample(&chans, rate, sr as u32);
        rate = sr as u32;
    }
    if let Some(n) = args.get("channels").and_then(Value::as_u64) {
        chans = adapt_channels(&chans, n.clamp(1, 2) as usize);
    }
    if let Some(l) = f(args, "normalize_lufs") {
        chans = effects::render(
            &Effect::LoudnessNormalize {
                target_lufs: l as f32,
                limit_peaks: true,
            },
            chans,
            rate,
            None,
        )?;
    }
    save(&out, &chans, rate, fmt)?;
    let st = analysis::measure(&chans, rate);
    let data = json!({ "output": out.display().to_string(), "sample_rate": rate, "channels": chans.len(), "format": s(args, "format").unwrap_or("pcm24"), "levels": stats_json(&st) });
    Ok(Output {
        text: format!(
            "Wrote {} ({rate} Hz, {} ch)\n{}",
            out.display(),
            chans.len(),
            summary_line(&st)
        ),
        data,
        png: None,
    })
}

fn mix_audio(args: &Value, cfg: &Config) -> R<Output> {
    let inputs = args
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or("`inputs` must be an array")?;
    if inputs.is_empty() {
        return Err("`inputs` is empty".into());
    }
    let out = output_path(cfg, req_str(args, "output")?, "wav")?;
    let fmt = wav_format(args)?;
    let mut p = Project::default();
    let mut used = Vec::new();
    for (i, inp) in inputs.iter().enumerate() {
        let path = input_path(
            cfg,
            req_str(inp, "path").map_err(|e| format!("inputs[{i}]: {e}"))?,
        )?;
        if path == out {
            return Err("the output must not be one of the inputs".into());
        }
        let a = load(&path)?;
        let mut chans = if p.tracks.is_empty() {
            a.chans
        } else {
            resample(&a.chans, a.rate, p.rate)
        };
        if p.tracks.is_empty() {
            p.rate = a.rate;
        }
        let offset = (f(inp, "offset").unwrap_or(0.0).max(0.0) * p.rate as f64).round() as usize;
        for c in &mut chans {
            c.splice(0..0, std::iter::repeat_n(0.0, offset));
        }
        let idx = p.add_track(
            path.file_stem().unwrap_or_default().to_string_lossy(),
            chans,
        );
        let t = &mut p.tracks[idx];
        t.gain = effects::db_to_lin(f(inp, "gain_db").unwrap_or(0.0) as f32);
        t.pan = f(inp, "pan").unwrap_or(0.0).clamp(-1.0, 1.0) as f32;
        t.mute = b(inp, "mute");
        used.push(path.display().to_string());
    }
    let mut mix = p.mixdown(0, p.len(), false);
    if mix.len() == 1 {
        mix = adapt_channels(&mix, 2);
    }
    if let Some(pk) = f(args, "normalize_peak_db") {
        mix = effects::render(
            &Effect::Normalize {
                peak_db: pk as f32,
                remove_dc: false,
            },
            mix,
            p.rate,
            None,
        )?;
    }
    save(&out, &mix, p.rate, fmt)?;
    let st = analysis::measure(&mix, p.rate);
    let mut warn = String::new();
    if st.peak_db > 0.0 && fmt != WavFormat::Float32 {
        warn = format!(
            "\nWarning: mix peaked at {:.1} dBFS and was clipped – lower gains or set normalize_peak_db.",
            st.peak_db
        );
    }
    let data = json!({ "output": out.display().to_string(), "inputs": used, "sample_rate": p.rate, "levels": stats_json(&st) });
    Ok(Output {
        text: format!("Wrote {}\n{}{warn}", out.display(), summary_line(&st)),
        data,
        png: None,
    })
}

fn generate_audio(args: &Value, cfg: &Config) -> R<Output> {
    let out = output_path(cfg, req_str(args, "output")?, "wav")?;
    let fmt = wav_format(args)?;
    let secs = f(args, "duration").ok_or("missing `duration`")? as f32;
    if !(0.0..=3600.0).contains(&secs) {
        return Err("duration must be 0–3600 seconds".into());
    }
    let amp = f(args, "amplitude").unwrap_or(0.5).clamp(0.0, 1.0) as f32;
    let rate = args
        .get("sample_rate")
        .and_then(Value::as_u64)
        .unwrap_or(44_100) as u32;
    let g = match req_str(args, "kind")? {
        "tone" => Generator::Tone {
            wave: match s(args, "waveform").unwrap_or("sine") {
                "sine" => Waveform::Sine,
                "square" => Waveform::Square,
                "sawtooth" => Waveform::Sawtooth,
                "triangle" => Waveform::Triangle,
                o => return Err(format!("unknown waveform {o}")),
            },
            freq: f(args, "frequency").unwrap_or(440.0) as f32,
            amp,
            secs,
        },
        "noise" => Generator::Noise {
            kind: match s(args, "noise_type").unwrap_or("white") {
                "white" => NoiseKind::White,
                "pink" => NoiseKind::Pink,
                "brown" => NoiseKind::Brown,
                o => return Err(format!("unknown noise_type {o}")),
            },
            amp,
            secs,
        },
        "silence" => Generator::Silence { secs },
        o => return Err(format!("unknown kind {o} (tone, noise, silence)")),
    };
    let mono_data = g.render(rate);
    let n = args
        .get("channels")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 2) as usize;
    save(&out, &vec![mono_data; n], rate, fmt)?;
    Ok(Output {
        text: format!(
            "Wrote {} ({secs} s {}, {rate} Hz, {n} ch)",
            out.display(),
            g.name().to_lowercase()
        ),
        data: json!({ "output": out.display().to_string(), "duration_secs": secs, "sample_rate": rate, "channels": n }),
        png: None,
    })
}

fn separate_stems(
    args: &Value,
    cfg: &Config,
    progress: &Progress,
    cancel: &CancelHandle,
) -> R<Output> {
    let input = input_path(cfg, req_str(args, "input")?)?;
    let fmt = wav_format(args)?;
    let stem_base = input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let out_dir = match s(args, "output_dir") {
        Some(d) => {
            let p = absolutize(cfg, d);
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
            let p = std::fs::canonicalize(&p).map_err(|e| e.to_string())?;
            check_root(cfg, &p)?;
            p
        }
        None => {
            let p = input.with_file_name(format!("{stem_base}_stems"));
            std::fs::create_dir_all(&p)
                .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
            p
        }
    };
    let a = load(&input)?;
    let (s0, e0) = range(args, a.rate, a.len())?;
    let chans: Vec<Vec<f32>> = a.chans.iter().map(|c| c[s0..e0].to_vec()).collect();
    let engine = s(args, "engine").unwrap_or("demucs");
    let started = std::time::Instant::now();
    let stems: Vec<(String, Vec<Vec<f32>>, u32)> = match engine {
        "builtin" => {
            if chans.len() < 2 {
                return Err(
                    "the builtin engine needs a stereo file; use engine=demucs for mono audio"
                        .into(),
                );
            }
            progress.report(0.0, Some(1.0), "centre extraction");
            let (v, i) =
                separation::centre_extract(&chans[0], &chans[1], a.rate, 150.0, 9000.0, 2.0);
            vec![
                ("vocals".into(), v, a.rate),
                ("instrumental".into(), i, a.rate),
            ]
        }
        "demucs" => {
            let model = match s(args, "model").unwrap_or("htdemucs") {
                "htdemucs" => DemucsModel::HtDemucs,
                "htdemucs_ft" => DemucsModel::HtDemucsFt,
                "htdemucs_6s" => DemucsModel::HtDemucs6s,
                o => return Err(format!("unknown model {o}")),
            };
            let command = s(args, "demucs_command")
                .map(str::to_string)
                .unwrap_or_else(|| cfg.demucs_command.clone());
            let tmp = std::env::temp_dir().join(format!(
                "rust_audacity_mcp_{}_{}",
                std::process::id(),
                started.elapsed().as_nanos() ^ (s0 as u128)
            ));
            std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
            let wav_in = tmp.join(format!(
                "{}.wav",
                stem_base
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
            ));
            save(&wav_in, &chans, a.rate, WavFormat::Float32)?;
            let (tx, rx) = std::sync::mpsc::channel::<StemEvent>();
            let p2 = progress.clone();
            let fwd = std::thread::spawn(move || {
                let mut last = String::new();
                for ev in rx {
                    match ev {
                        StemEvent::Progress(x) => {
                            p2.report(x as f64 * 100.0, Some(100.0), "separating")
                        }
                        StemEvent::Log(l) => last = l,
                        StemEvent::Done(_) => {}
                    }
                }
                last
            });
            let res = separation::run_demucs(
                &command,
                model,
                b(args, "two_stems"),
                &wav_in,
                &tmp.join("out"),
                &tx,
                cancel,
            );
            drop(tx);
            let _ = fwd.join();
            let res = res.map_err(|e| format!("{e:#}\n\nTip: install Demucs with `pip install -U demucs`, pass demucs_command, or use engine=\"builtin\" for a quick vocals/instrumental split."));
            let loaded = res.and_then(|files| {
                files
                    .into_iter()
                    .map(|(name, path)| load(&path).map(|au| (name, au.chans, au.rate)))
                    .collect::<R<Vec<_>>>()
            });
            let _ = std::fs::remove_dir_all(&tmp);
            loaded?
        }
        o => return Err(format!("unknown engine {o} (demucs or builtin)")),
    };
    let mut files = Vec::new();
    for (name, chans, rate) in &stems {
        let path = out_dir.join(format!("{stem_base}_{name}.wav"));
        save(&path, chans, *rate, fmt)?;
        let st = analysis::measure(chans, *rate);
        files.push(json!({ "stem": name, "path": path.display().to_string(), "duration_secs": round3(st.duration_secs), "rms_dbfs": db(st.rms_db) }));
    }
    let data = json!({ "engine": engine, "output_dir": out_dir.display().to_string(), "stems": files, "seconds_taken": round3(started.elapsed().as_secs_f64()) });
    let text = format!(
        "Separated {} into {} stems with {engine} in {:.1}s:\n{}",
        input.display(),
        stems.len(),
        started.elapsed().as_secs_f32(),
        files
            .iter()
            .map(|f| format!(
                "• {}: {}",
                f["stem"].as_str().unwrap_or(""),
                f["path"].as_str().unwrap_or("")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(Output {
        text,
        data,
        png: None,
    })
}

fn render_spectrogram(args: &Value, cfg: &Config) -> R<Output> {
    let input = input_path(cfg, req_str(args, "input")?)?;
    let out = match s(args, "output") {
        Some(o) => output_path(cfg, o, "png")?,
        None => input.with_file_name(format!(
            "{}_spectrogram.png",
            input.file_stem().unwrap_or_default().to_string_lossy()
        )),
    };
    let a = load(&input)?;
    let (s0, e0) = range(args, a.rate, a.len())?;
    let data: Vec<f32> = match s(args, "channel").unwrap_or("mix") {
        "left" => a.chans[0][s0..e0].to_vec(),
        "right" => a.chans.get(1).unwrap_or(&a.chans[0])[s0..e0].to_vec(),
        _ => mono(
            &a.chans
                .iter()
                .map(|c| c[s0..e0].to_vec())
                .collect::<Vec<_>>(),
        ),
    };
    let w = args
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(1200)
        .clamp(100, 4000) as usize;
    let h = args
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(400)
        .clamp(64, 2000) as usize;
    let spp = data.len() as f64 / w as f64;
    let mut px = specgram::render(&data, a.rate, 0.0, spp, w, h);
    // Faint guide lines at 100 Hz, 1 kHz, 10 kHz.
    for fq in [100.0, 1000.0, 10000.0] {
        if fq < a.rate as f32 / 2.0 {
            let y = (specgram::frac_of(fq, a.rate) * h as f32) as usize;
            if y < h {
                for x in (0..w).step_by(3) {
                    let p = &mut px[y * w + x];
                    *p = [
                        p[0].saturating_add(70),
                        p[1].saturating_add(70),
                        p[2].saturating_add(70),
                    ];
                }
            }
        }
    }
    let rgb: Vec<u8> = px.into_iter().flatten().collect();
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&rgb).map_err(|e| e.to_string())?;
    }
    std::fs::write(&out, &png_bytes).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    let (t0, t1) = (s0 as f64 / a.rate as f64, e0 as f64 / a.rate as f64);
    let data = json!({ "output": out.display().to_string(), "width": w, "height": h, "time_range_secs": [round3(t0), round3(t1)], "freq_range_hz": [specgram::MIN_FREQ, a.rate / 2] });
    let text = format!(
        "Spectrogram of {} ({:.2}–{:.2}s) saved to {}. x: time {:.2}s→{:.2}s; y: log frequency {} Hz (bottom) → {} Hz (top); guide lines at 100 Hz, 1 kHz, 10 kHz; brighter = louder (−96…0 dB).",
        input.display(),
        t0,
        t1,
        out.display(),
        t0,
        t1,
        specgram::MIN_FREQ,
        a.rate / 2
    );
    Ok(Output {
        text,
        data,
        png: Some(png_bytes),
    })
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn effect_specs_parse() {
        assert_eq!(parse_effect(&json!("fade_in")).unwrap(), Effect::FadeIn);
        assert_eq!(
            parse_effect(&json!({"effect": "Amplify", "params": {"db": 3.0}})).unwrap(),
            Effect::Amplify { db: 3.0 }
        );
        assert_eq!(
            parse_effect(&json!({"effect": "amplify", "db": -2.0})).unwrap(),
            Effect::Amplify { db: -2.0 }
        );
        match parse_effect(&json!({"effect": "compressor", "params": {"ratio": 6}})).unwrap() {
            Effect::Compressor {
                ratio,
                threshold_db,
                ..
            } => assert_eq!((ratio, threshold_db), (6.0, -18.0)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_effect(&json!({"effect": "distortion", "params": {"kind": "fuzz"}})).unwrap(),
            Effect::Distortion { .. }
        ));
        assert!(parse_effect(&json!({"effect": "Loudness Normalize (LUFS)"})).is_ok());
        assert!(
            parse_effect(&json!({"effect": "compressor", "params": {"bogus": 1}}))
                .unwrap_err()
                .contains("Valid")
        );
        assert!(
            parse_effect(&json!("nope"))
                .unwrap_err()
                .contains("noise_reduction")
        );
    }

    #[test]
    fn every_catalog_effect_has_an_id_that_round_trips() {
        for fx in Effect::catalog() {
            assert_eq!(parse_effect(&json!(effect_id(&fx))).unwrap(), fx);
        }
    }

    #[test]
    fn silent_regions_found() {
        let rate = 1000;
        let mut m = vec![0.5f32; 3000];
        m[1000..2000].iter_mut().for_each(|v| *v = 0.0);
        let r = silent_regions(&m, rate, -50.0, 0.3);
        assert_eq!(r.len(), 1);
        assert!(
            r[0].0 >= 980 && r[0].0 <= 1020 && r[0].1 >= 1980 && r[0].1 <= 2020,
            "{r:?}"
        );
    }
}
