//! An L3 Device facade over one untagged Ethernet device. No kernel route or
//! neighbor state is consulted. Time advances explicitly, including while idle.
use super::{
    arp, checksum, ethernet as eth,
    neighbor::NeighborState,
    route::{self, Route, RouteTable},
};
use crate::device::{Device, PacketBuf};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::Ipv4Addr,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct InterfaceConfig {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub mac: [u8; 6],
    /// Maximum IPv4 datagram size, excluding Ethernet headers.
    pub mtu: usize,
    /// Shared bound across unresolved and ready data packets.
    pub tx_capacity: usize,
    pub per_neighbor_capacity: usize,
    pub neighbor_capacity: usize,
    pub route_capacity: usize,
    pub control_capacity: usize,
    pub neighbor_ttl: Duration,
    pub retry_interval: Duration,
    pub resolution_timeout: Duration,
    pub failed_retry_delay: Duration,
    pub max_arp_attempts: u32,
    /// Zero disables traces. Oldest events are overwritten when full.
    pub event_capacity: usize,
}

impl InterfaceConfig {
    pub fn new(address: Ipv4Addr, prefix_len: u8, mac: [u8; 6]) -> Self {
        Self {
            address,
            prefix_len,
            mac,
            mtu: 1500,
            tx_capacity: 128,
            per_neighbor_capacity: 16,
            neighbor_capacity: 64,
            route_capacity: 64,
            control_capacity: 8,
            neighbor_ttl: Duration::from_secs(60),
            retry_interval: Duration::from_secs(1),
            resolution_timeout: Duration::from_secs(3),
            failed_retry_delay: Duration::from_secs(1),
            max_arp_attempts: 3,
            event_capacity: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InterfaceStats {
    pub accepted_ip: u64,
    pub submitted_ip: u64,
    pub submitted_control: u64,
    pub received_ip: u64,
    pub malformed: u64,
    pub ignored: u64,
    pub queue_full: u64,
    pub no_route: u64,
    pub arp_requests_queued: u64,
    pub arp_replies_queued: u64,
    pub control_drops: u64,
    pub neighbor_timeouts: u64,
    pub timeout_drops: u64,
    pub configuration_drops: u64,
    pub address_conflicts: u64,
}

/// Observations, not claims of wire delivery or a particular failure cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceEvent {
    RouteSelected {
        destination: Ipv4Addr,
        next_hop: Ipv4Addr,
        route: Route,
    },
    QueuedForNeighbor {
        next_hop: Ipv4Addr,
    },
    NeighborResolved {
        ip: Ipv4Addr,
        mac: [u8; 6],
    },
    ArpRequestQueued {
        target: Ipv4Addr,
        attempt: u32,
    },
    NeighborTimedOut {
        ip: Ipv4Addr,
        dropped: usize,
    },
    NoRoute {
        destination: Ipv4Addr,
    },
    QueueFull,
    AddressConflict {
        mac: [u8; 6],
    },
    Submitted {
        data: usize,
        control: usize,
    },
    ConfigurationChanged {
        dropped: usize,
    },
}

struct Pending {
    next_hop: Ipv4Addr,
    frame: PacketBuf,
}

pub struct EthernetIpv4<D> {
    device: D,
    config: InterfaceConfig,
    routes: RouteTable,
    neighbors: BTreeMap<Ipv4Addr, NeighborState>,
    pending: VecDeque<Pending>,
    ready: VecDeque<PacketBuf>,
    control: VecDeque<PacketBuf>,
    // Retained before applications can allocate, so queued data cannot use
    // every available frame and prevent the first ARP request from being sent.
    control_spare: Option<PacketBuf>,
    rx: Vec<PacketBuf>,
    neighbor_work: Vec<(Ipv4Addr, NeighborState)>,
    events: VecDeque<(Duration, InterfaceEvent)>,
    now: Duration,
    stats: InterfaceStats,
}

impl<D: Device> EthernetIpv4<D> {
    /// Creates a connected route automatically. The underlying device must
    /// carry Ethernet frames; do not wrap TUN or the current L3 SimDevice.
    pub fn new(mut device: D, config: InterfaceConfig) -> io::Result<Self> {
        if config.prefix_len > 32
            || !route::unicast(config.address)
            || !eth::unicast(config.mac)
            || config.mtu < 68
            || config.mtu > u16::MAX as usize
            || config.tx_capacity == 0
            || config.per_neighbor_capacity == 0
            || config.neighbor_capacity == 0
            || config.route_capacity == 0
            || config.control_capacity == 0
            || config.max_arp_attempts == 0
            || config.neighbor_ttl.is_zero()
            || config.retry_interval.is_zero()
            || config.resolution_timeout.is_zero()
            || config.failed_retry_delay.is_zero()
        {
            return Err(invalid("invalid Ethernet IPv4 configuration"));
        }
        let connected = Route::new(config.address, config.prefix_len, None)?;
        if config.prefix_len <= 30
            && (config.address == connected.network()
                || u32::from(config.address)
                    == u32::from(connected.network()) | !route::mask(config.prefix_len))
        {
            return Err(invalid(
                "interface address is a subnet network/broadcast address",
            ));
        }
        let mut spare = device
            .alloc()
            .ok_or_else(|| blocked("no frame for ARP reserve"))?;
        prepare_empty(&mut spare)?;
        if spare.tail_capacity() < config.mtu {
            return Err(invalid("MTU exceeds device frame capacity after headroom"));
        }
        let mut routes = RouteTable::new(config.route_capacity);
        routes.insert(connected)?;
        Ok(Self {
            device,
            routes,
            neighbors: BTreeMap::new(),
            pending: VecDeque::with_capacity(config.tx_capacity),
            ready: VecDeque::with_capacity(config.tx_capacity),
            control: VecDeque::with_capacity(config.control_capacity),
            control_spare: Some(spare),
            rx: Vec::new(),
            neighbor_work: Vec::with_capacity(config.neighbor_capacity),
            events: VecDeque::with_capacity(config.event_capacity),
            config,
            now: Duration::ZERO,
            stats: InterfaceStats::default(),
        })
    }

    pub fn config(&self) -> &InterfaceConfig {
        &self.config
    }
    pub fn stats(&self) -> InterfaceStats {
        self.stats
    }
    pub fn routes(&self) -> &RouteTable {
        &self.routes
    }
    pub fn neighbors(&self) -> impl Iterator<Item = (&Ipv4Addr, &NeighborState)> {
        self.neighbors.iter()
    }
    /// Queued data and control packets, excluding device-owned in-flight TX.
    pub fn pending(&self) -> usize {
        self.pending.len() + self.ready.len() + self.control.len()
    }
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }
    pub fn pop_event(&mut self) -> Option<(Duration, InterfaceEvent)> {
        self.events.pop_front()
    }

    /// Inserts/replaces a route. Already accepted packets are discarded on a
    /// successful change so they cannot leave using obsolete routing decisions.
    /// A gateway must be on this Ethernet link; recursive resolution is absent.
    pub fn add_route(&mut self, route: Route) -> io::Result<()> {
        if route
            .gateway()
            .is_some_and(|ip| ip == self.config.address || self.broadcast(ip))
        {
            return Err(invalid("gateway must be a unicast neighbor"));
        }
        self.routes.insert(route)?;
        self.discard_queued();
        Ok(())
    }

    pub fn set_gateway(&mut self, gateway: Ipv4Addr) -> io::Result<()> {
        self.add_route(Route::new(Ipv4Addr::UNSPECIFIED, 0, Some(gateway))?)
    }

    /// Remove a route, including the connected/default route if requested.
    /// Pending frames are invalidated exactly as for add_route.
    pub fn remove_route(&mut self, address: Ipv4Addr, prefix_len: u8) -> io::Result<bool> {
        let removed = self.routes.remove(address, prefix_len)?;
        if removed {
            self.discard_queued();
        }
        Ok(removed)
    }

    /// Forget a static or learned mapping. Queued frames may have its old MAC,
    /// so invalidate queues; a subsequent send starts fresh ARP resolution.
    pub fn remove_neighbor(&mut self, address: Ipv4Addr) -> bool {
        let removed = self.neighbors.remove(&address).is_some();
        if removed {
            self.discard_queued();
        }
        removed
    }

    /// Static neighbors never expire and cannot be overwritten by ARP.
    pub fn add_static_neighbor(&mut self, ip: Ipv4Addr, mac: [u8; 6]) -> io::Result<()> {
        if !route::unicast(ip)
            || self.broadcast(ip)
            || ip == self.config.address
            || !eth::unicast(mac)
        {
            return Err(invalid("invalid static neighbor"));
        }
        if !self.neighbors.contains_key(&ip)
            && self.neighbors.len() == self.config.neighbor_capacity
        {
            return Err(blocked("neighbor table full"));
        }
        // A previously framed packet might contain the old MAC.
        if self
            .neighbors
            .get(&ip)
            .and_then(|n| match *n {
                NeighborState::Static { mac } | NeighborState::Reachable { mac, .. } => Some(mac),
                _ => None,
            })
            .is_some_and(|old| old != mac)
        {
            self.discard_queued();
        }
        self.cancel_probes(ip);
        self.neighbors.insert(ip, NeighborState::Static { mac });
        self.event(InterfaceEvent::NeighborResolved { ip, mac });
        self.release_resolved();
        Ok(())
    }

    /// Call after a link reset/reassociation. Static configuration survives;
    /// queued packets and learned neighbors do not. Device-owned TX cannot be revoked.
    pub fn reset_link(&mut self) {
        self.discard_queued();
        self.neighbors
            .retain(|_, n| matches!(n, NeighborState::Static { .. }));
    }

    /// Drive expiration, ARP probes and bounded TX work, even while idle.
    /// RX is driven by `recv` (normally through UdpPool::poll), not this method.
    /// Backend errors are returned here/through recv, after earlier acceptance.
    pub fn advance(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(invalid("time went backwards"));
        }
        self.now = now;
        self.ensure_spare();
        let mut work = std::mem::take(&mut self.neighbor_work);
        work.clear();
        work.extend(self.neighbors.iter().map(|(&ip, &state)| (ip, state)));
        for &(ip, state) in &work {
            match state {
                NeighborState::Reachable { expires, .. } if now >= expires => {
                    self.neighbors.remove(&ip);
                }
                NeighborState::Failed { retry_after } if now >= retry_after => {
                    self.neighbors.remove(&ip);
                }
                NeighborState::Resolving { expires, .. } if now >= expires => {
                    self.neighbors.insert(
                        ip,
                        NeighborState::Failed {
                            retry_after: now.saturating_add(self.config.failed_retry_delay),
                        },
                    );
                    let before = self.pending.len();
                    self.pending.retain(|p| p.next_hop != ip);
                    let dropped = before - self.pending.len();
                    self.cancel_probes(ip);
                    self.stats.timeout_drops += dropped as u64;
                    self.stats.neighbor_timeouts += 1;
                    self.event(InterfaceEvent::NeighborTimedOut { ip, dropped });
                }
                NeighborState::Resolving {
                    attempts,
                    next_probe,
                    expires,
                } if now >= next_probe && attempts < self.config.max_arp_attempts => {
                    let request = arp::Packet {
                        operation: arp::Operation::Request,
                        sender_mac: self.config.mac,
                        sender_ip: self.config.address,
                        target_mac: [0; 6],
                        target_ip: ip,
                    };
                    if self.queue_arp(request, eth::BROADCAST) {
                        let attempt = attempts + 1;
                        self.neighbors.insert(
                            ip,
                            NeighborState::Resolving {
                                attempts: attempt,
                                next_probe: now.saturating_add(self.config.retry_interval),
                                expires,
                            },
                        );
                        self.stats.arp_requests_queued += 1;
                        self.event(InterfaceEvent::ArpRequestQueued {
                            target: ip,
                            attempt,
                        });
                    }
                }
                _ => {}
            }
        }
        work.clear();
        self.neighbor_work = work;
        self.release_resolved();
        self.flush()
    }

    /// Earliest timer deadline. Ready TX still requires readiness/progress;
    /// this is not a substitute for polling the device while work is pending.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.neighbors
            .values()
            .filter_map(|n| match *n {
                NeighborState::Static { .. } => None,
                NeighborState::Reachable { expires, .. } => Some(expires),
                NeighborState::Failed { retry_after } => Some(retry_after),
                NeighborState::Resolving {
                    attempts,
                    next_probe,
                    expires,
                } => Some(if attempts < self.config.max_arp_attempts {
                    next_probe.min(expires)
                } else {
                    expires
                }),
            })
            .min()
    }

    fn event(&mut self, event: InterfaceEvent) {
        if self.config.event_capacity == 0 {
            return;
        }
        if self.events.len() == self.config.event_capacity {
            self.events.pop_front();
        }
        self.events.push_back((self.now, event));
    }

    fn broadcast(&self, ip: Ipv4Addr) -> bool {
        ip.is_broadcast()
            || (self.config.prefix_len <= 30
                && u32::from(ip)
                    == (u32::from(self.config.address) & route::mask(self.config.prefix_len))
                        | !route::mask(self.config.prefix_len))
    }

    fn discard_queued(&mut self) {
        let dropped = self.pending.len() + self.ready.len();
        self.pending.clear();
        self.ready.clear();
        self.control.clear();
        self.neighbors
            .retain(|_, n| !matches!(n, NeighborState::Resolving { .. }));
        self.stats.configuration_drops += dropped as u64;
        self.event(InterfaceEvent::ConfigurationChanged { dropped });
    }

    fn ensure_spare(&mut self) {
        if self.control_spare.is_none() {
            if let Some(mut frame) = self.device.alloc() {
                if prepare_empty(&mut frame).is_ok() {
                    self.control_spare = Some(frame);
                }
            }
        }
    }

    fn queue_arp(&mut self, packet: arp::Packet, destination: [u8; 6]) -> bool {
        if self.control.len() == self.config.control_capacity {
            return false;
        }
        self.ensure_spare();
        let Some(mut frame) = self.control_spare.take() else {
            return false;
        };
        frame.set_len(arp::LEN);
        frame.as_mut_packet().copy_from_slice(&packet.encode());
        eth::prepend(&mut frame, self.config.mac, destination, eth::ARP);
        self.control.push_back(frame);
        true
    }

    fn release_resolved(&mut self) {
        for _ in 0..self.pending.len() {
            let mut pending = self.pending.pop_front().unwrap();
            if let Some(mac) = self
                .neighbors
                .get(&pending.next_hop)
                .and_then(|n| n.mac(self.now))
            {
                eth::prepend(&mut pending.frame, self.config.mac, mac, eth::IPV4);
                self.ready.push_back(pending.frame);
            } else {
                self.pending.push_back(pending);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let control = flush_front(&mut self.device, &mut self.control)?;
        self.stats.submitted_control += control as u64;
        // Do not report a data failure as an all-or-nothing control failure.
        if control > 0 {
            self.event(InterfaceEvent::Submitted { data: 0, control });
        }
        if !self.control.is_empty() {
            return Ok(());
        }
        let data = flush_front(&mut self.device, &mut self.ready)?;
        self.stats.submitted_ip += data as u64;
        if data > 0 {
            self.event(InterfaceEvent::Submitted { data, control: 0 });
        }
        Ok(())
    }

    fn cancel_probes(&mut self, ip: Ipv4Addr) {
        // A blocked backend may still have retries in our control queue.
        self.control.retain(|frame| {
            !arp::Packet::parse(&frame.as_slice()[eth::HEADER..])
                .is_some_and(|p| p.operation == arp::Operation::Request && p.target_ip == ip)
        });
    }

    fn learn(&mut self, ip: Ipv4Addr, mac: [u8; 6]) {
        if matches!(self.neighbors.get(&ip), Some(NeighborState::Static { .. })) {
            return;
        }
        if !self.neighbors.contains_key(&ip)
            && self.neighbors.len() == self.config.neighbor_capacity
        {
            return;
        }
        self.cancel_probes(ip);
        self.neighbors.insert(
            ip,
            NeighborState::Reachable {
                mac,
                expires: self.now.saturating_add(self.config.neighbor_ttl),
            },
        );
        self.event(InterfaceEvent::NeighborResolved { ip, mac });
    }

    fn receive_arp(&mut self, bytes: &[u8], source_mac: [u8; 6]) {
        let Some(packet) = arp::Packet::parse(bytes) else {
            self.stats.malformed += 1;
            return;
        };
        if packet.sender_mac != source_mac {
            self.stats.malformed += 1;
            return;
        }
        if packet.sender_ip == self.config.address && packet.sender_mac != self.config.mac {
            self.stats.address_conflicts += 1;
            self.event(InterfaceEvent::AddressConflict {
                mac: packet.sender_mac,
            });
            return;
        }
        if packet.target_ip != self.config.address {
            self.stats.ignored += 1;
            return;
        }
        match packet.operation {
            arp::Operation::Request => {
                // A zero sender IP is an address-conflict probe: answer but do not learn it.
                if route::unicast(packet.sender_ip) && !self.broadcast(packet.sender_ip) {
                    self.learn(packet.sender_ip, packet.sender_mac);
                } else if !packet.sender_ip.is_unspecified() {
                    self.stats.ignored += 1;
                    return;
                }
                let reply = arp::Packet {
                    operation: arp::Operation::Reply,
                    sender_mac: self.config.mac,
                    sender_ip: self.config.address,
                    target_mac: packet.sender_mac,
                    target_ip: packet.sender_ip,
                };
                if self.queue_arp(reply, packet.sender_mac) {
                    self.stats.arp_replies_queued += 1;
                } else {
                    self.stats.control_drops += 1;
                }
            }
            arp::Operation::Reply => {
                if packet.target_mac == self.config.mac
                    && route::unicast(packet.sender_ip)
                    && !self.broadcast(packet.sender_ip)
                    && matches!(self.neighbors.get(&packet.sender_ip),
                        Some(NeighborState::Resolving { expires, .. }) if self.now < *expires)
                {
                    self.learn(packet.sender_ip, packet.sender_mac);
                } else {
                    self.stats.ignored += 1;
                }
            }
        }
    }

    fn receive_frame(&mut self, mut frame: PacketBuf, out: &mut Vec<PacketBuf>) {
        let Some((destination_mac, source_mac, kind)) = eth::parse(frame.as_slice()) else {
            self.stats.malformed += 1;
            return;
        };
        if !eth::unicast(source_mac)
            || (destination_mac != self.config.mac && destination_mac != eth::BROADCAST)
        {
            self.stats.ignored += 1;
            return;
        }
        match kind {
            eth::ARP => self.receive_arp(&frame.as_slice()[eth::HEADER..], source_mac),
            eth::IPV4 => {
                let bytes = &frame.as_slice()[eth::HEADER..];
                let Some((_, destination, total)) = ipv4(bytes) else {
                    self.stats.malformed += 1;
                    return;
                };
                if total > self.config.mtu
                    || (destination != self.config.address && !self.broadcast(destination))
                {
                    self.stats.ignored += 1;
                    return;
                }
                frame.pull_header(eth::HEADER);
                frame.set_len(total); // Ethernet padding is not part of the IP datagram.
                self.stats.received_ip += 1;
                out.push(frame);
            }
            // Tagged traffic is deliberately excluded: one adapter is one untagged link.
            _ => self.stats.ignored += 1,
        }
    }
}

impl<D: Device> Device for EthernetIpv4<D> {
    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        self.device.poll_at(now)?;
        self.advance(now)
    }

    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        if max == 0 {
            self.advance(self.now)?;
            return Ok(0);
        }
        // Let a copying device use the reserved frame for RX too. Otherwise
        // replenishing the ARP reserve before recv could hold the final free
        // frame while a reply that would release queued data waits for a buffer.
        drop(self.control_spare.take());
        // Receive before flushing: an ARP reply must still be processed when TX is full.
        self.device.recv(max, &mut self.rx)?;
        let mut rx = std::mem::take(&mut self.rx);
        for frame in rx.drain(..) {
            self.receive_frame(frame, out);
        }
        self.rx = rx;
        self.advance(self.now)?;
        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let available = self.config.tx_capacity - self.pending.len() - self.ready.len();
        let limit = available.min(frames.len());
        // Validate the prospective accepted prefix before changing any caller-owned frame.
        for frame in &frames[..limit] {
            let (source, destination, total) =
                ipv4(frame.as_slice()).ok_or_else(|| invalid("invalid IPv4 packet"))?;
            if source != self.config.address
                || destination == self.config.address
                || (!route::unicast(destination) && !self.broadcast(destination))
                || total != frame.len()
                || total > self.config.mtu
                || frame.data_offset() < eth::HEADER
                || frame.tail_capacity() + eth::HEADER < 60
            {
                return Err(invalid(
                    "unsupported source, destination, MTU or Ethernet headroom",
                ));
            }
            if !self.broadcast(destination) {
                let Some(route) = self.routes.lookup(destination) else {
                    self.stats.no_route += 1;
                    self.event(InterfaceEvent::NoRoute { destination });
                    return Err(io::Error::new(io::ErrorKind::NotConnected, "no IPv4 route"));
                };
                let hop = route.next_hop(destination);
                if hop == self.config.address || self.broadcast(hop) || !route::unicast(hop) {
                    return Err(invalid("route has an invalid next hop"));
                }
                if matches!(self.neighbors.get(&hop), Some(NeighborState::Failed { retry_after }) if self.now < *retry_after)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "ARP resolution recently failed",
                    ));
                }
            }
        }
        let mut accepted = 0;
        for slot in &mut frames[..limit] {
            // The complete prospective prefix was validated above.
            let destination =
                Ipv4Addr::from(<[u8; 4]>::try_from(&slot.as_slice()[16..20]).unwrap());
            if self.broadcast(destination) {
                let mut frame = std::mem::take(slot);
                eth::prepend(&mut frame, self.config.mac, eth::BROADCAST, eth::IPV4);
                self.ready.push_back(frame);
            } else {
                let route = self.routes.lookup(destination).unwrap();
                let next_hop = route.next_hop(destination);
                let mac = self.neighbors.get(&next_hop).and_then(|n| n.mac(self.now));
                if mac.is_none() {
                    if self
                        .pending
                        .iter()
                        .filter(|p| p.next_hop == next_hop)
                        .count()
                        >= self.config.per_neighbor_capacity
                        || (!self.neighbors.contains_key(&next_hop)
                            && self.neighbors.len() == self.config.neighbor_capacity)
                    {
                        break;
                    }
                    if !matches!(
                        self.neighbors.get(&next_hop),
                        Some(NeighborState::Resolving { .. })
                    ) {
                        self.neighbors.insert(
                            next_hop,
                            NeighborState::Resolving {
                                attempts: 0,
                                next_probe: self.now,
                                expires: self.now.saturating_add(self.config.resolution_timeout),
                            },
                        );
                    }
                }
                self.event(InterfaceEvent::RouteSelected {
                    destination,
                    next_hop,
                    route,
                });
                let mut frame = std::mem::take(slot);
                if let Some(mac) = mac {
                    eth::prepend(&mut frame, self.config.mac, mac, eth::IPV4);
                    self.ready.push_back(frame);
                } else {
                    self.pending.push_back(Pending { next_hop, frame });
                    self.event(InterfaceEvent::QueuedForNeighbor { next_hop });
                }
            }
            accepted += 1;
        }
        if accepted < frames.len() {
            self.stats.queue_full += 1;
            self.event(InterfaceEvent::QueueFull);
        }
        self.stats.accepted_ip += accepted as u64;
        // No fallible backend work after ownership transfer. advance/recv submits later.
        Ok(accepted)
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        self.ensure_spare();
        self.control_spare.as_ref()?;
        let mut frame = self.device.alloc()?;
        prepare_empty(&mut frame).ok()?;
        Some(frame)
    }

    fn frame_size(&self) -> usize {
        self.device.frame_size()
    }
}

fn prepare_empty(frame: &mut PacketBuf) -> io::Result<()> {
    frame.set_len(0);
    let headroom = frame.data_offset().max(eth::HEADER);
    if headroom > frame.capacity() || frame.capacity() - headroom < 46 {
        return Err(invalid(
            "frame cannot hold Ethernet headers and minimum padding",
        ));
    }
    frame.set_headroom(headroom);
    Ok(())
}

fn flush_front<D: Device>(device: &mut D, queue: &mut VecDeque<PacketBuf>) -> io::Result<usize> {
    if queue.is_empty() {
        return Ok(0);
    }
    let (front, _) = queue.as_mut_slices();
    let accepted = match device.send(front) {
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
        result => result?,
    };
    queue.drain(..accepted);
    Ok(accepted)
}

/// Match the existing UDP/responder subset: no IPv4 options or fragments.
fn ipv4(bytes: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, usize)> {
    if bytes.len() < 20 || bytes[0] != 0x45 || bytes[8] == 0 {
        return None;
    }
    let total = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    if total < 20
        || total > bytes.len()
        || checksum(&bytes[..20]) != 0
        || u16::from_be_bytes([bytes[6], bytes[7]]) & !0x4000 != 0
    {
        return None;
    }
    Some((
        Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[12..16]).ok()?),
        Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[16..20]).ok()?),
        total,
    ))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn blocked(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}
