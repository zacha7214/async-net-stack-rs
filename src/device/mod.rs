mod backend;
pub(crate) mod buffer_pool;
mod loopback;
#[cfg(test)]
mod tests;
#[cfg(all(feature = "tun", target_os = "macos"))]
mod tun_reactor;

#[cfg(all(feature = "xdp", target_os = "linux"))]
pub use backend::af_xdp::sys::{
    XDP_COPY, XDP_SHARED_UMEM, XDP_UMEM_UNALIGNED_CHUNK_FLAG, XDP_USE_NEED_WAKEUP, XDP_ZEROCOPY,
};
#[cfg(all(feature = "xdp", target_os = "linux"))]
pub use backend::af_xdp::{
    AttachMode, UMem, XdpConfig, XdpCounters, XdpDevice, XdpMode, XdpStatistics, XskSocket,
};
#[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
pub use backend::DefaultDevice;
pub use backend::Error;
pub use buffer_pool::PacketBuf;
pub use loopback::LoopbackDevice;
#[cfg(all(feature = "tun", target_os = "macos"))]
pub use tun_reactor::UtunReactor;

/// A network device backend (TUN, AF_XDP, …).
///
/// Implementations are single-core by design and are therefore `!Send` /
/// `!Sync`. The shared frame pool uses interior mutability so the hot-path
/// methods can be expressed against `&self`; the device wrapper only needs
/// `&mut self` to hand out exclusive [`PacketBuf`] handles.
pub trait Device {
    /// Clear/recycle the previous `out`, then receive up to `max` frames.
    /// The device populates `out` with [`PacketBuf`]s
    /// backed by the device's own pool. For zero-copy backends, these may be
    /// frames the kernel already filled; for TUN, they are freshly allocated.
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> std::io::Result<usize>;

    /// Submit an accepted prefix of frames, without waiting for queue space.
    /// `Ok(n)` replaces `frames[..n]` with valid empty buffers; the suffix
    /// remains owned by the caller and can be retried. `Err` consumes nothing.
    /// Asynchronous backends report acceptance, not wire delivery; completion
    /// errors are exposed by their progress/stats API.
    fn send(&mut self, frames: &mut [PacketBuf]) -> std::io::Result<usize>;

    /// Allocate an empty frame for TX. Returns `None` if the pool is exhausted.
    fn alloc(&mut self) -> Option<PacketBuf>;

    /// Allocate up to `max` TX frames, appending to `out`.
    fn alloc_batch(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> usize {
        let start = out.len();
        for _ in 0..max {
            let Some(buf) = self.alloc() else {
                break;
            };
            out.push(buf);
        }
        out.len() - start
    }

    /// Maximum frame capacity (including headroom).
    fn frame_size(&self) -> usize;
}

/// Consume a submitted prefix, leaving safe empty slots.
pub(crate) fn recycle_frames(frames: &mut [PacketBuf]) {
    for frame in frames {
        drop(std::mem::take(frame));
    }
}

#[cfg(all(feature = "io_uring", target_os = "linux"))]
pub use backend::uring::{UringConfig, UringStats, UringTunDevice};
