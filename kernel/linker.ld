ENTRY(_start)

SECTIONS {
    . = 0x100000;

    __kernel_start = .;

    .text : ALIGN(16) {
        *(.text .text.*)
    }
    ASSERT(. >= 0x100000, "ERROR: kernel .text overlaps BootInfo handoff (0x1000) / trampoline (0x8000)")

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16) {
        SUBALIGN(8)
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
        SUBALIGN(8)
        *(.bss .bss.*)
        *(COMMON)
    }

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note*)
    }
}
