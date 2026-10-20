//! Fixed-platform heap and DMA storage bring-up; NIC queues remain inactive.
#![no_std]
#![no_main]
use core::{
    arch::{asm, },
    panic::PanicInfo,
};
use async_net_qemu_virt::memory::{heap::LockedHeap};

#[global_allocator]
static HEAP: LockedHeap<512> = LockedHeap::new();
extern crate alloc;
fn halt() -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    halt()
}
