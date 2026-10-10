use std::f32::consts::TAU;

pub fn sine(freq: f32, amplitude: f32, sample_rate: f32, len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| amplitude * (TAU * freq * i as f32 / sample_rate).sin())
        .collect()
}

pub fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0, |m, s| m.max(s.abs()))
}

pub fn db(ratio: f32) -> f32 {
    20.0 * ratio.log10()
}
