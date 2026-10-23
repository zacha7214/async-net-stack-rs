//! AArch64 generic physical counter. polls instead of configuring an IRQ.
use core::arch::asm;

pub fn frequency_hz() -> u64 {
    let value: u64;

    // SAFETY: The documented boot configuration permits these EL1 reads.
    unsafe {
        asm!(
        "mrs {value}, cntfrq_el0",
        value = out(reg) value,
        options(nomem, nostack)
        )
    };

    value
}

pub fn ticks() -> u64 {
    let value: u64;

    // ISB orders instruction execution before sampling the hardware counter.
    unsafe {
        asm!(
        "isb",
        "mrs {value}, cntpct_el0",
        value = out(reg) value,
        options(nomem, nostack))
    };

    value
}

/// Busy wait using elapsed hardware ticks.
pub fn delay_ms(milliseconds: u32) {
    let duration = (u128::from(frequency_hz()) * u128::from(milliseconds)).div_ceil(1000);

    let duration = u64::try_from(duration).expect("delay exceeds counter range");

    let start = ticks();
    while ticks().wrapping_sub(start) < duration {
        core::hint::spin_loop();
    }
}