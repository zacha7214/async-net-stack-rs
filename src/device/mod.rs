mod backend;
pub(crate) mod buffer_pool;
mod loopback;
#[cfg(test)]
mod tests;

pub use backend::DefaultDevice;
pub use backend::Error;
pub use buffer_pool::PacketBuf;
pub use loopback::LoopbackDevice;

/// A network device backend (TUN, AF_XDP, …).
///
/// Implementations are single-core by design and are therefore `!Send` /
/// `!Sync`. The shared frame pool uses interior mutability so the hot-path
/// methods can be expressed against `&self`; the device wrapper only needs
/// `&mut self` to hand out exclusive [`PacketBuf`] handles.
pub trait Device {
    /// Receive up to `max` frames. The device populates `out` with [`PacketBuf`]s
    /// backed by the device's own pool. For zero-copy backends, these may be
    /// frames the kernel already filled; for TUN, they are freshly allocated.
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> std::io::Result<usize>;

    /// Send frames. The device takes ownership and recycles them when the NIC
    /// or kernel has consumed them (immediately for TUN, eventually for XDP).
    fn send(&mut self, frames: &mut [PacketBuf]) -> std::io::Result<usize>;

    /// Allocate an empty frame for TX. Returns `None` if the pool is exhausted.
    fn alloc(&mut self) -> Option<PacketBuf>;

    /// Maximum frame capacity (including headroom).
    fn frame_size(&self) -> usize;
}

/// Recycle every frame in `frames`, returning each to its pool.
///
/// A backend calls this from [`Device::send`]: it has taken ownership of the
/// whole slice and must return the frames to the pool once the kernel/NIC has
/// consumed them (immediately for TUN). After this call the elements are
/// logically moved out — the caller must not read them — but their storage may
/// still be dropped or cleared afterwards because [`PacketBuf::drop`] is
/// idempotent (it checks its pool pointer before recycling).
pub(crate) fn recycle_frames(frames: &mut [PacketBuf]) {
    let ptr = frames.as_mut_ptr();
    let len = frames.len();
    for i in 0..len {
        // SAFETY: `ptr.add(i)` points to a live `PacketBuf` we exclusively own;
        // each is dropped exactly once here and never accessed again.
        unsafe { std::ptr::drop_in_place(ptr.add(i)) };
    }
}
