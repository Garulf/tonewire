use serde::{Deserialize, Serialize};

use crate::model::{Edge, Graph, GraphError, NodeId, NodeSpec};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("graph file is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("graph file version {0} is newer than this build supports ({FORMAT_VERSION})")]
    UnsupportedVersion(u32),
    #[error("graph file is inconsistent: {0}")]
    Invalid(#[from] GraphError),
}

#[derive(Serialize, Deserialize)]
struct GraphFile {
    version: u32,
    #[serde(default)]
    nodes: Vec<NodeEntry>,
    #[serde(default)]
    edges: Vec<Edge>,
}

#[derive(Serialize, Deserialize)]
struct NodeEntry {
    id: NodeId,
    #[serde(flatten)]
    spec: NodeSpec,
}

pub fn to_toml(graph: &Graph) -> String {
    let file = GraphFile {
        version: FORMAT_VERSION,
        nodes: graph
            .nodes
            .iter()
            .map(|(&id, spec)| NodeEntry {
                id,
                spec: spec.clone(),
            })
            .collect(),
        edges: graph.edges.iter().copied().collect(),
    };
    toml::to_string_pretty(&file).expect("graph is always serializable")
}

pub fn from_toml(text: &str) -> Result<Graph, FormatError> {
    let file: GraphFile = toml::from_str(text)?;
    if file.version > FORMAT_VERSION {
        return Err(FormatError::UnsupportedVersion(file.version));
    }
    let mut graph = Graph::default();
    for entry in file.nodes {
        graph.insert_node(entry.id, entry.spec);
    }
    graph.edges = file.edges.into_iter().collect();
    graph.validate()?;
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppRule, DeviceRef, NodeKind, PortRef};

    fn sample() -> Graph {
        let mut g = Graph::default();
        let mut mic = NodeSpec::new(NodeKind::DeviceInput);
        mic.device = Some(DeviceRef {
            id: "alsa_input.usb-mic".into(),
            name: "USB Mic".into(),
            loopback: false,
        });
        mic.channels = 1;
        let mic = g.add_node(mic);
        let eq = g.add_node(NodeSpec::new(NodeKind::Eq));
        let mut app = NodeSpec::new(NodeKind::AppSource);
        app.apps.push(AppRule {
            binary: "spotify".into(),
        });
        g.add_node(app);
        g.add_node(NodeSpec::new(NodeKind::Mixer { inputs: 3 }));
        g.set_param(eq, "band1_gain_db", 6.0).unwrap();
        g.connect(Edge {
            from: PortRef::new(mic, 0),
            to: PortRef::new(eq, 0),
        })
        .unwrap();
        g
    }

    #[test]
    fn round_trips() {
        let g = sample();
        let text = to_toml(&g);
        assert_eq!(from_toml(&text).unwrap(), g);
    }

    #[test]
    fn newer_version_is_rejected() {
        let text = to_toml(&sample()).replace("version = 1", "version = 99");
        assert!(matches!(
            from_toml(&text),
            Err(FormatError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn dangling_edge_is_rejected() {
        let text = format!(
            "{}\n[[edges]]\nfrom = {{ node = 1, port = 0 }}\nto = {{ node = 77, port = 0 }}\n",
            to_toml(&sample())
        );
        assert!(matches!(from_toml(&text), Err(FormatError::Invalid(_))));
    }

    #[test]
    fn garbage_is_a_parse_error() {
        assert!(matches!(from_toml("nodes = ["), Err(FormatError::Parse(_))));
    }
}
