use nnnoiseless::DenoiseState;

const I16_SCALE: f32 = 32_768.0;

/// RNNoise speech denoiser for one channel at 48 kHz. Output is delayed by
/// exactly [`Denoiser::LATENCY_FRAMES`].
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    input: [f32; DenoiseState::FRAME_SIZE],
    output: [f32; DenoiseState::FRAME_SIZE],
    position: usize,
    voice_probability: f32,
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    pub const LATENCY_FRAMES: usize = DenoiseState::FRAME_SIZE;
    pub const SAMPLE_RATE: f32 = 48_000.0;

    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            input: [0.0; DenoiseState::FRAME_SIZE],
            output: [0.0; DenoiseState::FRAME_SIZE],
            position: 0,
            voice_probability: 0.0,
        }
    }

    pub fn voice_probability(&self) -> f32 {
        self.voice_probability
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let delayed = self.output[self.position] / I16_SCALE;
            self.input[self.position] = *s * I16_SCALE;
            *s = delayed;
            self.position += 1;
            if self.position == DenoiseState::FRAME_SIZE {
                self.position = 0;
                self.voice_probability = self.state.process_frame(&mut self.output, &self.input);
            }
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::rms;

    fn noise(len: usize, amplitude: f32) -> Vec<f32> {
        let mut seed = 0x2545_f491u32;
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    #[test]
    fn reduces_stationary_noise() {
        let original = noise(48_000 * 2, 0.05);
        let mut signal = original.clone();
        Denoiser::new().process(&mut signal);
        let tail = 48_000;
        assert_eq!(signal.len(), original.len());
        assert!(rms(&signal[tail..]) < rms(&original[tail..]) * 0.5);
    }

    #[test]
    fn delays_by_one_frame() {
        let mut d = Denoiser::new();
        let mut first = vec![0.3; Denoiser::LATENCY_FRAMES];
        d.process(&mut first);
        assert!(first.iter().all(|&s| s == 0.0));
    }
}
