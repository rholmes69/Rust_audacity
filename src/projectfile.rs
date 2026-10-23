//! Rust Audacity project files (`.rap`).
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! "RAPROJ\0\x01"            8-byte magic + format version
//! u64 header_len            length of the JSON header
//! header (JSON)             rate, selection, per-track settings and sample counts
//! audio                     every track's channels in order, as raw f32 samples
//! ```
//!
//! Audio is stored losslessly as 32-bit float, so saving and reopening never changes a
//! sample. Everything is written to a temporary file first and then renamed, so an
//! interrupted save never leaves a half-written project behind.

use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    envelope::{EnvPoint, Envelope},
    project::{Project, Selection},
    track::{Clip, Track, TrackView},
    transcript::{Transcript, Word},
};

pub const EXTENSION: &str = "rap";
const MAGIC: &[u8; 8] = b"RAPROJ\0\x01";

#[derive(Serialize, Deserialize)]
struct Header {
    app: String,
    rate: u32,
    selection: [usize; 2],
    tracks: Vec<TrackHeader>,
    /// Free-form UI state (zoom, scroll, …) so a project reopens where you left it.
    #[serde(default)]
    view: Option<ViewState>,
    /// Word transcript of one track (by index).
    #[serde(default)]
    transcript: Option<TranscriptHeader>,
    #[serde(default)]
    labels: Vec<crate::labels::Label>,
    #[serde(default)]
    metadata: crate::metadata::Metadata,
}

#[derive(Serialize, Deserialize)]
struct TranscriptHeader {
    track: usize,
    language: String,
    words: Vec<Word>,
}

#[derive(Serialize, Deserialize)]
struct TrackHeader {
    name: String,
    channels: usize,
    samples: usize,
    gain: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    selected: bool,
    height: f32,
    spectrogram: bool,
    /// Volume envelope points as (sample, dB).
    #[serde(default)]
    envelope: Vec<(usize, f32)>,
    /// Clips (older projects: one clip for the whole track).
    #[serde(default)]
    clips: Option<Vec<Clip>>,
    #[serde(default)]
    live_fx: Vec<crate::dsp::realtime::LiveFx>,
}

/// Horizontal view saved with a project.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ViewState {
    pub left: f64,
    pub samples_per_pixel: f64,
}

/// Write `project` to `path`. The undo history is not saved (like Audacity's default).
pub fn save(path: &Path, project: &Project, view: Option<ViewState>) -> Result<()> {
    let header = Header {
        app: format!("Rust Audacity {}", env!("CARGO_PKG_VERSION")),
        rate: project.rate,
        selection: [project.selection.start, project.selection.end],
        tracks: project
            .tracks
            .iter()
            .map(|t| TrackHeader {
                name: t.name.clone(),
                channels: t.num_channels(),
                samples: t.len(),
                gain: t.gain,
                pan: t.pan,
                mute: t.mute,
                solo: t.solo,
                selected: t.selected,
                height: t.height,
                spectrogram: t.view == TrackView::Spectrogram,
                envelope: t.envelope.points.iter().map(|p| (p.pos, p.db)).collect(),
                clips: Some(t.clips.clone()),
                live_fx: t.live_fx.clone(),
            })
            .collect(),
        view,
        labels: project.labels.clone(),
        metadata: project.metadata.clone(),
        transcript: project.transcript.as_ref().and_then(|t| {
            Some(TranscriptHeader {
                track: project.transcript_track()?,
                language: t.language.clone(),
                words: t.words.clone(),
            })
        }),
    };
    let json = serde_json::to_vec(&header)?;

    let tmp = temp_path(path);
    let result = (|| -> Result<()> {
        let mut w = BufWriter::with_capacity(
            1 << 20,
            File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?,
        );
        w.write_all(MAGIC)?;
        w.write_all(&(json.len() as u64).to_le_bytes())?;
        w.write_all(&json)?;
        let mut buf = Vec::with_capacity(1 << 16);
        for t in &project.tracks {
            for ch in &t.channels {
                for chunk in ch.chunks(1 << 14) {
                    buf.clear();
                    for s in chunk {
                        buf.extend_from_slice(&s.to_le_bytes());
                    }
                    w.write_all(&buf)?;
                }
            }
        }
        w.into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()
            .context("flushing project file")?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.context(format!("saving {}", path.display())));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".saving");
    path.with_file_name(name)
}

/// A project read from disk.
pub struct Loaded {
    pub project: Project,
    pub view: Option<ViewState>,
}

/// Read a project saved with [`save`].
pub fn load(path: &Path) -> Result<Loaded> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)
        .context("not a Rust Audacity project")?;
    if &magic[..6] != b"RAPROJ" {
        bail!("{} is not a Rust Audacity project file", path.display());
    }
    if magic[7] > MAGIC[7] {
        bail!("this project was saved by a newer version of Rust Audacity");
    }
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let hlen = u64::from_le_bytes(len);
    if hlen > 64 << 20 || hlen > total {
        bail!("project header is damaged");
    }
    let mut json = vec![0u8; hlen as usize];
    r.read_exact(&mut json)?;
    let header: Header = serde_json::from_slice(&json).context("project header is damaged")?;

    let needed: u64 = header
        .tracks
        .iter()
        .map(|t| (t.channels * t.samples * 4) as u64)
        .sum();
    if 16 + hlen + needed > total {
        bail!("the project file is incomplete (it may have been truncated)");
    }

    let mut project = Project::default();
    project.rate = header.rate.max(1);
    let mut bytes = Vec::new();
    for th in header.tracks {
        if th.channels == 0 || th.channels > 8 {
            bail!("project header is damaged (track “{}”)", th.name);
        }
        let mut chans = Vec::with_capacity(th.channels);
        for _ in 0..th.channels {
            bytes.resize(th.samples * 4, 0);
            r.read_exact(&mut bytes)
                .context("the project file is incomplete")?;
            let ch: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            chans.push(ch);
        }
        let id = project.alloc_id();
        let mut t = Track::new(id, th.name, chans);
        t.gain = th.gain;
        t.pan = th.pan.clamp(-1.0, 1.0);
        t.mute = th.mute;
        t.solo = th.solo;
        t.selected = th.selected;
        t.height = th.height.clamp(60.0, 600.0);
        t.envelope = Envelope::from_points(
            th.envelope
                .iter()
                .map(|&(pos, db)| EnvPoint { pos, db })
                .collect(),
        );
        t.live_fx = th.live_fx;
        if let Some(clips) = th.clips {
            t.clips = clips;
            t.normalize_clips();
        }
        t.view = if th.spectrogram {
            TrackView::Spectrogram
        } else {
            TrackView::Waveform
        };
        project.tracks.push(t);
    }
    let [a, b] = header.selection;
    project.selection = Selection::new(a, b);
    if let Some(th) = header.transcript
        && let Some(t) = project.tracks.get(th.track)
    {
        project.transcript = Some(std::sync::Arc::new(Transcript {
            track_id: t.id,
            language: th.language,
            words: th.words,
        }));
    }
    project.labels = header.labels;
    project.metadata = header.metadata;
    crate::labels::sort(&mut project.labels);
    // Label ids must not collide with ids handed out later.
    project.ensure_ids_above(project.labels.iter().map(|l| l.id).max().unwrap_or(0));
    let max_fx = project
        .tracks
        .iter()
        .flat_map(|t| t.live_fx.iter().map(|f| f.id))
        .max()
        .unwrap_or(0);
    project.ensure_ids_above(max_fx);
    project.dirty = false;
    Ok(Loaded {
        project,
        view: header.view,
    })
}

/// True if the path looks like a project file.
pub fn is_project_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rap_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.rap");

        let mut p = Project::default();
        p.rate = 48000;
        let l: Vec<f32> = (0..5000).map(|i| (i as f32 * 0.37).sin()).collect();
        let r: Vec<f32> = l.iter().map(|x| -x * 0.25 + 1e-7).collect();
        p.add_track("Vocals", vec![l.clone(), r.clone()]);
        p.add_track("Bass ✓", vec![vec![0.5; 1234]]);
        p.tracks[0].gain = 0.5;
        p.tracks[0].pan = -0.3;
        p.tracks[1].mute = true;
        p.tracks[1].solo = true;
        p.tracks[1].selected = false;
        p.tracks[1].view = TrackView::Spectrogram;
        p.tracks[0].envelope = Envelope::from_points(vec![EnvPoint { pos: 7, db: -6.5 }]);
        p.selection = Selection::new(100, 900);
        p.tracks[0].split_clip_at(2000);
        p.tracks[0].clips[1].name = "Chorus".into();
        let mut fx = crate::dsp::realtime::LiveFx::builtin(
            900,
            crate::effects::Effect::Amplify { db: -6.0 },
        );
        fx.enabled = false;
        p.tracks[1].live_fx.push(fx);
        let mut pl = crate::dsp::realtime::LiveFx::plugin(
            901,
            crate::plugins::PluginDesc {
                format: crate::plugins::PluginFormat::Clap,
                path: "/x/y.clap".into(),
                id: "com.example.verb".into(),
                name: "Verb".into(),
                vendor: "Ex".into(),
                category: "audio-effect".into(),
                is_instrument: false,
            },
        );
        pl.plugin.as_mut().unwrap().state = vec![0, 1, 2, 250, 255];
        p.tracks[1].live_fx.push(pl);
        let view = ViewState {
            left: 12.5,
            samples_per_pixel: 64.0,
        };
        p.transcript = Some(std::sync::Arc::new(Transcript {
            track_id: p.tracks[1].id,
            language: "en".into(),
            words: vec![Word {
                start: 5,
                end: 9,
                text: "hi".into(),
                prob: 0.5,
            }],
        }));
        p.add_label(10, 20, "Verse");
        p.metadata.title = "My Song".into();
        save(&path, &p, Some(view)).unwrap();
        assert!(!temp_path(&path).exists());

        let loaded = load(&path).unwrap();
        let q = loaded.project;
        assert_eq!(loaded.view, Some(view));

        assert_eq!(q.rate, 48000);
        assert_eq!(q.selection, Selection::new(100, 900));
        assert_eq!(q.tracks.len(), 2);
        assert_eq!(q.tracks[0].name, "Vocals");
        assert_eq!(*q.tracks[0].channels[0], l, "audio must be bit-exact");
        assert_eq!(*q.tracks[0].channels[1], r);
        assert_eq!((q.tracks[0].gain, q.tracks[0].pan), (0.5, -0.3));
        assert_eq!(q.tracks[0].envelope, p.tracks[0].envelope);
        assert_eq!(q.tracks[1].name, "Bass ✓");
        assert!(q.tracks[1].mute && q.tracks[1].solo && !q.tracks[1].selected);
        assert_eq!(q.tracks[1].view, TrackView::Spectrogram);
        assert_eq!(q.tracks[1].len(), 1234);
        assert_eq!(q.labels, p.labels);
        assert_eq!(q.tracks[0].clips, p.tracks[0].clips);
        assert_eq!(q.tracks[0].clips[1].name, "Chorus");
        assert_eq!(q.tracks[1].live_fx, p.tracks[1].live_fx);
        let mut q2 = q;
        assert!(q2.alloc_id() > 901);
        let q = q2;
        assert_eq!(q.metadata.title, "My Song");
        let tr = q.transcript.as_ref().unwrap();
        assert_eq!(tr.track_id, q.tracks[1].id);
        assert_eq!(tr.words[0].text, "hi");
        assert!(!q.dirty);
        assert!(q.tracks[0].id != q.tracks[1].id);

        // A truncated file is rejected with a clear message.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
        let err = load(&path).err().unwrap().to_string();
        assert!(err.contains("incomplete"), "{err}");
        // So is a random file.
        std::fs::write(&path, b"hello world, not a project").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
