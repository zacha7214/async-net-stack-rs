use async_net_stack_rs::{
    device::{Device, PacketBuf},
    simulation::{AccessPoint, Link, SimDevice},
    transport::udp::build_ipv4,
};
use std::{io, net::Ipv4Addr, time::Duration};

fn port(ap: &AccessPoint, n: u8) -> SimDevice {
    ap.port(&[Ipv4Addr::new(10, 0, 0, n)], 8, 4).unwrap()
}
fn packet(source: &mut SimDevice, destination: [u8; 4]) -> PacketBuf {
    let mut frame = source.alloc().unwrap();
    build_ipv4(
        &mut frame,
        ([10, 0, 0, 1], 9000),
        (destination, 9000),
        b"probe",
        0,
    )
    .unwrap();

    frame
}

#[test]
fn restart_requires_association_and_preserves_unsent_ownership() {
    let mut ap = AccessPoint::default();
    let mut a = port(&ap, 1);
    let mut b = port(&ap, 2);
    let mut tx = [packet(&mut a, [10, 0, 0, 2])];
    assert_eq!(
        ap.associate(&a).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    assert_eq!(
        a.send(&mut tx).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    assert!(!tx[0].is_empty());

    ap.start();
    ap.associate(&a).unwrap();
    ap.associate(&b).unwrap();
    ap.set_link(
        &a,
        &b,
        Link {
            delay: Duration::from_secs(1),
            ..Link::default()
        },
    )
    .unwrap();

    assert_eq!(a.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert_eq!(b.queued(), 1);

    ap.stop();
    ap.stop();
    ap.start();
    assert!(!a.is_online());
    assert!(!b.is_online());

    ap.associate(&a).unwrap();
    ap.associate(&b).unwrap();
    ap.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(b.recv(8, &mut Vec::new()).unwrap(), 0);
    assert_eq!(ap.stats().disconnected_drops, 1);

    let mut tx = [packet(&mut a, [10, 0, 0, 2])];
    a.send(&mut tx).unwrap();
    ap.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(b.recv(8, &mut Vec::new()).unwrap(), 1);
}

#[test]
fn broadcast_is_scoped_and_disconnect_purges_copies_in_both_directions() {
    let mut ap = AccessPoint::default();
    let mut a = port(&ap, 1);
    let mut b = port(&ap, 2);
    let c = port(&ap, 3); // Never associated.
    let other = AccessPoint::default();
    let foreign = port(&other, 1); // Independent address space.
    ap.start();
    assert_eq!(
        ap.associate(&foreign).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );

    ap.associate(&a).unwrap();
    ap.associate(&b).unwrap();

    let mut tx = [packet(&mut a, [255; 4])];
    a.send(&mut tx).unwrap();
    assert_eq!(b.queued(), 1);
    assert_eq!(c.queued(), 0);
    assert_eq!(foreign.queued(), 0);

    let mut tx = [packet(&mut b, [10, 0, 0, 1])];
    b.send(&mut tx).unwrap();
    assert_eq!(a.queued(), 1);

    ap.disassociate(&a).unwrap();
    assert_eq!(a.queued(), 0);
    assert_eq!(b.queued(), 0);
    assert_eq!(ap.stats().disconnected_drops, 2);

    // Offline destination is a silent drop, not sender backpressure.
    let mut tx = [packet(&mut b, [10, 0, 0, 1])];
    assert_eq!(b.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert_eq!(ap.stats().disconnected_drops, 3);
}

#[test]
fn application_lifetime_controls_ap_and_received_handles_survive() {
    let mut ap = AccessPoint::default();
    let mut a = port(&ap, 1);
    let mut b = port(&ap, 2);
    ap.start();
    ap.associate(&a).unwrap();
    ap.associate(&b).unwrap();

    let mut tx = [packet(&mut a, [10, 0, 0, 2])];
    a.send(&mut tx).unwrap();
    let mut rx = Vec::new();
    b.recv(1, &mut rx).unwrap();
    drop(ap);

    assert!(!a.is_online());
    assert!(!b.is_online());
    assert_eq!(&rx[0].as_slice()[28..], b"probe");
}
