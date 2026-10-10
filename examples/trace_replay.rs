//! cargo run --locked --example trace_replay -- capture.pcap
//! cargo run --locked --example trace_replay -- --demo
use async_net_stack_rs::{
    coordinator::{Coordinator, PortConfig},
    device::Device,
    trace::{Capture, Limits, ReplayConfig, Severity, Trace, TraceDevice},
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err("usage: trace_replay <capture.pcap|capture.pcapng|--demo>".into());
    }

    let capture = if args[0] == "--demo" {
        Capture::parse(
            include_bytes!("../tests/fixtures/replay.pcap"),
            Limits::default(),
        )?
    } else {
        Capture::read(File::open(&args[0])?, Limits::default())?
    };

    replay(capture)?;
    Ok(())
}

fn replay(capture: Capture) -> io::Result<()> {
    let trace = Trace::analyze(capture);
    println!(
        "capture: {} records, {} interfaces, {} replayable packets, {} conversations",
        trace.capture().records.len(),
        trace.capture().interfaces.len(),
        trace.packets().len(),
        trace.conversations().len()
    );

    for interface in &trace.capture().interfaces {
        println!("interface: {interface:?}");
    }

    for note in &trace.capture().notes {
        println!("capture note: {note}");
    }

    for issue in trace.diagnostics() {
        println!(
            "record {} {:?}: {}",
            issue.record, issue.severity, issue.message
        );
    }

    for (i, flow) in trace.conversations().iter().enumerate() {
        println!("conversation {i}: interface={} vlan={:?} protocol={} {:?} <-> {:?} instance={} directions={:?} SYN={} FIN={} RST={}", flow.key.interface, flow.key.vlan_tags, flow.key.protocol, flow.key.endpoints[0], flow.key.endpoints[1], flow.instance, flow.packets, flow.syn_seen, flow.fin_seen, flow.reset_seen);
    }

    let skipped = trace
        .diagnostics()
        .iter()
        .filter(|d| d.severity == Severity::Skipped)
        .count();
    let mut expected: BTreeMap<u8, VecDeque<usize>> = BTreeMap::new();
    for (i, packet) in trace.packets().iter().enumerate() {
        expected.entry(packet.protocol).or_default().push_back(i);
    }

    let target = trace.packets().len();
    let mut coordinator = Coordinator::new(TraceDevice::new(trace, ReplayConfig::default())?);

    let mut ports = Vec::new();
    for &protocol in expected.keys() {
        ports.push((protocol, coordinator.port(protocol, PortConfig::default())?));
    }

    let mut received = 0;
    let mut late = 0;
    let mut rx = Vec::new();
    while let Some(at) = coordinator.next_deadline([]) {
        coordinator.poll(at, 64)?;

        for delivery in coordinator.with_device(|d| d.take_deliveries()) {
            late += usize::from(delivery.actual != delivery.scheduled);
            println!(
                "delivery: record={} scheduled_ns={} actual_ns={} lateness_ns={}",
                delivery.record,
                delivery.scheduled.as_nanos(),
                delivery.actual.as_nanos(),
                delivery
                    .actual
                    .saturating_sub(delivery.scheduled)
                    .as_nanos()
            );
        }

        for (protocol, port) in &mut ports {
            port.recv(64, &mut rx)?;
            for frame in rx.drain(..) {
                let index = expected
                    .get_mut(protocol)
                    .unwrap()
                    .pop_front()
                    .expect("unexpected output");
                coordinator.with_device(|d| {
                    let trace = d.trace();
                    assert_eq!(
                        frame.as_slice(),
                        trace.packet_bytes(&trace.packets()[index]).unwrap(),
                        "replay bytes changed"
                    );
                });
                received += 1;
            }
        }
    }

    assert_eq!(received, target, "coordinator lost a replay packet");
    assert_eq!(coordinator.stats().malformed, 0);
    assert_eq!(coordinator.stats().unclaimed, 0);
    assert!(ports.iter().all(|(_, p)| p.stats().queue_drops == 0));
    println!("result: delivered={received}, skipped={skipped}, late={late}, coordinator_drops=0, bytes_verified=true");

    Ok(())
}

#[test]
fn demo_replays_through_coordinator() {
    replay(
        Capture::parse(
            include_bytes!("../tests/fixtures/replay.pcap"),
            Limits::default(),
        )
        .unwrap(),
    )
    .unwrap();
}
