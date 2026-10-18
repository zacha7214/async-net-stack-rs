//! Portable TX and RX DMA ownership. Hardware mapping and descriptor operations stay
//! behind unsafe platform contracts; protocol engines continue using `Device`.
use super::{Device, PacketBuf, PacketPool};
use crate::platform::io;
use alloc::{collections::VecDeque, vec::Vec};
use core::{mem, time::Duration};

/// Device-visible address, not a CPU pointer or necessarily a physical address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaAddress(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Submission {
    /// Unique within this queue's lifetime; never reused, including on failure.
    pub token: u64,
    pub address: DmaAddress,
    pub length: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    pub token: u64,
    pub failed: bool,
}

/// Platform mapping/cache operations for TX payloads.
///
/// # Safety
/// A successful mapping must cover the entire packet payload at a stable device
/// address until unmap. Mapping failures leave no mapping behind. Synchronization
/// must implement the platform's cache/coherency requirements. These operations
/// must not panic. They must never initiate DMA themselves.
pub unsafe trait DmaMemory {
    type Mapping;

    /// # Safety
    /// Caller keeps the packet alive and CPU-inaccessible until DMA is quiescent.
    unsafe fn map(&mut self, packet: &PacketBuf) -> io::Result<Self::Mapping>;
    fn address(&self, mapping: &Self::Mapping) -> DmaAddress;

    /// # Safety
    /// Mapping is live and hardware does not yet own this packet.
    unsafe fn sync_for_device(&mut self, mapping: &mut Self::Mapping);

    /// # Safety
    /// Mapping is live and hardware has finished all accesses to the packet.
    unsafe fn sync_for_cpu(&mut self, mapping: &mut Self::Mapping);

    /// # Safety
    /// Mapping is live, DMA is quiescent, and CPU synchronization has completed.
    unsafe fn unmap(&mut self, mapping: Self::Mapping);
}

/// Descriptor/completion operations, independent of the DMA allocator.
///
/// # Safety
/// Completion means all DMA reads for that token have ended, even on failure.
/// `submit` errors mean no descriptor was published and no access can occur.
/// `stop() == true` proves ALL accesses have ended; false makes no such promise.
/// DMA may access only the published address range. The engine must handle
/// descriptor publication/completion memory ordering.
/// Methods must not panic. A new engine must have no outstanding external work.
pub unsafe trait TxEngine {
    /// # Safety
    /// Address/length describe a mapped, synchronized, exclusively held packet.
    /// It remains mapped and alive until completion or successful stop.
    unsafe fn submit(&mut self, submission: Submission, now: Duration) -> io::Result<()>;
    fn completion(&mut self, now: Duration) -> Option<Completion>;
    fn next_deadline(&self) -> Option<Duration> {
        None
    }
    /// Idempotent shutdown. Once called, no further submissions are permitted.
    fn stop(&mut self) -> bool;
}

#[derive(Clone, Copy, Debug)]
pub struct TxConfig {
    pub slots: usize,
    pub completion_budget: usize,
    /// Zero disables events. Overflow discards the oldest event.
    pub event_capacity: usize,
}

impl Default for TxConfig {
    fn default() -> Self {
        Self {
            slots: 64,
            completion_budget: 64,
            event_capacity: 128,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TxStats {
    pub submitted: u64,
    pub completed: u64,
    pub completion_errors: u64,
    pub stale_completions: u64,
    pub mapping_errors: u64,
    pub submission_errors: u64,
    pub cancelled: u64,
    pub event_overflow: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipEvent {
    Submitted(u64),
    Completed(Completion),
    StaleCompletion(u64),
    Cancelled(u64),
}

struct Flight<T> {
    token: u64,
    mapping: T,
    // Kept private: no safe payload access while the engine owns the packet.
    _packet: PacketBuf,
}

struct Resources<M: DmaMemory, E: TxEngine> {
    memory: M,
    engine: E,
    flights: Vec<Option<Flight<M::Mapping>>>,
}

/// Bounded TX-only `Device`. RX is intentionally empty; a NIC adapter can
/// compose this ownership queue with a separate RX posted-buffer contract.
///
/// If shutdown cannot prove quiescence, Drop quarantines (leaks) the entire
/// resource bundle, including mappings, packets, mapper and engine. It never
/// reuses DMA-visible memory based solely on elapsed time or a cancellation request.
pub struct TxQueue<M: DmaMemory, E: TxEngine> {
    resources: Option<Resources<M, E>>,
    pool: PacketPool,
    config: TxConfig,
    now: Duration,
    next_token: u64,
    stopped: bool,
    quiesced: bool,
    stats: TxStats,
    events: VecDeque<OwnershipEvent>,
}

impl<M: DmaMemory, E: TxEngine> TxQueue<M, E> {
    /// Bind a mapper and engine to this ownership queue.
    ///
    /// # Safety
    /// The mapper's addresses must be valid for this engine's DMA/IOMMU domain,
    /// direction and descriptor constraints. Neither component may have external
    /// outstanding work, and no other owner may submit work behind this queue.
    pub unsafe fn new(
        pool: PacketPool,
        memory: M,
        engine: E,
        config: TxConfig,
    ) -> io::Result<Self> {
        if config.slots == 0 || config.completion_budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero DMA queue capacity/budget",
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
            now: Duration::ZERO,
            next_token: 0,
            stopped: false,
            quiesced: false,
            stats: TxStats::default(),
            events: VecDeque::with_capacity(config.event_capacity),
        })
    }

    pub fn stats(&self) -> TxStats {
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

    pub fn pop_event(&mut self) -> Option<OwnershipEvent> {
        self.events.pop_front()
    }

    fn event(&mut self, event: OwnershipEvent) {
        if self.config.event_capacity == 0 {
            return;
        }

        if self.events.len() == self.config.event_capacity {
            self.events.pop_front();
            self.stats.event_overflow += 1;
        }

        self.events.push_back(event);
    }

    /// Retry after a failed stop, or poll completions while stopped. False means
    /// buffers are still held; true means all outstanding mappings were reclaimed.
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
        for index in 0..self.config.slots {
            let resource = self.resources.as_mut().unwrap();

            if let Some(mut flight) = resource.flights[index].take() {
                // SAFETY: stop proved all hardware accesses quiescent.
                unsafe {
                    resource.memory.sync_for_cpu(&mut flight.mapping);
                    resource.memory.unmap(flight.mapping);
                }

                self.stats.cancelled += 1;
                self.event(OwnershipEvent::Cancelled(flight.token));
            }
        }

        true
    }
}

impl<M: DmaMemory, E: TxEngine> Device for TxQueue<M, E> {
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
        self.resources.as_ref().unwrap().engine.next_deadline()
    }

    fn poll_at(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DMA clock moved backwards",
            ));
        }

        self.now = now;
        for _ in 0..self.config.completion_budget {
            let r = self.resources.as_mut().unwrap();
            let Some(completion) = r.engine.completion(now) else {
                break;
            };

            let index = r
                .flights
                .iter()
                .position(|f| f.as_ref().is_some_and(|f| f.token == completion.token));
            if let Some(index) = index {
                let mut flight = r.flights[index].take().unwrap();

                // SAFETY: engine completion proves DMA no longer accesses this mapping.
                unsafe {
                    r.memory.sync_for_cpu(&mut flight.mapping);
                    r.memory.unmap(flight.mapping);
                }

                self.stats.completed += 1;
                self.stats.completion_errors += u64::from(completion.failed);
                self.event(OwnershipEvent::Completed(completion));
            } else {
                self.stats.stale_completions += 1;
                self.event(OwnershipEvent::StaleCompletion(completion.token));
            }
        }

        Ok(())
    }

    fn recv(&mut self, _: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();

        Ok(0)
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }

        if self.stopped {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "DMA queue stopped",
            ));
        }

        let mut accepted = 0;
        for frame in frames {
            let r = self.resources.as_mut().unwrap();
            let Some(index) = r.flights.iter().position(Option::is_none) else {
                break;
            };

            if frame.is_empty() {
                if accepted > 0 {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "empty DMA packet",
                ));
            }

            let Some(token) = self.next_token.checked_add(1) else {
                if accepted > 0 {
                    break;
                }
                return Err(io::Error::other("DMA tokens exhausted"));
            };
            self.next_token = token;

            // SAFETY: frame is exclusively held and is retained on successful publication.
            let mut mapping = match unsafe { r.memory.map(frame) } {
                Ok(mapping) => mapping,
                Err(e) => {
                    self.stats.mapping_errors += 1;
                    if accepted > 0 {
                        break;
                    } else {
                        return Err(e);
                    }
                }
            };

            // SAFETY: no hardware access until submit succeeds.
            unsafe {
                r.memory.sync_for_device(&mut mapping);
            }

            let submission = Submission {
                token,
                address: r.memory.address(&mapping),
                length: frame.len(),
            };

            // SAFETY: mapping stays live in flights until completion or successful stop.
            if let Err(e) = unsafe { r.engine.submit(submission, self.now) } {
                // SAFETY: failed publication promises no hardware ownership.
                unsafe {
                    r.memory.sync_for_cpu(&mut mapping);
                    r.memory.unmap(mapping);
                }

                self.stats.submission_errors += 1;
                if accepted > 0 {
                    break;
                } else {
                    return Err(e);
                }
            }

            r.flights[index] = Some(Flight {
                token,
                mapping,
                _packet: mem::take(frame),
            });

            accepted += 1;
            self.stats.submitted += 1;
            self.event(OwnershipEvent::Submitted(token));
        }

        Ok(accepted)
    }
}

impl<M: DmaMemory, E: TxEngine> Drop for TxQueue<M, E> {
    fn drop(&mut self) {
        if !self.shutdown() {
            // Retain ALL resources: dropping a mapping, arena owner or engine
            // while the device may still use them could free DMA-visible memory.
            mem::forget(self.resources.take());
        }
    }
}

#[path = "dma_rx.rs"]
mod rx;
pub use rx::{RxCompletion, RxConfig, RxDmaMemory, RxEngine, RxQueue, RxStats, RxSubmission};
