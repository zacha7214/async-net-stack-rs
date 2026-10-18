//! Bounded IPv4 protocol dispatch over one L3 device.
//!
//! Call `Coordinator::poll` before polling components at the same timestamp.
//! Ports implement `Device`, so existing pools and custom engines compose without
//! owning the physical receive stream. Dispatch moves handles, never payloads.
use crate::{
    device::{Device, PacketBuf},
    net::checksum,
};
use alloc::vec::Vec;

use crate::platform::io;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use core::cell::RefCell;
use core::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct PortConfig {
    pub rx_capacity: usize,

    /// Maximum accepted packets per coordinator poll. Unused credit is not carried.
    pub tx_budget: usize,
}

impl Default for PortConfig {
    fn default() -> Self {
        Self {
            rx_capacity: 64,
            tx_budget: 64,
        }
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PortStats {
    pub received: u64,
    pub queue_drops: u64,
    pub submitted: u64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct CoordinatorStats {
    pub malformed: u64,
    pub unclaimed: u64,
}

struct Queue {
    protocol: u8,
    config: PortConfig,
    rx: VecDeque<PacketBuf>,
    credit: usize,
    stats: PortStats,
    active: bool,
}

struct Shared<D> {
    device: D,
    now: Duration,
    ports: Vec<Queue>,
}

/// One owner of receive dispatch and adapter progress. This is an IPv4/L3
/// boundary: wrap Ethernet devices in `EthernetIpv4` first.
pub struct Coordinator<D> {
    shared: Rc<RefCell<Shared<D>>>,
    rx: Vec<PacketBuf>,
    stats: CoordinatorStats,
}

/// Exclusive receive subscription for one IP protocol number (TCP=6, UDP=17).
/// Dropping a port releases queued frames and its subscription. Outstanding
/// packet handles retain their normal arena ownership.
pub struct ProtocolPort<D> {
    shared: Rc<RefCell<Shared<D>>>,
    index: usize,
}

impl<D: Device> Coordinator<D> {
    pub fn new(device: D) -> Self {
        Self {
            shared: Rc::new(RefCell::new(Shared {
                device,
                now: Duration::ZERO,
                ports: Vec::new(),
            })),
            rx: Vec::new(),
            stats: CoordinatorStats::default(),
        }
    }

    pub fn port(&mut self, protocol: u8, config: PortConfig) -> io::Result<ProtocolPort<D>> {
        if config.rx_capacity == 0 || config.tx_budget == 0 {
            return Err(invalid("zero port capacity or budget"));
        }

        let mut s = self.shared.borrow_mut();
        if s.ports.iter().any(|q| q.active && q.protocol == protocol) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "protocol already subscribed",
            ));
        }

        let queue = Queue {
            protocol,
            config,
            rx: VecDeque::with_capacity(config.rx_capacity),
            credit: 0,
            stats: PortStats::default(),
            active: true,
        };

        let index = if let Some(i) = s.ports.iter().position(|q| !q.active) {
            s.ports[i] = queue;
            i
        } else {
            s.ports.push(queue);
            s.ports.len() - 1
        };

        Ok(ProtocolPort {
            shared: self.shared.clone(),
            index,
        })
    }

    /// Advance the adapter, receive at most `budget` packets, and renew each
    /// port's TX credit. A full port drops only its own traffic; no other port
    /// is blocked. Repeated calls at the same time are allowed, and renew credit.
    pub fn poll(&mut self, now: Duration, budget: usize) -> io::Result<usize> {
        let mut s = self.shared.borrow_mut();
        if now < s.now {
            return Err(invalid("time went backwards"));
        }

        s.device.poll_at(now)?;
        s.now = now;
        for q in &mut s.ports {
            q.credit = q.config.tx_budget;
        }

        s.device.recv(budget, &mut self.rx)?;
        let count = self.rx.len();
        for frame in self.rx.drain(..) {
            let b = frame.as_slice();

            // Match the engines' supported subset, and reject fragments before
            // classification. Transport checksum validation remains with engines.
            if b.len() < 20
                || b[0] != 0x45
                || b[8] == 0
                || checksum(&b[..20]) != 0
                || u16::from_be_bytes([b[6], b[7]]) & !0x4000 != 0
                || !(20..=b.len()).contains(&(u16::from_be_bytes([b[2], b[3]]) as usize))
            {
                self.stats.malformed += 1;
                continue;
            }

            if let Some(q) = s.ports.iter_mut().find(|q| q.active && q.protocol == b[9]) {
                if q.rx.len() == q.config.rx_capacity {
                    q.stats.queue_drops += 1;
                } else {
                    q.stats.received += 1;
                    q.rx.push_back(frame);
                }
            } else {
                self.stats.unclaimed += 1;
            }
        }

        Ok(count)
    }

    pub fn stats(&self) -> CoordinatorStats {
        self.stats
    }

    /// Combine component deadlines with adapter deadlines. Readiness and pending
    /// application work still require polling; this is a timer hint, not a sleep API.
    pub fn next_deadline(
        &self,
        components: impl IntoIterator<Item = Option<Duration>>,
    ) -> Option<Duration> {
        let s = self.shared.borrow();
        components
            .into_iter()
            .flatten()
            .chain(s.device.next_deadline())
            .min()
            .map(|at| at.max(s.now))
    }

    /// Configure the underlying link between polls. The caller must coordinate
    /// protocol state when changing addresses or withdrawing configuration.
    pub fn with_device<R>(&mut self, f: impl FnOnce(&mut D) -> R) -> R {
        f(&mut self.shared.borrow_mut().device)
    }
}

impl<D> ProtocolPort<D> {
    pub fn stats(&self) -> PortStats {
        self.shared.borrow().ports[self.index].stats
    }

    pub fn queued(&self) -> usize {
        self.shared.borrow().ports[self.index].rx.len()
    }
}

impl<D> Drop for ProtocolPort<D> {
    fn drop(&mut self) {
        let mut s = self.shared.borrow_mut();
        let q = &mut s.ports[self.index];
        q.rx.clear();
        q.active = false;
    }
}

impl<D: Device> Device for ProtocolPort<D> {
    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        if now != self.shared.borrow().now {
            return Err(invalid("poll coordinator first at the same timestamp"));
        }

        Ok(())
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.shared.borrow().device.next_deadline()
    }

    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();

        let mut s = self.shared.borrow_mut();
        let q = &mut s.ports[self.index];
        out.extend(q.rx.drain(..max.min(q.rx.len())));

        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let mut s = self.shared.borrow_mut();
        let limit = frames.len().min(s.ports[self.index].credit);
        if limit == 0 {
            return Ok(0);
        }

        let n = s.device.send(&mut frames[..limit])?;
        s.ports[self.index].credit -= n;
        s.ports[self.index].stats.submitted += n as u64;

        Ok(n)
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        self.shared.borrow_mut().device.alloc()
    }

    fn frame_size(&self) -> usize {
        self.shared.borrow().device.frame_size()
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
