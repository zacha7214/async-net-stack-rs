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
//! 1. **free** — on the free list, owned by the pool;
//! 2. **live** — owned by exactly one [`PacketBuf`];
//! 3. **in flight** — owned by the device (e.g. queued by a loopback device,
//!    or handed to the kernel by a NIC backend).
//!
//! A [`PacketBuf`] automatically returns its frame to the free list when it is
//! dropped (`Vec::clear`, scope exit, …), or explicitly via
//! [`PacketBuf::recycle`].

use std::alloc::{self, Layout};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::ptr::NonNull;

/// A pre allocated arena of fixed-size frames.
///
/// Not `Send` / `Sync`: it is a single-core by design.
pub struct FramePool {
    /// Base of the arena
    ptr: NonNull<u8>,
    /// Total arena size in bytes
    total: usize,
    /// Stride between frames in bytes.
    frame_size: usize,
    /// total frame count
    num_frames: usize,
    /// alignment the arena was allocated with (for 'dealloc')
    layout: Layout,
    /// Whether this pool owns (and must free) `ptr`. `false` when constructed
    /// around externally managed memory (e.g. an AF_XDP UMEM mmap).
    owns_memory: bool,
    /// Owns the free-list memory.
    _free_list: Box<[usize]>,
    /// Cached pointer to `free_list` data. Stable across moves because
    /// the Box heap allocation doesn't relocate.
    free_list_ptr: *mut usize,
    /// Stack depth. The only mutable state; Cell gives us &self access.
    free_count: Cell<usize>,
    #[cfg(debug_assertions)]
    in_use: std::cell::RefCell<Vec<u8>>,
}

impl FramePool {
    /// Allocate a pool of `num_frames` frames of `frame_size` bytes each,
    /// aligned to `alignment` bytes.
    ///
    /// `frame_size` must be at least `size_of::<usize>()` because the free list
    /// stores its next-pointer inside free frames.
    pub(crate) fn new(num_frames: usize, frame_size: usize, alignment: usize) -> Self {
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

        let mut indices = Vec::with_capacity(num_frames);
        for i in (0..num_frames).rev() {
            indices.push(i);
        }
        let mut free_list = indices.into_boxed_slice();

        // Heap data pointer — stable for the lifetime of the Box.
        let free_list_ptr = (&mut *free_list).as_mut_ptr();

        let pool = Self {
            ptr,
            total,
            frame_size,
            num_frames,
            layout,
            owns_memory: true,
            _free_list: free_list,
            free_list_ptr,
            free_count: Cell::new(num_frames),
            #[cfg(debug_assertions)]
            in_use: RefCell::new(vec![0u8; num_frames.div_ceil(8)]),
        };

        pool
    }

    pub(crate) fn alloc(&self) -> Option<usize> {
        let count = self.free_count.get();
        if count == 0 {
            return None;
        }
        self.free_count.set(count - 1);
        // SAFETY: single-threaded, count-1 is in bounds.
        let idx = unsafe { *self.free_list_ptr.add(count - 1) };
        #[cfg(debug_assertions)]
        self.mark_used(idx, true);
        Some(idx)
    }

    pub(crate) fn free(&self, idx: usize) {
        #[cfg(debug_assertions)]
        self.mark_used(idx, false);
        let count = self.free_count.get();
        debug_assert!(count < self.num_frames, "free list overflow");
        // SAFETY: single-threaded, count is in bounds.
        unsafe {
            *self.free_list_ptr.add(count) = idx;
        }
        self.free_count.set(count + 1);
    }

    pub fn alloc_n(&self, out: &mut [usize]) -> usize {
        let count = self.free_count.get();
        let n = out.len().min(count);
        // SAFETY: non-overlapping, both pointers valid.
        unsafe {
            std::ptr::copy_nonoverlapping(self.free_list_ptr.add(count - n), out.as_mut_ptr(), n);
        }
        #[cfg(debug_assertions)]
        for i in 0..n {
            self.mark_used(out[i], true);
        }
        self.free_count.set(count - n);
        n
    }

    pub(crate) fn free_n(&self, indices: &[usize]) {
        #[cfg(debug_assertions)]
        for &idx in indices {
            self.mark_used(idx, false);
        }
        let count = self.free_count.get();
        let n = indices.len();
        debug_assert!(count + n <= self.num_frames, "free list overflow");
        // SAFETY: non-overlapping, both pointers valid.
        unsafe {
            std::ptr::copy_nonoverlapping(indices.as_ptr(), self.free_list_ptr.add(count), n);
        }
        self.free_count.set(count + n);
    }

    pub fn packet_buf(&self, idx: usize, len: usize) -> PacketBuf {
        debug_assert!(idx < self.num_frames);
        debug_assert!(len <= self.frame_size);

        // Reserve 128 bytes of headroom by default, or 1/4 of frame, whichever fits.
        let data_offset = (self.frame_size / 4).min(128).min(self.frame_size - len);

        PacketBuf {
            ptr: NonNull::new(self.frame_ptr(idx)).expect("non-null arena pointer"),
            capacity: self.frame_size,
            len,
            idx,
            _data_offset: data_offset,
            pool: self as *const FramePool,
        }
    }

    #[inline]
    fn frame_ptr(&self, idx: usize) -> *mut u8 {
        // SAFETY: base + idx*stride is within the arena by construction.
        unsafe { self.ptr.as_ptr().add(idx * self.frame_size) }
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

        // Build the free list stack exactly as in `new`
        let mut indices = Vec::with_capacity(num_frames);
        for i in (0..num_frames).rev() {
            indices.push(i);
        }

        let mut free_list = indices.into_boxed_slice();
        let free_list_ptr = (&mut *free_list).as_mut_ptr();
        Self {
            ptr,
            total,
            frame_size,
            num_frames,
            layout: Layout::from_size_align(1, 1).unwrap(),
            owns_memory: false,
            _free_list: free_list,
            free_list_ptr,
            free_count: Cell::new(num_frames), // all frames start free
            #[cfg(debug_assertions)]
            in_use: RefCell::new(vec![0u8; num_frames.div_ceil(8)]),
        }
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

    #[cfg(debug_assertions)]
    #[inline]
    fn mark_used(&self, idx: usize, used: bool) {
        let mut bits = self.in_use.borrow_mut();
        let (byte, bit) = (idx / 8, idx % 8);
        let mask = 1u8 << bit;

        if used {
            assert_eq!(bits[byte] & mask, 0, "frame {idx} double-allocated");
            bits[byte] |= mask;
        } else {
            assert_ne!(bits[byte] & mask, 0, "frame {idx} double-freed");
            bits[byte] &= !mask;
        }
    }
}

impl fmt::Debug for FramePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramePool")
            .field("num_frames", &self.num_frames)
            .field("frame_size", &self.frame_size)
            .field("total_bytes", &self.total_bytes())
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
    _data_offset: usize,
    capacity: usize,
    len: usize,
    idx: usize,
    /// Pointer to the owning pool; `null` once recycled/sent (disarms `Drop`).
    pool: *const FramePool,
}

impl PacketBuf {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(self.data_offset()), self.len) }
    }

    /// for later header prepends.
    #[inline]
    pub fn set_headroom(&mut self, headroom: usize) {
        assert!(headroom + self.len <= self.capacity);
        self._data_offset = headroom;
    }

    /// Prepend `bytes` to the front of the packet by moving `data_offset`
    /// backward and copying. Zero-copy relative to the frame; only copies
    /// the header bytes into the reserved headroom.
    #[inline]
    pub fn push_header(&mut self, bytes: &[u8]) {
        assert!(
            self._data_offset >= bytes.len(),
            "headroom exhausted: need {} bytes, have {} remaining",
            bytes.len(),
            self._data_offset,
        );
        self._data_offset -= bytes.len();
        self.len += bytes.len();
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.ptr.as_ptr().add(self._data_offset),
                bytes.len(),
            );
        }
    }

    #[inline]
    pub fn data_offset(&self) -> usize {
        self._data_offset
    }

    /// Strip `n` bytes from the front of the packet (e.g. after parsing a
    /// header that has been consumed). This moves `data_offset` forward.
    #[inline]
    pub fn pull_header(&mut self, n: usize) {
        assert!(n <= self.len);
        self._data_offset += n;
        self.len -= n;
    }

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
        self.capacity
    }

    /// The entire frame as a mutable slice from byte 0.
    /// Used by backends for DMA/`read(2)`. Application payload should be
    /// written at `data_offset` (or use `as_mut_packet` after setting len).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: this buffer exclusively owns its frame.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity) }
    }

    /// The valid packet bytes as a mutable slice.
    #[inline]
    pub fn as_mut_packet(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.data_offset()), self.len)
        }
    }

    /// Set the number of valid bytes (must be `<= capacity()`).
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.capacity,
            "len {len} exceeds capacity {}",
            self.capacity
        );

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
            .field("capacity", &self.capacity)
            .field("frame", &self.idx)
            .field("recycled", &self.pool.is_null())
            .finish()
    }
}
