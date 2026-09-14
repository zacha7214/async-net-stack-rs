//! Fixed arenas with exclusive packet handles. A non-atomic `Rc` keeps the
//! arena and free list alive across device moves and outstanding packets.
use std::alloc::{self, Layout};
use std::any::Any;
use std::cell::Cell;
use std::fmt;
use std::ptr::NonNull;
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct FramePool(Rc<PoolInner>);

struct PoolInner {
    ptr: NonNull<u8>,
    frame_size: usize,
    num_frames: usize,
    layout: Option<Layout>,
    // Keeps an externally mapped arena alive, independently of its socket.
    _owner: Option<Rc<dyn Any>>,
    // Cell permits mutation through shared pool handles without aliasing &mut.
    free_list: Box<[Cell<usize>]>,
    free_count: Cell<usize>,
    #[cfg(debug_assertions)]
    in_use: Box<[Cell<bool>]>,
}

impl FramePool {
    pub(crate) fn new(num_frames: usize, frame_size: usize, alignment: usize) -> Self {
        assert!(num_frames > 0 && frame_size > 0);
        let total = num_frames
            .checked_mul(frame_size)
            .expect("arena size overflow");
        let layout = Layout::from_size_align(total, alignment).expect("invalid layout");
        // Safe slice access may expose any byte, including unused capacity.
        // Zero once on construction; no per-packet clearing is necessary.
        let ptr = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        Self::build(ptr, frame_size, num_frames, Some(layout), None)
    }

    fn build(
        ptr: NonNull<u8>,
        frame_size: usize,
        num_frames: usize,
        layout: Option<Layout>,
        owner: Option<Rc<dyn Any>>,
    ) -> Self {
        Self(Rc::new(PoolInner {
            ptr,
            frame_size,
            num_frames,
            layout,
            _owner: owner,
            free_list: (0..num_frames).rev().map(Cell::new).collect(),
            free_count: Cell::new(num_frames),
            #[cfg(debug_assertions)]
            in_use: (0..num_frames).map(|_| Cell::new(false)).collect(),
        }))
    }

    /// # Safety
    /// The initialized arena must remain valid and exclusively managed by this
    /// pool until ALL its packet handles have dropped, not just the device.
    #[cfg(test)]
    pub(crate) unsafe fn from_raw_parts(
        ptr: *mut u8,
        total: usize,
        frame_size: usize,
        num_frames: usize,
    ) -> Self {
        Self::from_region(ptr, total, frame_size, num_frames, None)
    }

    /// # Safety
    /// `owner` must keep this initialized mapping valid; only this pool and
    /// kernel-owned frames may access it. Each live frame has a single owner.
    #[cfg(all(feature = "xdp", target_os = "linux"))]
    pub(crate) unsafe fn from_owned_region(
        ptr: *mut u8,
        total: usize,
        frame_size: usize,
        num_frames: usize,
        owner: Rc<dyn Any>,
    ) -> Self {
        Self::from_region(ptr, total, frame_size, num_frames, Some(owner))
    }

    #[cfg(any(test, all(feature = "xdp", target_os = "linux")))]
    unsafe fn from_region(
        ptr: *mut u8,
        total: usize,
        frame_size: usize,
        num_frames: usize,
        owner: Option<Rc<dyn Any>>,
    ) -> Self {
        assert!(num_frames > 0 && frame_size > 0);
        assert!(num_frames.checked_mul(frame_size).expect("arena overflow") <= total);
        Self::build(
            NonNull::new(ptr).expect("null arena"),
            frame_size,
            num_frames,
            None,
            owner,
        )
    }

    #[inline]
    pub(crate) fn alloc(&self) -> Option<usize> {
        let count = self.available();
        if count == 0 {
            return None;
        }
        let idx = self.0.free_list[count - 1].get();
        self.0.free_count.set(count - 1);
        #[cfg(debug_assertions)]
        self.mark_used(idx, true);
        Some(idx)
    }

    #[inline]
    pub(crate) fn free(&self, idx: usize) {
        #[cfg(debug_assertions)]
        self.mark_used(idx, false);
        let count = self.available();
        self.0.free_list[count].set(idx);
        self.0.free_count.set(count + 1);
    }

    pub(crate) fn alloc_n(&self, out: &mut [usize]) -> usize {
        let count = self.available();
        let n = out.len().min(count);
        for (dst, src) in out[..n].iter_mut().zip(&self.0.free_list[count - n..count]) {
            *dst = src.get();
            #[cfg(debug_assertions)]
            self.mark_used(*dst, true);
        }
        self.0.free_count.set(count - n);
        n
    }

    #[cfg(any(test, all(feature = "xdp", target_os = "linux")))]
    pub(crate) fn free_n(&self, indices: &[usize]) {
        let count = self.available();
        #[cfg(debug_assertions)]
        for &idx in indices {
            self.mark_used(idx, false);
        }
        for (&idx, dst) in indices
            .iter()
            .zip(&self.0.free_list[count..count + indices.len()])
        {
            dst.set(idx);
        }
        self.0.free_count.set(count + indices.len());
    }

    /// Internal ownership transfer: idx must be allocated and have no handle.
    pub(crate) fn packet_buf(&self, idx: usize, len: usize) -> PacketBuf {
        assert!(idx < self.num_frames() && len <= self.frame_size());
        PacketBuf {
            ptr: unsafe {
                NonNull::new_unchecked(self.0.ptr.as_ptr().add(idx * self.frame_size()))
            },
            data_offset: (self.frame_size() / 4)
                .min(128)
                .min(self.frame_size() - len),
            capacity: self.frame_size(),
            len,
            idx,
            pool: Some(self.clone()),
        }
    }

    pub(crate) fn alloc_batch(&self, max: usize, out: &mut Vec<PacketBuf>) -> usize {
        let mut indices = [0; 64];
        let mut total = 0;
        while total < max {
            let limit = indices.len().min(max - total);
            let n = self.alloc_n(&mut indices[..limit]);
            out.extend(indices[..n].iter().map(|&idx| self.packet_buf(idx, 0)));
            total += n;
            if n < limit {
                break;
            }
        }
        total
    }

    pub(crate) fn available(&self) -> usize {
        self.0.free_count.get()
    }
    pub(crate) fn num_frames(&self) -> usize {
        self.0.num_frames
    }
    pub(crate) fn frame_size(&self) -> usize {
        self.0.frame_size
    }

    #[cfg(debug_assertions)]
    fn mark_used(&self, idx: usize, used: bool) {
        let old = self.0.in_use[idx].replace(used);
        if used {
            assert!(!old, "frame {idx} double-allocated");
        } else {
            assert!(old, "frame {idx} double-freed");
        }
    }
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        if let Some(layout) = self.layout {
            unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) };
        }
    }
}

/// Exclusive packet storage. Handles are `!Send`/`!Sync`, may outlive their
/// device, and return their frame on drop. A consumed TX slot becomes an empty
/// zero-capacity buffer, which remains safe to inspect and drop.
pub struct PacketBuf {
    ptr: NonNull<u8>,
    data_offset: usize,
    capacity: usize,
    len: usize,
    idx: usize,
    pool: Option<FramePool>,
}

impl Default for PacketBuf {
    fn default() -> Self {
        Self {
            ptr: NonNull::dangling(),
            data_offset: 0,
            capacity: 0,
            len: 0,
            idx: 0,
            pool: None,
        }
    }
}

impl PacketBuf {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(self.data_offset), self.len) }
    }
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.capacity) }
    }
    #[inline]
    pub fn as_mut_packet(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(self.data_offset), self.len) }
    }
    pub fn set_headroom(&mut self, headroom: usize) {
        assert!(
            headroom <= self.capacity - self.len,
            "headroom exceeds capacity"
        );
        self.data_offset = headroom;
    }
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.capacity - self.data_offset,
            "len exceeds available capacity"
        );
        self.len = len;
    }
    pub fn push_header(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.data_offset, "headroom exhausted");
        self.data_offset -= bytes.len();
        self.len += bytes.len();
        self.as_mut_packet()[..bytes.len()].copy_from_slice(bytes);
    }
    pub fn pull_header(&mut self, n: usize) {
        assert!(n <= self.len);
        self.data_offset += n;
        self.len -= n;
    }
    pub fn data_offset(&self) -> usize {
        self.data_offset
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Entire frame size, including headroom. See `tail_capacity` for payload.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn tail_capacity(&self) -> usize {
        self.capacity - self.data_offset
    }
    pub fn recycle(self) {
        drop(self);
    }

    #[cfg(all(feature = "xdp", target_os = "linux"))]
    pub(crate) fn belongs_to(&self, pool: &FramePool) -> bool {
        self.pool
            .as_ref()
            .is_some_and(|p| Rc::ptr_eq(&p.0, &pool.0))
    }
    #[cfg(all(feature = "xdp", target_os = "linux"))]
    pub(crate) fn frame_index(&self) -> usize {
        self.idx
    }

    /// Transfer the frame to a kernel ring, leaving a valid empty TX slot.
    #[cfg(any(test, all(feature = "xdp", target_os = "linux")))]
    pub(crate) fn into_parts(mut self) -> (usize, usize) {
        self.pool = None;
        (self.idx, self.len)
    }
}
impl Drop for PacketBuf {
    #[inline]
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            pool.free(self.idx);
        }
    }
}
impl fmt::Debug for PacketBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketBuf")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .field("frame", &self.idx)
            .field("recycled", &self.pool.is_none())
            .finish()
    }
}
