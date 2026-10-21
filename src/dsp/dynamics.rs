//! Dynamics processors. All are stereo-linked: one gain curve is computed from
//! the loudest channel and applied to every channel, so the image doesn't shift.

use std::collections::VecDeque;

use super::biquad::{BUTTERWORTH_Q, Biquad};

fn coef(ms: f32, rate: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms * 0.001 * rate)).exp()
    }
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn linked_peak(chans: &[Vec<f32>], i: usize) -> f32 {
    chans.iter().fold(0f32, |m, c| m.max(c[i].abs()))
}

/// Feed-forward compressor with a soft knee.
pub fn compress(
    chans: &mut [Vec<f32>],
    rate: f32,
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    knee_db: f32,
    makeup_db: f32,
) {
    let Some(len) = chans.first().map(Vec::len) else {
        return;
    };
    let (att, rel) = (coef(attack_ms, rate), coef(release_ms, rate));
    let ratio = ratio.max(1.0);
    let makeup = lin(makeup_db);
    let mut env_db = -120f32;
    for i in 0..len {
        let level = db(linked_peak(chans, i));
        let c = if level > env_db { att } else { rel };
        env_db = level + c * (env_db - level);
        // Gain computer with soft knee.
        let over = env_db - threshold_db;
        let gr = if 2.0 * over < -knee_db {
            0.0
        } else if knee_db > 0.0 && 2.0 * over.abs() <= knee_db {
            (1.0 / ratio - 1.0) * (over + knee_db / 2.0).powi(2) / (2.0 * knee_db)
        } else {
            (1.0 / ratio - 1.0) * over
        };
        let g = lin(gr) * makeup;
        for ch in chans.iter_mut() {
            ch[i] *= g;
        }
    }
}

/// Brick-wall look-ahead limiter: guarantees no sample exceeds `ceiling_db`.
pub fn limit(chans: &mut [Vec<f32>], rate: f32, ceiling_db: f32, release_ms: f32) {
    let Some(len) = chans.first().map(Vec::len) else {
        return;
    };
    let ceiling = lin(ceiling_db);
    let look = ((rate * 0.005) as usize).max(1); // 5 ms look-ahead
    let required: Vec<f32> = (0..len)
        .map(|i| (ceiling / linked_peak(chans, i).max(1e-9)).min(1.0))
        .collect();
    // m[n] = min(required[n ..= n+look]) via monotonic deque.
    let mut m = vec![1f32; len];
    let mut dq: VecDeque<usize> = VecDeque::new();
    for j in (0..len).rev() {
        while dq.back().is_some_and(|&b| required[b] >= required[j]) {
            dq.pop_back();
        }
        dq.push_back(j);
        while dq.front().is_some_and(|&f| f > j + look) {
            dq.pop_front();
        }
        m[j] = required[*dq.front().unwrap()];
    }
    // Moving average (backward) smooths the attack ramp; it never exceeds the required gain at a peak.
    let mut avg = vec![1f32; len];
    let mut sum = look as f32; // window initially filled with 1.0
    let mut win: VecDeque<f32> = std::iter::repeat_n(1.0, look).collect();
    for i in 0..len {
        sum += m[i] - win.pop_front().unwrap();
        win.push_back(m[i]);
        avg[i] = (sum / look as f32).min(1.0);
    }
    let rel = coef(release_ms, rate);
    let mut g = 1f32;
    for i in 0..len {
        let target = avg[i];
        g = if target < g {
            target
        } else {
            target + rel * (g - target)
        };
        for ch in chans.iter_mut() {
            ch[i] *= g;
            // Final safety clamp for float rounding.
            ch[i] = ch[i].clamp(-ceiling, ceiling);
        }
    }
}

/// Noise gate: turns audio down by `reduction_db` when it falls below `threshold_db`.
pub fn gate(
    chans: &mut [Vec<f32>],
    rate: f32,
    threshold_db: f32,
    reduction_db: f32,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
) {
    let Some(len) = chans.first().map(Vec::len) else {
        return;
    };
    let thr = lin(threshold_db);
    let floor = lin(-reduction_db.abs());
    let env_rel = coef(20.0, rate);
    let (att, rel) = (coef(attack_ms, rate), coef(release_ms, rate));
    let hold = (hold_ms * 0.001 * rate) as usize;
    let mut env = 0f32;
    let mut held = 0usize;
    let mut g = floor;
    for i in 0..len {
        let p = linked_peak(chans, i);
        env = if p > env { p } else { p + env_rel * (env - p) };
        let open = if env >= thr {
            held = hold;
            true
        } else if held > 0 {
            held -= 1;
            true
        } else {
            false
        };
        let target = if open { 1.0 } else { floor };
        let c = if target > g { att } else { rel };
        g = target + c * (g - target);
        for ch in chans.iter_mut() {
            ch[i] *= g;
        }
    }
}

/// Split-band de-esser: reduces only the high band (above ~`freq_hz`) while
/// sibilance in that band exceeds `threshold_db`.
pub fn de_ess(
    chans: &mut [Vec<f32>],
    rate: f32,
    threshold_db: f32,
    freq_hz: f32,
    max_reduction_db: f32,
) {
    let Some(len) = chans.first().map(Vec::len) else {
        return;
    };
    // Detector: band-pass around the sibilance band, stereo-linked.
    let mut det: Vec<Biquad> = chans
        .iter()
        .map(|_| Biquad::bandpass(freq_hz * 1.4, 1.0, rate))
        .collect();
    // Complementary split: y = g·x + (1−g)·LP(x). With g = 1 the signal is untouched;
    // as g drops, only content above the (4th-order) low-pass is turned down.
    let mut split: Vec<[Biquad; 2]> = chans
        .iter()
        .map(|_| [Biquad::lowpass(freq_hz * 0.7, BUTTERWORTH_Q, rate); 2])
        .collect();
    let thr = lin(threshold_db);
    let (att, rel) = (coef(1.0, rate), coef(60.0, rate));
    let floor = lin(-max_reduction_db.abs());
    let mut env = 0f32;
    for i in 0..len {
        let mut p = 0f32;
        for (c, d) in chans.iter().zip(det.iter_mut()) {
            p = p.max(d.tick(c[i]).abs());
        }
        let co = if p > env { att } else { rel };
        env = p + co * (env - p);
        let g = if env > thr {
            (thr / env).max(floor)
        } else {
            1.0
        };
        for (c, s) in chans.iter_mut().zip(split.iter_mut()) {
            let stage1 = s[0].tick(c[i]);
            let low = s[1].tick(stage1);
            c[i] = g * c[i] + (1.0 - g) * low;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amp: f32, freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * freq / 44100.0).sin() * amp)
            .collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut x = vec![tone(0.3, 220.0, 44100)];
        for i in (1000..44100).step_by(5000) {
            x[0][i] = 1.5; // spikes
        }
        limit(&mut x, 44100.0, -1.0, 50.0);
        assert!(peak(&x[0]) <= lin(-1.0) + 1e-6);
        // Quiet material far from spikes is mostly untouched.
        assert!(peak(&x[0][3000..5500]) > 0.25);
    }

    #[test]
    fn compressor_reduces_loud_signal() {
        let mut x = vec![tone(0.9, 440.0, 44100)];
        compress(&mut x, 44100.0, -20.0, 4.0, 5.0, 100.0, 0.0, 0.0);
        let p = peak(&x[0][22050..]);
        // 0.9 ≈ -0.9 dBFS → 19.1 dB over → reduced to ~4.8 dB over → ≈ -15 dBFS.
        assert!((db(p) + 15.2).abs() < 1.5, "got {} dB", db(p));
    }

    #[test]
    fn gate_silences_quiet_parts() {
        let mut quiet = tone(0.001, 440.0, 22050);
        quiet.extend(tone(0.5, 440.0, 22050));
        let mut x = vec![quiet];
        gate(&mut x, 44100.0, -40.0, 60.0, 1.0, 10.0, 50.0);
        assert!(peak(&x[0][5000..20000]) < 1e-5);
        assert!(peak(&x[0][30000..]) > 0.49);
    }

    #[test]
    fn de_esser_targets_highs() {
        let mut lowtone = vec![tone(0.5, 300.0, 44100)];
        de_ess(&mut lowtone, 44100.0, -30.0, 5000.0, 12.0);
        assert!(peak(&lowtone[0][1000..]) > 0.48);
        let mut hiss = vec![tone(0.5, 7000.0, 44100)];
        de_ess(&mut hiss, 44100.0, -30.0, 5000.0, 12.0);
        assert!(peak(&hiss[0][4000..]) < 0.25);
    }
}
