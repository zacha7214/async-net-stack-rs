//! Bounded 64-byte-granule heap with reclaiming contiguous allocations.
//! Bookkeeping lives outside the managed memory. No allocation headers or heap
//! allocations are needed to allocate/free. First-fit scanning is bounded but
//! not constant-time; keep allocations out of interrupt context.
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

pub const GRANULE: usize = 64;
pub struct Heap<const WORDS: usize> {
    base: *mut u8,
    blocks: usize,
    used: [u64; WORDS],
}

impl<const WORDS: usize> Heap<WORDS> {
    pub const fn empty() -> Self {
        Self {
            base: ptr::null_mut(),
            blocks: 0,
            used: [0; WORDS],
        }
    }

    /// # Safety
    /// Exclusively own a writable allocation of `bytes` bytes at `base`, stable
    /// for the heap's entire lifetime. No previous allocations may remain live.
    pub unsafe fn init(&mut self, base: *mut u8, bytes: usize) -> bool {
        if self.blocks != 0
            || base.is_null()
            || base as usize % GRANULE != 0
            || bytes == 0
            || bytes % GRANULE != 0
            || bytes > isize::MAX as usize
            || (base as usize).checked_add(bytes).is_none()
            || bytes / GRANULE > WORDS.saturating_mul(64)
        {
            return false;
        }
        self.base = base;
        self.blocks = bytes / GRANULE;
        true
    }

    pub fn allocate(&mut self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return ptr::null_mut();
        }
        let count = layout.size().div_ceil(GRANULE);
        if count > self.blocks {
            return ptr::null_mut();
        }
        let mut first = 0;
        while first <= self.blocks - count {
            let address = self.base as usize + first * GRANULE;
            let Some(aligned) = address
                .checked_add(layout.align() - 1)
                .map(|n| n & !(layout.align() - 1))
            else {
                return ptr::null_mut();
            };

            first += (aligned - address) / GRANULE;
            if first > self.blocks - count {
                break;
            }

            if let Some(busy) =
                (first..first + count).find(|&i| self.used[i / 64] & (1 << (i % 64)) != 0)
            {
                first = busy + 1;
                continue;
            }

            for i in first..first + count {
                self.used[i / 64] |= 1 << (i % 64);
            }

            return unsafe { self.base.add(first * GRANULE) };
        }

        ptr::null_mut()
    }

    /// # Safety
    /// `pointer` and `layout` must exactly match a live allocation from this heap;
    /// all accesses must have ended. Call once only, with no surviving references.
    pub unsafe fn release(&mut self, pointer: *mut u8, layout: Layout) {
        let first = (pointer as usize - self.base as usize) / GRANULE;
        for i in first..first + layout.size().div_ceil(GRANULE) {
            self.used[i / 64] &= !(1 << (i % 64));
        }
    }
}

/// Serialized allocator. Interrupt handlers must never allocate: taking this
/// lock from an interrupt that preempted its owner would deadlock.
pub struct LockedHeap<const WORDS: usize> {
    locked: AtomicBool,
    heap: UnsafeCell<Heap<WORDS>>,
}
// All state access is serialized by the acquire/release lock.
unsafe impl<const WORDS: usize> Sync for LockedHeap<WORDS> {}

impl<const WORDS: usize> Default for LockedHeap<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> LockedHeap<WORDS> {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            heap: UnsafeCell::new(Heap::empty()),
        }
    }
    fn with<R>(&self, f: impl FnOnce(&mut Heap<WORDS>) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        struct Unlock<'a>(&'a AtomicBool);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        let _unlock = Unlock(&self.locked);
        f(unsafe { &mut *self.heap.get() })
    }

    /// # Safety
    /// Same ownership contract as `Heap::init`; call before using `alloc`.
    pub unsafe fn init(&self, base: *mut u8, bytes: usize) -> bool {
        self.with(|heap| unsafe { heap.init(base, bytes) })
    }
}

// Allocations never overlap, respect Layout, and are reclaimed only with the
// original pointer/Layout. These methods do not allocate or unwind on valid use.
unsafe impl<const WORDS: usize> GlobalAlloc for LockedHeap<WORDS> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|heap| heap.allocate(layout))
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        self
            .with(
                |heap| 
                unsafe { 
                heap
                    .release(pointer, layout) 
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{boxed::Box, vec::Vec};
    #[repr(align(4096))]
    struct Storage([u8; 16384]);
    
    #[test]
    fn alignment_exhaustion_fragmentation_and_reuse() {
        let mut storage = Box::new(Storage([0; 16384]));
        let mut heap = Heap::<4>::empty();
        
        assert!(unsafe { heap.init(storage.0.as_mut_ptr(), 16384) });
        assert!(!unsafe { heap.init(storage.0.as_mut_ptr(), 16384) });
        
        let mut live = Vec::new();
        for (size, align) in [(1, 1), (65, 128), (100, 4096), (7000, 64)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let p = heap.allocate(layout);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0);
            unsafe {
                ptr::write_bytes(p, 0xab, size);
            }
            live.push((p, layout));
        }

        assert!(heap
            .allocate(Layout::from_size_align(16384, 1).unwrap())
            .is_null());
        for (p, layout) in live {
            unsafe {
                heap.release(p, layout);
            }
        }

        let all = Layout::from_size_align(16384, 4096).unwrap();
        assert_eq!(heap.allocate(all), storage.0.as_mut_ptr());
        assert!(heap.allocate(Layout::new::<u64>()).is_null());
        unsafe {
            heap.release(storage.0.as_mut_ptr(), all);
        }
        assert_eq!(heap.allocate(all), storage.0.as_mut_ptr());
    }

    #[test]
    fn mixed_allocations_do_not_overlap_or_corrupt_live_bytes() {
        let mut storage = Box::new(Storage([0; 16384]));
        let mut heap = Heap::<4>::empty();
        assert!(unsafe { heap.init(storage.0.as_mut_ptr(), 16384) });
        let mut live = Vec::new();
        let mut rng = 7u64;
        for step in 0..3000 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            if step % 3 == 0 && !live.is_empty() {
                let (p, layout, byte): (*mut u8, Layout, u8) =
                    live.swap_remove(rng as usize % live.len());
                assert!(unsafe { core::slice::from_raw_parts(p, layout.size()) }
                    .iter()
                    .all(|&b| b == byte));
                unsafe {
                    heap.release(p, layout);
                }
            } else {
                let layout =
                    Layout::from_size_align((rng as usize % 900) + 1, 1 << ((rng >> 32) % 13))
                        .unwrap();
                let p = heap.allocate(layout);
                if !p.is_null() {
                    assert_eq!(p as usize % layout.align(), 0);
                    for &(q, other, _) in &live {
                        assert!(
                            p as usize + layout.size() <= q as usize
                                || q as usize + other.size() <= p as usize
                        );
                    }
                    unsafe {
                        ptr::write_bytes(p, step as u8, layout.size());
                    }
                    live.push((p, layout, step as u8));
                }
            }
        }

        for (p, layout, byte) in live {
            assert!(unsafe { core::slice::from_raw_parts(p, layout.size()) }
                .iter()
                .all(|&b| b == byte));
            unsafe {
                heap.release(p, layout);
            }
        }

        assert!(!heap
            .allocate(Layout::from_size_align(16384, 4096).unwrap())
            .is_null());
    }

    #[test]
    fn uninitialized_and_invalid_regions_fail_closed() {
        let mut heap = Heap::<1>::empty();
        assert!(heap.allocate(Layout::new::<u64>()).is_null());

        let mut storage = Box::new(Storage([0; 16384]));
        assert!(!unsafe { heap.init(storage.0.as_mut_ptr().add(1), 64) });
        assert!(!unsafe { heap.init(storage.0.as_mut_ptr(), 16384) });
        assert!(!unsafe { heap.init(storage.0.as_mut_ptr(), 0) });
    }
}

