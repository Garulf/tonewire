use std::collections::VecDeque;

use nodeaudio_graph::{Graph, NodeId};

const CAPACITY: usize = 200;

type DragKey = (NodeId, String);

struct Entry {
    graph: Graph,
    drag: Option<DragKey>,
}

/// Undo/redo of whole-graph snapshots. Graphs are small, so snapshots keep
/// undo trivially correct for every kind of edit.
#[derive(Default)]
pub struct History {
    undo: VecDeque<Entry>,
    redo: Vec<Entry>,
}

impl History {
    /// Records `before` as an undo step. Consecutive edits of the same param
    /// made while dragging collapse into one step, closed by the final
    /// non-dragging edit.
    pub fn record(&mut self, before: &Graph, drag: Option<DragKey>, dragging: bool) {
        if let (Some(key), Some(last)) = (&drag, self.undo.back_mut())
            && last.drag.as_ref() == Some(key)
        {
            if !dragging {
                last.drag = None;
            }
            return;
        }
        self.undo.push_back(Entry {
            graph: before.clone(),
            drag: if dragging { drag } else { None },
        });
        if self.undo.len() > CAPACITY {
            self.undo.pop_front();
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, current: &Graph) -> Option<Graph> {
        let entry = self.undo.pop_back()?;
        self.redo.push(Entry {
            graph: current.clone(),
            drag: None,
        });
        Some(entry.graph)
    }

    pub fn redo(&mut self, current: &Graph) -> Option<Graph> {
        let entry = self.redo.pop()?;
        self.undo.push_back(Entry {
            graph: current.clone(),
            drag: None,
        });
        Some(entry.graph)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodeaudio_graph::{NodeKind, NodeSpec};

    fn graph_with(nodes: usize) -> Graph {
        let mut g = Graph::default();
        for _ in 0..nodes {
            g.add_node(NodeSpec::new(NodeKind::Gain));
        }
        g
    }

    #[test]
    fn undo_and_redo_walk_snapshots() {
        let mut h = History::default();
        let (g0, g1, g2) = (graph_with(0), graph_with(1), graph_with(2));
        h.record(&g0, None, false);
        h.record(&g1, None, false);
        assert_eq!(h.undo(&g2), Some(g1.clone()));
        assert_eq!(h.undo(&g1), Some(g0.clone()));
        assert_eq!(h.undo(&g0), None);
        assert_eq!(h.redo(&g0), Some(g1.clone()));
        assert_eq!(h.redo(&g1), Some(g2));
    }

    #[test]
    fn a_drag_is_one_undo_step() {
        let mut h = History::default();
        let key = (NodeId(1), "gain_db".to_string());
        let start = graph_with(0);
        h.record(&start, Some(key.clone()), true);
        h.record(&graph_with(1), Some(key.clone()), true);
        h.record(&graph_with(2), Some(key.clone()), false);
        h.record(&graph_with(3), Some(key), true);
        assert_eq!(h.undo.len(), 2);
        assert_eq!(h.undo.front().unwrap().graph, start);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut h = History::default();
        h.record(&graph_with(0), None, false);
        h.undo(&graph_with(1));
        assert!(h.can_redo());
        h.record(&graph_with(0), None, false);
        assert!(!h.can_redo());
    }
}
