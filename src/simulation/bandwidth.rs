//! Host-independent FIFO serialization. Time is supplied by the caller.
//! This module can also be compiled inside a `no_std` crate with `alloc`.
extern crate alloc;
use alloc::collections::VecDeque;
use core::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Bandwidth {
    pub bits_per_second: u64,
    /// Includes the packet currently being serialized, but excludes propagation.
    pub queue_bytes: usize,
    /// Accounting overhead per L3 packet (e.g. modeled framing overhead).
    pub overhead_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Full,
    Overflow,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BandwidthStats {
    pub queued_bytes: usize,
    pub peak_bytes: usize,
    pub admitted: u64,
    pub dropped: u64,
    pub max_queue_delay: Duration,
}

struct Reservation<T> {
    finish: Duration,
    bytes: usize,
    owner: T,
}

/// FIFO service shared by any number of producers. Calls must use monotonic time.
/// Equal-time admissions are ordered by call order; there is no flow fairness.
pub struct Serializer<T> {
    config: Bandwidth,
    pending: VecDeque<Reservation<T>>,
    stats: BandwidthStats,
}

impl<T> Serializer<T> {
    pub fn new(config: Bandwidth) -> Option<Self> {
        (config.bits_per_second != 0 && config.queue_bytes != 0).then(|| Self {
            config,
            pending: VecDeque::new(),
            stats: BandwidthStats::default(),
        })
    }

    pub fn advance(&mut self, now: Duration) {
        while self.pending.front().is_some_and(|r| r.finish <= now) {
            self.stats.queued_bytes -= self.pending.pop_front().unwrap().bytes;
        }
    }

    pub fn stats(&self) -> BandwidthStats {
        self.stats
    }

    /// Cancel reservations without moving the deadlines of surviving packets.
    pub fn cancel(&mut self, mut matches: impl FnMut(&T) -> bool) {
        self.pending.retain(|r| !matches(&r.owner));

        self.stats.queued_bytes = self.pending.iter().map(|r| r.bytes).sum();
    }

    /// Reserve service and return arrival time, including propagation delay.
    /// Failure makes no reservation. All duration arithmetic is checked.
    pub fn admit(
        &mut self,
        now: Duration,
        packet_bytes: usize,
        propagation: Duration,
        owner: T,
    ) -> Result<Duration, AdmissionError> {
        self.advance(now);

        let result = self.reserve(now, packet_bytes, propagation, owner);
        if result.is_err() {
            self.stats.dropped += 1;
        }

        result
    }

    fn reserve(
        &mut self,
        now: Duration,
        packet_bytes: usize,
        propagation: Duration,
        owner: T,
    ) -> Result<Duration, AdmissionError> {
        let bytes = packet_bytes
            .checked_add(self.config.overhead_bytes)
            .ok_or(AdmissionError::Overflow)?;

        if bytes > self.config.queue_bytes - self.stats.queued_bytes {
            return Err(AdmissionError::Full);
        }

        let numerator = bytes as u128 * 8 * 1_000_000_000;
        let rate = self.config.bits_per_second as u128;
        let ns = numerator.div_ceil(rate);
        let seconds = u64::try_from(ns / 1_000_000_000).map_err(|_| AdmissionError::Overflow)?;

        let service = Duration::new(seconds, (ns % 1_000_000_000) as u32);
        let start = self.pending.back().map_or(now, |r| r.finish.max(now));

        let finish = start.checked_add(service).ok_or(AdmissionError::Overflow)?;
        let arrival = finish
            .checked_add(propagation)
            .ok_or(AdmissionError::Overflow)?;

        self.pending.push_back(Reservation {
            finish,
            bytes,
            owner,
        });

        self.stats.queued_bytes += bytes;
        self.stats.peak_bytes = self.stats.peak_bytes.max(self.stats.queued_bytes);

        self.stats.admitted += 1;
        self.stats.max_queue_delay = self.stats.max_queue_delay.max(start - now);

        Ok(arrival)
    }
}
