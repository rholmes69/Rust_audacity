//! Restoration: click/pop removal, mains-hum removal and clipping repair.

use super::biquad::Biquad;

/// Cubic Hermite interpolation across x[a..b] using the samples just outside the gap.
fn bridge(x: &mut [f32], a: usize, b: usize) {
    // a = first bad sample, b = one past the last bad sample.
    if a < 2 || b + 1 >= x.len() || b <= a {
        return;
    }
    let (p0, p1) = (x[a - 1], x[b]);
    let m0 = x[a - 1] - x[a - 2];
    let m1 = x[b + 1] - x[b];
    let span = (b - a + 1) as f32;
    for i in a..b {
        let t = (i - a + 1) as f32 / span;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        x[i] = h00 * p0 + h10 * m0 * span + h01 * p1 + h11 * m1 * span;
    }
}

/// Detect and repair short impulsive clicks. `sensitivity` 1–10 (higher finds
/// quieter clicks), `max_width_ms` limits how long a click may be. Returns clicks fixed.
pub fn remove_clicks(x: &mut [f32], rate: f32, sensitivity: f32, max_width_ms: f32) -> usize {
    let n = x.len();
    if n < 16 {
        return 0;
    }
    // Second difference highlights discontinuities.
    let d: Vec<f32> = (0..n)
        .map(|i| {
            if i == 0 || i + 1 >= n {
                0.0
            } else {
                (x[i + 1] - 2.0 * x[i] + x[i - 1]).abs()
            }
        })
        .collect();
    // Local RMS of d over ~10 ms blocks.
    let block = ((rate * 0.010) as usize).max(32);
    let local: Vec<f32> = d
        .chunks(block)
        .map(|c| (c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let k = 22.0 - sensitivity.clamp(1.0, 10.0) * 1.6; // 20.4 .. 6
    let max_w = ((max_width_ms * 0.001 * rate) as usize).max(1);
    let mut fixed = 0;
    let mut i = 2;
    while i + 2 < n {
        let bi = i / block;
        // Use the quieter of this and neighbouring blocks as reference so the click doesn't mask itself.
        let reference = [bi.saturating_sub(1), bi, (bi + 1).min(local.len() - 1)]
            .iter()
            .map(|&b| local[b])
            .fold(f32::MAX, f32::min)
            .max(1e-5);
        if d[i] > k * reference {
            let start = i.saturating_sub(1);
            let mut end = i + 1;
            while end < n && end - start <= max_w * 2 && d[end] > k * reference * 0.5 {
                end += 1;
            }
            if end - start <= max_w * 2 {
                bridge(x, start, (end + 1).min(n - 2));
                fixed += 1;
            }
            i = end + 2;
        } else {
            i += 1;
        }
    }
    fixed
}

/// Remove mains hum at `base_hz` (50 or 60) and its first `harmonics` multiples with narrow notches.
pub fn remove_hum(x: &mut [f32], rate: f32, base_hz: f32, harmonics: usize, q: f32) {
    for h in 1..=harmonics.max(1) {
        let f = base_hz * h as f32;
        if f >= rate * 0.45 {
            break;
        }
        Biquad::notch(f, q, rate).run(x);
    }
}

/// Rebuild clipped peaks. Samples at or above `threshold` (fraction of the peak)
/// are treated as clipped and replaced by a smooth curve; output is turned down by
/// `reduce_db` so the restored peaks fit. Returns number of clipped runs repaired.
pub fn repair_clipping(x: &mut [f32], threshold: f32, reduce_db: f32) -> usize {
    let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
    if peak <= 0.0 {
        return 0;
    }
    let lim = peak * threshold.clamp(0.5, 1.0) - 1e-7;
    let g = 10f32.powf(-reduce_db.abs() / 20.0);
    let orig = x.to_vec();
    x.iter_mut().for_each(|v| *v *= g);
    let n = orig.len();
    let mut runs = 0;
    let mut i = 2;
    while i + 2 < n {
        if orig[i].abs() >= lim {
            let sign = orig[i].signum();
            let a = i;
            let mut b = i;
            while b < n && orig[b].abs() >= lim && orig[b].signum() == sign {
                b += 1;
            }
            if b - a >= 2 && a >= 2 && b + 2 < n {
                // Extrapolate the slopes going into and out of the flat top.
                let m0 = (orig[a - 1] - orig[a - 2]) * g;
                let m1 = (orig[b + 1] - orig[b]) * g;
                let (p0, p1) = (orig[a - 1] * g, orig[b] * g);
                let span = (b - a + 1) as f32;
                for j in a..b {
                    let t = (j - a + 1) as f32 / span;
                    let (t2, t3) = (t * t, t * t * t);
                    let v = (2.0 * t3 - 3.0 * t2 + 1.0) * p0
                        + (t3 - 2.0 * t2 + t) * m0 * span
                        + (-2.0 * t3 + 3.0 * t2) * p1
                        + (t3 - t2) * m1 * span;
                    // Never make the peak smaller than it was.
                    x[j] = if v.abs() > x[j].abs() && v.signum() == sign {
                        v
                    } else {
                        x[j]
                    };
                }
                runs += 1;
            }
            i = b + 1;
        } else {
            i += 1;
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 44100.0).sin() * amp)
            .collect()
    }

    #[test]
    fn clicks_get_removed() {
        let clean = sine(300.0, 44100, 0.3);
        let mut x = clean.clone();
        for &p in &[5000usize, 15000, 30000] {
            x[p] += 0.8;
            x[p + 1] -= 0.6;
        }
        let fixed = remove_clicks(&mut x, 44100.0, 6.0, 2.0);
        assert!(fixed >= 3, "fixed {fixed}");
        let err = x
            .iter()
            .zip(&clean)
            .fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 0.1, "residual error {err}");
    }

    #[test]
    fn hum_removed() {
        let mut x: Vec<f32> = sine(60.0, 44100, 0.3)
            .iter()
            .zip(sine(1000.0, 44100, 0.3))
            .map(|(a, b)| a + b)
            .collect();
        remove_hum(&mut x, 44100.0, 60.0, 4, 30.0);
        // What's left should be ~the 1 kHz tone.
        let tail = &x[22050..];
        let p = tail.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!((p - 0.3).abs() < 0.05, "peak {p}");
    }

    #[test]
    fn clipping_rebuilt() {
        let mut x: Vec<f32> = sine(200.0, 44100, 1.4)
            .iter()
            .map(|v| v.clamp(-1.0, 1.0))
            .collect();
        let runs = repair_clipping(&mut x, 0.99, 6.0);
        assert!(runs > 10);
        let p = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        // Restored peaks rise above the old flat top (after the 6 dB reduction ~0.5).
        assert!(p > 0.55 && p < 1.0, "peak {p}");
    }
}
