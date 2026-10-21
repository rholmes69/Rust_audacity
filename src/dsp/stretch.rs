//! Time-stretching (tempo without pitch) using WSOLA, and pitch shifting
//! (pitch without tempo) as stretch + resample.

use crate::track::resample_ratio;

/// Change duration by `1/tempo` without changing pitch (tempo 1.25 = 25% faster).
/// All channels share the same splice points so stereo imaging is preserved.
pub fn time_stretch(chans: &[Vec<f32>], tempo: f64, rate: u32) -> Vec<Vec<f32>> {
    if chans.is_empty() || chans[0].is_empty() || (tempo - 1.0).abs() < 1e-6 || tempo <= 0.0 {
        return chans.to_vec();
    }
    let len = chans[0].len();
    let win = ((rate as f64 * 0.050) as usize).max(64) & !1; // 50 ms frames
    let hs = win / 2; // synthesis hop
    let ha = hs as f64 * tempo; // analysis hop
    let tol = (rate as f64 * 0.012) as isize; // ±12 ms search
    let out_len = (len as f64 / tempo).round() as usize;
    let window: Vec<f32> = (0..win)
        .map(|i| (std::f32::consts::PI * i as f32 / win as f32).sin().powi(2))
        .collect();

    // Mono guide signal (decimated by 4 for a cheap similarity search).
    const DEC: usize = 4;
    let guide: Vec<f32> = (0..len)
        .step_by(DEC)
        .map(|i| chans.iter().map(|c| c[i]).sum::<f32>())
        .collect();

    let mut out = vec![vec![0f32; out_len + win]; chans.len()];
    let mut norm = vec![0f32; out_len + win];
    let mut prev_in: isize = 0; // where the last frame was taken from
    let mut k = 0usize;
    loop {
        let out_pos = k * hs;
        if out_pos >= out_len {
            break;
        }
        let nominal = (k as f64 * ha) as isize;
        let pos = if k == 0 {
            0
        } else {
            // The ideal continuation of the previous frame is prev_in + hs.
            let target = prev_in + hs as isize;
            let lo = (nominal - tol).max(0);
            let hi = (nominal + tol).min(len as isize - 1);
            let mut best = nominal.clamp(0, len as isize - 1);
            let mut best_score = f32::MIN;
            let cmp_len = hs / DEC;
            let mut cand = lo;
            while cand <= hi {
                let (gc, gt) = (cand as usize / DEC, target.max(0) as usize / DEC);
                let mut score = 0f32;
                for j in 0..cmp_len {
                    let a = guide.get(gc + j).copied().unwrap_or(0.0);
                    let b = guide.get(gt + j).copied().unwrap_or(0.0);
                    score += a * b;
                }
                if score > best_score {
                    best_score = score;
                    best = cand;
                }
                cand += DEC as isize;
            }
            best
        };
        prev_in = pos;
        for (o, c) in out.iter_mut().zip(chans) {
            for i in 0..win {
                let src = pos as usize + i;
                let s = if src < len { c[src] } else { 0.0 };
                o[out_pos + i] += s * window[i];
            }
        }
        for i in 0..win {
            norm[out_pos + i] += window[i];
        }
        k += 1;
    }
    out.into_iter()
        .map(|mut o| {
            for (s, n) in o.iter_mut().zip(&norm) {
                if *n > 1e-3 {
                    *s /= n;
                }
            }
            o.truncate(out_len);
            o
        })
        .collect()
}

/// Shift pitch by `semitones` while keeping the duration.
pub fn pitch_shift(chans: &[Vec<f32>], semitones: f64, rate: u32) -> Vec<Vec<f32>> {
    if semitones.abs() < 1e-6 || chans.is_empty() {
        return chans.to_vec();
    }
    let len = chans[0].len();
    let p = 2f64.powf(semitones / 12.0);
    // Stretch so it is p times longer, then play it back p times faster.
    let stretched = time_stretch(chans, 1.0 / p, rate);
    stretched
        .iter()
        .map(|c| resample_ratio(c, p, len))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Estimate frequency from zero crossings.
    fn freq_of(x: &[f32], rate: u32) -> f32 {
        let zc = x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        zc as f32 * rate as f32 / x.len() as f32
    }

    #[test]
    fn tempo_changes_length_not_pitch() {
        let rate = 44100;
        let x = sine(440.0, rate, 2.0);
        let y = time_stretch(std::slice::from_ref(&x), 1.5, rate);
        assert_eq!(y[0].len(), (x.len() as f64 / 1.5).round() as usize);
        let f = freq_of(&y[0][2000..y[0].len() - 2000], rate);
        assert!((f - 440.0).abs() < 8.0, "freq {f}");
    }

    #[test]
    fn pitch_changes_pitch_not_length() {
        let rate = 44100;
        let x = sine(440.0, rate, 2.0);
        let y = pitch_shift(std::slice::from_ref(&x), 12.0, rate);
        assert_eq!(y[0].len(), x.len());
        let f = freq_of(&y[0][4000..y[0].len() - 4000], rate);
        assert!((f - 880.0).abs() < 15.0, "freq {f}");
    }
}
