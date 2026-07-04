ENTRY(_start)

SECTIONS {
    . = 0x20000000;

    .text : { *(.text .text.*) }
    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(0x1000);
    .data : SUBALIGN(8) { *(.data .data.*) }
    . = ALIGN(0x1000);
    .bss : SUBALIGN(8) { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : { *(.eh_frame) *(.comment) *(.note*) }
}
