use std::time::{Duration, Instant};

use tonewire_graph::{
    DeviceRef, Edge, EngineConfig, Graph, NodeId, NodeKind, NodeSpec, PortRef, engine,
};
use tonewire_io::null::{NULL_MIC, NULL_SPEAKERS};
use tonewire_io::{NullPlatform, NullProbe, Router, StreamConfig};

fn device(id: &str) -> Option<DeviceRef> {
    Some(DeviceRef {
        id: id.into(),
        name: id.into(),
        loopback: false,
    })
}

struct Rig {
    router: Router,
    probe: NullProbe,
    mic: NodeId,
    speakers: NodeId,
    virtual_mic: NodeId,
    _handle: tonewire_graph::EngineHandle,
}

fn rig() -> Rig {
    let mut g = Graph::default();
    let mut mic = NodeSpec::new(NodeKind::DeviceInput);
    mic.device = device(NULL_MIC);
    mic.channels = 1;
    let mic = g.add_node(mic);
    let eq = g.add_node(NodeSpec::new(NodeKind::Eq));
    let mut speakers = NodeSpec::new(NodeKind::DeviceOutput);
    speakers.device = device(NULL_SPEAKERS);
    let speakers = g.add_node(speakers);
    let virtual_mic = g.add_node(NodeSpec::new(NodeKind::VirtualMic));
    for (from, to) in [(mic, eq), (eq, speakers), (eq, virtual_mic)] {
        g.connect(Edge {
            from: PortRef::new(from, 0),
            to: PortRef::new(to, 0),
        })
        .unwrap();
    }

    let config = StreamConfig {
        sample_rate: 48_000,
        block_frames: 256,
    };
    let (mut handle, engine) = engine(
        EngineConfig {
            sample_rate: 48_000.0,
            max_block: 256,
        },
        None,
    );
    handle.install(&g).unwrap();
    let (platform, probe) = NullPlatform::start(config);
    let mut router = Router::new(Box::new(platform), engine, config);
    router.apply(g.endpoints());
    Rig {
        router,
        probe,
        mic,
        speakers,
        virtual_mic,
        _handle: handle,
    }
}

fn wait_for(rig: &mut Rig, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        rig.router.tick(Instant::now());
        if done(rig) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn mic_reaches_speakers_through_the_graph() {
    let mut rig = rig();
    assert_eq!(rig.router.master(), Some(rig.speakers));
    wait_for(&mut rig, "sine at speakers", |r| {
        (r.probe.take_peak(r.speakers) - 0.25).abs() < 0.02
    });
    assert!(rig.router.running());
}

#[test]
fn unplugging_the_mic_disconnects_only_that_node() {
    let mut rig = rig();
    wait_for(&mut rig, "signal", |r| r.probe.take_peak(r.speakers) > 0.2);
    rig.probe.unplug(NULL_MIC);
    wait_for(&mut rig, "mic disconnected", |r| {
        let status = r.router.status();
        let mic = &status.iter().find(|(n, _)| *n == r.mic).unwrap().1;
        !mic.connected && mic.message.is_some()
    });
    let status = rig.router.status();
    assert!(
        status
            .iter()
            .find(|(n, _)| *n == rig.speakers)
            .unwrap()
            .1
            .connected
    );
    assert!(
        status
            .iter()
            .find(|(n, _)| *n == rig.virtual_mic)
            .unwrap()
            .1
            .connected
    );
    assert!(rig.router.running());

    rig.probe.replug_defaults();
    wait_for(&mut rig, "signal after replug", |r| {
        r.probe.take_peak(r.speakers) > 0.2
    });
}

#[test]
fn losing_the_master_hands_the_clock_to_another_output() {
    let mut rig = rig();
    wait_for(&mut rig, "signal", |r| r.probe.take_peak(r.speakers) > 0.2);
    rig.probe.unplug(NULL_SPEAKERS);
    wait_for(&mut rig, "new master", |r| {
        r.router.master() == Some(r.virtual_mic)
    });
    wait_for(&mut rig, "virtual mic signal", |r| {
        r.probe.take_peak(r.virtual_mic) > 0.2
    });
}

#[test]
fn mute_all_silences_outputs() {
    let mut rig = rig();
    wait_for(&mut rig, "signal", |r| r.probe.take_peak(r.speakers) > 0.2);
    rig.router.set_muted(true);
    std::thread::sleep(Duration::from_millis(100));
    rig.probe.take_peak(rig.speakers);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.probe.take_peak(rig.speakers), 0.0);
}

#[test]
fn follow_default_output_reopens_when_the_default_changes() {
    let mut rig = rig();
    let mut g = Graph::default();
    let mic = NodeId(41);
    let out = NodeId(42);
    g.insert_node(mic, NodeSpec::new(NodeKind::DeviceInput));
    g.insert_node(out, NodeSpec::new(NodeKind::DeviceOutput));
    g.connect(Edge {
        from: PortRef::new(mic, 0),
        to: PortRef::new(out, 0),
    })
    .unwrap();
    rig.router.apply(g.endpoints());
    assert_eq!(rig.probe.opens(out), 1);
    rig.probe.switch_default_speakers();
    wait_for(&mut rig, "reopen on new default", |r| {
        r.probe.opens(out) == 2
    });
    assert_eq!(rig.probe.opens(mic), 1);
}

#[test]
fn reapplying_unchanged_endpoints_respects_backoff() {
    let mut rig = rig();
    let mut g = Graph::default();
    let out = NodeId(43);
    g.insert_node(out, NodeSpec::new(NodeKind::DeviceOutput));
    rig.probe.fail_opens(true);
    rig.router.apply(g.endpoints());
    rig.router.apply(g.endpoints());
    rig.router.tick(Instant::now());
    assert_eq!(rig.probe.opens(out), 1);
    g.node_mut(out).unwrap().device = device(NULL_SPEAKERS);
    rig.probe.fail_opens(false);
    rig.router.apply(g.endpoints());
    assert_eq!(
        rig.probe.opens(out),
        2,
        "a changed endpoint is retried at once"
    );
}
