ENTRY(_start)

/* Leave the base of RAM for QEMU's device tree. -kernel loads this ELF. */
MEMORY { RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 128M - 512K }

__stack_size = 64K;
__heap_size = 2M;      /* Matches LockedHeap<512>: 512 * 64 granules * 64 B. */
__page_pool_size = 4M; /* Matches PageAllocator<16>: 1024 pages. */

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

