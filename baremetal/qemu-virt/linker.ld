ENTRY(_start)

/* Leave the base of RAM for QEMU's device tree. -kernel loads this ELF. */
MEMORY { RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 128M - 512K }

SECTIONS {
    .text : ALIGN(16) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    } > RAM
    .rodata : ALIGN(16) { *(.rodata .rodata.*) } > RAM
    /* QEMU's ELF loader places initialized data directly at its RAM address. */
    .data : ALIGN(16) { *(.data .data.*) *(.got .got.*) } > RAM
    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    } > RAM
    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;
    } > RAM
    .heap (NOLOAD) : ALIGN(4096) {
        __heap_start = .;
        . += 2M;
        __heap_end = .;
    } > RAM
    /* Explicitly reserved usable RAM. Never include the kernel or stack in it. */
    .page_pool (NOLOAD) : ALIGN(4096) {
        __page_pool_start = .;
        . += 4M;
        __page_pool_end = .;
    } > RAM
    /DISCARD/ : { *(.eh_frame*) *(.comment*) }
}

