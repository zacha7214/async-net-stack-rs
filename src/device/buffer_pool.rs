//! Zero-copy frame pool and packet buffer handles.
//!
//! This is the shared memory substrate that every [`crate::Device`] backend is
//! built on:
//!
//! * A **copying** backend (TAP) `read()`s frames straight into pool frames and
//!   `write()`s them back out.
//! * A **zero-copy** backend (AF_XDP) hands the very same frames to the NIC so
//!   that DMA lands in pool memory directly — no copies.
//!
//! In both cases the application sees identical [`PacketBuf`] handles, so the
//! only thing that changes when you swap backends is that the copies vanish.
//!
//! # Ownership model
//!
//! A [`FramePool`] owns a single, fixed-size arena of `num_frames` frames of
//! `frame_size` bytes each. Frames are handed out one at a time and are
//! *exactly* one of:
//!
//! 1. **free** — on the intrusive free list, owned by the pool;
//! 2. **live** — owned by exactly one [`PacketBuf`];
//! 3. **in flight** — owned by the device (e.g. queued by a loopback device,
//!    or handed to the kernel by a NIC backend).
//!
//! A [`PacketBuf`] automatically returns its frame to the free list when it is
//! dropped (`Vec::clear`, scope exit, …), or explicitly via
//! [`PacketBuf::recycle`]. Frames are therefore never leaked as long as buffers
//! are dropped, and there is no per-packet heap allocation anywhere on the hot
//! path.
//!
//! # Safety / soundness
//!
//! The arena is single-threaded (`!Send` / `!Sync`). Frame memory is accessed
//! exclusively through raw pointers so that a live [`PacketBuf`] can coexist
//! with the pool borrowing its own free-list state via [`Cell`] interior
//! mutability; the *only* place a `&mut [u8]` into frame memory is ever created
//! is [`PacketBuf::as_mut_slice`], for the single frame that buffer owns.
//!
//! **Invariant:** a [`PacketBuf`] must not outlive the [`FramePool`] (and
//! therefore the device) it was allocated from. The device owns the pool, so
//! keep the device alive for as long as any of its buffers exist.

use std::alloc::{self, Layout};
use std::cell::Cell;
use std::fmt;
use std::ptr::NonNull;

/// Free-list sentinel: "no further free frame".
const NONE: usize = usize::MAX;

/// A preallocated arena of fixed-size frames.
///
/// Not `Send` / `Sync`: it is a single-core, single-threaded construct by
/// design (there are no locks or atomics on the fast path).
pub struct FramePool {
    /// Base of the arena.
    ptr: NonNull<u8>,
    /// Total arena size in bytes.
    total: usize,
    /// Stride between frames in bytes.
    frame_size: usize,
    /// Number of frames.
    num_frames: usize,
    /// Alignment the arena was allocated with (used for `dealloc`).
    layout: Layout,
    /// Whether this pool owns (and must free) `ptr`. `false` when constructed
    /// around externally managed memory (e.g. an AF_XDP UMEM mmap).
    owns_memory: bool,
    /// Head of the intrusive free list; `NONE` when exhausted.
    ///
    /// Each free frame stores the index of the next free frame in its first
    /// `usize` bytes. `Cell` lets `alloc`/`free` run through `&self`, which is
    /// what makes the "live buffers coexist with a borrowed device" model safe.
    free_head: Cell<usize>,
    #[cfg(debug_assertions)]
    in_use: std::cell::RefCell<Vec<u8>>,
}

impl FramePool {
    /// Allocate a pool of `num_frames` frames of `frame_size` bytes each,
    /// aligned to `alignment` bytes.
    ///
    /// `frame_size` must be at least `size_of::<usize>()` because the free list
    /// stores its next-pointer inside free frames.
    pub fn new(num_frames: usize, frame_size: usize, alignment: usize) -> Self {
        assert!(num_frames > 0, "num_frames must be > 0");
        assert!(
            frame_size >= std::mem::size_of::<usize>(),
            "frame_size must be at least {} bytes (intrusive free list)",
            std::mem::size_of::<usize>()
        );
        assert!(
            alignment.is_power_of_two(),
            "alignment must be a power of two"
        );
        assert!(
            alignment >= std::mem::align_of::<usize>(),
            "alignment must be at least {}",
            std::mem::align_of::<usize>()
        );

        let total = num_frames
            .checked_mul(frame_size)
            .expect("arena size overflow");

        let layout = Layout::from_size_align(total, alignment).expect("invalid layout");
        // SAFETY: `layout` has non-zero size (num_frames > 0, frame_size >= 8).
        let ptr = unsafe { alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::handle_alloc_error(layout);
        };

        let pool = Self {
            ptr,
            total,
            frame_size,
            num_frames,
            layout,
            owns_memory: true,
            free_head: Cell::new(0),
            #[cfg(debug_assertions)]
            in_use: std::cell::RefCell::new(vec![0u8; num_frames.div_ceil(8)]),
        };

        // Chain every frame onto the free list: 0 -> 1 -> … -> num_frames-1 -> NONE.
        for i in 0..num_frames {
            let next = if i + 1 < num_frames { i + 1 } else { NONE };
            // SAFETY: `i` is a valid frame index; we own the arena exclusively
            // during construction.
            unsafe { pool.write_next(i, next) };
        }
        pool
    }

    /// Wrap caller-provided memory as a frame pool.
    ///
    /// The pool does **not** own or free `ptr`; the caller must keep it alive
    /// and unaliased for the pool's lifetime. This is the hook an AF_XDP backend
    /// uses to point the pool at its UMEM mmap.
    ///
    /// # Safety
    /// `ptr` must be valid for reads and writes of `total` bytes and must not be
    /// aliased anywhere else. `num_frames * frame_size` must be `<= total`, and
    /// `frame_size >= size_of::<usize>()`.
    #[allow(dead_code)] // used by the future AF_XDP backend
    pub(crate) unsafe fn from_raw_parts(
        ptr: *mut u8,
        total: usize,
        frame_size: usize,
        num_frames: usize,
    ) -> Self {
        assert!(num_frames > 0, "num_frames must be > 0");
        assert!(
            frame_size >= std::mem::size_of::<usize>(),
            "frame_size must be at least {} bytes",
            std::mem::size_of::<usize>()
        );
        assert!(num_frames * frame_size <= total, "arena too small");
        let Some(ptr) = NonNull::new(ptr) else {
            panic!("null arena pointer");
        };

        let pool = Self {
            ptr,
            total,
            frame_size,
            num_frames,
            layout: Layout::from_size_align(1, 1).unwrap(),
            owns_memory: false,
            free_head: Cell::new(0),
            #[cfg(debug_assertions)]
            in_use: std::cell::RefCell::new(vec![0u8; num_frames.div_ceil(8)]),
        };
        for i in 0..num_frames {
            let next = if i + 1 < num_frames { i + 1 } else { NONE };
            // SAFETY: caller guarantees `ptr` is valid and unaliased.
            unsafe { pool.write_next(i, next) };
        }
        pool
    }

    /// Number of frames in the pool.
    #[inline]
    pub fn num_frames(&self) -> usize {
        self.num_frames
    }

    /// Per-frame capacity in bytes.
    #[inline]
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Total arena size in bytes.
    #[inline]
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// Pop a free frame index, or `None` if the pool is exhausted.
    #[inline]
    pub(crate) fn alloc(&self) -> Option<usize> {
        let head = self.free_head.get();
        if head == NONE {
            return None;
        }
        // SAFETY: `head` is a valid, currently-free frame index.
        let next = unsafe { self.read_next(head) };
        self.free_head.set(next);
        #[cfg(debug_assertions)]
        self.mark_used(head, true);
        Some(head)
    }

    /// Return a frame to the free list.
    ///
    /// `idx` must have been previously allocated and not already freed. This is
    /// `pub(crate)`: applications recycle only through [`PacketBuf`] (its
    /// `Drop` / `recycle`), never directly.
    #[inline]
    pub(crate) fn free(&self, idx: usize) {
        debug_assert!(idx < self.num_frames, "frame index {idx} out of range");
        #[cfg(debug_assertions)]
        self.mark_used(idx, false);
        // SAFETY: `idx` is a valid frame that is currently live (caller's
        // responsibility); we re-link it onto the free list before it is
        // handed out again.
        unsafe { self.write_next(idx, self.free_head.get()) };
        self.free_head.set(idx);
    }

    /// Build a [`PacketBuf`] for frame `idx` carrying `len` valid bytes.
    ///
    /// `idx` must be a currently-allocated (live) frame.
    #[inline]
    pub(crate) fn packet_buf(&self, idx: usize, len: usize) -> PacketBuf {
        debug_assert!(idx < self.num_frames);
        debug_assert!(len <= self.frame_size);
        PacketBuf {
            ptr: NonNull::new(self.frame_ptr(idx)).expect("non-null arena pointer"),
            cap: self.frame_size,
            len,
            idx,
            pool: self as *const FramePool,
        }
    }

    /// Raw pointer to frame `idx`.
    #[inline]
    pub(crate) fn frame_ptr(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.num_frames);
        // SAFETY: base + idx*stride is within the arena by construction.
        unsafe { self.ptr.as_ptr().add(idx * self.frame_size) }
    }

    /// Read the free-list next-pointer stored in frame `idx`.
    ///
    /// # Safety
    /// `idx` must be a valid frame index; the frame must currently be free.
    #[inline]
    unsafe fn read_next(&self, idx: usize) -> usize {
        // SAFETY: caller guarantees a valid, free frame; unaligned load is fine
        // for any stride.
        unsafe { (self.frame_ptr(idx) as *const usize).read_unaligned() }
    }

    /// Write the free-list next-pointer into frame `idx`.
    ///
    /// # Safety
    /// `idx` must be a valid frame index; the frame must currently be free.
    #[inline]
    unsafe fn write_next(&self, idx: usize, next: usize) {
        // SAFETY: caller guarantees a valid, free frame.
        unsafe { (self.frame_ptr(idx) as *mut usize).write_unaligned(next) };
    }

    #[cfg(debug_assertions)]
    #[inline]
    fn mark_used(&self, idx: usize, used: bool) {
        let mut bits = self.in_use.borrow_mut();
        let (byte, bit) = (idx / 8, idx % 8);
        let mask = 1u8 << bit;
        if used {
            assert!(bits[byte] & mask == 0, "frame {idx} double-allocated");
            bits[byte] |= mask;
        } else {
            assert!(bits[byte] & mask != 0, "frame {idx} double-freed");
            bits[byte] &= !mask;
        }
    }
}

impl fmt::Debug for FramePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramePool")
            .field("num_frames", &self.num_frames)
            .field("frame_size", &self.frame_size)
            .field("total_bytes", &self.total)
            .field("owns_memory", &self.owns_memory)
            .finish()
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        if self.owns_memory {
            // SAFETY: `ptr` was allocated with exactly this layout in `new`.
            unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }
}

/// A single frame loaned out by a [`FramePool`], owned by the application.
///
/// Recycles its frame back to the pool when dropped. `!Send` / `!Sync`, matching
/// the single-core design.
pub struct PacketBuf {
    ptr: NonNull<u8>,
    cap: usize,
    len: usize,
    idx: usize,
    /// Pointer to the owning pool; `null` once recycled/sent (disarms `Drop`).
    pool: *const FramePool,
}

impl PacketBuf {
    /// Number of valid bytes currently in the frame.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the frame carries no valid bytes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total writable capacity of the frame in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// The valid bytes as a shared slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` points at the buffer's exclusive frame, and `len <= cap`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The whole frame as a mutable slice (capacity bytes).
    ///
    /// Write your frame here, then call [`PacketBuf::set_len`] to record how
    /// many bytes are valid.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: this buffer exclusively owns its frame.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }

    /// Set the number of valid bytes (must be `<= capacity()`).
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        assert!(len <= self.cap, "len {len} exceeds capacity {}", self.cap);
        self.len = len;
    }

    /// Explicitly recycle this frame back to the pool.
    ///
    /// Equivalent to dropping the buffer; provided for clarity where the
    /// application wants to make the return explicit.
    #[inline]
    pub fn recycle(mut self) {
        self.recycle_inner();
    }

    /// Consume the buffer and hand its frame to the device's *in-flight* state
    /// (used by backends whose `send` queues frames rather than freeing them,
    /// e.g. a loopback or a NIC completion ring). Returns the frame index and
    /// length, disarming `Drop`.
    #[inline]
    pub(crate) fn into_parts(mut self) -> (usize, usize) {
        let (idx, len) = (self.idx, self.len);
        self.pool = std::ptr::null();
        (idx, len)
    }

    /// Raw pointer to the valid bytes (for `write(2)`-style backends).
    ///
    /// Only the Linux TAP backend uses this today, so it reads as dead code on
    /// non-Linux targets.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    #[inline]
    fn recycle_inner(&mut self) {
        if !self.pool.is_null() {
            // SAFETY: the pool outlives this buffer (documented invariant), and
            // `idx` is a live frame owned by this buffer.
            unsafe { (*self.pool).free(self.idx) };
            self.pool = std::ptr::null();
        }
    }
}

impl Drop for PacketBuf {
    #[inline]
    fn drop(&mut self) {
        self.recycle_inner();
    }
}

impl fmt::Debug for PacketBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketBuf")
            .field("len", &self.len)
            .field("capacity", &self.cap)
            .field("frame", &self.idx)
            .field("recycled", &self.pool.is_null())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_roundtrip_exhausts_and_reuses() {
        let pool = FramePool::new(4, 256, 4096);
        assert_eq!(pool.frame_size(), 256);
        assert_eq!(pool.num_frames(), 4);

        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(pool.alloc().unwrap());
        }
        assert!(pool.alloc().is_none(), "pool should be exhausted");

        // All four indices should be distinct.
        let mut sorted = got.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4);

        // Free two, then realloc them.
        pool.free(got[1]);
        pool.free(got[3]);
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let mut reuse = vec![a, b];
        reuse.sort_unstable();
        let mut expected = vec![got[1], got[3]];
        expected.sort_unstable();
        assert_eq!(reuse, expected);
        assert!(pool.alloc().is_none());
    }

    #[test]
    fn packet_buf_reads_writes_and_recycles() {
        let pool = FramePool::new(2, 64, 4096);
        let idx = pool.alloc().unwrap();

        {
            let mut buf = pool.packet_buf(idx, 0);
            assert!(buf.is_empty());
            assert_eq!(buf.capacity(), 64);

            let data = b"hello zero-copy";
            buf.as_mut_slice()[..data.len()].copy_from_slice(data);
            buf.set_len(data.len());

            assert_eq!(buf.len(), data.len());
            assert_eq!(buf.as_slice(), data);
        } // dropped here -> recycled

        // The same frame must be allocatable again after the buffer dropped.
        let idx2 = pool.alloc().unwrap();
        assert_eq!(idx, idx2);
    }

    #[test]
    fn explicit_recycle_and_drop_are_idempotent() {
        let pool = FramePool::new(2, 64, 4096);

        let idx = pool.alloc().unwrap();
        let buf = pool.packet_buf(idx, 4);
        buf.recycle(); // explicit recycle

        let idx2 = pool.alloc().unwrap();
        assert_eq!(idx, idx2);

        let buf = pool.packet_buf(idx2, 4);
        drop(buf); // Drop path
        let idx3 = pool.alloc().unwrap();
        assert_eq!(idx2, idx3);
    }

    #[test]
    #[should_panic(expected = "double-freed")]
    fn debug_build_detects_double_free() {
        let pool = FramePool::new(1, 64, 4096);
        let idx = pool.alloc().unwrap();
        pool.free(idx);
        pool.free(idx); // double free -> panic in debug builds
    }

    #[test]
    fn from_raw_parts_wraps_external_memory() {
        let frame_size = 128usize;
        let num = 3usize;
        let mut backing = vec![0u8; frame_size * num];
        let ptr = backing.as_mut_ptr();

        // SAFETY: `backing` stays alive for the pool's lifetime and is unaliased.
        let pool = unsafe { FramePool::from_raw_parts(ptr, backing.len(), frame_size, num) };

        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();
        assert!(pool.alloc().is_none());

        let mut buf = pool.packet_buf(a, 0);
        buf.as_mut_slice()[..4].copy_from_slice(&[1, 2, 3, 4]);
        buf.set_len(4);
        assert_eq!(buf.as_slice(), &[1, 2, 3, 4]);
        drop(buf);

        pool.free(b);
        pool.free(c);
        drop(pool);
        // backing still alive and unmodified structurally
        assert_eq!(backing.len(), frame_size * num);
    }
}
