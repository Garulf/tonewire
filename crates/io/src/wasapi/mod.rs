//! WASAPI backend. Each endpoint runs on its own MMCSS "Pro Audio" thread;
//! virtual devices are whatever virtual cables are installed.

mod apps;
mod policy;
mod stream;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nodeaudio_graph::{
    AppMode, AppStream, DeviceInfo, Direction, EndpointKind, EndpointSpec, NodeId,
};
use wasapi::{DeviceEnumerator, initialize_mta};

use crate::hub::StreamPort;
use crate::platform::{Platform, PlatformEvent, StreamConfig, StreamHandle};
use stream::{Source, StreamThread};

const RESCAN_EVERY: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct Shared {
    pub events: Vec<PlatformEvent>,
}

/// An app redirection made for an endpoint, undone when the endpoint closes.
struct Redirect {
    endpoint: NodeId,
    pid: u32,
    flow: Direction,
}

pub struct WasapiPlatform {
    shared: Arc<Mutex<Shared>>,
    devices: Vec<DeviceInfo>,
    apps: Vec<AppStream>,
    last_scan: Instant,
    routes: BTreeMap<NodeId, (EndpointSpec, Arc<AtomicBool>)>,
    redirects: Vec<Redirect>,
    policy: Option<policy::PolicyConfig>,
}

struct WasapiStream {
    thread: StreamThread,
    closed: Arc<AtomicBool>,
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.thread.stop();
    }
}

pub fn is_virtual_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["cable", "vb-audio", "virtual", "voicemeeter"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn scan_devices() -> Result<Vec<DeviceInfo>, String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let mut devices = Vec::new();
    for (direction, wasapi_direction) in [
        (Direction::Playback, wasapi::Direction::Render),
        (Direction::Capture, wasapi::Direction::Capture),
    ] {
        let default_id = enumerator
            .get_default_device(&wasapi_direction)
            .and_then(|d| d.get_id())
            .ok();
        let collection = enumerator
            .get_device_collection(&wasapi_direction)
            .map_err(|e| e.to_string())?;
        let count = collection.get_nbr_devices().map_err(|e| e.to_string())?;
        for index in 0..count {
            let Ok(device) = collection.get_device_at_index(index) else {
                continue;
            };
            let (Ok(id), Ok(name)) = (device.get_id(), device.get_friendlyname()) else {
                continue;
            };
            let channels = device
                .get_device_format()
                .map(|f| f.get_nchannels() as u8)
                .unwrap_or(2);
            devices.push(DeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                is_virtual: is_virtual_name(&name),
                id,
                name,
                direction,
                channels,
            });
        }
    }
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}

impl WasapiPlatform {
    pub fn start(_config: StreamConfig) -> Result<Self, String> {
        initialize_mta().ok().map_err(|e| e.to_string())?;
        let policy = match policy::PolicyConfig::new() {
            Ok(policy) => Some(policy),
            Err(e) => {
                log::warn!("per-app device switching unavailable, falling back to tap: {e}");
                None
            }
        };
        Ok(Self {
            shared: Arc::new(Mutex::new(Shared::default())),
            devices: scan_devices()?,
            apps: apps::scan(),
            last_scan: Instant::now(),
            routes: BTreeMap::new(),
            redirects: Vec::new(),
            policy,
        })
    }

    fn device(&self, id: &str) -> Option<&DeviceInfo> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// The recording side of a virtual cable whose playback side is `render`,
    /// matched by name ("CABLE Input" pairs with "CABLE Output").
    fn paired_capture(&self, render: &DeviceInfo) -> Option<&DeviceInfo> {
        let wanted = render
            .name
            .replace("Input", "Output")
            .replace("Speaker", "Mic");
        let captures: Vec<&DeviceInfo> = self
            .devices
            .iter()
            .filter(|d| d.direction == Direction::Capture && d.is_virtual)
            .collect();
        captures
            .iter()
            .find(|d| d.name == wanted)
            .or_else(|| (captures.len() == 1).then(|| &captures[0]))
            .copied()
    }

    fn source_for(&self, spec: &EndpointSpec) -> Result<Source, String> {
        let chosen = spec.device.as_ref().map(|d| d.id.clone());
        let require = |what: &str| {
            chosen
                .clone()
                .ok_or_else(|| format!("choose {what} for this node"))
        };
        Ok(match spec.kind {
            EndpointKind::DeviceInput => match &spec.device {
                Some(d) if d.loopback => Source::Loopback(Some(d.id.clone())),
                Some(d) => Source::Capture(Some(d.id.clone())),
                None => Source::Capture(None),
            },
            EndpointKind::DeviceOutput => Source::Render(chosen),
            EndpointKind::VirtualSpeaker => {
                Source::Loopback(Some(require("the virtual cable apps play into")?))
            }
            EndpointKind::VirtualMic => {
                Source::Render(Some(require("the virtual cable to send to")?))
            }
            EndpointKind::AppSource => match (spec.app_mode, &chosen, &self.policy) {
                (AppMode::Move, Some(cable), Some(_)) => Source::Loopback(Some(cable.clone())),
                _ => Source::Process(spec.apps.clone()),
            },
        })
    }

    fn apply_redirects(&mut self) {
        let Some(policy) = &self.policy else {
            return;
        };
        let running = apps::processes();
        for (node, (spec, _)) in &self.routes {
            let Some(cable) = spec.device.as_ref().and_then(|d| self.device(&d.id)) else {
                continue;
            };
            let (flow, target) = match spec.kind {
                EndpointKind::AppSource if spec.app_mode == AppMode::Move => {
                    (Direction::Playback, cable.id.clone())
                }
                EndpointKind::VirtualMic => match self.paired_capture(cable) {
                    Some(capture) => (Direction::Capture, capture.id.clone()),
                    None => continue,
                },
                _ => continue,
            };
            let done: BTreeSet<u32> = self
                .redirects
                .iter()
                .filter(|r| r.endpoint == *node)
                .map(|r| r.pid)
                .collect();
            for (pid, binary) in &running {
                if done.contains(pid) || !spec.apps.iter().any(|rule| rule.matches(binary)) {
                    continue;
                }
                match policy.redirect(*pid, flow, Some(&target)) {
                    Ok(()) => self.redirects.push(Redirect {
                        endpoint: *node,
                        pid: *pid,
                        flow,
                    }),
                    Err(e) => log::warn!("cannot move {binary} ({pid}): {e}"),
                }
            }
        }
    }

    fn release_redirects(&mut self, node: Option<NodeId>) {
        let (released, kept): (Vec<Redirect>, Vec<Redirect>) = std::mem::take(&mut self.redirects)
            .into_iter()
            .partition(|r| node.is_none_or(|n| r.endpoint == n));
        self.redirects = kept;
        if let Some(policy) = &self.policy {
            for r in released {
                let _ = policy.redirect(r.pid, r.flow, None);
            }
        }
    }
}

impl Drop for WasapiPlatform {
    fn drop(&mut self) {
        self.release_redirects(None);
    }
}

impl Platform for WasapiPlatform {
    fn name(&self) -> &'static str {
        "wasapi"
    }

    fn devices(&mut self) -> Vec<DeviceInfo> {
        self.devices.clone()
    }

    fn apps(&mut self) -> Vec<AppStream> {
        self.apps.clone()
    }

    fn open(
        &mut self,
        spec: &EndpointSpec,
        port: StreamPort,
        config: StreamConfig,
    ) -> Result<StreamHandle, String> {
        let source = self.source_for(spec)?;
        let closed = Arc::new(AtomicBool::new(false));
        let thread = StreamThread::spawn(spec.node, source, port, config, self.shared.clone())?;
        if !spec.apps.is_empty() {
            self.routes
                .insert(spec.node, (spec.clone(), closed.clone()));
            self.apply_redirects();
        }
        Ok(Box::new(WasapiStream { thread, closed }))
    }

    fn device_period(&self, config: StreamConfig) -> usize {
        config.sample_rate as usize / 100
    }

    fn poll(&mut self) -> Vec<PlatformEvent> {
        let closed: Vec<NodeId> = self
            .routes
            .iter()
            .filter(|(_, (_, closed))| closed.load(Ordering::Relaxed))
            .map(|(&n, _)| n)
            .collect();
        for node in closed {
            self.routes.remove(&node);
            self.release_redirects(Some(node));
        }

        let mut events = self
            .shared
            .lock()
            .map(|mut s| std::mem::take(&mut s.events))
            .unwrap_or_default();
        if self.last_scan.elapsed() >= RESCAN_EVERY {
            self.last_scan = Instant::now();
            if let Ok(devices) = scan_devices()
                && devices != self.devices
            {
                self.devices = devices;
                events.push(PlatformEvent::DevicesChanged);
            }
            let apps = apps::scan();
            if apps != self.apps {
                self.apps = apps;
                events.push(PlatformEvent::AppsChanged);
            }
            self.apply_redirects();
        }
        events
    }
}
