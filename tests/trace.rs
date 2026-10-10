use async_net_stack_rs::{
    device::{Device, LoopbackDevice},
    trace::{Capture, Limits, ReplayConfig, Severity, Trace, TraceDevice},
    transport::{
        tcp::{self, Segment},
        udp,
    },
};

use std::{io, time::Duration};

fn udp_packet(value: u8) -> Vec<u8> {
    let mut dev = LoopbackDevice::new();
    let mut frame = dev.alloc().unwrap();
    udp::build_ipv4(
        &mut frame,
        ([10, 0, 0, 1], 8000),
        ([10, 0, 0, 2], 9000),
        &[value],
        0,
    )
    .unwrap();

    frame.as_slice().to_vec()
}

fn u16b(n: u16, le: bool) -> [u8; 2] {
    if le {
        n.to_le_bytes()
    } else {
        n.to_be_bytes()
    }
}

fn u32b(n: u32, le: bool) -> [u8; 4] {
    if le {
        n.to_le_bytes()
    } else {
        n.to_be_bytes()
    }
}

fn pcap(le: bool, nano: bool, link: u32, records: &[(u32, Vec<u8>, u32)]) -> Vec<u8> {
    let mut b = u32b(if nano { 0xa1b23c4d } else { 0xa1b2c3d4 }, le).to_vec();
    b.extend(u16b(2, le));
    b.extend(u16b(4, le));

    for v in [0, 0, 65535, link] {
        b.extend(u32b(v, le));
    }

    for (fraction, bytes, original) in records {
        for v in [10, *fraction, bytes.len() as u32, *original] {
            b.extend(u32b(v, le));
        }

        b.extend(bytes);
    }

    b
}

fn one_pcap(packet: Vec<u8>) -> Vec<u8> {
    let len = packet.len() as u32;
    pcap(true, false, 101, &[(0, packet, len)])
}

fn block(kind: u32, mut body: Vec<u8>, le: bool) -> Vec<u8> {
    while body.len() % 4 != 0 {
        body.push(0);
    }

    let len = body.len() as u32 + 12;
    let mut b = u32b(kind, le).to_vec();
    b.extend(u32b(len, le));
    b.extend(body);
    b.extend(u32b(len, le));
    b
}

fn section(le: bool) -> Vec<u8> {
    let mut b = u32b(0x1a2b3c4d, le).to_vec();
    b.extend(u16b(1, le));
    b.extend(u16b(0, le));
    b.extend([255; 8]);
    block(0x0a0d0d0a, b, le)
}

fn idb(le: bool, resolution: u8) -> Vec<u8> {
    let mut b = u16b(101, le).to_vec();
    b.extend([0; 2]);
    b.extend(u32b(65535, le));
    b.extend(u16b(9, le));
    b.extend(u16b(1, le));
    b.extend([resolution, 0, 0, 0]);
    block(1, b, le)
}

fn epb(le: bool, id: u32, ticks: u64, packet: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [
        id,
        (ticks >> 32) as u32,
        ticks as u32,
        packet.len() as u32,
        packet.len() as u32,
    ] {
        b.extend(u32b(v, le));
    }
    b.extend(packet);
    block(6, b, le)
}

#[test]
fn classic_endian_and_timestamp_variants_preserve_originals() {
    for le in [true, false] {
        for nano in [true, false] {
            let packet = udp_packet(7);
            let b = pcap(le, nano, 101, &[(123, packet.clone(), packet.len() as u32)]);
            let capture = Capture::parse(&b, Limits::default()).unwrap();
            assert_eq!(capture.records[0].bytes, packet);
            assert_eq!(
                capture.records[0].timestamp,
                Some(Duration::new(10, if nano { 123 } else { 123000 }))
            );
            let trace = Trace::analyze(capture);
            assert_eq!(trace.conversations()[0].key.endpoints[0].port, Some(8000));
            assert_eq!(trace.packets()[0].at, Duration::ZERO);
        }
    }
}

#[test]
fn pcapng_sections_interfaces_and_binary_resolution() {
    let packet = udp_packet(1);

    let mut b = section(true);
    b.extend(idb(true, 6));
    b.extend(idb(true, 128 + 10));
    b.extend(epb(true, 1, 1024, &packet));
    b.extend(section(false));
    b.extend(idb(false, 9));
    b.extend(epb(false, 0, 2_000_000_001, &packet));

    let c = Capture::parse(&b, Limits::default()).unwrap();
    assert_eq!(c.interfaces.len(), 3);
    assert_eq!(c.records[0].interface, 1);
    assert_eq!(c.records[1].interface, 2);
    assert_eq!(c.records[0].timestamp, Some(Duration::from_secs(1)));
    assert_eq!(c.records[1].timestamp, Some(Duration::new(2, 1)));
    assert_eq!(Trace::analyze(c).conversations().len(), 2);
}

#[test]
fn missing_and_backward_times_never_reorder_records() {
    let packet = udp_packet(1);

    let mut b = section(true);
    b.extend(idb(true, 9));
    b.extend(epb(true, 0, 500, &packet));
    b.extend(epb(true, 0, 700, &packet));
    b.extend(epb(true, 0, 600, &packet));

    let mut body = (packet.len() as u32).to_le_bytes().to_vec();
    body.extend(&packet);
    b.extend(block(3, body, true));

    let trace = Trace::analyze(Capture::parse(&b, Limits::default()).unwrap());
    assert_eq!(
        trace.packets().iter().map(|p| p.record).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    assert_eq!(
        trace
            .packets()
            .iter()
            .map(|p| p.at.as_nanos())
            .collect::<Vec<_>>(),
        vec![0, 200, 200, 200]
    );

    assert_eq!(
        trace
            .diagnostics()
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count(),
        2
    );
}

#[test]
fn vlan_and_padding_are_explicit_transformations() {
    let ip = udp_packet(1);

    let mut frame = vec![0; 12];
    frame.extend([0x81, 0, 0, 7, 0x08, 0]);
    frame.extend(&ip);
    frame.extend([0; 8]);

    let c = Capture::parse(
        &pcap(true, false, 1, &[(0, frame.clone(), frame.len() as u32)]),
        Limits::default(),
    )
    .unwrap();

    let trace = Trace::analyze(c);
    assert_eq!(trace.packet_bytes(&trace.packets()[0]).unwrap(), ip);
    assert_eq!(trace.capture().records[0].bytes, frame);
    assert_eq!(trace.conversations()[0].key.vlan_tags, vec![7]);
    assert_eq!(trace.diagnostics().len(), 2);
}

#[test]
fn unsupported_truncated_and_invalid_records_remain_in_capture() {
    let ip = udp_packet(1);
    let b = pcap(
        true,
        false,
        101,
        &[
            (0, ip.clone(), 100),
            (1, vec![0x60; 40], 40),
            (2, vec![0; 20], 20),
        ],
    );

    let trace = Trace::analyze(Capture::parse(&b, Limits::default()).unwrap());
    assert_eq!(trace.capture().records.len(), 3);
    assert!(trace.packets().is_empty());
    assert_eq!(
        trace
            .diagnostics()
            .iter()
            .filter(|d| d.severity == Severity::Skipped)
            .count(),
        3
    );

    let trace = Trace::analyze(
        Capture::parse(
            &pcap(true, false, 113, &[(0, ip.clone(), ip.len() as u32)]),
            Limits::default(),
        )
        .unwrap(),
    );

    assert!(trace.diagnostics()[0].message.contains("link type"));
}

#[test]
fn invalid_transport_is_replayed_without_repair() {
    let mut ip = udp_packet(1);
    ip[28] ^= 1;
    let trace = Trace::analyze(Capture::parse(&one_pcap(ip.clone()), Limits::default()).unwrap());
    assert_eq!(trace.packets().len(), 1);
    assert_eq!(trace.packet_bytes(&trace.packets()[0]).unwrap(), ip);
    assert!(trace
        .diagnostics()
        .iter()
        .any(|d| d.message.contains("transport")));
}

#[test]
fn replay_exhaustion_lateness_and_send_ownership() {
    let packet = udp_packet(1);
    let b = pcap(
        true,
        false,
        101,
        &[
            (0, packet.clone(), packet.len() as u32),
            (1, packet.clone(), packet.len() as u32),
        ],
    );

    let trace = Trace::analyze(Capture::parse(&b, Limits::default()).unwrap());
    let mut dev = TraceDevice::new(
        trace,
        ReplayConfig {
            frames: 1,
            frame_size: 128,
        },
    )
    .unwrap();

    let mut out = Vec::new();
    dev.recv(1, &mut out).unwrap();
    let held = out.pop().unwrap();
    dev.poll_at(Duration::from_micros(5)).unwrap();
    assert_eq!(
        dev.recv(1, &mut out).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    assert!(!dev.finished());
    drop(held);
    assert_eq!(dev.recv(1, &mut out).unwrap(), 1);
    let report = dev.take_deliveries();
    assert_eq!(report[1].record, 2);
    assert_eq!(
        report[1].actual - report[1].scheduled,
        Duration::from_micros(4)
    );
    assert_eq!(
        dev.send(&mut out).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(out[0].as_slice(), packet);
    assert!(dev.poll_at(Duration::ZERO).is_err());

    drop(dev);
    assert_eq!(out[0].as_slice(), packet);
}

#[test]
fn replay_waits_for_deadline_and_limits_each_batch() {
    let packet = udp_packet(1);
    let b = pcap(
        true,
        false,
        101,
        &[
            (0, packet.clone(), packet.len() as u32),
            (10, packet.clone(), packet.len() as u32),
            (10, packet.clone(), packet.len() as u32),
        ],
    );

    let mut dev = TraceDevice::new(
        Trace::analyze(Capture::parse(&b, Limits::default()).unwrap()),
        ReplayConfig::default(),
    )
    .unwrap();

    let mut out = Vec::new();
    assert_eq!(dev.recv(20, &mut out).unwrap(), 1);
    assert_eq!(dev.recv(20, &mut out).unwrap(), 0);
    assert_eq!(dev.next_deadline(), Some(Duration::from_micros(10)));
    dev.poll_at(Duration::from_micros(10)).unwrap();

    assert_eq!(dev.recv(1, &mut out).unwrap(), 1);
    assert!(!dev.finished());
    assert_eq!(dev.recv(1, &mut out).unwrap(), 1);
    assert!(dev.finished());
    assert_eq!(dev.next_deadline(), None);
}

#[test]
fn malformed_containers_and_resource_limits_fail_explicitly() {
    let b = one_pcap(udp_packet(1));
    for length in 0..b.len() {
        if length == 24 {
            continue;
        }
        assert!(
            Capture::parse(&b[..length], Limits::default()).is_err(),
            "prefix {length}"
        );
    }

    assert!(Capture::parse(
        &b,
        Limits {
            bytes: b.len() - 1,
            ..Limits::default()
        }
    )
    .is_err());
    assert!(Capture::parse(
        &b,
        Limits {
            records: 0,
            ..Limits::default()
        }
    )
    .is_err());
    assert!(Capture::parse(
        &b,
        Limits {
            interfaces: 0,
            ..Limits::default()
        }
    )
    .is_err());

    let mut ng = section(true);
    ng.extend(idb(true, 6));
    ng.extend(epb(true, 8, 0, &udp_packet(1)));
    assert!(Capture::parse(&ng, Limits::default()).is_err());

    let mut ng = section(true);
    *ng.last_mut().unwrap() ^= 1;
    assert!(Capture::parse(&ng, Limits::default()).is_err());
}

#[test]
fn reused_tcp_tuple_splits_openings_but_not_syn_retransmission() {
    let mut dev = LoopbackDevice::new();
    let mut frame = dev.alloc().unwrap();
    let mut records = Vec::new();
    for seq in [100, 100, 200] {
        tcp::build_ipv4(
            &mut frame,
            &Segment {
                source: "10.0.0.1:8000".parse().unwrap(),
                destination: "10.0.0.2:9000".parse().unwrap(),
                sequence: seq,
                acknowledgment: 0,
                flags: tcp::SYN,
                window: 1000,
                mss: None,
                payload: &[],
            },
        )
        .unwrap();
        records.push((0, frame.as_slice().to_vec(), frame.len() as u32));
    }

    let trace = Trace::analyze(
        Capture::parse(&pcap(true, false, 101, &records), Limits::default()).unwrap(),
    );

    assert_eq!(trace.conversations().len(), 2);
    assert_eq!(trace.conversations()[0].packets, [2, 0]);
    assert_eq!(trace.conversations()[1].instance, 2);
}
