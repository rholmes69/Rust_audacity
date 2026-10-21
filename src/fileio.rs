//! Import (via Symphonia: WAV, FLAC, MP3, OGG/Vorbis, AAC/M4A, AIFF, ...) and WAV export (via hound).

use std::{fs::File, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::{
    codecs::audio::AudioDecoderOptions,
    errors::Error as SymError,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

pub struct DecodedAudio {
    /// Planar channels.
    pub channels: Vec<Vec<f32>>,
    pub rate: u32,
}

/// Decode an audio file of any supported format into planar f32.
pub fn import(path: &Path) -> Result<DecodedAudio> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unsupported or unrecognised audio format")?;

    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("file has no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("audio track has no codec parameters"))?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("no decoder for this codec")?;

    let mut rate = params.sample_rate.unwrap_or(0);
    let mut num_ch = params.channels.as_ref().map(|c| c.count()).unwrap_or(0);
    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut interleaved: Vec<f32> = Vec::new();

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            // Corrupt frames are skipped, like most players do.
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let spec = buf.spec();
        if rate == 0 {
            rate = spec.rate();
        }
        let ch = spec.channels().count().max(1);
        if num_ch == 0 {
            num_ch = ch;
        }
        if planar.is_empty() {
            planar = vec![Vec::new(); ch];
        }
        interleaved.clear();
        buf.copy_to_vec_interleaved(&mut interleaved);
        for frame in interleaved.chunks_exact(ch) {
            for (c, s) in planar.iter_mut().zip(frame) {
                c.push(*s);
            }
        }
    }

    if planar.is_empty() || planar[0].is_empty() {
        bail!("no audio could be decoded from {}", path.display());
    }
    if rate == 0 {
        rate = crate::project::DEFAULT_RATE;
    }
    // Keep at most stereo; fold extra channels into L/R.
    if planar.len() > 2 {
        let len = planar[0].len();
        let mut l = vec![0.0; len];
        let mut r = vec![0.0; len];
        for (i, ch) in planar.iter().enumerate() {
            let dst = if i % 2 == 0 { &mut l } else { &mut r };
            for (d, s) in dst.iter_mut().zip(ch) {
                *d += s;
            }
        }
        let k = 2.0 / planar.len() as f32;
        l.iter_mut().chain(r.iter_mut()).for_each(|x| *x *= k);
        planar = vec![l, r];
    }
    Ok(DecodedAudio {
        channels: planar,
        rate,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavFormat {
    Pcm16,
    Pcm24,
    Float32,
}

impl WavFormat {
    pub fn label(self) -> &'static str {
        match self {
            WavFormat::Pcm16 => "16-bit PCM",
            WavFormat::Pcm24 => "24-bit PCM",
            WavFormat::Float32 => "32-bit float",
        }
    }
}

/// Write planar audio to a WAV file (with TPDF dither for 16-bit).
pub fn export_wav(path: &Path, channels: &[Vec<f32>], rate: u32, fmt: WavFormat) -> Result<()> {
    if channels.is_empty() {
        bail!("nothing to export");
    }
    let (bits, sample_format) = match fmt {
        WavFormat::Pcm16 => (16, hound::SampleFormat::Int),
        WavFormat::Pcm24 => (24, hound::SampleFormat::Int),
        WavFormat::Float32 => (32, hound::SampleFormat::Float),
    };
    let spec = hound::WavSpec {
        channels: channels.len() as u16,
        sample_rate: rate,
        bits_per_sample: bits,
        sample_format,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    let len = channels[0].len();
    let mut seed: u32 = 0x1234_5678;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    };
    for i in 0..len {
        for ch in channels {
            let s = ch[i];
            match fmt {
                WavFormat::Float32 => w.write_sample(s)?,
                WavFormat::Pcm16 => {
                    let dither = (rnd() - rnd()) / 32768.0;
                    let v = ((s + dither).clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    w.write_sample(v)?
                }
                WavFormat::Pcm24 => {
                    let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                    w.write_sample(v)?
                }
            }
        }
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rust_audacity_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.wav");
        let l: Vec<f32> = (0..4410).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let r: Vec<f32> = l.iter().map(|x| -x).collect();
        for fmt in [WavFormat::Pcm16, WavFormat::Pcm24, WavFormat::Float32] {
            export_wav(&path, &[l.clone(), r.clone()], 44100, fmt).unwrap();
            let d = import(&path).unwrap();
            assert_eq!(d.rate, 44100);
            assert_eq!(d.channels.len(), 2);
            assert_eq!(d.channels[0].len(), l.len());
            assert!((d.channels[0][100] - l[100]).abs() < 1e-3, "{fmt:?}");
            assert!((d.channels[1][100] - r[100]).abs() < 1e-3, "{fmt:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
