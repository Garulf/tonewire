use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Capture,
    Playback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub direction: Direction,
    pub channels: u8,
    pub is_virtual: bool,
    pub is_default: bool,
}

/// An app currently playing or recording audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppStream {
    pub id: u64,
    pub binary: String,
    pub name: String,
    pub direction: Direction,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndpointStatus {
    pub connected: bool,
    pub xruns: u32,
    pub message: Option<String>,
}
