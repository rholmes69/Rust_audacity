//! Import (via Symphonia: WAV, FLAC, MP3, OGG/Vorbis, AAC/M4A, AIFF, ...) and export to
//! WAV (hound), FLAC (flacenc), MP3 (LAME) and Ogg Vorbis (libvorbis/aoTuV).

use std::{
    fs::File,
    io::{BufWriter, Write},
    num::{NonZeroU8, NonZeroU32},
    path::Path,
};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    let mut rnd = dither_source();
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

/// Simple xorshift noise source for TPDF dither.
fn dither_source() -> impl FnMut() -> f32 {
    let mut seed: u32 = 0x1234_5678;
    move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32
    }
}

/// Every export format the app can write.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExportFormat {
    Wav(WavFormat),
    /// Lossless FLAC, 16- or 24-bit.
    Flac {
        bits: u8,
    },
    /// MP3 at a constant bitrate (kbps).
    Mp3 {
        kbps: u32,
    },
    /// Ogg Vorbis with a quality setting from 0 (small) to 10 (best).
    Ogg {
        quality: u8,
    },
}

impl Default for ExportFormat {
    fn default() -> Self {
        ExportFormat::Wav(WavFormat::Pcm16)
    }
}

/// Bitrates offered for MP3 export.
pub const MP3_BITRATES: [u32; 8] = [64, 96, 128, 160, 192, 224, 256, 320];

impl ExportFormat {
    /// All format families with sensible default settings (for menus).
    pub fn families() -> [ExportFormat; 4] {
        [
            ExportFormat::Wav(WavFormat::Pcm16),
            ExportFormat::Flac { bits: 16 },
            ExportFormat::Mp3 { kbps: 192 },
            ExportFormat::Ogg { quality: 6 },
        ]
    }

    pub fn family_name(&self) -> &'static str {
        match self {
            ExportFormat::Wav(_) => "WAV",
            ExportFormat::Flac { .. } => "FLAC",
            ExportFormat::Mp3 { .. } => "MP3",
            ExportFormat::Ogg { .. } => "Ogg Vorbis",
        }
    }

    pub fn same_family(&self, other: &ExportFormat) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Wav(_) => "wav",
            ExportFormat::Flac { .. } => "flac",
            ExportFormat::Mp3 { .. } => "mp3",
            ExportFormat::Ogg { .. } => "ogg",
        }
    }

    pub fn label(&self) -> String {
        match self {
            ExportFormat::Wav(w) => format!("WAV, {}", w.label()),
            ExportFormat::Flac { bits } => format!("FLAC, {bits}-bit lossless"),
            ExportFormat::Mp3 { kbps } => format!("MP3, {kbps} kbps"),
            ExportFormat::Ogg { quality } => format!("Ogg Vorbis, quality {quality}"),
        }
    }

    /// Lossy formats and integer formats cannot carry levels above 0 dBFS.
    pub fn clips_above_full_scale(&self) -> bool {
        !matches!(self, ExportFormat::Wav(WavFormat::Float32))
    }

    /// Guess a format from a file extension.
    pub fn from_extension(ext: &str) -> Option<ExportFormat> {
        match ext.to_ascii_lowercase().as_str() {
            "wav" | "wave" => Some(ExportFormat::Wav(WavFormat::Pcm16)),
            "flac" => Some(ExportFormat::Flac { bits: 16 }),
            "mp3" => Some(ExportFormat::Mp3 { kbps: 192 }),
            "ogg" | "oga" => Some(ExportFormat::Ogg { quality: 6 }),
            _ => None,
        }
    }
}

/// Write planar audio in any supported format.
pub fn export(path: &Path, channels: &[Vec<f32>], rate: u32, fmt: ExportFormat) -> Result<()> {
    if channels.is_empty() || channels[0].is_empty() {
        bail!("nothing to export");
    }
    match fmt {
        ExportFormat::Wav(w) => export_wav(path, channels, rate, w),
        ExportFormat::Flac { bits } => export_flac(path, channels, rate, bits),
        ExportFormat::Mp3 { kbps } => export_mp3(path, channels, rate, kbps),
        ExportFormat::Ogg { quality } => export_ogg(path, channels, rate, quality),
    }
}

/// Lossless FLAC (16-bit is dithered, 24-bit is rounded).
pub fn export_flac(path: &Path, channels: &[Vec<f32>], rate: u32, bits: u8) -> Result<()> {
    use flacenc::{component::BitRepr, error::Verify};
    let bits: usize = if bits >= 24 { 24 } else { 16 };
    let nch = channels.len().min(8);
    let len = channels[0].len();
    let scale = ((1i64 << (bits - 1)) - 1) as f32;
    let mut rnd = dither_source();
    let mut samples = Vec::with_capacity(len * nch);
    for i in 0..len {
        for ch in &channels[..nch] {
            let d = if bits == 16 {
                (rnd() - rnd()) / 32768.0
            } else {
                0.0
            };
            samples.push(((ch[i] + d).clamp(-1.0, 1.0) * scale).round() as i32);
        }
    }
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow!("FLAC encoder setup failed: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(&samples, nch, bits, rate as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow!("FLAC encoding failed: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow!("FLAC encoding failed: {e:?}"))?;
    let mut bytes = sink.as_slice().to_vec();
    // flacenc stores the (shorter) final block as the minimum block size, which makes some
    // decoders (Symphonia) treat this fixed-block-size stream as variable. The FLAC spec says the
    // minimum excludes the last block, so set it equal to the maximum.
    if bytes.len() > 12 && &bytes[..4] == b"fLaC" && bytes[4] & 0x7f == 0 {
        let (min, max) = (8..10, 10..12);
        let m = bytes[max].to_vec();
        bytes[min].copy_from_slice(&m);
    }
    std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))
}

/// Sample rates LAME can write directly.
const MP3_RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

/// Constant-bitrate MP3 via LAME.
pub fn export_mp3(path: &Path, channels: &[Vec<f32>], rate: u32, kbps: u32) -> Result<()> {
    use mp3lame_encoder::{Bitrate, Builder, DualPcm, FlushNoGap, MonoPcm, Quality};
    // MP3 only supports a fixed set of rates: resample anything else to the nearest one above.
    let out_rate = if MP3_RATES.contains(&rate) {
        rate
    } else {
        MP3_RATES
            .iter()
            .copied()
            .find(|&r| r >= rate)
            .unwrap_or(48000)
    };
    let resampled;
    let channels = if out_rate != rate {
        resampled = crate::track::resample(channels, rate, out_rate);
        &resampled[..]
    } else {
        channels
    };
    let stereo = channels.len() > 1;
    let brate = match kbps {
        0..=79 => Bitrate::Kbps64,
        80..=111 => Bitrate::Kbps96,
        112..=143 => Bitrate::Kbps128,
        144..=175 => Bitrate::Kbps160,
        176..=207 => Bitrate::Kbps192,
        208..=239 => Bitrate::Kbps224,
        240..=287 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    };
    let lame_err =
        |what: &str, e: &dyn std::fmt::Debug| anyhow!("MP3 encoder: {what} failed ({e:?})");
    let mut b = Builder::new().ok_or_else(|| anyhow!("could not start the MP3 encoder"))?;
    b.set_num_channels(if stereo { 2 } else { 1 })
        .map_err(|e| lame_err("channels", &e))?;
    b.set_sample_rate(out_rate)
        .map_err(|e| lame_err("sample rate", &e))?;
    b.set_brate(brate).map_err(|e| lame_err("bitrate", &e))?;
    b.set_quality(Quality::Best)
        .map_err(|e| lame_err("quality", &e))?;
    b.set_to_write_vbr_tag(false)
        .map_err(|e| lame_err("tag", &e))?;
    let mut enc = b.build().map_err(|e| lame_err("setup", &e))?;

    let clamp = |c: &[f32]| c.iter().map(|x| x.clamp(-1.0, 1.0)).collect::<Vec<f32>>();
    let left = clamp(&channels[0]);
    let right = if stereo {
        clamp(&channels[1])
    } else {
        Vec::new()
    };
    let mut out: Vec<u8> = Vec::new();
    const CHUNK: usize = 1152 * 32;
    for start in (0..left.len()).step_by(CHUNK) {
        let end = (start + CHUNK).min(left.len());
        out.reserve(mp3lame_encoder::max_required_buffer_size(end - start));
        if stereo {
            enc.encode_to_vec(
                DualPcm {
                    left: &left[start..end],
                    right: &right[start..end],
                },
                &mut out,
            )
        } else {
            enc.encode_to_vec(MonoPcm(&left[start..end]), &mut out)
        }
        .map_err(|e| lame_err("encoding", &e))?;
    }
    out.reserve(7200);
    enc.flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|e| lame_err("flush", &e))?;
    std::fs::write(path, &out).with_context(|| format!("writing {}", path.display()))
}

/// Ogg Vorbis (quality 0–10, like most encoders).
pub fn export_ogg(path: &Path, channels: &[Vec<f32>], rate: u32, quality: u8) -> Result<()> {
    use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};
    let file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let nch = NonZeroU8::new(channels.len().clamp(1, 8) as u8).unwrap();
    let rate_nz = NonZeroU32::new(rate).ok_or_else(|| anyhow!("invalid sample rate"))?;
    let serial = (rate as i32) ^ (channels[0].len() as i32) ^ 0x5241_4f47;
    let mut builder = VorbisEncoderBuilder::new_with_serial(rate_nz, nch, file, serial);
    builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: (quality.min(10) as f32 / 10.0).clamp(-0.1, 1.0),
    });
    let _ = builder.comment_tag("ENCODER", "Rust Audacity");
    let mut enc = builder
        .build()
        .map_err(|e| anyhow!("Ogg Vorbis encoder setup failed: {e}"))?;
    let chans: Vec<Vec<f32>> = channels[..nch.get() as usize]
        .iter()
        .map(|c| c.iter().map(|x| x.clamp(-1.0, 1.0)).collect())
        .collect();
    const CHUNK: usize = 4096;
    let len = chans[0].len();
    for start in (0..len).step_by(CHUNK) {
        let end = (start + CHUNK).min(len);
        let block: Vec<&[f32]> = chans.iter().map(|c| &c[start..end]).collect();
        enc.encode_audio_block(&block)
            .map_err(|e| anyhow!("Ogg Vorbis encoding failed: {e}"))?;
    }
    let mut w = enc
        .finish()
        .map_err(|e| anyhow!("Ogg Vorbis encoding failed: {e}"))?;
    w.flush()?;
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

    /// Every compressed format must decode back (via Symphonia) to audio of the
    /// same rate, channel count, roughly the same length and similar content.
    #[test]
    fn compressed_formats_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rust_audacity_fmt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 44100;
        let n = rate as usize; // 1 s
        let l: Vec<f32> = (0..n)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5)
            .collect();
        let r: Vec<f32> = l.iter().map(|x| x * 0.5).collect();
        for fmt in [
            ExportFormat::Flac { bits: 16 },
            ExportFormat::Flac { bits: 24 },
            ExportFormat::Mp3 { kbps: 192 },
            ExportFormat::Ogg { quality: 6 },
        ] {
            let path = dir.join(format!("rt.{}", fmt.extension()));
            export(&path, &[l.clone(), r.clone()], rate, fmt).unwrap();
            let d = import(&path).unwrap_or_else(|e| panic!("{fmt:?}: {e:#}"));
            assert_eq!(d.rate, rate, "{fmt:?}");
            assert_eq!(d.channels.len(), 2, "{fmt:?}");
            let got = d.channels[0].len() as i64;
            assert!(
                (got - n as i64).abs() < 3000,
                "{fmt:?}: length {got} vs {n}"
            );
            let rms = |c: &[f32]| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt();
            let (rl, rr) = (rms(&d.channels[0]), rms(&d.channels[1]));
            assert!((rl - 0.3536).abs() < 0.03, "{fmt:?}: left rms {rl}");
            assert!(
                (rr / rl - 0.5).abs() < 0.05,
                "{fmt:?}: right/left {}",
                rr / rl
            );
            if matches!(fmt, ExportFormat::Flac { .. }) {
                assert_eq!(got, n as i64);
                assert!((d.channels[0][1234] - l[1234]).abs() < 1e-3);
            }
        }
        // MP3 at an unsupported rate is resampled.
        let path = dir.join("rt96.mp3");
        export(
            &path,
            std::slice::from_ref(&l),
            96000,
            ExportFormat::Mp3 { kbps: 128 },
        )
        .unwrap();
        assert_eq!(import(&path).unwrap().rate, 48000);
        std::fs::remove_dir_all(&dir).ok();
    }
}
