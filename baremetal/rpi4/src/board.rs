//! Raspberry Pi 4 (BCM2711, low-peripheral mode) addresses and clocks.
use runtime::pl011;

/// PL011 UART0 on GPIO14/15. Requires `enable_uart=1` and
/// `dtoverlay=disable-bt` in config.txt; the firmware sets the 48 MHz clock
/// and pin muxing. 3.3 V levels.
pub const UART0: pl011::Config = pl011::Config {
    base: 0xfe20_1000,
    clock_hz: 48_000_000,
    baud: 115_200,
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