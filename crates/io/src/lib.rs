#![deny(unsafe_op_in_unsafe_fn)]

pub mod hub;
pub mod null;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire;
pub mod platform;
pub mod ring;
pub mod router;
#[cfg(windows)]
pub mod wasapi;

pub use null::{NullPlatform, NullProbe};
pub use platform::{Platform, PlatformEvent, StreamConfig, StreamHandle};
pub use router::{Changes, Router};
