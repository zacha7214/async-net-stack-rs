//! Shared UDP discovery and TCP recovery, driven entirely by simulated time.
//! cargo run --locked --example coordinator_lab
use async_net_stack_rs::{
    api::{
        tcp::{ConnectionId, TcpConfig, TcpPool},
        Action, Service, UdpPool,
    },
    coordinator::{Coordinator, PortConfig, ProtocolPort},
    simulation::{Bandwidth, Link, Network, SimDevice},
};

use async_net_stack_rs::platform::io;
use std::{net::SocketAddrV4, time::Duration};

struct Endpoint {
    coordinator: Coordinator<SimDevice>,
    udp: UdpPool<ProtocolPort<SimDevice>>,
    tcp: TcpPool<ProtocolPort<SimDevice>>,
}

impl Endpoint {
    fn new(net: &Network, address: SocketAddrV4) -> io::Result<Self> {
        let mut coordinator = Coordinator::new(net.port(&[*address.ip()], 128, 64)?);
        let udp = UdpPool::new(coordinator.port(17, PortConfig::default())?, 32, 8)?;

        let mut sequence = 1000u32;
        let tcp = TcpPool::with_sequence_source(
            coordinator.port(6, PortConfig::default())?,
            TcpConfig::default(),
            move || {
                sequence = sequence.wrapping_add(100_000);
                Ok(sequence)
            },
        )?;

        Ok(Self {
            coordinator,
            udp,
            tcp,
        })
    }

    fn poll(&mut self, now: Duration) -> io::Result<()> {
        self.coordinator.poll(now, 64)?;
        self.udp
            .poll(now, Duration::from_secs(10), 32, |_| Action::Ignore)?;

        self.tcp.poll(now, 32)?;

        Ok(())
    }
}
fn main() -> io::Result<()> {
    run()
}
fn run() -> io::Result<()> {
    let net = Network::default();
    let client_addr: SocketAddrV4 = "10.0.0.1:9000".parse().unwrap();
    let server_addr: SocketAddrV4 = "10.0.0.2:9000".parse().unwrap();
    let mut client = Endpoint::new(&net, client_addr)?;
    let mut server = Endpoint::new(&net, server_addr)?;
    // Independent full-duplex service; UDP and TCP share each direction.
    for reverse in [false, true] {
        let bandwidth = net.bottleneck(Bandwidth {
            bits_per_second: 1_000_000,
            queue_bytes: 4096,
            overhead_bytes: 0,
        })?;
        client.coordinator.with_device(|a| {
            server.coordinator.with_device(|b| {
                if reverse {
                    net.set_bottleneck(b, a, Some(&bandwidth))
                } else {
                    net.set_bottleneck(a, b, Some(&bandwidth))
                }
            })
        })?;
    }

    client.udp.bind(Service {
        address: client_addr,
        id: 0,
    })?;

    server.udp.bind(Service {
        address: server_addr,
        id: 7,
    })?;

    server.tcp.listen(server_addr)?;

    client.udp.discover(client_addr, server_addr, 7)?;

    let mut outgoing: Option<ConnectionId> = None;
    let mut incoming = None;
    let mut sent = false;
    let mut received = Vec::new();
    for step in 0..800 {
        // A one-way partition after the handshake destroys the first data
        // transmission. Healing preserves the connection; TCP's timer retries.
        if step == 20 || step == 80 {
            let partitioned = step == 20;

            client.coordinator.with_device(|a| {
                server.coordinator.with_device(|b| {
                    net.set_link(
                        a,
                        b,
                        Link {
                            partitioned,
                            ..Link::default()
                        },
                    )
                })
            })?;

            println!("t={:?}: partition={partitioned}", net.now());
        }

        client.poll(net.now())?;
        server.poll(net.now())?;
        if outgoing.is_none() && client.udp.peers().any(|p| p.service.id == 7) {
            println!("t={:?}: UDP discovered service; opening TCP", net.now());
            outgoing = Some(client.tcp.connect(client_addr, server_addr)?);
        }

        if incoming.is_none() {
            incoming = server.tcp.accept(server_addr)?;
        }

        if step >= 25 && !sent && incoming.is_some() {
            assert_eq!(
                client
                    .tcp
                    .write(outgoing.unwrap(), b"recovered after partition")?,
                25
            );
            sent = true;
        }

        if let Some(id) = incoming {
            let mut bytes = [0; 64];
            match server.tcp.read(id, &mut bytes) {
                Ok(n) => received.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }

        if received.len() == 25 {
            assert_eq!(received, b"recovered after partition");
            assert!(client.tcp.stats().retransmitted > 0);
            assert!(net.stats().fault_drops > 0);
            println!(
                "t={:?}: recovered {} bytes; retransmissions={}, fault_drops={}",
                net.now(),
                received.len(),
                client.tcp.stats().retransmitted,
                net.stats().fault_drops
            );
            return Ok(());
        }

        net.advance(Duration::from_millis(10))?;
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "scenario failed to recover",
    ))
}

#[test]
fn discovery_and_tcp_recover_on_one_interface() {
    run().unwrap();
}
