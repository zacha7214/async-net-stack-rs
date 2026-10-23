//! QEMU `virt` addresses and clocks, as launched by scripts/qemu-virt-net.py.
use runtime::{pl011, virtio::probe::Window};

pub const UART0: pl011::Config = pl011::Config {
    base: 0x0900_0000,
    clock_hz: 24_000_000,
    baud: 115_200,
};

/// QEMU virt's 32 fixed virtio-mmio transport slots. Not general discovery.
pub const VIRTIO_MMIO: Window = Window {
    base: 0x0a00_0000,
    stride: 0x200,
    slots: 32,
};

/// Page-allocator bitmap words covering `__page_pool_size` in linker.ld.
pub const PAGE_POOL_WORDS: usize = 16;

/// Console for the panic/exception paths, which never return to the owner.
///
/// # Safety
/// Single CPU with interrupts masked; any previous UART owner never resumes.
pub unsafe fn console() -> pl011::Uart {
    unsafe { pl011::Uart::new(UART0) }
}

