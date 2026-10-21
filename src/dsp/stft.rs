//! Short-time Fourier transform helpers built on `realfft`.
//!
//! `stft_map` runs a windowed analysis → user callback → overlap-add resynthesis
//! over one or more input channels at once, which is the basis for noise
//! reduction and stem separation. `analyze` runs analysis only.

use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex, num_complex::Complex};

pub type Spectrum = Vec<Complex<f32>>;

pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect()
}

/// A reusable forward/inverse FFT pair of size `n`.
pub struct Fft {
    pub n: usize,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    scratch_in: Vec<f32>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        Self {
            n,
            fwd: planner.plan_fft_forward(n),
            inv: planner.plan_fft_inverse(n),
            scratch_in: vec![0.0; n],
        }
    }

    pub fn bins(&self) -> usize {
        self.n / 2 + 1
    }

    /// Forward transform of `frame * window` (frame shorter than n is zero padded).
    pub fn forward(&mut self, frame: &[f32], window: &[f32], out: &mut Spectrum) {
        out.resize(self.bins(), Complex::new(0.0, 0.0));
        for (i, s) in self.scratch_in.iter_mut().enumerate() {
            *s = frame.get(i).copied().unwrap_or(0.0) * window[i];
        }
        let _ = self.fwd.process(&mut self.scratch_in, out);
    }

    /// Inverse transform (normalised) into `out` (length n). Consumes `spec`.
    pub fn inverse(&mut self, spec: &mut Spectrum, out: &mut [f32]) {
        // DC and Nyquist bins must be purely real for realfft.
        if let Some(c) = spec.first_mut() {
            c.im = 0.0;
        }
        if let Some(c) = spec.last_mut() {
            c.im = 0.0;
        }
        let _ = self.inv.process(spec, out);
        let k = 1.0 / self.n as f32;
        out.iter_mut().for_each(|x| *x *= k);
    }
}

/// Bin index → frequency in Hz.
pub fn bin_freq(bin: usize, n: usize, rate: f32) -> f32 {
    bin as f32 * rate / n as f32
}

/// Process `inputs` (equal-length channels) frame by frame. For each frame the callback
/// receives the input spectra and must fill `n_out` output spectra (initialised to zero),
/// which are inverse-transformed and overlap-added. Returns `n_out` channels with the
/// same length as the inputs. The callback also gets the frame index.
pub fn stft_map(
    inputs: &[&[f32]],
    n_out: usize,
    n: usize,
    hop: usize,
    mut f: impl FnMut(usize, &[Spectrum], &mut [Spectrum]),
) -> Vec<Vec<f32>> {
    let len = inputs.first().map_or(0, |c| c.len());
    if len == 0 {
        return vec![Vec::new(); n_out];
    }
    let win = hann(n);
    let mut fft = Fft::new(n);
    // Pad so the first/last samples are covered by full frames.
    let padded_len = len + 2 * n;
    let mut acc = vec![vec![0f32; padded_len]; n_out];
    let mut wsum = vec![0f32; padded_len];
    let mut in_spec: Vec<Spectrum> = vec![Vec::new(); inputs.len()];
    let mut out_spec: Vec<Spectrum> = vec![vec![Complex::new(0.0, 0.0); n / 2 + 1]; n_out];
    let mut frame = vec![0f32; n];
    let mut time = vec![0f32; n];

    let mut pos = 0usize; // position in padded coordinates
    let mut idx = 0usize;
    while pos + n <= padded_len {
        for (ci, ch) in inputs.iter().enumerate() {
            for (i, s) in frame.iter_mut().enumerate() {
                let p = pos + i;
                *s = if p >= n && p - n < len {
                    ch[p - n]
                } else {
                    0.0
                };
            }
            fft.forward(&frame, &win, &mut in_spec[ci]);
        }
        for o in out_spec.iter_mut() {
            o.iter_mut().for_each(|c| *c = Complex::new(0.0, 0.0));
        }
        f(idx, &in_spec, &mut out_spec);
        for (oi, o) in out_spec.iter_mut().enumerate() {
            fft.inverse(o, &mut time);
            for i in 0..n {
                acc[oi][pos + i] += time[i] * win[i];
            }
        }
        for i in 0..n {
            wsum[pos + i] += win[i] * win[i];
        }
        pos += hop;
        idx += 1;
    }
    acc.into_iter()
        .map(|a| {
            (0..len)
                .map(|i| {
                    let w = wsum[i + n];
                    if w > 1e-6 { a[i + n] / w } else { 0.0 }
                })
                .collect()
        })
        .collect()
}

/// Analysis only: call `f` with each frame's spectrum.
pub fn analyze(x: &[f32], n: usize, hop: usize, mut f: impl FnMut(&Spectrum)) {
    if x.is_empty() {
        return;
    }
    let win = hann(n);
    let mut fft = Fft::new(n);
    let mut spec = Vec::new();
    let mut pos = 0;
    loop {
        let end = (pos + n).min(x.len());
        fft.forward(&x[pos..end], &win, &mut spec);
        f(&spec);
        if pos + n >= x.len() {
            break;
        }
        pos += hop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip() {
        let x: Vec<f32> = (0..10_000)
            .map(|i| ((i as f32) * 0.013).sin() * 0.7)
            .collect();
        let y = stft_map(&[&x], 1, 1024, 256, |_, i, o| o[0].clone_from(&i[0]));
        assert_eq!(y[0].len(), x.len());
        let err = x
            .iter()
            .zip(&y[0])
            .fold(0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 1e-4, "max error {err}");
    }
}
