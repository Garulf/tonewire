use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BiquadKind {
    LowPass,
    HighPass,
    BandPass,
    Notch,
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiquadCoeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl BiquadCoeffs {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    pub fn design(kind: BiquadKind, freq_hz: f32, q: f32, gain_db: f32, sample_rate: f32) -> Self {
        let sr = f64::from(sample_rate);
        let freq = f64::from(freq_hz).clamp(1.0, sr * 0.49);
        let q = f64::from(q).max(0.05);
        let a = 10f64.powf(f64::from(gain_db) / 40.0);
        let w0 = 2.0 * PI * freq / sr;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            BiquadKind::LowPass => {
                let b1 = 1.0 - cos;
                (b1 / 2.0, b1, b1 / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            BiquadKind::HighPass => {
                let b1 = 1.0 + cos;
                (
                    b1 / 2.0,
                    -b1,
                    b1 / 2.0,
                    1.0 + alpha,
                    -2.0 * cos,
                    1.0 - alpha,
                )
            }
            BiquadKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BiquadKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BiquadKind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            BiquadKind::LowShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - k),
                    (a + 1.0) + (a - 1.0) * cos + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - k,
                )
            }
            BiquadKind::HighShelf => {
                let k = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - k),
                    (a + 1.0) - (a - 1.0) * cos + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - k,
                )
            }
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    pub fn magnitude_db(&self, freq_hz: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * PI * f64::from(freq_hz) / f64::from(sample_rate);
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let num = num_re.hypot(num_im);
        let den = den_re.hypot(den_im);
        (20.0 * (num / den).log10()) as f32
    }
}

#[derive(Debug, Clone)]
pub struct Biquad {
    coeffs: BiquadCoeffs,
    z1: f64,
    z2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new(BiquadCoeffs::IDENTITY)
    }
}

impl Biquad {
    pub fn new(coeffs: BiquadCoeffs) -> Self {
        Self {
            coeffs,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn coeffs(&self) -> BiquadCoeffs {
        self.coeffs
    }

    pub fn set_coeffs(&mut self, coeffs: BiquadCoeffs) {
        self.coeffs = coeffs;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let c = &self.coeffs;
        let x = f64::from(x);
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y as f32
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{db, rms, sine};

    const SR: f32 = 48_000.0;

    fn measured_gain_db(coeffs: BiquadCoeffs, freq: f32) -> f32 {
        let input = sine(freq, 0.5, SR, SR as usize);
        let mut output = input.clone();
        Biquad::new(coeffs).process(&mut output);
        let settled = SR as usize / 2;
        db(rms(&output[settled..]) / rms(&input[settled..]))
    }

    #[test]
    fn low_shelf_boosts_bass_and_leaves_highs() {
        let c = BiquadCoeffs::design(BiquadKind::LowShelf, 100.0, 0.707, 6.0, SR);
        assert!((measured_gain_db(c, 30.0) - 6.0).abs() < 0.5);
        assert!(measured_gain_db(c, 5_000.0).abs() < 0.3);
    }

    #[test]
    fn analytic_magnitude_matches_measurement() {
        let cases = [
            (BiquadKind::Peak, 1_000.0, 2.0, -9.0),
            (BiquadKind::LowPass, 2_000.0, 0.707, 0.0),
            (BiquadKind::HighPass, 500.0, 0.707, 0.0),
            (BiquadKind::HighShelf, 4_000.0, 0.707, 4.0),
        ];
        for (kind, freq, q, gain) in cases {
            let c = BiquadCoeffs::design(kind, freq, q, gain, SR);
            for probe in [100.0, 1_000.0, 3_000.0, 10_000.0] {
                let analytic = c.magnitude_db(probe, SR);
                let measured = measured_gain_db(c, probe);
                assert!(
                    (analytic - measured).abs() < 0.2,
                    "{kind:?} at {probe} Hz: analytic {analytic} measured {measured}"
                );
            }
        }
    }

    #[test]
    fn peak_hits_requested_gain_at_center() {
        let c = BiquadCoeffs::design(BiquadKind::Peak, 1_000.0, 1.0, -9.0, SR);
        assert!((c.magnitude_db(1_000.0, SR) + 9.0).abs() < 0.01);
    }

    #[test]
    fn out_of_range_frequency_is_clamped_not_unstable() {
        let c = BiquadCoeffs::design(BiquadKind::LowPass, 90_000.0, 0.707, 0.0, SR);
        let mut out = sine(1_000.0, 0.5, SR, 4_800);
        Biquad::new(c).process(&mut out);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 2.0));
    }
}
