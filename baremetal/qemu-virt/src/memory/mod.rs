//! Physical memory management; deliberately independent of a Rust heap.
pub mod dma;
pub mod heap;
pub mod page;

use page::{InitError, PageAllocator};

/// Claim the linker-reserved 4 MiB QEMU pool (1024 frames, 128 bytes of bitmap).
///
/// # Safety
/// Call only once, using this project's linker script and QEMU boot contract.
/// No other code may own or use the pool until frames are allocated from it.
/// All CPUs/interrupt handlers must coordinate through this allocator.
pub unsafe fn qemu_page_pool() -> Result<PageAllocator<16>, InitError> {
    extern "C" {
        static __page_pool_start: u8;
        static __page_pool_end: u8;
    }

    let start = core::ptr::addr_of!(__page_pool_start) as usize;
    let end = core::ptr::addr_of!(__page_pool_end) as usize;

    // SAFETY: Caller grants exclusive ownership of the linker-reserved region.
    unsafe {
        PageAllocator::new(start, end)
    }
}
