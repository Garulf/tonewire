#![deny(unsafe_op_in_unsafe_fn)]

pub mod buffer;
pub mod catalog;
pub mod engine;
pub mod format;
pub mod model;
pub mod nodes;
pub mod plan;
pub mod processor;
pub mod shared;

pub const MAX_CHANNELS: usize = 8;

pub use buffer::AudioBuf;
pub use engine::{Engine, EngineConfig, EngineHandle, NodeMeter, StatsSnapshot, engine};
pub use model::{
    AppMode, AppRule, DeviceRef, Edge, EndpointKind, EndpointSpec, Graph, GraphError, NodeId,
    NodeKind, NodeSpec, NvAfxEffect, PortRef,
};
pub use nodes::ExternalFactory;
pub use processor::{EndpointIo, NodeProcessor, ProcessCtx};
