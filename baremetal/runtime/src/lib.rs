//! Board-independent bare-metal runtime building blocks, also tested on the host.
//!
//! Boards supply physical addresses, the RAM layout (via `runtime-sections.ld`),
//! entry-state checks, the console used by panic/exception handlers, and
//! `kernel_main`. Everything here is parameterized by those choices.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod arch;
pub mod memory;
pub mod pl011;
#[cfg(target_arch = "aarch64")]
pub mod selftests;
pub mod virtio;

