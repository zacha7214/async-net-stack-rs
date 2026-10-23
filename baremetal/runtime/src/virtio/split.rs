//! Direct split descriptors with bounded metadata and retained submission owners.
//! No network-header policy lives here. See docs/virtio-split-rings.md.
use crate::memory::dma::OwnedDmaRegion;
use alloc::vec::Vec;
use core::ptr::NonNull;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Barrier {
    Publish,
    Notify,
    Acquire,
}

/// Ring storage and architecture ordering, independently instrumentable in tests.
/// # Safety
/// Storage must be stable, writable, initialized and exclusively CPU-managed by
/// this ring. Device accesses follow Virtio. Raw accesses must be volatile and
/// little endian; barriers must order compiler/CPU/device memory as named. Methods
/// must not panic for in-range aligned accesses. No mutable slices may overlap
/// device access. DMA payload synchronization remains the caller's responsibility.
pub unsafe trait RingMemory {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn device_address(&self) -> u64;
    /// # Safety
    /// Offset must be aligned/in bounds and valid for the current ownership phase.
    unsafe fn read16(&mut self, offset: usize) -> u16;
    /// # Safety
    /// Offset must be aligned/in bounds and valid for the current ownership phase.
    unsafe fn read32(&mut self, offset: usize) -> u32;
    /// # Safety
    /// Offset must be aligned/in bounds and driver-owned.
    unsafe fn write16(&mut self, offset: usize, value: u16);
    /// # Safety
    /// Offset must be aligned/in bounds and driver-owned.
    unsafe fn write32(&mut self, offset: usize, value: u32);
    /// # Safety
    /// Offset must be aligned/in bounds and driver-owned.
    unsafe fn write64(&mut self, offset: usize, value: u64);
    fn barrier(&mut self, kind: Barrier);
}

pub struct DmaRingMemory {
    region: OwnedDmaRegion,
    pointer: NonNull<u8>,
}

impl DmaRingMemory {
    pub fn new(mut region: OwnedDmaRegion) -> Self {
        // Take a raw pointer before publication; never create a slice again.
        let pointer = NonNull::new(region.as_mut_slice().as_mut_ptr()).unwrap();
        Self { region, pointer }
    }
}

unsafe impl RingMemory for DmaRingMemory {
    fn len(&self) -> usize {
        self.region.len()
    }

    fn device_address(&self) -> u64 {
        self.region.device_address().0
    }

    unsafe fn read16(&mut self, offset: usize) -> u16 {
        u16::from_le(unsafe {
            core::ptr::read_volatile(self.pointer.as_ptr().add(offset).cast::<u16>())
        })
    }

    unsafe fn read32(&mut self, offset: usize) -> u32 {
        u32::from_le(unsafe {
            core::ptr::read_volatile(self.pointer.as_ptr().add(offset).cast::<u32>())
        })
    }

    unsafe fn write16(&mut self, offset: usize, value: u16) {
        unsafe {
            core::ptr::write_volatile(
                self.pointer.as_ptr().add(offset).cast::<u16>(),
                value.to_le(),
            );
        }
    }

    unsafe fn write32(&mut self, offset: usize, value: u32) {
        unsafe {
            core::ptr::write_volatile(
                self.pointer.as_ptr().add(offset).cast::<u32>(),
                value.to_le(),
            );
        }
    }

    unsafe fn write64(&mut self, offset: usize, value: u64) {
        unsafe {
            core::ptr::write_volatile(
                self.pointer.as_ptr().add(offset).cast::<u64>(),
                value.to_le(),
            );
        }
    }

    fn barrier(&mut self, _: Barrier) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
        // Host-only inert tests on other architectures; not a DMA barrier port.
        #[cfg(not(target_arch = "aarch64"))]
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub available: usize,
    pub used: usize,
    pub bytes: usize,
}

impl Layout {
    pub fn new(size: u16) -> Result<Self, Error> {
        if !size.is_power_of_two() || size > 32768 {
            return Err(Error::InvalidSize);
        }

        let n = size as usize;
        let available = 16 * n;
        let used = (available + 6 + 2 * n + 3) & !3;
        let bytes = (used + 6 + 8 * n + 3) & !3;
        Ok(Self {
            available,
            used,
            bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSize,
    InvalidMemory,
    NotActive,
    Faulted,
    EmptyChain,
    InvalidBuffer,
    InvalidDirection,
    ChainTooLong,
    Full,
    TokenOrder,
    UsedIndex,
    UsedId,
    UsedLength,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub address: u64,
    pub length: u32,
    pub writable: bool,
}

/// Successful publication cannot be reported as an ordinary failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    pub head: u16,
    pub notify: bool,
}

pub struct Completion<T> {
    pub token: u64,
    pub written: u32,
    pub owner: T,
}

struct Flight<T> {
    token: u64,
    count: u16,
    writable: u32,
    owner: T,
}

struct Resources<M, T> {
    memory: M,
    flights: Vec<Option<Flight<T>>>,

    // CPU only links
    next: Vec<u16>,
}

pub struct SplitRing<M: RingMemory, T> {
    resources: Option<Resources<M, T>>,
    layout: Layout,
    size: u16,
    free_head: u16,
    free_count: u16,
    available: u16,
    consumed: u16,
    pending: u16,
    last_token: Option<u64>,
    active: bool,
    faulted: bool,
}

impl<M: RingMemory, T> SplitRing<M, T> {
    /// Storage must not yet be exposed to a device. Preallocates
    pub fn new(mut memory: M, size: u16) -> Result<Self, Error> {
        let layout = Layout::new(size)?;
        if memory.len() < layout.bytes
            || memory.device_address() % 16 != 0
            || memory
                .device_address()
                .checked_add(layout.bytes as u64)
                .is_none()
        {
            return Err(Error::InvalidMemory);
        }

        for offset in (0..layout.bytes).step_by(4) {
            unsafe {
                memory.write32(offset, 0);
            }
        }

        // Polling mode: advisory completion interrupts. spurious allowed.
        unsafe {
            memory.write16(layout.available, 1);
        }

        Ok(Self {
            resources: Some(Resources {
                memory,
                flights: (0..size).map(|_| None).collect(),
                next: (0..size)
                    .map(|i| if i + 1 < size { i + 1 } else { u16::MAX })
                    .collect(),
            }),
            layout,
            size,
            free_head: 0,
            free_count: size,
            available: 0,
            consumed: 0,
            pending: 0,
            last_token: None,
            active: false,
            faulted: false,
        })
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn free_descriptors(&self) -> u16 {
        self.free_count
    }

    pub fn pending(&self) -> u16 {
        self.pending
    }

    pub fn addresses(&self) -> (u64, u64, u64) {
        let base = self.resources.as_ref().unwrap().memory.device_address();

        (
            base,
            base + self.layout.available as u64,
            base + self.layout.used as u64,
        )
    }

    /// Call before QueueReady or any device exposure.
    ///  Drop now quarantines storage and all owners unless the transport confirms reset.
    ///
    /// # Safety
    /// Device and memory must share a domain and obey bounds/completion ordering.
    pub unsafe fn activate(&mut self) {
        self.active = true;
    }

    /// Terminal transition: no reuse without constructing a new, zeroed ring.
    /// # Safety
    /// Device wide reset has ended ALL access to ring and
    /// payload memory. No further pub/sub may occur after.
    pub unsafe fn quiesce(&mut self) {
        self.active = false;
        self.faulted = true;
    }

    /// All fallible checks precede publication; Err returns the unchanged owner.
    /// Tokens must increase among accepted submissions within this ring.
    /// # Safety
    /// Every buffer must map the corresponding bytes retained by `owner`, in this
    /// device domain with stated access direction.
    /// Retain all packet, mapping, and header ownership in T.
    /// remove competing CPU access until completion or reset.
    /// Payload cache/device synch must be done.
    /// Address ranges must not alias the ring or each other incompatibly.
    pub unsafe fn submit(
        &mut self,
        token: u64,
        buffers: &[Buffer],
        owner: T,
    ) -> Result<Published, (Error, T)> {
        let invalid = if self.faulted {
            Some(Error::Faulted)
        } else if !self.active {
            Some(Error::NotActive)
        } else if buffers.is_empty() {
            Some(Error::EmptyChain)
        } else if buffers.len() > self.size as usize {
            Some(Error::ChainTooLong)
        } else if self.last_token.is_some_and(|last| token <= last) {
            Some(Error::TokenOrder)
        } else {
            None
        };

        if let Some(error) = invalid {
            return Err((error, owner));
        }

        let mut total = 0u32;
        let mut writable = 0u32;
        let mut seen_write = false;
        for buffer in buffers {
            if buffer.length == 0
                || buffer
                    .address
                    .checked_add(buffer.length as u64 - 1)
                    .is_none()
            {
                return Err((Error::InvalidBuffer, owner));
            }

            let Some(sum) = total.checked_add(buffer.length) else {
                return Err((Error::ChainTooLong, owner));
            };

            total = sum;
            if buffer.writable {
                seen_write = true;
                writable += buffer.length;
            } else if seen_write {
                return Err((Error::InvalidDirection, owner));
            }
        }

        if buffers.len() > self.free_count as usize {
            return Err((Error::Full, owner));
        }

        let r = self.resources.as_mut().unwrap();
        let head = self.free_head;
        let mut id = head;
        for (i, buffer) in buffers.iter().enumerate() {
            let next = r.next[id as usize];
            let more = i + 1 < buffers.len();
            let offset = id as usize * 16;

            unsafe {
                r.memory.write64(offset, buffer.address);
                r.memory.write32(offset + 8, buffer.length);
                r.memory.write16(
                    offset + 12,
                    u16::from(more) | if buffer.writable { 2 } else { 0 },
                );

                r.memory.write16(offset + 14, if more { next } else { 0 });
            }

            id = next;
        }

        self.free_head = id;
        self.free_count -= buffers.len() as u16;
        r.flights[head as usize] = Some(Flight {
            token,
            count: buffers.len() as u16,
            writable,
            owner,
        });

        self.pending += 1;
        self.last_token = Some(token);
        unsafe {
            r.memory.write16(
                self.layout.available + 4 + 2 * (self.available % self.size) as usize,
                head,
            );
        }

        r.memory.barrier(Barrier::Publish);
        self.available = self.available.wrapping_add(1);
        unsafe {
            r.memory.write16(self.layout.available + 2, self.available);
        }
        // Publishing idx commits ownership even if notification is suppressed.
        r.memory.barrier(Barrier::Notify);
        let flags = unsafe { r.memory.read16(self.layout.used) };
        Ok(Published {
            head,
            notify: flags & 1 == 0,
        })
    }

    /// Acquire at most one completion. Corrupt metadata latches a fault, retaining
    /// every unresolved owner until reset. Readable-only chains ignore used.len
    /// (not a TX delivery/byte-count result); writable chains enforce their bound.
    pub fn poll(&mut self) -> Result<Option<Completion<T>>, Error> {
        if self.faulted {
            return Err(Error::Faulted);
        }
        if !self.active {
            return Err(Error::NotActive);
        }

        let r = self.resources.as_mut().unwrap();
        let observed = unsafe { r.memory.read16(self.layout.used + 2) };
        let count = observed.wrapping_sub(self.consumed);
        if count > self.pending {
            self.faulted = true;
            return Err(Error::UsedIndex);
        }

        if count == 0 {
            return Ok(None);
        }

        r.memory.barrier(Barrier::Acquire);

        let offset = self.layout.used + 4 + 8 * (self.consumed % self.size) as usize;
        let id = unsafe { r.memory.read32(offset) };
        let written = unsafe { r.memory.read32(offset + 4) };
        let flight = r.flights.get(id as usize).and_then(Option::as_ref);

        let Some(flight) = flight else {
            self.faulted = true;
            return Err(Error::UsedId);
        };

        if flight.writable != 0 && written > flight.writable {
            self.faulted = true;
            return Err(Error::UsedLength);
        }

        let count = flight.count;
        let flight = r.flights[id as usize].take().unwrap();
        let mut last = id as u16;
        for _ in 1..count {
            last = r.next[last as usize];
        }

        r.next[last as usize] = self.free_head;
        self.free_head = id as u16;
        self.free_count += count;
        self.pending -= 1;
        self.consumed = self.consumed.wrapping_add(1);

        Ok(Some(Completion {
            token: flight.token,
            written,
            owner: flight.owner,
        }))
    }
}

impl<M: RingMemory, T> Drop for SplitRing<M, T> {
    fn drop(&mut self) {
        if self.active {
            if let Some(resources) = self.resources.take() {
                core::mem::forget(resources);
            }
        }
    }
}

#[cfg(test)]
mod tests;

