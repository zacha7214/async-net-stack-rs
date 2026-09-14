use clap::{Parser, ValueEnum};
use serde_json::{json, Value};
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::hint::{black_box, spin_loop};
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Direct,
    HostCopy,
    GuestCopy,
    BothCopy,
}
impl Mode {
    fn host_copy(self) -> bool {
        matches!(self, Self::HostCopy | Self::BothCopy)
    }
    fn guest_copy(self) -> bool {
        matches!(self, Self::GuestCopy | Self::BothCopy)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Wait {
    Spin,
    Pipe,
}

#[derive(Parser, Debug)]
#[command(about = "Shared-memory NIC ownership and copy-cost lab (two native processes, no VM)")]
struct Args {
    #[arg(long, value_enum, default_value = "direct")]
    mode: Mode,
    #[arg(long, value_enum, default_value = "spin")]
    wait: Wait,
    #[arg(long, default_value_t = 1_000_000)]
    packets: u64,
    /// Warmup packets; fully drained before measurement.
    #[arg(long, default_value_t = 100_000)]
    warmup: u64,
    /// Ethernet test-frame bytes, excluding FCS, preamble and IFG.
    #[arg(long, default_value_t = 1500)]
    size: usize,
    #[arg(long, default_value_t = 1024)]
    ring: usize,
    #[arg(long, default_value_t = 64)]
    batch: usize,
    /// Bytes between frame starts; zero rounds size up to a 128-byte boundary.
    #[arg(long, default_value_t = 0)]
    stride: usize,
    /// Headers checks identity/length; full additionally reads every payload byte.
    #[arg(long, default_value = "headers", value_parser = ["headers", "full"])]
    verify: String,
    /// Fail after this many seconds without progress.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    // Private child protocol: only the parent creates/initializes this mapping.
    #[arg(long, hide = true)]
    worker: Option<PathBuf>,
}

#[repr(C, align(128))]
struct Counter(AtomicU64);

#[repr(C)]
struct Control {
    published: Counter,
    completed: Counter,
    ready: Counter,
    phase: Counter,
    phase_done: Counter,
    armed: Counter,
}
impl Control {
    fn new() -> Self {
        Self {
            published: Counter(AtomicU64::new(0)),
            completed: Counter(AtomicU64::new(0)),
            ready: Counter(AtomicU64::new(0)),
            phase: Counter(AtomicU64::new(0)),
            phase_done: Counter(AtomicU64::new(0)),
            armed: Counter(AtomicU64::new(0)),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    offset: u64,
    sequence: u64,
    len: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Stats {
    packets: u64,
    batches: u64,
    empty_polls: u64,
    notifications: u64,
    notification_full: u64,
    poll_calls: u64,
    read_calls: u64,
    wall_ns: u64,
    user_ns: u64,
    system_ns: u64,
}
impl Stats {
    fn json(self) -> Value {
        json!({"packets": self.packets, "batches": self.batches,
            "packets_per_batch": self.packets as f64 / self.batches.max(1) as f64,
            "empty_polls": self.empty_polls, "notification_writes": self.notifications,
            "notification_eagain": self.notification_full, "poll_calls": self.poll_calls,
            "read_calls": self.read_calls, "wall_seconds": self.wall_ns as f64 / 1e9,
            "user_cpu_seconds": self.user_ns as f64 / 1e9,
            "system_cpu_seconds": self.system_ns as f64 / 1e9})
    }
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    ring: usize,
    stride: usize,
    payload: usize,
    bytes: usize,
}
const DESCRIPTORS: usize = 4096;
const REPORT: usize = 1024;

impl Layout {
    fn new(a: &Args) -> Result<Self> {
        if !a.ring.is_power_of_two() || a.ring > 65536 || a.ring < 2 {
            return Err("ring must be a power of two in 2..=65536".into());
        }
        if a.batch == 0 || a.batch > a.ring || !(64..=9000).contains(&a.size) {
            return Err("batch must be in 1..=ring and size in 64..=9000".into());
        }
        if a.packets == 0
            || a.packets.checked_mul(a.size as u64 * 2).is_none()
            || a.packets
                .checked_add(a.warmup)
                .filter(|n| *n <= u64::MAX / 2)
                .is_none()
            || a.timeout == 0
            || a.timeout > 3600
        {
            return Err(
                "packets must be positive, total <= u64::MAX/2, timeout in 1..=3600".into(),
            );
        }
        let stride = if a.stride == 0 {
            (a.size + 127) & !127
        } else {
            a.stride
        };
        if stride < a.size || stride > 65536 || stride % 128 != 0 {
            return Err("stride must fit size and be a multiple of 128, at most 65536".into());
        }
        let payload = (DESCRIPTORS + a.ring * size_of::<Descriptor>() + 4095) & !4095;
        let bytes = a
            .ring
            .checked_mul(stride)
            .and_then(|n| payload.checked_add(n))
            .ok_or("mapping size overflows address space")?;
        if bytes > 512 * 1024 * 1024 {
            return Err("mapping is limited to 512 MiB".into());
        }
        Ok(Self {
            ring: a.ring,
            stride,
            payload,
            bytes,
        })
    }

    fn index(self, sequence: u64) -> usize {
        (sequence & (self.ring as u64 - 1)) as usize
    }
    fn offset(self, sequence: u64) -> usize {
        self.payload + self.index(sequence) * self.stride
    }
    fn validate(self, d: Descriptor, sequence: u64, size: usize) -> Result<usize> {
        // Exact slot matching also prevents a malformed descriptor aliasing an
        // outstanding frame. Never trust an offset before validating it.
        if d.sequence != sequence
            || d.len as usize != size
            || d.reserved != 0
            || d.offset != self.offset(sequence) as u64
        {
            return Err(format!("invalid descriptor at sequence {sequence}").into());
        }
        Ok(d.offset as usize)
    }
}

struct Mapping {
    ptr: NonNull<u8>,
    layout: Layout,
}
impl Mapping {
    fn open(file: &File, layout: Layout) -> Result<Self> {
        if file.metadata()?.len() != layout.bytes as u64 {
            return Err("shared file length does not match configuration".into());
        }
        // SAFETY: file has the checked size and remains mapped until Drop.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                layout.bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).ok_or("null mapping")?,
            layout,
        })
    }
    fn control(&self) -> &Control {
        // SAFETY: mmap is page aligned, parent initializes Control before spawn;
        // only its atomic fields are shared, and the mapping outlives this ref.
        unsafe { &*self.ptr.as_ptr().cast::<Control>() }
    }
    fn descriptor(&self, sequence: u64) -> *mut Descriptor {
        // SAFETY: the masked index lies in the descriptor array.
        unsafe {
            self.ptr
                .as_ptr()
                .add(DESCRIPTORS)
                .cast::<Descriptor>()
                .add(self.layout.index(sequence))
        }
    }
    fn frame(&self, sequence: u64) -> *mut u8 {
        // SAFETY: each slot has stride >= size bytes inside the mapping.
        unsafe { self.ptr.as_ptr().add(self.layout.offset(sequence)) }
    }
    fn report(&self) -> *mut Stats {
        // SAFETY: fixed, aligned storage disjoint from Control and descriptors.
        unsafe { self.ptr.as_ptr().add(REPORT).cast::<Stats>() }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: no local references survive self; the other process owns its
        // own mapping. The parent reaps its child before unmapping on errors.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.layout.bytes);
        }
    }
}

struct TempFile(PathBuf);
impl TempFile {
    fn create(layout: Layout) -> Result<(Self, File)> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("shm-nic-{}-{stamp}", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let guard = Self(path);
        file.set_len(layout.bytes as u64)?;
        Ok((guard, file))
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Doorbell {
    read: RawFd,
    write: RawFd,
    mode: Wait,
    closed: Cell<bool>,
}
impl Doorbell {
    // The caller retains the owned handles for this object's entire lifetime.
    fn new(read: RawFd, write: RawFd, mode: Wait) -> Result<Self> {
        for fd in [read, write] {
            // SAFETY: both fds are open pipe endpoints, owned by caller.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(Self {
            read,
            write,
            mode,
            closed: Cell::new(false),
        })
    }
    fn notify(&self, s: &mut Stats) -> Result<()> {
        if self.mode == Wait::Spin {
            return Ok(());
        }
        if self.closed.get() {
            return Err("peer closed notification pipe".into());
        }
        loop {
            let token = 1u64;
            // SAFETY: write reads exactly 8 initialized bytes. This is <= PIPE_BUF.
            let n = unsafe { libc::write(self.write, (&token as *const u64).cast(), 8) };
            if n == 8 {
                s.notifications += 1;
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => continue,
                // A full pipe already holds a wakeup. Counters, not tokens,
                // identify work, so coalescing here cannot lose a packet.
                io::ErrorKind::WouldBlock => {
                    s.notification_full += 1;
                    return Ok(());
                }
                _ => return Err(e.into()),
            }
        }
    }
    fn idle(&self, s: &mut Stats) -> Result<()> {
        s.empty_polls += 1;
        if self.mode == Wait::Spin {
            spin_loop();
            return Ok(());
        }
        if self.closed.get() {
            return Err("peer closed notification pipe".into());
        }
        let mut pollfd = libc::pollfd {
            fd: self.read,
            events: libc::POLLIN,
            revents: 0,
        };
        s.poll_calls += 1;
        // Bounded wait permits timeout checks even when the peer disappears.
        let n = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        } else if n > 0 {
            let mut tokens = [0u8; 4096];
            s.read_calls += 1;
            let n = unsafe { libc::read(self.read, tokens.as_mut_ptr().cast(), tokens.len()) };
            if n == 0 {
                // The peer may have completed its final batch between our
                // counter check and this read. Recheck ownership once first.
                self.closed.set(true);
                return Ok(());
            }
            if n < 0 {
                let e = io::Error::last_os_error();
                if !matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(e.into());
                }
            }
        }
        // Always return to an acquire load of the ring after draining tokens.
        Ok(())
    }
}

#[derive(Default)]
struct Stall(Option<Instant>);
impl Stall {
    fn check(&mut self, timeout: u64) -> Result<()> {
        if self.0.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(timeout) {
            return Err("peer made no progress before --timeout".into());
        }
        Ok(())
    }
    fn progress(&mut self) {
        self.0 = None;
    }
}

fn cpu_time() -> io::Result<(u64, u64)> {
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let u = unsafe { usage.assume_init() };
    let ns = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
    Ok((ns(u.ru_utime), ns(u.ru_stime)))
}

// Local experimental Ethernet type, with fixed 32-byte test header.
const PREFIX: [u8; 16] = [2, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 1, 0x88, 0xb5, 0x53, 0x4d];
#[inline(never)]
fn generate(frame: &mut [u8], sequence: u64) {
    for (i, b) in frame[32..].iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(sequence as u8);
    }
    frame[..16].copy_from_slice(&PREFIX);
    frame[16..24].copy_from_slice(&sequence.to_le_bytes());
    let len = frame.len() as u32;
    frame[24..28].copy_from_slice(&len.to_le_bytes());
    frame[28..32].fill(0);
}
#[inline(never)]
fn copy_packet(from: &[u8], to: &mut [u8]) {
    // Keep the extra copy observable even when the consumer only checks headers.
    black_box(to).copy_from_slice(black_box(from));
}
fn validate_frame(frame: &[u8], sequence: u64, full: bool) -> bool {
    frame.len() >= 32
        && frame[..16] == PREFIX
        && frame[16..24] == sequence.to_le_bytes()
        && frame[24..28] == (frame.len() as u32).to_le_bytes()
        && frame[28..32] == [0; 4]
        && (!full
            || frame[32..]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == (i as u8).wrapping_add(sequence as u8)))
}

fn run(
    map: &Mapping,
    a: &Args,
    bell: &Doorbell,
    producer: bool,
    range: std::ops::Range<u64>,
    phase: u64,
) -> Result<Stats> {
    let c = map.control();
    let mut stats = Stats::default();
    let mut stall = Stall::default();
    let start = range.start;
    let end = range.end;
    let mut cursor = start;
    let copy = if producer {
        a.mode.host_copy()
    } else {
        a.mode.guest_copy()
    };
    let mut scratch = vec![0u8; if copy { a.batch * a.size } else { 0 }];
    // Allocate and touch staging buffers before the timer. Parent pre-faults RAM.
    for byte in scratch.iter_mut().step_by(4096) {
        *black_box(byte) = 0;
    }
    if producer {
        wait_counter(&c.armed.0, phase, a.timeout)?;
    } else {
        c.armed.0.store(phase, Ordering::Release);
        while c.phase.0.load(Ordering::Acquire) != phase {
            stall.check(a.timeout)?;
            spin_loop();
        }
        stall.progress();
    }
    let cpu = cpu_time()?;
    let time = Instant::now();
    if producer {
        c.phase.0.store(phase, Ordering::Release);
    }
    // Peer index is cached until we need more free/available entries.
    let mut peer = start;
    while cursor < end {
        let mut available = if producer {
            a.ring as u64 - (cursor - peer)
        } else {
            peer - cursor
        };
        if available < a.batch as u64 {
            peer = if producer {
                c.completed.0.load(Ordering::Acquire)
            } else {
                c.published.0.load(Ordering::Acquire)
            };
            if (producer && (peer > cursor || cursor - peer > a.ring as u64))
                || (!producer && (peer < cursor || peer - cursor > a.ring as u64 || peer > end))
            {
                return Err("invalid shared ring index".into());
            }
            available = if producer {
                a.ring as u64 - (cursor - peer)
            } else {
                peer - cursor
            };
        }
        let n = available.min(a.batch as u64).min(end - cursor);
        if n == 0 {
            stall.check(a.timeout)?;
            bell.idle(&mut stats)?;
            continue;
        }
        stall.progress();
        for i in 0..n {
            let seq = cursor + i;
            if producer {
                // SAFETY: completed acquire grants exclusive ownership of this
                // slot. No peer may read it until published release below.
                let frame = unsafe { std::slice::from_raw_parts_mut(map.frame(seq), a.size) };
                if copy {
                    let staging = &mut scratch[i as usize * a.size..(i as usize + 1) * a.size];
                    generate(staging, seq);
                    copy_packet(staging, frame);
                } else {
                    generate(frame, seq);
                }
                unsafe {
                    map.descriptor(seq).write(Descriptor {
                        offset: map.layout.offset(seq) as u64,
                        sequence: seq,
                        len: a.size as u32,
                        reserved: 0,
                    });
                }
            } else {
                // SAFETY: published acquire grants read ownership until this
                // batch completes. Checked descriptor identifies this exact slot.
                let d = unsafe { map.descriptor(seq).read() };
                map.layout.validate(d, seq, a.size)?;
                let frame = unsafe { std::slice::from_raw_parts(map.frame(seq), a.size) };
                let input = if copy {
                    let staging = &mut scratch[i as usize * a.size..(i as usize + 1) * a.size];
                    copy_packet(frame, staging);
                    &*staging
                } else {
                    frame
                };
                if !validate_frame(black_box(input), seq, a.verify == "full") {
                    return Err(format!("corrupt packet at sequence {seq}").into());
                }
            }
        }
        cursor += n;
        stats.packets += n;
        stats.batches += 1;
        if producer {
            c.published.0.store(cursor, Ordering::Release);
        } else {
            c.completed.0.store(cursor, Ordering::Release);
        }
        bell.notify(&mut stats)?;
    }
    // Producer's measurement includes delivery and final completion, not merely
    // enqueue acceptance. Consumer has released all packet references by here.
    while producer && c.completed.0.load(Ordering::Acquire) != end {
        stall.check(a.timeout)?;
        bell.idle(&mut stats)?;
    }
    stats.wall_ns = time.elapsed().as_nanos() as u64;
    let after = cpu_time()?;
    stats.user_ns = after.0.saturating_sub(cpu.0);
    stats.system_ns = after.1.saturating_sub(cpu.1);
    Ok(stats)
}

fn wait_counter(counter: &AtomicU64, value: u64, timeout: u64) -> Result<()> {
    let mut stall = Stall::default();
    while counter.load(Ordering::Acquire) != value {
        stall.check(timeout)?;
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn worker(a: &Args, layout: Layout) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(a.worker.as_ref().unwrap())?;
    let map = Mapping::open(&file, layout)?;
    let c = map.control();
    let bell = Doorbell::new(libc::STDIN_FILENO, libc::STDOUT_FILENO, a.wait)?;
    c.ready.0.store(1, Ordering::Release);
    run(&map, a, &bell, false, 0..a.warmup, 1)?;
    c.phase_done.0.store(1, Ordering::Release);
    let stats = run(&map, a, &bell, false, a.warmup..a.warmup + a.packets, 2)?;
    // SAFETY: only child writes report; parent reads after phase_done acquire.
    unsafe {
        map.report().write(stats);
    }
    c.phase_done.0.store(2, Ordering::Release);
    Ok(())
}

pub fn main() -> Result<()> {
    let a = Args::parse();
    let layout = Layout::new(&a)?;
    if a.worker.is_some() {
        return worker(&a, layout);
    }
    let (temp, file) = TempFile::create(layout)?;
    let map = Mapping::open(&file, layout)?;
    // SAFETY: no child yet; initialize atomics and pre-fault every mapping page.
    unsafe {
        map.ptr.as_ptr().write_bytes(0, layout.bytes);
        map.ptr.as_ptr().cast::<Control>().write(Control::new());
    }
    let mut child = ChildGuard(
        Command::new(std::env::current_exe()?)
            .args(std::env::args_os().skip(1))
            .arg("--worker")
            .arg(&temp.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let bell = Doorbell::new(
        child.0.stdout.as_ref().unwrap().as_raw_fd(),
        child.0.stdin.as_ref().unwrap().as_raw_fd(),
        a.wait,
    )?;
    let c = map.control();
    wait_counter(&c.ready.0, 1, a.timeout)?;
    // Both processes now have independent MAP_SHARED mappings. No pathname is
    // needed during measurement; closing files does not invalidate the pages.
    std::fs::remove_file(&temp.0)?;
    drop(file);
    run(&map, &a, &bell, true, 0..a.warmup, 1)?;
    wait_counter(&c.phase_done.0, 1, a.timeout)?;
    let host = run(&map, &a, &bell, true, a.warmup..a.warmup + a.packets, 2)?;
    wait_counter(&c.phase_done.0, 2, a.timeout)?;
    let guest = unsafe { map.report().read() };
    if !child.0.wait()?.success() {
        return Err("worker exited unsuccessfully".into());
    }
    let seconds = host.wall_ns as f64 / 1e9;
    let bytes = a.packets as u128 * a.size as u128;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "experiment": "two-native-process-shared-memory-rx", "real_vm": false,
            "physical_dma": false, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "config": {"mode": a.mode.to_possible_value().unwrap().get_name(),
                "wait": a.wait.to_possible_value().unwrap().get_name(), "packets": a.packets,
                "warmup_packets": a.warmup, "size": a.size, "ring": a.ring, "batch": a.batch,
                "stride": layout.stride, "mapping_bytes": layout.bytes, "verify": a.verify},
            "measurement": {"completed_packets": guest.packets, "pending": c.published.0.load(Ordering::Acquire)
                - c.completed.0.load(Ordering::Acquire), "packets_per_second": a.packets as f64 / seconds,
                "frame_gbps": bytes as f64 * 8.0 / seconds / 1e9,
                "extra_payload_copy_bytes": bytes * (a.mode.host_copy() as u128 + a.mode.guest_copy() as u128),
                "cpu_ns_per_packet": (host.user_ns + host.system_ns + guest.user_ns + guest.system_ns) as f64 / a.packets as f64},
            "host": host.json(), "guest_model": guest.json()
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptors_reject_wrong_slot_length_and_sequence() {
        let a = Args::parse_from(["test", "--ring", "2"]);
        let l = Layout::new(&a).unwrap_err();
        assert!(l.to_string().contains("batch"));
        let a = Args::parse_from(["test", "--ring", "2", "--batch", "1"]);
        let l = Layout::new(&a).unwrap();
        let mut d = Descriptor {
            offset: l.offset(3) as u64,
            sequence: 3,
            len: a.size as u32,
            reserved: 0,
        };
        assert!(l.validate(d, 3, a.size).is_ok());
        assert_eq!(l.offset(1), l.offset(3));
        assert!(l.validate(d, 1, a.size).is_err());
        d.offset = u64::MAX;
        assert!(l.validate(d, 3, a.size).is_err());
        d.offset = l.offset(3) as u64;
        d.len += 1;
        assert!(l.validate(d, 3, a.size).is_err());
    }
    #[test]
    fn detects_stale_headers_and_payload_corruption() {
        for size in [64, 1500, 9000] {
            let mut frame = vec![0; size];
            generate(&mut frame, 257);
            assert!(validate_frame(&frame, 257, true));
            assert!(!validate_frame(&frame, 258, false));
            frame[size - 1] ^= 1;
            assert!(!validate_frame(&frame, 257, true));
            assert!(validate_frame(&frame, 257, false));
        }
    }
    #[test]
    fn control_and_report_do_not_overlap() {
        assert!(size_of::<Control>() <= REPORT);
        assert!(REPORT + size_of::<Stats>() <= DESCRIPTORS);
        let a = Args::parse_from(["test", "--ring", "65536", "--stride", "65536"]);
        assert!(Layout::new(&a).is_err());
    }
}
