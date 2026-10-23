// Common EL1 startup after a board's `_start` has validated its entry state.
// Contract on entry: EL1, one CPU, interrupts masked, x0 = board boot argument.
.section .text.runtime_entry, "ax"
.global __runtime_start
.type __runtime_start, %function
__runtime_start:
    msr daifset, #0xf
    msr spsel, #1                // Use SP_EL1.
    ldr x1, =__stack_top
    mov sp, x1                   // AAPCS64 requires 16-byte stack alignment.

    ldr x1, =__bss_start         // x0 stays live for kernel_main.
    ldr x2, =__bss_end
.Lzero:
    cmp x1, x2
    b.hs .Lready
    str xzr, [x1], #8
    b .Lzero
.Lready:
    adr x1, exception_vectors
    msr vbar_el1, x1
    isb
    bl kernel_main
.global __runtime_park
__runtime_park:
    wfe
    b __runtime_park

// Sixteen 128-byte vector slots. Any exception is fatal in these examples.
// No register save is needed because the handler never resumes execution.
.balign 2048
exception_vectors:
.rept 16
    b exception_entry
    .balign 128
.endr
exception_entry:
    msr daifset, #0xf
    ldr x0, =__stack_top          // Discard the failed call stack.
    mov sp, x0
    mrs x0, esr_el1
    mrs x1, elr_el1
    bl exception_fatal
    b __runtime_park