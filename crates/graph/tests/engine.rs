use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use nodeaudio_graph::{
    AudioBuf, Edge, EndpointIo, EngineConfig, Graph, NodeId, NodeKind, NodeSpec, PortRef, engine,
};

struct CountingAlloc;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

const BLOCK: usize = 128;

/// Sine on every input endpoint, outputs recorded into preallocated storage.
struct SineIo {
    phase: f32,
    recorded: Vec<f32>,
    cursor: usize,
}

impl SineIo {
    fn new(capacity: usize) -> Self {
        Self {
            phase: 0.0,
            recorded: vec![0.0; capacity],
            cursor: 0,
        }
    }
}

impl EndpointIo for SineIo {
    fn read(&mut self, _: NodeId, dest: &mut AudioBuf, frames: usize) -> bool {
        let start = self.phase;
        for c in 0..dest.channel_count() {
            let mut phase = start;
            for s in dest.channel_mut(c, frames) {
                *s = 0.5 * phase.sin();
                phase += std::f32::consts::TAU * 440.0 / 48_000.0;
            }
            self.phase = phase % std::f32::consts::TAU;
        }
        true
    }

    fn write(&mut self, _: NodeId, src: &AudioBuf, frames: usize) {
        let end = (self.cursor + frames).min(self.recorded.len());
        let n = end - self.cursor;
        self.recorded[self.cursor..end].copy_from_slice(&src.channel(0, frames)[..n]);
        self.cursor = end;
    }
}

fn link(g: &mut Graph, from: NodeId, to: NodeId) {
    g.connect(Edge {
        from: PortRef::new(from, 0),
        to: PortRef::new(to, 0),
    })
    .unwrap();
}

fn chain(kinds: &[NodeKind]) -> (Graph, Vec<NodeId>) {
    let mut g = Graph::default();
    let mut ids = vec![g.add_node(NodeSpec::new(NodeKind::DeviceInput))];
    for kind in kinds {
        let id = g.add_node(NodeSpec::new(kind.clone()));
        link(&mut g, *ids.last().unwrap(), id);
        ids.push(id);
    }
    let out = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
    link(&mut g, *ids.last().unwrap(), out);
    ids.push(out);
    (g, ids)
}

fn config() -> EngineConfig {
    EngineConfig {
        sample_rate: 48_000.0,
        max_block: BLOCK,
    }
}

#[test]
fn processing_and_plan_swaps_do_not_allocate() {
    let (mut g, ids) = chain(&[
        NodeKind::Gate,
        NodeKind::Eq,
        NodeKind::DeEsser,
        NodeKind::Compressor,
        NodeKind::Denoise,
        NodeKind::Limiter,
        NodeKind::Meter,
    ]);
    let (mut handle, mut engine) = engine(config(), None);
    handle.set_metering(true);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(0);
    engine.prepare_thread();

    for block in 0..1_000 {
        if block == 400 {
            let eq = ids[2];
            g.set_param(eq, "band1_gain_db", 9.0).unwrap();
            handle.set_param(&g, eq);
            let extra = g.add_node(NodeSpec::new(NodeKind::Mixer { inputs: 2 }));
            link(&mut g, ids[0], extra);
            handle.install(&g).unwrap();
        }
        if block % 100 == 0 {
            handle.pump();
            handle.meters();
        }
        ARMED.with(|a| a.set(true));
        engine.process(&mut io, BLOCK);
        ARMED.with(|a| a.set(false));
    }
    assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
}

fn render(swap_at: Option<usize>, blocks: usize) -> Vec<f32> {
    let (mut g, _) = chain(&[NodeKind::Eq]);
    let eq = NodeId(2);
    g.set_param(eq, "band2_gain_db", 12.0).unwrap();
    let (mut handle, mut engine) = engine(config(), None);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(blocks * BLOCK);
    for block in 0..blocks {
        if Some(block) == swap_at {
            g.add_node(NodeSpec::new(NodeKind::Gain));
            handle.install(&g).unwrap();
        }
        engine.process(&mut io, BLOCK);
    }
    io.recorded
}

#[test]
fn plan_swap_keeps_filter_state() {
    let reference = render(None, 40);
    let swapped = render(Some(20), 40);
    let after_fade = 22 * BLOCK;
    assert_eq!(&reference[after_fade..], &swapped[after_fade..]);
    let dip = &swapped[20 * BLOCK..21 * BLOCK];
    assert!(
        dip.last().unwrap().abs() < 0.05,
        "old plan fades out before the swap"
    );
}

#[test]
fn param_change_applies_without_recompile() {
    let (mut g, ids) = chain(&[NodeKind::Gain]);
    let (mut handle, mut engine) = engine(config(), None);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(200 * BLOCK);
    for _ in 0..50 {
        engine.process(&mut io, BLOCK);
    }
    g.set_param(ids[1], "gain_db", -6.0206).unwrap();
    handle.set_param(&g, ids[1]);
    for _ in 0..150 {
        engine.process(&mut io, BLOCK);
    }
    let before = io.recorded[40 * BLOCK..50 * BLOCK]
        .iter()
        .fold(0f32, |m, s| m.max(s.abs()));
    let after = io.recorded[190 * BLOCK..]
        .iter()
        .fold(0f32, |m, s| m.max(s.abs()));
    assert!(
        (after / before - 0.5).abs() < 0.01,
        "ratio {}",
        after / before
    );
}

#[test]
fn rejected_graph_keeps_running_plan() {
    let (g, _) = chain(&[NodeKind::Gain]);
    let (mut handle, mut engine) = engine(config(), None);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(4 * BLOCK);
    engine.process(&mut io, BLOCK);
    let mut broken = g.clone();
    broken.edges.insert(Edge {
        from: PortRef::new(NodeId(2), 0),
        to: PortRef::new(NodeId(2), 0),
    });
    assert!(handle.install(&broken).is_err());
    engine.process(&mut io, BLOCK);
    engine.process(&mut io, BLOCK);
    assert!(
        io.recorded[2 * BLOCK..3 * BLOCK]
            .iter()
            .any(|s| s.abs() > 0.1)
    );
}

#[test]
fn oversized_callbacks_are_split_into_blocks() {
    let (g, _) = chain(&[NodeKind::Gain]);
    let (mut handle, mut engine) = engine(config(), None);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(1_000);
    engine.process(&mut io, 1_000);
    assert_eq!(io.cursor, 1_000);
}

#[test]
fn meters_report_signal_only_when_enabled() {
    let (g, ids) = chain(&[NodeKind::Meter]);
    let (mut handle, mut engine) = engine(config(), None);
    handle.install(&g).unwrap();
    let mut io = SineIo::new(0);
    engine.process(&mut io, BLOCK);
    assert!(handle.meters().iter().all(|m| m.peak == 0.0));
    handle.set_metering(true);
    engine.process(&mut io, BLOCK);
    let meter = handle
        .meters()
        .into_iter()
        .find(|m| m.node == ids[1])
        .unwrap();
    assert!((meter.peak - 0.5).abs() < 0.01);
}
