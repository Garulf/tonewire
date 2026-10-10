//! Native PipeWire backend. All PipeWire objects live on one thread running
//! the PipeWire main loop; the control thread talks to it through a channel.

mod format;
mod session;

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use nodeaudio_graph::{AppStream, DeviceInfo, EndpointSpec};
use pipewire as pw;

use crate::hub::StreamPort;
use crate::platform::{Platform, PlatformEvent, StreamConfig, StreamHandle};

const OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Prefix of every PipeWire node this process creates, so our own nodes can
/// be hidden from device and app lists.
pub(crate) const NODE_PREFIX: &str = "nodeaudio.";

#[derive(Default)]
pub(crate) struct Shared {
    pub devices: Vec<DeviceInfo>,
    pub apps: Vec<AppStream>,
    pub events: Vec<PlatformEvent>,
}

pub(crate) enum Cmd {
    Open {
        handle: u64,
        spec: EndpointSpec,
        port: Box<StreamPort>,
        config: StreamConfig,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Close(u64),
    Quit,
}

pub struct PipeWirePlatform {
    commands: pw::channel::Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
    next_handle: u64,
}

struct PwStream {
    handle: u64,
    commands: pw::channel::Sender<Cmd>,
}

impl Drop for PwStream {
    fn drop(&mut self) {
        let _ = self.commands.send(Cmd::Close(self.handle));
    }
}

impl Drop for PipeWirePlatform {
    fn drop(&mut self) {
        let _ = self.commands.send(Cmd::Quit);
    }
}

impl PipeWirePlatform {
    /// Connects to the PipeWire daemon and starts the loop thread.
    pub fn connect(config: StreamConfig) -> Result<Self, String> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (commands, receiver) = pw::channel::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || session::run(receiver, thread_shared, config, ready_tx))
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv_timeout(OPEN_TIMEOUT)
            .map_err(|_| "PipeWire did not answer".to_string())??;
        Ok(Self {
            commands,
            shared,
            next_handle: 1,
        })
    }
}

impl Platform for PipeWirePlatform {
    fn name(&self) -> &'static str {
        "pipewire"
    }

    fn follows_default(&self) -> bool {
        true
    }

    fn devices(&mut self) -> Vec<DeviceInfo> {
        self.shared
            .lock()
            .map(|s| s.devices.clone())
            .unwrap_or_default()
    }

    fn apps(&mut self) -> Vec<AppStream> {
        self.shared
            .lock()
            .map(|s| s.apps.clone())
            .unwrap_or_default()
    }

    fn open(
        &mut self,
        spec: &EndpointSpec,
        port: StreamPort,
        config: StreamConfig,
    ) -> Result<StreamHandle, String> {
        let handle = self.next_handle;
        self.next_handle += 1;
        let (reply, result) = mpsc::channel();
        self.commands
            .send(Cmd::Open {
                handle,
                spec: spec.clone(),
                port: Box::new(port),
                config,
                reply,
            })
            .map_err(|_| "PipeWire thread has stopped".to_string())?;
        result
            .recv_timeout(OPEN_TIMEOUT)
            .map_err(|_| "PipeWire did not open the stream in time".to_string())??;
        Ok(Box::new(PwStream {
            handle,
            commands: self.commands.clone(),
        }))
    }

    fn poll(&mut self) -> Vec<PlatformEvent> {
        self.shared
            .lock()
            .map(|mut s| std::mem::take(&mut s.events))
            .unwrap_or_default()
    }
}
