// Adapted from Zach Allen's bare-metal-utils; see docs/virtio-net-plan.md.
//! Fixed-size bitmap allocator of 4 KiB physical frames, with contiguous runs.
//!
//! Does not dereference physical addresses, map pages, zero memory, or allocate
//! its own metadata. `&mut self` serializes bookkeeping; it is not an IRQ lock.

pub const PAGE_SIZE: usize = 4096;
const BITS: usize = u64::BITS as usize;

#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    EmptyOrReversed,
    Unaligned,
    InsufficientMetadata,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AllocError {
    InvalidRequest,
    OutOfMemory,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FreeError {
    OutsidePool,
    AlreadyFree,
}

/// An address descriptor, not a Rust reference or an automatically freed owner.
/// Copying this does not create another allocation. Dropping it does not free it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRun {
    start: usize,
    pages: usize,
}

impl PageRun {
    pub fn physical_start(self) -> usize {
        self.start
    }
    pub fn page_count(self) -> usize {
        self.pages
    }
    pub fn byte_len(self) -> usize {
        self.pages * PAGE_SIZE
    }
}

pub struct PageAllocator<const WORDS: usize> {
    start: usize,
    pages: usize,
    free: usize,
    used: [u64; WORDS],
}

impl<const WORDS: usize> PageAllocator<WORDS> {
    /// All frames in `[start, end)` become free; boundaries must be page aligned.
    ///
    /// # Safety
    /// The range must be exclusively owned, usable physical RAM, excluding the
    /// kernel, stacks, metadata, firmware, device tree, MMIO and reservations.
    /// Do not construct overlapping allocators, including replacing an allocator
    /// while any of its allocations remain in use. Moving the allocator is OK.
    pub unsafe fn new(start: usize, end: usize) -> Result<Self, InitError> {
        if end <= start {
            return Err(InitError::EmptyOrReversed);
        }
        if start % PAGE_SIZE != 0 || end % PAGE_SIZE != 0 {
            return Err(InitError::Unaligned);
        }
        let pages = (end - start) / PAGE_SIZE;
        if pages.div_ceil(BITS) > WORDS {
            return Err(InitError::InsufficientMetadata);
        }
        Ok(Self {
            start,
            pages,
            free: pages,
            used: [0; WORDS],
        })
    }

    pub fn total_pages(&self) -> usize {
        self.pages
    }
    pub fn free_pages(&self) -> usize {
        self.free
    }

    pub fn allocate_page(&mut self) -> Result<PageRun, AllocError> {
        self.allocate(1, 1)
    }

    /// Allocate physically contiguous frames. Alignment is a power of two in
    /// pages and applies to the physical address, not the bitmap index.
    /// Contents are uninitialized/left over from the previous owner.
    pub fn allocate(
        &mut self,
        pages: usize,
        alignment_pages: usize,
    ) -> Result<PageRun, AllocError> {
        if pages == 0
            || !alignment_pages.is_power_of_two()
            || alignment_pages.checked_mul(PAGE_SIZE).is_none()
        {
            return Err(AllocError::InvalidRequest);
        }
        if pages > self.free {
            return Err(AllocError::OutOfMemory);
        }
        let base_frame = self.start / PAGE_SIZE;
        let mut candidate = 0;
        while candidate <= self.pages - pages {
            let frame = base_frame + candidate;
            let padding = frame.wrapping_neg() & (alignment_pages - 1);
            let Some(aligned) = candidate.checked_add(padding) else {
                break;
            };
            candidate = aligned;
            if candidate > self.pages - pages {
                break;
            }
            let end = candidate + pages;
            if let Some(occupied) = (candidate..end).find(|&index| self.is_used(index)) {
                // Any run beginning before this occupied frame would overlap it.
                candidate = occupied + 1;
                continue;
            }
            for index in candidate..end {
                self.set_used(index, true);
            }
            self.free -= pages;
            return Ok(PageRun {
                start: self.start + candidate * PAGE_SIZE,
                pages,
            });
        }

        Err(AllocError::OutOfMemory)
    }

    /// Return exactly one previously allocated run.
    ///
    /// # Safety
    /// `run` must be a currently live allocation from THIS allocator. No Rust
    /// references, page-table mappings used by others, DMA, or concurrent users
    /// may access it after release. Copies of its descriptor become stale.
    /// Range/bitmap checks catch some mistakes, but cannot detect a stale handle
    /// after its address has been reallocated. They do not make misuse safe.
    pub unsafe fn release(&mut self, run: PageRun) -> Result<(), FreeError> {
        let offset = run
            .start
            .checked_sub(self.start)
            .ok_or(FreeError::OutsidePool)?;
        let first = offset / PAGE_SIZE;
        if offset % PAGE_SIZE != 0
            || run.pages == 0
            || first >= self.pages
            || run.pages > self.pages - first
        {
            return Err(FreeError::OutsidePool);
        }
        let end = first + run.pages;
        if (first..end).any(|index| !self.is_used(index)) {
            return Err(FreeError::AlreadyFree);
        }
        for index in first..end {
            self.set_used(index, false);
        }
        self.free += run.pages;
        Ok(())
    }

    fn is_used(&self, index: usize) -> bool {
        self.used[index / BITS] & (1_u64 << (index % BITS)) != 0
    }

    fn set_used(&mut self, index: usize, used: bool) {
        let mask = 1_u64 << (index % BITS);
        if used {
            self.used[index / BITS] |= mask;
        } else {
            self.used[index / BITS] &= !mask;
        }
    }
}

#[cfg(test)]
#[path = "page_tests.rs"]
mod tests;

