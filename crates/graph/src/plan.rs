use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::buffer::{AudioBuf, DelayLine};
use crate::catalog;
use crate::model::{Graph, GraphError, NodeId, NodeKind};
use crate::nodes::{self, ExternalFactory, PassThrough};
use crate::processor::{EndpointIo, NodeProcessor, ProcessCtx};
use crate::shared::NodeShared;

pub struct CompileCtx<'a> {
    pub sample_rate: f32,
    pub max_block: usize,
    pub external: Option<&'a dyn ExternalFactory>,
}

/// Identity of a step for carrying processor state into the next plan.
#[derive(Debug, Clone, PartialEq)]
pub struct StepSig {
    pub node: NodeId,
    pub kind: NodeKind,
    pub channels: u8,
}

struct Source {
    step: usize,
    port: usize,
    delay: Option<DelayLine>,
}

struct Step {
    node: NodeId,
    processor: Box<dyn NodeProcessor>,
    shared: Arc<NodeShared>,
    sources: Vec<Vec<Source>>,
    inputs: Vec<AudioBuf>,
    outputs: Vec<AudioBuf>,
    is_source: bool,
    is_sink: bool,
    simple_effect: bool,
}

pub struct Plan {
    steps: Vec<Step>,
    /// `(new step, old step)` pairs whose processors move into this plan.
    carry: Vec<(usize, usize)>,
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub warnings: Vec<(NodeId, String)>,
    /// Processing latency in frames at each node's output (sinks: at input).
    pub latency: BTreeMap<NodeId, usize>,
    pub layout: Vec<StepSig>,
}

pub fn build(
    graph: &Graph,
    ctx: &CompileCtx<'_>,
    shared: &HashMap<NodeId, Arc<NodeShared>>,
    previous: &[StepSig],
) -> Result<(Box<Plan>, BuildReport), GraphError> {
    graph.validate()?;
    let order = graph.topological_order()?;
    let index: HashMap<NodeId, usize> = order.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let mut report = BuildReport::default();
    let mut steps: Vec<Step> = Vec::with_capacity(order.len());
    let mut output_latency: Vec<usize> = Vec::with_capacity(order.len());

    for &node in &order {
        let spec = &graph.nodes[&node];
        let info = catalog::describe(&spec.kind);
        let channels = usize::from(spec.channels);
        let processor = nodes::create(
            &spec.kind,
            channels,
            ctx.sample_rate,
            ctx.max_block,
            ctx.external,
        )
        .unwrap_or_else(|err| {
            report.warnings.push((node, err));
            Box::new(PassThrough) as Box<dyn NodeProcessor>
        });

        let mut sources: Vec<Vec<Source>> = (0..info.inputs).map(|_| Vec::new()).collect();
        let mut arrival = 0;
        for edge in graph.edges.iter().filter(|e| e.to.node == node) {
            let step = index[&edge.from.node];
            arrival = arrival.max(output_latency[step]);
            sources[usize::from(edge.to.port)].push(Source {
                step,
                port: usize::from(edge.from.port),
                delay: None,
            });
        }
        for source in sources.iter_mut().flatten() {
            let lag = arrival - output_latency[source.step];
            if lag > 0 {
                let src_channels = steps[source.step].outputs[source.port].channel_count();
                source.delay = Some(DelayLine::new(src_channels, lag, ctx.max_block));
            }
        }

        let latency = arrival + processor.latency();
        output_latency.push(latency);
        report.latency.insert(node, latency);
        report.layout.push(StepSig {
            node,
            kind: spec.kind.clone(),
            channels: spec.channels,
        });
        steps.push(Step {
            node,
            simple_effect: info.inputs == 1 && info.outputs == 1 && processor.latency() == 0,
            processor,
            shared: shared
                .get(&node)
                .cloned()
                .unwrap_or_else(|| Arc::new(NodeShared::new(spec))),
            inputs: (0..info.inputs)
                .map(|_| AudioBuf::new(channels, ctx.max_block))
                .collect(),
            outputs: (0..info.outputs)
                .map(|_| AudioBuf::new(channels, ctx.max_block))
                .collect(),
            is_source: info.inputs == 0,
            is_sink: info.outputs == 0,
            sources,
        });
    }

    let carry = report
        .layout
        .iter()
        .enumerate()
        .filter_map(|(new, sig)| {
            previous
                .iter()
                .position(|old| old == sig)
                .map(|old| (new, old))
        })
        .collect();
    Ok((Box::new(Plan { steps, carry }), report))
}

impl Plan {
    /// Moves processors (and their filter state) for unchanged nodes out of
    /// `old` into `self`. Allocation-free; runs on the audio thread.
    pub fn adopt_state(&mut self, old: &mut Plan) {
        for &(new, prev) in &self.carry {
            if let (Some(n), Some(o)) = (self.steps.get_mut(new), old.steps.get_mut(prev)) {
                std::mem::swap(&mut n.processor, &mut o.processor);
            }
        }
    }

    pub fn process(
        &mut self,
        io: &mut dyn EndpointIo,
        frames: usize,
        fade: (f32, f32),
        metering: bool,
    ) {
        for k in 0..self.steps.len() {
            let (done, rest) = self.steps.split_at_mut(k);
            let step = &mut rest[0];
            gather(step, done, frames);

            let all_silent = !step.is_source && step.inputs.iter().all(AudioBuf::is_silent);
            if step.simple_effect && step.shared.bypassed() {
                step.pass_through(frames);
            } else if all_silent && !step.is_sink && step.processor.skips_silence() {
                step.outputs.iter_mut().for_each(|o| o.clear(frames));
            } else {
                let mut ctx = ProcessCtx {
                    node: step.node,
                    frames,
                    inputs: &mut step.inputs,
                    outputs: &mut step.outputs,
                    shared: &step.shared,
                    io,
                    fade,
                };
                step.processor.process(&mut ctx);
            }

            if metering {
                let metered = step.outputs.first().or(step.inputs.first());
                if let Some(buf) = metered {
                    let (peak, rms) = buf.peak_rms(frames);
                    step.shared.meter.record(peak, rms);
                }
            }
        }
    }
}

impl Step {
    fn pass_through(&mut self, frames: usize) {
        let (input, output) = (&mut self.inputs[0], &mut self.outputs[0]);
        if input.channel_count() == output.channel_count() {
            std::mem::swap(input, output);
        } else {
            output.mix_from(input, frames, 1.0, false);
        }
    }
}

fn gather(step: &mut Step, done: &mut [Step], frames: usize) {
    for (input, sources) in step.inputs.iter_mut().zip(&mut step.sources) {
        input.clear(frames);
        for source in sources.iter_mut() {
            let produced = &done[source.step].outputs[source.port];
            match &mut source.delay {
                None => input.mix_from(produced, frames, 1.0, true),
                Some(delay) => {
                    delay.scratch_mut().mix_from(produced, frames, 1.0, false);
                    input.mix_from(delay.process_scratch(frames), frames, 1.0, true);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Edge, NodeSpec, PortRef};
    use crate::processor::NoEndpoints;

    pub(crate) const BLOCK: usize = 128;

    pub(crate) fn ctx() -> CompileCtx<'static> {
        CompileCtx {
            sample_rate: 48_000.0,
            max_block: BLOCK,
            external: None,
        }
    }

    /// Endpoints backed by plain vectors: a constant input and captured outputs.
    #[derive(Default)]
    pub(crate) struct TestIo {
        pub input: HashMap<NodeId, f32>,
        pub output: HashMap<NodeId, Vec<Vec<f32>>>,
    }

    impl EndpointIo for TestIo {
        fn read(&mut self, node: NodeId, dest: &mut AudioBuf, frames: usize) -> bool {
            let Some(&value) = self.input.get(&node) else {
                return false;
            };
            for c in 0..dest.channel_count() {
                dest.channel_mut(c, frames).fill(value);
            }
            true
        }

        fn write(&mut self, node: NodeId, src: &AudioBuf, frames: usize) {
            let out = self.output.entry(node).or_default();
            out.resize(src.channel_count(), Vec::new());
            for (c, channel) in out.iter_mut().enumerate() {
                channel.extend_from_slice(src.channel(c, frames));
            }
        }
    }

    pub(crate) fn connect(g: &mut Graph, from: NodeId, to: NodeId, to_port: u8) {
        g.connect(Edge {
            from: PortRef::new(from, 0),
            to: PortRef::new(to, to_port),
        })
        .unwrap();
    }

    fn run(graph: &Graph, io: &mut TestIo, blocks: usize) -> BuildReport {
        let (mut plan, report) = build(graph, &ctx(), &HashMap::new(), &[]).unwrap();
        for _ in 0..blocks {
            plan.process(io, BLOCK, (1.0, 1.0), false);
        }
        report
    }

    #[test]
    fn two_sources_into_one_input_are_summed() {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let b = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let out = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        connect(&mut g, a, out, 0);
        connect(&mut g, b, out, 0);
        let mut io = TestIo::default();
        io.input.insert(a, 0.25);
        io.input.insert(b, 0.5);
        run(&g, &mut io, 2);
        assert!(io.output[&out][0].iter().all(|&s| (s - 0.75).abs() < 1e-6));
    }

    #[test]
    fn one_output_feeds_two_destinations() {
        let mut g = Graph::default();
        let mic = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let speakers = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        let virtual_mic = g.add_node(NodeSpec::new(NodeKind::VirtualMic));
        connect(&mut g, mic, speakers, 0);
        connect(&mut g, mic, virtual_mic, 0);
        let mut io = TestIo::default();
        io.input.insert(mic, 0.5);
        run(&g, &mut io, 1);
        assert_eq!(io.output[&speakers][1][0], 0.5);
        assert_eq!(io.output[&virtual_mic][0][BLOCK - 1], 0.5);
    }

    #[test]
    fn mono_source_reaches_both_stereo_channels() {
        let mut g = Graph::default();
        let mut mono = NodeSpec::new(NodeKind::DeviceInput);
        mono.channels = 1;
        let mic = g.add_node(mono);
        let out = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        connect(&mut g, mic, out, 0);
        let mut io = TestIo::default();
        io.input.insert(mic, 0.3);
        run(&g, &mut io, 1);
        assert_eq!(io.output[&out].len(), 2);
        assert_eq!(io.output[&out][1][0], 0.3);
    }

    #[test]
    fn latent_branch_is_aligned_with_dry_branch() {
        let mut g = Graph::default();
        let mic = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let denoise = g.add_node(NodeSpec::new(NodeKind::Denoise));
        let mixer = g.add_node(NodeSpec::new(NodeKind::Mixer { inputs: 2 }));
        let out = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        connect(&mut g, mic, denoise, 0);
        connect(&mut g, denoise, mixer, 0);
        connect(&mut g, mic, mixer, 1);
        connect(&mut g, mixer, out, 0);
        let mut io = TestIo::default();
        io.input.insert(mic, 0.1);
        let report = run(&g, &mut io, 2);
        assert_eq!(report.latency[&mixer], 480);
        let dry_arrival = io.output[&out][0].iter().position(|&s| s != 0.0);
        assert!(
            dry_arrival.is_none(),
            "dry path must be delayed past the first 256 frames"
        );
    }

    #[test]
    fn unchanged_nodes_carry_their_processors() {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::Eq));
        let b = g.add_node(NodeSpec::new(NodeKind::Gain));
        let (_, first) = build(&g, &ctx(), &HashMap::new(), &[]).unwrap();
        g.remove_node(b);
        let c = g.add_node(NodeSpec::new(NodeKind::Compressor));
        let (plan, _) = build(&g, &ctx(), &HashMap::new(), &first.layout).unwrap();
        assert_eq!(plan.carry.len(), 1);
        assert_eq!(plan.steps[plan.carry[0].0].node, a);
        assert!(plan.steps.iter().any(|s| s.node == c));
    }

    #[test]
    fn missing_external_factory_degrades_to_passthrough_with_warning() {
        let mut g = Graph::default();
        let nv = g.add_node(NodeSpec::new(NodeKind::NvAfx {
            effect: crate::model::NvAfxEffect::Denoiser,
        }));
        let (_, report) = build(&g, &ctx(), &HashMap::new(), &[]).unwrap();
        assert_eq!(report.warnings[0].0, nv);
    }

    #[test]
    fn graph_without_endpoints_runs_silently() {
        let mut g = Graph::default();
        let a = g.add_node(NodeSpec::new(NodeKind::Eq));
        let b = g.add_node(NodeSpec::new(NodeKind::Compressor));
        connect(&mut g, a, b, 0);
        let (mut plan, _) = build(&g, &ctx(), &HashMap::new(), &[]).unwrap();
        plan.process(&mut NoEndpoints, BLOCK, (1.0, 1.0), true);
    }
}
