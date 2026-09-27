//! Best-effort remote telemetry, isolated from the userspace network stack.
//!
//! The producer never waits for the exporter: try_lock contention or a full
//! preallocated buffer drops the record. This is bounded, not a hard-real-time
//! or wait-free guarantee. No formatting, sockets, or clock reads on record().
//! UDP is unauthenticated: use a trusted management network or encrypted tunnel.
use crate::api::tcp::{TcpEvent, TcpState, TcpWait};
use std::{
    io::{self, Read},
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const HEADER: usize = 56;
const RECORD: usize = 112;
const BATCH: usize = 10; // 1176-byte UDP payload, below a typical Ethernet MTU.

pub struct TelemetryConfig {
    /// Explicit destination; never inferred from untrusted incoming packets.
    pub client: SocketAddr,
    /// Select a management-interface source address if available. Port 0 is fine.
    pub bind: SocketAddr,
    pub capacity: usize,
    pub export_interval: Duration,
    pub heartbeat_interval: Duration,
}
impl TelemetryConfig {
    pub fn new(client: SocketAddr) -> Self {
        Self {
            client,
            bind: if client.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            }
            .parse()
            .unwrap(),
            capacity: 1024,
            export_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_secs(1),
        }
    }
}

struct Shared {
    records: Mutex<Vec<TcpEvent>>,
    stopped: AtomicBool,
    progress: AtomicU64,
    dropped: AtomicU64,
    send_errors: AtomicU64,
    source_overwrites: AtomicU64,
}

/// One application producer and one exporter. Disabling telemetry means not
/// constructing this object. No event payloads or application data are exported.
pub struct Telemetry {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    capacity: usize,
}

impl Telemetry {
    pub fn start(config: TelemetryConfig) -> io::Result<Self> {
        if config.capacity == 0
            || config.capacity > 1_048_576
            || config.export_interval.is_zero()
            || config.heartbeat_interval.is_zero()
            || config.export_interval > config.heartbeat_interval
            || config.bind.is_ipv4() != config.client.is_ipv4()
            || config.client.port() == 0
            || config.client.ip().is_unspecified()
            || config.client.ip().is_multicast()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid telemetry configuration",
            ));
        }

        let socket = UdpSocket::bind(config.bind)?;
        socket.connect(config.client)?;
        socket.set_nonblocking(true)?;

        let mut session = [0; 8];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut session)?;
        let shared = Arc::new(Shared {
            records: Mutex::new(Vec::with_capacity(config.capacity)),
            stopped: AtomicBool::new(false),
            progress: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            source_overwrites: AtomicU64::new(0),
        });

        let worker_shared = shared.clone();
        let capacity = config.capacity;
        let worker = thread::Builder::new()
            .name("telemetry-export".into())
            .spawn(move || {
                export(worker_shared, socket, config, session);
            })?;

        Ok(Self {
            shared,
            worker: Some(worker),
            capacity,
        })
    }

    /// Call once after a successful application iteration, not from a timer
    /// thread. A live exporter with a stationary progress counter is not proof
    /// of application health. source_overwrites is TcpPool::stats().events_overwritten.
    pub fn progress(&self, source_overwrites: u64) {
        self.shared.progress.fetch_add(1, Ordering::Relaxed);
        self.shared
            .source_overwrites
            .store(source_overwrites, Ordering::Relaxed);
    }

    /// False means telemetry was discarded; application execution must continue.
    pub fn record(&self, event: TcpEvent) -> bool {
        if let Ok(mut records) = self.shared.records.try_lock() {
            if records.len() < self.capacity {
                records.push(event);
                return true;
            }
        }
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);

        false
    }

    pub fn exporter_finished(&self) -> bool {
        self.worker
            .as_ref()
            .map_or(true, |worker| worker.is_finished())
    }

    /// Orderly, potentially blocking shutdown, only outside the measured loop.
    /// Drains already collected records once; UDP delivery is never guaranteed.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.shared.stopped.store(true, Ordering::Relaxed);

        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            worker.join()
        } else {
            Ok(())
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }

        // Drop must not stall the application. Use shutdown() for explicit join.
    }
}

fn export(shared: Arc<Shared>, socket: UdpSocket, cfg: TelemetryConfig, session: [u8; 8]) {
    let mut pending = Vec::with_capacity(cfg.capacity);
    let mut sequence = 0u64;
    let mut heartbeat = Instant::now();

    loop {
        let stopping = shared.stopped.load(Ordering::Relaxed);
        {
            let Ok(mut records) = shared.records.lock() else {
                return;
            };

            std::mem::swap(&mut *records, &mut pending);
        } // Never hold the producer lock during encoding or socket work.

        if pending.is_empty() && (Instant::now() >= heartbeat || stopping) {
            send(&socket, &shared, session, &mut sequence, &[]);
            heartbeat = Instant::now() + cfg.heartbeat_interval;
        }

        for batch in pending.chunks(BATCH) {
            send(&socket, &shared, session, &mut sequence, batch);
            heartbeat = Instant::now() + cfg.heartbeat_interval;
        }

        pending.clear();
        if stopping {
            break;
        }

        thread::park_timeout(cfg.export_interval);
    }
}

fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn send(
    socket: &UdpSocket,
    shared: &Shared,
    session: [u8; 8],
    sequence: &mut u64,
    events: &[TcpEvent],
) {
    let mut bytes = [0; HEADER + RECORD * BATCH];
    bytes[..4].copy_from_slice(b"NSTL");
    bytes[4] = 1;
    bytes[6..8].copy_from_slice(&(events.len() as u16).to_be_bytes());
    bytes[8..16].copy_from_slice(&session);
    put(&mut bytes, 16, *sequence);

    *sequence = sequence.wrapping_add(1);
    put(&mut bytes, 24, shared.progress.load(Ordering::Relaxed));
    put(&mut bytes, 32, shared.dropped.load(Ordering::Relaxed));
    put(&mut bytes, 40, shared.send_errors.load(Ordering::Relaxed));
    put(
        &mut bytes,
        48,
        shared.source_overwrites.load(Ordering::Relaxed),
    );

    for (index, event) in events.iter().enumerate() {
        let record = &mut bytes[HEADER + index * RECORD..HEADER + (index + 1) * RECORD];
        let s = event.status;
        let f = s.flow;
        put(record, 0, micros(event.at));
        put(record, 8, event.connection.as_u64());

        record[16] = state(s.state);
        record[17] = wait(f.wait);
        record[20..24].copy_from_slice(&s.local.ip().octets());
        record[24..28].copy_from_slice(&s.remote.ip().octets());
        record[28..30].copy_from_slice(&s.local.port().to_be_bytes());
        record[30..32].copy_from_slice(&s.remote.port().to_be_bytes());

        for (i, value) in [
            s.send_buffered as u64,
            s.receive_buffered as u64,
            f.congestion_window as u64,
            f.slow_start_threshold as u64,
            f.bytes_in_flight as u64,
            f.outstanding_segments as u64,
            micros(f.retransmission_timeout),
            f.timeout_retransmissions,
            f.advertised_window as u64,
            f.peer_window as u64,
        ]
        .into_iter()
        .enumerate()
        {
            put(record, 32 + i * 8, value);
        }
    }

    let len = HEADER + RECORD * events.len();
    if socket.send(&bytes[..len]).map_or(true, |n| n != len) {
        shared.send_errors.fetch_add(1, Ordering::Relaxed);
    }
}

fn state(value: TcpState) -> u8 {
    match value {
        TcpState::SynSent => 0,
        TcpState::SynReceived => 1,
        TcpState::Established => 2,
        TcpState::FinWait1 => 3,
        TcpState::FinWait2 => 4,
        TcpState::CloseWait => 5,
        TcpState::Closing => 6,
        TcpState::LastAck => 7,
        TcpState::TimeWait => 8,
        TcpState::Closed => 9,
        TcpState::Reset => 10,
        TcpState::TimedOut => 11,
        TcpState::Failed => 12,
    }
}

fn wait(value: TcpWait) -> u8 {
    match value {
        TcpWait::Application => 0,
        TcpWait::Acknowledgment => 1,
        TcpWait::PeerWindow => 2,
        TcpWait::CongestionWindow => 3,
        TcpWait::FlightLimit => 4,
        TcpWait::Device => 5,
        TcpWait::Handshake => 6,
        TcpWait::Closing => 7,
        TcpWait::Terminal => 8,
        TcpWait::Ready => 9,
    }
}
