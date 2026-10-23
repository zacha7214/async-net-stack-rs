//! AArch64 pieces shared by every board: generic timer and EL1 startup.
//!
//! The board's `_start` (in `.text.boot`) validates its own entry state, reaches
//! EL1 with interrupts masked, then branches to `__runtime_start` with `x0`
//! holding a board-defined boot argument. The runtime sets the stack, zeroes
//! BSS, installs fatal exception vectors and calls the board's
//! `extern "C" fn kernel_main(boot_arg: usize) -> !`. Exceptions call the
//! board's `extern "C" fn exception_fatal(esr: u64, elr: u64) -> !`.
pub mod timer;

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("entry.S"));