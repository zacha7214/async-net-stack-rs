.section .text.boot, "ax"
.global _start
.type _start, %function
_start:
    // Contract: QEMU virt, secure=off, virtualization=off, one CPU, MMU off.
    // x0 is passed through unchanged as kernel_main's boot argument.
    msr daifset, #0xf
    mrs x1, CurrentEL
    cmp x1, #4                   // EL1 only; do not silently run at another EL.
    b.ne __runtime_park
    mrs x1, sctlr_el1
    tst x1, #1                 // MMU must be off.
    b.ne __runtime_park
    tst x1, #4                 // Data cache must be off.
    b.ne __runtime_park
    tst x1, #0x1000            // Instruction cache must be off.
    b.ne __runtime_park
    b __runtime_start

