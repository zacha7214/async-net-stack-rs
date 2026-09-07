//! AF_XDP (XDP sockets) backend — Linux only (`feature = "xdp"`).
//!
//! A libc-only implementation: hand-written ABI structs ([`sys`]), UMEM +
//! fill/completion rings ([`umem`]), RX/TX rings with zero-copy→copy bind
//! probing ([`socket`]), a hand-written default BPF program + XSKMAP loaded
//! with raw `bpf(2)` and attached over netlink ([`prog`]), and the
//! [`XdpDevice`] `Device` implementation ([`device`]).
//!
//! Requires kernel >= 5.4 (ring flags, `IFLA_XDP_EXPECTED_FD`); zero-copy
//! additionally requires driver support and is probed at runtime.
pub mod sys;

mod device;
mod prog;
mod socket;
mod umem;

pub use device::{XdpConfig, XdpDevice};
pub use prog::AttachMode;
pub use socket::XskSocket;
pub use sys::XdpStatistics;
pub use umem::UMem;

#[cfg(test)]
mod tests;
