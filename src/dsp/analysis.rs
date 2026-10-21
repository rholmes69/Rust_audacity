//! Measurement: EBU R128 loudness, peaks, RMS, spectrum.

use ebur128::{EbuR128, Mode};

use super::stft::analyze;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub duration_secs: f64,
    pub peak_db: f32,
    pub true_peak_db: f32,
    pub rms_db: f32,
    /// Integrated loudness (LUFS); None if the audio is too short or silent.
    pub lufs: Option<f64>,
    /// Loudness range (LU).
    pub lra: Option<f64>,
    pub dc_offset: f32,
    pub crest_db: f32,
}

fn db(x: f32) -> f32 {
    if x <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * x.log10()
    }
}

/// Integrated loudness in LUFS of planar audio.
pub fn integrated_lufs(chans: &[Vec<f32>], rate: u32) -> Option<f64> {
    let mut m = EbuR128::new(chans.len() as u32, rate, Mode::I).ok()?;
    let refs: Vec<&[f32]> = chans.iter().map(|c| c.as_slice()).collect();
    m.add_frames_planar_f32(&refs).ok()?;
    m.loudness_global().ok().filter(|v| v.is_finite())
}

pub fn measure(chans: &[Vec<f32>], rate: u32) -> Stats {
    let len = chans.first().map_or(0, Vec::len);
    let mut st = Stats {
        duration_secs: len as f64 / rate as f64,
        ..Default::default()
    };
    if len == 0 {
        return st;
    }
    let mut peak = 0f32;
    let mut sumsq = 0f64;
    let mut sum = 0f64;
    for c in chans {
        for &v in c {
            peak = peak.max(v.abs());
            sumsq += (v as f64) * (v as f64);
            sum += v as f64;
        }
    }
    let n = (len * chans.len()) as f64;
    let rms = (sumsq / n).sqrt() as f32;
    st.peak_db = db(peak);
    st.rms_db = db(rms);
    st.dc_offset = (sum / n) as f32;
    st.crest_db = st.peak_db - st.rms_db;
    if let Ok(mut m) = EbuR128::new(
        chans.len() as u32,
        rate,
        Mode::I | Mode::LRA | Mode::TRUE_PEAK,
    ) {
        let refs: Vec<&[f32]> = chans.iter().map(|c| c.as_slice()).collect();
        if m.add_frames_planar_f32(&refs).is_ok() {
            st.lufs = m.loudness_global().ok().filter(|v| v.is_finite());
            st.lra = m
                .loudness_range()
                .ok()
                .filter(|v| v.is_finite() && st.lufs.is_some());
            let tp = (0..chans.len() as u32)
                .filter_map(|c| m.true_peak(c).ok())
                .fold(0f64, f64::max);
            st.true_peak_db = db(tp as f32);
        }
    }
    st
}

/// Averaged (Welch) power spectrum of a mono signal, in dBFS per bin.
pub fn spectrum(x: &[f32], n: usize) -> Vec<f32> {
    let bins = n / 2 + 1;
    let mut acc = vec![0f64; bins];
    let mut frames = 0usize;
    analyze(x, n, n / 2, |spec| {
        for (a, c) in acc.iter_mut().zip(spec) {
            *a += c.norm_sqr() as f64;
        }
        frames += 1;
    });
    // Hann window coherent gain is 0.5 → a full-scale sine reads ~0 dB.
    let norm = (n as f64 * 0.5 / 2.0).powi(2);
    acc.into_iter()
        .map(|p| {
            let v = p / frames.max(1) as f64 / norm;
            (10.0 * v.max(1e-20).log10()) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lufs_of_reference_tone() {
        // EBU: a 1 kHz sine at -18 dBFS peak, stereo, measures about -18 LUFS... (mono-sum differs).
        // Check a full-scale-ish stereo 1 kHz sine at -20 dBFS lands near -20 LUFS (+/- 1).
        let rate = 48000;
        let amp = 10f32.powf(-20.0 / 20.0);
        let s: Vec<f32> = (0..rate as usize * 5)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / rate as f32).sin() * amp)
            .collect();
        let l = integrated_lufs(&[s.clone(), s], rate).unwrap();
        assert!((l - -20.0).abs() < 1.0, "measured {l}");
    }

    #[test]
    fn spectrum_peak_at_tone() {
        let rate = 44100.0;
        let s: Vec<f32> = (0..44100)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / rate).sin())
            .collect();
        let sp = spectrum(&s, 4096);
        let (bin, &v) = sp
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        let f = bin as f32 * rate / 4096.0;
        assert!((f - 1000.0).abs() < 15.0);
        assert!(v > -3.0 && v < 1.0, "level {v}");
    }
}
