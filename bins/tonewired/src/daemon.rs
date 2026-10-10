use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use tonewire_graph::catalog::{self, Category};
use tonewire_graph::format::{from_toml, to_toml};
use tonewire_graph::{
    Edge, EngineConfig, EngineHandle, ExternalFactory, Graph, GraphError, NodeId, NodeKind,
    NodeSpec, engine,
};
use tonewire_io::{Router, StreamConfig};
use tonewire_proto::{
    ClientMsg, Command, DaemonState, EngineStats, Event, GraphState, KindAvailability,
    MeterReading, PROTOCOL_VERSION, Reply, ServerMsg,
};

use crate::backend::{self, BackendChoice};
use crate::history::History;
use crate::persist::{Settings, Store};
use crate::server::{self, Client, ClientId};

const TICK: Duration = Duration::from_millis(33);
const STATS_EVERY: Duration = Duration::from_secs(1);
const AUTOSAVE_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    OpenEditor,
    ToggleMute,
    Quit,
}

pub enum Inbound {
    Connected(ClientId, Client),
    Message(ClientId, ClientMsg),
    Disconnected(ClientId),
    Tray(TrayAction),
    Terminate,
}

pub struct Options {
    pub backend: BackendChoice,
    pub socket: String,
    pub store: Store,
}

/// What the tray shows; refreshed by the daemon loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrayStatus {
    pub muted: bool,
    pub line: String,
}

pub trait TrayView: Send {
    fn update(&mut self, status: &TrayStatus);
}

struct Audio {
    engine: EngineHandle,
    router: Router,
    latency_frames: usize,
}

pub struct Daemon {
    options: Options,
    settings: Settings,
    graph: Graph,
    history: History,
    external: Option<Arc<dyn ExternalFactory>>,
    audio: Audio,
    clients: BTreeMap<ClientId, Client>,
    greeted: BTreeSet<ClientId>,
    inbox: Receiver<Inbound>,
    inbox_tx: Sender<Inbound>,
    dirty_since: Option<Instant>,
    last_stats: Instant,
    warnings: Vec<(NodeId, String)>,
    tray: Option<Box<dyn TrayView>>,
    tray_status: TrayStatus,
}

fn structure(graph: &Graph) -> (Vec<(NodeId, &NodeKind, u8)>, &BTreeSet<Edge>) {
    let nodes = graph
        .nodes
        .iter()
        .map(|(&id, spec)| (id, &spec.kind, spec.channels))
        .collect();
    (nodes, &graph.edges)
}

impl Daemon {
    pub fn new(
        options: Options,
        external: Option<Arc<dyn ExternalFactory>>,
    ) -> Result<Self, String> {
        let settings = options.store.load_settings();
        let graph = options.store.load_graph();
        let audio = start_audio(&options.backend, &settings, external.clone())?;
        let (inbox_tx, inbox) = unbounded();
        let mut daemon = Self {
            options,
            settings,
            graph,
            history: History::default(),
            external,
            audio,
            clients: BTreeMap::new(),
            greeted: BTreeSet::new(),
            inbox,
            inbox_tx,
            dirty_since: None,
            last_stats: Instant::now(),
            warnings: Vec::new(),
            tray: None,
            tray_status: TrayStatus::default(),
        };
        daemon.install();
        daemon.audio.router.apply(daemon.graph.endpoints());
        Ok(daemon)
    }

    pub fn sender(&self) -> Sender<Inbound> {
        self.inbox_tx.clone()
    }

    pub fn set_tray(&mut self, tray: Box<dyn TrayView>) {
        self.tray = Some(tray);
        self.refresh_tray();
    }

    pub fn serve(&self) -> std::io::Result<()> {
        server::serve(&self.options.socket, self.inbox_tx.clone())
    }

    pub fn run(mut self) {
        log::info!(
            "tonewired running ({} backend, socket {})",
            self.audio.router.backend_name(),
            self.options.socket
        );
        loop {
            let deadline = Instant::now() + TICK;
            while let Ok(message) = self.inbox.recv_deadline(deadline) {
                if !self.handle(message) {
                    self.save_now();
                    self.flush_clients();
                    log::info!("tonewired stopped");
                    return;
                }
            }
            self.tick();
        }
    }

    fn handle(&mut self, message: Inbound) -> bool {
        match message {
            Inbound::Connected(id, client) => {
                self.clients.insert(id, client);
                self.audio.engine.set_metering(true);
            }
            Inbound::Disconnected(id) => self.drop_client(id),
            Inbound::Message(id, ClientMsg::Hello { version }) => {
                if version != PROTOCOL_VERSION {
                    log::warn!(
                        "client {id} speaks protocol {version}, expected {PROTOCOL_VERSION}"
                    );
                    self.drop_client(id);
                } else {
                    self.greeted.insert(id);
                    self.send_to(id, ServerMsg::Event(Event::State(Box::new(self.state()))));
                }
            }
            Inbound::Message(
                id,
                ClientMsg::Request {
                    id: request,
                    command,
                },
            ) => {
                if !self.greeted.contains(&id) {
                    self.drop_client(id);
                    return true;
                }
                if command == Command::Shutdown {
                    self.send_to(
                        id,
                        ServerMsg::Reply {
                            id: request,
                            reply: Reply::Ok,
                        },
                    );
                    return false;
                }
                let reply = self.execute(command);
                self.send_to(id, ServerMsg::Reply { id: request, reply });
            }
            Inbound::Tray(TrayAction::Quit) | Inbound::Terminate => return false,
            Inbound::Tray(TrayAction::ToggleMute) => {
                self.audio.router.set_muted(!self.audio.router.is_muted());
                self.broadcast_graph();
                self.refresh_tray();
            }
            Inbound::Tray(TrayAction::OpenEditor) => crate::launch::editor(),
        }
        true
    }

    /// Gives writer threads a moment to deliver final replies before exit.
    fn flush_clients(&self) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && !self.clients.values().all(Client::is_drained) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fn drop_client(&mut self, id: ClientId) {
        self.clients.remove(&id);
        self.greeted.remove(&id);
        if self.clients.is_empty() {
            self.audio.engine.set_metering(false);
        }
    }

    fn execute(&mut self, command: Command) -> Reply {
        match self.apply(command) {
            Ok(reply) => reply,
            Err(message) => Reply::Error(message),
        }
    }

    fn apply(&mut self, command: Command) -> Result<Reply, String> {
        let before = self.graph.clone();
        let mut drag = None;
        let mut dragging = false;
        let mut graph = self.graph.clone();
        match command {
            Command::GetState => {
                self.broadcast(Event::State(Box::new(self.state())));
                return Ok(Reply::Ok);
            }
            Command::ExportGraph => return Ok(Reply::Exported(to_toml(&self.graph))),
            Command::Undo | Command::Redo => {
                let restored = if command == Command::Undo {
                    self.history.undo(&self.graph)
                } else {
                    self.history.redo(&self.graph)
                };
                let Some(restored) = restored else {
                    return Err("nothing to undo or redo".into());
                };
                self.commit(restored, &before);
                return Ok(Reply::Ok);
            }
            Command::SetMuteAll(muted) => {
                self.audio.router.set_muted(muted);
                self.broadcast_graph();
                self.refresh_tray();
                return Ok(Reply::Ok);
            }
            Command::SetEngineConfig(engine) => {
                self.settings.set_engine(engine);
                self.options
                    .store
                    .save_settings(&self.settings)
                    .map_err(|e| format!("cannot save settings: {e}"))?;
                self.restart_audio()?;
                self.broadcast(Event::State(Box::new(self.state())));
                return Ok(Reply::Ok);
            }
            Command::AddNode { kind, pos } => {
                let mut spec = NodeSpec::new(kind);
                spec.pos = pos;
                graph.add_node(spec);
            }
            Command::RemoveNode { node } => {
                graph
                    .remove_node(node)
                    .ok_or(GraphError::UnknownNode(node).to_string())?;
            }
            Command::MoveNode { node, pos } => graph.node_mut(node).map_err(err)?.pos = pos,
            Command::Connect { edge } => graph.connect(edge).map_err(err)?,
            Command::Disconnect { edge } => {
                graph.disconnect(edge);
            }
            Command::SetParam {
                node,
                key,
                value,
                dragging: d,
            } => {
                graph.set_param(node, &key, value).map_err(err)?;
                drag = Some((node, key));
                dragging = d;
            }
            Command::SetBypass { node, bypass } => {
                graph.node_mut(node).map_err(err)?.bypass = bypass
            }
            Command::SetChannels { node, channels } => {
                graph.set_channels(node, channels).map_err(err)?
            }
            Command::SetDevice { node, device } => {
                graph.node_mut(node).map_err(err)?.device = device
            }
            Command::RenameNode { node, name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("name cannot be empty".into());
                }
                graph.node_mut(node).map_err(err)?.name = name.chars().take(64).collect();
            }
            Command::SetApps { node, apps, mode } => {
                let spec = graph.node_mut(node).map_err(err)?;
                spec.apps = apps;
                spec.app_mode = mode;
            }
            Command::ImportGraph { toml } => graph = from_toml(&toml).map_err(|e| e.to_string())?,
            Command::Shutdown => unreachable!("handled by the caller"),
        }
        if graph == self.graph {
            if let Some(key) = &drag
                && !dragging
            {
                self.history.close_drag(key);
            }
            return Ok(Reply::Ok);
        }
        self.history.record(&before, drag, dragging);
        self.commit(graph, &before);
        Ok(Reply::Ok)
    }

    /// Makes `graph` current, pushing only what changed to the engine and
    /// the router.
    fn commit(&mut self, graph: Graph, before: &Graph) {
        self.graph = graph;
        if structure(&self.graph) != structure(before) {
            self.install();
        } else {
            for (&id, spec) in &self.graph.nodes {
                if before
                    .nodes
                    .get(&id)
                    .is_none_or(|b| b.params != spec.params || b.bypass != spec.bypass)
                {
                    self.audio.engine.set_param(&self.graph, id);
                }
            }
        }
        let endpoints = self.graph.endpoints();
        if endpoints != before.endpoints() {
            self.audio.router.apply(endpoints);
        }
        self.dirty_since.get_or_insert_with(Instant::now);
        self.broadcast_graph();
    }

    fn install(&mut self) {
        match self.audio.engine.install(&self.graph) {
            Ok(report) => {
                self.audio.latency_frames = report
                    .latency
                    .iter()
                    .filter(|(id, _)| {
                        self.graph.nodes[*id]
                            .kind
                            .endpoint_kind()
                            .is_some_and(|k| !k.is_capture())
                    })
                    .map(|(_, &l)| l)
                    .max()
                    .unwrap_or(0);
                if report.warnings != self.warnings {
                    self.warnings = report.warnings;
                    self.broadcast(Event::Warnings(self.warnings.clone()));
                }
            }
            Err(e) => log::error!("graph rejected by the engine: {e}"),
        }
    }

    fn restart_audio(&mut self) -> Result<(), String> {
        self.audio.router.apply(Vec::new());
        let audio = start_audio(&self.options.backend, &self.settings, self.external.clone())?;
        let muted = self.audio.router.is_muted();
        self.audio = audio;
        self.audio.router.set_muted(muted);
        self.audio.engine.set_metering(!self.clients.is_empty());
        self.install();
        self.audio.router.apply(self.graph.endpoints());
        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.audio.engine.pump();
        let changes = self.audio.router.tick(now);
        if changes.devices {
            self.broadcast(Event::DevicesChanged(self.audio.router.devices().to_vec()));
        }
        if changes.apps {
            self.broadcast(Event::AppsChanged(self.audio.router.apps().to_vec()));
        }
        if changes.status {
            self.broadcast(Event::NodeStatus(self.audio.router.status()));
        }
        if !self.clients.is_empty() {
            let meters = self
                .audio
                .engine
                .meters()
                .into_iter()
                .map(|m| MeterReading {
                    node: m.node,
                    peak: m.peak,
                    rms: m.rms,
                    reduction_db: m.reduction_db,
                })
                .collect();
            self.broadcast(Event::Meters(meters));
        }
        if now - self.last_stats >= STATS_EVERY {
            self.last_stats = now;
            let stats = self.stats();
            if !self.clients.is_empty() {
                self.broadcast(Event::Stats(stats));
            }
            self.refresh_tray();
        }
        if self.dirty_since.is_some_and(|t| now - t >= AUTOSAVE_AFTER) {
            self.save_now();
        }
    }

    fn save_now(&mut self) {
        if self.dirty_since.take().is_some()
            && let Err(e) = self.options.store.save_graph(&self.graph)
        {
            log::error!("cannot save graph: {e}");
        }
    }

    fn stats(&self) -> EngineStats {
        let snapshot = self.audio.engine.stats();
        let engine = self.settings.engine();
        let latency = self.audio.latency_frames + 2 * engine.block_frames as usize;
        EngineStats {
            dsp_load: snapshot.dsp_load,
            sample_rate: engine.sample_rate,
            block_frames: engine.block_frames,
            graph_latency_ms: latency as f32 * 1000.0 / engine.sample_rate as f32,
            running: self.audio.router.running(),
        }
    }

    fn refresh_tray(&mut self) {
        let status = TrayStatus {
            muted: self.audio.router.is_muted(),
            line: if self.audio.router.running() {
                let stats = self.stats();
                format!("Running, DSP {:.0}%", stats.dsp_load * 100.0)
            } else if self.graph.nodes.is_empty() {
                "Idle: graph is empty".into()
            } else {
                "Idle: no output connected".into()
            },
        };
        if status != self.tray_status {
            self.tray_status = status;
            if let Some(tray) = &mut self.tray {
                tray.update(&self.tray_status);
            }
        }
    }

    fn graph_state(&self) -> GraphState {
        GraphState {
            graph: self.graph.clone(),
            can_undo: self.history.can_undo(),
            can_redo: self.history.can_redo(),
            muted: self.audio.router.is_muted(),
        }
    }

    fn state(&self) -> DaemonState {
        DaemonState {
            graph: self.graph_state(),
            devices: self.audio.router.devices().to_vec(),
            apps: self.audio.router.apps().to_vec(),
            kinds: self.kinds(),
            engine: self.settings.engine(),
            backend: self.audio.router.backend_name().into(),
        }
    }

    fn kinds(&self) -> Vec<KindAvailability> {
        catalog::menu()
            .into_iter()
            .map(|kind| {
                let info = catalog::describe(&kind);
                let unavailable = match (&kind, info.category) {
                    (NodeKind::NvAfx { effect }, Category::Ai) => {
                        backend::nvafx_unavailable(self.external.as_deref(), effect)
                    }
                    _ => None,
                };
                KindAvailability {
                    kind,
                    category: info.category,
                    label: info.label,
                    unavailable,
                }
            })
            .collect()
    }

    fn broadcast_graph(&mut self) {
        self.broadcast(Event::GraphChanged(Box::new(self.graph_state())));
    }

    fn broadcast(&mut self, event: Event) {
        let mut dropped = Vec::new();
        for (&id, client) in &self.clients {
            if self.greeted.contains(&id) && !client.send(ServerMsg::Event(event.clone())) {
                dropped.push(id);
            }
        }
        for id in dropped {
            log::warn!("client {id} is not keeping up; disconnecting");
            self.drop_client(id);
        }
    }

    fn send_to(&mut self, id: ClientId, message: ServerMsg) {
        if let Some(client) = self.clients.get(&id)
            && !client.send(message)
        {
            self.drop_client(id);
        }
    }
}

fn err(e: GraphError) -> String {
    e.to_string()
}

fn start_audio(
    choice: &BackendChoice,
    settings: &Settings,
    external: Option<Arc<dyn ExternalFactory>>,
) -> Result<Audio, String> {
    let engine_settings = settings.engine();
    let stream = StreamConfig {
        sample_rate: engine_settings.sample_rate,
        block_frames: engine_settings.block_frames,
    };
    let platform = backend::create(choice, stream)?;
    let (handle, engine) = engine(
        EngineConfig {
            sample_rate: engine_settings.sample_rate as f32,
            max_block: 1024,
        },
        external,
    );
    Ok(Audio {
        engine: handle,
        router: Router::new(platform, engine, stream),
        latency_frames: 0,
    })
}
