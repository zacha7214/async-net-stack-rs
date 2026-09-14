//! Experimental Linux TUN fallback: batched io_uring READ/WRITE requests.
//! This amortizes submission syscalls; TUN still copies payload bytes. Fixed
//! file registration removes per-request fd lookup, but is not zero-copy.
//! Requires Linux >= 6.0 for synchronous cancellation during shutdown.
mod ring;
use crate::device::buffer_pool::FramePool;
use crate::device::{DefaultDevice, Device, Error, PacketBuf};
use ring::{Ring, Sqe};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

#[derive(Clone, Copy, Debug)]
pub struct UringConfig {
    /// Total outstanding reads + writes (power of two, 2..=4096).
    pub entries: usize,
    /// Outstanding RX budget. Zero gives a TX-only submission experiment.
    pub rx_depth: usize,
}
impl Default for UringConfig {
    fn default() -> Self {
        Self {
            entries: 128,
            rx_depth: 64,
        }
    }
}
impl UringConfig {
    fn validate(self) -> io::Result<()> {
        if !self.entries.is_power_of_two()
            || !(2..=4096).contains(&self.entries)
            || self.rx_depth >= self.entries
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid io_uring depth",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct UringStats {
    pub enter_calls: u64,
    pub rx_submitted: u64,
    pub rx_completed: u64,
    pub tx_submitted: u64,
    pub tx_completed: u64,
    pub tx_completed_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub submit_errors: u64,
    pub last_errno: Option<i32>,
}
struct Pending {
    buf: PacketBuf,
    read: bool,
}

pub struct UringTunDevice {
    ring: Ring,
    // Retained through ring teardown. Packet memory lives in pending/ready/pool.
    fd: OwnedFd,
    name: String,
    pool: FramePool,
    pending: Vec<Option<Pending>>,
    free: Vec<usize>,
    ready: VecDeque<PacketBuf>,
    cfg: UringConfig,
    rx_pending: usize,
    tx_pending: usize,
    stats: UringStats,
    error: Option<io::Error>,
}
impl UringTunDevice {
    pub fn new(name: &str, mtu: usize, cfg: UringConfig) -> Result<Self, Error> {
        cfg.validate()?;
        Self::from_tun(DefaultDevice::new_with_mtu(name, mtu)?, cfg).map_err(Error::from)
    }
    pub fn from_tun(tun: DefaultDevice, cfg: UringConfig) -> io::Result<Self> {
        cfg.validate()?;
        let (fd, name, pool) = tun.into_io_parts();
        Self::from_parts(fd, name, pool, cfg)
    }
    fn from_parts(
        fd: OwnedFd,
        name: String,
        pool: FramePool,
        cfg: UringConfig,
    ) -> io::Result<Self> {
        cfg.validate()?;
        if cfg.rx_depth >= pool.num_frames() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RX depth must leave frames for TX",
            ));
        }
        let ring = Ring::new(cfg.entries as u32, fd.as_raw_fd())?;
        let entries = ring.entries();
        Ok(Self {
            ring,
            fd,
            name,
            pool,
            pending: (0..entries).map(|_| None).collect(),
            free: (0..entries).rev().collect(),
            ready: VecDeque::with_capacity(cfg.rx_depth),
            cfg,
            rx_pending: 0,
            tx_pending: 0,
            stats: UringStats::default(),
            error: None,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn stats(&self) -> UringStats {
        self.stats
    }
    pub fn pending_tx(&self) -> usize {
        self.tx_pending
    }
    /// Submit queued work and reap completions. Never waits for RX traffic or
    /// TX space. Call regularly, including after the last send, to reap TX.
    pub fn progress(&mut self) -> io::Result<()> {
        self.reap();
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        if self.free.len() < self.pending.len() {
            self.stats.enter_calls += 1;
            if let Err(err) = self.ring.submit() {
                self.stats.submit_errors += 1;
                self.stats.last_errno = err.raw_os_error();
                return Err(err);
            }
            self.reap();
        }
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        Ok(())
    }
    fn record_error(&mut self, err: io::Error) {
        self.stats.last_errno = err.raw_os_error();
        if self.error.is_none() {
            self.error = Some(err);
        }
    }
    fn reap(&mut self) {
        while let Some(cqe) = self.ring.pop() {
            let id = cqe.user_data as usize;
            let mut op = self
                .pending
                .get_mut(id)
                .and_then(Option::take)
                .expect("kernel returned a live request id");
            self.free.push(id);
            if op.read {
                self.rx_pending -= 1;
            } else {
                self.tx_pending -= 1;
            }
            if cqe.result < 0 {
                let errno = -cqe.result;
                if op.read && matches!(errno, libc::EAGAIN | libc::EINTR) {
                    continue;
                }
                if op.read {
                    self.stats.rx_errors += 1;
                } else {
                    self.stats.tx_errors += 1;
                }
                self.record_error(io::Error::from_raw_os_error(errno));
            } else if op.read {
                if cqe.result == 0 || cqe.result as usize >= op.buf.tail_capacity() {
                    self.stats.rx_errors += 1;
                    self.record_error(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty or possibly truncated TUN read",
                    ));
                } else {
                    op.buf.set_len(cqe.result as usize);
                    self.stats.rx_completed += 1;
                    self.ready.push_back(op.buf);
                }
            } else if cqe.result as usize != op.buf.len() {
                self.stats.tx_errors += 1;
                self.record_error(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short io_uring datagram write",
                ));
            } else {
                self.stats.tx_completed += 1;
                self.stats.tx_completed_bytes += cqe.result as u64;
            }
        }
        self.ring.release_completions();
    }
    fn queue(&mut self, mut buf: PacketBuf, read: bool) {
        let id = self.free.pop().expect("space checked");
        let offset = buf.data_offset();
        let address = unsafe { buf.as_mut_slice().as_mut_ptr().add(offset) } as u64;
        let len = if read { buf.tail_capacity() } else { buf.len() };
        self.pending[id] = Some(Pending { buf, read });
        self.ring.push(Sqe {
            opcode: if read { 22 } else { 23 },
            flags: 1,
            fd: 0,
            address,
            len: len as u32,
            user_data: id as u64,
            ..Sqe::default()
        });
        if read {
            self.rx_pending += 1;
            self.stats.rx_submitted += 1;
        } else {
            self.tx_pending += 1;
            self.stats.tx_submitted += 1;
        }
    }
}
impl AsRawFd for UringTunDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
impl Device for UringTunDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        self.reap();
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        while self.rx_pending + self.ready.len() < self.cfg.rx_depth
            && !self.free.is_empty()
            && self.ring.free_slots() > 0
        {
            let Some(idx) = self.pool.alloc() else {
                break;
            };
            self.queue(self.pool.packet_buf(idx, 0), true);
        }
        self.progress()?;
        for _ in 0..max.min(self.ready.len()) {
            out.push(self.ready.pop_front().unwrap());
        }
        Ok(out.len())
    }
    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        self.reap();
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let n = frames
            .len()
            .min(self.free.len())
            .min(self.ring.free_slots());
        if frames[..n]
            .iter()
            .any(|f| f.is_empty() || f.len() > u32::MAX as usize)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid io_uring TX frame",
            ));
        }
        for frame in &mut frames[..n] {
            self.queue(std::mem::take(frame), false);
        }
        // Once accepted, buffers stay owned by this backend even if enter
        // fails. Report the deferred failure on the next progress/recv/send.
        if let Err(err) = self.progress() {
            self.error = Some(err);
        }
        Ok(n)
    }
    fn alloc(&mut self) -> Option<PacketBuf> {
        let idx = self.pool.alloc()?;
        Some(self.pool.packet_buf(idx, 0))
    }
    fn alloc_batch(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> usize {
        self.pool.alloc_batch(max, out)
    }
    fn frame_size(&self) -> usize {
        self.pool.frame_size()
    }
}
impl Drop for UringTunDevice {
    fn drop(&mut self) {
        // A close alone is insufficient: io_uring teardown can be asynchronous.
        // Ordinary rings (no SQPOLL) cannot consume unpublished entries here.
        // On cancellation failure/timeout, quarantine outstanding handles so
        // neither the arena nor a foreign pool can reuse kernel-owned bytes.
        if self.ring.cancel_all().is_err() {
            for op in &mut self.pending {
                if let Some(op) = op.take() {
                    std::mem::forget(op);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;
    use std::time::{Duration, Instant};
    #[test]
    #[ignore = "requires Linux >= 6.0 with io_uring enabled; no root required"]
    fn datagrams_batch_complete_and_cancel_without_traffic() {
        let (a, b) = UnixDatagram::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let cfg = UringConfig {
            entries: 16,
            rx_depth: 4,
        };
        let mut dev = UringTunDevice::from_parts(
            a.into(),
            "test".into(),
            FramePool::new(32, 2048, 4096),
            cfg,
        )
        .unwrap();
        let mut tx = Vec::with_capacity(8);
        for i in 0..8u8 {
            let mut buf = dev.alloc().unwrap();
            buf.set_len(100);
            buf.as_mut_packet().fill(i);
            tx.push(buf);
        }
        assert_eq!(dev.send(&mut tx).unwrap(), 8);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = Vec::new();
        while received.len() < 8 || dev.pending_tx() != 0 {
            assert!(Instant::now() < deadline);
            dev.progress().unwrap();
            let mut bytes = [0; 200];
            if let Ok(n) = b.recv(&mut bytes) {
                assert_eq!(n, 100);
                received.push(bytes[0]);
            }
        }
        received.sort_unstable();
        assert_eq!(received, (0..8u8).collect::<Vec<_>>());
        let mut rx = Vec::new();
        dev.recv(4, &mut rx).unwrap();
        b.send(b"response").unwrap();
        while rx.is_empty() {
            assert!(Instant::now() < deadline);
            dev.recv(4, &mut rx).unwrap();
        }
        assert_eq!(rx[0].as_slice(), b"response");
        drop(dev); // pending reads MUST cancel even with no further traffic
        assert_eq!(rx[0].as_slice(), b"response");
    }
}
