//! Spectrogram rendering (log-frequency heat map) to plain RGB pixels.

use super::stft::{Fft, hann};

pub const MIN_FREQ: f32 = 40.0;
const FFT_N: usize = 2048;
const FLOOR_DB: f32 = -96.0;

/// "Inferno"-like colour ramp.
const RAMP: [(f32, [u8; 3]); 7] = [
    (0.00, [0, 0, 4]),
    (0.18, [40, 11, 84]),
    (0.38, [101, 21, 110]),
    (0.55, [159, 42, 99]),
    (0.72, [212, 72, 66]),
    (0.86, [245, 125, 21]),
    (1.00, [252, 255, 164]),
];

pub fn colour(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in RAMP.windows(2) {
        let ((t0, c0), (t1, c1)) = (w[0], w[1]);
        if t <= t1 {
            let f = (t - t0) / (t1 - t0);
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f) as u8;
            return [mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2])];
        }
    }
    [252, 255, 164]
}

/// Frequency shown at a vertical fraction (0 = top) of the lane.
pub fn freq_at(frac: f32, rate: u32) -> f32 {
    let fmax = rate as f32 / 2.0;
    (fmax.ln() + (MIN_FREQ.ln() - fmax.ln()) * frac).exp()
}

/// Vertical fraction (0 = top) for a frequency.
pub fn frac_of(freq: f32, rate: u32) -> f32 {
    let fmax = rate as f32 / 2.0;
    (fmax.ln() - freq.max(1.0).ln()) / (fmax.ln() - MIN_FREQ.ln())
}

/// Render a log-frequency spectrogram as RGB pixels (row-major, top row = highest frequency).
/// `left` = first sample at the left edge, `spp` = samples per pixel column.
pub fn render(
    data: &[f32],
    rate: u32,
    left: f64,
    spp: f64,
    width: usize,
    height: usize,
) -> Vec<[u8; 3]> {
    let mut img = vec![[0u8, 0, 4]; width.max(1) * height.max(1)];
    if data.is_empty() || width == 0 || height == 0 {
        return img;
    }
    let mut fft = Fft::new(FFT_N);
    let win = hann(FFT_N);
    let mut spec = Vec::new();
    let mut frame = vec![0f32; FFT_N];
    let bins = FFT_N / 2 + 1;
    // Precompute the (fractional) bin for every row.
    let row_bin: Vec<f32> = (0..height)
        .map(|y| freq_at((y as f32 + 0.5) / height as f32, rate) * FFT_N as f32 / rate as f32)
        .collect();
    let norm = (FFT_N as f32 / 4.0).powi(2);
    let mut db = vec![0f32; bins];
    for x in 0..width {
        let centre = left + (x as f64 + 0.5) * spp;
        if centre < 0.0 || centre >= data.len() as f64 {
            continue;
        }
        let start = centre as isize - (FFT_N / 2) as isize;
        for (i, f) in frame.iter_mut().enumerate() {
            let p = start + i as isize;
            *f = if p >= 0 && (p as usize) < data.len() {
                data[p as usize]
            } else {
                0.0
            };
        }
        fft.forward(&frame, &win, &mut spec);
        for (d, c) in db.iter_mut().zip(&spec) {
            *d = 10.0 * (c.norm_sqr() / norm).max(1e-12).log10();
        }
        for (y, &b) in row_bin.iter().enumerate() {
            let i0 = (b as usize).min(bins - 1);
            let i1 = (i0 + 1).min(bins - 1);
            let fr = b - i0 as f32;
            let v = db[i0] + (db[i1] - db[i0]) * fr;
            img[y * width + x] = colour((v - FLOOR_DB) / -FLOOR_DB);
        }
    }
    img
}
