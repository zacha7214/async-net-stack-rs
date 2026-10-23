// Adapted from Zach Allen's bare-metal-utils; see docs/virtio-net-plan.md.
//! Polling PL011 UART driver. All accesses are 32-bit MMIO. Boards supply the
//! base address and reference clock; see [`Config`].
use core::{
    fmt,
    ptr::{read_volatile, write_volatile},
};

const DR: usize = 0x00;
const FR: usize = 0x18;
const IBRD: usize = 0x24;
const FBRD: usize = 0x28;
const LCR_H: usize = 0x2c;
const CR: usize = 0x30;
const IMSC: usize = 0x38;
const ICR: usize = 0x44;

/// Board-specific placement and clocking of one PL011 instance.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub base: usize,
    /// UARTCLK as configured by the board or its firmware.
    pub clock_hz: u32,
    pub baud: u32,
}

impl Config {
    /// Divisor in 1/64ths: UARTCLK / (16 * baud), rounded to nearest.
    /// Returns (IBRD, FBRD).
    pub const fn divisors(&self) -> (u32, u32) {
        let scaled = (self.clock_hz as u64 * 4 + self.baud as u64 / 2) / self.baud as u64;
        ((scaled >> 6) as u32, (scaled & 0x3f) as u32)
    }
}

pub struct Uart {
    base: usize,
}

impl Uart {
    /// Initialize as `config.baud`, 8 data bits, no parity, one stop bit.
    ///
    /// # Safety
    /// `config.base` must be a PL011 mapped as Device memory (or accessed with
    /// the MMU off), and the caller must own it exclusively. These runtimes have
    /// one CPU and masked interrupts; this is not a shared lock. Calling again
    /// (e.g. from a panic handler) re-initializes the same device.
    pub unsafe fn new(config: Config) -> Self {
        let mut uart = Self { base: config.base };
        while uart.read(FR) & (1 << 3) != 0 {
            core::hint::spin_loop();
        }
        uart.write(CR, 0);
        uart.write(IMSC, 0);
        uart.write(ICR, 0x7ff);
        let (integer, fraction) = config.divisors();
        uart.write(IBRD, integer);
        uart.write(FBRD, fraction);
        uart.write(LCR_H, (3 << 5) | (1 << 4)); // 8-bit words, FIFO enabled.
        uart.write(CR, 1 | (1 << 8) | (1 << 9)); // UART, TX and RX enabled.
        uart
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: Construction establishes the device mapping and ownership;
        // internal offsets refer to aligned PL011 registers.
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&mut self, offset: usize, value: u32) {
        // SAFETY: Same mapping/ownership contract as read(). Volatile prevents
        // removal of hardware accesses; it is not a general memory barrier.
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    pub fn write_byte(&mut self, byte: u8) {
        while self.read(FR) & (1 << 5) != 0 {
            core::hint::spin_loop();
        }
        self.write(DR, u32::from(byte));
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }

            self.write_byte(byte);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn divisors_match_reference_clocks() {
        let at = |clock_hz| {
            Config {
                base: 0,
                clock_hz,
                baud: 115_200,
            }
            .divisors()
        };
        // QEMU virt: 24 MHz / (16 * 115200) = 13 + 1/48 -> FBRD 1.
        assert_eq!(at(24_000_000), (13, 1));
        // Raspberry Pi 4 firmware default UARTCLK 48 MHz -> 26 + 0.0417.
        assert_eq!(at(48_000_000), (26, 3));
    }
}

