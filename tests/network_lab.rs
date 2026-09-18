use async_net_stack_rs::{
    api::{Action, Service, UdpPool},
    device::Device,
    simulation::{Link, Network},
    transport::udp::{build_ipv4, parse_ipv4},
};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};
fn addr(n: u8) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, n), 9000)
}
const LEASE: Duration = Duration::from_secs(1);

#[test]
fn discovery_transfer_partition_and_expiry() {
    let net = Network::default();
    let a = net.port(&[*addr(1).ip()], 16, 8).unwrap();
    let b = net.port(&[*addr(2).ip(), *addr(3).ip()], 16, 8).unwrap();
    let mut pool = UdpPool::new(b, 8, 8).unwrap();
    for n in [2, 3] {
        pool.bind(Service {
            address: addr(n),
            id: 7,
        })
        .unwrap();
    }
    net.set_link(&a, pool.device_mut(), Link::default())
        .unwrap();
    let mut client = UdpPool::new(a, 8, 8).unwrap();
    client
        .bind(Service {
            address: addr(1),
            id: 0,
        })
        .unwrap();
    client
        .discover(addr(1), SocketAddrV4::new(Ipv4Addr::BROADCAST, 9000), 7)
        .unwrap();
    client.flush().unwrap();
    pool.poll(net.now(), LEASE, 8, |_| panic!("discovery reached handler"))
        .unwrap();
    pool.flush().unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| panic!("advert reached handler"))
        .unwrap();
    assert_eq!(client.peers().count(), 2);
    for n in [2, 3] {
        client.send_to(addr(1), addr(n), b"odd").unwrap();
    }
    client.flush().unwrap();
    pool.poll(net.now(), LEASE, 8, |_| Action::Echo).unwrap();
    pool.flush().unwrap();
    let mut received = Vec::new();
    client
        .poll(net.now(), LEASE, 8, |d| {
            assert_eq!(d.payload, b"odd");
            received.push(d.source);
            Action::Ignore
        })
        .unwrap();
    assert_eq!(received, vec![addr(2), addr(3)]);
    net.set_link(
        client.device_mut(),
        pool.device_mut(),
        Link {
            partitioned: true,
            ..Link::default()
        },
    )
    .unwrap();
    client.send_to(addr(1), addr(2), b"lost").unwrap();
    client.flush().unwrap();
    assert_eq!(pool.poll(net.now(), LEASE, 8, |_| panic!()).unwrap(), 0);
    assert_eq!(net.stats().fault_drops, 1);
    net.advance(LEASE).unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(client.peers().count(), 0);
    net.set_link(client.device_mut(), pool.device_mut(), Link::default())
        .unwrap();
    client.send_to(addr(1), addr(2), b"healed").unwrap();
    client.flush().unwrap();
    assert_eq!(pool.poll(net.now(), LEASE, 8, |_| Action::Echo).unwrap(), 1);
}

#[test]
fn delayed_reorder_and_partial_send_preserve_ownership() {
    let net = Network::default();
    let mut a = net.port(&[*addr(1).ip()], 4, 4).unwrap();
    let mut b = net.port(&[*addr(2).ip()], 4, 2).unwrap();
    net.set_link(
        &a,
        &b,
        Link {
            alternating_delay: Duration::from_millis(10),
            ..Link::default()
        },
    )
    .unwrap();
    let mut tx = Vec::new();
    for n in 0..3 {
        let mut f = a.alloc().unwrap();
        build_ipv4(
            &mut f,
            (addr(1).ip().octets(), 9000),
            (addr(2).ip().octets(), 9000),
            &[n],
            0,
        )
        .unwrap();
        tx.push(f);
    }
    assert_eq!(a.send(&mut tx).unwrap(), 2);
    assert!(tx[0].is_empty());
    assert_eq!(parse_ipv4(tx[2].as_slice()).unwrap().payload, &[2]);
    let mut rx = Vec::new();
    assert_eq!(b.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(parse_ipv4(rx[0].as_slice()).unwrap().payload, &[0]);
    assert_eq!(a.send(&mut tx[2..]).unwrap(), 1);
    assert_eq!(b.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(parse_ipv4(rx[0].as_slice()).unwrap().payload, &[2]);
    net.advance(Duration::from_millis(10)).unwrap();
    assert_eq!(b.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(parse_ipv4(rx[0].as_slice()).unwrap().payload, &[1]);
    drop(a);
    drop(b);
    assert_eq!(parse_ipv4(rx[0].as_slice()).unwrap().payload, &[1]);
}

#[test]
fn malformed_udp_and_truncations_are_rejected() {
    let net = Network::default();
    let mut a = net.port(&[*addr(1).ip()], 4, 4).unwrap();
    let mut f = a.alloc().unwrap();
    build_ipv4(
        &mut f,
        (addr(1).ip().octets(), 9000),
        (addr(2).ip().octets(), 9000),
        b"hello",
        0,
    )
    .unwrap();
    for n in 0..f.len() {
        assert!(parse_ipv4(&f.as_slice()[..n]).is_err());
    }
    f.as_mut_packet()[28] ^= 1;
    assert!(parse_ipv4(f.as_slice()).is_err());
}

#[test]
fn mtu_loss_queue_bounds_and_monotonic_clock() {
    let net = Network::default();
    let a = net.port(&[*addr(1).ip()], 8, 2).unwrap();
    let mut b = net.port(&[*addr(2).ip()], 8, 2).unwrap();
    net.set_link(
        &a,
        &b,
        Link {
            mtu: 30,
            ..Link::default()
        },
    )
    .unwrap();
    let mut client = UdpPool::new(a, 2, 1).unwrap();
    client
        .bind(Service {
            address: addr(1),
            id: 0,
        })
        .unwrap();
    client.send_to(addr(1), addr(2), b"large").unwrap();
    client.send_to(addr(1), addr(2), b"large").unwrap();
    assert_eq!(
        client
            .send_to(addr(1), addr(2), b"large")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(client.flush().unwrap(), 2);
    assert_eq!(net.stats().fault_drops, 2);
    net.set_link(
        client.device_mut(),
        &b,
        Link {
            drop_every: 2,
            ..Link::default()
        },
    )
    .unwrap();
    for _ in 0..2 {
        client.send_to(addr(1), addr(2), b"ok").unwrap();
    }
    client.flush().unwrap();
    let mut rx = Vec::new();
    assert_eq!(b.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(net.stats().fault_drops, 3);
    client.poll(LEASE, LEASE, 8, |_| Action::Ignore).unwrap();
    assert!(client
        .poll(Duration::ZERO, LEASE, 8, |_| Action::Ignore)
        .is_err());
}

#[test]
fn broadcast_congestion_does_not_duplicate_other_recipients() {
    let net = Network::default();
    let mut a = net.port(&[*addr(1).ip()], 8, 2).unwrap();
    let mut b = net.port(&[*addr(2).ip()], 8, 1).unwrap();
    let mut c = net.port(&[*addr(3).ip()], 8, 2).unwrap();
    let mut tx = Vec::new();
    for n in 0..2 {
        let mut f = a.alloc().unwrap();
        build_ipv4(
            &mut f,
            (addr(1).ip().octets(), 9000),
            ([255; 4], 9000),
            &[n],
            0,
        )
        .unwrap();
        tx.push(f);
    }
    assert_eq!(a.send(&mut tx).unwrap(), 2);
    let mut rx = Vec::new();
    assert_eq!(b.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(c.recv(8, &mut rx).unwrap(), 2);
    assert_eq!(parse_ipv4(rx[0].as_slice()).unwrap().payload, &[0]);
    assert_eq!(parse_ipv4(rx[1].as_slice()).unwrap().payload, &[1]);
    assert_eq!(net.stats().queue_drops, 1);
}

#[test]
fn peer_capacity_refresh_and_response_saturation_are_observable() {
    let net = Network::default();
    let mut client = UdpPool::new(net.port(&[*addr(1).ip()], 16, 8).unwrap(), 8, 1).unwrap();
    client
        .bind(Service {
            address: addr(1),
            id: 0,
        })
        .unwrap();
    let mut worker = UdpPool::new(
        net.port(&[*addr(2).ip(), *addr(3).ip()], 16, 8).unwrap(),
        1,
        1,
    )
    .unwrap();
    for n in [2, 3] {
        worker
            .bind(Service {
                address: addr(n),
                id: 7,
            })
            .unwrap();
    }
    client
        .discover(addr(1), SocketAddrV4::new(Ipv4Addr::BROADCAST, 9000), 7)
        .unwrap();
    client.flush().unwrap();
    worker
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(worker.stats().response_drops, 1);
    worker.flush().unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(client.peers().count(), 1);
    net.advance(Duration::from_millis(500)).unwrap();
    client.discover(addr(1), addr(2), 7).unwrap();
    client.flush().unwrap();
    worker
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    worker.flush().unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(client.peers().next().unwrap().last_seen, net.now());
    client.discover(addr(1), addr(3), 7).unwrap();
    client.flush().unwrap();
    worker
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    worker.flush().unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(client.stats().peer_overflow, 1);
    net.advance(Duration::from_millis(600)).unwrap();
    client
        .poll(net.now(), LEASE, 8, |_| Action::Ignore)
        .unwrap();
    assert_eq!(client.peers().count(), 1, "refresh extended the lease");
}
