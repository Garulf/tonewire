use crate::level::time_coeff;

#[derive(Debug, Clone)]
pub struct Smoothed {
    value: f64,
    target: f64,
    coeff: f64,
}

impl Smoothed {
    pub fn new(value: f32, time_ms: f32, sample_rate: f32) -> Self {
        Self {
            value: f64::from(value),
            target: f64::from(value),
            coeff: f64::from(time_coeff(time_ms, sample_rate)),
        }
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = f64::from(target);
    }

    pub fn target(&self) -> f32 {
        self.target as f32
    }

    pub fn jump_to(&mut self, value: f32) {
        self.value = f64::from(value);
        self.target = f64::from(value);
    }

    pub fn is_settled(&self) -> bool {
        self.value == self.target
    }

    pub fn tick(&mut self) -> f32 {
        if self.value != self.target {
            self.value = self.target + (self.value - self.target) * self.coeff;
            if (self.value - self.target).abs() < 1e-7 {
                self.value = self.target;
            }
        }
        self.value as f32
    }

    pub fn apply(&mut self, samples: &mut [f32]) {
        if self.is_settled() {
            let gain = self.value as f32;
            if gain != 1.0 {
                samples.iter_mut().for_each(|s| *s *= gain);
            }
            return;
        }
        for s in samples {
            *s *= self.tick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target_and_settles() {
        let mut s = Smoothed::new(0.0, 5.0, 48_000.0);
        s.set_target(1.0);
        let first = s.tick();
        assert!(first > 0.0 && first < 0.01);
        for _ in 0..48_000 {
            s.tick();
        }
        assert!(s.is_settled());
        assert_eq!(s.tick(), 1.0);
    }
}
