use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use nodeaudio_graph::{NodeProcessor, ProcessCtx};
use rtrb::{Consumer, Producer, RingBuffer};

/// Something that turns fixed-size mono frames into processed frames on a
/// worker thread (an NVIDIA effect, or a stand-in in tests).
pub trait FrameEffect: Send + 'static {
    fn frame_len(&self) -> usize;
    fn set_intensity(&mut self, intensity: f32);
    fn run(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String>;
}

/// Audio-thread side of a worker-hosted effect. Audio goes to the worker
/// through one ring and comes back through another that starts prefilled
/// with `latency` frames of silence, so the delay is constant. When the
/// worker falls behind, the dry signal (delayed to match) fills in.
pub struct WorkerNode {
    to_worker: Producer<f32>,
    from_worker: Consumer<f32>,
    dry: nodeaudio_graph::buffer::DelayLine,
    mono: Vec<f32>,
    debt: usize,
    latency: usize,
    intensity: Arc<AtomicU32>,
    worker: Thread,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub fallbacks: Arc<AtomicU32>,
}

impl WorkerNode {
    pub fn spawn(mut effect: Box<dyn FrameEffect>, max_block: usize) -> Result<Self, String> {
        let frame = effect.frame_len().max(1);
        let latency = 2 * frame;
        let capacity = latency + 4 * frame + 2 * max_block;
        let (to_worker, mut worker_in) = RingBuffer::<f32>::new(capacity);
        let (mut worker_out, from_worker) = RingBuffer::<f32>::new(capacity);
        if let Ok(chunk) = worker_out.write_chunk_uninit(latency) {
            chunk.fill_from_iter(std::iter::repeat(0.0));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let intensity = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let (thread_stop, thread_intensity) = (stop.clone(), intensity.clone());
        let join = std::thread::Builder::new()
            .name("nvafx-worker".into())
            .spawn(move || {
                let mut input = vec![0.0f32; frame];
                let mut output = vec![0.0f32; frame];
                let mut applied = f32::NAN;
                while !thread_stop.load(Ordering::Relaxed) {
                    let wanted = f32::from_bits(thread_intensity.load(Ordering::Relaxed));
                    if wanted != applied {
                        effect.set_intensity(wanted);
                        applied = wanted;
                    }
                    while worker_in.slots() >= frame && worker_out.slots() >= frame {
                        if let Ok(chunk) = worker_in.read_chunk(frame) {
                            let (a, b) = chunk.as_slices();
                            input[..a.len()].copy_from_slice(a);
                            input[a.len()..].copy_from_slice(b);
                            chunk.commit_all();
                        }
                        if let Err(e) = effect.run(&input, &mut output) {
                            log::warn!("NVIDIA effect failed, passing audio through: {e}");
                            output.copy_from_slice(&input);
                        }
                        if let Ok(chunk) = worker_out.write_chunk_uninit(frame) {
                            chunk.fill_from_iter(output.iter().copied());
                        }
                    }
                    std::thread::park_timeout(Duration::from_millis(5));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            to_worker,
            from_worker,
            dry: nodeaudio_graph::buffer::DelayLine::new(1, latency, max_block),
            mono: vec![0.0; max_block],
            debt: 0,
            latency,
            intensity,
            worker: join.thread().clone(),
            stop,
            join: Some(join),
            fallbacks: Arc::new(AtomicU32::new(0)),
        })
    }
}

impl Drop for WorkerNode {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.unpark();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl NodeProcessor for WorkerNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        let frames = ctx.frames;
        let intensity = if ctx.shared.bypassed() {
            0.0
        } else {
            ctx.shared.param(0)
        };
        self.intensity.store(intensity.to_bits(), Ordering::Relaxed);

        let input = &ctx.inputs[0];
        let mono = &mut self.mono[..frames];
        mono.fill(0.0);
        if !input.is_silent() {
            let scale = 1.0 / input.channel_count() as f32;
            for c in 0..input.channel_count() {
                for (m, &x) in mono.iter_mut().zip(input.channel(c, frames)) {
                    *m += x * scale;
                }
            }
        }
        if let Ok(chunk) = self
            .to_worker
            .write_chunk_uninit(frames.min(self.to_worker.slots()))
        {
            chunk.fill_from_iter(mono.iter().copied());
        }
        self.worker.unpark();

        let dry = self.dry.scratch_mut();
        dry.channel_mut(0, frames).copy_from_slice(mono);
        let dry = self.dry.process_scratch(frames);
        let dry: &[f32] = dry.channel(0, frames);

        let skip = self.debt.min(self.from_worker.slots());
        if skip > 0
            && let Ok(chunk) = self.from_worker.read_chunk(skip)
        {
            chunk.commit_all();
            self.debt -= skip;
        }
        let output = &mut ctx.outputs[0];
        match self.from_worker.read_chunk(frames) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                let split = a.len();
                for c in 0..output.channel_count() {
                    for (i, s) in output.channel_mut(c, frames).iter_mut().enumerate() {
                        *s = if i < split { a[i] } else { b[i - split] };
                    }
                }
                chunk.commit_all();
            }
            Err(_) => {
                self.debt += frames;
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                for c in 0..output.channel_count() {
                    output.channel_mut(c, frames).copy_from_slice(dry);
                }
            }
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodeaudio_graph::{
        AudioBuf, Edge, EngineConfig, Graph, NodeId, NodeKind, NodeSpec, NvAfxEffect, PortRef,
        engine,
    };

    struct HalfGain;

    impl FrameEffect for HalfGain {
        fn frame_len(&self) -> usize {
            480
        }
        fn set_intensity(&mut self, _: f32) {}
        fn run(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String> {
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * 0.5;
            }
            Ok(())
        }
    }

    struct Factory;

    impl nodeaudio_graph::ExternalFactory for Factory {
        fn create(
            &self,
            _: &NodeKind,
            _: usize,
            _: f32,
            max_block: usize,
        ) -> Result<Box<dyn NodeProcessor>, String> {
            Ok(Box::new(WorkerNode::spawn(Box::new(HalfGain), max_block)?))
        }
    }

    struct Impulse {
        sent: bool,
        out: Vec<f32>,
    }

    impl nodeaudio_graph::EndpointIo for Impulse {
        fn read(&mut self, _: NodeId, dest: &mut AudioBuf, frames: usize) -> bool {
            let samples = dest.channel_mut(0, frames);
            samples.fill(0.0);
            if !self.sent {
                samples[0] = 1.0;
                self.sent = true;
            }
            true
        }
        fn write(&mut self, _: NodeId, src: &AudioBuf, frames: usize) {
            self.out.extend_from_slice(src.channel(0, frames));
        }
    }

    #[test]
    fn worker_effect_has_constant_reported_latency() {
        let mut g = Graph::default();
        let mut mic = NodeSpec::new(NodeKind::DeviceInput);
        mic.channels = 1;
        let mic = g.add_node(mic);
        let nv = g.add_node(NodeSpec::new(NodeKind::NvAfx {
            effect: NvAfxEffect::Denoiser,
        }));
        let mut out = NodeSpec::new(NodeKind::DeviceOutput);
        out.channels = 1;
        let out = g.add_node(out);
        for (a, b) in [(mic, nv), (nv, out)] {
            g.connect(Edge {
                from: PortRef::new(a, 0),
                to: PortRef::new(b, 0),
            })
            .unwrap();
        }
        let config = EngineConfig {
            sample_rate: 48_000.0,
            max_block: 128,
        };
        let (mut handle, mut engine) = engine(config, Some(Arc::new(Factory)));
        let report = handle.install(&g).unwrap();
        assert!(report.warnings.is_empty());
        assert_eq!(report.latency[&nv], 960);

        let mut io = Impulse {
            sent: false,
            out: Vec::new(),
        };
        for _ in 0..40 {
            engine.process(&mut io, 128);
            std::thread::sleep(Duration::from_millis(1));
        }
        let peak = io.out.iter().position(|&s| s.abs() > 0.1).unwrap();
        assert_eq!(peak, 960);
        assert!((io.out[peak] - 0.5).abs() < 1e-6);
    }
}
