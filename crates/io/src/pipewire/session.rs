use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use pipewire as pw;
use pw::properties::{PropertiesBox, properties};
use pw::registry::{GlobalObject, RegistryRc};
use pw::spa::utils::dict::DictRef;
use pw::stream::{StreamFlags, StreamRc, StreamState};
use pw::types::ObjectType;
use tonewire_graph::{AppMode, AppStream, DeviceInfo, Direction, EndpointKind, EndpointSpec};

use super::format::{audio_format, pod};
use super::{Cmd, NODE_PREFIX, Shared};
use crate::hub::{CapturePort, PlaybackPort, StreamPort};
use crate::platform::{PlatformEvent, StreamConfig};

const REFRESH_EVERY: Duration = Duration::from_millis(250);

struct NodeEntry {
    props: BTreeMap<String, String>,
}

impl NodeEntry {
    fn get(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    fn media_class(&self) -> &str {
        self.get("media.class").unwrap_or("")
    }

    fn name(&self) -> &str {
        self.get("node.name").unwrap_or("")
    }

    fn is_ours(&self) -> bool {
        self.name().starts_with(NODE_PREFIX)
    }

    fn binary(&self) -> &str {
        self.get("application.process.binary")
            .or_else(|| self.get("application.name"))
            .unwrap_or("")
    }
}

struct PortEntry {
    node: u32,
    output: bool,
    channel: String,
}

struct Endpoint {
    spec: EndpointSpec,
    name: String,
    _stream: StreamRc,
    _listener: Box<dyn Any>,
    /// App stream nodes redirected to this endpoint through metadata.
    moved: BTreeSet<u32>,
    /// App nodes whose ports are linked into this endpoint (tap mode).
    tapped: BTreeMap<u32, Vec<pw::link::Link>>,
}

#[derive(Default)]
struct Defaults {
    sink: Option<String>,
    source: Option<String>,
}

struct Session {
    core: pw::core::CoreRc,
    shared: Arc<Mutex<Shared>>,
    nodes: HashMap<u32, NodeEntry>,
    ports: HashMap<u32, PortEntry>,
    endpoints: HashMap<u64, Endpoint>,
    metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    defaults: Rc<RefCell<Defaults>>,
    dirty: bool,
}

fn props_of(dict: &DictRef) -> BTreeMap<String, String> {
    dict.iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Pulls `"name": "<value>"` out of the JSON PipeWire stores for defaults.
fn json_name(value: &str) -> Option<String> {
    let start = value.find("\"name\"")? + 6;
    let rest = &value[start..];
    let open = rest.find('"')? + 1;
    let close = rest[open..].find('"')?;
    Some(rest[open..open + close].to_owned())
}

impl Session {
    fn on_global(
        &mut self,
        registry: &RegistryRc,
        global: &GlobalObject<&DictRef>,
        session: &Rc<RefCell<Session>>,
    ) {
        let Some(props) = global.props else {
            return;
        };
        match global.type_ {
            ObjectType::Node => {
                self.nodes.insert(
                    global.id,
                    NodeEntry {
                        props: props_of(props),
                    },
                );
                self.dirty = true;
            }
            ObjectType::Port => {
                let node = props.get("node.id").and_then(|v| v.parse().ok());
                if let Some(node) = node {
                    self.ports.insert(
                        global.id,
                        PortEntry {
                            node,
                            output: props.get("port.direction") == Some("out"),
                            channel: props.get("audio.channel").unwrap_or("").to_owned(),
                        },
                    );
                    self.dirty = true;
                }
            }
            ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                match registry.bind::<pw::metadata::Metadata, _>(global) {
                    Ok(metadata) => {
                        let defaults = self.defaults.clone();
                        let weak = Rc::downgrade(session);
                        let listener = metadata
                            .add_listener_local()
                            .property(move |subject, key, _type, value| {
                                if subject == 0 {
                                    let name = value.and_then(json_name);
                                    let mut d = defaults.borrow_mut();
                                    match key {
                                        Some("default.audio.sink") => d.sink = name,
                                        Some("default.audio.source") => d.source = name,
                                        _ => return 0,
                                    }
                                    if let Some(s) = weak.upgrade()
                                        && let Ok(mut s) = s.try_borrow_mut()
                                    {
                                        s.dirty = true;
                                    }
                                }
                                0
                            })
                            .register();
                        self.metadata = Some((metadata, listener));
                    }
                    Err(e) => log::warn!("cannot bind default metadata: {e}"),
                }
            }
            _ => {}
        }
    }

    fn on_global_remove(&mut self, id: u32) {
        if self.nodes.remove(&id).is_some() || self.ports.remove(&id).is_some() {
            for endpoint in self.endpoints.values_mut() {
                endpoint.moved.remove(&id);
                endpoint.tapped.remove(&id);
            }
            self.dirty = true;
        }
    }

    fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let devices = self.device_list();
        let apps = self.app_list();
        if let Ok(mut shared) = self.shared.lock() {
            if shared.devices != devices {
                shared.devices = devices;
                shared.events.push(PlatformEvent::DevicesChanged);
            }
            if shared.apps != apps {
                shared.apps = apps;
                shared.events.push(PlatformEvent::AppsChanged);
            }
        }
        self.apply_app_rules();
    }

    fn device_list(&self) -> Vec<DeviceInfo> {
        let defaults = self.defaults.borrow();
        let mut devices: Vec<DeviceInfo> = self
            .nodes
            .values()
            .filter(|n| !n.is_ours())
            .filter_map(|n| {
                let class = n.media_class();
                let direction = match class {
                    "Audio/Sink" => Direction::Playback,
                    "Audio/Source" | "Audio/Source/Virtual" => Direction::Capture,
                    _ => return None,
                };
                let default = match direction {
                    Direction::Playback => defaults.sink.as_deref(),
                    Direction::Capture => defaults.source.as_deref(),
                };
                Some(DeviceInfo {
                    id: n.name().to_owned(),
                    name: n
                        .get("node.description")
                        .or_else(|| n.get("node.nick"))
                        .unwrap_or(n.name())
                        .to_owned(),
                    direction,
                    channels: n
                        .get("audio.channels")
                        .and_then(|c| c.parse().ok())
                        .unwrap_or(2),
                    is_virtual: n.get("node.virtual") == Some("true")
                        || class.ends_with("/Virtual")
                        || n.get("factory.name")
                            .is_some_and(|f| f.contains("null-audio-sink")),
                    is_default: default == Some(n.name()),
                })
            })
            .collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        devices
    }

    fn app_list(&self) -> Vec<AppStream> {
        let mut apps: Vec<AppStream> = self
            .nodes
            .iter()
            .filter(|(_, n)| !n.is_ours())
            .filter_map(|(&id, n)| {
                let direction = match n.media_class() {
                    "Stream/Output/Audio" => Direction::Playback,
                    "Stream/Input/Audio" => Direction::Capture,
                    _ => return None,
                };
                Some(AppStream {
                    id: u64::from(id),
                    binary: n.binary().to_owned(),
                    name: n
                        .get("application.name")
                        .or_else(|| n.get("node.description"))
                        .unwrap_or(n.binary())
                        .to_owned(),
                    direction,
                })
            })
            .collect();
        apps.sort_by_key(|a| a.id);
        apps
    }

    fn our_node(&self, name: &str) -> Option<u32> {
        self.nodes
            .iter()
            .find(|(_, n)| n.name() == name)
            .map(|(&id, _)| id)
    }

    /// Moves, taps or re-targets app streams that match an endpoint's rules.
    fn apply_app_rules(&mut self) {
        let mut moves: Vec<(u64, u32, String)> = Vec::new();
        let mut taps: Vec<(u64, u32, u32)> = Vec::new();
        for (&handle, endpoint) in &self.endpoints {
            let wanted_class = match endpoint.spec.kind {
                EndpointKind::AppSource => "Stream/Output/Audio",
                EndpointKind::VirtualMic => "Stream/Input/Audio",
                _ => continue,
            };
            let tap = endpoint.spec.kind == EndpointKind::AppSource
                && endpoint.spec.app_mode == AppMode::Tap;
            for (&app, node) in &self.nodes {
                if node.is_ours()
                    || node.media_class() != wanted_class
                    || !endpoint
                        .spec
                        .apps
                        .iter()
                        .any(|rule| rule.matches(node.binary()))
                {
                    continue;
                }
                if tap {
                    if !endpoint.tapped.contains_key(&app)
                        && let Some(ours) = self.our_node(&endpoint.name)
                    {
                        taps.push((handle, app, ours));
                    }
                } else if !endpoint.moved.contains(&app) {
                    moves.push((handle, app, endpoint.name.clone()));
                }
            }
        }
        for (handle, app, target) in moves {
            if let Some((metadata, _)) = &self.metadata {
                metadata.set_property(app, "target.object", None, Some(&target));
                if let Some(endpoint) = self.endpoints.get_mut(&handle) {
                    endpoint.moved.insert(app);
                }
            }
        }
        for (handle, app, ours) in taps {
            let links = self.link_ports(app, ours);
            if !links.is_empty()
                && let Some(endpoint) = self.endpoints.get_mut(&handle)
            {
                endpoint.tapped.insert(app, links);
            }
        }
    }

    /// Links every output port of `from` to the input port of `to` carrying
    /// the same channel (mono ports feed every input).
    fn link_ports(&self, from: u32, to: u32) -> Vec<pw::link::Link> {
        let outputs: Vec<(u32, &PortEntry)> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == from && p.output && !p.channel.starts_with("MONITOR"))
            .map(|(&id, p)| (id, p))
            .collect();
        let inputs: Vec<(u32, &PortEntry)> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == to && !p.output)
            .map(|(&id, p)| (id, p))
            .collect();
        let mut links = Vec::new();
        for (out_id, out) in &outputs {
            for (in_id, input) in &inputs {
                let matches = out.channel == input.channel
                    || out.channel == "MONO"
                    || input.channel == "MONO";
                if !matches {
                    continue;
                }
                let props = properties! {
                    "link.output.node" => from.to_string(),
                    "link.output.port" => out_id.to_string(),
                    "link.input.node" => to.to_string(),
                    "link.input.port" => in_id.to_string(),
                    "object.linger" => "false",
                };
                match self
                    .core
                    .create_object::<pw::link::Link>("link-factory", &props)
                {
                    Ok(link) => links.push(link),
                    Err(e) => log::warn!("cannot link app port {out_id} to {in_id}: {e}"),
                }
            }
        }
        links
    }

    fn open(
        &mut self,
        handle: u64,
        spec: EndpointSpec,
        port: StreamPort,
        config: StreamConfig,
    ) -> Result<(), String> {
        let name = format!("{NODE_PREFIX}{}.{handle}", spec.node.0);
        let channels = spec.channels.max(1);
        let (props, direction) = stream_props(&spec, &name, config);
        let stream = StreamRc::new(self.core.clone(), &name, props).map_err(|e| e.to_string())?;
        let node = spec.node;
        let shared = self.shared.clone();
        let on_state = move |old: StreamState, new: StreamState| {
            if let StreamState::Error(message) = new {
                log::warn!("PipeWire stream for {node} failed ({old:?}): {message}");
                if let Ok(mut s) = shared.lock() {
                    s.events.push(PlatformEvent::StreamFailed(node, message));
                }
            }
        };
        let listener: Box<dyn Any> = match port {
            StreamPort::Capture(port) => Box::new(
                stream
                    .add_local_listener_with_user_data(port)
                    .state_changed(move |_, _, old, new| on_state(old, new))
                    .process(capture)
                    .register()
                    .map_err(|e| e.to_string())?,
            ),
            StreamPort::Playback(port) => Box::new(
                stream
                    .add_local_listener_with_user_data(port)
                    .state_changed(move |_, _, old, new| on_state(old, new))
                    .process(playback)
                    .register()
                    .map_err(|e| e.to_string())?,
            ),
        };
        let format = audio_format(config.sample_rate, u32::from(channels));
        let mut params = [pod(&format)];
        stream
            .connect(
                direction,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| e.to_string())?;
        self.endpoints.insert(
            handle,
            Endpoint {
                spec,
                name,
                _stream: stream,
                _listener: listener,
                moved: BTreeSet::new(),
                tapped: BTreeMap::new(),
            },
        );
        self.dirty = true;
        Ok(())
    }

    fn close(&mut self, handle: u64) {
        if let Some(endpoint) = self.endpoints.remove(&handle) {
            self.release_apps(&endpoint);
        }
    }

    fn release_apps(&self, endpoint: &Endpoint) {
        if let Some((metadata, _)) = &self.metadata {
            for &app in &endpoint.moved {
                metadata.set_property(app, "target.object", None, None);
            }
        }
    }

    fn shutdown(&mut self) {
        let endpoints: Vec<Endpoint> = self.endpoints.drain().map(|(_, e)| e).collect();
        for endpoint in &endpoints {
            self.release_apps(endpoint);
        }
    }
}

fn stream_props(
    spec: &EndpointSpec,
    name: &str,
    config: StreamConfig,
) -> (PropertiesBox, pw::spa::utils::Direction) {
    use pw::spa::utils::Direction as PwDirection;
    let description = match spec.kind {
        EndpointKind::AppSource => format!("tonewire: {} (apps)", spec.name),
        EndpointKind::VirtualMic | EndpointKind::VirtualSpeaker => spec.name.clone(),
        _ => format!("tonewire: {}", spec.name),
    };
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::NODE_NAME => name,
        *pw::keys::NODE_DESCRIPTION => description,
        *pw::keys::APP_NAME => "tonewire",
        *pw::keys::NODE_LATENCY => format!("{}/{}", config.block_frames, config.sample_rate),
        *pw::keys::AUDIO_CHANNELS => spec.channels.max(1).to_string(),
    };
    let direction = match spec.kind {
        EndpointKind::DeviceInput => {
            props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
            if spec.device.as_ref().is_some_and(|d| d.loopback) {
                props.insert("stream.capture.sink", "true");
            }
            PwDirection::Input
        }
        EndpointKind::DeviceOutput => {
            props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
            PwDirection::Output
        }
        EndpointKind::VirtualSpeaker | EndpointKind::AppSource => {
            props.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
            props.insert("node.virtual", "true");
            PwDirection::Input
        }
        EndpointKind::VirtualMic => {
            props.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
            props.insert("node.virtual", "true");
            props.insert("node.always-process", "true");
            PwDirection::Output
        }
    };
    if let Some(device) = &spec.device {
        props.insert("target.object", device.id.as_str());
        props.insert("node.dont-reconnect", "true");
    }
    (props, direction)
}

fn capture(stream: &pw::stream::Stream, port: &mut CapturePort) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    if let Some(bytes) = data.data() {
        let end = (offset + size).min(bytes.len());
        let bytes = &bytes[offset.min(end)..end];
        // SAFETY: f32 has no invalid bit patterns; misaligned buffers are skipped.
        let (head, samples, _) = unsafe { bytes.align_to::<f32>() };
        if head.is_empty() {
            let channels = port.channels();
            port.push_interleaved(samples, channels);
        }
    }
}

fn playback(stream: &pw::stream::Stream, port: &mut PlaybackPort) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let channels = port.channels();
    let stride = channels * std::mem::size_of::<f32>();
    let mut written = 0;
    if let Some(bytes) = data.data() {
        let capacity = bytes.len() / stride;
        let frames = if requested > 0 {
            requested.min(capacity)
        } else {
            capacity
        };
        // SAFETY: f32 has no invalid bit patterns; misaligned buffers are skipped.
        let (head, samples, _) = unsafe { bytes[..frames * stride].align_to_mut::<f32>() };
        if head.is_empty() {
            port.render_interleaved(samples, channels);
            written = frames;
        }
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = (written * stride) as u32;
}

pub(super) fn run(
    receiver: pw::channel::Receiver<Cmd>,
    shared: Arc<Mutex<Shared>>,
    _config: StreamConfig,
    ready: mpsc::Sender<Result<(), String>>,
) {
    pw::init();
    let setup = || -> Result<_, pw::Error> {
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        Ok((main_loop, context, core, registry))
    };
    let (main_loop, _context, core, registry) = match setup() {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };

    let session = Rc::new(RefCell::new(Session {
        core: core.clone(),
        shared,
        nodes: HashMap::new(),
        ports: HashMap::new(),
        endpoints: HashMap::new(),
        metadata: None,
        defaults: Rc::new(RefCell::new(Defaults::default())),
        dirty: true,
    }));

    let weak_loop = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            log::error!("PipeWire error on object {id} ({res}): {message}");
            if id == 0
                && let Some(main_loop) = weak_loop.upgrade()
            {
                main_loop.quit();
            }
        })
        .register();

    let registry_weak = registry.downgrade();
    let on_add = session.clone();
    let on_remove = session.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(registry) = registry_weak.upgrade() {
                on_add.borrow_mut().on_global(&registry, global, &on_add);
            }
        })
        .global_remove(move |id| on_remove.borrow_mut().on_global_remove(id))
        .register();

    let refresher = session.clone();
    let timer = main_loop
        .loop_()
        .add_timer(move |_| refresher.borrow_mut().refresh());
    let _ = timer.update_timer(Some(REFRESH_EVERY), Some(REFRESH_EVERY));

    let commands = session.clone();
    let weak_loop = main_loop.downgrade();
    let _receiver = receiver.attach(main_loop.loop_(), move |command| match command {
        Cmd::Open {
            handle,
            spec,
            port,
            config,
            reply,
        } => {
            let result = commands.borrow_mut().open(handle, spec, *port, config);
            let _ = reply.send(result);
        }
        Cmd::Close(handle) => commands.borrow_mut().close(handle),
        Cmd::Quit => {
            if let Some(main_loop) = weak_loop.upgrade() {
                main_loop.quit();
            }
        }
    });

    let _ = ready.send(Ok(()));
    main_loop.run();
    session.borrow_mut().shutdown();
}
