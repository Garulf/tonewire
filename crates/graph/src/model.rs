use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortRef {
    pub node: NodeId,
    pub port: u8,
}

impl PortRef {
    pub fn new(node: NodeId, port: u8) -> Self {
        Self { node, port }
    }
}

/// `from` is an output port, `to` is an input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub from: PortRef,
    pub to: PortRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NvAfxEffect {
    Denoiser,
    Dereverb,
    DereverbDenoiser,
    StudioVoice,
    SpeakerFocus,
}

impl NvAfxEffect {
    pub const ALL: [Self; 5] = [
        Self::Denoiser,
        Self::Dereverb,
        Self::DereverbDenoiser,
        Self::StudioVoice,
        Self::SpeakerFocus,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Denoiser => "NVIDIA Denoiser",
            Self::Dereverb => "NVIDIA Dereverb",
            Self::DereverbDenoiser => "NVIDIA Dereverb + Denoiser",
            Self::StudioVoice => "NVIDIA Studio Voice",
            Self::SpeakerFocus => "NVIDIA Speaker Focus",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    DeviceInput,
    DeviceOutput,
    /// A virtual speaker other apps play into; a source inside the graph.
    VirtualSpeaker,
    /// A virtual microphone other apps record from; a destination in the graph.
    VirtualMic,
    AppSource,
    Mixer {
        inputs: u8,
    },
    Splitter {
        outputs: u8,
    },
    Gain,
    Pan,
    Eq,
    DeEsser,
    Compressor,
    Gate,
    Limiter,
    Denoise,
    NvAfx {
        effect: NvAfxEffect,
    },
    Meter,
}

impl NodeKind {
    pub fn is_endpoint(&self) -> bool {
        self.endpoint_kind().is_some()
    }

    pub fn endpoint_kind(&self) -> Option<EndpointKind> {
        Some(match self {
            Self::DeviceInput => EndpointKind::DeviceInput,
            Self::DeviceOutput => EndpointKind::DeviceOutput,
            Self::VirtualSpeaker => EndpointKind::VirtualSpeaker,
            Self::VirtualMic => EndpointKind::VirtualMic,
            Self::AppSource => EndpointKind::AppSource,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    DeviceInput,
    DeviceOutput,
    VirtualSpeaker,
    VirtualMic,
    AppSource,
}

impl EndpointKind {
    pub fn is_capture(self) -> bool {
        matches!(
            self,
            Self::DeviceInput | Self::VirtualSpeaker | Self::AppSource
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceRef {
    /// Stable backend identifier: PipeWire `node.name` or WASAPI endpoint id.
    pub id: String,
    pub name: String,
    /// Capture what a playback device outputs instead of a microphone.
    pub loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppRule {
    /// Process binary name, matched case-insensitively (`spotify`, `Discord.exe`).
    pub binary: String,
}

impl AppRule {
    pub fn matches(&self, binary: &str) -> bool {
        let strip = |s: &str| {
            let lower = s.to_ascii_lowercase();
            lower
                .strip_suffix(".exe")
                .map(str::to_owned)
                .unwrap_or(lower)
        };
        strip(&self.binary) == strip(binary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppMode {
    /// The app is heard only through the graph.
    #[default]
    Move,
    /// The graph receives a copy and the app keeps playing where it was.
    Tap,
}

fn default_channels() -> u8 {
    2
}

/// Every field but `kind` has a default so graph files written by older
/// versions keep loading when fields are added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub kind: NodeKind,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pos: [f32; 2],
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub params: BTreeMap<String, f32>,
    /// Device for device I/O nodes; `None` follows the system default.
    #[serde(default)]
    pub device: Option<DeviceRef>,
    /// Apps routed into an App Source, or apps that should record from a Virtual Mic.
    #[serde(default)]
    pub apps: Vec<AppRule>,
    #[serde(default)]
    pub app_mode: AppMode,
}

impl NodeSpec {
    pub fn new(kind: NodeKind) -> Self {
        let info = catalog::describe(&kind);
        Self {
            name: info.label.clone(),
            channels: info.default_channels,
            kind,
            pos: [0.0, 0.0],
            bypass: false,
            params: BTreeMap::new(),
            device: None,
            apps: Vec::new(),
            app_mode: AppMode::default(),
        }
    }

    pub fn param(&self, key: &str) -> f32 {
        catalog::describe(&self.kind)
            .param(key)
            .map(|p| p.clamp(self.params.get(key).copied().unwrap_or(p.default)))
            .unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
pub enum GraphError {
    #[error("node {0} does not exist")]
    UnknownNode(NodeId),
    #[error("node {0} has no output port {1}")]
    NoSuchOutput(NodeId, u8),
    #[error("node {0} has no input port {1}")]
    NoSuchInput(NodeId, u8),
    #[error("that connection already exists")]
    DuplicateEdge,
    #[error("connecting these would create a feedback loop")]
    Cycle,
    #[error("node {0} has no parameter `{1}`")]
    UnknownParam(NodeId, String),
    #[error("channel count must be between 1 and {max}", max = crate::MAX_CHANNELS)]
    BadChannelCount,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: BTreeMap<NodeId, NodeSpec>,
    pub edges: BTreeSet<Edge>,
    pub next_id: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub node: NodeId,
    pub kind: EndpointKind,
    pub name: String,
    pub channels: u8,
    pub device: Option<DeviceRef>,
    pub apps: Vec<AppRule>,
    pub app_mode: AppMode,
}

impl Graph {
    pub fn add_node(&mut self, spec: NodeSpec) -> NodeId {
        self.next_id = self.next_id.max(1);
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(id, spec);
        id
    }

    pub fn insert_node(&mut self, id: NodeId, spec: NodeSpec) {
        self.next_id = self.next_id.max(id.0 + 1);
        self.nodes.insert(id, spec);
    }

    pub fn remove_node(&mut self, id: NodeId) -> Option<(NodeSpec, Vec<Edge>)> {
        let spec = self.nodes.remove(&id)?;
        let removed: Vec<Edge> = self
            .edges
            .iter()
            .filter(|e| e.from.node == id || e.to.node == id)
            .copied()
            .collect();
        for e in &removed {
            self.edges.remove(e);
        }
        Some((spec, removed))
    }

    pub fn node(&self, id: NodeId) -> Result<&NodeSpec, GraphError> {
        self.nodes.get(&id).ok_or(GraphError::UnknownNode(id))
    }

    pub fn node_mut(&mut self, id: NodeId) -> Result<&mut NodeSpec, GraphError> {
        self.nodes.get_mut(&id).ok_or(GraphError::UnknownNode(id))
    }

    pub fn check_edge(&self, edge: Edge) -> Result<(), GraphError> {
        let from = self.node(edge.from.node)?;
        let to = self.node(edge.to.node)?;
        if edge.from.port >= catalog::describe(&from.kind).outputs {
            return Err(GraphError::NoSuchOutput(edge.from.node, edge.from.port));
        }
        if edge.to.port >= catalog::describe(&to.kind).inputs {
            return Err(GraphError::NoSuchInput(edge.to.node, edge.to.port));
        }
        if self.edges.contains(&edge) {
            return Err(GraphError::DuplicateEdge);
        }
        if edge.from.node == edge.to.node || self.reaches(edge.to.node, edge.from.node) {
            return Err(GraphError::Cycle);
        }
        Ok(())
    }

    pub fn connect(&mut self, edge: Edge) -> Result<(), GraphError> {
        self.check_edge(edge)?;
        self.edges.insert(edge);
        Ok(())
    }

    pub fn disconnect(&mut self, edge: Edge) -> bool {
        self.edges.remove(&edge)
    }

    pub fn set_param(&mut self, id: NodeId, key: &str, value: f32) -> Result<f32, GraphError> {
        let spec = self.node_mut(id)?;
        let info = catalog::describe(&spec.kind);
        let param = info
            .param(key)
            .ok_or_else(|| GraphError::UnknownParam(id, key.to_owned()))?;
        let value = param.clamp(value);
        spec.params.insert(key.to_owned(), value);
        Ok(value)
    }

    pub fn set_channels(&mut self, id: NodeId, channels: u8) -> Result<(), GraphError> {
        if channels == 0 || usize::from(channels) > crate::MAX_CHANNELS {
            return Err(GraphError::BadChannelCount);
        }
        self.node_mut(id)?.channels = channels;
        Ok(())
    }

    fn reaches(&self, start: NodeId, target: NodeId) -> bool {
        let mut stack = vec![start];
        let mut seen = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if n == target {
                return true;
            }
            if seen.insert(n) {
                stack.extend(
                    self.edges
                        .iter()
                        .filter(|e| e.from.node == n)
                        .map(|e| e.to.node),
                );
            }
        }
        false
    }

    /// Nodes in an order where every node comes after everything feeding it.
    pub fn topological_order(&self) -> Result<Vec<NodeId>, GraphError> {
        let mut indegree: BTreeMap<NodeId, usize> = self.nodes.keys().map(|&id| (id, 0)).collect();
        for e in &self.edges {
            *indegree
                .get_mut(&e.to.node)
                .ok_or(GraphError::UnknownNode(e.to.node))? += 1;
            if !self.nodes.contains_key(&e.from.node) {
                return Err(GraphError::UnknownNode(e.from.node));
            }
        }
        let mut ready: Vec<NodeId> = indegree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .rev()
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = ready.pop() {
            order.push(id);
            for e in self.edges.iter().filter(|e| e.from.node == id) {
                let d = indegree.get_mut(&e.to.node).expect("checked above");
                *d -= 1;
                if *d == 0 {
                    ready.push(e.to.node);
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            Err(GraphError::Cycle)
        }
    }

    /// Every structural problem in the graph, for graphs that did not come
    /// through [`Graph::connect`] (files, imports).
    pub fn validate(&self) -> Result<(), GraphError> {
        for e in &self.edges {
            let from = self.node(e.from.node)?;
            let to = self.node(e.to.node)?;
            if e.from.port >= catalog::describe(&from.kind).outputs {
                return Err(GraphError::NoSuchOutput(e.from.node, e.from.port));
            }
            if e.to.port >= catalog::describe(&to.kind).inputs {
                return Err(GraphError::NoSuchInput(e.to.node, e.to.port));
            }
        }
        for spec in self.nodes.values() {
            if spec.channels == 0 || usize::from(spec.channels) > crate::MAX_CHANNELS {
                return Err(GraphError::BadChannelCount);
            }
        }
        self.topological_order().map(|_| ())
    }

    pub fn endpoints(&self) -> Vec<EndpointSpec> {
        self.nodes
            .iter()
            .filter_map(|(&node, spec)| {
                Some(EndpointSpec {
                    node,
                    kind: spec.kind.endpoint_kind()?,
                    name: spec.name.clone(),
                    channels: spec.channels,
                    device: spec.device.clone(),
                    apps: spec.apps.clone(),
                    app_mode: spec.app_mode,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let b = g.add_node(NodeSpec::new(NodeKind::Eq));
        let c = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        g.connect(Edge {
            from: PortRef::new(a, 0),
            to: PortRef::new(b, 0),
        })
        .unwrap();
        g.connect(Edge {
            from: PortRef::new(b, 0),
            to: PortRef::new(c, 0),
        })
        .unwrap();
        (g, a, b, c)
    }

    #[test]
    fn cycle_is_rejected() {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::Gain));
        let b = g.add_node(NodeSpec::new(NodeKind::Gain));
        g.connect(Edge {
            from: PortRef::new(a, 0),
            to: PortRef::new(b, 0),
        })
        .unwrap();
        let back = Edge {
            from: PortRef::new(b, 0),
            to: PortRef::new(a, 0),
        };
        assert_eq!(g.connect(back), Err(GraphError::Cycle));
        let own = Edge {
            from: PortRef::new(a, 0),
            to: PortRef::new(a, 0),
        };
        assert_eq!(g.connect(own), Err(GraphError::Cycle));
    }

    #[test]
    fn duplicate_and_bad_ports_are_rejected() {
        let (mut g, a, b, c) = chain();
        let dup = Edge {
            from: PortRef::new(a, 0),
            to: PortRef::new(b, 0),
        };
        assert_eq!(g.connect(dup), Err(GraphError::DuplicateEdge));
        let into_source = Edge {
            from: PortRef::new(b, 0),
            to: PortRef::new(a, 0),
        };
        assert_eq!(g.connect(into_source), Err(GraphError::NoSuchInput(a, 0)));
        let from_sink = Edge {
            from: PortRef::new(c, 0),
            to: PortRef::new(b, 0),
        };
        assert_eq!(g.connect(from_sink), Err(GraphError::NoSuchOutput(c, 0)));
    }

    #[test]
    fn topological_order_respects_edges() {
        let (g, a, b, c) = chain();
        let order = g.topological_order().unwrap();
        let pos = |id| order.iter().position(|&n| n == id).unwrap();
        assert!(pos(a) < pos(b) && pos(b) < pos(c));
    }

    #[test]
    fn validate_catches_cycles_from_files() {
        let (mut g, a, b, _) = chain();
        g.edges.insert(Edge {
            from: PortRef::new(b, 0),
            to: PortRef::new(a, 0),
        });
        assert!(g.validate().is_err());
    }

    #[test]
    fn removing_a_node_removes_its_edges() {
        let (mut g, _, b, _) = chain();
        let (_, removed) = g.remove_node(b).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn params_are_clamped_and_unknown_params_rejected() {
        let (mut g, _, b, _) = chain();
        assert_eq!(g.set_param(b, "band1_gain_db", 99.0).unwrap(), 24.0);
        assert!(matches!(
            g.set_param(b, "nope", 1.0),
            Err(GraphError::UnknownParam(..))
        ));
    }

    #[test]
    fn app_rules_ignore_case_and_exe_suffix() {
        let rule = AppRule {
            binary: "Discord.exe".into(),
        };
        assert!(rule.matches("discord"));
        assert!(rule.matches("DISCORD.EXE"));
        assert!(!rule.matches("discord-ptb"));
    }
}
