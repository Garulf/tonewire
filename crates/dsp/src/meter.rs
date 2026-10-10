#[derive(Debug, Clone, Default)]
pub struct PeakRms {
    peak: f32,
    sum_squares: f64,
    count: u64,
}

impl PeakRms {
    pub fn process(&mut self, samples: &[f32]) {
        let mut sum = 0.0f32;
        for &s in samples {
            self.peak = self.peak.max(s.abs());
            sum += s * s;
        }
        self.sum_squares += f64::from(sum);
        self.count += samples.len() as u64;
    }

    pub fn take(&mut self) -> (f32, f32) {
        let rms = if self.count == 0 {
            0.0
        } else {
            (self.sum_squares / self.count as f64).sqrt() as f32
        };
        let result = (self.peak, rms);
        *self = Self::default();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::sine;

    #[test]
    fn reports_peak_and_rms_then_resets() {
        let mut m = PeakRms::default();
        m.process(&sine(1_000.0, 0.5, 48_000.0, 48_000));
        let (peak, rms) = m.take();
        assert!((peak - 0.5).abs() < 1e-3);
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 1e-3);
        assert_eq!(m.take(), (0.0, 0.0));
    }
}
