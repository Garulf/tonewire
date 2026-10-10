use std::any::Any;

use tonewire_graph::{AppStream, DeviceInfo, EndpointSpec, NodeId};

use crate::hub::StreamPort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    pub sample_rate: u32,
    /// Engine block and ring target in frames.
    pub block_frames: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            block_frames: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformEvent {
    DevicesChanged,
    AppsChanged,
    StreamFailed(NodeId, String),
}

/// A running stream. Dropping it stops the stream and releases the device.
pub type StreamHandle = Box<dyn Any + Send>;

/// One audio system (PipeWire, WASAPI, or the null backend for tests).
pub trait Platform: Send {
    fn name(&self) -> &'static str;
    fn devices(&mut self) -> Vec<DeviceInfo>;
    fn apps(&mut self) -> Vec<AppStream>;
    /// Opens the stream behind `spec`, feeding or draining `port`.
    fn open(
        &mut self,
        spec: &EndpointSpec,
        port: StreamPort,
        config: StreamConfig,
    ) -> Result<StreamHandle, String>;
    /// Events since the last call. Must not block.
    fn poll(&mut self) -> Vec<PlatformEvent>;

    /// True when streams without an explicit device move to a new system
    /// default on their own (PipeWire); otherwise the router reopens them.
    fn follows_default(&self) -> bool {
        false
    }

    /// Frames a device stream delivers or consumes per callback.
    fn device_period(&self, config: StreamConfig) -> usize {
        config.block_frames as usize
    }
}
