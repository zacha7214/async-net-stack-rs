.section .text.boot, "ax"
.global _start
.type _start, %function
_start:
    // Contract: Pi 4 firmware (or QEMU raspi4b) enters at EL2 or EL1 with the
    // MMU off and the DTB address in x0, which is preserved for kernel_main.
    msr daifset, #0xf
    mrs x1, mpidr_el1
    and x1, x1, #0xff            // Only core 0 runs; others park.
    cbnz x1, __runtime_park
    mrs x1, CurrentEL
    cmp x1, #8
    b.eq .Lfrom_el2
    cmp x1, #4
    b.eq .Lel1
    b __runtime_park             // EL3 needs the firmware's armstub first.

.Lfrom_el2:
    mov x1, #(1 << 31)           // HCR_EL2.RW: EL1 is AArch64. No traps.
    msr hcr_el2, x1
    mov x1, #3                   // CNTHCTL_EL2: EL1 physical counter/timer access.
    msr cnthctl_el2, x1
    msr cntvoff_el2, xzr
    ldr x1, =0x30d00800          // SCTLR_EL1 RES1 bits; MMU and caches off.
    msr sctlr_el1, x1
    mov x1, #0x3c5               // SPSR_EL2: EL1h, DAIF masked.
    msr spsr_el2, x1
    adr x1, .Lel1
    msr elr_el2, x1
    eret

.Lel1:
    mrs x1, sctlr_el1
    tst x1, #1                   // MMU must be off.
    b.ne __runtime_park
    tst x1, #4                   // Data cache must be off.
    b.ne __runtime_park
    b __runtime_start