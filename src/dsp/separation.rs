//! Stem separation.
//!
//! Two engines:
//! * **Built-in** – fast, dependency-free spectral centre extraction. For each
//!   time-frequency bin it measures how "centre-panned" the stereo signal is and
//!   moves centred, vocal-range content into a *Vocals* stem; everything else is
//!   the *Instrumental* stem. Works on stereo mixes; good for karaoke/practice.
//! * **Demucs (AI)** – runs Meta's open-source Hybrid Transformer Demucs model
//!   through its command-line tool, giving 2, 4 (vocals/drums/bass/other) or 6 stems.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc::Sender},
};

use anyhow::{Context, Result, anyhow, bail};

use super::stft::{bin_freq, stft_map};

/// Separate a stereo mix into (vocals, instrumental), each stereo.
/// `strength` 0.5–4: higher keeps only strongly centred content in the vocal stem.
pub fn centre_extract(
    l: &[f32],
    r: &[f32],
    rate: u32,
    low_hz: f32,
    high_hz: f32,
    strength: f32,
) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let n = 4096;
    let hop = n / 4;
    let rate_f = rate as f32;
    let sharp = strength.clamp(0.25, 8.0);
    // Smooth band-limit weight for the vocal range (half-octave cosine edges).
    let band = |f: f32| -> f32 {
        let edge = |x: f32| 0.5 - 0.5 * (std::f32::consts::PI * x.clamp(0.0, 1.0)).cos();
        let lo = edge((f / low_hz).log2() * 2.0 + 1.0);
        let hi = edge((high_hz / f.max(1.0)).log2() * 2.0 + 1.0);
        lo * hi
    };
    let out = stft_map(&[l, r], 4, n, hop, |_, input, output| {
        let (sl, sr) = (&input[0], &input[1]);
        for b in 0..sl.len() {
            let (a, c) = (sl[b], sr[b]);
            let energy = a.norm_sqr() + c.norm_sqr();
            // 1.0 when L == R (centre), 0 when uncorrelated or opposite.
            let sim = if energy > 1e-12 {
                (2.0 * (a * c.conj()).re / energy).max(0.0)
            } else {
                0.0
            };
            let mask = sim.powf(sharp) * band(bin_freq(b, n, rate_f));
            let centre = (a + c) * 0.5 * mask;
            output[0][b] = centre;
            output[1][b] = centre;
            output[2][b] = a - centre;
            output[3][b] = c - centre;
        }
    });
    let mut it = out.into_iter();
    let (vl, vr, il, ir) = (
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
        it.next().unwrap(),
    );
    (vec![vl, vr], vec![il, ir])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemucsModel {
    /// Default 4-stem hybrid transformer model.
    HtDemucs,
    /// Fine-tuned 4-stem version (slower, a bit better).
    HtDemucsFt,
    /// 6 stems: adds guitar and piano.
    HtDemucs6s,
}

impl DemucsModel {
    pub const ALL: [DemucsModel; 3] = [
        DemucsModel::HtDemucs,
        DemucsModel::HtDemucsFt,
        DemucsModel::HtDemucs6s,
    ];
    pub fn id(self) -> &'static str {
        match self {
            DemucsModel::HtDemucs => "htdemucs",
            DemucsModel::HtDemucsFt => "htdemucs_ft",
            DemucsModel::HtDemucs6s => "htdemucs_6s",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DemucsModel::HtDemucs => "htdemucs – 4 stems (fast)",
            DemucsModel::HtDemucsFt => "htdemucs_ft – 4 stems (best, 4× slower)",
            DemucsModel::HtDemucs6s => "htdemucs_6s – 6 stems (+guitar, piano)",
        }
    }
}

/// A separated stem: name (e.g. "vocals"), planar audio and its sample rate.
pub struct Stem {
    pub name: String,
    pub channels: Vec<Vec<f32>>,
    pub rate: u32,
}

/// Messages from a background separation job to the UI.
pub enum StemEvent {
    Progress(f32),
    Log(String),
    Done(std::result::Result<Vec<Stem>, String>),
}

/// Handle used to cancel a running Demucs process.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<Option<Child>>>);

impl CancelHandle {
    pub fn cancel(&self) {
        if let Some(c) = self.0.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

/// Split a command line like `python -m demucs` into program + args (supports "quoted parts").
pub fn split_command(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in cmd.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Extract the last "NN%" progress value from tqdm output.
pub fn parse_progress(text: &str) -> Option<f32> {
    let idx = text.rfind('%')?;
    let digits: String = text[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits
        .parse::<f32>()
        .ok()
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
}

/// Run Demucs on `input` (a WAV file) writing stems under `out_dir`. Blocks; call from a thread.
/// Returns (stem name, wav path) pairs.
pub fn run_demucs(
    command: &str,
    model: DemucsModel,
    two_stems: bool,
    input: &Path,
    out_dir: &Path,
    events: &Sender<StemEvent>,
    cancel: &CancelHandle,
) -> Result<Vec<(String, PathBuf)>> {
    let parts = split_command(command);
    let (prog, pre_args) = parts
        .split_first()
        .ok_or_else(|| anyhow!("no Demucs command configured"))?;
    let mut cmd = Command::new(prog);
    cmd.args(pre_args)
        .arg("-n")
        .arg(model.id())
        .arg("-o")
        .arg(out_dir);
    if two_stems {
        cmd.arg("--two-stems=vocals");
    }
    cmd.arg(input).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().with_context(|| {
        format!("could not start `{command}`.\nInstall Demucs with:  pip install -U demucs\n(or set the command to e.g. `python -m demucs`)")
    })?;
    let mut stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    *cancel.0.lock().unwrap() = Some(child);

    let ev = events.clone();
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        for line in s.lines() {
            let _ = ev.send(StemEvent::Log(line.to_string()));
        }
    });
    // tqdm writes progress to stderr with carriage returns.
    let mut log = String::new();
    let mut buf = [0u8; 1024];
    let mut pending = String::new();
    let mut handle_line = |line: &str| {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Some(p) = parse_progress(line) {
            let _ = events.send(StemEvent::Progress(p));
        } else {
            log.push_str(line);
            log.push('\n');
            let _ = events.send(StemEvent::Log(line.to_string()));
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
            handle_line(&line);
        }
    }
    handle_line(&pending);
    let _ = out_thread.join();
    let status = cancel
        .0
        .lock()
        .unwrap()
        .take()
        .map(|mut c| c.wait())
        .transpose()?;
    match status {
        Some(s) if s.success() => {}
        Some(s) => {
            let tail: String = log
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            bail!("Demucs failed ({s}).\n{tail}");
        }
        None => bail!("Demucs was cancelled"),
    }
    let stem_dir = out_dir
        .join(model.id())
        .join(input.file_stem().unwrap_or_default());
    let mut stems: Vec<(String, PathBuf)> = std::fs::read_dir(&stem_dir)
        .with_context(|| {
            format!(
                "Demucs finished but no stems found in {}",
                stem_dir.display()
            )
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("wav")
                    || e.eq_ignore_ascii_case("mp3")
                    || e.eq_ignore_ascii_case("flac")
            })
        })
        .map(|p| {
            (
                p.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                p,
            )
        })
        .collect();
    // Present in a musical order.
    let order = [
        "vocals",
        "no_vocals",
        "drums",
        "bass",
        "guitar",
        "piano",
        "other",
    ];
    stems.sort_by_key(|(n, _)| order.iter().position(|o| o == n).unwrap_or(99));
    if stems.is_empty() {
        bail!(
            "Demucs finished but produced no audio files in {}",
            stem_dir.display()
        );
    }
    Ok(stems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centre_extract_separates_panned_sources() {
        let rate = 44100;
        let n = rate as usize * 2;
        // "Vocal": 600 Hz in the centre. "Guitar": 2500 Hz hard left.
        let v: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 600.0 * i as f32 / rate as f32).sin() * 0.4)
            .collect();
        let g: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 2500.0 * i as f32 / rate as f32).sin() * 0.4)
            .collect();
        let l: Vec<f32> = v.iter().zip(&g).map(|(a, b)| a + b).collect();
        let r = v.clone();
        let (voc, inst) = centre_extract(&l, &r, rate, 120.0, 10000.0, 2.0);
        let rms = |x: &[f32]| {
            (x[8192..n - 8192].iter().map(|s| s * s).sum::<f32>() / (n - 16384) as f32).sqrt()
        };
        // Right channel of the instrumental should have lost most of the centred vocal.
        assert!(
            rms(&inst[1]) < 0.4 * rms(&r),
            "inst R {} vs {}",
            rms(&inst[1]),
            rms(&r)
        );
        // Vocal stem should be mostly the 600 Hz tone, not the guitar.
        let diff: Vec<f32> = voc[0].iter().zip(&v).map(|(a, b)| a - b).collect();
        assert!(rms(&diff) < 0.5 * rms(&v), "vocal error {}", rms(&diff));
        // Stems sum back to the original.
        let recon: Vec<f32> = voc[0].iter().zip(&inst[0]).map(|(a, b)| a + b).collect();
        let err = recon
            .iter()
            .zip(&l)
            .fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-3);
    }

    #[test]
    fn command_splitting_and_progress() {
        assert_eq!(
            split_command(r#"python -m demucs"#),
            vec!["python", "-m", "demucs"]
        );
        assert_eq!(
            split_command(r#""C:\My Py\python.exe" -m demucs"#),
            vec![r"C:\My Py\python.exe", "-m", "demucs"]
        );
        assert_eq!(
            parse_progress(" 45%|████▌     | 17.5/39.0 [00:10<00:12"),
            Some(0.45)
        );
        assert_eq!(parse_progress("100%|██████████|"), Some(1.0));
        assert_eq!(parse_progress("Separating track"), None);
    }
    /// End-to-end check of the Demucs plumbing using a stand-in script that mimics
    /// the real CLI (arguments, tqdm progress on stderr, output folder layout).
    /// Skipped when python3 isn't available.
    #[test]
    fn demucs_plumbing_with_fake_cli() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ra_fake_demucs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_demucs.py");
        std::fs::write(
            &script,
            r#"
import sys, os, shutil, time
args = sys.argv[1:]
model = args[args.index("-n") + 1]
out = args[args.index("-o") + 1]
two = any(a.startswith("--two-stems") for a in args)
inp = args[-1]
for p in (0, 25, 50, 75, 100):
    sys.stderr.write("\r%3d%%|####      | %d/100 [00:01<00:01]" % (p, p)); sys.stderr.flush()
print("Separated tracks will be stored in", out)
name = os.path.splitext(os.path.basename(inp))[0]
dest = os.path.join(out, model, name)
os.makedirs(dest, exist_ok=True)
stems = ["vocals", "no_vocals"] if two else ["drums", "bass", "other", "vocals"]
for s in stems:
    shutil.copy(inp, os.path.join(dest, s + ".wav"))
"#,
        )
        .unwrap();
        let input = dir.join("song.wav");
        crate::fileio::export_wav(
            &input,
            &[vec![0.1; 4410], vec![0.2; 4410]],
            44100,
            crate::fileio::WavFormat::Float32,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let cmd = format!("python3 \"{}\"", script.display());
        let stems = run_demucs(
            &cmd,
            DemucsModel::HtDemucs,
            false,
            &input,
            &dir.join("out"),
            &tx,
            &CancelHandle::default(),
        )
        .unwrap();
        let names: Vec<&str> = stems.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["vocals", "drums", "bass", "other"]);
        let progress: Vec<f32> = rx
            .try_iter()
            .filter_map(|e| {
                if let StemEvent::Progress(p) = e {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(progress.last(), Some(&1.0));
        let two = run_demucs(
            &cmd,
            DemucsModel::HtDemucs,
            true,
            &input,
            &dir.join("out2"),
            &tx,
            &CancelHandle::default(),
        )
        .unwrap();
        assert_eq!(
            two.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["vocals", "no_vocals"]
        );
        // A missing program gives a helpful error.
        let err = run_demucs(
            "definitely-not-a-real-demucs",
            DemucsModel::HtDemucs,
            false,
            &input,
            &dir.join("o3"),
            &tx,
            &CancelHandle::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("pip install"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
