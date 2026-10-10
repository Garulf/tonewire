//! Measures DSP cost of a typical voice chain:
//! `cargo run --release -p nodeaudio-graph --example bench`

use std::time::Instant;

use nodeaudio_graph::{
    AudioBuf, Edge, EndpointIo, EngineConfig, Graph, NodeId, NodeKind, NodeSpec, PortRef, engine,
};

const SAMPLE_RATE: f32 = 48_000.0;
const BLOCK: usize = 128;
const SECONDS: usize = 20;

struct Voice {
    phase: f32,
}

impl EndpointIo for Voice {
    fn read(&mut self, _: NodeId, dest: &mut AudioBuf, frames: usize) -> bool {
        for s in dest.channel_mut(0, frames) {
            *s = 0.3 * self.phase.sin() + 0.05 * (self.phase * 17.0).sin();
            self.phase =
                (self.phase + std::f32::consts::TAU * 180.0 / SAMPLE_RATE) % std::f32::consts::TAU;
        }
        true
    }

    fn write(&mut self, _: NodeId, src: &AudioBuf, frames: usize) {
        std::hint::black_box(src.channel(0, frames));
    }
}

fn link(g: &mut Graph, from: NodeId, to: NodeId) {
    g.connect(Edge {
        from: PortRef::new(from, 0),
        to: PortRef::new(to, 0),
    })
    .expect("valid edge");
}

fn main() {
    let mut g = Graph::default();
    let mut mic = NodeSpec::new(NodeKind::DeviceInput);
    mic.channels = 1;
    let mut previous = g.add_node(mic);
    for kind in [
        NodeKind::Gate,
        NodeKind::Eq,
        NodeKind::DeEsser,
        NodeKind::Compressor,
        NodeKind::Denoise,
        NodeKind::Limiter,
    ] {
        let id = g.add_node(NodeSpec::new(kind));
        link(&mut g, previous, id);
        previous = id;
    }
    let eq = NodeId(3);
    g.set_param(eq, "band1_gain_db", 6.0).expect("eq param");
    let speakers = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
    let virtual_mic = g.add_node(NodeSpec::new(NodeKind::VirtualMic));
    link(&mut g, previous, speakers);
    link(&mut g, previous, virtual_mic);

    let (mut handle, mut engine) = engine(
        EngineConfig {
            sample_rate: SAMPLE_RATE,
            max_block: BLOCK,
        },
        None,
    );
    handle.install(&g).expect("graph compiles");
    engine.prepare_thread();
    let mut io = Voice { phase: 0.0 };

    let blocks = SECONDS * SAMPLE_RATE as usize / BLOCK;
    let started = Instant::now();
    for _ in 0..blocks {
        engine.process(&mut io, BLOCK);
    }
    let elapsed = started.elapsed();
    let per_block = elapsed.as_secs_f64() / blocks as f64;
    let budget = BLOCK as f64 / f64::from(SAMPLE_RATE);
    println!(
        "{} nodes, {SECONDS} s of audio in {:.1} ms: {:.2} us per {BLOCK}-frame block, {:.3}% of one core",
        g.nodes.len(),
        elapsed.as_secs_f64() * 1e3,
        per_block * 1e6,
        per_block / budget * 100.0
    );
}
