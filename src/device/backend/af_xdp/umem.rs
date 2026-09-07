//! UMEM: the shared frame arena plus the fill (FQ) and completion (CQ) rings.
//!
//! Ownership model, matching [`crate::device::buffer_pool::FramePool`]:
//!
//! * **fill ring**  — frames owned by the *kernel* for RX (user is producer);
//! * **completion ring** — frames the kernel is *returning* after TX (user is
//!   consumer);
//! * RX/TX rings live on the socket (see [`super::socket::XskSocket`]).
//!
//! Frames move kernel → user through RX/CQ and user → kernel through
//! FQ/TX. [`UMem::fill_reclaim`] lets a TX-heavy device take back entries
//! it pushed but the kernel has not consumed yet, which is how the
//! [`super::device::XdpDevice`] balances RX and TX demand without a fixed
//! partition.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use super::sys::*;

/// RAII wrapper around an `mmap(2)` region.
struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mmap {
    fn anonymous(len: usize) -> io::Result<Self> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).expect("mmap returned null"),
            len,
        })
    }

    fn file_backed(len: usize, fd: RawFd, offset: libc::off_t) -> io::Result<Self> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(p.cast()).expect("mmap returned null"),
            len,
        })
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` came from a successful mmap in the constructors.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// One shared-memory ring: header (producer/consumer/flags) + descriptor array.
///
/// Counter fields are accessed as `AtomicU32`; the kernel publishes its
/// counters with release semantics and we publish ours likewise. Descriptors
/// are plain reads/writes ordered after the counter acquire.
pub(crate) struct Ring {
    /// The mmap (kept alive for the ring's lifetime).
    _map: Mmap,
    producer: *mut u32,
    consumer: *mut u32,
    /// `null` on kernels that do not report a flags offset.
    pub(crate) flags: *mut u32,
    desc: *mut u8,
    pub entries: u32,
    mask: u32,
    desc_size: usize,
}

impl Ring {
    /// `off` holds the struct-relative offsets from `XDP_MMAP_OFFSETS`;
    /// `pgoff` is the mmap file offset that selects this ring (one of the
    /// `XDP_*_PGOFF_*` constants).
    pub(crate) fn new(
        fd: RawFd,
        off: &XdpRingOffset,
        pgoff: libc::off_t,
        entries: usize,
        desc_size: usize,
    ) -> io::Result<Self> {
        assert!(
            entries.is_power_of_two() && entries > 0,
            "ring entries must be a power of two, got {entries}"
        );
        let entries = entries as u32;
        let len = off.desc as usize + entries as usize * desc_size;
        let map = Mmap::file_backed(len, fd, pgoff)?;
        let base = map.as_ptr();

        Ok(Self {
            _map: map,
            // SAFETY: the kernel reported these offsets; they lie within the
            // mapped region sized `off.desc + entries * desc_size`.
            producer: unsafe { base.add(off.producer as usize) }.cast(),
            consumer: unsafe { base.add(off.consumer as usize) }.cast(),
            flags: if off.flags != 0 {
                unsafe { base.add(off.flags as usize) }.cast()
            } else {
                std::ptr::null_mut()
            },
            desc: unsafe { base.add(off.desc as usize) },
            entries,
            mask: entries - 1,
            desc_size,
        })
    }

    #[inline]
    pub(crate) fn producer_load(&self) -> u32 {
        // SAFETY: the kernel mapped a u32 at `producer`.
        unsafe { (&*self.producer.cast::<AtomicU32>()).load(Ordering::Acquire) }
    }

    #[inline]
    pub(crate) fn producer_store(&self, v: u32) {
        // SAFETY: the kernel mapped a u32 at `producer`.
        unsafe { (&*self.producer.cast::<AtomicU32>()).store(v, Ordering::Release) }
    }

    #[inline]
    pub(crate) fn consumer_load(&self) -> u32 {
        // SAFETY: the kernel mapped a u32 at `consumer`.
        unsafe { (&*self.consumer.cast::<AtomicU32>()).load(Ordering::Acquire) }
    }

    #[inline]
    pub(crate) fn consumer_store(&self, v: u32) {
        // SAFETY: the kernel mapped a u32 at `consumer`.
        unsafe { (&*self.consumer.cast::<AtomicU32>()).store(v, Ordering::Release) }
    }

    #[inline]
    pub(crate) fn flags_load(&self) -> u32 {
        if self.flags.is_null() {
            0
        } else {
            // SAFETY: the kernel mapped a u32 at `flags`.
            unsafe { (&*self.flags.cast::<AtomicU32>()).load(Ordering::Relaxed) }
        }
    }

    #[inline]
    pub(crate) fn index(&self, i: u32) -> usize {
        (i & self.mask) as usize
    }

    /// Read a `u64` descriptor (fill/completion rings).
    #[inline]
    pub(crate) fn desc64(&self, i: u32) -> u64 {
        debug_assert_eq!(self.desc_size, 8);
        // SAFETY: desc array bounds; happens-after the producer acquire.
        unsafe { *self.desc.add(self.index(i) * 8).cast::<u64>() }
    }

    /// Write a `u64` descriptor (fill ring).
    #[inline]
    pub(crate) fn write_desc64(&self, i: u32, v: u64) {
        debug_assert_eq!(self.desc_size, 8);
        // SAFETY: desc array bounds; published by the producer release.
        unsafe { *self.desc.add(self.index(i) * 8).cast::<u64>() = v };
    }

    /// Read an `xdp_desc` (RX/TX rings).
    #[inline]
    pub(crate) fn desc16(&self, i: u32) -> (u64, u32) {
        debug_assert_eq!(self.desc_size, 16);
        // SAFETY: desc array bounds; happens-after the producer acquire.
        let d = unsafe { &*self.desc.add(self.index(i) * 16).cast::<XdpDesc>() };
        (d.addr, d.len)
    }

    /// Write an `xdp_desc` (TX ring).
    #[inline]
    pub(crate) fn write_desc16(&self, i: u32, addr: u64, len: u32) {
        debug_assert_eq!(self.desc_size, 16);
        // SAFETY: desc array bounds; published by the producer release.
        unsafe {
            *self.desc.add(self.index(i) * 16).cast::<XdpDesc>() =
                XdpDesc { addr, len, options: 0 };
        }
    }
}

/// A UMEM region plus its fill/completion rings.
pub struct UMem {
    /// Held for its lifetime: closing this fd tears the UMEM down.
    _fd: OwnedFd,
    /// The frame arena (externally owned by this struct; the `FramePool`
    /// wraps it without owning it).
    map: Mmap,
    fq: Ring,
    cq: Ring,
    chunk_size: usize,
    num_frames: usize,
    /// Our own fill-ring producer (we are the producer).
    cached_fill_producer: u32,
    /// Our own completion-ring consumer (we are the consumer).
    cached_cq_consumer: u32,
}

impl UMem {
    /// Create a UMEM of `num_frames` frames of `chunk_size` bytes each,
    /// with `headroom` bytes reserved at the front of every chunk.
    ///
    /// Aligned mode (`flags == 0`) requires `chunk_size` to be a power of
    /// two >= 2048; pass [`XDP_UMEM_UNALIGNED_CHUNK_FLAG`] for arbitrary
    /// sizes on kernels that support it.
    pub fn new(
        num_frames: usize,
        chunk_size: usize,
        headroom: usize,
        fill_entries: usize,
        cq_entries: usize,
        flags: u32,
    ) -> io::Result<Self> {
        assert!(num_frames > 0, "num_frames must be > 0");
        if flags & XDP_UMEM_UNALIGNED_CHUNK_FLAG == 0 {
            assert!(
                chunk_size.is_power_of_two() && chunk_size >= 2048,
                "aligned UMEM chunk_size must be a power of two >= 2048, got {chunk_size}"
            );
        }

        let fd = unsafe { libc::socket(AF_XDP, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error())
                .map_err(|e| io::Error::other(format!("AF_XDP socket: {e}")));
        }
        // SAFETY: `fd >= 0` and freshly created.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let total = num_frames
            .checked_mul(chunk_size)
            .ok_or_else(|| io::Error::other("UMEM size overflow"))?;
        let map = Mmap::anonymous(total)
            .map_err(|e| io::Error::other(format!("UMEM arena mmap: {e}")))?;

        let reg = XdpUmemReg {
            addr: map.as_ptr() as u64,
            len: total as u64,
            chunk_size: chunk_size as u32,
            headroom: headroom as u32,
            flags,
            tx_metadata_len: 0,
        };
        set_sockopt(fd.as_raw_fd(), XDP_UMEM_REG, &reg)
            .map_err(|e| io::Error::other(format!("setsockopt XDP_UMEM_REG: {e}")))?;
        set_sockopt(fd.as_raw_fd(), XDP_UMEM_FILL_RING, &(fill_entries as libc::c_int))
            .map_err(|e| io::Error::other(format!("setsockopt XDP_UMEM_FILL_RING: {e}")))?;
        set_sockopt(fd.as_raw_fd(), XDP_UMEM_COMPLETION_RING, &(cq_entries as libc::c_int))
            .map_err(|e| io::Error::other(format!("setsockopt XDP_UMEM_COMPLETION_RING: {e}")))?;

        let mut offs = XdpMmapOffsets::default();
        get_sockopt(fd.as_raw_fd(), XDP_MMAP_OFFSETS, &mut offs)
            .map_err(|e| io::Error::other(format!("getsockopt XDP_MMAP_OFFSETS: {e}")))?;
        let fq = Ring::new(
            fd.as_raw_fd(),
            &offs.fr,
            XDP_UMEM_PGOFF_FILL_RING,
            fill_entries,
            8,
        )
        .map_err(|e| io::Error::other(format!("fill ring mmap: {e}")))?;
        let cq = Ring::new(
            fd.as_raw_fd(),
            &offs.cr,
            XDP_UMEM_PGOFF_COMPLETION_RING,
            cq_entries,
            8,
        )
        .map_err(|e| io::Error::other(format!("completion ring mmap: {e}")))?;

        Ok(Self {
            _fd: fd,
            map,
            fq,
            cq,
            chunk_size,
            num_frames,
            cached_fill_producer: 0,
            cached_cq_consumer: 0,
        })
    }

    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.map.as_ptr()
    }

    /// The UMEM socket fd. For the single-socket model the data socket is
    /// this same fd (`XskSocket::new` borrows it; the UMEM outlives the
    /// socket and closes it).
    pub(crate) fn fd(&self) -> RawFd {
        self._fd.as_raw_fd()
    }

    pub(crate) fn len_bytes(&self) -> usize {
        self.num_frames * self.chunk_size
    }

    /// Entries we pushed to the fill ring that the kernel has not consumed
    /// yet (still reclaimable).
    fn fill_owned(&self) -> usize {
        self.cached_fill_producer.wrapping_sub(self.fq.consumer_load()) as usize
    }

    /// Free slots in the fill ring.
    fn fill_free_slots(&self) -> usize {
        self.fq.entries as usize - self.fill_owned()
    }

    /// Push frame addresses onto the fill ring; returns how many fit.
    /// Kicks the socket when the fill ring's need-wakeup flag is set (the
    /// kernel went to sleep waiting for RX buffers).
    pub(crate) fn fill_push(&mut self, addrs: &[u64]) -> usize {
        let n = addrs.len().min(self.fill_free_slots());
        for (i, &addr) in addrs[..n].iter().enumerate() {
            self.fq.write_desc64(self.cached_fill_producer + i as u32, addr);
        }
        self.cached_fill_producer = self.cached_fill_producer.wrapping_add(n as u32);
        if n > 0 {
            self.fq.producer_store(self.cached_fill_producer);
            if self.fq.flags_load() & XDP_RING_NEED_WAKEUP != 0 {
                // Same fd as the bound socket in the single-socket model.
                kick(self._fd.as_raw_fd());
            }
        }
        n
    }

    /// Reclaim up to `max` unconsumed fill-ring entries (newest first) for
    /// TX use; returns their addresses.
    pub(crate) fn fill_reclaim(&mut self, addrs: &mut [u64]) -> usize {
        let owned = self.fill_owned();
        let n = addrs.len().min(owned);
        for (i, slot) in addrs[..n].iter_mut().enumerate() {
            let pos = self.fq.index(self.cached_fill_producer.wrapping_sub(1 + i as u32));
            *slot = self.fq.desc64(pos as u32);
        }
        self.cached_fill_producer = self.cached_fill_producer.wrapping_sub(n as u32);
        if n > 0 {
            self.fq.producer_store(self.cached_fill_producer);
        }
        n
    }

    /// Pop up to `max` completed TX descriptors from the completion ring.
    pub(crate) fn cq_pop(&mut self, out: &mut Vec<u64>, max: usize) -> usize {
        let prod = self.cq.producer_load();
        let mut n = 0;
        while self.cached_cq_consumer != prod && n < max {
            out.push(self.cq.desc64(self.cached_cq_consumer));
            self.cached_cq_consumer = self.cached_cq_consumer.wrapping_add(1);
            n += 1;
        }
        if n > 0 {
            self.cq.consumer_store(self.cached_cq_consumer);
        }
        n
    }

    /// Fill-ring state for diagnostics.
    #[cfg(test)]
    pub(crate) fn debug_fill_state(&self) -> (u32, u32, u32, u64, u64, u64) {
        (
            self.cached_fill_producer,
            self.fq.producer_load(),
            self.fq.consumer_load(),
            self.fq.desc64(0),
            self.fq.desc64(1),
            self.fq.desc64(2),
        )
    }
}
