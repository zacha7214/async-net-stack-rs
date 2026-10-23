//! Raspberry Pi 4 board: runtime bring-up only. The Ethernet (GENET) driver,
//! MMU/cache setup and device-tree parsing are later milestones.
#![no_std]
#![no_main]
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    panic::PanicInfo,
};

extern crate alloc;
use runtime::{
    memory::{self, dma::DmaAllocator, heap::LockedHeap},
    pl011::Uart,
    selftests,
};

// Real hardware caveat: with the MMU off, all data accesses are Device memory,
// where the heap lock's exclusive load/store may never succeed. QEMU raspi4b
// does not model this; enable the MMU before relying on the heap on silicon.
#[global_allocator]
static HEAP: LockedHeap<512> = LockedHeap::new();
mod board;
global_asm!(include_str!("boot.S"));

fn halt() -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let mut uart = unsafe { board::console() };
    let _ = writeln!(uart, "PANIC: {info}");

    halt()
}

#[no_mangle]
extern "C" fn exception_fatal(syndrome: u64, address: u64) -> ! {
    let mut uart = unsafe {
        board::console()
    };
    let _ = writeln!(
        uart,
        "EXCEPTION: ESR={syndrome:#x} ELR={address:#x}"
    );

    halt()
}

#[no_mangle]
pub extern "C" fn kernel_main(dtb: usize) -> ! {
    let mut uart = unsafe { Uart::new(board::UART0) };
    writeln!(
        uart,
        "rpi4-boot: EL1, core 0, polling, caches/MMU off; DTB={dtb:#x}"
    )
        .unwrap();

    let (heap_start, heap_bytes) = memory::heap_region();
    assert!(unsafe { HEAP.init(heap_start, heap_bytes) });
    selftests::heap(&HEAP, &mut uart);
    selftests::clock(&mut uart);

    // No DMA owner exists yet; the self-test returns every page.
    unsafe {
        selftests::pages::<{
            board::PAGE_POOL_WORDS
        }>(&mut uart)
    };

    // CPU-side check only: caches are off, so the pool is coherent. Whether
    // GENET sees these as identity bus addresses is part of the driver work.
    let (cpu, bytes) = memory::page_pool_region();
    let dma = unsafe {
        DmaAllocator::new(cpu, cpu.as_ptr() as usize, bytes)
    }.unwrap();
    selftests::dma(&dma, &mut uart);

    writeln!(uart, "BOOT DONE: rpi4 runtime (no network driver yet)")
        .unwrap();

    halt()
}