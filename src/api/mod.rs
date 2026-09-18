//! Poll-based pools of virtual IPv4 UDP endpoints over an L3 [`Device`].
//!
//! A pool multiplexes many application addresses over one TUN or simulated
//! device. Queues are bounded; `WouldBlock` means retry after polling. This is
//! best-effort UDP, not reliable streams. AF_XDP needs an Ethernet/neighbor
//! adapter and cannot be passed directly to this L3 API.
use crate::device::{Device, PacketBuf};
use crate::transport::udp::{build_ipv4, parse_ipv4, Datagram};
use std::{collections::BTreeMap, io, net::SocketAddrV4, time::Duration};

const QUERY: &[u8] = b"ANSP\x01\x00";
const ADVERT: &[u8] = b"ANSP\x01\x01";

#[derive(Clone, Copy, Debug)]
pub struct Service {
    pub address: SocketAddrV4,
    pub id: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub service: Service,
    pub last_seen: Duration,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PoolStats {
    pub received: u64,
    pub invalid: u64,
    pub unbound: u64,
    pub submitted: u64,
    pub response_drops: u64,
    pub peer_overflow: u64,
}

/// Returning `Echo` reuses the RX handle as a TX packet; no payload allocation.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Ignore,
    Echo,
}

pub struct UdpPool<D> {
    device: D,
    services: BTreeMap<SocketAddrV4, u32>,
    peers: BTreeMap<SocketAddrV4, Peer>,
    tx: Vec<PacketBuf>,
    rx: Vec<PacketBuf>,
    capacity: usize,
    peer_capacity: usize,
    now: Duration,
    stats: PoolStats,
}
impl<D: Device> UdpPool<D> {
    pub fn new(device: D, queue_capacity: usize, peer_capacity: usize) -> io::Result<Self> {
        if queue_capacity == 0 || peer_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "capacities must be nonzero",
            ));
        }
        Ok(Self {
            device,
            services: BTreeMap::new(),
            peers: BTreeMap::new(),
            tx: Vec::with_capacity(queue_capacity),
            rx: Vec::with_capacity(queue_capacity),
            capacity: queue_capacity,
            peer_capacity,
            now: Duration::ZERO,
            stats: PoolStats::default(),
        })
    }
    pub fn bind(&mut self, service: Service) -> io::Result<()> {
        if service.address.port() == 0
            || service.address.ip().is_unspecified()
            || service.address.ip().is_broadcast()
            || service.address.ip().is_multicast()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bind requires unicast IP and nonzero port",
            ));
        }
        if self.services.contains_key(&service.address) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "endpoint already bound",
            ));
        }
        self.services.insert(service.address, service.id);
        Ok(())
    }
    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }
    pub fn stats(&self) -> PoolStats {
        self.stats
    }
    pub fn pending(&self) -> usize {
        self.tx.len()
    }
    /// Access backend-specific progress/completion counters (e.g. io_uring).
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Queue a datagram. Success means queued, not delivered. On failure the
    /// payload remains with the caller and no partial datagram is queued.
    pub fn send_to(
        &mut self,
        source: SocketAddrV4,
        destination: SocketAddrV4,
        payload: &[u8],
    ) -> io::Result<()> {
        if !self.services.contains_key(&source) {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "source is not bound",
            ));
        }
        if self.tx.len() == self.capacity {
            return Err(blocked());
        }
        let mut frame = self.device.alloc().ok_or_else(blocked)?;
        build_ipv4(
            &mut frame,
            (source.ip().octets(), source.port()),
            (destination.ip().octets(), destination.port()),
            payload,
            0,
        )?;
        self.tx.push(frame);
        Ok(())
    }
    /// Discover a service ID at a unicast seed, or 255.255.255.255:port.
    /// Repeat queries to refresh leases; discovery is unauthenticated lab traffic.
    pub fn discover(
        &mut self,
        source: SocketAddrV4,
        seed: SocketAddrV4,
        service_id: u32,
    ) -> io::Result<()> {
        let mut payload = [0; 10];
        payload[..6].copy_from_slice(QUERY);
        payload[6..].copy_from_slice(&service_id.to_be_bytes());
        self.send_to(source, seed, &payload)
    }
    pub fn flush(&mut self) -> io::Result<usize> {
        if self.tx.is_empty() {
            return Ok(0);
        }
        let accepted = self.device.send(&mut self.tx)?;
        self.tx.drain(..accepted);
        self.stats.submitted += accepted as u64;
        Ok(accepted)
    }
    /// Advance monotonic application time, expire peers, submit pending TX,
    /// and process at most `budget` packets. Replies flush on the next call.
    /// Receives continue while TX is blocked, allowing bidirectional progress.
    /// Discovery magic is reserved and not delivered to the application.
    pub fn poll(
        &mut self,
        now: Duration,
        lease: Duration,
        budget: usize,
        mut handler: impl FnMut(Datagram<'_>) -> Action,
    ) -> io::Result<usize> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "time went backwards",
            ));
        }
        self.now = now;
        self.peers.retain(|_, peer| now - peer.last_seen < lease);
        self.flush()?;
        self.device.recv(budget.min(self.capacity), &mut self.rx)?;
        let count = self.rx.len();
        for mut frame in self.rx.drain(..) {
            let Ok(datagram) = parse_ipv4(frame.as_slice()) else {
                self.stats.invalid += 1;
                continue;
            };
            self.stats.received += 1;
            let source = datagram.source;
            let destination = datagram.destination;
            let discovery = datagram.payload.len() == 10 && datagram.payload[..4] == *b"ANSP";
            if discovery && datagram.payload[..6] == *QUERY {
                let id = u32::from_be_bytes(datagram.payload[6..10].try_into().unwrap());
                for (&address, &service_id) in &self.services {
                    if service_id != id
                        || address.port() != destination.port()
                        || (address != destination && !destination.ip().is_broadcast())
                    {
                        continue;
                    }
                    if self.tx.len() == self.capacity {
                        self.stats.response_drops += 1;
                        continue;
                    }
                    let Some(mut reply) = self.device.alloc() else {
                        self.stats.response_drops += 1;
                        continue;
                    };
                    let mut payload = [0; 10];
                    payload[..6].copy_from_slice(ADVERT);
                    payload[6..].copy_from_slice(&id.to_be_bytes());
                    build_ipv4(
                        &mut reply,
                        (address.ip().octets(), address.port()),
                        (source.ip().octets(), source.port()),
                        &payload,
                        0,
                    )?;
                    self.tx.push(reply);
                }
                continue;
            }
            if !self.services.contains_key(&destination) {
                self.stats.unbound += 1;
                continue;
            }
            if discovery {
                if datagram.payload[..6] == *ADVERT {
                    if self.peers.len() < self.peer_capacity || self.peers.contains_key(&source) {
                        let id = u32::from_be_bytes(datagram.payload[6..10].try_into().unwrap());
                        self.peers.insert(
                            source,
                            Peer {
                                service: Service {
                                    address: source,
                                    id,
                                },
                                last_seen: now,
                            },
                        );
                    } else {
                        self.stats.peer_overflow += 1;
                    }
                }
                continue;
            }
            if matches!(handler(datagram), Action::Echo) {
                if self.tx.len() == self.capacity {
                    self.stats.response_drops += 1;
                    continue;
                }
                let responder = crate::net::Responder {
                    ipv4: destination.ip().octets(),
                    mac: [0; 6],
                    udp_port: Some(destination.port()),
                };
                if responder
                    .respond(&mut frame, crate::net::LinkLayer::Ip)
                    .is_reply()
                {
                    self.tx.push(frame);
                }
            }
        }
        Ok(count)
    }
}
fn blocked() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "UDP pool TX queue or frame pool full",
    )
}
