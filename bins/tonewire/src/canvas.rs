use std::collections::{BTreeMap, BTreeSet};

use egui::{Pos2, Ui};
use egui_snarl::ui::{PinInfo, SnarlPin, SnarlViewer};
use egui_snarl::{InPin, InPinId, NodeId as SnarlId, OutPin, OutPinId, Snarl};
use tonewire_graph::catalog::{self, Category};
use tonewire_graph::{Edge, Graph, NodeId, NodeKind, PortRef};
use tonewire_proto::Command;

use crate::model::Model;
use crate::widgets::{self, Outbox};

/// Keeps a snarl canvas in step with the daemon's graph. Snarl nodes carry
/// the daemon's node id; the daemon stays the source of truth.
#[derive(Default)]
pub struct Canvas {
    pub snarl: Snarl<NodeId>,
    ids: BTreeMap<NodeId, SnarlId>,
}

fn to_pos(pos: [f32; 2]) -> Pos2 {
    Pos2::new(pos[0], pos[1])
}

impl Canvas {
    pub fn sync(&mut self, graph: &Graph, pointer_down: bool) {
        let stale: Vec<NodeId> = self
            .ids
            .keys()
            .filter(|id| !graph.nodes.contains_key(id))
            .copied()
            .collect();
        for id in stale {
            if let Some(snarl_id) = self.ids.remove(&id) {
                self.snarl.remove_node(snarl_id);
            }
        }
        for (&id, spec) in &graph.nodes {
            match self.ids.get(&id) {
                None => {
                    let snarl_id = self.snarl.insert_node(to_pos(spec.pos), id);
                    self.ids.insert(id, snarl_id);
                }
                Some(&snarl_id) if !pointer_down => {
                    if let Some(info) = self.snarl.get_node_info_mut(snarl_id) {
                        info.pos = to_pos(spec.pos);
                    }
                }
                Some(_) => {}
            }
        }

        let wanted: BTreeSet<(OutPinId, InPinId)> = graph
            .edges
            .iter()
            .filter_map(|e| {
                Some((
                    OutPinId {
                        node: *self.ids.get(&e.from.node)?,
                        output: usize::from(e.from.port),
                    },
                    InPinId {
                        node: *self.ids.get(&e.to.node)?,
                        input: usize::from(e.to.port),
                    },
                ))
            })
            .collect();
        let current: BTreeSet<(OutPinId, InPinId)> = self.snarl.wires().collect();
        for &(from, to) in current.difference(&wanted) {
            self.snarl.disconnect(from, to);
        }
        for &(from, to) in wanted.difference(&current) {
            self.snarl.connect(from, to);
        }
    }

    /// Nodes the user dragged to a new spot, reported once the drag ends.
    pub fn moved_nodes(&self, graph: &Graph) -> Vec<(NodeId, [f32; 2])> {
        self.snarl
            .nodes_info()
            .filter_map(|info| {
                let spec = graph.nodes.get(&info.value)?;
                let pos = [info.pos.x.round(), info.pos.y.round()];
                let moved =
                    (pos[0] - spec.pos[0]).abs() > 0.5 || (pos[1] - spec.pos[1]).abs() > 0.5;
                moved.then_some((info.value, pos))
            })
            .collect()
    }
}

pub struct Viewer<'a> {
    pub model: &'a mut Model,
    pub outbox: &'a mut Outbox,
    pub selected: &'a mut Option<NodeId>,
}

fn edge(snarl: &Snarl<NodeId>, from: OutPinId, to: InPinId) -> Edge {
    Edge {
        from: PortRef::new(snarl[from.node], from.output as u8),
        to: PortRef::new(snarl[to.node], to.input as u8),
    }
}

impl SnarlViewer<NodeId> for Viewer<'_> {
    fn title(&mut self, node: &NodeId) -> String {
        self.model
            .graph
            .nodes
            .get(node)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    fn inputs(&mut self, node: &NodeId) -> usize {
        self.model
            .graph
            .nodes
            .get(node)
            .map_or(0, |s| usize::from(catalog::describe(&s.kind).inputs))
    }

    fn outputs(&mut self, node: &NodeId) -> usize {
        self.model
            .graph
            .nodes
            .get(node)
            .map_or(0, |s| usize::from(catalog::describe(&s.kind).outputs))
    }

    fn show_input(
        &mut self,
        pin: &InPin,
        ui: &mut Ui,
        snarl: &mut Snarl<NodeId>,
    ) -> impl SnarlPin + 'static {
        let spec = self.model.graph.nodes.get(&snarl[pin.id.node]);
        if let Some(spec) = spec
            && matches!(spec.kind, NodeKind::Mixer { .. })
        {
            ui.label(format!("In {}", pin.id.input + 1));
        }
        PinInfo::circle().with_fill(widgets::channel_color(spec.map_or(2, |s| s.channels)))
    }

    fn show_output(
        &mut self,
        pin: &OutPin,
        ui: &mut Ui,
        snarl: &mut Snarl<NodeId>,
    ) -> impl SnarlPin + 'static {
        let spec = self.model.graph.nodes.get(&snarl[pin.id.node]);
        if let Some(spec) = spec
            && matches!(spec.kind, NodeKind::Splitter { .. })
        {
            ui.label(format!("Out {}", pin.id.output + 1));
        }
        PinInfo::circle().with_fill(widgets::channel_color(spec.map_or(2, |s| s.channels)))
    }

    fn has_body(&mut self, _: &NodeId) -> bool {
        true
    }

    fn show_body(
        &mut self,
        node: SnarlId,
        _: &[InPin],
        _: &[OutPin],
        ui: &mut Ui,
        snarl: &mut Snarl<NodeId>,
    ) {
        let id = snarl[node];
        let Some(spec) = self.model.graph.nodes.get(&id).cloned() else {
            return;
        };
        ui.vertical(|ui| {
            ui.set_max_width(240.0);
            widgets::status_line(ui, self.model, id);
            if spec.kind.is_endpoint() {
                widgets::device_picker(ui, self.model, self.outbox, id, &spec);
                if !spec.apps.is_empty() {
                    let names: Vec<&str> = spec.apps.iter().map(|a| a.binary.as_str()).collect();
                    ui.weak(format!("Apps: {}", names.join(", ")));
                }
            }
            if spec.kind == NodeKind::Eq {
                widgets::eq_curve(ui, &spec);
            }
            for info in widgets::inline_params(&spec.kind) {
                widgets::param(ui, self.model, self.outbox, id, &info);
            }
            let dynamics = catalog::describe(&spec.kind).category == Category::Dynamics;
            widgets::meter(ui, self.model.meters.get(&id), dynamics);
            ui.horizontal(|ui| {
                if spec.bypass {
                    ui.weak("Bypassed");
                }
                if ui.small_button("Details").clicked() {
                    *self.selected = Some(id);
                }
            });
        });
    }

    fn has_graph_menu(&mut self, _: Pos2, _: &mut Snarl<NodeId>) -> bool {
        true
    }

    fn show_graph_menu(&mut self, pos: Pos2, ui: &mut Ui, _: &mut Snarl<NodeId>) {
        ui.label("Add node");
        for category in Category::ALL {
            let kinds: Vec<_> = self
                .model
                .kinds
                .iter()
                .filter(|k| k.category == category)
                .cloned()
                .collect();
            if kinds.is_empty() {
                continue;
            }
            ui.menu_button(category.label(), |ui| {
                for kind in kinds {
                    let response =
                        ui.add_enabled(kind.unavailable.is_none(), egui::Button::new(&kind.label));
                    let response = match &kind.unavailable {
                        Some(reason) => response.on_disabled_hover_text(reason),
                        None => response,
                    };
                    if response.clicked() {
                        self.outbox.push(Command::AddNode {
                            kind: kind.kind.clone(),
                            pos: [pos.x, pos.y],
                        });
                        ui.close();
                    }
                }
            });
        }
    }

    fn has_node_menu(&mut self, _: &NodeId) -> bool {
        true
    }

    fn show_node_menu(
        &mut self,
        node: SnarlId,
        _: &[InPin],
        _: &[OutPin],
        ui: &mut Ui,
        snarl: &mut Snarl<NodeId>,
    ) {
        let id = snarl[node];
        let bypass = self.model.graph.nodes.get(&id).is_some_and(|s| s.bypass);
        if ui.button("Details").clicked() {
            *self.selected = Some(id);
            ui.close();
        }
        if ui
            .button(if bypass { "Enable" } else { "Bypass" })
            .clicked()
        {
            self.outbox.push(Command::SetBypass {
                node: id,
                bypass: !bypass,
            });
            ui.close();
        }
        if ui.button("Remove").clicked() {
            self.outbox.push(Command::RemoveNode { node: id });
            if *self.selected == Some(id) {
                *self.selected = None;
            }
            ui.close();
        }
    }

    fn connect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<NodeId>) {
        let edge = edge(snarl, from.id, to.id);
        snarl.connect(from.id, to.id);
        self.outbox.push(Command::Connect { edge });
    }

    fn disconnect(&mut self, from: &OutPin, to: &InPin, snarl: &mut Snarl<NodeId>) {
        let edge = edge(snarl, from.id, to.id);
        snarl.disconnect(from.id, to.id);
        self.outbox.push(Command::Disconnect { edge });
    }

    fn drop_outputs(&mut self, pin: &OutPin, snarl: &mut Snarl<NodeId>) {
        for remote in &pin.remotes {
            self.outbox.push(Command::Disconnect {
                edge: edge(snarl, pin.id, *remote),
            });
        }
        snarl.drop_outputs(pin.id);
    }

    fn drop_inputs(&mut self, pin: &InPin, snarl: &mut Snarl<NodeId>) {
        for remote in &pin.remotes {
            self.outbox.push(Command::Disconnect {
                edge: edge(snarl, *remote, pin.id),
            });
        }
        snarl.drop_inputs(pin.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonewire_graph::NodeSpec;

    fn graph() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let mut b_spec = NodeSpec::new(NodeKind::Mixer { inputs: 2 });
        b_spec.pos = [300.0, 40.0];
        let b = g.add_node(b_spec);
        g.connect(Edge {
            from: PortRef::new(a, 0),
            to: PortRef::new(b, 1),
        })
        .unwrap();
        (g, a, b)
    }

    #[test]
    fn sync_mirrors_nodes_and_wires() {
        let (mut g, a, b) = graph();
        let mut canvas = Canvas::default();
        canvas.sync(&g, false);
        assert_eq!(canvas.snarl.nodes().count(), 2);
        let wires: Vec<_> = canvas.snarl.wires().collect();
        assert_eq!(wires.len(), 1);
        assert_eq!(canvas.snarl[wires[0].0.node], a);
        assert_eq!(canvas.snarl[wires[0].1.node], b);
        assert_eq!(wires[0].1.input, 1);

        g.remove_node(a);
        canvas.sync(&g, false);
        assert_eq!(canvas.snarl.nodes().count(), 1);
        assert_eq!(canvas.snarl.wires().count(), 0);
    }

    #[test]
    fn local_drag_is_reported_and_not_overwritten_mid_drag() {
        let (g, _, b) = graph();
        let mut canvas = Canvas::default();
        canvas.sync(&g, false);
        let snarl_b = canvas.ids[&b];
        canvas.snarl.get_node_info_mut(snarl_b).unwrap().pos = Pos2::new(500.0, 90.0);
        canvas.sync(&g, true);
        assert_eq!(canvas.moved_nodes(&g), vec![(b, [500.0, 90.0])]);
        canvas.sync(&g, false);
        assert!(canvas.moved_nodes(&g).is_empty());
    }
}
