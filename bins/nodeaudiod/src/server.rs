use std::io;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use interprocess::local_socket::traits::{ListenerExt as _, Stream as _};
use nodeaudio_proto::transport::{self, RecvHalf, SendHalf};
use nodeaudio_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, read_frame, write_frame};

use crate::daemon::Inbound;

pub type ClientId = u64;

const OUTBOX: usize = 512;

/// The daemon's handle for sending to one client. Sending never blocks:
/// a client too slow to keep up is dropped.
pub struct Client {
    outbox: Sender<ServerMsg>,
}

impl Client {
    pub fn is_drained(&self) -> bool {
        self.outbox.is_empty()
    }

    pub fn send(&self, message: ServerMsg) -> bool {
        match self.outbox.try_send(message) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Accepts editor connections on `address` until the process exits.
pub fn serve(address: &str, inbox: Sender<Inbound>) -> io::Result<()> {
    let listener = transport::listen(address)?;
    std::thread::Builder::new()
        .name("ipc-accept".into())
        .spawn(move || {
            let mut next_id: ClientId = 1;
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let id = next_id;
                        next_id += 1;
                        let (recv, send) = stream.split();
                        let (outbox_tx, outbox_rx) = bounded(OUTBOX);
                        spawn_writer(id, send, outbox_rx);
                        if inbox
                            .send(Inbound::Connected(id, Client { outbox: outbox_tx }))
                            .is_err()
                        {
                            return;
                        }
                        spawn_reader(id, recv, inbox.clone());
                    }
                    Err(e) => log::warn!("accepting editor connection failed: {e}"),
                }
            }
        })?;
    Ok(())
}

fn spawn_writer(id: ClientId, mut send: SendHalf, outbox: Receiver<ServerMsg>) {
    let spawned = std::thread::Builder::new()
        .name(format!("ipc-write-{id}"))
        .spawn(move || {
            if write_frame(
                &mut send,
                &ServerMsg::Hello {
                    version: PROTOCOL_VERSION,
                },
            )
            .is_err()
            {
                return;
            }
            for message in outbox {
                if write_frame(&mut send, &message).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("cannot start writer for client {id}: {e}");
    }
}

fn spawn_reader(id: ClientId, mut recv: RecvHalf, inbox: Sender<Inbound>) {
    let spawned = std::thread::Builder::new()
        .name(format!("ipc-read-{id}"))
        .spawn(move || {
            loop {
                match read_frame::<_, ClientMsg>(&mut recv) {
                    Ok(message) => {
                        if inbox.send(Inbound::Message(id, message)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        if !e.is_disconnect() {
                            log::warn!("client {id} sent a bad frame, disconnecting: {e}");
                        }
                        let _ = inbox.send(Inbound::Disconnected(id));
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log::error!("cannot start reader for client {id}: {e}");
    }
}
