//! Spectral editing: change only a box of time × frequency (select it on the spectrogram).
//! Edges are tapered in both time and frequency so the edit doesn't click or ring.

use serde::{Deserialize, Serialize};

use super::stft::{bin_freq, stft_map};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum SpectralOp {
    /// Remove the selected frequencies in the selected time.
    Delete,
    /// Turn them down by this many dB.
    Reduce(f32),
    /// Keep only the selected frequencies (in the selected time).
    Isolate,
    /// Replace them with what surrounds them in time (good for coughs, clicks, beeps).
    Heal,
}

impl SpectralOp {
    pub fn name(&self) -> String {
        match self {
            SpectralOp::Delete => "Spectral Delete".into(),
            SpectralOp::Reduce(db) => format!("Spectral Reduce {db:.0} dB"),
            SpectralOp::Isolate => "Spectral Isolate".into(),
            SpectralOp::Heal => "Spectral Heal".into(),
        }
    }
}

const N: usize = 2048;
const HOP: usize = 256;

/// How much of a frame centred at `c` is inside [start, end): 1 inside, a raised-cosine
/// fade over `taper` samples outside.
fn time_weight(c: f64, start: f64, end: f64, taper: f64) -> f32 {
    let d = if c < start {
        start - c
    } else if c >= end {
        c - end
    } else {
        return 1.0;
    };
    if d >= taper {
        0.0
    } else {
        (0.5 + 0.5 * (std::f64::consts::PI * d / taper).cos()) as f32
    }
}

/// Same idea in frequency, with the fade measured in octaves.
fn freq_weight(f: f32, lo: f32, hi: f32, taper_oct: f32) -> f32 {
    if f <= 0.0 {
        return if lo <= 0.0 { 1.0 } else { 0.0 };
    }
    let d = if f < lo {
        (lo / f).log2()
    } else if f > hi {
        (f / hi).log2()
    } else {
        return 1.0;
    };
    if d >= taper_oct {
        0.0
    } else {
        0.5 + 0.5 * (std::f32::consts::PI * d / taper_oct).cos()
    }
}

/// Apply `op` to the box [start, end) × [lo_hz, hi_hz] of one channel, in place.
pub fn apply(
    ch: &mut [f32],
    rate: u32,
    start: usize,
    end: usize,
    lo_hz: f32,
    hi_hz: f32,
    op: SpectralOp,
) {
    let len = ch.len();
    let (start, end) = (start.min(len), end.min(len));
    if start >= end || hi_hz <= lo_hz {
        return;
    }
    let r = rate as f32;
    let taper = (0.008 * r as f64).max(64.0); // ~8 ms
    // Work on the box plus enough context for the transform and the tapers.
    let pad = N + taper as usize + HOP;
    let s0 = start.saturating_sub(pad);
    let e0 = (end + pad).min(len);
    let seg = &ch[s0..e0];
    let (st, en) = ((start - s0) as f64, (end - s0) as f64);
    // Frame k covers padded positions [k·HOP, k·HOP + N), i.e. segment samples starting
    // at k·HOP − N; its centre is k·HOP − N/2.
    let centre = |k: usize| k as f64 * HOP as f64 - (N / 2) as f64;
    let bins = N / 2 + 1;
    let fw: Vec<f32> = (0..bins)
        .map(|b| freq_weight(bin_freq(b, N, r), lo_hz, hi_hz, 1.0 / 6.0))
        .collect();

    // Healing needs the surroundings' spectrum: average magnitudes just before and just
    // after the box.
    let reference: Option<(Vec<f32>, Vec<f32>)> = if op == SpectralOp::Heal {
        let mut before = vec![0f32; bins];
        let mut after = vec![0f32; bins];
        let (mut nb, mut na) = (0f32, 0f32);
        let ctx = (taper + N as f64) * 1.0;
        let _ = stft_map(&[seg], 1, N, HOP, |k, i, o| {
            let c = centre(k);
            let s = &i[0];
            if c < st - taper && c >= st - taper - ctx {
                for (b, v) in before.iter_mut().zip(s) {
                    *b += v.norm();
                }
                nb += 1.0;
            } else if c >= en + taper && c < en + taper + ctx {
                for (a, v) in after.iter_mut().zip(s) {
                    *a += v.norm();
                }
                na += 1.0;
            }
            o[0].clone_from(s);
        });
        if nb > 0.0 {
            before.iter_mut().for_each(|x| *x /= nb);
        }
        if na > 0.0 {
            after.iter_mut().for_each(|x| *x /= na);
        }
        // With context on one side only, use it for both.
        if nb == 0.0 {
            before.clone_from(&after);
        }
        if na == 0.0 {
            after.clone_from(&before);
        }
        Some((before, after))
    } else {
        None
    };

    let out = stft_map(&[seg], 1, N, HOP, |k, i, o| {
        let c = centre(k);
        let wt = time_weight(c, st, en, taper);
        let s = &i[0];
        let d = &mut o[0];
        if wt <= 0.0 {
            d.clone_from(s);
            return;
        }
        let along = ((c - st) / (en - st).max(1.0)).clamp(0.0, 1.0) as f32;
        for b in 0..bins {
            let m = wt * fw[b];
            let g = match op {
                SpectralOp::Delete => 1.0 - m,
                SpectralOp::Reduce(db) => 1.0 - m * (1.0 - 10f32.powf(-db.abs() / 20.0)),
                SpectralOp::Isolate => 1.0 - wt * (1.0 - fw[b]),
                SpectralOp::Heal => {
                    let (before, after) = reference.as_ref().expect("computed above");
                    let target = before[b] + (after[b] - before[b]) * along;
                    let mag = s[b].norm();
                    // Only ever turn things down to the surroundings' level.
                    let ratio = if mag > 1e-12 {
                        (target / mag).min(1.0)
                    } else {
                        1.0
                    };
                    1.0 - m * (1.0 - ratio)
                }
            };
            d[b] = s[b] * g;
        }
    });
    // Write back only where something could have changed.
    let w0 = start.saturating_sub(taper as usize + N).max(s0);
    let w1 = (end + taper as usize + N).min(e0);
    ch[w0..w1].copy_from_slice(&out[0][w0 - s0..w1 - s0]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f32, n: usize, rate: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * f / rate).sin() * 0.5)
            .collect()
    }

    fn band_energy(x: &[f32], f: f32, rate: f32) -> f32 {
        // Goertzel-style single-bin energy.
        let w = 2.0 * std::f32::consts::PI * f / rate;
        let (mut re, mut im) = (0f32, 0f32);
        for (i, v) in x.iter().enumerate() {
            re += v * (w * i as f32).cos();
            im += v * (w * i as f32).sin();
        }
        (re * re + im * im).sqrt() / x.len() as f32
    }

    #[test]
    fn delete_removes_only_the_box() {
        let rate = 44100.0;
        let n = 44100;
        let low = tone(300.0, n, rate);
        let high = tone(5000.0, n, rate);
        let mut x: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        let orig = x.clone();
        apply(
            &mut x,
            44100,
            15000,
            30000,
            4000.0,
            6000.0,
            SpectralOp::Delete,
        );
        let inside = 18000..27000;
        assert!(
            band_energy(&x[inside.clone()], 5000.0, rate)
                < 0.01 * band_energy(&orig[inside.clone()], 5000.0, rate)
        );
        let kept =
            band_energy(&x[inside.clone()], 300.0, rate) / band_energy(&orig[inside], 300.0, rate);
        assert!((kept - 1.0).abs() < 0.02, "low tone kept: {kept}");
        // Outside the time range nothing changes.
        assert!(
            x[..10000]
                .iter()
                .zip(&orig[..10000])
                .all(|(a, b)| (a - b).abs() < 1e-4)
        );
        assert!(
            x[36000..]
                .iter()
                .zip(&orig[36000..])
                .all(|(a, b)| (a - b).abs() < 1e-4)
        );
    }

    #[test]
    fn heal_fills_a_burst_from_its_surroundings() {
        let rate = 44100.0;
        let n = 44100;
        let mut x = tone(440.0, n, rate);
        // A loud 2–4 kHz "cough" in the middle.
        for i in 20000..23000 {
            x[i] += (i as f32 * 0.9).sin() * 0.8 * (i as f32 * 0.37).cos();
        }
        let before = x.clone();
        apply(
            &mut x,
            44100,
            20000,
            23000,
            1500.0,
            20000.0,
            SpectralOp::Heal,
        );
        let rms = |v: &[f32]| (v.iter().map(|a| a * a).sum::<f32>() / v.len() as f32).sqrt();
        // The burst (components near 3.7 and 8.9 kHz) is mostly gone…
        let f = 0.53 * rate / (2.0 * std::f32::consts::PI);
        let left =
            band_energy(&x[20500..22500], f, rate) / band_energy(&before[20500..22500], f, rate);
        assert!(left < 0.2, "burst left: {left}");
        assert!(rms(&x[20500..22500]) < 0.75 * rms(&before[20500..22500]));
        // The 440 Hz tone underneath survives.
        let kept = band_energy(&x[20500..22500], 440.0, rate)
            / band_energy(&before[20500..22500], 440.0, rate);
        assert!(kept > 0.9, "tone kept: {kept}");
    }

    #[test]
    fn reduce_and_isolate() {
        let rate = 44100.0;
        let n = 30000;
        let mut x: Vec<f32> = tone(300.0, n, rate)
            .iter()
            .zip(tone(5000.0, n, rate))
            .map(|(a, b)| a + b)
            .collect();
        let orig = x.clone();
        apply(
            &mut x,
            44100,
            0,
            n,
            4000.0,
            6000.0,
            SpectralOp::Reduce(12.0),
        );
        let r = band_energy(&x[5000..25000], 5000.0, rate)
            / band_energy(&orig[5000..25000], 5000.0, rate);
        assert!((r - 0.251).abs() < 0.03, "−12 dB → {r}");
        let mut y = orig.clone();
        apply(&mut y, 44100, 0, n, 4000.0, 6000.0, SpectralOp::Isolate);
        assert!(
            band_energy(&y[5000..25000], 300.0, rate)
                < 0.01 * band_energy(&orig[5000..25000], 300.0, rate)
        );
        assert!(
            band_energy(&y[5000..25000], 5000.0, rate)
                > 0.95 * band_energy(&orig[5000..25000], 5000.0, rate)
        );
    }
}
