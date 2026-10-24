// Adapted from Zach Allen's bare-metal-utils; see docs/virtio-net-plan.md.
//! Physical memory management; deliberately independent of a Rust heap.
pub mod dma;
pub mod heap;
pub mod page;

pub use linked::{heap_region, linker_page_pool, page_pool_region};

/// Regions reserved by `runtime-sections.ld`. Board scripts choose the sizes.
mod linked {
    use super::page::{InitError, PageAllocator};
    use core::ptr::NonNull;

    extern "C" {
        static mut __heap_start: u8;
        static __heap_end: u8;
        static mut __page_pool_start: u8;
        static __page_pool_end: u8;
    }

    /// Start and length of the linker-reserved heap. Only addresses are
    /// computed; ownership is granted by `LockedHeap::init`.
    pub unsafe fn heap_region() -> (*mut u8, usize) {
        let start = core::ptr::addr_of_mut!(__heap_start);
        let end = core::ptr::addr_of!(__heap_end) as usize;
        
        (start, end - start as usize)
    }

    /// Start and length of the linker-reserved, page-aligned physical pool.
    pub unsafe fn page_pool_region() -> (NonNull<u8>, usize) {
        let start = core::ptr::addr_of_mut!(__page_pool_start);
        let end = core::ptr::addr_of!(__page_pool_end) as usize;
        
        (NonNull::new(start).unwrap(), end - start as usize)
    }

    /// Claim the linker-reserved pool as raw physical pages. `WORDS * 64`
    /// pages must cover the board's `__page_pool_size`.
    ///
    /// # Safety
    /// Call at most once at a time: no other code (including a `DmaAllocator`
    /// over the same region) may own or use the pool until it is dropped.
    /// All CPUs/interrupt handlers must coordinate through this allocator.
    pub unsafe fn linker_page_pool<const WORDS: usize>() -> Result<PageAllocator<WORDS>, InitError>
    {
        let (start, bytes) = page_pool_region();
        let start = start.as_ptr() as usize;
        
        // SAFETY: Caller grants exclusive ownership of the linker-reserved region.
        unsafe {
            PageAllocator::new(start, start + bytes)
        }
    }
}

