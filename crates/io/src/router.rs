use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tonewire_graph::{
    AppStream, DeviceInfo, Direction, EndpointKind, EndpointSpec, EndpointStatus, Engine, NodeId,
};

use crate::hub::{HubControl, StreamPort, StreamStatus};
use crate::platform::{Platform, PlatformEvent, StreamConfig, StreamHandle};
use crate::ring::{RingStats, ring_target};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

struct OpenStream {
    spec: EndpointSpec,
    /// The system default it was opened on, for endpoints that follow it.
    opened_on_default: Option<String>,
    _handle: StreamHandle,
    status: Arc<StreamStatus>,
    ring: Arc<RingStats>,
}

struct Failure {
    message: String,
    retry_at: Instant,
    backoff: Duration,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    pub devices: bool,
    pub apps: bool,
    pub status: bool,
}

/// Keeps the platform's open streams in line with the graph's endpoints,
/// elects the clock master, and retries failed streams with backoff.
pub struct Router {
    platform: Box<dyn Platform>,
    hub: HubControl,
    config: StreamConfig,
    desired: BTreeMap<NodeId, EndpointSpec>,
    open: BTreeMap<NodeId, OpenStream>,
    failed: BTreeMap<NodeId, Failure>,
    devices: Vec<DeviceInfo>,
    apps: Vec<AppStream>,
    last_status: Vec<(NodeId, EndpointStatus)>,
}

impl Router {
    pub fn new(mut platform: Box<dyn Platform>, engine: Engine, config: StreamConfig) -> Self {
        let devices = platform.devices();
        let apps = platform.apps();
        Self {
            platform,
            hub: HubControl::new(engine),
            config,
            desired: BTreeMap::new(),
            open: BTreeMap::new(),
            failed: BTreeMap::new(),
            devices,
            apps,
            last_status: Vec::new(),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        self.platform.name()
    }

    pub fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    pub fn apps(&self) -> &[AppStream] {
        &self.apps
    }

    pub fn set_muted(&self, muted: bool) {
        self.hub.set_muted(muted);
    }

    pub fn is_muted(&self) -> bool {
        self.hub.is_muted()
    }

    /// True while some playback stream is driving the engine.
    pub fn running(&self) -> bool {
        self.hub.clock().master().is_some_and(|m| {
            self.open
                .get(&m)
                .is_some_and(|o| o.status.running.load(Ordering::Relaxed))
        })
    }

    pub fn master(&self) -> Option<NodeId> {
        self.hub.clock().master()
    }

    pub fn apply(&mut self, endpoints: Vec<EndpointSpec>) {
        let previous = std::mem::replace(
            &mut self.desired,
            endpoints.into_iter().map(|e| (e.node, e)).collect(),
        );
        let stale: Vec<NodeId> = self
            .open
            .iter()
            .filter(|(id, o)| {
                self.desired
                    .get(id)
                    .is_none_or(|d| needs_reopen(&o.spec, d))
            })
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            self.close(id);
        }
        self.failed.retain(|id, _| {
            self.desired
                .get(id)
                .is_some_and(|d| previous.get(id) == Some(d))
        });
        self.open_missing(Instant::now(), false);
    }

    fn follows_default(spec: &EndpointSpec) -> Option<Direction> {
        match (spec.kind, &spec.device) {
            (EndpointKind::DeviceInput, None) => Some(Direction::Capture),
            (EndpointKind::DeviceOutput, None) => Some(Direction::Playback),
            _ => None,
        }
    }

    fn current_default(&self, direction: Direction) -> Option<String> {
        self.devices
            .iter()
            .find(|d| d.direction == direction && d.is_default)
            .map(|d| d.id.clone())
    }

    pub fn tick(&mut self, now: Instant) -> Changes {
        let mut changes = Changes::default();
        for event in self.platform.poll() {
            match event {
                PlatformEvent::DevicesChanged => changes.devices = true,
                PlatformEvent::AppsChanged => changes.apps = true,
                PlatformEvent::StreamFailed(node, message) => {
                    log::warn!("stream for node {node} failed: {message}");
                    self.close(node);
                    self.fail(node, message, now);
                }
            }
        }
        if changes.devices {
            self.devices = self.platform.devices();
            let manage_defaults = !self.platform.follows_default();
            let gone: Vec<NodeId> = self
                .open
                .iter()
                .filter(|(_, o)| {
                    let default_moved = manage_defaults
                        && Self::follows_default(&o.spec)
                            .is_some_and(|dir| self.current_default(dir) != o.opened_on_default);
                    default_moved || !self.device_present(&o.spec)
                })
                .map(|(&id, _)| id)
                .collect();
            for id in gone {
                self.close(id);
            }
        }
        if changes.apps {
            self.apps = self.platform.apps();
        }
        self.open_missing(now, changes.devices);
        self.hub.collect();
        let status = self.status();
        changes.status = status != self.last_status;
        self.last_status = status;
        changes
    }

    pub fn status(&self) -> Vec<(NodeId, EndpointStatus)> {
        self.desired
            .keys()
            .map(|&id| {
                let status = if let Some(open) = self.open.get(&id) {
                    EndpointStatus {
                        connected: true,
                        xruns: open.ring.xruns(),
                        message: None,
                    }
                } else if let Some(failure) = self.failed.get(&id) {
                    EndpointStatus {
                        connected: false,
                        xruns: 0,
                        message: Some(failure.message.clone()),
                    }
                } else {
                    EndpointStatus::default()
                };
                (id, status)
            })
            .collect()
    }

    fn device_present(&self, spec: &EndpointSpec) -> bool {
        let Some(device) = &spec.device else {
            return true;
        };
        let wanted = if spec.kind.is_capture() && !device.loopback {
            Direction::Capture
        } else {
            Direction::Playback
        };
        self.devices
            .iter()
            .any(|d| d.id == device.id && d.direction == wanted)
    }

    fn open_missing(&mut self, now: Instant, devices_changed: bool) {
        let pending: Vec<EndpointSpec> = self
            .desired
            .values()
            .filter(|spec| !self.open.contains_key(&spec.node))
            .cloned()
            .collect();
        for spec in pending {
            if let Some(failure) = self.failed.get(&spec.node)
                && failure.retry_at > now
                && !devices_changed
            {
                continue;
            }
            if !self.device_present(&spec) {
                let name = spec.device.as_ref().map_or("", |d| d.name.as_str());
                self.fail(spec.node, format!("{name} is not connected"), now);
                continue;
            }
            self.try_open(spec, now);
        }
        self.elect_master();
    }

    fn try_open(&mut self, spec: EndpointSpec, now: Instant) {
        let capture = spec.kind.is_capture();
        let target = ring_target(
            self.config.block_frames as usize,
            self.platform.device_period(self.config),
        );
        let Some(port) = self
            .hub
            .add(spec.node, capture, usize::from(spec.channels), target)
        else {
            self.fail(spec.node, "too many endpoints".into(), now);
            return;
        };
        let (status, ring) = match &port {
            StreamPort::Capture(p) => (p.status.clone(), p.ring_stats().clone()),
            StreamPort::Playback(p) => (p.status.clone(), p.ring_stats().clone()),
        };
        let opened_on_default =
            Self::follows_default(&spec).and_then(|dir| self.current_default(dir));
        match self.platform.open(&spec, port, self.config) {
            Ok(handle) => {
                self.failed.remove(&spec.node);
                self.open.insert(
                    spec.node,
                    OpenStream {
                        opened_on_default,
                        spec,
                        _handle: handle,
                        status,
                        ring,
                    },
                );
            }
            Err(message) => {
                self.hub.remove(spec.node);
                self.fail(spec.node, message, now);
            }
        }
    }

    fn fail(&mut self, node: NodeId, message: String, now: Instant) {
        let backoff = self
            .failed
            .get(&node)
            .map_or(FIRST_RETRY, |f| (f.backoff * 2).min(MAX_RETRY));
        self.failed.insert(
            node,
            Failure {
                message,
                retry_at: now + backoff,
                backoff,
            },
        );
    }

    fn close(&mut self, node: NodeId) {
        if self.open.remove(&node).is_some() {
            self.hub.remove(node);
        }
        self.elect_master();
    }

    fn elect_master(&mut self) {
        let rank = |kind: EndpointKind| match kind {
            EndpointKind::DeviceOutput => Some(0),
            EndpointKind::VirtualMic => Some(1),
            _ => None,
        };
        let current = self.hub.clock().master();
        if current.is_some_and(|m| self.open.contains_key(&m)) {
            return;
        }
        let best = self
            .open
            .values()
            .filter_map(|o| rank(o.spec.kind).map(|r| (r, o.spec.node)))
            .min()
            .map(|(_, node)| node);
        self.hub.clock().set_master(best);
    }
}

fn needs_reopen(old: &EndpointSpec, new: &EndpointSpec) -> bool {
    let virtual_name_changed = matches!(
        new.kind,
        EndpointKind::VirtualMic | EndpointKind::VirtualSpeaker
    ) && old.name != new.name;
    old.kind != new.kind
        || old.channels != new.channels
        || old.device != new.device
        || old.apps != new.apps
        || old.app_mode != new.app_mode
        || virtual_name_changed
}
