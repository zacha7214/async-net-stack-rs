//! Scripted Ethernet peer tests. These do not need Linux, privileges, or sleeps.
use async_net_stack_rs::{
    device::{Device, LoopbackDevice, PacketBuf},
    net::{
        arp,
        neighbor::NeighborState,
        route::{Route, RouteTable},
        EthernetIpv4, InterfaceConfig,
    },
    transport::udp,
};
use std::{collections::VecDeque, io, net::Ipv4Addr, time::Duration};

const LOCAL: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
const PEER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 50);
const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
const LOCAL_MAC: [u8; 6] = [2, 0, 0, 0, 0, 20];
const PEER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 50];

struct Wire {
    pool: LoopbackDevice,
    incoming: VecDeque<Vec<u8>>,
    sent: Vec<Vec<u8>>,
    send_limit: usize,
    fail_send: bool,
}

impl Wire {
    fn new(frames: usize) -> Self {
        Self {
            pool: LoopbackDevice::with_capacity(frames, frames),
            incoming: VecDeque::new(),
            sent: Vec::new(),
            send_limit: usize::MAX,
            fail_send: false,
        }
    }

    fn inject_arp(&mut self, operation: arp::Operation, ip: Ipv4Addr, mac: [u8; 6]) {
        let mut bytes = vec![0; 60];
        bytes[..6].copy_from_slice(&LOCAL_MAC);
        bytes[6..12].copy_from_slice(&mac);
        bytes[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        bytes[14..42].copy_from_slice(
            &arp::Packet {
                operation,
                sender_mac: mac,
                sender_ip: ip,
                target_mac: LOCAL_MAC,
                target_ip: LOCAL,
            }
            .encode(),
        );

        self.incoming.push_back(bytes);
    }
}

impl Device for Wire {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        while out.len() < max && !self.incoming.is_empty() {
            let Some(mut frame) = self.pool.alloc() else {
                break;
            };
            let bytes = self.incoming.pop_front().unwrap();
            frame.set_len(bytes.len());
            frame.as_mut_packet().copy_from_slice(&bytes);
            out.push(frame);
        }

        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        if self.fail_send {
            return Err(io::Error::other("scripted failure"));
        }

        let n = frames.len().min(self.send_limit);
        for frame in &mut frames[..n] {
            self.sent.push(frame.as_slice().to_vec());
            drop(std::mem::take(frame));
        }

        Ok(n)
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        self.pool.alloc()
    }

    fn frame_size(&self) -> usize {
        self.pool.frame_size()
    }

}

fn interface() -> EthernetIpv4<Wire> {
    EthernetIpv4::new(
        Wire::new(32),
        InterfaceConfig::new(
        LOCAL, 
        24,
        LOCAL_MAC)
    ).unwrap()
}

fn packet(interface: &mut EthernetIpv4<Wire>, destination: Ipv4Addr, value: u8) -> PacketBuf {
    let mut frame = interface.alloc().unwrap();
    udp::build_ipv4(
        &mut frame,
        (LOCAL.octets(), 9000),
        (destination.octets(), 9000),
        &[value],
        0,
    )
    .unwrap();

    frame
}

#[test]
fn routes_normalize_and_choose_longest_prefix() {
    let mut routes = RouteTable::new(3);
    let default = Route::new(Ipv4Addr::UNSPECIFIED, 0, Some(GATEWAY)).unwrap();
    let connected = Route::new(LOCAL, 24, None).unwrap();
    let host = Route::new(PEER, 32, Some(GATEWAY)).unwrap();
    routes.insert(default).unwrap();
    routes.insert(connected).unwrap();
    routes.insert(host).unwrap();

    assert_eq!(connected.network(), Ipv4Addr::new(192, 168, 1, 0));
    assert_eq!(routes.lookup(PEER), Some(host));
    assert_eq!(
        routes.lookup(Ipv4Addr::new(192, 168, 1, 60)),
        Some(connected)
    );

    assert_eq!(routes.lookup(Ipv4Addr::new(203, 0, 113, 1)), Some(default));
    assert!(Route::new(LOCAL, 33, None).is_err());
    assert!(routes.insert(Route::new(LOCAL, 16, None).unwrap()).is_err());
}

#[test]
fn arp_miss_queues_then_reply_releases_original_ip_packet() {
    let mut interface = interface();
    let frame = packet(&mut interface, PEER, 7);
    let original = frame.as_slice().to_vec();
    let mut tx = vec![frame];
    assert_eq!(interface.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    interface.advance(Duration::ZERO).unwrap();

    let request = &interface.device_mut().sent[0];
    assert_eq!(&request[..6], &[0xff; 6]);

    let request = arp::Packet::parse(&request[14..]).unwrap();
    assert_eq!(request.operation, arp::Operation::Request);
    assert_eq!(request.target_ip, PEER);
    interface
        .device_mut()
        .inject_arp(arp::Operation::Reply, PEER, PEER_MAC);

    let mut rx = Vec::new();
    assert_eq!(interface.recv(8, &mut rx).unwrap(), 0);
    assert_eq!(interface.pending(), 0);

    let data = &interface.device_mut().sent[1];
    assert_eq!(&data[..6], &PEER_MAC);
    assert_eq!(&data[14..14 + original.len()], &original);
    assert_eq!(data.len(), 60);
    assert!(data[14 + original.len()..].iter().all(|&b| b == 0));
}

#[test]
fn gateway_mac_does_not_replace_remote_ip_and_static_entries_do_not_learn() {
    let mut interface = interface();
    interface.set_gateway(GATEWAY).unwrap();
    interface.add_static_neighbor(GATEWAY, PEER_MAC).unwrap();

    let remote = Ipv4Addr::new(203, 0, 113, 9);
    let frame = packet(&mut interface, remote, 3);
    interface.send(&mut [frame]).unwrap();
    interface.advance(Duration::ZERO).unwrap();

    let data = &interface.device_mut().sent[0];
    assert_eq!(&data[..6], &PEER_MAC);
    assert_eq!(&data[30..34], &remote.octets());
    interface
        .device_mut()
        .inject_arp(arp::Operation::Request, GATEWAY, [2, 9, 9, 9, 9, 9]);

    interface.recv(8, &mut Vec::new()).unwrap();
    assert!(interface
        .neighbors()
        .any(|(&ip, &state)| ip == GATEWAY && state == NeighborState::Static { mac: PEER_MAC }));
}

#[test]
fn partial_acceptance_and_backend_failures_keep_packet_ownership() {
    let mut config = InterfaceConfig::new(LOCAL, 24, LOCAL_MAC);
    config.tx_capacity = 1;

    let mut interface = EthernetIpv4::new(Wire::new(8), config).unwrap();
    interface.add_static_neighbor(PEER, PEER_MAC).unwrap();
    let mut tx = vec![
        packet(&mut interface, PEER, 1),
        packet(&mut interface, PEER, 2),
    ];

    let second = tx[1].as_slice().to_vec();
    assert_eq!(interface.send(&mut tx).unwrap(), 1);
    assert!(tx[0].is_empty());
    assert_eq!(tx[1].as_slice(), second);
    interface.device_mut().fail_send = true;

    assert!(interface.advance(Duration::ZERO).is_err());
    assert_eq!(interface.pending(), 1);
    assert_eq!(interface.send(&mut tx[1..]).unwrap(), 0);
    interface.device_mut().fail_send = false;
    interface.advance(Duration::ZERO).unwrap();
    assert_eq!(interface.send(&mut tx[1..]).unwrap(), 1);

    interface.advance(Duration::ZERO).unwrap();
    assert_eq!(interface.device_mut().sent.len(), 2);
    assert_eq!(interface.device_mut().sent[0][42], 1);
    assert_eq!(interface.device_mut().sent[1][42], 2);
}

#[test]
fn invalid_batch_consumes_nothing_and_no_route_is_reported() {
    let mut interface = interface();
    let first = packet(&mut interface, PEER, 1);
    let mut bad = packet(&mut interface, PEER, 2);
    bad.as_mut_packet()[0] = 0;

    let mut tx = vec![first, bad];
    assert!(interface.send(&mut tx).is_err());
    assert!(tx.iter().all(|p| !p.is_empty()));
    assert_eq!(interface.pending(), 0);

    let mut remote = [packet(&mut interface, Ipv4Addr::new(203, 0, 113, 1), 3)];
    assert_eq!(
        interface.send(&mut remote).unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );

    assert!(!remote[0].is_empty());
    assert_eq!(interface.stats().no_route, 1);
}

#[test]
fn unresolved_peer_does_not_block_resolved_peer_and_times_out() {
    let mut interface = interface();
    interface.add_static_neighbor(GATEWAY, PEER_MAC).unwrap();
    let mut tx = vec![
        packet(&mut interface, PEER, 1),
        packet(&mut interface, GATEWAY, 2),
    ];

    interface.send(&mut tx).unwrap();
    interface.advance(Duration::ZERO).unwrap();
    assert_eq!(interface.stats().submitted_ip, 1);
    assert_eq!(interface.pending(), 1);

    interface.advance(Duration::from_secs(3)).unwrap();
    assert_eq!(interface.pending(), 0);
    assert_eq!(interface.stats().timeout_drops, 1);
    assert_eq!(interface.stats().neighbor_timeouts, 1);
    assert!(interface.advance(Duration::from_secs(2)).is_err());
}

#[test]
fn reserved_control_frame_allows_arp_when_application_exhausts_pool() {
    let mut interface =
        EthernetIpv4::new(Wire::new(2), InterfaceConfig::new(LOCAL, 24, LOCAL_MAC)).unwrap();

    let frame = packet(&mut interface, PEER, 1);
    assert!(interface.alloc().is_none());
    interface.send(&mut [frame]).unwrap();
    interface.advance(Duration::ZERO).unwrap();
    assert_eq!(interface.stats().submitted_control, 1);

    // Advancing before receiving must not reserve the final RX allocation.
    interface.advance(Duration::from_millis(1)).unwrap();
    interface
        .device_mut()
        .inject_arp(arp::Operation::Reply, PEER, PEER_MAC);

    interface.recv(1, &mut Vec::new()).unwrap();
    assert_eq!(interface.stats().submitted_ip, 1);
}

#[test]
fn receive_strips_ethernet_padding() {
    let mut interface = interface();
    let mut frame = interface.device_mut().alloc().unwrap();
    udp::build_ipv4(
        &mut frame,
        (PEER.octets(), 9000),
        (LOCAL.octets(), 9000),
        b"a",
        0,
    )
    .unwrap();

    let mut bytes = vec![0; 60];
    bytes[..6].copy_from_slice(&LOCAL_MAC);
    bytes[6..12].copy_from_slice(&PEER_MAC);
    bytes[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    bytes[14..14 + frame.len()].copy_from_slice(frame.as_slice());
    drop(frame);

    interface.device_mut().incoming.push_back(bytes);
    let mut rx = Vec::new();
    assert_eq!(interface.recv(8, &mut rx).unwrap(), 1);
    assert_eq!(rx[0].len(), 29);
    assert_eq!(udp::parse_ipv4(rx[0].as_slice()).unwrap().payload, b"a");

    interface.reset_link();
    assert_eq!(interface.pending(), 0);
}
