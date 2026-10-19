.section .text.boot, "ax"
.global _start
.type _start, %function
_start:
// QEMU virt, secure=off, virtualization=off, one CPU, MMU is off.
    msr daifset, #0xf
    mrs x0, CurrentEL
    cmp x0, #4                   // EL1 only; do not silently run at another EL.
    b.ne .Lpark
    mrs x0, sctlr_el1
    tst x0, #1                 // MMU must be off.
    b.ne .Lpark
    tst x0, #4                 // Data cache must be off.
    b.ne .Lpark
    tst x0, #0x1000            // Instruction cache must be off.
    b.ne .Lpark
    msr spsel, #1                // Use SP_EL1.
    ldr x0, =__stack_top
    mov sp, x0                   // AAPCS64 requires 16-byte stack alignment.

    ldr x0, =__bss_start
    ldr x1, =__bss_end

.Lzero:
    cmp x0, x1
    b.hs .Lready
    str xzr, [x0], #8
    b .Lzero

.Lready:
    adr x0, exception_vectors
    msr vbar_el1, x0
    isb
    bl kernel_main

.Lpark:
    wfe
    b .Lpark

// Sixteen 128-byte vector slots. Any exception is fatal currently.
// No register save is needed because the handler never resumes execution.
.balign 2048
exception_vectors:
.rept 16
    b exception_entry
    .balign 128
.endr

exception_entry:
    msr daifset, #0xf
    ldr x0, =__stack_top
    mov sp, x0
    mrs x0, esr_el1
    mrs x1, elr_el1
    bl exception_fatal
    b .Lpark
