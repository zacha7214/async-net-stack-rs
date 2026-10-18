use async_net_stack_rs::{
    device::{Device, PacketBuf},
    simulation::{
        bandwidth::{AdmissionError, Serializer},
        AccessPoint, Bandwidth, Link, Network, SimDevice,
    },
    transport::udp::build_ipv4,
};

use std::{net::Ipv4Addr, time::Duration};

fn ip(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 0, 0, n)
}

fn config(bytes: usize) -> Bandwidth {
    Bandwidth {
        bits_per_second: 8_000,
        queue_bytes: bytes,
        overhead_bytes: 0,
    }
}

fn packet(d: &mut SimDevice, target: Ipv4Addr) -> PacketBuf {
    let mut f = d.alloc().unwrap();
    build_ipv4(
        &mut f,
        (ip(1).octets(), 9000),
        (target.octets(), 9000),
        &[0; 72],
        0,
    )
    .unwrap();
    assert_eq!(f.len(), 100);

    f
}

#[test]
fn shared_fifo_serialization_and_ownership() {
    let net = Network::default();
    let mut a = net.port(&[ip(1)], 8, 8).unwrap();
    let mut b = net.port(&[ip(2)], 8, 8).unwrap();
    let mut c = net.port(&[ip(3)], 8, 8).unwrap();
    let bottleneck = net.bottleneck(config(200)).unwrap();
    for source in [&a, &b] {
        net.set_bottleneck(source, &c, Some(&bottleneck)).unwrap();
    }

    net.set_link(
        &a,
        &c,
        Link {
            delay: Duration::from_millis(10),
            ..Link::default()
        },
    )
    .unwrap();

    let mut tx = [packet(&mut a, ip(3))];
    let pointer = tx[0].as_slice().as_ptr();
    a.send(&mut tx).unwrap();

    let mut tx = [packet(&mut b, ip(3)), packet(&mut b, ip(3))];
    assert_eq!(b.send(&mut tx).unwrap(), 2);
    assert!(tx.iter().all(PacketBuf::is_empty));
    assert_eq!(net.stats().bandwidth_drops, 1);
    assert_eq!(bottleneck.stats().queued_bytes, 200);
    assert_eq!(
        bottleneck.stats().max_queue_delay,
        Duration::from_millis(100)
    );

    assert_eq!(c.next_deadline(), Some(Duration::from_millis(110)));
    net.advance(Duration::from_millis(100)).unwrap();
    assert_eq!(
        bottleneck.stats().queued_bytes,
        100,
        "propagation holds no service capacity"
    );

    let mut rx = Vec::new();
    assert_eq!(c.recv(8, &mut rx).unwrap(), 0);
    net.advance(Duration::from_millis(10)).unwrap();
    assert_eq!(c.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(
        rx[0].as_slice().as_ptr(),
        pointer,
        "unicast remains zero-copy"
    );

    net.advance(Duration::from_millis(90)).unwrap();
    assert_eq!(c.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(bottleneck.stats().queued_bytes, 0);
}

#[test]
fn rounding_overhead_and_overflow_are_explicit() {
    let mut s = Serializer::new(Bandwidth {
        bits_per_second: 3,
        queue_bytes: 10,
        overhead_bytes: 1,
    })
    .unwrap();
    assert_eq!(
        s.admit(Duration::ZERO, 1, Duration::ZERO, ()),
        Ok(Duration::new(5, 333_333_334))
    );

    let before = s.stats().queued_bytes;
    assert_eq!(
        s.admit(Duration::MAX, 1, Duration::ZERO, ()),
        Err(AdmissionError::Overflow)
    );

    assert_eq!(s.stats().queued_bytes, 0);
    assert_eq!(before, 2);
    assert!(Serializer::<()>::new(config(0)).is_none());
    assert!(Serializer::<()>::new(Bandwidth {
        bits_per_second: 0,
        ..config(10)
    })
    .is_none());
}

#[test]
fn faults_do_not_reserve_service_and_reconfiguration_preserves_deadlines() {
    let net = Network::default();
    let mut a = net.port(&[ip(1)], 8, 8).unwrap();
    let b = net.port(&[ip(2)], 8, 8).unwrap();
    let bw = net.bottleneck(config(200)).unwrap();
    net.set_bottleneck(&a, &b, Some(&bw)).unwrap();
    net.set_link(
        &a,
        &b,
        Link {
            drop_every: 2,
            ..Link::default()
        },
    )
    .unwrap();

    let mut tx = [packet(&mut a, ip(2)), packet(&mut a, ip(2))];
    a.send(&mut tx).unwrap();
    assert_eq!(bw.stats().admitted, 1);
    assert_eq!(net.stats().fault_drops, 1);

    net.set_link(
        &a,
        &b,
        Link {
            partitioned: true,
            ..Link::default()
        },
    )
    .unwrap();

    net.set_bottleneck(&a, &b, None).unwrap();
    assert_eq!(b.next_deadline(), Some(Duration::from_millis(100)));
    let other = Network::default().bottleneck(config(100)).unwrap();
    assert!(net.set_bottleneck(&a, &b, Some(&other)).is_err());
}

#[test]
fn ap_teardown_releases_service_reservations() {
    let mut ap = AccessPoint::default();
    let mut a = ap.port(&[ip(1)], 8, 8).unwrap();
    let b = ap.port(&[ip(2)], 8, 8).unwrap();
    let bw = ap.bottleneck(config(100)).unwrap();
    ap.set_bottleneck(&a, &b, Some(&bw)).unwrap();
    ap.start();
    ap.associate(&a).unwrap();
    ap.associate(&b).unwrap();

    let mut tx = [packet(&mut a, ip(2))];
    a.send(&mut tx).unwrap();
    ap.disassociate(&a).unwrap();
    assert_eq!(bw.stats().queued_bytes, 0);
    ap.associate(&a).unwrap();

    let mut tx = [packet(&mut a, ip(2))];
    a.send(&mut tx).unwrap();
    assert_eq!(b.next_deadline(), Some(Duration::from_millis(100)));
    ap.stop();

    assert_eq!(bw.stats().queued_bytes, 0);
    assert_eq!(b.queued(), 0);
    assert_eq!(ap.stats().disconnected_drops, 2);
}

#[test]
fn shaped_receiver_overflow_is_consumed_but_unshaped_backpressures() {
    let net = Network::default();
    let mut a = net.port(&[ip(1)], 8, 8).unwrap();
    let b = net.port(&[ip(2)], 8, 1).unwrap();
    let bw = net.bottleneck(config(400)).unwrap();
    net.set_bottleneck(&a, &b, Some(&bw)).unwrap();

    let mut tx = [packet(&mut a, ip(2)), packet(&mut a, ip(2))];
    assert_eq!(a.send(&mut tx).unwrap(), 2);
    assert_eq!(net.stats().queue_drops, 1);
    assert_eq!(bw.stats().admitted, 1);

    net.set_bottleneck(&a, &b, None).unwrap();
    let mut tx = [packet(&mut a, ip(2))];
    assert_eq!(a.send(&mut tx).unwrap(), 0);
    assert_eq!(tx[0].len(), 100);
}

#[test]
fn broadcast_shares_service_and_delay_can_reorder_after_serialization() {
    let net = Network::default();
    let mut a = net.port(&[ip(1)], 8, 8).unwrap();
    let mut b = net.port(&[ip(2)], 8, 8).unwrap();
    let c = net.port(&[ip(3)], 8, 8).unwrap();
    let bw = net.bottleneck(config(200)).unwrap();
    net.set_bottleneck(&a, &b, Some(&bw)).unwrap();
    net.set_bottleneck(&a, &c, Some(&bw)).unwrap();
    net.set_link(
        &a,
        &b,
        Link {
            delay: Duration::from_millis(500),
            ..Link::default()
        },
    )
    .unwrap();

    let mut tx = [packet(&mut a, Ipv4Addr::BROADCAST)];
    a.send(&mut tx).unwrap();
    assert_eq!(b.next_deadline(), Some(Duration::from_millis(600)));
    assert_eq!(c.next_deadline(), Some(Duration::from_millis(200)));
    net.advance(Duration::from_millis(200)).unwrap();
    net.set_link(&a, &b, Link::default()).unwrap();

    let mut tx = [packet(&mut a, ip(2))];
    a.send(&mut tx).unwrap();
    assert_eq!(b.next_deadline(), Some(Duration::from_millis(300)));
    net.advance(Duration::from_millis(100)).unwrap();
    assert_eq!(b.recv(8, &mut Vec::new()).unwrap(), 1);
}

#[test]
fn cancellation_preserves_other_flows_and_overflow_does_not_reserve() {
    let mut s = Serializer::new(config(300)).unwrap();
    s.admit(Duration::ZERO, 100, Duration::ZERO, 1).unwrap();
    s.admit(Duration::ZERO, 100, Duration::ZERO, 2).unwrap();
    s.cancel(|owner| *owner == 1);
    assert_eq!(s.stats().queued_bytes, 100);
    assert_eq!(
        s.admit(Duration::ZERO, 100, Duration::ZERO, 3),
        Ok(Duration::from_millis(300))
    );

    let net = Network::default();
    let mut a = net.port(&[ip(1)], 8, 8).unwrap();
    let b = net.port(&[ip(2)], 8, 8).unwrap();
    let bw = net.bottleneck(config(100)).unwrap();
    net.set_bottleneck(&a, &b, Some(&bw)).unwrap();
    net.set_link(
        &a,
        &b,
        Link {
            delay: Duration::MAX,
            ..Link::default()
        },
    )
    .unwrap();

    let mut tx = [packet(&mut a, ip(2))];
    assert_eq!(a.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert_eq!(net.stats().timing_drops, 1);
    assert_eq!(bw.stats().queued_bytes, 0);
    assert_eq!(b.next_deadline(), None);
}
