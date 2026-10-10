use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nodeaudio_graph::{AppRule, NodeId};
use wasapi::{
    AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::w;

use super::Shared;
use super::apps;
use crate::hub::StreamPort;
use crate::platform::{PlatformEvent, StreamConfig};

const WAIT_MS: u32 = 20;
const PROCESS_RETRY: Duration = Duration::from_secs(2);

pub enum Source {
    /// A recording device, or the default one.
    Capture(Option<String>),
    /// What a playback device is playing, or the default one.
    Loopback(Option<String>),
    /// A playback device, or the default one.
    Render(Option<String>),
    /// The audio of the first running process matching the rules.
    Process(Vec<AppRule>),
}

pub struct StreamThread {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl StreamThread {
    pub fn spawn(
        node: NodeId,
        source: Source,
        port: StreamPort,
        config: StreamConfig,
        shared: Arc<Mutex<Shared>>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name(format!("wasapi-{}", node.0))
            .spawn(move || {
                let _ = initialize_mta();
                let mut task = 0u32;
                // SAFETY: registers this thread with MMCSS; reverted below.
                let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) };
                if let Err(message) = run(source, port, config, &thread_stop)
                    && !thread_stop.load(Ordering::Relaxed)
                    && let Ok(mut s) = shared.lock()
                {
                    s.events.push(PlatformEvent::StreamFailed(node, message));
                }
                if let Ok(handle) = mmcss {
                    // SAFETY: handle came from AvSetMmThreadCharacteristicsW on this thread.
                    let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn device(id: &Option<String>, direction: Direction) -> Result<wasapi::Device, String> {
    let enumerator = DeviceEnumerator::new().map_err(err)?;
    match id {
        Some(id) => enumerator.get_device(id).map_err(err),
        None => enumerator.get_default_device(&direction).map_err(err),
    }
}

fn shared_mode(client: &AudioClient) -> Result<StreamMode, String> {
    let (default_period, _) = client.get_device_period().map_err(err)?;
    Ok(StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    })
}

fn run(
    source: Source,
    port: StreamPort,
    config: StreamConfig,
    stop: &AtomicBool,
) -> Result<(), String> {
    match (source, port) {
        (Source::Render(id), StreamPort::Playback(mut port)) => {
            let device = device(&id, Direction::Render)?;
            let mut client = device.get_iaudioclient().map_err(err)?;
            let channels = port.channels();
            let format = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                config.sample_rate as usize,
                channels,
                None,
            );
            client
                .initialize_client(&format, &Direction::Render, &shared_mode(&client)?)
                .map_err(err)?;
            let event = client.set_get_eventhandle().map_err(err)?;
            let render = client.get_audiorenderclient().map_err(err)?;
            let capacity = client.get_buffer_size().map_err(err)? as usize;
            let mut samples = vec![0.0f32; capacity * channels];
            let mut bytes = vec![0u8; capacity * channels * 4];
            client.start_stream().map_err(err)?;
            while !stop.load(Ordering::Relaxed) {
                if event.wait_for_event(WAIT_MS).is_err() {
                    continue;
                }
                let frames =
                    (client.get_available_space_in_frames().map_err(err)? as usize).min(capacity);
                if frames == 0 {
                    continue;
                }
                let n = frames * channels;
                port.render_interleaved(&mut samples[..n], channels);
                for (chunk, s) in bytes[..n * 4]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(&samples[..n])
                {
                    *chunk = s.to_le_bytes();
                }
                render
                    .write_to_device(frames, &bytes[..n * 4], None)
                    .map_err(err)?;
            }
            let _ = client.stop_stream();
            Ok(())
        }
        (Source::Capture(id), StreamPort::Capture(mut port)) => {
            let device = device(&id, Direction::Capture)?;
            let client = device.get_iaudioclient().map_err(err)?;
            capture_loop(client, &mut port, config, stop)
        }
        (Source::Loopback(id), StreamPort::Capture(mut port)) => {
            let device = device(&id, Direction::Render)?;
            let client = device.get_iaudioclient().map_err(err)?;
            capture_loop(client, &mut port, config, stop)
        }
        (Source::Process(rules), StreamPort::Capture(mut port)) => {
            while !stop.load(Ordering::Relaxed) {
                let pid = apps::processes()
                    .into_iter()
                    .find(|(_, binary)| rules.iter().any(|r| r.matches(binary)))
                    .map(|(pid, _)| pid);
                match pid {
                    Some(pid) => {
                        let client =
                            AudioClient::new_application_loopback_client(pid, true).map_err(err)?;
                        if let Err(e) = capture_loop(client, &mut port, config, stop) {
                            log::info!("app capture for process {pid} ended: {e}");
                        }
                    }
                    None => {
                        let slice = Duration::from_millis(u64::from(WAIT_MS));
                        let frames = config.sample_rate as usize * WAIT_MS as usize / 1000;
                        let mut waited = Duration::ZERO;
                        while waited < PROCESS_RETRY && !stop.load(Ordering::Relaxed) {
                            port.push_silence(frames);
                            std::thread::sleep(slice);
                            waited += slice;
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Err("endpoint direction does not match its device".into()),
    }
}

fn capture_loop(
    mut client: AudioClient,
    port: &mut crate::hub::CapturePort,
    config: StreamConfig,
    stop: &AtomicBool,
) -> Result<(), String> {
    let channels = port.channels();
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        config.sample_rate as usize,
        channels,
        None,
    );
    let mode = shared_mode(&client).unwrap_or(StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    });
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(err)?;
    let event = client.set_get_eventhandle().map_err(err)?;
    let capture = client.get_audiocaptureclient().map_err(err)?;
    let capacity = client.get_buffer_size().map_err(err)? as usize;
    let mut bytes = vec![0u8; capacity.max(4096) * channels * 4];
    let mut samples = vec![0.0f32; capacity.max(4096) * channels];
    client.start_stream().map_err(err)?;
    while !stop.load(Ordering::Relaxed) {
        let _ = event.wait_for_event(WAIT_MS);
        while let Some(packet) = capture.get_next_packet_size().map_err(err)? {
            if packet == 0 {
                break;
            }
            let (frames, _info) = capture.read_from_device(&mut bytes).map_err(err)?;
            let n = (frames as usize * channels).min(samples.len());
            for (s, chunk) in samples[..n].iter_mut().zip(bytes.as_chunks::<4>().0) {
                *s = f32::from_le_bytes(*chunk);
            }
            port.push_interleaved(&samples[..n], channels);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}
