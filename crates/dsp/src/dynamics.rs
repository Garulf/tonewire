use crate::level::{SILENCE_DB, db_to_gain, gain_to_db, time_coeff};

#[derive(Debug, Clone)]
pub struct EnvelopeFollower {
    attack: f32,
    release: f32,
    value: f32,
}

impl EnvelopeFollower {
    pub fn new(attack_ms: f32, release_ms: f32, sample_rate: f32) -> Self {
        Self {
            attack: time_coeff(attack_ms, sample_rate),
            release: time_coeff(release_ms, sample_rate),
            value: 0.0,
        }
    }

    pub fn set_times(&mut self, attack_ms: f32, release_ms: f32, sample_rate: f32) {
        self.attack = time_coeff(attack_ms, sample_rate);
        self.release = time_coeff(release_ms, sample_rate);
    }

    #[inline]
    pub fn process(&mut self, rectified: f32) -> f32 {
        let coeff = if rectified > self.value {
            self.attack
        } else {
            self.release
        };
        self.value = rectified + (self.value - rectified) * coeff;
        self.value
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
    }
}

#[inline]
fn frame_peak(channels: &[&mut [f32]], i: usize) -> f32 {
    channels.iter().fold(0.0f32, |m, ch| m.max(ch[i].abs()))
}

#[inline]
fn apply_frame_gain(channels: &mut [&mut [f32]], i: usize, gain: f32) {
    for ch in channels.iter_mut() {
        ch[i] *= gain;
    }
}

fn frames(channels: &[&mut [f32]]) -> usize {
    channels.iter().map(|c| c.len()).min().unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorSettings {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

impl Default for CompressorSettings {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 3.0,
            knee_db: 6.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            makeup_db: 0.0,
        }
    }
}

pub(crate) fn static_reduction_db(
    level_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
) -> f32 {
    let slope = 1.0 / ratio.max(1.0) - 1.0;
    let over = level_db - threshold_db;
    if knee_db > 0.0 && 2.0 * over.abs() <= knee_db {
        slope * (over + knee_db / 2.0).powi(2) / (2.0 * knee_db)
    } else if over > 0.0 {
        slope * over
    } else {
        0.0
    }
}

#[derive(Debug, Clone)]
pub struct Compressor {
    settings: CompressorSettings,
    sample_rate: f32,
    detector: EnvelopeFollower,
    makeup: f32,
    reduction_db: f32,
}

impl Compressor {
    pub fn new(settings: CompressorSettings, sample_rate: f32) -> Self {
        let mut c = Self {
            settings,
            sample_rate,
            detector: EnvelopeFollower::new(settings.attack_ms, settings.release_ms, sample_rate),
            makeup: 1.0,
            reduction_db: 0.0,
        };
        c.set(settings);
        c
    }

    pub fn set(&mut self, settings: CompressorSettings) {
        self.settings = settings;
        self.detector
            .set_times(settings.attack_ms, settings.release_ms, self.sample_rate);
        self.makeup = db_to_gain(settings.makeup_db);
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.reduction_db
    }

    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        let s = self.settings;
        for i in 0..frames(channels) {
            let env = self.detector.process(frame_peak(channels, i));
            let reduction = if env <= 1e-6 {
                0.0
            } else {
                static_reduction_db(gain_to_db(env), s.threshold_db, s.ratio, s.knee_db)
            };
            self.reduction_db = reduction;
            apply_frame_gain(channels, i, db_to_gain(reduction) * self.makeup);
        }
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.reduction_db = 0.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateSettings {
    pub threshold_db: f32,
    pub range_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for GateSettings {
    fn default() -> Self {
        Self {
            threshold_db: -50.0,
            range_db: -80.0,
            attack_ms: 1.0,
            hold_ms: 50.0,
            release_ms: 150.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Gate {
    settings: GateSettings,
    sample_rate: f32,
    detector: EnvelopeFollower,
    threshold: f32,
    floor: f32,
    hold_samples: u32,
    hold_left: u32,
    open_coeff: f32,
    close_coeff: f32,
    gain: f32,
}

impl Gate {
    pub fn new(settings: GateSettings, sample_rate: f32) -> Self {
        let mut g = Self {
            settings,
            sample_rate,
            detector: EnvelopeFollower::new(0.5, 20.0, sample_rate),
            threshold: 0.0,
            floor: 0.0,
            hold_samples: 0,
            hold_left: 0,
            open_coeff: 0.0,
            close_coeff: 0.0,
            gain: 0.0,
        };
        g.set(settings);
        g.gain = g.floor;
        g
    }

    pub fn set(&mut self, settings: GateSettings) {
        self.settings = settings;
        self.threshold = db_to_gain(settings.threshold_db);
        self.floor = db_to_gain(settings.range_db.min(0.0));
        self.hold_samples = (settings.hold_ms.max(0.0) * 0.001 * self.sample_rate) as u32;
        self.open_coeff = time_coeff(settings.attack_ms, self.sample_rate);
        self.close_coeff = time_coeff(settings.release_ms, self.sample_rate);
    }

    pub fn gain_reduction_db(&self) -> f32 {
        gain_to_db(self.gain).max(SILENCE_DB)
    }

    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        for i in 0..frames(channels) {
            let env = self.detector.process(frame_peak(channels, i));
            let target = if env >= self.threshold {
                self.hold_left = self.hold_samples;
                1.0
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
                1.0
            } else {
                self.floor
            };
            let coeff = if target > self.gain {
                self.open_coeff
            } else {
                self.close_coeff
            };
            self.gain = target + (self.gain - target) * coeff;
            apply_frame_gain(channels, i, self.gain);
        }
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.hold_left = 0;
        self.gain = self.floor;
    }
}

#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: f32,
    release: f32,
    gain: f32,
}

impl Limiter {
    pub fn new(ceiling_db: f32, release_ms: f32, sample_rate: f32) -> Self {
        Self {
            ceiling: db_to_gain(ceiling_db),
            release: time_coeff(release_ms, sample_rate),
            gain: 1.0,
        }
    }

    pub fn set(&mut self, ceiling_db: f32, release_ms: f32, sample_rate: f32) {
        self.ceiling = db_to_gain(ceiling_db);
        self.release = time_coeff(release_ms, sample_rate);
    }

    pub fn gain_reduction_db(&self) -> f32 {
        gain_to_db(self.gain)
    }

    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        for i in 0..frames(channels) {
            let peak = frame_peak(channels, i);
            self.gain = 1.0 + (self.gain - 1.0) * self.release;
            if peak * self.gain > self.ceiling {
                self.gain = self.ceiling / peak;
            }
            apply_frame_gain(channels, i, self.gain);
        }
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{db, peak, sine};

    const SR: f32 = 48_000.0;

    #[test]
    fn compressor_follows_static_curve() {
        let settings = CompressorSettings {
            threshold_db: -20.0,
            ratio: 4.0,
            knee_db: 0.0,
            attack_ms: 1.0,
            release_ms: 200.0,
            makeup_db: 0.0,
        };
        let mut c = Compressor::new(settings, SR);
        let mut signal = sine(1_000.0, db_to_gain(-8.0), SR, SR as usize);
        c.process(&mut [signal.as_mut_slice()]);
        let out_db = db(peak(&signal[SR as usize / 2..]));
        assert!((out_db + 17.0).abs() < 0.6, "got {out_db}");
    }

    #[test]
    fn compressor_leaves_quiet_signal_alone() {
        let mut c = Compressor::new(CompressorSettings::default(), SR);
        let original = sine(440.0, db_to_gain(-40.0), SR, 9_600);
        let mut signal = original.clone();
        c.process(&mut [signal.as_mut_slice()]);
        assert!((peak(&signal) - peak(&original)).abs() < 1e-4);
    }

    #[test]
    fn soft_knee_is_continuous() {
        let below = static_reduction_db(-23.01, -20.0, 4.0, 6.0);
        let above = static_reduction_db(-16.99, -20.0, 4.0, 6.0);
        assert!(below.abs() < 0.01);
        assert!((above - static_reduction_db(-17.0, -20.0, 4.0, 0.0)).abs() < 0.02);
    }

    #[test]
    fn gate_attenuates_signal_below_threshold() {
        let settings = GateSettings {
            threshold_db: -40.0,
            ..GateSettings::default()
        };
        let mut g = Gate::new(settings, SR);
        let original = sine(440.0, db_to_gain(-60.0), SR, SR as usize);
        let mut signal = original.clone();
        g.process(&mut [signal.as_mut_slice()]);
        let reduction = db(peak(&signal[24_000..]) / peak(&original));
        assert!(reduction < -30.0, "got {reduction}");
    }

    #[test]
    fn gate_opens_for_loud_signal() {
        let mut g = Gate::new(GateSettings::default(), SR);
        let original = sine(440.0, db_to_gain(-10.0), SR, 9_600);
        let mut signal = original.clone();
        g.process(&mut [signal.as_mut_slice()]);
        let level = db(peak(&signal[4_800..]) / peak(&original));
        assert!(level > -0.1, "got {level}");
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut l = Limiter::new(-1.0, 50.0, SR);
        let mut left = sine(200.0, db_to_gain(6.0), SR, 4_800);
        let mut right = sine(3_000.0, db_to_gain(3.0), SR, 4_800);
        l.process(&mut [left.as_mut_slice(), right.as_mut_slice()]);
        let ceiling = db_to_gain(-1.0) + 1e-4;
        assert!(peak(&left) <= ceiling && peak(&right) <= ceiling);
    }
}
