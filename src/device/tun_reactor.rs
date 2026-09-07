//! Event-driven, sharded utun reactor for macOS.
//!
//! Three throughput levers the plain single-device backend does not provide:
//!
//! 1. **kqueue (`EVFILT_READ`)** — each shard blocks in one `kevent` call
//!    instead of polling/`sleep`-ing, then drains every queued datagram.
//! 2. **Core pinning + sharding** — one utun unit per worker thread, each pinned
//!    (best-effort) to its own core. This matches the frame pool's `!Send`,
//!    single-core design: every pool stays confined to its own thread.
//! 3. **Batching** — each wake reads up to [`FRAMES_PER_BATCH`] frames into a
//!    preallocated `Vec` and hands the whole batch to the handler in one shot;
//!    TX frames are written back-to-back in a single `send` call.
//!
//! This is an opt-in layer: the plain backend (one device, one thread, direct
//! `recv`/`send`) stays available for A/B comparison.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::device::{DefaultDevice, Device, Error, PacketBuf};

/// Datagrams read per kqueue wake (matches the pool size, so one wake can drain
/// the entire pool).
const FRAMES_PER_BATCH: usize = 256;

/// kqueue wait timeout; the stop flag is checked at least this often.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A utun device handed off to exactly one worker thread.
///
/// The inner device is `!Send` — its frame pool holds raw pointers and is
/// single-core by design. Moving it *once*, to the single thread that will use
/// it, is sound: the worker never shares it (the pool is also `!Sync`, so no
/// handle can be shared either), and no `PacketBuf` can escape that thread
/// (`PacketBuf` is `!Send` for the same reason). This wrapper records that
/// invariant.
struct SendDevice(DefaultDevice);

// SAFETY: see the `SendDevice` invariant above; the device is used on exactly
// one thread, never aliased.
unsafe impl Send for SendDevice {}

/// Sharded, kqueue-driven utun reactor.
pub struct UtunReactor {
    devices: Vec<SendDevice>,
    names: Vec<String>,
}

impl UtunReactor {
    /// One device, one worker thread: the event-driven single-shard baseline.
    pub fn single(mtu: usize) -> Result<Self, Error> {
        Self::sharded(1, mtu)
    }

    /// Open `shards` utun devices (unit 0 = "next available unit" on macOS).
    pub fn sharded(shards: usize, mtu: usize) -> Result<Self, Error> {
        assert!(shards > 0, "shards must be > 0");

        let mut devices = Vec::with_capacity(shards);
        let mut names = Vec::with_capacity(shards);
        for _ in 0..shards {
            let device = DefaultDevice::new_with_mtu(0, mtu)?;
            let name = device.name()?;
            devices.push(SendDevice(device));
            names.push(name);
        }
        Ok(Self { devices, names })
    }

    /// Interface names, one per shard (e.g. `["utun0", "utun1"]`). The caller
    /// must configure each interface externally (`ifconfig`).
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Spawn one pinned worker thread per device and block until `stop` is set.
    ///
    /// Each worker registers its device with its own kqueue, waits for
    /// `EVFILT_READ`, batch-reads up to [`FRAMES_PER_BATCH`] frames, and calls
    /// `handler(shard, device, rx)`. The handler owns packet semantics: echoing
    /// is `device.send(rx)`; frames it does not send are recycled automatically
    /// on the next batch read.
    ///
    /// `handler` is shared across shards, so it must be `Sync` (use atomics for
    /// cross-shard accounting). The first shard to hit an I/O error records it,
    /// signals `stop`, and the error is returned once all workers have exited.
    pub fn run<F>(&mut self, stop: Arc<AtomicBool>, handler: F) -> io::Result<()>
    where
        F: Fn(usize, &mut DefaultDevice, &mut Vec<PacketBuf>) + Sync,
    {
        let first_error: Mutex<Option<io::Error>> = Mutex::new(None);

        std::thread::scope(|scope| {
            for (shard, wrapped) in self.devices.drain(..).enumerate() {
                let stop = &stop;
                let handler = &handler;
                let first_error = &first_error;
                scope.spawn(move || {
                    if let Err(e) = shard_loop(shard, wrapped, stop, handler) {
                        let mut guard = first_error.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(e);
                        }
                        // Ask the remaining shards to wind down.
                        stop.store(true, Ordering::Relaxed);
                    }
                });
            }
        });

        match first_error.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Best-effort core affinity via the mach `THREAD_AFFINITY_POLICY`.
///
/// macOS has no public "pin to exactly this CPU" API. On Apple Silicon the
/// kernel rejects this legacy policy with `KERN_NOT_SUPPORTED`, and true
/// pinning requires a kernel extension. Treat this as an advisory hint only:
/// failures are non-fatal and the reactor ignores them.
#[allow(deprecated)] // `mach_thread_self` is stable ABI despite libc's deprecation
pub fn pin_thread_to_core(core: usize) -> io::Result<()> {
    let policy = libc::thread_affinity_policy {
        // Tag 0 means "no affinity"; use `core + 1` as a best-effort hint.
        affinity_tag: (core as libc::integer_t).wrapping_add(1),
    };
    // SAFETY: `policy` is a valid `thread_affinity_policy` for the
    // `THREAD_AFFINITY_POLICY` flavor, applied to the calling thread.
    let result = unsafe {
        libc::thread_policy_set(
            libc::mach_thread_self(),
            libc::THREAD_AFFINITY_POLICY as libc::thread_policy_flavor_t,
            &policy as *const libc::thread_affinity_policy as *mut libc::integer_t,
            libc::THREAD_AFFINITY_POLICY_COUNT,
        )
    };
    if result != libc::KERN_SUCCESS {
        // `result` is a mach kern_return, not an errno — keep it as a message,
        // not a bogus OS error.
        return Err(io::Error::other(format!(
            "thread_policy_set returned kern_return {result}"
        )));
    }
    Ok(())
}

fn shard_loop<F>(
    shard: usize,
    wrapped: SendDevice,
    stop: &AtomicBool,
    handler: &F,
) -> io::Result<()>
where
    F: Fn(usize, &mut DefaultDevice, &mut Vec<PacketBuf>) + Sync,
{
    // This thread now owns the device exclusively; no handle leaves the thread.
    let SendDevice(mut device) = wrapped;

    // Best-effort: confine this shard to its own core.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let _ = pin_thread_to_core(shard % cores);

    let kq = Kqueue::new()?;
    kq.register_read(device.as_raw_fd(), shard)?;

    let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
    let mut rx: Vec<PacketBuf> = Vec::with_capacity(FRAMES_PER_BATCH);

    while !stop.load(Ordering::Relaxed) {
        let n = kq.wait(&mut events, Some(STOP_POLL_INTERVAL))?;
        if n == 0 {
            continue; // timeout: re-check the stop flag
        }

        // The fd is readable: drain everything the kernel has queued in one
        // tight batch of reads (utun is one datagram per syscall, so this batch
        // replaces that many kqueue wakeups).
        device.recv(FRAMES_PER_BATCH, &mut rx)?;
        if !rx.is_empty() {
            handler(shard, &mut device, &mut rx);
        }
    }
    Ok(())
}

/// Thin RAII wrapper around a kqueue descriptor.
struct Kqueue(OwnedFd);

impl Kqueue {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd >= 0` and freshly created.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Register `fd` for level-triggered read notifications, tagged with `udata`.
    fn register_read(&self, fd: RawFd, udata: usize) -> io::Result<()> {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = fd as libc::uintptr_t;
        ev.filter = libc::EVFILT_READ;
        ev.flags = libc::EV_ADD | libc::EV_ENABLE;
        ev.udata = udata as *mut libc::c_void;
        // SAFETY: `ev` is a valid change entry for this kqueue.
        let ret = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait for up to `events.len()` events, bounded by `timeout` (or forever
    /// when `None`). Returns the number of events delivered.
    fn wait(&self, events: &mut [libc::kevent], timeout: Option<Duration>) -> io::Result<usize> {
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let timeout_ptr = match &ts {
            Some(t) => t as *const libc::timespec,
            None => std::ptr::null(),
        };
        // SAFETY: `events` is a valid output buffer of `nevents` entries.
        let ret = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                timeout_ptr,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::{Kqueue, pin_thread_to_core};
    use std::time::Duration;

    #[test]
    fn kqueue_wakes_when_fd_becomes_readable() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid 2-slot output array for `socketpair`.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) },
            0
        );
        let (read_end, write_end) = (fds[0], fds[1]);

        let kq = Kqueue::new().unwrap();
        kq.register_read(read_end, 0xCAFE).unwrap();

        // Nothing queued yet: the wait should time out with zero events.
        let mut events: [libc::kevent; 4] = unsafe { std::mem::zeroed() };
        assert_eq!(
            kq.wait(&mut events, Some(Duration::from_millis(10)))
                .unwrap(),
            0
        );

        // One datagram -> exactly one readable event, tagged with our udata.
        let msg = b"x";
        // SAFETY: `msg` is a valid 1-byte buffer to write.
        assert_eq!(
            unsafe { libc::write(write_end, msg.as_ptr() as *const libc::c_void, msg.len()) },
            1
        );
        let n = kq.wait(&mut events, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(n, 1);
        assert_eq!(events[0].filter, libc::EVFILT_READ);
        assert_eq!(events[0].ident as libc::c_int, read_end);
        assert_eq!(events[0].udata as usize, 0xCAFE);

        // SAFETY: valid fds.
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
    }

    #[test]
    fn affinity_hint_is_best_effort() {
        // On Apple Silicon the legacy affinity policy returns KERN_NOT_SUPPORTED;
        // on older Intel macOS it may succeed. The contract is only that the
        // call is safe and never panics — the reactor treats either outcome as
        // advisory.
        let _ = pin_thread_to_core(0);
    }
}
