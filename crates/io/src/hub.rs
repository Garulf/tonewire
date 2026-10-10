use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use nodeaudio_graph::{AudioBuf, EndpointIo, Engine, NodeId};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::ring::{ReadMode, RingReader, RingWriter, ring};

pub const MAX_ENDPOINTS: usize = 64;

/// The engine's end of one endpoint ring.
enum Port {
    /// Audio captured by a device stream, read by the engine.
    Capture(RingReader),
    /// Audio produced by the engine, played by a device stream.
    Playback(RingWriter),
}

enum HubCommand {
    Add(NodeId, Port),
    Remove(NodeId),
}

/// Engine-side endpoint table, living on the audio thread with the engine.
pub struct Hub {
    ports: Vec<(NodeId, Port)>,
    commands: Consumer<HubCommand>,
    retired: Producer<Port>,
    muted: Arc<AtomicBool>,
}

impl Hub {
    fn apply_commands(&mut self) {
        while let Ok(command) = self.commands.pop() {
            match command {
                HubCommand::Add(node, port) => {
                    self.remove(node);
                    if self.ports.len() < self.ports.capacity() {
                        self.ports.push((node, port));
                    } else {
                        let _ = self.retired.push(port);
                    }
                }
                HubCommand::Remove(node) => self.remove(node),
            }
        }
    }

    fn remove(&mut self, node: NodeId) {
        if let Some(i) = self.ports.iter().position(|(n, _)| *n == node) {
            let (_, port) = self.ports.swap_remove(i);
            if let Err(rtrb::PushError::Full(port)) = self.retired.push(port) {
                std::mem::forget(port);
            }
        }
    }
}

impl EndpointIo for Hub {
    fn read(&mut self, node: NodeId, dest: &mut AudioBuf, frames: usize) -> bool {
        let Some(Port::Capture(reader)) = self
            .ports
            .iter_mut()
            .find(|(n, _)| *n == node)
            .map(|(_, p)| p)
        else {
            return false;
        };
        let channels = dest.channel_count().min(reader.channels());
        let mut slices = dest.slices_mut(frames);
        reader.read(frames, ReadMode::Corrected, |i, frame| {
            for c in 0..channels {
                slices[c][i] = frame[c];
            }
        })
    }

    fn write(&mut self, node: NodeId, src: &AudioBuf, frames: usize) {
        let Some(Port::Playback(writer)) = self
            .ports
            .iter_mut()
            .find(|(n, _)| *n == node)
            .map(|(_, p)| p)
        else {
            return;
        };
        let muted = self.muted.load(Ordering::Relaxed);
        let channels = writer.channels().min(src.channel_count());
        writer.write(frames, |i, frame| {
            frame.fill(0.0);
            if !muted && !src.is_silent() {
                for (c, sample) in frame.iter_mut().enumerate().take(channels) {
                    *sample = src.channel(c, frames)[i];
                }
            }
        });
    }
}

/// The engine plus its endpoint table: whichever stream is clock master runs it.
pub struct Driver {
    engine: Engine,
    hub: Hub,
    prepared: Option<ThreadId>,
}

impl Driver {
    pub fn run(&mut self, frames: usize) {
        let thread = std::thread::current().id();
        if self.prepared != Some(thread) {
            self.engine.prepare_thread();
            self.prepared = Some(thread);
        }
        self.hub.apply_commands();
        self.engine.process(&mut self.hub, frames);
    }
}

/// Shared between all playback streams: who the master is and the driver
/// it runs. Only the master's callback ever locks the driver.
#[derive(Clone)]
pub struct Clock {
    master: Arc<AtomicU32>,
    driver: Arc<Mutex<Driver>>,
    max_block: usize,
}

impl Clock {
    pub fn is_master(&self, node: NodeId) -> bool {
        self.master.load(Ordering::Acquire) == node.0
    }

    pub fn master(&self) -> Option<NodeId> {
        match self.master.load(Ordering::Acquire) {
            0 => None,
            n => Some(NodeId(n)),
        }
    }

    pub fn set_master(&self, node: Option<NodeId>) {
        self.master
            .store(node.map_or(0, |n| n.0), Ordering::Release);
    }

    /// Runs the engine for `frames` if `node` is master. Never blocks.
    fn tick(&self, node: NodeId, frames: usize) -> bool {
        if !self.is_master(node) {
            return false;
        }
        match self.driver.try_lock() {
            Ok(mut driver) => {
                let mut remaining = frames;
                while remaining > 0 {
                    let chunk = remaining.min(self.max_block);
                    driver.run(chunk);
                    remaining -= chunk;
                }
                true
            }
            Err(_) => false,
        }
    }
}

/// Status a stream reports and the control thread reads.
#[derive(Debug, Default)]
pub struct StreamStatus {
    pub running: AtomicBool,
    pub device_errors: AtomicU32,
}

/// Device-side end of a capture endpoint.
pub struct CapturePort {
    pub node: NodeId,
    writer: RingWriter,
    pub status: Arc<StreamStatus>,
}

impl CapturePort {
    pub fn channels(&self) -> usize {
        self.writer.channels()
    }

    pub fn ring_stats(&self) -> &Arc<crate::ring::RingStats> {
        self.writer.stats()
    }

    /// Pushes interleaved device audio with `device_channels` channels,
    /// adapting it to the node's channel count.
    pub fn push_interleaved(&mut self, data: &[f32], device_channels: usize) {
        self.status.running.store(true, Ordering::Relaxed);
        let device_channels = device_channels.max(1);
        let frames = data.len() / device_channels;
        self.writer.write(frames, |i, frame| {
            adapt(&data[i * device_channels..(i + 1) * device_channels], frame);
        });
    }

    pub fn push_silence(&mut self, frames: usize) {
        self.writer.write(frames, |_, frame| frame.fill(0.0));
    }
}

/// Device-side end of a playback endpoint.
pub struct PlaybackPort {
    pub node: NodeId,
    reader: RingReader,
    clock: Clock,
    pub status: Arc<StreamStatus>,
}

impl PlaybackPort {
    pub fn channels(&self) -> usize {
        self.reader.channels()
    }

    pub fn ring_stats(&self) -> &Arc<crate::ring::RingStats> {
        self.reader.stats()
    }

    pub fn is_master(&self) -> bool {
        self.clock.is_master(self.node)
    }

    /// Fills an interleaved device buffer with `device_channels` channels.
    /// When this endpoint is the clock master it runs the engine first.
    pub fn render_interleaved(&mut self, out: &mut [f32], device_channels: usize) {
        self.status.running.store(true, Ordering::Relaxed);
        let device_channels = device_channels.max(1);
        let frames = out.len() / device_channels;
        let mode = if self.clock.tick(self.node, frames) {
            ReadMode::Direct
        } else {
            ReadMode::Corrected
        };
        let delivered = self.reader.read(frames, mode, |i, frame| {
            adapt(
                frame,
                &mut out[i * device_channels..(i + 1) * device_channels],
            );
        });
        if !delivered {
            out.fill(0.0);
        }
    }
}

/// Copies one frame between channel layouts: mono is duplicated, mono
/// targets get the average, otherwise channels map by index.
pub fn adapt(src: &[f32], dst: &mut [f32]) {
    match (src.len(), dst.len()) {
        (s, d) if s == d => dst.copy_from_slice(src),
        (1, _) => dst.fill(src[0]),
        (s, 1) => dst[0] = src.iter().sum::<f32>() / s as f32,
        (_, _) => {
            for (i, d) in dst.iter_mut().enumerate() {
                *d = src.get(i).copied().unwrap_or(0.0);
            }
        }
    }
}

pub enum StreamPort {
    Capture(CapturePort),
    Playback(PlaybackPort),
}

/// Control-thread handle for adding and removing endpoints.
pub struct HubControl {
    commands: Producer<HubCommand>,
    retired: Consumer<Port>,
    clock: Clock,
    muted: Arc<AtomicBool>,
    target_frames: usize,
}

impl HubControl {
    pub fn new(engine: Engine, target_frames: usize) -> Self {
        let max_block = engine.config().max_block;
        let (commands_tx, commands_rx) = RingBuffer::new(MAX_ENDPOINTS * 2);
        let (retired_tx, retired_rx) = RingBuffer::new(MAX_ENDPOINTS * 2);
        let muted = Arc::new(AtomicBool::new(false));
        let hub = Hub {
            ports: Vec::with_capacity(MAX_ENDPOINTS),
            commands: commands_rx,
            retired: retired_tx,
            muted: muted.clone(),
        };
        Self {
            commands: commands_tx,
            retired: retired_rx,
            clock: Clock {
                master: Arc::new(AtomicU32::new(0)),
                driver: Arc::new(Mutex::new(Driver {
                    engine,
                    hub,
                    prepared: None,
                })),
                max_block,
            },
            muted,
            target_frames,
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Creates both ends of an endpoint; the engine end is queued for the hub.
    pub fn add(&mut self, node: NodeId, capture: bool, channels: usize) -> Option<StreamPort> {
        self.collect();
        let channels = channels.clamp(1, nodeaudio_graph::MAX_CHANNELS);
        let (writer, reader) = ring(channels, self.target_frames, self.clock.max_block);
        let status = Arc::new(StreamStatus::default());
        let (engine_end, stream_end) = if capture {
            (
                Port::Capture(reader),
                StreamPort::Capture(CapturePort {
                    node,
                    writer,
                    status,
                }),
            )
        } else {
            (
                Port::Playback(writer),
                StreamPort::Playback(PlaybackPort {
                    node,
                    reader,
                    clock: self.clock.clone(),
                    status,
                }),
            )
        };
        self.commands.push(HubCommand::Add(node, engine_end)).ok()?;
        Some(stream_end)
    }

    pub fn remove(&mut self, node: NodeId) {
        self.collect();
        let _ = self.commands.push(HubCommand::Remove(node));
    }

    /// Drops retired engine ends on this (non-real-time) thread.
    pub fn collect(&mut self) {
        while self.retired.pop().is_ok() {}
    }

    /// Runs the engine directly when no stream is master, so command queues
    /// keep draining (used by the null backend and tests).
    pub fn run_unclocked(&self, frames: usize) {
        if let Ok(mut driver) = self.clock.driver.lock() {
            driver.run(frames);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodeaudio_graph::{Edge, EngineConfig, Graph, NodeKind, NodeSpec, PortRef, engine};

    #[test]
    fn master_playback_runs_engine_and_hears_capture() {
        let mut g = Graph::default();
        let mic = g.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let out = g.add_node(NodeSpec::new(NodeKind::DeviceOutput));
        g.connect(Edge {
            from: PortRef::new(mic, 0),
            to: PortRef::new(out, 0),
        })
        .unwrap();
        let config = EngineConfig {
            sample_rate: 48_000.0,
            max_block: 128,
        };
        let (mut handle, engine) = engine(config, None);
        handle.install(&g).unwrap();
        let mut control = HubControl::new(engine, 128);
        let Some(StreamPort::Capture(mut capture)) = control.add(mic, true, 2) else {
            panic!("capture port");
        };
        let Some(StreamPort::Playback(mut playback)) = control.add(out, false, 2) else {
            panic!("playback port");
        };
        control.clock().set_master(Some(out));

        let mut device_out = vec![0.0f32; 256];
        let mut heard = false;
        for _ in 0..10 {
            capture.push_interleaved(&[0.25; 256], 2);
            playback.render_interleaved(&mut device_out, 2);
            heard |= device_out.iter().any(|&s| (s - 0.25).abs() < 1e-6);
        }
        assert!(heard);

        control.set_muted(true);
        for _ in 0..4 {
            capture.push_interleaved(&[0.25; 256], 2);
            playback.render_interleaved(&mut device_out, 2);
        }
        assert!(device_out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn adapt_handles_layouts() {
        let mut stereo = [0.0; 2];
        adapt(&[0.5], &mut stereo);
        assert_eq!(stereo, [0.5, 0.5]);
        let mut mono = [0.0];
        adapt(&[0.2, 0.4], &mut mono);
        assert!((mono[0] - 0.3).abs() < 1e-6);
        let mut quad = [9.0; 4];
        adapt(&[0.1, 0.2], &mut quad);
        assert_eq!(quad, [0.1, 0.2, 0.0, 0.0]);
    }
}
