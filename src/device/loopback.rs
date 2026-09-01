//! In-memory loopback device for tests and benchmarks.
//!
//! Models the copy-based TUN data path without touching the kernel:
//! * [`Device::send`] copies each payload into a fixed-size "kernel" arena and
//!   recycles the TX frame (like `write(2)` copying out of the pool);
//! * [`Device::recv`] copies the queued payload back into a freshly allocated
//!   frame (like `read(2)` copying into the pool).
//!
//! Because it needs no privileges and no peer, it is the harness the
//! [`Device`](crate::device::Device) tests and benchmarks run against.

use std::collections::VecDeque;
use std::io;

use crate::device::Device;
use crate::device::buffer_pool::{FramePool, PacketBuf};
use crate::device::recycle_frames;

const FRAME_COUNT: usize = 256;
const FRAME_SIZE: usize = 2048;
const FRAME_ALIGN: usize = 4096;

pub struct LoopbackDevice {
    pool: FramePool,
    /// Contiguous "kernel" buffer: `kernel_slots` regions of `frame_size` bytes
    /// each.
    arena: Vec<u8>,
    /// Free arena slots (LIFO stack, like the frame pool's free list).
    free_slots: Vec<usize>,
    /// Queued `(slot, len)` descriptors awaiting [`Device::recv`].
    rx: VecDeque<(usize, usize)>,
    /// Number of frames dropped because the kernel arena was full.
    dropped: u64,
}

impl LoopbackDevice {
    pub fn new() -> Self {
        Self::with_capacity(FRAME_COUNT, FRAME_COUNT)
    }

    /// Create a loopback device with a `pool_frames`-frame pool (app side) and a
    /// `kernel_slots`-slot kernel queue. Decoupling them lets tests exercise TX
    /// backpressure without starving the frame pool.
    pub fn with_capacity(pool_frames: usize, kernel_slots: usize) -> Self {
        assert!(pool_frames > 0, "pool_frames must be > 0");
        assert!(kernel_slots > 0, "kernel_slots must be > 0");

        let mut free_slots = Vec::with_capacity(kernel_slots);
        for i in (0..kernel_slots).rev() {
            free_slots.push(i);
        }

        Self {
            pool: FramePool::new(pool_frames, FRAME_SIZE, FRAME_ALIGN),
            arena: vec![0u8; kernel_slots * FRAME_SIZE],
            free_slots,
            rx: VecDeque::with_capacity(kernel_slots),
            dropped: 0,
        }
    }

    /// Number of frames currently queued and awaiting [`Device::recv`].
    pub fn queued(&self) -> usize {
        self.rx.len()
    }

    /// Number of frames dropped because the kernel arena was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl Default for LoopbackDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl Device for LoopbackDevice {
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> io::Result<usize> {
        out.clear();
        let slot_size = self.pool.frame_size();
        let n = max.min(self.rx.len());

        for _ in 0..n {
            let Some((slot, len)) = self.rx.pop_front() else {
                break;
            };
            let Some(mut buf) = self.alloc() else {
                // Pool exhausted: put the slot back and stop.
                self.free_slots.push(slot);
                break;
            };
            let base = slot * slot_size;
            let off = buf.data_offset();
            buf.as_mut_slice()[off..off + len].copy_from_slice(&self.arena[base..base + len]);
            buf.set_len(len);
            out.push(buf);
            self.free_slots.push(slot);
        }

        Ok(out.len())
    }

    fn send(&mut self, frames: &mut [PacketBuf]) -> io::Result<usize> {
        let slot_size = self.pool.frame_size();
        let mut sent = 0usize;

        for (i, frame) in frames.iter().enumerate() {
            let src = frame.as_slice();
            let Some(slot) = self.free_slots.pop() else {
                // Kernel arena full: drop the rest (best-effort, like a full TX
                // queue) and stop.
                self.dropped += (frames.len() - i) as u64;
                break;
            };
            let base = slot * slot_size;
            self.arena[base..base + src.len()].copy_from_slice(src);
            self.rx.push_back((slot, src.len()));
            sent += 1;
        }

        // The device has taken ownership of the whole slice; recycle every frame.
        recycle_frames(frames);
        Ok(sent)
    }

    fn alloc(&mut self) -> Option<PacketBuf> {
        let idx = self.pool.alloc()?;
        Some(self.pool.packet_buf(idx, 0))
    }

    fn frame_size(&self) -> usize {
        self.pool.frame_size()
    }
}
