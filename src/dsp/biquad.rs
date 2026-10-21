//! RBJ "Audio EQ Cookbook" biquad filters.

use std::f32::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn from_raw(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn w0(freq: f32, rate: f32) -> (f32, f32) {
        let f = freq.clamp(1.0, rate * 0.499);
        let w = 2.0 * PI * f / rate;
        (w.cos(), w.sin())
    }

    pub fn lowpass(freq: f32, q: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let alpha = s / (2.0 * q);
        Self::from_raw(
            (1.0 - c) / 2.0,
            1.0 - c,
            (1.0 - c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    pub fn highpass(freq: f32, q: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let alpha = s / (2.0 * q);
        Self::from_raw(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    /// Constant 0 dB peak gain band-pass.
    pub fn bandpass(freq: f32, q: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let alpha = s / (2.0 * q);
        Self::from_raw(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    pub fn notch(freq: f32, q: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let alpha = s / (2.0 * q);
        Self::from_raw(1.0, -2.0 * c, 1.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    pub fn peaking(freq: f32, q: f32, gain_db: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = s / (2.0 * q);
        Self::from_raw(
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    /// Shelf with slope S = 1.
    pub fn low_shelf(freq: f32, gain_db: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = s / 2.0 * std::f32::consts::SQRT_2;
        let sa = 2.0 * a.sqrt() * alpha;
        Self::from_raw(
            a * ((a + 1.0) - (a - 1.0) * c + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sa),
            (a + 1.0) + (a - 1.0) * c + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sa,
        )
    }

    pub fn high_shelf(freq: f32, gain_db: f32, rate: f32) -> Self {
        let (c, s) = Self::w0(freq, rate);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = s / 2.0 * std::f32::consts::SQRT_2;
        let sa = 2.0 * a.sqrt() * alpha;
        Self::from_raw(
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    #[inline]
    pub fn tick(&mut self, x0: f32) -> f32 {
        let y0 = self.b0 * x0 + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }

    pub fn run(mut self, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            *s = self.tick(*s);
        }
    }

    /// Magnitude response in dB at `freq`.
    pub fn response_db(&self, freq: f32, rate: f32) -> f32 {
        let w = 2.0 * PI * freq / rate;
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
    }
}

pub const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaking_gain_at_centre() {
        let f = Biquad::peaking(1000.0, 1.0, 6.0, 44100.0);
        assert!((f.response_db(1000.0, 44100.0) - 6.0).abs() < 0.05);
        assert!(f.response_db(50.0, 44100.0).abs() < 0.5);
    }

    #[test]
    fn shelves() {
        let lo = Biquad::low_shelf(200.0, 9.0, 44100.0);
        assert!((lo.response_db(20.0, 44100.0) - 9.0).abs() < 0.3);
        assert!(lo.response_db(10000.0, 44100.0).abs() < 0.3);
        let hi = Biquad::high_shelf(4000.0, -6.0, 44100.0);
        assert!((hi.response_db(18000.0, 44100.0) + 6.0).abs() < 0.5);
    }

    #[test]
    fn notch_kills_centre() {
        let n = Biquad::notch(60.0, 10.0, 44100.0);
        assert!(n.response_db(60.0, 44100.0) < -40.0);
        assert!(n.response_db(200.0, 44100.0).abs() < 0.5);
    }
}
