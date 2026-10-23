//! Spectral noise reduction (noise-profile gate), in the spirit of Audacity's
//! "Noise Reduction": learn a noise fingerprint from a noise-only selection,
//! then attenuate each frequency bin while it stays near that fingerprint.

use serde::{Deserialize, Serialize};

use super::stft::{analyze, stft_map};

pub const FFT_N: usize = 2048;
const HOP: usize = FFT_N / 4;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NoiseProfile {
    pub rate: u32,
    /// Mean power per bin.
    pub power: Vec<f32>,
    pub frames: usize,
}

/// Build a noise profile from the given channels (all channels are pooled).
pub fn learn_profile(channels: &[&[f32]], rate: u32) -> Option<NoiseProfile> {
    let bins = FFT_N / 2 + 1;
    let mut power = vec![0f64; bins];
    let mut frames = 0usize;
    for ch in channels {
        if ch.len() < FFT_N / 2 {
            continue;
        }
        analyze(ch, FFT_N, HOP, |spec| {
            for (p, c) in power.iter_mut().zip(spec) {
                *p += c.norm_sqr() as f64;
            }
            frames += 1;
        });
    }
    if frames == 0 {
        return None;
    }
    Some(NoiseProfile {
        rate,
        power: power
            .into_iter()
            .map(|p| (p / frames as f64) as f32)
            .collect(),
        frames,
    })
}

/// Learn a profile from the quietest half-second of the audio (for unattended use:
/// batch processing and AI agents). None if the audio is too short or silent.
pub fn auto_profile(channels: &[Vec<f32>], rate: u32) -> Option<NoiseProfile> {
    let win = (rate / 2) as usize;
    let len = channels.first()?.len();
    if len < win || win == 0 {
        return None;
    }
    let mut best: Option<(f32, usize)> = None;
    let mut i = 0;
    while i + win <= len {
        let e: f32 = channels
            .iter()
            .map(|c| c[i..i + win].iter().map(|x| x * x).sum::<f32>())
            .sum::<f32>()
            / (win * channels.len()) as f32;
        if e > 1e-12 && best.is_none_or(|(be, _)| e < be) {
            best = Some((e, i));
        }
        i += win / 4;
    }
    let (_, at) = best?;
    let refs: Vec<&[f32]> = channels.iter().map(|c| &c[at..at + win]).collect();
    learn_profile(&refs, rate)
}

/// Apply noise reduction to one channel.
/// * `reduction_db` – how far to turn noise down (e.g. 12 dB)
/// * `sensitivity_db` – how far above the noise floor a bin must be to count as signal (e.g. 6 dB)
/// * `smoothing_bins` – frequency smoothing of the gain mask (reduces "musical noise")
pub fn reduce(
    x: &[f32],
    profile: &NoiseProfile,
    reduction_db: f32,
    sensitivity_db: f32,
    smoothing_bins: usize,
    rate: u32,
) -> Vec<f32> {
    let floor = 10f32.powf(-reduction_db.abs() / 20.0);
    let thresh = 10f32.powf(sensitivity_db / 10.0);
    let bins = FFT_N / 2 + 1;
    // Release ≈ 150 ms in frames; attack is instant.
    let frames_per_sec = rate as f32 / HOP as f32;
    let release = (-1.0 / (0.15 * frames_per_sec)).exp();
    let mut prev = vec![floor; bins];
    let mut raw = vec![0f32; bins];
    let mut smooth = vec![0f32; bins];
    let out = stft_map(&[x], 1, FFT_N, HOP, |_, input, output| {
        let spec = &input[0];
        for b in 0..bins {
            let signal = spec[b].norm_sqr() > profile.power[b] * thresh;
            raw[b] = if signal { 1.0 } else { floor };
        }
        // Frequency smoothing (moving average over ±smoothing_bins).
        let k = smoothing_bins as isize;
        for b in 0..bins as isize {
            let (lo, hi) = ((b - k).max(0) as usize, ((b + k) as usize).min(bins - 1));
            smooth[b as usize] = raw[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
        }
        // Time smoothing: instant attack, exponential release toward the floor.
        for b in 0..bins {
            let g = smooth[b].max(prev[b] * release).max(floor);
            prev[b] = g;
            output[0][b] = spec[b] * g;
        }
    });
    out.into_iter().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn reduces_noise_keeps_tone() {
        let rate = 44100;
        let n = noise(rate as usize, 0.05, 7);
        let profile = learn_profile(&[&n], rate).unwrap();
        // Noise-only section should drop by roughly the reduction amount.
        let other_noise = noise(rate as usize, 0.05, 99);
        let cleaned = reduce(&other_noise, &profile, 18.0, 6.0, 3, rate);
        let drop = 20.0 * (rms(&cleaned[4096..40000]) / rms(&other_noise[4096..40000])).log10();
        assert!(drop < -12.0, "noise only dropped {drop} dB");
        // A loud tone over the noise must survive.
        let tone: Vec<f32> = (0..rate as usize)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / rate as f32).sin() * 0.5)
            .collect();
        let mixed: Vec<f32> = tone.iter().zip(&other_noise).map(|(a, b)| a + b).collect();
        let cleaned = reduce(&mixed, &profile, 18.0, 6.0, 3, rate);
        let ratio = rms(&cleaned[4096..40000]) / rms(&tone[4096..40000]);
        assert!((0.9..1.1).contains(&ratio), "tone level ratio {ratio}");
    }
}
