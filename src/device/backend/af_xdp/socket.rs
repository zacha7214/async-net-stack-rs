//! AF_XDP socket, cached ring counters, and independent RX/TX wakeups.
use super::sys::*;
use super::umem::{validate_ring_size, Ring, UMem};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::rc::Rc;

/// A bound socket retaining its fd and arena even if the UMem wrapper moves or
/// drops. The high-level device manages exclusive frame ownership.
pub struct XskSocket {
    fd: Rc<OwnedFd>,
    _arena: Rc<dyn std::any::Any>,
    rx: Ring,
    tx: Ring,
    bind_flags: libc::c_ushort,
    cached_tx_producer: u32,
    cached_tx_consumer: u32,
    cached_rx_consumer: u32,
}

impl XskSocket {
    /// Auto mode with kernel zero-copy-to-copy fallback. Explicit flags are
    /// strict: XDP_ZEROCOPY fails when unsupported; XDP_COPY forces copying.
    /// Omit both flags to let the kernel select the best supported mode.
    pub fn new(
        umem: &UMem,
        ifindex: u32,
        queue_id: u32,
        rx_entries: usize,
        tx_entries: usize,
        bind_flags: libc::c_ushort,
    ) -> io::Result<Self> {
        validate_ring_size(rx_entries)?;
        validate_ring_size(tx_entries)?;
        if bind_flags & !(XDP_COPY | XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP) != 0
            || bind_flags & (XDP_COPY | XDP_ZEROCOPY) == (XDP_COPY | XDP_ZEROCOPY)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported or conflicting bind flags",
            ));
        }
        let fd = umem.fd();
        set_sockopt(fd, XDP_RX_RING, &(rx_entries as u32))?;
        set_sockopt(fd, XDP_TX_RING, &(tx_entries as u32))?;
        let mut offs = XdpMmapOffsets::default();
        get_sockopt(fd, XDP_MMAP_OFFSETS, &mut offs)?;
        // Map before bind; a mapping error must never cause a second bind.
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
        let sa = SockAddrXdp {
            family: AF_XDP as _,
            flags: bind_flags,
            ifindex,
            queue_id,
            shared_umem_fd: 0,
        };
        let ret = unsafe {
            libc::bind(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                mem::size_of::<SockAddrXdp>() as _,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut opts = XdpOptions::default();
        get_sockopt(fd, XDP_OPTIONS, &mut opts)?;
        let active = if opts.flags & XDP_OPTIONS_ZEROCOPY != 0 {
            XDP_ZEROCOPY
        } else {
            XDP_COPY
        };
        if bind_flags & XDP_ZEROCOPY != 0 && active != XDP_ZEROCOPY {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel did not confirm zero-copy",
            ));
        }
        Ok(Self {
            fd: umem._fd.clone(),
            _arena: umem.arena_owner(),
            rx,
            tx,
            bind_flags: (bind_flags & !(XDP_COPY | XDP_ZEROCOPY)) | active,
            cached_tx_producer: 0,
            cached_tx_consumer: tx_entries as u32,
            cached_rx_consumer: 0,
        })
    }
    pub(crate) fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
    pub fn bind_flags(&self) -> libc::c_ushort {
        self.bind_flags
    }
    pub fn need_wakeup_enabled(&self) -> bool {
        self.bind_flags & XDP_USE_NEED_WAKEUP != 0 && !self.tx.flags.is_null()
    }
    pub fn options(&self) -> io::Result<u32> {
        let mut opts = XdpOptions::default();
        get_sockopt(self.fd(), XDP_OPTIONS, &mut opts)?;
        Ok(opts.flags)
    }
    pub fn stats(&self) -> io::Result<XdpStatistics> {
        let mut stats = XdpStatistics::default();
        get_sockopt(self.fd(), XDP_STATISTICS, &mut stats)?;
        Ok(stats)
    }
    pub fn kick(&self) {
        kick(self.fd());
    }

    pub(crate) fn wake_tx(&self) {
        if !self.need_wakeup_enabled() || self.tx.flags_load() & XDP_RING_NEED_WAKEUP != 0 {
            self.kick();
        }
    }
    pub(crate) fn wake_rx(&self) {
        let mut pfd = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pfd, 1, 0);
        }
    }
    pub(crate) fn tx_free_slots(&mut self, needed: usize) -> usize {
        let mut free = self
            .cached_tx_consumer
            .wrapping_sub(self.cached_tx_producer);
        if (free as usize) < needed {
            self.cached_tx_consumer = self.tx.consumer_load().wrapping_add(self.tx.entries);
            free = self
                .cached_tx_consumer
                .wrapping_sub(self.cached_tx_producer);
        }
        free as usize
    }
    pub(crate) fn rx_pop(&mut self, out: &mut Vec<(u64, u32)>, max: usize) -> usize {
        let n = max.min(
            self.rx
                .producer_load()
                .wrapping_sub(self.cached_rx_consumer) as usize,
        );
        for i in 0..n {
            out.push(
                self.rx
                    .desc16(self.cached_rx_consumer.wrapping_add(i as u32)),
            );
        }
        self.cached_rx_consumer = self.cached_rx_consumer.wrapping_add(n as u32);
        if n > 0 {
            self.rx.consumer_store(self.cached_rx_consumer);
        }
        n
    }
    pub(crate) fn tx_push(&mut self, descs: &[(u64, u32)]) -> usize {
        let n = descs.len().min(self.tx_free_slots(descs.len()));
        for (i, &(addr, len)) in descs[..n].iter().enumerate() {
            self.tx
                .write_desc16(self.cached_tx_producer.wrapping_add(i as u32), addr, len);
        }
        self.cached_tx_producer = self.cached_tx_producer.wrapping_add(n as u32);
        if n > 0 {
            self.tx.producer_store(self.cached_tx_producer);
        }
        n
    }
    #[cfg(test)]
    pub(crate) fn debug_rx_state(&self) -> (u32, u32, u32) {
        (
            self.cached_rx_consumer,
            self.rx.producer_load(),
            self.rx.consumer_load(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tx_full_returns_immediately_and_wraps() {
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let mut socket = XskSocket {
            fd: Rc::new(fd),
            _arena: Rc::new(()),
            rx: Ring::simulated(4, 16),
            tx: Ring::simulated(4, 16),
            bind_flags: XDP_COPY,
            cached_tx_producer: u32::MAX - 1,
            cached_tx_consumer: 2,
            cached_rx_consumer: 0,
        };
        socket.tx.consumer_store(u32::MAX - 1);
        assert_eq!(socket.tx_push(&[(256, 60); 5]), 4);
        assert_eq!(socket.tx_push(&[(256, 60)]), 0);
        assert_eq!(socket.tx.producer_load(), 2);
        socket.tx.consumer_store(1);
        assert_eq!(socket.tx_push(&[(512, 100); 4]), 3);
        assert_eq!(socket.tx.producer_load(), 5);
    }
}
