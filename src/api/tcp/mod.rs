//! Initial single-threaded IPv4 TCP endpoints over an L3 Device.
//!
//! Use TUN/SimDevice directly, or EthernetIpv4<XdpDevice> for Ethernet. `poll`
//! advances the adapter clock, receives segments, and drives TCP output. This
//! pool owns its device's receive stream; do not independently poll a UdpPool
//! over the same device. Shared UDP/TCP dispatch is a subsequent extension.
//!
//! The sender uses bounded flight queues, slow start and congestion avoidance.
//! Receive delivery is in order: out-of-order data is ACKed but not buffered.
//! MSS, bounded buffers, retransmission/backoff, zero-window probes, half-close,
//! and TIME-WAIT are included. Window scaling, SACK, timestamps, ECN negotiation,
//! simultaneous open, urgent data, PMTU adaptation, and fast retransmit/recovery
//! are absent; loss recovery currently uses the retransmission timer. This is a
//! lab implementation, not RFC-complete TCP.
mod connection;

use crate::{
    device::{Device, PacketBuf},
    transport::tcp::{self, Segment, ACK, FIN, RST, SYN, URG},
};
use connection::Connection;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Read},
    net::SocketAddrV4,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(u64);
impl ConnectionId {
    /// Stable within a pool; combine with telemetry session ID across restarts.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed,
    Reset,
    TimedOut,
    Failed,
}
impl TcpState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Closed | Self::Reset | Self::TimedOut | Self::Failed
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TcpConfig {
    pub max_connections: usize,
    pub max_listeners: usize,
    pub control_capacity: usize,
    /// Per-connection bytes, including unacknowledged payload on the send side.
    pub send_capacity: usize,
    /// Receive storage, 1..=65535 bytes (window scaling is not negotiated).
    pub receive_capacity: usize,
    /// Maximum advertised free space; capped by actual remaining receive storage.
    pub receive_window_limit: u16,
    /// Initial congestion window in negotiated MSS units, 1..=4. The RFC 5681
    /// initial-window bound for large MSS values is applied automatically.
    pub initial_cwnd_segments: u16,
    /// Local byte ceiling (1..=2^30), never advertised on the wire. Actual output
    /// is also limited by send storage, flight slots and the peer receive window;
    /// without window scaling a peer can permit at most 65535 bytes in flight.
    pub max_cwnd_bytes: usize,
    /// Initial slow-start threshold (1..=2^30 bytes); loss updates it dynamically.
    pub initial_ssthresh_bytes: usize,
    /// Bounds retransmission descriptors, even with very small peer MSS values.
    pub max_inflight_segments: usize,
    /// Maximum segment submissions per connection in one poll.
    pub tx_burst: usize,
    /// Collect fixed-size state events, without formatting, I/O or extra clocks.
    pub verbose_state: bool,
    /// Global bounded event ring; oldest events are overwritten when full.
    pub event_capacity: usize,
    /// Minimum interval between changing flow-metric snapshots. State/wait
    /// transitions and retransmissions bypass this limit. No periodic event is
    /// emitted for an unchanged connection, including an unchanged ACK wait.
    pub state_report_interval: Duration,
    /// IPv4 datagram size; must also fit the underlying adapter and frame pool.
    pub mtu: usize,
    /// Local receive MSS and maximum send segment size before peer negotiation.
    pub mss: u16,
    pub initial_rto: Duration,
    pub max_rto: Duration,
    pub max_retransmits: u32,
    pub handshake_timeout: Duration,
    pub send_timeout: Duration,
    pub close_timeout: Duration,
    /// Default 2 MSL. A tuple cannot be reused while it remains in TIME-WAIT.
    pub time_wait: Duration,
}
impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_listeners: 16,
            control_capacity: 64,
            send_capacity: 16 * 1024,
            receive_capacity: 16 * 1024,
            receive_window_limit: u16::MAX,
            initial_cwnd_segments: 2,
            max_cwnd_bytes: 16 * 1024,
            initial_ssthresh_bytes: 65535,
            max_inflight_segments: 128,
            tx_burst: 32,
            verbose_state: false,
            event_capacity: 256,
            state_report_interval: Duration::from_millis(250),
            mtu: 1500,
            mss: 536,
            initial_rto: Duration::from_secs(1),
            max_rto: Duration::from_secs(60),
            max_retransmits: 8,
            handshake_timeout: Duration::from_secs(30),
            send_timeout: Duration::from_secs(120),
            close_timeout: Duration::from_secs(120),
            time_wait: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TcpStats {
    pub received: u64,
    pub malformed: u64,
    pub ignored: u64,
    pub submitted: u64,
    pub retransmitted: u64,
    pub resets_submitted: u64,
    pub control_drops: u64,
    pub connection_overflow: u64,
    pub events_overwritten: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct ConnectionStatus {
    pub local: SocketAddrV4,
    pub remote: SocketAddrV4,
    pub state: TcpState,
    pub send_buffered: usize,
    pub receive_buffered: usize,
    pub flow: FlowStatus,
    /// A local route/address/packet error, distinct from a peer reset or timeout.
    pub local_error: Option<io::ErrorKind>,
}

/// Why output is currently idle. A wait is reported once until its reason changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpWait {
    Application,
    Acknowledgment,
    PeerWindow,
    CongestionWindow,
    FlightLimit,
    Device,
    Handshake,
    Closing,
    Terminal,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowStatus {
    pub advertised_window: u16,
    pub peer_window: u16,
    pub congestion_window: usize,
    pub slow_start_threshold: usize,
    /// Submitted sequence-space bytes that have not been acknowledged.
    pub bytes_in_flight: usize,
    /// Includes a prepared segment awaiting device capacity.
    pub outstanding_segments: usize,
    pub retransmission_timeout: Duration,
    pub timeout_retransmissions: u64,
    pub wait: TcpWait,
}

/// Snapshot on a state/wait transition or timeout retransmission. Stable waiting
/// periods emit no repeated events, even if poll is called continuously. Changed
/// flow metrics are additionally sampled at state_report_interval; status() gives
/// an on-demand snapshot regardless of whether event collection is enabled.
#[derive(Clone, Copy, Debug)]
pub struct TcpEvent {
    pub at: Duration,
    pub connection: ConnectionId,
    pub status: ConnectionStatus,
}

#[derive(Clone, Copy)]
struct Reset {
    local: SocketAddrV4,
    remote: SocketAddrV4,
    sequence: u32,
    acknowledgment: u32,
    flags: u8,
}

pub struct TcpPool<D> {
    device: D,
    config: TcpConfig,
    connections: BTreeMap<ConnectionId, Connection>,
    listeners: BTreeSet<SocketAddrV4>,
    resets: VecDeque<Reset>,
    rx: Vec<PacketBuf>,
    now: Duration,
    next_id: u64,
    stats: TcpStats,
    events: VecDeque<TcpEvent>,
}

impl<D: Device> TcpPool<D> {
    pub fn new(device: D, config: TcpConfig) -> io::Result<Self> {
        if config.max_connections == 0
            || config.max_listeners == 0
            || config.control_capacity == 0
            || config.send_capacity == 0
            || config.send_capacity > (1 << 30)
            || config.receive_window_limit == 0
            || !(1..=4).contains(&config.initial_cwnd_segments)
            || config.max_cwnd_bytes == 0
            || config.max_cwnd_bytes > (1 << 30)
            || config.initial_ssthresh_bytes == 0
            || config.initial_ssthresh_bytes > (1 << 30)
            || config.max_inflight_segments == 0
            || config.max_inflight_segments > u16::MAX as usize
            || config.tx_burst == 0
            || (config.verbose_state
                && (config.event_capacity == 0 || config.state_report_interval.is_zero()))
            || config.receive_capacity == 0
            || config.receive_capacity > u16::MAX as usize
            || config.mtu < 68
            || config.mtu > u16::MAX as usize
            || config.mss == 0
            || config.mss as usize > config.mtu - 40
            || config.mss as usize > config.receive_capacity
            || config.initial_rto < Duration::from_secs(1)
            || config.max_rto < config.initial_rto
            || config.handshake_timeout.is_zero()
            || config.send_timeout.is_zero()
            || config.close_timeout.is_zero()
            || config.time_wait.is_zero()
        {
            return Err(invalid("invalid TCP configuration"));
        }
        Ok(Self {
            device,
            config,
            connections: BTreeMap::new(),
            listeners: BTreeSet::new(),
            resets: VecDeque::with_capacity(config.control_capacity),
            rx: Vec::new(),
            now: Duration::ZERO,
            next_id: 1,
            stats: TcpStats::default(),
            events: VecDeque::with_capacity(if config.verbose_state {
                config.event_capacity
            } else {
                0
            }),
        })
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }
    pub fn stats(&self) -> TcpStats {
        self.stats
    }
    pub fn config(&self) -> TcpConfig {
        self.config
    }

    /// Exact IPv4 bind, with no wildcard address or automatic ephemeral port selection.
    pub fn listen(&mut self, local: SocketAddrV4) -> io::Result<()> {
        validate_address(local)?;
        if self.listeners.contains(&local) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "TCP listener exists",
            ));
        }
        if self.listeners.len() == self.config.max_listeners {
            return Err(blocked("listener table full"));
        }
        self.listeners.insert(local);
        Ok(())
    }

    /// Stops accepting new connections and releases unaccepted children.
    pub fn unlisten(&mut self, local: SocketAddrV4) {
        self.listeners.remove(&local);
        self.connections
            .retain(|_, c| c.accepted || c.local != local);
    }

    /// Queues an active open. Poll until status becomes Established or terminal.
    /// Sequence numbers use OS entropy on the supported Unix hosts.
    pub fn connect(
        &mut self,
        local: SocketAddrV4,
        remote: SocketAddrV4,
    ) -> io::Result<ConnectionId> {
        validate_address(local)?;
        validate_address(remote)?;
        if local == remote {
            return Err(invalid("identical TCP endpoints"));
        }
        if self.connections.len() == self.config.max_connections {
            return Err(blocked("connection table full"));
        }
        if self.find(local, remote).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "TCP tuple already active",
            ));
        }
        let sequence = initial_sequence()?;
        let id = self.allocate_id()?;
        self.connections.insert(
            id,
            Connection::active(local, remote, sequence, self.now, &self.config),
        );
        Ok(id)
    }

    pub fn accept(&mut self, local: SocketAddrV4) -> io::Result<Option<ConnectionId>> {
        if !self.listeners.contains(&local) {
            return Err(invalid("no listener at this address"));
        }
        for (&id, connection) in &mut self.connections {
            if connection.local == local
                && !connection.accepted
                && matches!(
                    connection.state,
                    TcpState::Established | TcpState::CloseWait
                )
            {
                connection.accepted = true;
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub fn status(&self, id: ConnectionId) -> io::Result<ConnectionStatus> {
        let c = self.connections.get(&id).ok_or_else(missing)?;
        Ok(c.status(&self.config))
    }

    /// Drain outside the packet-processing path. The library never prints events.
    pub fn pop_event(&mut self) -> Option<TcpEvent> {
        self.events.pop_front()
    }

    fn report_states(&mut self) {
        if !self.config.verbose_state {
            return;
        }
        for (&id, c) in &mut self.connections {
            let status = c.status(&self.config);
            let key = (
                status.state,
                status.flow.wait,
                status.flow.timeout_retransmissions,
            );
            let transition = c.reported != Some(key);
            let metrics_changed = c.reported_flow != Some(status.flow);
            let sample_due = self.now
                >= c.reported_at
                    .saturating_add(self.config.state_report_interval);
            if !transition && !(metrics_changed && sample_due) {
                continue;
            }
            c.reported = Some(key);
            c.reported_flow = Some(status.flow);
            c.reported_at = self.now;
            if self.events.len() == self.config.event_capacity {
                self.events.pop_front();
                self.stats.events_overwritten = self.stats.events_overwritten.saturating_add(1);
            }
            self.events.push_back(TcpEvent {
                at: self.now,
                connection: id,
                status,
            });
        }
    }

    pub fn connections(&self) -> impl Iterator<Item = ConnectionId> + '_ {
        self.connections.keys().copied()
    }

    /// Accepts a prefix into a bounded byte queue. WouldBlock consumes no bytes.
    pub fn write(&mut self, id: ConnectionId, bytes: &[u8]) -> io::Result<usize> {
        self.connections
            .get_mut(&id)
            .ok_or_else(missing)?
            .write(bytes, &self.config)
    }

    /// Returns buffered bytes, WouldBlock while waiting, or zero after peer FIN.
    /// Empty output buffers return zero without implying EOF.
    pub fn read(&mut self, id: ConnectionId, out: &mut [u8]) -> io::Result<usize> {
        self.connections.get_mut(&id).ok_or_else(missing)?.read(out)
    }

    /// Half-close after all accepted application bytes have been acknowledged.
    pub fn close(&mut self, id: ConnectionId) -> io::Result<()> {
        self.connections
            .get_mut(&id)
            .ok_or_else(missing)?
            .close(self.now, &self.config)
    }

    /// Local abort, with a best-effort bounded RST queue.
    pub fn abort(&mut self, id: ConnectionId) -> io::Result<()> {
        let c = self.connections.get_mut(&id).ok_or_else(missing)?;
        let reset = c.reset_packet();
        c.fail(TcpState::Reset);
        self.queue_reset(reset);
        self.report_states();
        Ok(())
    }

    /// Release a terminal connection. TIME-WAIT cannot be removed early.
    pub fn remove(&mut self, id: ConnectionId) -> io::Result<()> {
        if !self
            .connections
            .get(&id)
            .ok_or_else(missing)?
            .state
            .is_terminal()
        {
            return Err(blocked("connection is not terminal"));
        }
        self.report_states();
        self.connections.remove(&id);
        Ok(())
    }

    /// Receive at most budget packets; submit at most tx_burst segments per connection
    /// and one reset per call. All time is supplied by the caller, with no sleeps.
    /// Submission is not remote delivery. Local route/address errors fail the
    /// affected connection (see status); other backend errors return to the caller.
    pub fn poll(&mut self, now: Duration, budget: usize) -> io::Result<usize> {
        if now < self.now {
            return Err(invalid("time went backwards"));
        }
        self.now = now;
        for c in self.connections.values_mut() {
            c.tick(now, &self.config);
        }
        self.report_states();
        self.connections
            .retain(|_, c| c.accepted || !c.state.is_terminal());
        self.device.poll_at(now)?;
        self.device.recv(budget, &mut self.rx)?;
        let mut rx = std::mem::take(&mut self.rx);
        let count = rx.len();
        // Restore/recycle scratch packets even if accepting a SYN cannot obtain entropy.
        let result = (|| {
            for frame in rx.drain(..) {
                let bytes = frame.as_slice();
                if bytes.len() < 20 || bytes[9] != 6 {
                    self.stats.ignored += 1;
                    continue;
                }
                match tcp::parse_ipv4(bytes) {
                    Ok(segment) => {
                        self.stats.received += 1;
                        self.input(&segment)?;
                    }
                    Err(_) => self.stats.malformed += 1,
                }
            }
            Ok::<(), io::Error>(())
        })();
        rx.clear();
        self.rx = rx;
        result?;
        self.flush_reset()?;
        for connection in self.connections.values_mut() {
            for _ in 0..self.config.tx_burst {
                let (sent, retransmitted) =
                    match connection.transmit(&mut self.device, now, &self.config) {
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::NotConnected
                                    | io::ErrorKind::AddrNotAvailable
                                    | io::ErrorKind::InvalidInput
                            ) =>
                        {
                            connection.fail(TcpState::Failed);
                            connection.local_error = Some(e.kind());
                            break;
                        }
                        result => result?,
                    };
                self.stats.submitted += u64::from(sent);
                self.stats.retransmitted += u64::from(retransmitted);
                if !sent {
                    break;
                }
            }
        }
        self.report_states();
        self.device.poll_at(now)?;
        Ok(count)
    }

    /// TCP timer deadline only; also consider device readiness and adapter timers.
    /// Call poll regularly while connecting, sending, closing, or advertising a window update.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.connections
            .values()
            .filter_map(|c| c.next_deadline(&self.config))
            .min()
    }

    fn allocate_id(&mut self) -> io::Result<ConnectionId> {
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| invalid("connection IDs exhausted"))?;
        let id = ConnectionId(self.next_id);
        self.next_id = next;
        Ok(id)
    }

    fn find(&self, local: SocketAddrV4, remote: SocketAddrV4) -> Option<ConnectionId> {
        self.connections
            .iter()
            .find(|(_, c)| c.local == local && c.remote == remote && !c.state.is_terminal())
            .map(|(&id, _)| id)
    }

    fn input(&mut self, segment: &Segment<'_>) -> io::Result<()> {
        if validate_address(segment.source).is_err()
            || validate_address(segment.destination).is_err()
            || segment.flags & URG != 0
        {
            self.stats.ignored += 1;
            return Ok(());
        }
        if let Some(id) = self.find(segment.destination, segment.source) {
            self.connections
                .get_mut(&id)
                .unwrap()
                .input(segment, self.now, &self.config);
            return Ok(());
        }
        // An L3 device may deliver broadcasts or packets for other local users.
        // Only emit resets for IP addresses explicitly used by this pool.
        if !self
            .listeners
            .iter()
            .any(|a| a.ip() == segment.destination.ip())
            && !self
                .connections
                .values()
                .any(|c| c.local.ip() == segment.destination.ip())
        {
            self.stats.ignored += 1;
            return Ok(());
        }
        if segment.flags & RST != 0 {
            return Ok(());
        }
        if self.listeners.contains(&segment.destination) && segment.flags & (SYN | ACK | FIN) == SYN
        {
            if self.connections.len() == self.config.max_connections {
                self.stats.connection_overflow += 1;
                return Ok(());
            }
            let sequence = initial_sequence()?;
            let id = self.allocate_id()?;
            self.connections.insert(
                id,
                Connection::passive(segment, sequence, self.now, &self.config),
            );
        } else {
            self.queue_reset(if segment.flags & ACK != 0 {
                Reset {
                    local: segment.destination,
                    remote: segment.source,
                    sequence: segment.acknowledgment,
                    acknowledgment: 0,
                    flags: RST,
                }
            } else {
                Reset {
                    local: segment.destination,
                    remote: segment.source,
                    sequence: 0,
                    acknowledgment: segment.sequence.wrapping_add(segment.sequence_len()),
                    flags: RST | ACK,
                }
            });
        }
        Ok(())
    }

    fn queue_reset(&mut self, reset: Reset) {
        if self.resets.len() == self.config.control_capacity {
            self.stats.control_drops += 1;
        } else {
            self.resets.push_back(reset);
        }
    }

    fn flush_reset(&mut self) -> io::Result<()> {
        let Some(reset) = self.resets.front().copied() else {
            return Ok(());
        };
        let Some(mut frame) = self.device.alloc() else {
            return Ok(());
        };
        tcp::build_ipv4(
            &mut frame,
            &Segment {
                source: reset.local,
                destination: reset.remote,
                sequence: reset.sequence,
                acknowledgment: reset.acknowledgment,
                flags: reset.flags,
                window: 0,
                mss: None,
                payload: &[],
            },
        )?;
        let n = match self.device.send(std::slice::from_mut(&mut frame)) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotConnected
                        | io::ErrorKind::AddrNotAvailable
                        | io::ErrorKind::InvalidInput
                ) =>
            {
                // A stateless reset must not permanently block live connections
                // when there is no usable route back to an unsolicited sender.
                self.resets.pop_front();
                self.stats.control_drops += 1;
                return Ok(());
            }
            result => result?,
        };
        if n == 1 {
            self.resets.pop_front();
            self.stats.resets_submitted += 1;
        }
        Ok(())
    }
}

fn initial_sequence() -> io::Result<u32> {
    let mut bytes = [0; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}
fn validate_address(address: SocketAddrV4) -> io::Result<()> {
    if address.port() == 0
        || address.ip().is_unspecified()
        || address.ip().is_broadcast()
        || address.ip().is_multicast()
    {
        Err(invalid("TCP needs a unicast IP and nonzero port"))
    } else {
        Ok(())
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn blocked(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}
fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "unknown TCP connection")
}
