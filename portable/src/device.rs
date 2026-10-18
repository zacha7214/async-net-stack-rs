//! Portable packet ownership and device interface; no OS backends.
// Raw frame-index hooks serve host backends and the upcoming DMA adapter.
#[allow(dead_code)]
#[path = "../../src/device/buffer_pool.rs"]
pub(crate) mod buffer_pool;

#[path = "../../src/device/contract.rs"]
mod contract;

#[path = "../../src/device/loopback.rs"]
mod loopback;

pub use buffer_pool::{PacketBuf, PacketPool};
pub(crate) use contract::recycle_frames;
pub use contract::Device;
pub use loopback::LoopbackDevice;

#[cfg(test)]
#[path = "../../src/device/tests.rs"]
mod tests;

#[path = "../../src/device/dma.rs"]
pub mod dma;
