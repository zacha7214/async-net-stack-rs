//! AF_XDP socket: bind + RX/TX rings + need-wakeup kick.
//!
//! Feature probing lives in [`XskSocket::new`]: it first tries to bind with
//! the requested flags (typically `XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP`) and,
//! if the kernel/driver rejects zero-copy (`EOPNOTSUPP`/`ENOTSUPP`), retries
//! with `XDP_COPY`. After a successful zero-copy bind it also verifies via
//! `getsockopt(XDP_OPTIONS)` that the kernel really granted it.

use std::io;
use std::mem;
use std::os::fd::RawFd;

use super::sys::*;
use super::umem::{Ring, UMem};

/// A bound AF_XDP socket with its RX/TX rings mapped.
///
/// Single-socket model (like libxdp's `umem->refcount == 1` path): the RX/TX
/// rings and the bind live on the UMEM's own socket fd. [`XskSocket`] only
/// borrows that fd — [`UMem`] owns and closes it, so the UMEM must outlive
/// the socket (the device declares the UMEM after the socket).
pub struct XskSocket {
    fd: RawFd,
    rx: Option<Ring>,
    tx: Option<Ring>,
    /// The bind flags the kernel actually accepted.
    bind_flags: libc::c_ushort,
    /// Our TX producer (we are the producer).
    cached_tx_producer: u32,
    /// Our RX consumer (we are the consumer).
    cached_rx_consumer: u32,
}

impl XskSocket {
    /// Bind `umem`'s socket fd as an AF_XDP socket for `ifindex`/`queue_id`
    /// and map its RX/TX rings.
    ///
    /// If zero-copy was requested and bind fails for **any** reason
    /// (drivers differ: `EOPNOTSUPP`, `ENOTSUPP`, `EINVAL`, ...), retries
    /// with `XDP_COPY` — the same policy as the kernel's xdpsock sample.
    /// `rx_entries` and `tx_entries` must be powers of two.
    pub fn new(
        umem: &UMem,
        ifindex: u32,
        queue_id: u32,
        rx_entries: usize,
        tx_entries: usize,
        bind_flags: libc::c_ushort,
    ) -> io::Result<Self> {
        assert!(rx_entries.is_power_of_two() && rx_entries > 0);
        assert!(tx_entries.is_power_of_two() && tx_entries > 0);

        let fd = umem.fd();
        set_sockopt(fd, XDP_RX_RING, &(rx_entries as libc::c_int))?;
        set_sockopt(fd, XDP_TX_RING, &(tx_entries as libc::c_int))?;

        let copy_flags = (bind_flags & !XDP_ZEROCOPY) | XDP_COPY;
        let mut last_err: Option<io::Error> = None;

        for flags in [bind_flags, copy_flags] {
            if last_err.is_some() && flags == bind_flags {
                continue; // already tried exactly this
            }
            match Self::bind_and_map(fd, ifindex, queue_id, rx_entries, tx_entries, flags) {
                Ok(sock) => return Ok(sock),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::other("AF_XDP bind failed")))
    }

    fn bind_and_map(
        fd: RawFd,
        ifindex: u32,
        queue_id: u32,
        rx_entries: usize,
        tx_entries: usize,
        flags: libc::c_ushort,
    ) -> io::Result<Self> {
        let sa = SockAddrXdp {
            family: AF_XDP as libc::c_ushort,
            flags,
            ifindex,
            queue_id,
            shared_umem_fd: 0,
        };
        let ret = unsafe {
            libc::bind(
                fd,
                &sa as *const SockAddrXdp as *const libc::sockaddr,
                mem::size_of::<SockAddrXdp>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut offs = XdpMmapOffsets::default();
        get_sockopt(fd, XDP_MMAP_OFFSETS, &mut offs)?;
        let rx = Ring::new(
            fd,
            &offs.rx,
            XDP_PGOFF_RX_RING,
            rx_entries,
            mem::size_of::<XdpDesc>(),
        )?;
        let tx = Ring::new(
            fd,
            &offs.tx,
            XDP_PGOFF_TX_RING,
            tx_entries,
            mem::size_of::<XdpDesc>(),
        )?;

        Ok(Self {
            fd,
            rx: Some(rx),
            tx: Some(tx),
            bind_flags: flags,
            cached_tx_producer: 0,
            cached_rx_consumer: 0,
        })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    /// The bind flags the kernel actually accepted.
    pub fn bind_flags(&self) -> libc::c_ushort {
        self.bind_flags
    }

    /// Whether the kernel granted `XDP_USE_NEED_WAKEUP`.
    pub fn need_wakeup_enabled(&self) -> bool {
        match &self.tx {
            Some(tx) => self.bind_flags & XDP_USE_NEED_WAKEUP != 0 && !tx.flags.is_null(),
            None => false,
        }
    }

    /// `getsockopt(XDP_OPTIONS)` flags (e.g. zero-copy actually active).
    pub fn options(&self) -> io::Result<u32> {
        let mut opts = XdpOptions::default();
        get_sockopt(self.fd, XDP_OPTIONS, &mut opts)?;
        Ok(opts.flags)
    }

    /// `getsockopt(XDP_STATISTICS)`.
    pub fn stats(&self) -> io::Result<XdpStatistics> {
        let mut stats = XdpStatistics::default();
        get_sockopt(self.fd, XDP_STATISTICS, &mut stats)?;
        Ok(stats)
    }

    /// Kick the kernel (see [`super::sys::kick`]).
    pub fn kick(&self) {
        kick(self.fd);
    }

    /// RX-ring state for diagnostics.
    #[cfg(test)]
    pub(crate) fn debug_rx_state(&self) -> (u32, u32, u32) {
        let rx = self.rx.as_ref().expect("rx ring present");
        (
            self.cached_rx_consumer,
            rx.producer_load(),
            rx.consumer_load(),
        )
    }

    /// True when the kernel asked to be kicked (read the TX ring flag).
    fn tx_needs_wakeup(&self) -> bool {
        match &self.tx {
            Some(tx) => tx.flags_load() & XDP_RING_NEED_WAKEUP != 0,
            None => true,
        }
    }

    /// Free slots in the TX ring.
    fn tx_free_slots(&self) -> usize {
        let tx = self.tx.as_ref().expect("tx ring present");
        let consumer = tx.consumer_load();
        tx.entries as usize - self.cached_tx_producer.wrapping_sub(consumer) as usize
    }

    /// Pop up to `max` received `(addr, len)` descriptors from the RX ring.
    pub(crate) fn rx_pop(&mut self, out: &mut Vec<(u64, u32)>, max: usize) -> usize {
        let rx = self.rx.as_ref().expect("rx ring present");
        let prod = rx.producer_load();
        let mut n = 0;
        while self.cached_rx_consumer != prod && n < max {
            out.push(rx.desc16(self.cached_rx_consumer));
            self.cached_rx_consumer = self.cached_rx_consumer.wrapping_add(1);
            n += 1;
        }
        if n > 0 {
            rx.consumer_store(self.cached_rx_consumer);
        }
        n
    }

    /// Push `(addr, len)` descriptors onto the TX ring; returns how many fit.
    /// Kicks the kernel when the need-wakeup flag is set.
    pub(crate) fn tx_push(&mut self, descs: &[(u64, u32)]) -> usize {
        let free = self.tx_free_slots();
        let n = descs.len().min(free);
        let tx = self.tx.as_ref().expect("tx ring present");
        for (i, &(addr, len)) in descs[..n].iter().enumerate() {
            tx.write_desc16(self.cached_tx_producer + i as u32, addr, len);
        }
        self.cached_tx_producer = self.cached_tx_producer.wrapping_add(n as u32);
        if n > 0 {
            tx.producer_store(self.cached_tx_producer);
            if self.tx_needs_wakeup() {
                self.kick();
            }
        }
        n
    }
}
