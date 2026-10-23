//! Batch processing: run an effect chain over many files and export them all.
//! Used by File ▸ Batch Process… in the app and by the `batch_process` MCP tool.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use crate::{
    dsp::{analysis, denoise},
    effects::{CANCELLED, Effect, EffectJob},
    fileio::{self, ExportFormat},
    metadata::Metadata,
    project::Project,
};

/// Audio file extensions picked up when adding a folder.
pub const AUDIO_EXTS: [&str; 11] = [
    "wav", "mp3", "flac", "ogg", "oga", "m4a", "aac", "aif", "aiff", "opus", "wma",
];

#[derive(Clone, Debug)]
pub struct BatchSettings {
    pub chain: Vec<Effect>,
    pub format: ExportFormat,
    /// None = write each output next to its input.
    pub out_dir: Option<PathBuf>,
    /// Added to the file name, e.g. "_processed".
    pub suffix: String,
    pub overwrite: bool,
    /// Tags to write (artist, album, cover…); the title is taken from each file name when
    /// `title_from_file` is set.
    pub meta: Option<Metadata>,
    pub title_from_file: bool,
}

#[derive(Clone, Debug)]
pub struct FileResult {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub error: Option<String>,
    pub lufs_before: Option<f64>,
    pub lufs_after: Option<f64>,
    pub peak_after_db: f32,
    pub secs: f32,
}

pub fn is_audio_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Audio files in a folder (optionally including sub-folders), sorted by name.
pub fn audio_files_in(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if recursive {
                    stack.push(p);
                }
            } else if is_audio_file(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Where the output for `input` goes (never the input itself; numbered if it exists and
/// `overwrite` is off).
pub fn output_path(input: &Path, s: &BatchSettings) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let dir = s
        .out_dir
        .clone()
        .unwrap_or_else(|| input.parent().map(Path::to_path_buf).unwrap_or_default());
    let ext = s.format.extension();
    let mut p = dir.join(format!("{stem}{}.{ext}", s.suffix));
    let mut k = 2;
    while (p.exists() && !s.overwrite) || p == input {
        p = dir.join(format!("{stem}{} ({k}).{ext}", s.suffix));
        k += 1;
    }
    p
}

/// Process one file: import, run the chain over all of it, export.
pub fn process_file(input: &Path, s: &BatchSettings, cancel: &AtomicBool) -> FileResult {
    let started = Instant::now();
    let mut r = FileResult {
        input: input.to_path_buf(),
        output: None,
        error: None,
        lufs_before: None,
        lufs_after: None,
        peak_after_db: f32::NEG_INFINITY,
        secs: 0.0,
    };
    let result = (|| -> Result<(PathBuf, f32, Option<f64>), String> {
        let audio = fileio::import(input).map_err(|e| format!("{e:#}"))?;
        r.lufs_before = analysis::measure(&audio.channels, audio.rate).lufs;
        let mut p = Project::default();
        let name = input
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        p.add_audio(name.clone(), audio.channels, audio.rate);
        if !s.chain.is_empty() {
            if s.chain
                .iter()
                .any(|e| matches!(e, Effect::NoiseReduction { .. }))
            {
                let ch: Vec<Vec<f32>> = p.tracks[0].channels.iter().map(|c| c.to_vec()).collect();
                p.noise_profile = denoise::auto_profile(&ch, p.rate);
            }
            let job = EffectJob::prepare(&p, "Batch", s.chain.clone())?;
            let res = job.run(|_, _| !cancel.load(Ordering::Relaxed))?;
            res.commit(&mut p);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        let mix = p.mixdown(0, p.len(), false);
        let st = analysis::measure(&mix, p.rate);
        let out = output_path(input, s);
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let meta = s.meta.clone().map(|mut m| {
            if s.title_from_file {
                m.title = name.clone();
            }
            m
        });
        let meta = meta.or_else(|| {
            s.title_from_file.then(|| Metadata {
                title: name.clone(),
                ..Default::default()
            })
        });
        fileio::export_tagged(&out, &mix, p.rate, s.format, meta.as_ref(), &[])
            .map_err(|e| format!("{e:#}"))?;
        Ok((out, st.peak_db, st.lufs))
    })();
    match result {
        Ok((out, peak, lufs)) => {
            r.output = Some(out);
            r.peak_after_db = peak;
            r.lufs_after = lufs;
        }
        Err(e) => r.error = Some(e),
    }
    r.secs = started.elapsed().as_secs_f32();
    r
}

/// Process every file in order. `progress(done, total, current)` is called before each file
/// and once at the end; returning results as they finish through `on_result`.
pub fn run(
    inputs: &[PathBuf],
    s: &BatchSettings,
    cancel: &AtomicBool,
    mut on_file: impl FnMut(usize, usize, &Path),
    mut on_result: impl FnMut(&FileResult),
) -> Vec<FileResult> {
    let mut out = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        on_file(i, inputs.len(), input);
        let r = process_file(input, s, cancel);
        on_result(&r);
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileio::WavFormat;

    #[test]
    fn batch_runs_chain_and_names_outputs() {
        let dir = std::env::temp_dir().join(format!("ra_batch_{}", std::process::id()));
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 44100;
        let mut inputs = Vec::new();
        for (i, amp) in [0.05f32, 0.4].iter().enumerate() {
            let tone: Vec<f32> = (0..rate as usize * 2)
                .map(|k| (k as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * amp)
                .collect();
            let p = dir.join(format!("take{i}.wav"));
            fileio::export_wav(&p, &[tone], rate, WavFormat::Float32).unwrap();
            inputs.push(p);
        }
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert_eq!(audio_files_in(&dir, false), inputs);
        let s = BatchSettings {
            chain: vec![Effect::LoudnessNormalize {
                target_lufs: -16.0,
                limit_peaks: true,
            }],
            format: ExportFormat::Flac { bits: 16 },
            out_dir: Some(out_dir.clone()),
            suffix: "_norm".into(),
            overwrite: false,
            meta: Some(Metadata {
                artist: "Me".into(),
                ..Default::default()
            }),
            title_from_file: true,
        };
        let mut seen = Vec::new();
        let res = run(
            &inputs,
            &s,
            &AtomicBool::new(false),
            |i, n, _| seen.push((i, n)),
            |_| {},
        );
        assert_eq!(seen, vec![(0, 2), (1, 2)]);
        for r in &res {
            assert!(r.error.is_none(), "{:?}", r.error);
            let l = r.lufs_after.unwrap();
            assert!((l + 16.0).abs() < 0.6, "normalised to {l}");
        }
        assert_eq!(
            res[0].output.as_ref().unwrap(),
            &out_dir.join("take0_norm.flac")
        );
        let tag = metaflac::Tag::read_from_path(res[0].output.as_ref().unwrap()).unwrap();
        assert_eq!(tag.get_vorbis("TITLE").unwrap().next(), Some("take0"));
        assert_eq!(tag.get_vorbis("ARTIST").unwrap().next(), Some("Me"));
        // Running again doesn't overwrite: outputs are numbered.
        let again = process_file(&inputs[0], &s, &AtomicBool::new(false));
        assert_eq!(again.output.unwrap(), out_dir.join("take0_norm (2).flac"));
        // A broken file is reported, not fatal.
        std::fs::write(dir.join("bad.wav"), b"not audio").unwrap();
        let bad = process_file(&dir.join("bad.wav"), &s, &AtomicBool::new(false));
        assert!(bad.error.is_some() && bad.output.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
