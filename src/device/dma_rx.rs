//! Portable posted RX ownership; device access is confined to driver contracts.
use super::DmaAddress;
use super::{Device, PacketBuf, PacketPool};
use crate::platform::io;
use alloc::{collections::VecDeque, vec::Vec};
use core::{mem, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxSubmission {
    pub token: u64,
    pub address: DmaAddress,
    /// Writable bytes starting at the packet's current headroom offset.
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxCompletion {
    pub token: u64,
    pub length: usize,
    pub failed: bool,
}

/// RX mapping and cache operations, separate from the TX mapping direction.
///
/// # Safety
/// Map the entire `packet.tail_capacity()` starting at `packet.data_offset()`
/// for device writes. Addresses and storage remain stable until unmap. Obtain
/// writable pointers through `as_mut_slice`, never through a shared slice.
/// Do not change packet length, headroom or ownership during mapping.
/// Mapping failure leaves no mapping behind. Synchronization implements the
/// platform's cache rules, including preserving neighboring allocations. Methods
/// must not panic or initiate DMA. No CPU payload access while device-owned.
pub unsafe trait RxDmaMemory {
    type Mapping;

    /// # Safety
    /// Caller retains exclusive storage until DMA is quiescent and unmapped.
    unsafe fn map(&mut self, packet: &mut PacketBuf) -> io::Result<Self::Mapping>;
    fn address(&self, mapping: &Self::Mapping) -> DmaAddress;

    /// # Safety
    /// Live mapping, not yet published to hardware.
    unsafe fn sync_for_device(&mut self, mapping: &mut Self::Mapping);

    /// # Safety
    /// Live mapping; all device accesses have ended.
    unsafe fn sync_for_cpu(&mut self, mapping: &mut Self::Mapping);

    /// # Safety
    /// DMA is quiescent and CPU synchronization has completed.
    unsafe fn unmap(&mut self, mapping: Self::Mapping);
}

/// RX descriptor operations.
///
/// # Safety
/// Access only the published range, regardless of reported completion length.
/// A completion (even failed or malformed) proves all accesses for its token
/// have ended. Successful nonempty lengths within capacity must describe bytes
/// written by this receive, not stale buffer contents. Publication errors mean
/// no descriptor became visible and no access can occur. Implement descriptor
/// publication/acquisition barriers. `stop() == true` proves ALL DMA quiescent;
/// false makes no promise. Methods must not panic. Start without external work.
pub unsafe trait RxEngine {
    /// # Safety
    /// Range is mapped, synchronized and exclusively retained until completion
    /// or confirmed stop. No further posts are allowed after calling stop.
    unsafe fn post(&mut self, submission: RxSubmission, now: Duration) -> io::Result<()>;

    fn completion(&mut self, now: Duration) -> Option<RxCompletion>;

    fn next_deadline(&self) -> Option<Duration> {
        None
    }

    fn stop(&mut self) -> bool;
}

#[derive(Clone, Copy, Debug)]
pub struct RxConfig {
    /// Bound on posted plus completed-but-undelivered packets.
    pub slots: usize,
    pub completion_budget: usize,
}

impl Default for RxConfig {
    fn default() -> Self {
        Self {
            slots: 64,
            completion_budget: 64,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RxStats {
    pub posted: u64,
    pub completed: u64,
    pub delivered: u64,
    pub completion_errors: u64,
    pub invalid_lengths: u64,
    pub stale_completions: u64,
    pub mapping_errors: u64,
    pub posting_errors: u64,
    pub cancelled: u64,
}

struct Flight<T> {
    token: u64,
    capacity: usize,
    mapping: T,
    packet: PacketBuf,
}

struct Resources<M: RxDmaMemory, E: RxEngine> {
    memory: M,
    engine: E,
    flights: Vec<Option<Flight<M::Mapping>>>,
}

/// Bounded RX-only device. Explicitly allocate empty packets and `post` them;
/// no implicit replenishment competes with callers for the pool. `recv` also
/// drives completions at the most recent `poll_at` time. Ready packets retain
/// their slots until delivered. TX can be composed separately by a NIC adapter.
/// Unconfirmed shutdown quarantines the entire DMA resource bundle on drop.
pub struct RxQueue<M: RxDmaMemory, E: RxEngine> {
    resources: Option<Resources<M, E>>,
    pool: PacketPool,
    config: RxConfig,
    ready: VecDeque<PacketBuf>,
    now: Duration,
    next_token: u64,
    stopped: bool,
    quiesced: bool,
    stats: RxStats,
}

impl<M: RxDmaMemory, E: RxEngine> RxQueue<M, E> {
    /// # Safety
    /// Mapper and engine must share a DMA domain, direction and descriptor
    /// constraints, with no outstanding work or external submission owner.
    pub unsafe fn new(
        pool: PacketPool,
        memory: M,
        engine: E,
        config: RxConfig,
    ) -> io::Result<Self> {
        if config.slots == 0 || config.completion_budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero RX capacity/budget",
            ));
        }

        Ok(Self {
            resources: Some(Resources {
                memory,
                engine,
                flights: (0..config.slots).map(|_| None).collect(),
            }),
            pool,
            config,
            ready: VecDeque::with_capacity(config.slots),
            now: Duration::ZERO,
            next_token: 0,
            stopped: false,
            quiesced: false,
            stats: RxStats::default(),
        })
    }

    pub fn stats(&self) -> RxStats {
        self.stats
    }

    pub fn pending(&self) -> usize {
        self.resources
            .as_ref()
            .unwrap()
            .flights
            .iter()
            .filter(|f| f.is_some())
            .count()
    }

    pub fn ready(&self) -> usize {
        self.ready.len()
    }

    /// Accept a prefix of empty buffers, preserving headroom. Accepted handles
    /// become empty placeholders; errors consume nothing. A later failure after
    /// partial acceptance returns the count and records the failure in stats.
    pub fn post(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let mut accepted = 0;

        for frame in frames {
            match self.post_one(frame) {
                Ok(true) => accepted += 1,
                Ok(false) => break,
                Err(_) if accepted > 0 => break,
                Err(e) => return Err(e),
            }
        }

        Ok(accepted)
    }

    fn post_one(&mut self, frame: &mut PacketBuf) -> io::Result<bool> {
        if self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "RX queue stopped",
            ));
        }

        if self.pending() + self.ready.len() == self.config.slots {
            return Ok(false);
        }

        if !frame.is_empty() || frame.tail_capacity() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RX requires empty writable buffer",
            ));
        }

        let token = self
            .next_token
            .checked_add(1)
            .ok_or_else(|| io::Error::other("RX tokens exhausted"))?;

        self.next_token = token;
        let capacity = frame.tail_capacity();

        let resource = self.resources.as_mut().unwrap();

        let index = resource.flights.iter().position(Option::is_none).unwrap();

        // SAFETY: exclusive buffer retained through completion or confirmed stop.
        let mut mapping = match unsafe { resource.memory.map(frame) } {
            Ok(m) => m,
            Err(e) => {
                self.stats.mapping_errors += 1;
                return Err(e);
            }
        };

        // SAFETY: mapping is live and has not been published.
        unsafe {
            resource.memory.sync_for_device(&mut mapping);
        }

        let submission = RxSubmission {
            token,
            address: resource.memory.address(&mapping),
            capacity,
        };

        // SAFETY: successful post retains mapping and packet privately.
        if let Err(e) = unsafe { resource.engine.post(submission, self.now) } {
            // SAFETY: post failure guarantees no device access.
            unsafe {
                resource.memory.sync_for_cpu(&mut mapping);
                resource.memory.unmap(mapping);
            }

            self.stats.posting_errors += 1;
            return Err(e);
        }

        resource.flights[index] = Some(Flight {
            token,
            capacity,
            mapping,
            packet: mem::take(frame),
        });

        self.stats.posted += 1;
        Ok(true)
    }

    fn progress(&mut self) {
        if self.quiesced {
            return;
        }

        for _ in 0..self.config.completion_budget {
            let resource = self.resources.as_mut().unwrap();

            let Some(c) = resource.engine.completion(self.now) else {
                break;
            };

            let Some(index) = resource
                .flights
                .iter()
                .position(|f| f.as_ref().is_some_and(|f| f.token == c.token))
            else {
                self.stats.stale_completions += 1;
                continue;
            };

            let mut f = resource.flights[index].take().unwrap();

            // SAFETY: matching completion guarantees quiescence even on error.
            unsafe {
                resource.memory.sync_for_cpu(&mut f.mapping);
                resource.memory.unmap(f.mapping);
            }
            self.stats.completed += 1;

            let valid = c.length > 0 && c.length <= f.capacity;
            self.stats.invalid_lengths += u64::from(!valid);
            self.stats.completion_errors += u64::from(c.failed);
            if valid && !c.failed {
                // Validate before set_len or exposing any received bytes.
                f.packet.set_len(c.length);
                self.ready.push_back(f.packet);
            }
        }
    }

    /// Stop posting permanently. Ready packets remain available to recv.
    /// False retains posted ownership; polling and retrying shutdown remain legal.
    pub fn shutdown(&mut self) -> bool {
        self.stopped = true;
        if self.quiesced {
            return true;
        }

        let resource = self.resources.as_mut().unwrap();
        if !resource.engine.stop() {
            return false;
        }

        self.quiesced = true;
        for slot in &mut resource.flights {
            if let Some(mut f) = slot.take() {
                // SAFETY: stop confirmed all accesses have ended.
                unsafe {
                    resource.memory.sync_for_cpu(&mut f.mapping);
                    resource.memory.unmap(f.mapping);
                }

                self.stats.cancelled += 1;
            }
        }

        true
    }
}

impl<M: RxDmaMemory, E: RxEngine> Device for RxQueue<M, E> {
    fn alloc(&mut self) -> Option<PacketBuf> {
        if self.stopped {
            None
        } else {
            self.pool.alloc()
        }
    }

    fn frame_size(&self) -> usize {
        self.pool.frame_size()
    }

    fn next_deadline(&self) -> Option<Duration> {
        if self.quiesced {
            None
        } else {
            self.resources.as_ref().unwrap().engine.next_deadline()
        }
    }

    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RX clock moved backwards",
            ));
        }

        self.now = now;
        self.progress();

        Ok(())
    }

    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        if max == 0 {
            return Ok(0);
        }

        self.progress();
        for _ in 0..max.min(self.ready.len()) {
            out.push(self.ready.pop_front().unwrap());
        }

        self.stats.delivered += out.len() as u64;
        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        if frames.is_empty() {
            Ok(0)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "RX-only DMA queue",
            ))
        }
    }
}

impl<M: RxDmaMemory, E: RxEngine> Drop for RxQueue<M, E> {
    fn drop(&mut self) {
        if !self.shutdown() {
            mem::forget(self.resources.take());
        }
    }
}
