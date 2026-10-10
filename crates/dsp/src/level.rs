pub const SILENCE_DB: f32 = -120.0;

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 1e-6 {
        SILENCE_DB
    } else {
        20.0 * gain.log10()
    }
}

pub(crate) fn time_coeff(time_ms: f32, sample_rate: f32) -> f32 {
    let samples = time_ms.max(0.01) * 0.001 * sample_rate;
    (-1.0 / samples).exp()
}
