use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("connection error: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
}

impl FrameError {
    pub fn is_disconnect(&self) -> bool {
        matches!(self, Self::Io(e) if matches!(
            e.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        ))
    }
}

/// Writes a little-endian `u32` length followed by the postcard body.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<(), FrameError> {
    let body = postcard::to_stdvec(message)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, FrameError> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::*;
    use tonewire_graph::{Edge, NodeId, NodeKind, NodeSpec, PortRef};

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(message: T) {
        let mut wire = Vec::new();
        write_frame(&mut wire, &message).unwrap();
        let back: T = read_frame(&mut wire.as_slice()).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn messages_round_trip() {
        let mut graph = tonewire_graph::Graph::default();
        let a = graph.add_node(NodeSpec::new(NodeKind::DeviceInput));
        let b = graph.add_node(NodeSpec::new(NodeKind::Mixer { inputs: 3 }));
        graph
            .connect(Edge {
                from: PortRef::new(a, 0),
                to: PortRef::new(b, 2),
            })
            .unwrap();
        round_trip(ClientMsg::Hello {
            version: PROTOCOL_VERSION,
        });
        round_trip(ClientMsg::Request {
            id: 7,
            command: Command::SetParam {
                node: NodeId(3),
                key: "gain_db".into(),
                value: -3.0,
                dragging: true,
            },
        });
        round_trip(ServerMsg::Event(Event::GraphChanged(Box::new(
            GraphState {
                graph,
                can_undo: true,
                can_redo: false,
                muted: false,
            },
        ))));
        round_trip(ServerMsg::Reply {
            id: 1,
            reply: Reply::Error("nope".into()),
        });
    }

    #[test]
    fn oversized_length_is_rejected_before_allocating() {
        let wire = u32::MAX.to_le_bytes();
        let result: Result<ClientMsg, _> = read_frame(&mut wire.as_slice());
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn truncated_frame_is_a_disconnect() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &ClientMsg::Hello { version: 1 }).unwrap();
        wire.pop();
        let result: Result<ClientMsg, _> = read_frame(&mut wire.as_slice());
        assert!(result.unwrap_err().is_disconnect());
    }

    #[test]
    fn garbage_body_is_a_decode_error() {
        let mut wire = 4u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        let result: Result<ClientMsg, _> = read_frame(&mut wire.as_slice());
        assert!(matches!(result, Err(FrameError::Decode(_))));
    }
}
