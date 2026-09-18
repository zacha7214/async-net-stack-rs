//! Deterministic single-thread L3 fabric. Unicast transfers packet ownership
//! without copying payloads; limited broadcast fans out with copies. This is
//! a userspace model, not an emulation of Linux scheduling or socket queues.
use crate::device::{buffer_pool::FramePool, Device, PacketBuf};
use std::{cell::RefCell, collections::BTreeMap, io, net::Ipv4Addr, rc::Rc, time::Duration};

/// Directed link configuration, changed live to reproduce partitions/healing.
#[derive(Default, Clone, Copy, Debug)]
pub struct Link {
    pub delay: Duration,
    /// Additional delay on every second packet, permitting deterministic reorder.
    pub alternating_delay: Duration,
    /// Drop every Nth accepted packet; zero disables loss.
    pub drop_every: u64,
    pub partitioned: bool,
    /// Zero disables the limit; oversized packets are silently dropped (PMTU black hole).
    pub mtu: usize,
}
#[derive(Default, Clone, Copy, Debug)]
pub struct FabricStats {
    pub delivered: u64,
    pub fault_drops: u64,
    pub queue_drops: u64,
    pub unroutable: u64,
    pub backpressure: u64,
}
struct Pending {
    at: Duration,
    frame: PacketBuf,
}
struct Port {
    pool: FramePool,
    queue: Vec<Pending>,
    capacity: usize,
    active: bool,
}
#[derive(Default)]
struct State {
    now: Duration,
    ports: Vec<Port>,
    routes: BTreeMap<Ipv4Addr, usize>,
    links: BTreeMap<(usize, usize), (Link, u64)>,
    stats: FabricStats,
}
#[derive(Clone, Default)]
pub struct Network(Rc<RefCell<State>>);
impl Network {
    /// Create a pool-backed port with one or more virtual host addresses.
    /// Queue capacity counts both ready and delayed frames.
    pub fn port(
        &self,
        addresses: &[Ipv4Addr],
        frames: usize,
        queue: usize,
    ) -> io::Result<SimDevice> {
        let mut state = self.0.borrow_mut();
        if frames == 0
            || queue == 0
            || addresses.is_empty()
            || addresses.iter().any(|a| {
                a.is_unspecified()
                    || a.is_broadcast()
                    || a.is_multicast()
                    || state.routes.contains_key(a)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid capacity or conflicting/non-unicast address",
            ));
        }
        let id = state.ports.len();
        let pool = FramePool::new(frames, 2048, 4096);
        state.ports.push(Port {
            pool,
            queue: Vec::with_capacity(queue),
            capacity: queue,
            active: true,
        });
        for &address in addresses {
            state.routes.insert(address, id);
        }
        Ok(SimDevice {
            network: self.clone(),
            id,
        })
    }
    pub fn set_link(&self, from: &SimDevice, to: &SimDevice, link: Link) -> io::Result<()> {
        if !Rc::ptr_eq(&self.0, &from.network.0) || !Rc::ptr_eq(&self.0, &to.network.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ports belong to another fabric",
            ));
        }
        // Reset the fault sequence on reconfiguration. Already queued traffic
        // keeps its original delivery time, including across a partition.
        self.0
            .borrow_mut()
            .links
            .insert((from.id, to.id), (link, 0));
        Ok(())
    }
    pub fn advance(&self, elapsed: Duration) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        state.now = state
            .now
            .checked_add(elapsed)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "clock overflow"))?;
        Ok(())
    }
    pub fn now(&self) -> Duration {
        self.0.borrow().now
    }
    pub fn stats(&self) -> FabricStats {
        self.0.borrow().stats
    }
}

pub struct SimDevice {
    network: Network,
    id: usize,
}
impl SimDevice {
    pub fn queued(&self) -> usize {
        self.network.0.borrow().ports[self.id].queue.len()
    }
}
impl Drop for SimDevice {
    fn drop(&mut self) {
        let mut state = self.network.0.borrow_mut();
        state.ports[self.id].active = false;
        state.ports[self.id].queue.clear();
        state.routes.retain(|_, id| *id != self.id);
    }
}
impl Device for SimDevice {
    fn alloc(&mut self) -> Option<PacketBuf> {
        let state = self.network.0.borrow();
        let pool = &state.ports[self.id].pool;
        Some(pool.packet_buf(pool.alloc()?, 0))
    }
    fn frame_size(&self) -> usize {
        2048
    }
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        let mut state = self.network.0.borrow_mut();
        let now = state.now;
        let queue = &mut state.ports[self.id].queue;
        // Stable sort preserves insertion order for identical deadlines.
        queue.sort_by_key(|p| p.at);
        let n = queue.iter().take(max).take_while(|p| p.at <= now).count();
        out.extend(queue.drain(..n).map(|p| p.frame));
        state.stats.delivered += n as u64;
        Ok(n)
    }
    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let mut state = self.network.0.borrow_mut();
        let mut accepted = 0;
        for frame in frames {
            let packet = frame.as_slice();
            if packet.len() < 20 || packet[0] >> 4 != 4 || packet.len() > 2048 {
                if accepted != 0 {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fabric expects IPv4 frames",
                ));
            }
            let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
            let broadcast = destination.is_broadcast();
            let route = state.routes.get(&destination).copied();
            let targets = if broadcast {
                0..state.ports.len()
            } else {
                route.unwrap_or(0)..route.map_or(0, |id| id + 1)
            };
            if !broadcast && route.is_none() {
                state.stats.unroutable += 1;
            }
            // Congestion is checked before faults; retries do not advance the
            // fault sequence. Broadcast drops copies instead of partial retry.
            if !broadcast
                && route.is_some_and(|id| state.ports[id].queue.len() == state.ports[id].capacity)
            {
                state.stats.backpressure += 1;
                break;
            }
            let mut delivery = None;
            for target in targets {
                if broadcast && (target == self.id || !state.ports[target].active) {
                    continue;
                }
                let (link, sequence) = state.links.entry((self.id, target)).or_default();
                *sequence = sequence.wrapping_add(1);
                let link = *link;
                let sequence = *sequence;
                if link.partitioned
                    || (link.mtu != 0 && packet.len() > link.mtu)
                    || (link.drop_every != 0 && sequence % link.drop_every == 0)
                {
                    state.stats.fault_drops += 1;
                    continue;
                }
                let delay = link.delay.saturating_add(if sequence % 2 == 0 {
                    link.alternating_delay
                } else {
                    Duration::ZERO
                });
                let at = state.now.saturating_add(delay);
                if broadcast {
                    let port = &mut state.ports[target];
                    if port.queue.len() == port.capacity {
                        state.stats.queue_drops += 1;
                        continue;
                    }
                    let Some(index) = port.pool.alloc() else {
                        state.stats.queue_drops += 1;
                        continue;
                    };
                    let mut copy = port.pool.packet_buf(index, 0);
                    copy.set_headroom(0);
                    copy.set_len(packet.len());
                    copy.as_mut_packet().copy_from_slice(packet);
                    port.queue.push(Pending { at, frame: copy });
                } else {
                    delivery = Some((target, at));
                }
            }
            if let Some((target, at)) = delivery {
                state.ports[target].queue.push(Pending {
                    at,
                    frame: std::mem::take(frame),
                });
            }
            drop(std::mem::take(frame));
            accepted += 1;
        }
        Ok(accepted)
    }
}
