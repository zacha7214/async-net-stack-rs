mod memory;
mod queue;
mod wire;
use crate::vm_packet;
use clap::Parser;
use memory::{Fault, Memory, Region, Span};
use queue::{read_scatter, Queue};
use serde_json::json;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wire::{u32_at, u64_at, Message, Wire};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn bad(reason: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::new(io::ErrorKind::InvalidData, reason.into()).into()
}
const FEATURES: u64 = (1 << 27) | (1 << 30) | (1 << 32) | (1 << 33) | (1 << 40);
const PROTOCOL: u64 = (1 << 3) | (1 << 5) | (1 << 13);
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[derive(Parser)]
#[command(about = "QEMU vhost-user synthetic RX lab; see docs/vhost-user-lab.md")]
struct Args {
    #[arg(long)]
    socket: PathBuf,
    /// Generated Ethernet frame size (excluding the 12-byte virtio header).
    #[arg(long, default_value_t = 1500)]
    size: usize,
    #[arg(long, default_value_t = 64)]
    batch: usize,
    /// Initial rate limit; zero runs without pacing. No packets before guest START.
    #[arg(long, default_value_t = 10000)]
    pps: u64,
    #[arg(long, default_value_t = 1000000000)]
    max_packets: u64,
    /// Include the first N frame GPA/IOVA samples in session output.
    #[arg(long, default_value_t = 8)]
    samples: usize,
    #[arg(long)]
    trace_control: bool,
}
struct Session {
    id: u64,
    target: u64,
    generated: u64,
    start: Instant,
}
struct Backend {
    memory: Memory,
    queues: [Queue; 2],
    features: u64,
    protocol: u64,
    owner: bool,
    channel: Option<Wire>,
    pending_miss: Option<(u64, u8, Instant)>,
    session: Option<Session>,
    previous_session: Option<u64>,
    tx_completed: u64,
    rx_completed: u64,
    iotlb_updates: u64,
    iotlb_invalidations: u64,
    queue_stops: u64,
    spans: Vec<Span>,
}
impl Backend {
    fn new() -> Self {
        Self {
            memory: Memory::default(),
            queues: [Queue::default(), Queue::default()],
            features: 0,
            protocol: 0,
            owner: false,
            channel: None,
            pending_miss: None,
            session: None,
            previous_session: None,
            tx_completed: 0,
            rx_completed: 0,
            iotlb_updates: 0,
            iotlb_invalidations: 0,
            queue_stops: 0,
            spans: Vec::with_capacity(64),
        }
    }
    fn handle(&mut self, mut m: Message, wire: &mut Wire, trace: bool) -> Result<()> {
        if trace {
            eprintln!(
                "request={} bytes={} fds={}",
                m.request,
                m.body.len(),
                m.fds.len()
            );
        }
        let mut reply = None;
        match m.request {
            1 => {
                m.check(0, 0)?;
                reply = Some(FEATURES.to_ne_bytes().to_vec());
            }
            15 => {
                m.check(0, 0)?;
                reply = Some(PROTOCOL.to_ne_bytes().to_vec());
            }
            17 => {
                m.check(0, 0)?;
                reply = Some(1u64.to_ne_bytes().to_vec());
            }
            3 => {
                m.check(0, 0)?;
                self.owner = true;
            }
            4 | 34 => {
                m.check(0, 0)?;
                self.queues = [Queue::default(), Queue::default()];
                self.session = None;
                self.pending_miss = None;
                if m.request == 4 {
                    self.owner = false;
                    self.memory = Memory::default();
                    self.features = 0;
                }
            }
            16 => {
                m.check(8, 0)?;
                let f = u64_at(&m.body, 0);
                if f & !PROTOCOL != 0 {
                    return Err(bad("unsupported protocol features"));
                }
                self.protocol = f;
            }
            2 => {
                m.check(8, 0)?;
                let f = u64_at(&m.body, 0);
                if !self.owner || f & !FEATURES != 0 || f & (1 << 32) == 0 {
                    return Err(bad("unsupported virtio features or missing SET_OWNER"));
                }
                self.features = f;
                self.memory.iommu = f & (1 << 33) != 0;
                if f & (1 << 30) == 0 {
                    for q in &mut self.queues {
                        q.enabled = true;
                    }
                }
                println!(
                    "{}",
                    json!({"event":"features", "bits":format!("{f:#x}"), "access_platform":self.memory.iommu, "ring_reset":f & (1<<40) != 0})
                );
            }
            5 => {
                if m.body.len() < 8 {
                    return Err(bad("short memory table"));
                }
                let n = u32_at(&m.body, 0) as usize;
                if n == 0 || n > 8 || u32_at(&m.body, 4) != 0 {
                    return Err(bad("memory table needs 1..=8 regions"));
                }
                m.check(8 + n * 32, n)?;
                let mut regions = Vec::with_capacity(n);
                for (i, fd) in m.fds.drain(..).enumerate() {
                    let p = 8 + i * 32;
                    regions.push(Region::new(
                        fd,
                        u64_at(&m.body, p),
                        u64_at(&m.body, p + 16),
                        u64_at(&m.body, p + 8),
                        u64_at(&m.body, p + 24),
                    )?);
                }
                self.memory.replace(regions)?;
                self.pending_miss = None;
                println!("{}", json!({"event":"memory", "regions":n}));
            }
            8 | 10 | 11 | 18 => {
                m.check(8, 0)?;
                let index = u32_at(&m.body, 0) as usize;
                let value = u32_at(&m.body, 4);
                let q = self
                    .queues
                    .get_mut(index)
                    .ok_or_else(|| bad("only queue indices 0 and 1 are supported"))?;
                match m.request {
                    8 => {
                        if !(2..=1024).contains(&value) || !value.is_power_of_two() {
                            return Err(bad("queue size must be power of two in 2..=1024"));
                        }
                        q.num = value as u16;
                        q.last_used = None;
                    }
                    10 => {
                        if value > u16::MAX as u32 {
                            return Err(bad("invalid split ring base"));
                        }
                        q.last_avail = value as u16;
                        q.last_used = None;
                    }
                    11 => {
                        let base = q.stop();
                        self.queue_stops += 1;
                        let mut b = (index as u32).to_ne_bytes().to_vec();
                        b.extend_from_slice(&(base as u32).to_ne_bytes());
                        reply = Some(b);
                        println!(
                            "{}",
                            json!({"event":"queue_stop", "queue":index, "base":base})
                        );
                    }
                    18 => {
                        if value > 1 {
                            return Err(bad("invalid queue enable state"));
                        }
                        q.enabled = value != 0;
                    }
                    _ => unreachable!(),
                }
            }
            9 => {
                m.check(40, 0)?;
                if u32_at(&m.body, 4) != 0 {
                    return Err(bad("dirty logging is not supported"));
                }
                let index = u32_at(&m.body, 0) as usize;
                let q = self
                    .queues
                    .get_mut(index)
                    .ok_or_else(|| bad("invalid queue index"))?;
                q.desc = u64_at(&m.body, 8);
                q.used = u64_at(&m.body, 16);
                q.avail = u64_at(&m.body, 24);
                q.configured = true;
                q.last_used = None;
            }
            12..=14 => {
                if m.body.len() != 8 {
                    return Err(bad("short ring fd message"));
                }
                let value = u64_at(&m.body, 0);
                if value & !0x1ff != 0 {
                    return Err(bad("invalid ring fd flags"));
                }
                let nofd = value & 0x100 != 0;
                m.check(8, if nofd { 0 } else { 1 })?;
                let q = self
                    .queues
                    .get_mut((value & 0xff) as usize)
                    .ok_or_else(|| bad("invalid queue fd index"))?;
                let fd = m.fds.pop();
                if let Some(f) = &fd {
                    queue::nonblocking(f)?;
                }
                match m.request {
                    12 => {
                        q.kick = fd;
                        q.started = true;
                    }
                    13 => q.call = fd,
                    14 => q.error = fd,
                    _ => unreachable!(),
                }
            }
            21 => {
                m.check(0, 1)?;
                let fd = m.fds.pop().unwrap().into_raw_fd();
                self.channel = Some(Wire::new(unsafe { UnixStream::from_raw_fd(fd) })?);
            }
            22 => {
                // struct vhost_iotlb_msg: 24 bytes of u64 fields, perm/type,
                // and six trailing ABI padding bytes (not packed to 26).
                m.check(32, 0)?;
                let address = u64_at(&m.body, 0);
                let size = u64_at(&m.body, 8);
                match m.body[25] {
                    2 => {
                        self.memory
                            .update(address, size, u64_at(&m.body, 16), m.body[24])?;
                        self.iotlb_updates += 1;
                    }
                    3 => {
                        self.memory.invalidate(address, size)?;
                        self.iotlb_invalidations += 1;
                    }
                    _ => return Err(bad("unsupported frontend IOTLB message")),
                }
                self.pending_miss = None;
            }
            _ => return Err(bad(format!("unsupported vhost-user request {}", m.request))),
        }
        if let Some(body) = reply {
            wire.send(m.request, true, &body)?;
        } else if m.flags & 8 != 0 {
            wire.send(m.request, true, &0u64.to_ne_bytes())?;
        }
        Ok(())
    }
    fn fault(&mut self, fault: Fault) -> Result<()> {
        match fault {
            Fault::Invalid(reason) => Err(bad(reason)),
            Fault::Missing {
                address,
                permission,
            } => {
                if let Some((old, _, since)) = self.pending_miss {
                    if since.elapsed() > Duration::from_secs(10) {
                        return Err(bad(format!(
                            "IOTLB miss at {old:#x} was not resolved by QEMU"
                        )));
                    }
                    // One outstanding miss avoids flooding QEMU while another
                    // queue is waiting for the same control-plane round trip.
                    return Ok(());
                }
                if self.protocol & PROTOCOL & ((1 << 3) | (1 << 5)) != ((1 << 3) | (1 << 5)) {
                    return Err(bad("IOTLB requires BACKEND_REQ and REPLY_ACK"));
                }
                let mut b = [0u8; 32];
                b[..8].copy_from_slice(&address.to_ne_bytes());
                b[24] = permission;
                b[25] = 1;
                self.channel
                    .as_mut()
                    .ok_or_else(|| bad("missing backend-request fd"))?
                    .send(1, false, &b)?;
                self.pending_miss = Some((address, permission, Instant::now()));
                Ok(())
            }
        }
    }
    fn tx(&mut self, a: &Args) -> Result<usize> {
        if !self.queues[1].running() {
            return Ok(0);
        }
        let rings = match self.queues[1].map(&self.memory) {
            Ok(r) => r,
            Err(e) => {
                self.fault(e)?;
                return Ok(0);
            }
        };
        let mut completed = 0;
        let mut fault = None;
        for _ in 0..a.batch {
            let head = match self.queues[1].peek(&self.memory, rings, false, &mut self.spans) {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) => {
                    fault = Some(e);
                    break;
                }
            };
            let mut h = [0; vm_packet::HEADER];
            if read_scatter(&self.spans, 12, &mut h) {
                let id = vm_packet::u64_at(&h, 40);
                if self.queues[1].enabled
                    && vm_packet::identify(&h, b"VS")
                    && self.previous_session != Some(id)
                {
                    let count = vm_packet::u64_at(&h, 24);
                    if count == 0 || count > a.max_packets {
                        return Err(bad("guest requested invalid packet count"));
                    }
                    self.previous_session = Some(id);
                    self.session = Some(Session {
                        id,
                        target: count,
                        generated: 0,
                        start: Instant::now(),
                    });
                    println!(
                        "{}",
                        json!({"event":"session_start", "session":id, "packets":count, "size":a.size, "pps":a.pps})
                    );
                } else if self.queues[1].enabled
                    && vm_packet::identify(&h, b"VE")
                    && self.session.as_ref().map(|s| s.id) == Some(id)
                {
                    self.session = None;
                }
            }
            self.queues[1].complete(rings, head, 0);
            completed += 1;
        }
        if completed != 0 {
            self.queues[1].publish(rings)?;
            self.tx_completed += completed as u64;
        }
        if let Some(e) = fault {
            self.fault(e)?;
        }
        Ok(completed)
    }
    fn rx(&mut self, a: &Args) -> Result<usize> {
        if !self.queues[0].running() || !self.queues[0].enabled || self.session.is_none() {
            return Ok(0);
        }
        let s = self.session.as_ref().unwrap();
        let allowed = if a.pps == 0 {
            s.target
        } else {
            ((s.start.elapsed().as_secs_f64() * a.pps as f64) as u64 + 1).min(s.target)
        };
        if s.generated >= allowed {
            return Ok(0);
        }
        let rings = match self.queues[0].map(&self.memory) {
            Ok(r) => r,
            Err(e) => {
                self.fault(e)?;
                return Ok(0);
            }
        };
        let mut completed = 0;
        let mut fault = None;
        for _ in 0..a.batch {
            let s = self.session.as_ref().unwrap();
            if s.generated >= allowed {
                break;
            }
            let head = match self.queues[0].peek(&self.memory, rings, true, &mut self.spans) {
                Ok(Some(h)) => h,
                Ok(None) => break,
                Err(e) => {
                    fault = Some(e);
                    break;
                }
            };
            if self.spans.iter().map(|s| s.len).sum::<usize>() < a.size + 12 {
                return Err(bad(
                    "guest RX chain too short; use MTU 1500 and disable mergeable buffers/offloads",
                ));
            }
            let frame_gpa =
                frame_gpa(&self.spans).ok_or_else(|| bad("missing packet data span"))?;
            let header = vm_packet::header(*b"VD", s.generated, frame_gpa, a.size, s.id);
            generate(&self.spans, &header, s.generated, a.size);
            if s.generated < a.samples as u64 {
                println!(
                    "{}",
                    json!({"event":"rx_sample", "session":s.id, "sequence":s.generated, "frame_gpa":format!("{frame_gpa:#x}"), "descriptor_head":head, "segments":self.spans.len()})
                );
            }
            self.queues[0].complete(rings, head, (a.size + 12) as u32);
            self.session.as_mut().unwrap().generated += 1;
            completed += 1;
        }
        if completed != 0 {
            self.queues[0].publish(rings)?;
            self.rx_completed += completed as u64;
        }
        if let Some(e) = fault {
            self.fault(e)?;
        }
        if let Some(s) = &self.session {
            if s.generated == s.target {
                println!(
                    "{}",
                    json!({"event":"session_generated", "session":s.id, "packets":s.generated, "seconds":s.start.elapsed().as_secs_f64(), "payload_staging_copies":0})
                );
                self.session = None;
            }
        }
        Ok(completed)
    }
}
fn frame_gpa(spans: &[Span]) -> Option<u64> {
    let mut skip = 12;
    for s in spans {
        if skip < s.len {
            return Some(s.gpa + skip as u64);
        }
        skip -= s.len;
    }
    None
}
fn generate(spans: &[Span], header: &[u8; vm_packet::HEADER], sequence: u64, size: usize) {
    let mut offset = 0;
    for s in spans {
        let n = s.len.min(size + 12 - offset);
        for i in 0..n {
            let p = offset + i;
            let byte = if p < 12 {
                0
            } else if p < 12 + vm_packet::HEADER {
                header[p - 12]
            } else {
                vm_packet::payload_byte(sequence, p - 12 - vm_packet::HEADER)
            };
            // Each write targets the leased guest descriptor buffer directly.
            unsafe {
                s.ptr.add(i).write(byte);
            }
        }
        offset += n;
        if offset == size + 12 {
            break;
        }
    }
}
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn main() -> Result<()> {
    let a = Args::parse();
    if !(64..=1514).contains(&a.size)
        || !(1..=256).contains(&a.batch)
        || a.samples > 64
        || a.max_packets == 0
        || a.pps > 100_000_000
    {
        return Err(bad(
            "size 64..=1514, batch 1..=256, samples <=64, pps <=100000000 required",
        ));
    }
    unsafe {
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
    }
    // Refuse to unlink an existing socket. Use a private directory for the lab.
    let listener = UnixListener::bind(&a.socket)?;
    let _guard = SocketGuard(a.socket.clone());
    listener.set_nonblocking(true)?;
    eprintln!(
        "listening on {}; waiting for QEMU, then guest START",
        a.socket.display()
    );
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if STOP.load(Ordering::Relaxed) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    };
    let mut wire = Wire::new(stream)?;
    let mut backend = Backend::new();
    let mut fds = Vec::with_capacity(4);
    while !STOP.load(Ordering::Relaxed) && !wire.eof {
        wire.flush()?;
        for _ in 0..32 {
            match wire.receive()? {
                Some(m) => backend.handle(m, &mut wire, a.trace_control)?,
                None => break,
            }
        }
        if wire.eof {
            break;
        }
        if let Some(c) = &mut backend.channel {
            c.flush()?;
        }
        let work = backend.tx(&a)? + backend.rx(&a)?;
        fds.clear();
        fds.push(libc::pollfd {
            fd: wire.stream.as_raw_fd(),
            events: libc::POLLIN | if wire.wants_write() { libc::POLLOUT } else { 0 },
            revents: 0,
        });
        if let Some(c) = &backend.channel {
            fds.push(libc::pollfd {
                fd: c.stream.as_raw_fd(),
                events: if c.wants_write() { libc::POLLOUT } else { 0 },
                revents: 0,
            });
        }
        let first_kick = fds.len();
        for q in &backend.queues {
            if let Some(k) = &q.kick {
                fds.push(libc::pollfd {
                    fd: k.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        let timeout = if work > 0 || (backend.session.is_some() && a.pps == 0) {
            0
        } else if backend.session.is_some() {
            1
        } else {
            50
        };
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
        let mut index = first_kick;
        for q in &backend.queues {
            if let Some(k) = &q.kick {
                if fds[index].revents & libc::POLLIN != 0 {
                    queue::drain_kick(k)?;
                }
                index += 1;
            }
        }
    }
    println!(
        "{}",
        json!({"event":"backend_stopped", "rx_completed":backend.rx_completed, "tx_completed":backend.tx_completed,
        "iotlb_updates":backend.iotlb_updates, "iotlb_invalidations":backend.iotlb_invalidations, "queue_stops":backend.queue_stops,
        "rx_notifications":backend.queues[0].notifications, "tx_notifications":backend.queues[1].notifications})
    );
    Ok(())
}
