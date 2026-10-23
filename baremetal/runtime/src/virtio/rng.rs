//! Bounded, polling virtio-rng entropy with an explicitly shared TCP source.
//! Device entropy is trusted; no timestamp seed, deterministic fallback or PRNG.
use super::{mmio, split::Buffer};
use crate::memory::dma::{DmaAllocator, OwnedDmaRegion};
use alloc::{boxed::Box, rc::Rc, vec};
use async_net_stack_core::platform::io;
use core::cell::RefCell;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Bytes exposed in each writable request, 1..=4096.
    pub request_bytes: usize,
    /// Fixed CPU cache capacity, request_bytes..=4096.
    pub cache_bytes: usize,
    pub reset_poll_budget: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            request_bytes: 256,
            cache_bytes: 1024,
            reset_poll_budget: 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidConfig,
    Allocation,
    Transport(mmio::Error),
    Busy,
    Stopped,
    EmptyCompletion,
    TokensExhausted,
}
impl From<mmio::Error> for Error {
    fn from(error: mmio::Error) -> Self {
        Self::Transport(error)
    }
}
impl Error {
    pub fn into_io(self) -> io::Error {
        let kind = match self {
            Self::InvalidConfig => io::ErrorKind::InvalidInput,
            Self::Busy => io::ErrorKind::WouldBlock,
            Self::Stopped => io::ErrorKind::NotConnected,
            Self::EmptyCompletion => io::ErrorKind::InvalidData,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, "Virtio entropy unavailable; inspect driver status")
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub submitted: u64,
    pub completed: u64,
    pub short_reads: u64,
    pub bytes_received: u64,
    pub bytes_delivered: u64,
    pub cancelled: u64,
}

struct Inner<R: mmio::Registers> {
    // Drop/reset this first. Active ring owners retain DMA storage on failure.
    transport: mmio::RngTransport<R, OwnedDmaRegion>,
    // Exactly one request region, either here or owned by the active ring.
    spare: Option<OwnedDmaRegion>,
    cache: Box<[u8]>,
    head: usize,
    len: usize,
    request_bytes: usize,
    token: u64,
    pending: bool,
    stopped: bool,
    fault: Option<Error>,
    stats: Stats,
}
impl<R: mmio::Registers> Inner<R> {
    fn running(&self) -> Result<(), Error> {
        if self.stopped {
            Err(Error::Stopped)
        } else if let Some(error) = self.fault {
            Err(error)
        } else {
            Ok(())
        }
    }
    fn fail(&mut self, error: Error) -> Error {
        self.fault = Some(error);
        self.clear_cache();
        error
    }
    fn clear_cache(&mut self) {
        self.cache.fill(0);
        self.head = 0;
        self.len = 0;
    }
    fn submit(&mut self) -> Result<(), Error> {
        if self.pending || self.cache.len() - self.len < self.request_bytes {
            return Ok(());
        }
        let token = self
            .token
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::TokensExhausted))?;
        let region = self
            .spare
            .take()
            .expect("entropy region is idle or in flight");
        let buffer = Buffer {
            address: region.device_address().0,
            length: self.request_bytes as u32,
            writable: true,
        };
        // SAFETY: exclusive owned region in this coherent DMA domain. Cache
        // maintenance is a no-op; split-ring publication/acquisition supplies
        // device barriers. No CPU view is created while the ring owns the region.
        if let Err((error, region)) = unsafe { self.transport.submit(0, token, &[buffer], region) }
        {
            self.spare = Some(region);
            return Err(self.fail(error.into()));
        }
        self.token = token;
        self.pending = true;
        self.stats.submitted += 1;
        Ok(())
    }
    fn poll(&mut self) -> Result<(), Error> {
        self.running()?;
        // One completion and at most one new submission: short responses cannot
        // cause an unbounded refill loop during network processing.
        let completion = self
            .transport
            .completion(0)
            .map_err(|e| self.fail(e.into()))?;
        if let Some(mut completion) = completion {
            self.pending = false;
            self.stats.completed += 1;
            let length = completion.written as usize;
            if length == 0 {
                self.spare = Some(completion.owner);
                return Err(self.fail(Error::EmptyCompletion));
            }
            // The ring bounded length by request_bytes and acquired DMA writes.
            // Capacity was reserved before publication, regardless of short reads.
            let bytes = &completion.owner.as_mut_slice()[..length];
            for &byte in bytes {
                self.cache[(self.head + self.len) % self.cache.len()] = byte;
                self.len += 1;
            }
            self.stats.bytes_received += length as u64;
            self.stats.short_reads += u64::from(length < self.request_bytes);
            self.spare = Some(completion.owner);
        }
        let pending = self
            .transport
            .pending_interrupts()
            .map_err(|e| self.fail(e.into()))?;
        // Entropy has no device configuration fields; config IRQ needs no reads.
        self.transport
            .acknowledge_interrupts(pending)
            .map_err(|e| self.fail(e.into()))?;
        self.submit()
    }
    fn try_fill(&mut self, out: &mut [u8]) -> io::Result<()> {
        self.running().map_err(Error::into_io)?;
        if out.len() > self.cache.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entropy request exceeds cache capacity",
            ));
        }
        if out.len() > self.len {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "entropy cache needs polling",
            ));
        }
        // All-or-nothing: neither destination nor cache changes on errors. No
        // MMIO, allocation, wait or refill is performed by this consumer path.
        for byte in out.iter_mut() {
            *byte = self.cache[self.head];
            self.cache[self.head] = 0;
            self.head = (self.head + 1) % self.cache.len();
        }
        self.len -= out.len();
        self.stats.bytes_delivered += out.len() as u64;
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), Error> {
        self.stopped = true;
        self.clear_cache();
        self.transport.reset()?;
        if self.pending {
            self.stats.cancelled += 1;
        }
        self.pending = false;
        self.spare = None;
        Ok(())
    }
}

/// Shared single-threaded service. All handles/callbacks share one DMA owner.
/// Poll it from the main loop; callbacks only consume completed cached bytes.
/// Last-handle Drop resets the device or quarantines DMA owners on failure.
pub struct VirtioRng<R: mmio::Registers>(Rc<RefCell<Inner<R>>>);
impl<R: mmio::Registers> Clone for VirtioRng<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<R: mmio::Registers> VirtioRng<R> {
    /// Initialize modern device ID 4 and post one writable request; never wait
    /// for entropy. Metadata allocation failure uses the runtime panic handler.
    /// # Safety
    /// Registers and allocator must satisfy RngTransport::new's exclusive device,
    /// coherent DMA-domain and truthful reset/completion contracts. Randomness
    /// quality depends on the trusted device/backend; this driver cannot prove it.
    pub unsafe fn new(registers: R, dma: DmaAllocator, config: Config) -> Result<Self, Error> {
        if config.request_bytes == 0
            || config.request_bytes > 4096
            || config.cache_bytes < config.request_bytes
            || config.cache_bytes > 4096
            || config.reset_poll_budget == 0
        {
            return Err(Error::InvalidConfig);
        }
        let mut transport =
            unsafe { mmio::RngTransport::new(registers, dma.clone(), config.reset_poll_budget)? };
        transport.initialize()?;
        transport.configure_queue(0, 1)?;
        let spare = dma
            .allocate(config.request_bytes, 64)
            .map_err(|_| Error::Allocation)?;
        let cache = vec![0; config.cache_bytes].into_boxed_slice();
        transport.start()?;
        let mut inner = Inner {
            transport,
            spare: Some(spare),
            cache,
            head: 0,
            len: 0,
            request_bytes: config.request_bytes,
            token: 0,
            pending: false,
            stopped: false,
            fault: None,
            stats: Stats::default(),
        };
        inner.submit()?;
        Ok(Self(Rc::new(RefCell::new(inner))))
    }
    pub fn available(&self) -> usize {
        self.0.borrow().len
    }
    pub fn pending(&self) -> bool {
        self.0.borrow().pending
    }
    pub fn stats(&self) -> Stats {
        self.0.borrow().stats
    }
    pub fn fault(&self) -> Option<Error> {
        self.0.borrow().fault
    }
    /// Bounded progress, even for short reads. Does not wait for completion.
    pub fn poll(&self) -> Result<(), Error> {
        self.0.try_borrow_mut().map_err(|_| Error::Busy)?.poll()
    }
    /// Consume cached entropy atomically or leave the destination unchanged.
    /// WouldBlock requires subsequent main-loop polling, not a fallback seed.
    pub fn try_fill(&self, out: &mut [u8]) -> io::Result<()> {
        self.0
            .try_borrow_mut()
            .map_err(|_| Error::Busy.into_io())?
            .try_fill(out)
    }
    /// Build the callback for TcpPool::with_sequence_source only when at least
    /// four bytes are ready. Later exhaustion returns WouldBlock without waiting.
    /// Retain another handle to poll/refill and explicitly shut down the service.
    pub fn tcp_sequence_source(&self) -> io::Result<impl FnMut() -> io::Result<u32> + 'static>
    where
        R: 'static,
    {
        let inner = self.0.try_borrow().map_err(|_| Error::Busy.into_io())?;
        inner.running().map_err(Error::into_io)?;
        if inner.cache.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TCP entropy cache must hold four bytes",
            ));
        }
        if inner.len < 4 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "TCP entropy is not ready",
            ));
        }
        let rng = self.clone();
        Ok(move || {
            let mut bytes = [0; 4];
            rng.try_fill(&mut bytes)?;
            Ok(u32::from_le_bytes(bytes))
        })
    }
    /// Terminal stop shared by every handle and callback. Retry reset on error;
    /// no DMA storage is released until confirmed. Buffered bytes are discarded.
    pub fn shutdown(&self) -> Result<(), Error> {
        self.0.try_borrow_mut().map_err(|_| Error::Busy)?.shutdown()
    }
}

#[cfg(test)]
mod tests;

