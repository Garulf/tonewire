use serde::{Deserialize, Serialize};

use tonewire_graph::catalog::Category;
use tonewire_graph::{
    AppMode, AppRule, AppStream, DeviceInfo, DeviceRef, Edge, EndpointStatus, Graph, NodeId,
    NodeKind,
};

pub const PROTOCOL_VERSION: u32 = 1;

pub type RequestId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    GetState,
    AddNode {
        kind: NodeKind,
        pos: [f32; 2],
    },
    RemoveNode {
        node: NodeId,
    },
    MoveNode {
        node: NodeId,
        pos: [f32; 2],
    },
    Connect {
        edge: Edge,
    },
    Disconnect {
        edge: Edge,
    },
    /// `dragging` edits of the same param merge into one undo step until a
    /// non-dragging edit arrives.
    SetParam {
        node: NodeId,
        key: String,
        value: f32,
        dragging: bool,
    },
    SetBypass {
        node: NodeId,
        bypass: bool,
    },
    SetChannels {
        node: NodeId,
        channels: u8,
    },
    SetDevice {
        node: NodeId,
        device: Option<DeviceRef>,
    },
    RenameNode {
        node: NodeId,
        name: String,
    },
    SetApps {
        node: NodeId,
        apps: Vec<AppRule>,
        mode: AppMode,
    },
    ImportGraph {
        toml: String,
    },
    ExportGraph,
    SetEngineConfig(EngineSettings),
    SetMuteAll(bool),
    Undo,
    Redo,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Exported(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSettings {
    pub sample_rate: u32,
    pub block_frames: u32,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            block_frames: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindAvailability {
    pub kind: NodeKind,
    pub category: Category,
    pub label: String,
    /// Why the kind cannot be used right now, if it cannot.
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphState {
    pub graph: Graph,
    pub can_undo: bool,
    pub can_redo: bool,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonState {
    pub graph: GraphState,
    pub devices: Vec<DeviceInfo>,
    pub apps: Vec<AppStream>,
    pub kinds: Vec<KindAvailability>,
    pub engine: EngineSettings,
    pub backend: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeterReading {
    pub node: NodeId,
    pub peak: f32,
    pub rms: f32,
    pub reduction_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineStats {
    pub dsp_load: f32,
    pub sample_rate: u32,
    pub block_frames: u32,
    /// Worst input-to-output processing latency through the graph, in ms.
    pub graph_latency_ms: f32,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    State(Box<DaemonState>),
    GraphChanged(Box<GraphState>),
    DevicesChanged(Vec<DeviceInfo>),
    AppsChanged(Vec<AppStream>),
    NodeStatus(Vec<(NodeId, EndpointStatus)>),
    Warnings(Vec<(NodeId, String)>),
    Meters(Vec<MeterReading>),
    Stats(EngineStats),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { version: u32 },
    Request { id: RequestId, command: Command },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello { version: u32 },
    Reply { id: RequestId, reply: Reply },
    Event(Event),
}
