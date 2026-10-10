use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use interprocess::local_socket::Stream;
use tonewire_graph::{DeviceRef, Edge, NodeId, NodeKind, PortRef};
use tonewire_proto::transport::connect;
use tonewire_proto::{
    ClientMsg, Command as Cmd, DaemonState, Event, GraphState, PROTOCOL_VERSION, Reply, ServerMsg,
    read_frame, write_frame,
};

struct DaemonProcess {
    child: Child,
    socket: String,
    config_dir: PathBuf,
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.config_dir);
    }
}

fn start(tag: &str, prepare: impl FnOnce(&PathBuf)) -> DaemonProcess {
    let base = std::env::temp_dir().join(format!("tonewired-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    prepare(&base);
    let socket = base.join("ipc.sock").to_string_lossy().into_owned();
    let child = Command::new(env!("CARGO_BIN_EXE_tonewired"))
        .args(["--backend", "null", "--no-tray", "--config-dir"])
        .arg(&base)
        .args(["--socket", &socket])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    DaemonProcess {
        child,
        socket,
        config_dir: base,
    }
}

struct Client {
    stream: Stream,
    next_id: u64,
    graph: Option<GraphState>,
    events: Vec<Event>,
}

impl Client {
    fn connect(daemon: &DaemonProcess) -> (Self, DaemonState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match connect(&daemon.socket) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("daemon did not come up: {e}"),
            }
        };
        let hello: ServerMsg = read_frame(&mut stream).unwrap();
        assert_eq!(
            hello,
            ServerMsg::Hello {
                version: PROTOCOL_VERSION
            }
        );
        write_frame(
            &mut stream,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let mut client = Self {
            stream,
            next_id: 1,
            graph: None,
            events: Vec::new(),
        };
        let state = client.wait_event(|e| match e {
            Event::State(s) => Some((**s).clone()),
            _ => None,
        });
        client.graph = Some(state.graph.clone());
        (client, state)
    }

    fn recv(&mut self) -> ServerMsg {
        let msg: ServerMsg = read_frame(&mut self.stream).unwrap();
        if let ServerMsg::Event(Event::GraphChanged(g)) = &msg {
            self.graph = Some((**g).clone());
        }
        msg
    }

    fn request(&mut self, command: Cmd) -> Reply {
        let id = self.next_id;
        self.next_id += 1;
        write_frame(&mut self.stream, &ClientMsg::Request { id, command }).unwrap();
        loop {
            match self.recv() {
                ServerMsg::Reply { id: got, reply } if got == id => return reply,
                ServerMsg::Event(e) => self.events.push(e),
                _ => {}
            }
        }
    }

    fn wait_event<T>(&mut self, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        if let Some(found) = self.events.iter().find_map(&mut pick) {
            self.events.clear();
            return found;
        }
        loop {
            if let ServerMsg::Event(e) = self.recv()
                && let Some(found) = pick(&e)
            {
                return found;
            }
        }
    }

    fn graph(&self) -> &tonewire_graph::Graph {
        &self.graph.as_ref().unwrap().graph
    }

    fn add(&mut self, kind: NodeKind) -> NodeId {
        assert_eq!(
            self.request(Cmd::AddNode {
                kind,
                pos: [0.0, 0.0]
            }),
            Reply::Ok
        );
        *self.graph().nodes.keys().last().unwrap()
    }
}

fn link(client: &mut Client, from: NodeId, to: NodeId) -> Reply {
    client.request(Cmd::Connect {
        edge: Edge {
            from: PortRef::new(from, 0),
            to: PortRef::new(to, 0),
        },
    })
}

fn null_device(id: &str) -> Option<DeviceRef> {
    Some(DeviceRef {
        id: id.into(),
        name: id.into(),
        loopback: false,
    })
}

#[test]
fn build_a_chain_and_receive_meters() {
    let daemon = start("chain", |_| {});
    let (mut c, state) = Client::connect(&daemon);
    assert_eq!(state.backend, "null");
    assert!(state.devices.iter().any(|d| d.id == "null.mic"));

    let mic = c.add(NodeKind::DeviceInput);
    c.request(Cmd::SetDevice {
        node: mic,
        device: null_device("null.mic"),
    });
    let eq = c.add(NodeKind::Eq);
    let out = c.add(NodeKind::DeviceOutput);
    c.request(Cmd::SetDevice {
        node: out,
        device: null_device("null.speakers"),
    });
    assert_eq!(link(&mut c, mic, eq), Reply::Ok);
    assert_eq!(link(&mut c, eq, out), Reply::Ok);
    assert!(matches!(link(&mut c, out, mic), Reply::Error(_)));

    let level = c.wait_event(|e| match e {
        Event::Meters(m) => m
            .iter()
            .find(|m| m.node == eq && m.peak > 0.2)
            .map(|m| m.peak),
        _ => None,
    });
    assert!(level < 0.3);
    let stats = c.wait_event(|e| match e {
        Event::Stats(s) => Some(s.clone()),
        _ => None,
    });
    assert!(stats.running);
}

#[test]
fn undo_reverts_and_redo_restores() {
    let daemon = start("undo", |_| {});
    let (mut c, _) = Client::connect(&daemon);
    let a = c.add(NodeKind::Gain);
    let b = c.add(NodeKind::Gain);
    link(&mut c, a, b);
    assert_eq!(c.graph().edges.len(), 1);
    assert_eq!(c.request(Cmd::Undo), Reply::Ok);
    assert!(c.graph().edges.is_empty());
    assert_eq!(c.request(Cmd::Redo), Reply::Ok);
    assert_eq!(c.graph().edges.len(), 1);

    for value in [-1.0, -2.0, -3.0] {
        c.request(Cmd::SetParam {
            node: a,
            key: "gain_db".into(),
            value,
            dragging: true,
        });
    }
    c.request(Cmd::SetParam {
        node: a,
        key: "gain_db".into(),
        value: -4.0,
        dragging: false,
    });
    assert_eq!(c.request(Cmd::Undo), Reply::Ok);
    assert_eq!(c.graph().nodes[&a].params.get("gain_db"), None);
    assert_eq!(c.request(Cmd::Redo), Reply::Ok);

    let set = |c: &mut Client, value: f32, dragging: bool| {
        c.request(Cmd::SetParam {
            node: a,
            key: "gain_db".into(),
            value,
            dragging,
        })
    };
    set(&mut c, -6.0, true);
    set(&mut c, -6.0, false);
    set(&mut c, -9.0, true);
    set(&mut c, -9.0, false);
    assert_eq!(c.request(Cmd::Undo), Reply::Ok);
    assert_eq!(c.graph().nodes[&a].params.get("gain_db"), Some(&-6.0));
}

#[test]
fn graph_survives_restart_and_corrupt_file_is_set_aside() {
    let config_dir;
    {
        let daemon = start("persist", |_| {});
        let (mut c, _) = Client::connect(&daemon);
        c.add(NodeKind::Compressor);
        assert_eq!(c.request(Cmd::Shutdown), Reply::Ok);
        config_dir = daemon.config_dir.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !config_dir.join("graph.toml").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let saved = std::fs::read_to_string(config_dir.join("graph.toml")).unwrap();
        assert!(saved.contains("compressor"));
    }

    let daemon = start("corrupt", |dir| {
        std::fs::write(dir.join("graph.toml"), "[[nodes]\nbroken").unwrap();
    });
    let (_, state) = Client::connect(&daemon);
    assert!(state.graph.graph.nodes.is_empty());
    assert!(daemon.config_dir.join("graph.toml.bad").exists());
}

#[test]
fn a_misbehaving_client_does_not_affect_others() {
    let daemon = start("garbage", |_| {});
    let (mut good, _) = Client::connect(&daemon);
    let mut bad = connect(&daemon.socket).unwrap();
    use std::io::Write;
    bad.write_all(&[8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
        .unwrap();
    drop(bad);
    std::thread::sleep(Duration::from_millis(200));
    good.add(NodeKind::Gate);
    assert_eq!(good.graph().nodes.len(), 1);
}

#[test]
fn second_daemon_on_same_socket_refuses_to_start() {
    let first = start("single", |_| {});
    let (_c, _) = Client::connect(&first);
    let status = Command::new(env!("CARGO_BIN_EXE_tonewired"))
        .args(["--backend", "null", "--no-tray", "--config-dir"])
        .arg(&first.config_dir)
        .args(["--socket", &first.socket])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}
