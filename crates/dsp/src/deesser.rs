use crate::biquad::{Biquad, BiquadCoeffs, BiquadKind};
use crate::dynamics::{EnvelopeFollower, static_reduction_db};
use crate::level::{db_to_gain, gain_to_db};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeEsserSettings {
    pub freq_hz: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub max_reduction_db: f32,
}

impl Default for DeEsserSettings {
    fn default() -> Self {
        Self {
            freq_hz: 6_000.0,
            threshold_db: -30.0,
            ratio: 4.0,
            max_reduction_db: -12.0,
        }
    }
}

/// Split-band de-esser. A 4th-order Linkwitz-Riley crossover at `freq_hz`
/// splits the signal; only the high band is turned down, and the bands sum
/// back flat (allpass) when no reduction is applied.
#[derive(Debug, Clone)]
pub struct DeEsser {
    settings: DeEsserSettings,
    sample_rate: f32,
    crossover: Vec<Crossover>,
    band: Vec<(f32, f32)>,
    detector: EnvelopeFollower,
    reduction_db: f32,
}

impl DeEsser {
    pub fn new(channels: usize, settings: DeEsserSettings, sample_rate: f32) -> Self {
        let mut d = Self {
            settings,
            sample_rate,
            crossover: vec![Crossover::default(); channels],
            band: vec![(0.0, 0.0); channels],
            detector: EnvelopeFollower::new(1.0, 60.0, sample_rate),
            reduction_db: 0.0,
        };
        d.set(settings);
        d
    }

    pub fn set(&mut self, settings: DeEsserSettings) {
        self.settings = settings;
        for crossover in &mut self.crossover {
            crossover.set(settings.freq_hz, self.sample_rate);
        }
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.reduction_db
    }

    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        let s = self.settings;
        let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
        for i in 0..frames {
            let mut band_peak = 0.0f32;
            for ((channel, crossover), band) in
                channels.iter().zip(&mut self.crossover).zip(&mut self.band)
            {
                *band = crossover.split(channel[i]);
                band_peak = band_peak.max(band.1.abs());
            }
            let env = self.detector.process(band_peak);
            let reduction = if env <= 1e-6 {
                0.0
            } else {
                static_reduction_db(gain_to_db(env), s.threshold_db, s.ratio, 3.0)
                    .max(s.max_reduction_db.min(0.0))
            };
            self.reduction_db = reduction;
            let gain = db_to_gain(reduction);
            for (channel, &(low, high)) in channels.iter_mut().zip(&self.band) {
                channel[i] = low + gain * high;
            }
        }
    }

    pub fn reset(&mut self) {
        self.crossover.iter_mut().for_each(Crossover::reset);
        self.detector.reset();
        self.reduction_db = 0.0;
    }
}

#[derive(Debug, Clone, Default)]
struct Crossover {
    low: [Biquad; 2],
    high: [Biquad; 2],
}

impl Crossover {
    fn set(&mut self, freq_hz: f32, sample_rate: f32) {
        let lp = BiquadCoeffs::design(BiquadKind::LowPass, freq_hz, 0.707, 0.0, sample_rate);
        let hp = BiquadCoeffs::design(BiquadKind::HighPass, freq_hz, 0.707, 0.0, sample_rate);
        self.low.iter_mut().for_each(|b| b.set_coeffs(lp));
        self.high.iter_mut().for_each(|b| b.set_coeffs(hp));
    }

    #[inline]
    fn split(&mut self, x: f32) -> (f32, f32) {
        let [low_a, low_b] = &mut self.low;
        let [high_a, high_b] = &mut self.high;
        let low = low_b.process_sample(low_a.process_sample(x));
        let high = high_b.process_sample(high_a.process_sample(x));
        (low, high)
    }

    fn reset(&mut self) {
        self.low
            .iter_mut()
            .chain(self.high.iter_mut())
            .for_each(Biquad::reset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{db, rms, sine};

    const SR: f32 = 48_000.0;

    fn reduction_for(freq: f32) -> f32 {
        let original = sine(freq, db_to_gain(-10.0), SR, SR as usize);
        let mut left = original.clone();
        let mut right = original.clone();
        let mut d = DeEsser::new(2, DeEsserSettings::default(), SR);
        d.process(&mut [left.as_mut_slice(), right.as_mut_slice()]);
        let tail = SR as usize / 2;
        db(rms(&left[tail..]) / rms(&original[tail..]))
    }

    #[test]
    fn sibilant_band_is_reduced() {
        let r = reduction_for(7_000.0);
        assert!(r <= -6.0, "7 kHz reduced by {r} dB");
    }

    #[test]
    fn crossover_sums_flat_without_reduction() {
        let mut c = Crossover::default();
        c.set(6_000.0, SR);
        for freq in [200.0, 6_000.0, 12_000.0] {
            let input = sine(freq, 0.5, SR, SR as usize / 2);
            let output: Vec<f32> = input
                .iter()
                .map(|&x| {
                    let (l, h) = c.split(x);
                    l + h
                })
                .collect();
            let tail = SR as usize / 4;
            let change = db(rms(&output[tail..]) / rms(&input[tail..]));
            assert!(change.abs() < 0.05, "{freq} Hz changed by {change} dB");
        }
    }

    #[test]
    fn low_frequencies_pass_untouched() {
        let r = reduction_for(300.0);
        assert!(r.abs() < 0.5, "300 Hz changed by {r} dB");
    }
}
