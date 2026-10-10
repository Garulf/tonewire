use std::collections::HashMap;
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use nodeaudio_graph::{AppStream, DeviceInfo, Direction, EndpointSpec, NodeId};

use crate::hub::{CapturePort, PlaybackPort, StreamPort};
use crate::platform::{Platform, PlatformEvent, StreamConfig, StreamHandle};

pub const NULL_MIC: &str = "null.mic";
pub const NULL_SPEAKERS: &str = "null.speakers";
const SINE_HZ: f32 = 440.0;
const SINE_AMPLITUDE: f32 = 0.25;

struct Capture {
    port: CapturePort,
    device: Option<String>,
    phase: f32,
}

struct Playback {
    port: PlaybackPort,
    device: Option<String>,
}

#[derive(Default)]
struct State {
    opens: HashMap<NodeId, usize>,
    fail_opens: bool,
    devices: Vec<DeviceInfo>,
    captures: Vec<Capture>,
    playbacks: Vec<Playback>,
    events: Vec<PlatformEvent>,
    peaks: HashMap<NodeId, f32>,
}

/// A backend with a synthetic sine microphone and a speaker that only
/// measures what it is sent, driven by a timer thread. Used for tests and
/// for running the daemon without an audio system.
pub struct NullPlatform {
    state: Arc<Mutex<State>>,
    running: Arc<AtomicBool>,
}

/// Test hooks into a running [`NullPlatform`].
#[derive(Clone)]
pub struct NullProbe {
    state: Arc<Mutex<State>>,
}

struct NullStream {
    node: NodeId,
    state: Weak<Mutex<State>>,
}

impl Drop for NullStream {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade()
            && let Ok(mut state) = state.lock()
        {
            state.captures.retain(|c| c.port.node != self.node);
            state.playbacks.retain(|p| p.port.node != self.node);
        }
    }
}

impl Drop for NullPlatform {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn default_devices() -> Vec<DeviceInfo> {
    vec![
        DeviceInfo {
            id: NULL_MIC.into(),
            name: "Null microphone (440 Hz)".into(),
            direction: Direction::Capture,
            channels: 1,
            is_virtual: false,
            is_default: true,
        },
        DeviceInfo {
            id: NULL_SPEAKERS.into(),
            name: "Null speakers".into(),
            direction: Direction::Playback,
            channels: 2,
            is_virtual: false,
            is_default: true,
        },
    ]
}

impl NullPlatform {
    pub fn start(config: StreamConfig) -> (Self, NullProbe) {
        let state = Arc::new(Mutex::new(State {
            devices: default_devices(),
            ..State::default()
        }));
        let running = Arc::new(AtomicBool::new(true));
        let thread_state = Arc::downgrade(&state);
        let thread_running = running.clone();
        std::thread::Builder::new()
            .name("null-audio".into())
            .spawn(move || run(thread_state, thread_running, config))
            .expect("spawn null audio thread");
        (
            Self {
                state: state.clone(),
                running,
            },
            NullProbe { state },
        )
    }
}

fn run(state: Weak<Mutex<State>>, running: Arc<AtomicBool>, config: StreamConfig) {
    let frames = config.block_frames.max(1) as usize;
    let period = Duration::from_secs_f64(frames as f64 / f64::from(config.sample_rate));
    let step = TAU * SINE_HZ / config.sample_rate as f32;
    let mut scratch = vec![0.0f32; frames * nodeaudio_graph::MAX_CHANNELS];
    let mut mono = vec![0.0f32; frames];
    let mut deadline = Instant::now();
    while running.load(Ordering::Relaxed) {
        deadline += period;
        let Some(state) = state.upgrade() else {
            return;
        };
        if let Ok(mut state) = state.lock() {
            let state = &mut *state;
            for capture in &mut state.captures {
                for s in &mut mono {
                    *s = SINE_AMPLITUDE * capture.phase.sin();
                    capture.phase = (capture.phase + step) % TAU;
                }
                capture.port.push_interleaved(&mono, 1);
            }
            for playback in &mut state.playbacks {
                let channels = playback.port.channels();
                let out = &mut scratch[..frames * channels];
                playback.port.render_interleaved(out, channels);
                let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                let entry = state.peaks.entry(playback.port.node).or_default();
                *entry = entry.max(peak);
            }
        }
        drop(state);
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else {
            deadline = now;
        }
    }
}

impl Platform for NullPlatform {
    fn name(&self) -> &'static str {
        "null"
    }

    fn devices(&mut self) -> Vec<DeviceInfo> {
        self.state
            .lock()
            .map(|s| s.devices.clone())
            .unwrap_or_default()
    }

    fn apps(&mut self) -> Vec<AppStream> {
        vec![AppStream {
            id: 1,
            binary: "null-player".into(),
            name: "Null player".into(),
            direction: Direction::Playback,
        }]
    }

    fn open(
        &mut self,
        spec: &EndpointSpec,
        port: StreamPort,
        _: StreamConfig,
    ) -> Result<StreamHandle, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "null backend poisoned".to_string())?;
        *state.opens.entry(spec.node).or_default() += 1;
        if state.fail_opens {
            return Err("null backend told to fail".into());
        }
        let device = spec.device.as_ref().map(|d| d.id.clone());
        let node = spec.node;
        match port {
            StreamPort::Capture(port) => state.captures.push(Capture {
                port,
                device,
                phase: 0.0,
            }),
            StreamPort::Playback(port) => state.playbacks.push(Playback { port, device }),
        }
        Ok(Box::new(NullStream {
            node,
            state: Arc::downgrade(&self.state),
        }))
    }

    fn poll(&mut self) -> Vec<PlatformEvent> {
        self.state
            .lock()
            .map(|mut s| std::mem::take(&mut s.events))
            .unwrap_or_default()
    }
}

impl NullProbe {
    /// Peak sent to a playback endpoint since the last call.
    pub fn take_peak(&self, node: NodeId) -> f32 {
        self.state
            .lock()
            .ok()
            .and_then(|mut s| s.peaks.remove(&node))
            .unwrap_or(0.0)
    }

    pub fn unplug(&self, id: &str) {
        if let Ok(mut s) = self.state.lock() {
            s.devices.retain(|d| d.id != id);
            let failed: Vec<NodeId> = s
                .captures
                .iter()
                .filter(|c| c.device.as_deref() == Some(id))
                .map(|c| c.port.node)
                .chain(
                    s.playbacks
                        .iter()
                        .filter(|p| p.device.as_deref() == Some(id))
                        .map(|p| p.port.node),
                )
                .collect();
            s.events.push(PlatformEvent::DevicesChanged);
            for node in failed {
                s.events
                    .push(PlatformEvent::StreamFailed(node, "device removed".into()));
            }
        }
    }

    /// Times `open` was attempted for `node`.
    pub fn opens(&self, node: NodeId) -> usize {
        self.state
            .lock()
            .map(|s| s.opens.get(&node).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn fail_opens(&self, fail: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.fail_opens = fail;
        }
    }

    /// Adds a second pair of speakers and makes it the default output.
    pub fn switch_default_speakers(&self) {
        if let Ok(mut s) = self.state.lock() {
            for d in s
                .devices
                .iter_mut()
                .filter(|d| d.direction == Direction::Playback)
            {
                d.is_default = false;
            }
            s.devices.push(DeviceInfo {
                id: "null.headphones".into(),
                name: "Null headphones".into(),
                direction: Direction::Playback,
                channels: 2,
                is_virtual: false,
                is_default: true,
            });
            s.events.push(PlatformEvent::DevicesChanged);
        }
    }

    pub fn replug_defaults(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.devices = default_devices();
            s.events.push(PlatformEvent::DevicesChanged);
        }
    }
}
