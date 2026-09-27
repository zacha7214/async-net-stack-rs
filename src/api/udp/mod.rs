//! UDP admission, scheduling and observable local outcomes.
use crate::device::PacketBuf;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::SocketAddrV4,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatagramId(pub u64);

#[derive(Clone, Copy, Debug)]
pub struct UdpConfig {
    pub queue_capacity: usize,
    pub peer_capacity: usize,
    pub service_capacity: usize,
    /// Bounds scheduling state, independently of discovered peers.
    pub max_tx_peers: usize,
    pub per_peer_capacity: usize,
    pub tx_budget: usize,
    /// Packet round robin between destination IP:port pairs; FIFO within a peer.
    pub fair_queue: bool,
    /// IPv4 bytes/sec, including IP/UDP headers. None disables pacing.
    /// Pacing is not adaptive congestion control. First packet may send immediately.
    pub bytes_per_second: Option<u64>,
    pub per_peer_bytes_per_second: Option<u64>,
    pub queue_lifetime: Option<Duration>,
    /// Zero disables events; overflow overwrites oldest with a counter.
    pub event_capacity: usize,
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 128,
            peer_capacity: 64,
            service_capacity: 256,
            max_tx_peers: 128,
            per_peer_capacity: 128,
            tx_budget: 128,
            fair_queue: false,
            bytes_per_second: None,
            per_peer_bytes_per_second: None,
            queue_lifetime: None,
            event_capacity: 0,
        }
    }
}

impl UdpConfig {
    pub(super) fn validate(&self) -> io::Result<()> {
        if self.queue_capacity == 0
            || self.peer_capacity == 0
            || self.service_capacity == 0
            || self.max_tx_peers == 0
            || self.per_peer_capacity == 0
            || self.tx_budget == 0
            || self.bytes_per_second == Some(0)
            || self.per_peer_bytes_per_second == Some(0)
            || self.queue_lifetime.is_some_and(|d| d.is_zero())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid UDP configuration",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SendOptions {
    /// Absolute time in the same monotonic domain as poll/advance. Combined with
    /// the configured lifetime using the earlier deadline. Stops at device handoff.
    pub deadline: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpFailure {
    SourceUnavailable,
    InvalidDestination,
    QueueFull,
    PeerQueueFull,
    PeerLimit,
    NoBuffer,
    InvalidPacket,
    DeadlineExpired,
    ConfigurationChanged,
    /// Device error before acceptance. ErrorKind alone cannot reliably distinguish
    /// a missing route from a recently failed ARP resolution.
    Device(io::ErrorKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpOutcome {
    Rejected(UdpFailure),
    Dropped(UdpFailure),
    Submitted,
}

#[derive(Clone, Copy, Debug)]
pub struct UdpEvent {
    pub at: Duration,
    /// None for rejection before queue admission; Some for an accepted datagram.
    pub id: Option<DatagramId>,
    pub source: SocketAddrV4,
    pub destination: SocketAddrV4,
    pub generation: u64,
    pub outcome: UdpOutcome,
}

pub(super) struct Queued {
    pub frame: PacketBuf,
    pub id: DatagramId,
    pub source: SocketAddrV4,
    pub destination: SocketAddrV4,
    pub deadline: Option<Duration>,
    pub generation: u64,
}
struct PeerBudget {
    count: usize,
    next: Duration,
}
pub(super) struct Scheduler {
    pub queue: VecDeque<Queued>,
    peers: BTreeMap<SocketAddrV4, PeerBudget>,
    next: Duration,
    last: Option<SocketAddrV4>,
}

impl Scheduler {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            peers: BTreeMap::new(),
            next: Duration::ZERO,
            last: None,
        }
    }

    pub fn admission(
        &mut self,
        peer: SocketAddrV4,
        now: Duration,
        cfg: &UdpConfig,
    ) -> Result<(), UdpFailure> {
        self.peers.retain(|_, b| b.count > 0 || b.next > now);
        if self.queue.len() >= cfg.queue_capacity {
            return Err(UdpFailure::QueueFull);
        }

        if let Some(b) = self.peers.get(&peer) {
            if b.count >= cfg.per_peer_capacity {
                return Err(UdpFailure::PeerQueueFull);
            }
        } else if self.peers.len() >= cfg.max_tx_peers {
            return Err(UdpFailure::PeerLimit);
        }

        Ok(())
    }

    pub fn push(&mut self, q: Queued) {
        self.peers
            .entry(q.destination)
            .or_insert(PeerBudget {
                count: 0,
                next: Duration::ZERO,
            })
            .count += 1;

        self.queue.push_back(q);
    }
    pub fn remove(&mut self, index: usize) -> Queued {
        let q = self.queue.remove(index).unwrap();
        self.peers.get_mut(&q.destination).unwrap().count -= 1;

        q
    }

    /// Bounded scan, no per-poll allocations. A blocked peer may be bypassed in
    /// fair mode. Byte rates are charged only after acceptance.
    pub fn candidate(&self, now: Duration, fair: bool, blocked: &[SocketAddrV4]) -> Option<usize> {
        if now < self.next {
            return None;
        }

        if !fair {
            return self
                .queue
                .front()
                .filter(|q| {
                    !blocked.contains(&q.destination) && self.peers[&q.destination].next <= now
                })
                .map(|_| 0);
        }

        self.queue
            .iter()
            .enumerate()
            .filter(|(_, q)| {
                !blocked.contains(&q.destination) && self.peers[&q.destination].next <= now
            })
            .min_by_key(|(i, q)| {
                (
                    self.last.is_some_and(|last| q.destination <= last),
                    q.destination,
                    *i,
                )
            })
            .map(|(i, _)| i)
    }

    pub fn accepted(&mut self, peer: SocketAddrV4, bytes: usize, now: Duration, cfg: &UdpConfig) {
        self.last = Some(peer);
        self.next = now.saturating_add(spacing(bytes, cfg.bytes_per_second));
        self.peers.get_mut(&peer).unwrap().next =
            now.saturating_add(spacing(bytes, cfg.per_peer_bytes_per_second));
    }

    pub fn next_deadline(&self, fair: bool) -> Option<Duration> {
        let ready = self
            .queue
            .iter()
            .take(if fair { self.queue.len() } else { 1 })
            .map(|q| self.next.max(self.peers[&q.destination].next))
            .min();

        ready
            .into_iter()
            .chain(self.queue.iter().filter_map(|q| q.deadline))
            .min()
    }
}

fn spacing(bytes: usize, rate: Option<u64>) -> Duration {
    let Some(rate) = rate else {
        return Duration::ZERO;
    };

    let ns = (bytes as u128 * 1_000_000_000).div_ceil(rate as u128);
    Duration::new(
        (ns / 1_000_000_000).min(u64::MAX as u128) as u64,
        (ns % 1_000_000_000) as u32,
    )
}
