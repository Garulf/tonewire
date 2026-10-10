use std::collections::BTreeMap;

use nodeaudio_graph::{AppStream, DeviceInfo, EndpointStatus, Graph, NodeId};
use nodeaudio_proto::{
    DaemonState, EngineSettings, EngineStats, Event, KindAvailability, MeterReading,
};

/// The editor's copy of daemon state, updated from events.
#[derive(Default)]
pub struct Model {
    pub connected: bool,
    pub connection_error: Option<String>,
    pub graph: Graph,
    pub can_undo: bool,
    pub can_redo: bool,
    pub muted: bool,
    pub devices: Vec<DeviceInfo>,
    pub apps: Vec<AppStream>,
    pub kinds: Vec<KindAvailability>,
    pub engine: EngineSettings,
    pub backend: String,
    pub status: BTreeMap<NodeId, EndpointStatus>,
    pub warnings: BTreeMap<NodeId, String>,
    pub meters: BTreeMap<NodeId, MeterReading>,
    pub stats: Option<EngineStats>,
    /// Param being dragged locally; its value is not overwritten by echoes.
    pub dragging: Option<(NodeId, String)>,
}

impl Model {
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::State(state) => self.apply_state(*state),
            Event::GraphChanged(state) => {
                let mut graph = state.graph;
                if let Some((node, key)) = &self.dragging
                    && let (Some(local), Some(remote)) =
                        (self.graph.nodes.get(node), graph.nodes.get_mut(node))
                    && let Some(&value) = local.params.get(key)
                {
                    remote.params.insert(key.clone(), value);
                }
                self.graph = graph;
                self.can_undo = state.can_undo;
                self.can_redo = state.can_redo;
                self.muted = state.muted;
            }
            Event::DevicesChanged(devices) => self.devices = devices,
            Event::AppsChanged(apps) => self.apps = apps,
            Event::NodeStatus(status) => self.status = status.into_iter().collect(),
            Event::Warnings(warnings) => self.warnings = warnings.into_iter().collect(),
            Event::Meters(meters) => {
                for meter in meters {
                    let entry = self.meters.entry(meter.node).or_insert(meter);
                    entry.peak = meter.peak.max(entry.peak * 0.8);
                    entry.rms = meter.rms;
                    entry.reduction_db = meter.reduction_db;
                }
            }
            Event::Stats(stats) => self.stats = Some(stats),
        }
    }

    fn apply_state(&mut self, state: DaemonState) {
        self.graph = state.graph.graph;
        self.can_undo = state.graph.can_undo;
        self.can_redo = state.graph.can_redo;
        self.muted = state.graph.muted;
        self.devices = state.devices;
        self.apps = state.apps;
        self.kinds = state.kinds;
        self.engine = state.engine;
        self.backend = state.backend;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodeaudio_graph::{NodeKind, NodeSpec};
    use nodeaudio_proto::GraphState;

    #[test]
    fn echo_does_not_override_a_param_being_dragged() {
        let mut model = Model::default();
        let id = model.graph.add_node(NodeSpec::new(NodeKind::Gain));
        model.graph.set_param(id, "gain_db", -10.0).unwrap();
        model.dragging = Some((id, "gain_db".into()));

        let mut stale = model.graph.clone();
        stale.set_param(id, "gain_db", -3.0).unwrap();
        stale.node_mut(id).unwrap().name = "Renamed".into();
        model.apply(Event::GraphChanged(Box::new(GraphState {
            graph: stale,
            can_undo: true,
            can_redo: false,
            muted: false,
        })));
        assert_eq!(model.graph.nodes[&id].params["gain_db"], -10.0);
        assert_eq!(model.graph.nodes[&id].name, "Renamed");
    }

    #[test]
    fn meters_decay_instead_of_dropping() {
        let mut model = Model::default();
        let reading = |peak| MeterReading {
            node: NodeId(1),
            peak,
            rms: 0.0,
            reduction_db: 0.0,
        };
        model.apply(Event::Meters(vec![reading(1.0)]));
        model.apply(Event::Meters(vec![reading(0.0)]));
        assert!((model.meters[&NodeId(1)].peak - 0.8).abs() < 1e-6);
    }
}
