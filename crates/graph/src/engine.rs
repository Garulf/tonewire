use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::model::{Graph, GraphError, NodeId};
use crate::nodes::ExternalFactory;
use crate::plan::{self, BuildReport, CompileCtx, Plan, StepSig};
use crate::processor::EndpointIo;
use crate::shared::{AtomicF32, NodeShared};

const PLAN_QUEUE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    pub sample_rate: f32,
    pub max_block: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            max_block: 1024,
        }
    }
}

#[derive(Debug, Default)]
pub struct EngineStats {
    load: AtomicF32,
    blocks: AtomicU64,
    last_frames: AtomicU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StatsSnapshot {
    /// Peak fraction of real time spent processing since the last snapshot.
    pub dsp_load: f32,
    pub blocks: u64,
    pub last_block_frames: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeMeter {
    pub node: NodeId,
    pub peak: f32,
    pub rms: f32,
    pub reduction_db: f32,
}

pub fn engine(
    config: EngineConfig,
    external: Option<Arc<dyn ExternalFactory>>,
) -> (EngineHandle, Engine) {
    let (plan_tx, plan_rx) = RingBuffer::new(PLAN_QUEUE);
    let (garbage_tx, garbage_rx) = RingBuffer::new(PLAN_QUEUE * 2);
    let stats = Arc::new(EngineStats::default());
    let metering = Arc::new(AtomicBool::new(false));
    let handle = EngineHandle {
        config,
        external,
        plan_tx,
        garbage_rx,
        shared: HashMap::new(),
        layout: Vec::new(),
        backlog: None,
        stats: stats.clone(),
        metering: metering.clone(),
    };
    let engine = Engine {
        config,
        current: None,
        incoming: None,
        plan_rx,
        garbage_tx,
        fade_in: false,
        stats,
        metering,
    };
    (handle, engine)
}

/// Control-thread side: compiles graphs, owns parameter storage, reads meters.
pub struct EngineHandle {
    config: EngineConfig,
    external: Option<Arc<dyn ExternalFactory>>,
    plan_tx: Producer<Box<Plan>>,
    garbage_rx: Consumer<Box<Plan>>,
    shared: HashMap<NodeId, Arc<NodeShared>>,
    layout: Vec<StepSig>,
    backlog: Option<(Box<Plan>, Vec<StepSig>)>,
    stats: Arc<EngineStats>,
    metering: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// Compiles `graph` and queues it for the audio thread. On error the
    /// running plan is left untouched.
    pub fn install(&mut self, graph: &Graph) -> Result<BuildReport, GraphError> {
        self.collect_garbage();
        self.shared.retain(|id, _| graph.nodes.contains_key(id));
        for (&id, spec) in &graph.nodes {
            let expected = crate::catalog::describe(&spec.kind).params.len();
            match self.shared.get(&id) {
                Some(s) if s.param_count() == expected => s.load_from(spec),
                _ => {
                    self.shared.insert(id, Arc::new(NodeShared::new(spec)));
                }
            }
        }
        let ctx = CompileCtx {
            sample_rate: self.config.sample_rate,
            max_block: self.config.max_block,
            external: self.external.as_deref(),
        };
        let (plan, report) = plan::build(graph, &ctx, &self.shared, &self.layout)?;
        self.backlog = Some((plan, report.layout.clone()));
        self.flush_backlog();
        Ok(report)
    }

    /// Frees retired plans and pushes a queued plan once the audio thread has
    /// room. Call periodically from the control thread.
    pub fn pump(&mut self) {
        self.collect_garbage();
        self.flush_backlog();
    }

    fn flush_backlog(&mut self) {
        let Some((plan, layout)) = self.backlog.take() else {
            return;
        };
        match self.plan_tx.push(plan) {
            Ok(()) => self.layout = layout,
            Err(rtrb::PushError::Full(plan)) => self.backlog = Some((plan, layout)),
        }
    }

    pub fn has_backlog(&self) -> bool {
        self.backlog.is_some()
    }

    pub fn collect_garbage(&mut self) {
        while let Ok(old) = self.garbage_rx.pop() {
            drop(old);
        }
    }

    pub fn node_shared(&self, node: NodeId) -> Option<&Arc<NodeShared>> {
        self.shared.get(&node)
    }

    /// Applies a param change without recompiling.
    pub fn set_param(&self, graph: &Graph, node: NodeId) {
        if let (Some(shared), Some(spec)) = (self.shared.get(&node), graph.nodes.get(&node)) {
            shared.load_from(spec);
        }
    }

    pub fn set_metering(&self, on: bool) {
        self.metering.store(on, Ordering::Relaxed);
    }

    pub fn meters(&self) -> Vec<NodeMeter> {
        let mut meters: Vec<NodeMeter> = self
            .shared
            .iter()
            .map(|(&node, shared)| {
                let (peak, rms, reduction_db) = shared.meter.take();
                NodeMeter {
                    node,
                    peak,
                    rms,
                    reduction_db,
                }
            })
            .collect();
        meters.sort_by_key(|m| m.node);
        meters
    }

    pub fn stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            dsp_load: self.stats.load.swap(0.0),
            blocks: self.stats.blocks.load(Ordering::Relaxed),
            last_block_frames: self.stats.last_frames.load(Ordering::Relaxed),
        }
    }
}

/// Audio-thread side. `process` never allocates, locks or frees.
pub struct Engine {
    config: EngineConfig,
    current: Option<Box<Plan>>,
    incoming: Option<Box<Plan>>,
    plan_rx: Consumer<Box<Plan>>,
    garbage_tx: Producer<Box<Plan>>,
    fade_in: bool,
    stats: Arc<EngineStats>,
    metering: Arc<AtomicBool>,
}

impl Engine {
    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// Fills per-thread caches some DSP code allocates on first use. Backends
    /// call this once on the thread that will run [`Engine::process`].
    pub fn prepare_thread(&self) {
        nodeaudio_dsp::denoise::warm_up_thread();
    }

    /// Runs the graph for `frames` frames, in chunks of at most `max_block`.
    pub fn process(&mut self, io: &mut dyn EndpointIo, frames: usize) {
        let started = Instant::now();
        let mut remaining = frames;
        while remaining > 0 {
            let chunk = remaining.min(self.config.max_block);
            self.process_chunk(io, chunk);
            remaining -= chunk;
        }
        let budget = frames as f32 / self.config.sample_rate;
        if budget > 0.0 {
            self.stats
                .load
                .fetch_max_positive(started.elapsed().as_secs_f32() / budget);
        }
        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        self.stats
            .last_frames
            .store(frames as u32, Ordering::Relaxed);
    }

    fn process_chunk(&mut self, io: &mut dyn EndpointIo, frames: usize) {
        if self.incoming.is_none() && self.garbage_tx.slots() > 0 {
            self.incoming = self.plan_rx.pop().ok();
        }
        if self.current.is_none() && self.incoming.is_some() {
            self.current = self.incoming.take();
            self.fade_in = true;
        }
        let metering = self.metering.load(Ordering::Relaxed);
        let fade = match (self.incoming.is_some(), self.fade_in) {
            (true, _) => (1.0, 0.0),
            (false, true) => (0.0, 1.0),
            (false, false) => (1.0, 1.0),
        };
        self.fade_in = false;
        if let Some(plan) = self.current.as_mut() {
            plan.process(io, frames, fade, metering);
        }
        if let Some(mut next) = self.incoming.take() {
            if let Some(mut old) = self.current.take() {
                next.adopt_state(&mut old);
                if let Err(rtrb::PushError::Full(old)) = self.garbage_tx.push(old) {
                    std::mem::forget(old);
                }
            }
            self.current = Some(next);
            self.fade_in = true;
        }
    }
}
