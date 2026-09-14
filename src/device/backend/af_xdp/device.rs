//! Single-queue AF_XDP with bounded batches and explicit frame ownership.
use super::prog::{AttachMode, XskProgram};
use super::socket::XskSocket;
use super::sys::*;
use super::umem::{validate_ring_size, UMem};
use crate::device::buffer_pool::FramePool;
use crate::device::{Device, PacketBuf};
use std::io;
use std::os::fd::{AsRawFd, RawFd};

/// Copy policy. Auto lets the kernel fall back; ZeroCopy is a strict probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XdpMode {
    #[default]
    Auto,
    Copy,
    ZeroCopy,
}

#[derive(Clone, Debug)]
pub struct XdpConfig {
    pub frames: usize,
    pub chunk_size: usize,
    /// Additional UMEM headroom; RX always honors the descriptor's offset,
    /// which can also include the kernel's XDP_PACKET_HEADROOM.
    pub headroom: usize,
    pub fill_entries: usize,
    pub cq_entries: usize,
    pub rx_entries: usize,
    pub tx_entries: usize,
    pub attach: bool,
    /// Force generic/skb XDP. Auto binds in copy mode when this is selected.
    pub attach_generic: bool,
    pub max_queues: u32,
    pub mode: XdpMode,
    /// Maximum packets per recv/send/progress operation (1..=4096).
    pub batch_size: usize,
    /// Free frames withheld from RX refill so TX allocation can make progress.
    pub tx_reserve: usize,
}
impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            frames: 4096,
            chunk_size: 4096,
            headroom: 0,
            fill_entries: 2048,
            cq_entries: 2048,
            rx_entries: 1024,
            tx_entries: 1024,
            attach: true,
            attach_generic: false,
            max_queues: 64,
            mode: XdpMode::Auto,
            batch_size: 64,
            tx_reserve: 512,
        }
    }
}
impl XdpConfig {
    pub fn validate(&self, queue: u32) -> io::Result<()> {
        for entries in [
            self.fill_entries,
            self.cq_entries,
            self.rx_entries,
            self.tx_entries,
        ] {
            validate_ring_size(entries)?;
        }
        if self.frames < 2
            || self.tx_reserve >= self.frames
            || self.batch_size == 0
            || self.batch_size > 4096
            || !self.chunk_size.is_power_of_two()
            || self.chunk_size < 2048
            || self.chunk_size > u32::MAX as usize
            || self.headroom > self.chunk_size - 256
            || self.frames.checked_mul(self.chunk_size).is_none()
            || self.max_queues == 0
            || queue >= self.max_queues
            || (self.attach_generic && self.mode == XdpMode::ZeroCopy)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid XDP configuration",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct XdpCounters {
    pub rx_packets: u64,
    pub tx_submitted: u64,
    pub tx_completed: u64,
    pub tx_backpressure: u64,
    pub invalid_descriptors: u64,
}
const FREE: u8 = 0;
const RX_KERNEL: u8 = 1;
const APP: u8 = 2;
const TX_KERNEL: u8 = 3;

pub struct XdpDevice {
    // Detach before closing the socket; outstanding PacketBufs retain arena.
    prog: XskProgram,
    xsk: XskSocket,
    pool: FramePool,
    umem: UMem,
    state: Vec<u8>,
    rx_scratch: Vec<(u64, u32)>,
    tx_scratch: Vec<(u64, u32)>,
    cq_scratch: Vec<u64>,
    indices: Vec<usize>,
    fill_scratch: Vec<u64>,
    cfg: XdpConfig,
    rx_owned: usize,
    rx_target: usize,
    tx_pending: usize,
    attach_mode: AttachMode,
    counters: XdpCounters,
}
impl XdpDevice {
    pub fn new(ifname: &str, queue_id: u32) -> io::Result<Self> {
        Self::with_config(ifname, queue_id, &XdpConfig::default())
    }
    pub fn with_config(ifname: &str, queue_id: u32, cfg: &XdpConfig) -> io::Result<Self> {
        cfg.validate(queue_id)?;
        let name = std::ffi::CString::new(ifname)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in interface name"))?;
        let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if ifindex == 0 {
            return Err(io::Error::last_os_error());
        }
        let umem = UMem::new(
            cfg.frames,
            cfg.chunk_size,
            cfg.headroom,
            cfg.fill_entries,
            cfg.cq_entries,
            0,
        )?;
        // Virtio and other drivers may initialize their XDP receive resources
        // during attach. Bind only after that, with the redirect guard disabled.
        let mut prog = XskProgram::new(ifindex, cfg.max_queues)?;
        let attach_mode = if cfg.attach {
            prog.attach(cfg.attach_generic)?
        } else {
            AttachMode::None
        };
        if attach_mode == AttachMode::AlreadyAttached {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an XDP program is already attached; its XSKMAP is not this device's map",
            ));
        }
        if attach_mode == AttachMode::Generic && cfg.mode == XdpMode::ZeroCopy {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "generic XDP cannot provide zero-copy",
            ));
        }
        let flags = XDP_USE_NEED_WAKEUP
            | match cfg.mode {
                XdpMode::ZeroCopy => XDP_ZEROCOPY,
                XdpMode::Copy => XDP_COPY,
                XdpMode::Auto if attach_mode == AttachMode::Generic => XDP_COPY,
                XdpMode::Auto => 0,
            };
        let xsk = XskSocket::new(
            &umem,
            ifindex,
            queue_id,
            cfg.rx_entries,
            cfg.tx_entries,
            flags,
        )?;
        let pool = unsafe {
            FramePool::from_owned_region(
                umem.base_ptr(),
                umem.len_bytes(),
                cfg.chunk_size,
                cfg.frames,
                umem.arena_owner(),
            )
        };
        let batch = cfg.batch_size;
        let mut dev = Self {
            prog,
            xsk,
            pool,
            umem,
            state: vec![FREE; cfg.frames],
            rx_scratch: Vec::with_capacity(batch),
            tx_scratch: Vec::with_capacity(batch),
            cq_scratch: Vec::with_capacity(batch),
            indices: vec![0; batch],
            fill_scratch: vec![0; batch],
            cfg: cfg.clone(),
            rx_owned: 0,
            rx_target: cfg.fill_entries.min(cfg.frames - cfg.tx_reserve),
            tx_pending: 0,
            attach_mode,
            counters: XdpCounters::default(),
        };
        dev.refill();
        dev.prog.set_socket(queue_id, dev.xsk.fd())?;
        Ok(dev)
    }
    pub fn bind_flags(&self) -> libc::c_ushort {
        self.xsk.bind_flags()
    }
    pub fn need_wakeup_enabled(&self) -> bool {
        self.xsk.need_wakeup_enabled()
    }
    pub fn stats(&self) -> io::Result<XdpStatistics> {
        self.xsk.stats()
    }
    pub fn counters(&self) -> XdpCounters {
        self.counters
    }
    pub fn attach_mode(&self) -> AttachMode {
        self.attach_mode
    }
    pub fn is_zero_copy(&self) -> bool {
        self.bind_flags() & XDP_ZEROCOPY != 0
    }
    pub fn pending_tx(&self) -> usize {
        self.tx_pending
    }
    /// For explicitly attaching this device's program with bpftool when
    /// `attach=false`. A separate redirect program must use this map.
    pub fn map_fd(&self) -> RawFd {
        self.prog.map_fd()
    }
    pub fn program_fd(&self) -> RawFd {
        self.prog.program_fd()
    }

    /// Reap a bounded completion batch and drive RX/TX wakeups without waiting.
    pub fn progress(&mut self) -> usize {
        let n = self.drain_cq();
        self.refill();
        if self.tx_pending > 0 {
            self.xsk.wake_tx();
        }
        n
    }
    fn drain_cq(&mut self) -> usize {
        self.cq_scratch.clear();
        let n = self.umem.cq_pop(&mut self.cq_scratch, self.cfg.batch_size);
        let mut valid = 0;
        for &addr in &self.cq_scratch {
            let idx = (addr / self.cfg.chunk_size as u64) as usize;
            if self.state.get(idx) != Some(&TX_KERNEL) {
                self.counters.invalid_descriptors += 1;
                continue;
            }
            self.state[idx] = FREE;
            self.indices[valid] = idx;
            valid += 1;
        }
        self.pool.free_n(&self.indices[..valid]);
        self.tx_pending -= valid;
        self.counters.tx_completed += valid as u64;
        n
    }
    fn refill(&mut self) {
        // Only the unpublished free list can provide RX frames. The shared FQ
        // consumer does not tell us which buffers the hardware still owns.
        let mut budget = (self.rx_target - self.rx_owned)
            .min(self.pool.available().saturating_sub(self.cfg.tx_reserve));
        while budget > 0 {
            let limit = budget.min(self.indices.len());
            let n = self.pool.alloc_n(&mut self.indices[..limit]);
            for i in 0..n {
                self.fill_scratch[i] = (self.indices[i] * self.cfg.chunk_size) as u64;
            }
            let pushed = self.umem.fill_push(&self.fill_scratch[..n]);
            for &idx in &self.indices[..pushed] {
                self.state[idx] = RX_KERNEL;
            }
            self.pool.free_n(&self.indices[pushed..n]);
            self.rx_owned += pushed;
            budget -= pushed;
            if pushed < limit {
                break;
            }
        }
        // RX wakeup is poll(), not the TX sendto() kick. Check even when no
        // new entries were published: NEED_WAKEUP can change while idle.
        if !self.xsk.need_wakeup_enabled() || self.umem.fill_needs_wakeup() {
            self.xsk.wake_rx();
        }
    }
    #[cfg(test)]
    pub(crate) fn debug_umem(&self) -> &UMem {
        &self.umem
    }
    #[cfg(test)]
    pub(crate) fn debug_socket(&self) -> &XskSocket {
        &self.xsk
    }
    #[cfg(test)]
    pub(crate) fn debug_prog_map_fd(&self) -> RawFd {
        self.map_fd()
    }
}
impl AsRawFd for XdpDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.xsk.fd()
    }
}

impl Device for XdpDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        self.progress();
        self.rx_scratch.clear();
        self.xsk
            .rx_pop(&mut self.rx_scratch, max.min(self.cfg.batch_size));
        for &(addr, len) in &self.rx_scratch {
            let Some((idx, offset)) = decode_rx(addr, len, self.cfg.chunk_size, self.cfg.frames)
            else {
                self.counters.invalid_descriptors += 1;
                continue; // quarantine malformed descriptors, never alias frames
            };
            if self.state[idx] != RX_KERNEL {
                self.counters.invalid_descriptors += 1;
                continue;
            }
            self.rx_owned -= 1;
            self.state[idx] = APP;
            let mut buf = self.pool.packet_buf(idx, 0);
            buf.set_headroom(offset);
            buf.set_len(len as usize);
            out.push(buf);
        }
        self.counters.rx_packets += out.len() as u64;
        Ok(out.len())
    }
    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        self.drain_cq();
        let requested = frames.len().min(self.cfg.batch_size);
        let n = requested.min(self.xsk.tx_free_slots(requested));
        if n < requested {
            self.counters.tx_backpressure += 1;
        }
        // Validate before consuming anything; addresses from another UMEM must
        // never be interpreted as this socket's frame indices.
        for buf in &frames[..n] {
            if !buf.belongs_to(&self.pool) || buf.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TX needs a nonempty frame from this XDP device",
                ));
            }
        }
        self.tx_scratch.clear();
        for buf in &mut frames[..n] {
            let idx = buf.frame_index();
            self.tx_scratch.push((
                (idx * self.cfg.chunk_size + buf.data_offset()) as u64,
                buf.len() as u32,
            ));
            self.state[idx] = TX_KERNEL;
            std::mem::take(buf).into_parts();
        }
        let pushed = self.xsk.tx_push(&self.tx_scratch);
        assert_eq!(pushed, n, "single producer owns reserved TX slots");
        self.tx_pending += n;
        self.counters.tx_submitted += n as u64;
        if self.tx_pending > 0 {
            self.xsk.wake_tx();
        }
        Ok(n)
    }
    fn alloc(&mut self) -> Option<PacketBuf> {
        if self.pool.available() == 0 {
            self.progress();
        }
        let idx = self.pool.alloc()?;
        self.state[idx] = APP;
        let mut buf = self.pool.packet_buf(idx, 0);
        buf.set_headroom(self.cfg.headroom);
        Some(buf)
    }
    fn alloc_batch(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> usize {
        self.drain_cq();
        let n = self
            .pool
            .alloc_n(&mut self.indices[..max.min(self.cfg.batch_size)]);
        for &idx in &self.indices[..n] {
            self.state[idx] = APP;
            let mut buf = self.pool.packet_buf(idx, 0);
            buf.set_headroom(self.cfg.headroom);
            out.push(buf);
        }
        n
    }
    fn frame_size(&self) -> usize {
        self.cfg.chunk_size
    }
}

fn decode_rx(addr: u64, len: u32, chunk: usize, frames: usize) -> Option<(usize, usize)> {
    let addr = usize::try_from(addr).ok()?;
    let idx = addr / chunk;
    let offset = addr % chunk;
    (idx < frames && len > 0 && len as usize <= chunk - offset).then_some((idx, offset))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rx_offset_comes_from_descriptor() {
        assert_eq!(decode_rx(4096 + 256, 1500, 4096, 4), Some((1, 256)));
        assert_eq!(decode_rx(4000, 200, 4096, 4), None);
        assert_eq!(decode_rx(u64::MAX, 1, 4096, 4), None);
    }
    #[test]
    fn configuration_rejects_invalid_geometry_before_syscalls() {
        let mut cfg = XdpConfig::default();
        assert!(cfg.validate(0).is_ok());
        cfg.tx_reserve = cfg.frames;
        assert!(cfg.validate(0).is_err());
        cfg.tx_reserve = 0;
        cfg.fill_entries = 3;
        assert!(cfg.validate(0).is_err());
    }
}
