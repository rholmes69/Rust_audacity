//! Tone shaping: graphic EQ, bass & treble, distortion.

use serde::{Deserialize, Serialize};

use super::biquad::Biquad;

pub const EQ_BANDS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];
const EQ_Q: f32 = 1.41; // ≈ one-octave bandwidth

pub fn eq_filters(gains: &[f32; 10], rate: f32) -> Vec<Biquad> {
    EQ_BANDS
        .iter()
        .zip(gains)
        .filter(|(f, g)| g.abs() > 0.01 && **f < rate * 0.45)
        .map(|(&f, &g)| Biquad::peaking(f, EQ_Q, g, rate))
        .collect()
}

pub fn graphic_eq(x: &mut [f32], gains: &[f32; 10], rate: f32) {
    for f in eq_filters(gains, rate) {
        f.run(x);
    }
}

/// Combined response (dB) of the EQ at `freq` — used to draw the curve.
pub fn eq_response(gains: &[f32; 10], rate: f32, freq: f32) -> f32 {
    eq_filters(gains, rate)
        .iter()
        .map(|f| f.response_db(freq, rate))
        .sum()
}

pub const EQ_PRESETS: &[(&str, [f32; 10])] = &[
    ("Flat", [0.0; 10]),
    (
        "Bass Boost",
        [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ),
    (
        "Bass Cut",
        [-8.0, -6.0, -4.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ),
    (
        "Treble Boost",
        [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.5, 3.0, 4.5, 6.0],
    ),
    (
        "Voice Clarity",
        [-6.0, -4.0, -2.0, -1.0, 0.0, 1.0, 3.0, 3.0, 1.5, 0.0],
    ),
    (
        "Telephone",
        [-24.0, -24.0, -18.0, -6.0, 0.0, 3.0, 3.0, -6.0, -24.0, -24.0],
    ),
    (
        "Loudness (smile)",
        [5.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0],
    ),
    (
        "Warmth",
        [2.0, 3.0, 3.0, 2.0, 1.0, 0.0, -1.0, -2.0, -2.0, -3.0],
    ),
];

/// Simple bass/treble shelves (Audacity's Bass and Treble uses ~250 Hz and ~4 kHz turnovers).
pub fn bass_treble(x: &mut [f32], bass_db: f32, treble_db: f32, rate: f32) {
    if bass_db.abs() > 0.01 {
        Biquad::low_shelf(250.0, bass_db, rate).run(x);
    }
    if treble_db.abs() > 0.01 {
        Biquad::high_shelf(4000.0, treble_db, rate).run(x);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistortionKind {
    /// Smooth tanh saturation.
    Soft,
    /// Hard clipping.
    Hard,
    /// Asymmetric "tube" curve (adds even harmonics).
    Tube,
    /// Fuzz: heavy hard-clip with rectification.
    Fuzz,
}

impl DistortionKind {
    pub const ALL: [DistortionKind; 4] = [
        DistortionKind::Soft,
        DistortionKind::Hard,
        DistortionKind::Tube,
        DistortionKind::Fuzz,
    ];
}

pub fn distort(x: &mut [f32], kind: DistortionKind, drive_db: f32, mix: f32, output_db: f32) {
    let drive = 10f32.powf(drive_db / 20.0);
    let out = 10f32.powf(output_db / 20.0);
    let mix = mix.clamp(0.0, 1.0);
    for s in x.iter_mut() {
        let d = *s * drive;
        let w = match kind {
            DistortionKind::Soft => d.tanh(),
            DistortionKind::Hard => d.clamp(-1.0, 1.0),
            DistortionKind::Tube => {
                if d >= 0.0 {
                    1.0 - (-d).exp()
                } else {
                    -(1.0 - d.exp()) * 0.8
                }
            }
            DistortionKind::Fuzz => {
                let r = d.clamp(-1.0, 1.0);
                (r + 0.3 * r.abs()).clamp(-1.0, 1.0)
            }
        };
        *s = (*s * (1.0 - mix) + w * mix) * out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_curve_matches_bands() {
        let mut g = [0.0; 10];
        g[5] = 6.0;
        assert!((eq_response(&g, 44100.0, 1000.0) - 6.0).abs() < 0.2);
        assert!(eq_response(&g, 44100.0, 60.0).abs() < 0.3);
    }

    #[test]
    fn soft_distortion_bounded() {
        let mut x: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.05).sin()).collect();
        distort(&mut x, DistortionKind::Soft, 24.0, 1.0, 0.0);
        assert!(x.iter().all(|v| v.abs() <= 1.0));
    }
}
