use super::PacketBuf;
use alloc::vec::Vec;

/// A network device backend (TUN, AF_XDP, …).
///
/// Implementations are single-core by design and are therefore `!Send` /
/// `!Sync`. The shared frame pool uses interior mutability so the hot-path
/// methods can be expressed against `&self`; the device wrapper only needs
/// `&mut self` to hand out exclusive [`PacketBuf`] handles.
pub trait Device {
    /// Advance protocol adapters with application-owned monotonic time. Raw
    /// backends have no protocol timers. Call even while idle; recv still drives RX.
    fn poll_at(&mut self, _now: core::time::Duration) -> crate::platform::io::Result<()> {
        Ok(())
    }

    /// Adapter timer hint; raw devices normally have no protocol deadlines.
    fn next_deadline(&self) -> Option<core::time::Duration> {
        None
    }

    /// Clear/recycle the previous `out`, then receive up to `max` frames.
    /// The device populates `out` with [`PacketBuf`]s
    /// backed by the device's own pool. For zero-copy backends, these may be
    /// frames the kernel already filled; for TUN, they are freshly allocated.
    fn recv(&mut self, max: usize, out: &mut Vec<PacketBuf>) -> crate::platform::io::Result<usize>;

    /// Submit an accepted prefix of frames, without waiting for queue space.
    /// `Ok(n)` replaces `frames[..n]` with valid empty buffers; the suffix
    /// remains owned by the caller and can be retried. `Err` consumes nothing.
    /// Asynchronous backends report acceptance, not wire delivery; completion
    /// errors are exposed by their progress/stats API.
    fn send(&mut self, frames: &mut [PacketBuf]) -> crate::platform::io::Result<usize>;

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
pub fn recycle_frames(frames: &mut [PacketBuf]) {
    for frame in frames {
        drop(core::mem::take(frame));
    }
}
