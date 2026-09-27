//! Poll-based pools of virtual IPv4 UDP endpoints over an L3 [`Device`].
//!
//! A pool multiplexes many application addresses over one TUN or simulated
//! device. Queues are bounded; `WouldBlock` means retry after polling. This is
//! best-effort UDP, not reliable streams. Wrap AF_XDP in [`crate::net::EthernetIpv4`]
//! before passing it to this L3 API. Polling also advances adapter timers.
pub mod tcp;
pub mod udp;
use crate::device::{Device, PacketBuf};
use crate::transport::udp::{build_ipv4, parse_ipv4, Datagram};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::SocketAddrV4,
    time::Duration,
};
pub use tcp::{ConnectionId, TcpConfig, TcpPool, TcpState};
pub use udp::{DatagramId, SendOptions, UdpConfig, UdpEvent, UdpFailure, UdpOutcome};
use udp::{Queued, Scheduler};

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
    pub rejected: u64,
    pub dropped: u64,
    pub events_overwritten: u64,
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
    tx: Scheduler,
    config: UdpConfig,
    events: VecDeque<UdpEvent>,
    next_id: u64,
    generation: u64,
    blocked_peers: Vec<SocketAddrV4>,
    service_work: Vec<(SocketAddrV4, u32)>,
    rx: Vec<PacketBuf>,
    capacity: usize,
    peer_capacity: usize,
    now: Duration,
    stats: PoolStats,
}
impl<D: Device> UdpPool<D> {
    pub fn new(device: D, queue_capacity: usize, peer_capacity: usize) -> io::Result<Self> {
        Self::with_config(
            device,
            UdpConfig {
                queue_capacity,
                peer_capacity,
                max_tx_peers: queue_capacity,
                per_peer_capacity: queue_capacity,
                tx_budget: queue_capacity,
                ..UdpConfig::default()
            },
        )
    }
    pub fn with_config(device: D, config: UdpConfig) -> io::Result<Self> {
        config.validate()?;
        Ok(Self {
            device,
            services: BTreeMap::new(),
            peers: BTreeMap::new(),
            tx: Scheduler::new(config.queue_capacity),
            rx: Vec::with_capacity(config.queue_capacity),
            capacity: config.queue_capacity,
            peer_capacity: config.peer_capacity,
            events: VecDeque::with_capacity(config.event_capacity),
            next_id: 1,
            generation: 0,
            blocked_peers: Vec::with_capacity(config.queue_capacity),
            service_work: Vec::with_capacity(config.service_capacity),
            config,
            now: Duration::ZERO,
            stats: PoolStats::default(),
        })
    }
    pub fn config(&self) -> UdpConfig {
        self.config
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn pop_event(&mut self) -> Option<UdpEvent> {
        self.events.pop_front()
    }
    /// Scheduling/deadline hint only: device readiness and adapter timers remain external.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.tx
            .next_deadline(self.config.fair_queue)
            .map(|at| at.max(self.now))
    }

    fn event(&mut self, event: UdpEvent) {
        if self.config.event_capacity == 0 {
            return;
        }
        if self.events.len() == self.config.event_capacity {
            self.events.pop_front();
            self.stats.events_overwritten = self.stats.events_overwritten.saturating_add(1);
        }
        self.events.push_back(event);
    }
    fn rejected(&mut self, source: SocketAddrV4, destination: SocketAddrV4, reason: UdpFailure) {
        self.stats.rejected += 1;
        self.event(UdpEvent {
            at: self.now,
            id: None,
            source,
            destination,
            generation: self.generation,
            outcome: UdpOutcome::Rejected(reason),
        });
    }
    fn finish(&mut self, q: Queued, outcome: UdpOutcome) {
        if matches!(outcome, UdpOutcome::Dropped(_)) {
            self.stats.dropped += 1;
        }
        self.event(UdpEvent {
            at: self.now,
            id: Some(q.id),
            source: q.source,
            destination: q.destination,
            generation: q.generation,
            outcome,
        });
    }
    /// Remove an endpoint and cancel its UDP-owned output. Backend-owned packets
    /// cannot be recalled by this generic API. Discovery state is invalidated.
    pub fn unbind(&mut self, address: SocketAddrV4) -> bool {
        if self.services.remove(&address).is_none() {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        self.peers.clear();
        let mut i = 0;
        while i < self.tx.queue.len() {
            if self.tx.queue[i].source == address {
                let q = self.tx.remove(i);
                self.finish(q, UdpOutcome::Dropped(UdpFailure::ConfigurationChanged));
            } else {
                i += 1;
            }
        }
        true
    }
    /// Validate and replace the complete endpoint set before mutating anything.
    /// This configures UDP only; TUN/kernel routes must be configured separately.
    pub fn replace_services(&mut self, services: &[Service]) -> io::Result<()> {
        let replacement = self.validate_services(services)?;
        if replacement != self.services {
            self.install_services(replacement);
        }
        Ok(())
    }
    fn validate_services(&self, services: &[Service]) -> io::Result<BTreeMap<SocketAddrV4, u32>> {
        if services.len() > self.config.service_capacity {
            return Err(blocked());
        }
        let mut replacement = BTreeMap::new();
        for service in services {
            validate_bind(service.address)?;
            if replacement.insert(service.address, service.id).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "duplicate endpoint",
                ));
            }
        }
        Ok(replacement)
    }
    fn install_services(&mut self, services: BTreeMap<SocketAddrV4, u32>) {
        while !self.tx.queue.is_empty() {
            let q = self.tx.remove(0);
            self.finish(q, UdpOutcome::Dropped(UdpFailure::ConfigurationChanged));
        }
        self.tx = Scheduler::new(self.config.queue_capacity);
        self.services = services;
        self.peers.clear();
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn bind(&mut self, service: Service) -> io::Result<()> {
        validate_bind(service.address)?;
        if self.services.contains_key(&service.address) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "endpoint already bound",
            ));
        }
        if self.services.len() >= self.config.service_capacity {
            return Err(blocked());
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
        self.tx.queue.len()
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
        self.send_with_options(source, destination, payload, SendOptions::default())
            .map(|_| ())
    }
    pub fn send_with_options(
        &mut self,
        source: SocketAddrV4,
        destination: SocketAddrV4,
        payload: &[u8],
        options: SendOptions,
    ) -> io::Result<DatagramId> {
        if let Err(reason) = self.admit(source, destination, options) {
            self.rejected(source, destination, reason);
            return Err(failure_error(reason));
        }
        let Some(mut frame) = self.device.alloc() else {
            self.rejected(source, destination, UdpFailure::NoBuffer);
            return Err(blocked());
        };
        if let Err(error) = build_ipv4(
            &mut frame,
            (source.ip().octets(), source.port()),
            (destination.ip().octets(), destination.port()),
            payload,
            0,
        ) {
            self.rejected(source, destination, UdpFailure::InvalidPacket);
            return Err(error);
        }
        self.enqueue(frame, source, destination, options)
    }
    fn admit(
        &mut self,
        source: SocketAddrV4,
        destination: SocketAddrV4,
        options: SendOptions,
    ) -> Result<(), UdpFailure> {
        if !self.services.contains_key(&source) {
            return Err(UdpFailure::SourceUnavailable);
        }
        if destination.port() == 0
            || destination.ip().is_unspecified()
            || destination.ip().is_multicast()
        {
            return Err(UdpFailure::InvalidDestination);
        }
        if options.deadline.is_some_and(|at| at <= self.now) {
            return Err(UdpFailure::DeadlineExpired);
        }
        self.tx.admission(destination, self.now, &self.config)
    }
    fn enqueue(
        &mut self,
        frame: PacketBuf,
        source: SocketAddrV4,
        destination: SocketAddrV4,
        options: SendOptions,
    ) -> io::Result<DatagramId> {
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("UDP IDs exhausted"))?;
        let id = DatagramId(self.next_id);
        self.next_id = next;
        let deadline = options
            .deadline
            .into_iter()
            .chain(
                self.config
                    .queue_lifetime
                    .map(|d| self.now.saturating_add(d)),
            )
            .min();
        self.tx.push(Queued {
            frame,
            id,
            source,
            destination,
            deadline,
            generation: self.generation,
        });
        Ok(id)
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
    /// Advance the queue clock and expire stale traffic without receiving packets.
    pub fn advance(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "time went backwards",
            ));
        }
        self.now = now;
        let mut i = 0;
        while i < self.tx.queue.len() {
            if self.tx.queue[i].deadline.is_some_and(|at| now >= at) {
                let q = self.tx.remove(i);
                self.finish(q, UdpOutcome::Dropped(UdpFailure::DeadlineExpired));
            } else {
                i += 1;
            }
        }
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<usize> {
        self.advance(self.now)?;
        self.blocked_peers.clear();
        let mut accepted = 0;
        for _ in 0..self.config.tx_budget {
            let Some(i) = self
                .tx
                .candidate(self.now, self.config.fair_queue, &self.blocked_peers)
            else {
                break;
            };
            let peer = self.tx.queue[i].destination;
            let bytes = self.tx.queue[i].frame.len();
            // Single-datagram submission isolates permanent failures. Device::send
            // consumes nothing on Err; WouldBlock is ordinary backpressure.
            match self
                .device
                .send(std::slice::from_mut(&mut self.tx.queue[i].frame))
            {
                Ok(1) => {
                    self.tx.accepted(peer, bytes, self.now, &self.config);
                    let q = self.tx.remove(i);
                    self.finish(q, UdpOutcome::Submitted);
                    self.stats.submitted += 1;
                    accepted += 1;
                }
                Ok(0) => self.blocked_peers.push(peer),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "device accepted beyond submitted slice",
                    ))
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.blocked_peers.push(peer),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::InvalidInput
                            | io::ErrorKind::AddrNotAvailable
                            | io::ErrorKind::NotConnected
                            | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    let q = self.tx.remove(i);
                    self.finish(q, UdpOutcome::Dropped(UdpFailure::Device(e.kind())));
                    if self.config.fair_queue {
                        self.blocked_peers.push(peer);
                    }
                }
                Err(e) => return Err(e),
            }
        }
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
        self.advance(now)?;
        self.device.poll_at(now)?;
        self.peers.retain(|_, peer| now - peer.last_seen < lease);
        self.flush()?;
        self.device.recv(budget.min(self.capacity), &mut self.rx)?;
        let count = self.rx.len();
        let mut rx = std::mem::take(&mut self.rx);
        let result = (|| {
            for mut frame in rx.drain(..) {
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
                    let mut services = std::mem::take(&mut self.service_work);
                    services.clear();
                    services.extend(self.services.iter().map(|(&a, &id)| (a, id)));
                    for &(address, service_id) in &services {
                        if service_id != id
                            || address.port() != destination.port()
                            || (address != destination && !destination.ip().is_broadcast())
                        {
                            continue;
                        }
                        let mut payload = [0; 10];
                        payload[..6].copy_from_slice(ADVERT);
                        payload[6..].copy_from_slice(&id.to_be_bytes());
                        if self.send_to(address, source, &payload).is_err() {
                            self.stats.response_drops += 1;
                        }
                    }
                    services.clear();
                    self.service_work = services;
                    continue;
                }
                if !self.services.contains_key(&destination) {
                    self.stats.unbound += 1;
                    continue;
                }
                if discovery {
                    if datagram.payload[..6] == *ADVERT {
                        if self.peers.len() < self.peer_capacity || self.peers.contains_key(&source)
                        {
                            let id =
                                u32::from_be_bytes(datagram.payload[6..10].try_into().unwrap());
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
                    if let Err(reason) = self.admit(destination, source, SendOptions::default()) {
                        self.stats.response_drops += 1;
                        self.rejected(destination, source, reason);
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
                        self.enqueue(frame, destination, source, SendOptions::default())?;
                    }
                }
            }
            Ok(count)
        })();
        rx.clear();
        self.rx = rx;
        result
    }
}
fn blocked() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "UDP pool TX queue or frame pool full",
    )
}

fn validate_bind(address: SocketAddrV4) -> io::Result<()> {
    if address.port() == 0
        || address.ip().is_unspecified()
        || address.ip().is_broadcast()
        || address.ip().is_multicast()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bind requires unicast IP and nonzero port",
        ));
    }
    Ok(())
}
fn failure_error(reason: UdpFailure) -> io::Error {
    let kind = match reason {
        UdpFailure::SourceUnavailable => io::ErrorKind::AddrNotAvailable,
        UdpFailure::InvalidDestination | UdpFailure::InvalidPacket => io::ErrorKind::InvalidInput,
        UdpFailure::DeadlineExpired => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::WouldBlock,
    };
    io::Error::new(kind, "UDP admission rejected; see failure event")
}

impl<D: Device> UdpPool<crate::net::EthernetIpv4<D>> {
    /// Coordinated static/DHCP lease application for the userspace Ethernet
    /// adapter. Every endpoint must use the new address. Failed validation leaves
    /// both layers intact; identical renewal preserves queued traffic and peers.
    pub fn configure_ipv4(
        &mut self,
        address: std::net::Ipv4Addr,
        prefix: u8,
        gateway: Option<std::net::Ipv4Addr>,
        services: &[Service],
    ) -> io::Result<()> {
        let replacement = self.validate_services(services)?;
        if services.iter().any(|s| *s.address.ip() != address) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint does not match interface address",
            ));
        }
        let changed = self.device.configure_ipv4(address, prefix, gateway)?;
        if changed || replacement != self.services {
            if !changed {
                self.device.reset_link();
            }
            self.install_services(replacement);
        }
        Ok(())
    }
    /// Lease expiry: cancel both adapter- and UDP-owned queues and unbind all
    /// endpoints. Reconfiguration is required before this adapter can send again.
    pub fn withdraw_ipv4(&mut self) {
        self.device.withdraw_ipv4();
        self.install_services(BTreeMap::new());
    }
}
