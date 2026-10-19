//! Fixed-platform heap and DMA storage bring-up; NIC queues remain inactive.
#![no_std]
#![no_main]
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    panic::PanicInfo,
};

extern crate alloc;

fn main() {
    
}
