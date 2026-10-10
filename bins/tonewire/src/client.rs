use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use interprocess::local_socket::traits::Stream as _;
use tonewire_proto::transport::{RecvHalf, SendHalf, connect};
use tonewire_proto::{ClientMsg, Command, PROTOCOL_VERSION, ServerMsg, read_frame, write_frame};

const RETRY: Duration = Duration::from_secs(1);
const DAEMON_STARTUP: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Incoming {
    Connected,
    Disconnected(String),
    Message(ServerMsg),
}

/// A background connection to the daemon that reconnects on its own and
/// starts the daemon if none is running.
pub struct Connection {
    commands: Sender<Command>,
    pub incoming: Receiver<Incoming>,
}

impl Connection {
    pub fn start(address: String, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (commands_tx, commands_rx) = unbounded();
        let (incoming_tx, incoming_rx) = unbounded();
        std::thread::Builder::new()
            .name("daemon-connection".into())
            .spawn(move || run(address, commands_rx, incoming_tx, wake))
            .expect("spawn connection thread");
        Self {
            commands: commands_tx,
            incoming: incoming_rx,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

fn daemon_path() -> PathBuf {
    let exe = format!("tonewired{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&exe)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe))
}

fn launch_daemon(address: &str) -> std::io::Result<()> {
    let mut command = Process::new(daemon_path());
    if std::env::var_os("TONEWIRE_SOCKET").is_none() {
        command.args(["--socket", address]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn connect_or_launch(
    address: &str,
    launched: &mut bool,
) -> Result<interprocess::local_socket::Stream, String> {
    if let Ok(stream) = connect(address) {
        return Ok(stream);
    }
    if !*launched {
        *launched = true;
        launch_daemon(address).map_err(|e| format!("cannot start tonewired: {e}"))?;
        let deadline = Instant::now() + DAEMON_STARTUP;
        while Instant::now() < deadline {
            if let Ok(stream) = connect(address) {
                return Ok(stream);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err("the tonewire daemon is not running".into())
}

fn run(
    address: String,
    commands: Receiver<Command>,
    incoming: Sender<Incoming>,
    wake: impl Fn() + Send + Sync + 'static,
) {
    let wake = std::sync::Arc::new(wake);
    let mut launched = false;
    loop {
        let stream = match connect_or_launch(&address, &mut launched) {
            Ok(stream) => stream,
            Err(e) => {
                let _ = incoming.send(Incoming::Disconnected(e));
                wake();
                std::thread::sleep(RETRY);
                continue;
            }
        };
        let (mut recv, mut send) = stream.split();
        match handshake(&mut recv, &mut send) {
            Ok(()) => {
                let _ = incoming.send(Incoming::Connected);
                wake();
            }
            Err(e) => {
                let _ = incoming.send(Incoming::Disconnected(e));
                wake();
                std::thread::sleep(RETRY);
                continue;
            }
        }
        let reader_incoming = incoming.clone();
        let reader_wake = wake.clone();
        let reader = std::thread::spawn(move || {
            loop {
                match read_frame::<_, ServerMsg>(&mut recv) {
                    Ok(message) => {
                        if reader_incoming.send(Incoming::Message(message)).is_err() {
                            return;
                        }
                        reader_wake();
                    }
                    Err(e) => {
                        let _ = reader_incoming.send(Incoming::Disconnected(e.to_string()));
                        reader_wake();
                        return;
                    }
                }
            }
        });
        let mut next_id = 1;
        loop {
            match commands.recv_timeout(Duration::from_millis(250)) {
                Ok(command) => {
                    let message = ClientMsg::Request {
                        id: next_id,
                        command,
                    };
                    next_id += 1;
                    if write_frame(&mut send, &message).is_err() {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if reader.is_finished() {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
        drop(send);
        let _ = reader.join();
        std::thread::sleep(RETRY);
    }
}

fn handshake(recv: &mut RecvHalf, send: &mut SendHalf) -> Result<(), String> {
    write_frame(
        send,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .map_err(|e| e.to_string())?;
    match read_frame::<_, ServerMsg>(recv).map_err(|e| e.to_string())? {
        ServerMsg::Hello { version } if version == PROTOCOL_VERSION => Ok(()),
        ServerMsg::Hello { version } => Err(format!(
            "the running daemon speaks protocol {version} but this editor needs {PROTOCOL_VERSION}; restart tonewired"
        )),
        _ => Err("unexpected greeting from the daemon".into()),
    }
}
