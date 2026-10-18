mod backend;
pub mod dma;

// pub (crate) for test access
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
pub use buffer_pool::{PacketBuf, PacketPool};
pub use loopback::LoopbackDevice;

#[cfg(all(feature = "tun", target_os = "macos"))]
pub use tun_reactor::UtunReactor;

mod contract;
pub(crate) use contract::recycle_frames;
pub use contract::Device;

#[cfg(all(feature = "io_uring", target_os = "linux"))]
pub use backend::uring::{UringConfig, UringStats, UringTunDevice};
