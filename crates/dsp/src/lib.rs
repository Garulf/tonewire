#![deny(unsafe_op_in_unsafe_fn)]

pub mod biquad;
pub mod deesser;
pub mod denoise;
pub mod dynamics;
pub mod level;
pub mod meter;
pub mod smooth;

#[cfg(test)]
pub(crate) mod testutil;

pub use biquad::{Biquad, BiquadCoeffs, BiquadKind};
pub use deesser::DeEsser;
pub use deesser::DeEsserSettings;
pub use denoise::Denoiser;
pub use dynamics::{Compressor, CompressorSettings, EnvelopeFollower, Gate, GateSettings, Limiter};
pub use level::{db_to_gain, gain_to_db};
pub use meter::PeakRms;
pub use smooth::Smoothed;
