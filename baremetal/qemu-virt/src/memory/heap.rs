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
