//! [`XdpDevice`]: the AF_XDP `Device` implementation.
//!
//! Data path:
//!
//! * **recv** — drain RX-ring descriptors into [`PacketBuf`]s backed by the
//!   UMEM (zero-copy: the kernel wrote the packet straight into pool
//!   memory), then top the fill ring back up from the pool's free list
//!   (frames the application dropped).
//! * **send** — move every frame onto the TX ring (kicking the kernel as
//!   needed) and return TX completions to the pool's free list.
//! * **alloc** — hand out a free frame for TX; if the free list is empty,
//!   reclaim fill-ring entries we pushed but the kernel has not consumed
//!   yet, so TX-heavy workloads never starve.
//!
//! Frames are recycled into the pool free list by [`PacketBuf`]'s `Drop`,
//! exactly like every other backend; the device just mediates between the
//! pool free list and the kernel's rings.
//!
//! Notes on the initial version:
//! * aligned UMEM chunks only (`chunk_size` a power of two >= 2048), and
//!   `headroom == 0` (like the kernel's xdpsock): RX data lands at frame
//!   byte 0, TX frames must be written from byte 0, so
//!   [`PacketBuf::push_header`] has no room to prepend (there is no 4-byte
//!   address-family prefix — that is a TUN quirk).
//! * one queue, one socket; shared-UMEM/multi-queue and busy-poll are later.

use std::io;
#[cfg(test)]
use std::os::fd::RawFd;

use crate::device::buffer_pool::FramePool;
use crate::device::{Device, PacketBuf};

use super::prog::{AttachMode, XskProgram};
use super::socket::XskSocket;
use super::sys::*;
use super::umem::UMem;

/// Number of descriptors batched per ring interaction.
const BATCH: usize = 64;

/// Configuration for [`XdpDevice::with_config`].
#[derive(Clone, Debug)]
pub struct XdpConfig {
    /// UMEM frame count.
    pub frames: usize,
    /// UMEM chunk size (power of two >= 2048 in aligned mode).
    pub chunk_size: usize,
    /// Bytes reserved at the front of each chunk (0 for now, see module docs).
    pub headroom: usize,
    pub fill_entries: usize,
    pub cq_entries: usize,
    pub rx_entries: usize,
    pub tx_entries: usize,
    /// Load and attach the default redirect program (needs CAP_BPF +
    /// CAP_NET_ADMIN). Without it, packets only flow if another program is
    /// already attached and redirects to this queue's socket.
    pub attach: bool,
    /// Attach in generic/skb mode instead of native driver mode. Required
    /// on veth-style devices (and generally when the driver's native XDP
    /// path does not deliver XSK redirects); also the only mode available
    /// for devices without native XDP.
    pub attach_generic: bool,
    /// XSKMAP size (max queue id + 1).
    pub max_queues: u32,
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            frames: 1024,
            chunk_size: 4096,
            headroom: 0,
            fill_entries: 1024,
            cq_entries: 1024,
            rx_entries: 512,
            tx_entries: 512,
            attach: true,
            attach_generic: false,
            max_queues: 64,
        }
    }
}

/// AF_XDP network device over one interface queue.
///
/// `!Send` / `!Sync` like every backend: the UMEM rings are mapped in the
/// creating process and the design is single-core.
pub struct XdpDevice {
    xsk: XskSocket,
    /// Owns the XSKMAP + attached program; detaches on drop. No runtime use.
    _prog: XskProgram,
    pool: FramePool,
    rx_scratch: Vec<(u64, u32)>,
    tx_scratch: Vec<(u64, u32)>,
    cq_scratch: Vec<u64>,
    reclaim_scratch: Vec<u64>,
    chunk_size: usize,
    headroom: usize,
    attach_mode: AttachMode,
    /// The UMEM outlives the pool: declared last so it drops last.
    umem: UMem,
}

impl XdpDevice {
    /// Create a device on `ifname`, queue 0, with default configuration.
    /// Tries zero-copy and falls back to copy mode automatically.
    pub fn new(ifname: &str, queue_id: u32) -> io::Result<Self> {
        Self::with_config(ifname, queue_id, &XdpConfig::default())
    }

    pub fn with_config(ifname: &str, queue_id: u32, cfg: &XdpConfig) -> io::Result<Self> {
        let cifname = std::ffi::CString::new(ifname)
            .map_err(|_| io::Error::other("interface name contains a NUL byte"))?;
        let ifindex = unsafe { libc::if_nametoindex(cifname.as_ptr()) };
        if ifindex == 0 {
            return Err(io::Error::other(format!("no such interface: {ifname}")));
        }

        let umem = UMem::new(
            cfg.frames,
            cfg.chunk_size,
            cfg.headroom,
            cfg.fill_entries,
            cfg.cq_entries,
            0,
        )?;
        let xsk = XskSocket::new(
            &umem,
            ifindex,
            queue_id,
            cfg.rx_entries,
            cfg.tx_entries,
            XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP,
        )?;
        // SAFETY: the UMEM arena is mapped and unaliased, and the device
        // holds `umem` (declared last) for the pool's entire lifetime.
        let pool = unsafe {
            FramePool::from_raw_parts(umem.base_ptr(), umem.len_bytes(), cfg.chunk_size, cfg.frames)
        };

        let mut prog = XskProgram::new(ifindex, cfg.max_queues)?;
        let attach_mode = if cfg.attach {
            prog.attach(cfg.attach_generic)?
        } else {
            AttachMode::None
        };
        prog.set_socket(queue_id, xsk.fd())?;

        let mut dev = Self {
            xsk,
            _prog: prog,
            pool,
            rx_scratch: Vec::with_capacity(BATCH),
            tx_scratch: Vec::with_capacity(BATCH),
            cq_scratch: Vec::with_capacity(BATCH),
            reclaim_scratch: vec![0u64; BATCH],
            chunk_size: cfg.chunk_size,
            headroom: cfg.headroom,
            attach_mode,
            umem,
        };
        if attach_mode == AttachMode::None {
            eprintln!(
                "xdp: no XDP program attached (attach disabled); \
                 traffic needs an externally attached redirect program"
            );
        }

        // Hand every frame to the kernel's fill ring; TX alloc reclaims on
        // demand, RX drops flow back through the pool free list.
        let mut idxs = vec![0usize; cfg.frames];
        let n = dev.pool.alloc_n(&mut idxs);
        let mut addrs = Vec::with_capacity(n);
        for &idx in &idxs[..n] {
            addrs.push(idx as u64 * cfg.chunk_size as u64);
        }
        let pushed = dev.umem.fill_push(&addrs);
        debug_assert_eq!(pushed, n, "fill ring smaller than frame count");

        Ok(dev)
    }

    /// The bind flags the kernel actually accepted (zero-copy vs copy).
    pub fn bind_flags(&self) -> libc::c_ushort {
        self.xsk.bind_flags()
    }

    /// Whether the kernel granted need-wakeup on this socket.
    pub fn need_wakeup_enabled(&self) -> bool {
        self.xsk.need_wakeup_enabled()
    }

    /// `getsockopt(XDP_STATISTICS)` — first tool for diagnosing drops.
    pub fn stats(&self) -> io::Result<XdpStatistics> {
        self.xsk.stats()
    }

    /// How the default program ended up attached.
    pub fn attach_mode(&self) -> AttachMode {
        self.attach_mode
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
        self._prog.map_fd()
    }

    /// Return TX completions to the pool's free list.
    fn drain_cq(&mut self) -> usize {
        let mut total = 0;
        loop {
            self.cq_scratch.clear();
            let n = self.umem.cq_pop(&mut self.cq_scratch, BATCH);
            total += n;
            if n == 0 {
                break;
            }
            let mut idxs = [0usize; BATCH];
            for (i, &addr) in self.cq_scratch.iter().enumerate() {
                idxs[i] = addr as usize / self.chunk_size;
            }
            self.pool.free_n(&idxs[..n]);
        }
        total
    }

    /// Move frames from the pool free list to the kernel fill ring.
    fn refill_fill_ring(&mut self) {
        let mut idxs = [0usize; BATCH];
        loop {
            let n = self.pool.alloc_n(&mut idxs);
            if n == 0 {
                break;
            }
            let mut addrs = [0u64; BATCH];
            for i in 0..n {
                addrs[i] = idxs[i] as u64 * self.chunk_size as u64;
            }
            let pushed = self.umem.fill_push(&addrs[..n]);
            if pushed < n {
                // Fill ring full: give the remainder back to the pool.
                self.pool.free_n(&idxs[pushed..n]);
                break;
            }
        }
    }

    /// Reclaim fill-ring entries the kernel has not consumed for TX use.
    fn reclaim_fill_for_tx(&mut self) {
        self.reclaim_scratch.clear();
        self.reclaim_scratch.resize(BATCH, 0);
        let n = self.umem.fill_reclaim(&mut self.reclaim_scratch);
        self.reclaim_scratch.truncate(n);
        let mut idxs = [0usize; BATCH];
        for (i, &addr) in self.reclaim_scratch.iter().enumerate() {
            idxs[i] = addr as usize / self.chunk_size;
        }
        self.pool.free_n(&idxs[..n]);
    }

    /// Move every frame out of `frames`, disarm the (now stale) slots, and
    /// convert to TX descriptors. `send` owns all frames per the `Device`
    /// contract; `data_offset` is folded into the TX address so echoed RX
    /// frames and fresh TX frames both transmit their payload bytes.
    fn take_parts(&self, frames: &mut [PacketBuf]) -> Vec<(u64, u32)> {
        let mut out = Vec::with_capacity(frames.len());
        for i in 0..frames.len() {
            // SAFETY: we exclusively own `frames`; each element is moved out
            // exactly once and its slot is replaced with a valid,
            // drop-disarmed state so the caller's later `clear()` is a
            // no-op (same idempotency contract as `recycle_frames`).
            let slot = unsafe { frames.as_mut_ptr().add(i) };
            let buf = unsafe { std::ptr::read(slot) };
            let off = buf.data_offset();
            let (idx, len) = buf.into_parts();
            unsafe { (&mut *slot).disarm() };
            out.push((idx as u64 * self.chunk_size as u64 + off as u64, len as u32));
        }
        out
    }
}

impl Device for XdpDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        self.rx_scratch.clear();
        let n = self.xsk.rx_pop(&mut self.rx_scratch, max);
        for &(addr, len) in &self.rx_scratch[..n] {
            let idx = addr as usize / self.chunk_size;
            let mut buf = self.pool.packet_buf(idx, len as usize);
            buf.set_headroom(self.headroom);
            out.push(buf);
        }
        self.refill_fill_ring();
        Ok(n)
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let total = frames.len();
        if total == 0 {
            return Ok(0);
        }

        let descs = self.take_parts(frames);

        let mut pushed = 0usize;
        while pushed < descs.len() {
            let n = self.xsk.tx_push(&descs[pushed..]);
            pushed += n;
            if pushed < descs.len() {
                // TX ring full: reclaim completions, kick, and retry.
                // The kernel always drains the TX ring, so this terminates.
                self.drain_cq();
                self.xsk.kick();
                unsafe { libc::sched_yield() };
            }
        }

        self.drain_cq();
        self.refill_fill_ring();
        self.tx_scratch = descs; // reuse the allocation next send
        Ok(total)
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        self.drain_cq();
        let idx = match self.pool.alloc() {
            Some(idx) => idx,
            None => {
                self.reclaim_fill_for_tx();
                self.pool.alloc()?
            }
        };
        let mut buf = self.pool.packet_buf(idx, 0);
        buf.set_headroom(self.headroom);
        Some(buf)
    }

    fn frame_size(&self) -> usize {
        self.chunk_size
    }
}
