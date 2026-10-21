//! Space & stereo: Freeverb-style reverb, mid/side stereo width, centre-cancel vocal remover.

use super::biquad::{BUTTERWORTH_Q, Biquad};

// Freeverb tunings (samples at 44.1 kHz), scaled to the actual rate.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
}

impl Comb {
    fn tick(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.idx];
        self.store = out * (1.0 - damp) + self.store * damp;
        self.buf[self.idx] = input + self.store * feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn tick(&mut self, input: f32) -> f32 {
        let b = self.buf[self.idx];
        let out = b - input;
        self.buf[self.idx] = input + b * 0.5;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

struct Tank {
    combs: Vec<Comb>,
    aps: Vec<Allpass>,
}

impl Tank {
    fn new(rate: f32, spread: usize) -> Self {
        let scale = rate / 44100.0;
        let sz = |n: usize| (((n + spread) as f32 * scale) as usize).max(1);
        Self {
            combs: COMBS
                .iter()
                .map(|&n| Comb {
                    buf: vec![0.0; sz(n)],
                    idx: 0,
                    store: 0.0,
                })
                .collect(),
            aps: ALLPASSES
                .iter()
                .map(|&n| Allpass {
                    buf: vec![0.0; sz(n)],
                    idx: 0,
                })
                .collect(),
        }
    }
    fn tick(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let mut out = self
            .combs
            .iter_mut()
            .map(|c| c.tick(input, feedback, damp))
            .sum::<f32>();
        for a in &mut self.aps {
            out = a.tick(out);
        }
        out
    }
}

/// Reverb. `room` 0–1, `damping` 0–1, `wet`/`dry` in dB, `width` 0–1, `pre_delay_ms`.
/// Output is `tail` samples longer than the input so the reverb can ring out.
#[allow(clippy::too_many_arguments)]
pub fn reverb(
    chans: &[Vec<f32>],
    rate: f32,
    room: f32,
    damping: f32,
    wet_db: f32,
    dry_db: f32,
    width: f32,
    pre_delay_ms: f32,
    tail: usize,
) -> Vec<Vec<f32>> {
    let Some(len) = chans.first().map(Vec::len) else {
        return Vec::new();
    };
    let feedback = 0.7 + 0.28 * room.clamp(0.0, 1.0);
    let damp = damping.clamp(0.0, 1.0) * 0.4;
    let wet = 10f32.powf(wet_db / 20.0) * 0.015; // Freeverb's fixed input gain
    let dry = if dry_db <= -60.0 {
        0.0
    } else {
        10f32.powf(dry_db / 20.0)
    };
    let width = width.clamp(0.0, 1.0);
    let (w1, w2) = ((width / 2.0 + 0.5), ((1.0 - width) / 2.0));
    let pre = (pre_delay_ms.max(0.0) * 0.001 * rate) as usize;
    let mut left = Tank::new(rate, 0);
    let mut right = Tank::new(rate, STEREO_SPREAD);
    let stereo = chans.len() > 1;
    let total = len + tail;
    let mut out = vec![vec![0f32; total]; chans.len()];
    for i in 0..total {
        let src = |c: usize, j: usize| chans[c].get(j).copied().unwrap_or(0.0);
        let input_idx = i.checked_sub(pre);
        let mono_in = input_idx.map_or(0.0, |j| {
            (src(0, j) + if stereo { src(1, j) } else { src(0, j) }) * 0.5
        });
        let l = left.tick(mono_in, feedback, damp);
        let r = right.tick(mono_in, feedback, damp);
        let (wl, wr) = ((l * w1 + r * w2) * wet, (r * w1 + l * w2) * wet);
        if stereo {
            out[0][i] = src(0, i) * dry + wl;
            out[1][i] = src(1, i) * dry + wr;
            for c in 2..chans.len() {
                out[c][i] = src(c, i) * dry;
            }
        } else {
            out[0][i] = src(0, i) * dry + (wl + wr) * 0.5;
        }
    }
    out
}

/// Mid/side stereo width. `width` 0 = mono, 1 = unchanged, 2 = extra wide.
pub fn stereo_width(chans: &mut [Vec<f32>], width: f32, mid_db: f32, side_db: f32) -> bool {
    if chans.len() < 2 {
        return false;
    }
    let mg = 10f32.powf(mid_db / 20.0);
    let sg = 10f32.powf(side_db / 20.0) * width.max(0.0);
    let (l, rest) = chans.split_at_mut(1);
    for (a, b) in l[0].iter_mut().zip(rest[0].iter_mut()) {
        let m = (*a + *b) * 0.5 * mg;
        let s = (*a - *b) * 0.5 * sg;
        *a = m + s;
        *b = m - s;
    }
    true
}

/// Classic karaoke vocal remover: subtract the channels so centre-panned material
/// (usually the lead vocal) cancels. Bass below `keep_bass_hz` is preserved from the mid signal.
pub fn vocal_remover(chans: &mut [Vec<f32>], rate: f32, keep_bass_hz: f32) -> bool {
    if chans.len() < 2 {
        return false;
    }
    let mut mid: Vec<f32> = chans[0]
        .iter()
        .zip(&chans[1])
        .map(|(a, b)| (a + b) * 0.5)
        .collect();
    if keep_bass_hz > 0.0 {
        let lp = Biquad::lowpass(keep_bass_hz, BUTTERWORTH_Q, rate);
        lp.run(&mut mid);
        lp.run(&mut mid); // 24 dB/oct
    } else {
        mid.iter_mut().for_each(|v| *v = 0.0);
    }
    let (l, rest) = chans.split_at_mut(1);
    for ((a, b), m) in l[0].iter_mut().zip(rest[0].iter_mut()).zip(&mid) {
        let side = (*a - *b) * 0.5;
        *a = m + side;
        *b = m - side;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverb_adds_tail() {
        let mut x = vec![0f32; 4410];
        x[0] = 1.0;
        let out = reverb(&[x], 44100.0, 0.8, 0.5, 0.0, 0.0, 1.0, 0.0, 44100);
        assert_eq!(out[0].len(), 4410 + 44100);
        let energy: f32 = out[0][10000..].iter().map(|v| v * v).sum();
        assert!(energy > 1e-6);
    }

    #[test]
    fn vocal_remover_cancels_centre() {
        let v: Vec<f32> = (0..44100).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut chans = vec![v.clone(), v];
        vocal_remover(&mut chans, 44100.0, 0.0);
        assert!(chans[0].iter().all(|x| x.abs() < 1e-6));
    }

    #[test]
    fn width_zero_is_mono() {
        let mut chans = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        stereo_width(&mut chans, 0.0, 0.0, 0.0);
        assert_eq!(chans[0], chans[1]);
    }
}
