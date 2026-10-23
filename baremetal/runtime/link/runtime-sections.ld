/* Shared section layout and the symbol contract used by the runtime.
 *
 * A board script declares ENTRY, a MEMORY region named RAM and these sizes,
 * then INCLUDEs this file:
 *   __stack_size, __heap_size, __page_pool_size
 *
 * Exported: __bss_start/__bss_end, __stack_top, __heap_start/__heap_end,
 * __page_pool_start/__page_pool_end. The board's `.text.boot` comes first so a
 * raw image (e.g. kernel8.img) starts with its entry instruction.
 */
SECTIONS {
    .text : ALIGN(16) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    } > RAM
    .rodata : ALIGN(16) { *(.rodata .rodata.*) } > RAM
    /* Loaders place initialized data directly at its RAM address. */
    .data : ALIGN(16) { *(.data .data.*) *(.got .got.*) } > RAM
    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    } > RAM
    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += __stack_size;
        __stack_top = .;
    } > RAM
    .heap (NOLOAD) : ALIGN(4096) {
        __heap_start = .;
        . += __heap_size;
        __heap_end = .;
    } > RAM
    /* Explicitly reserved usable RAM. Never include the kernel or stack in it. */
    .page_pool (NOLOAD) : ALIGN(4096) {
        __page_pool_start = .;
        . += __page_pool_size;
        __page_pool_end = .;
    } > RAM
    /DISCARD/ : { *(.eh_frame*) *(.comment*) }
}

