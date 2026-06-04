ENTRY(_start)

SECTIONS {
    /* Place ExRun in a region of the kernel's higher half that is NOT
     * covered by the existing higher-half map (which uses 2 MB huge
     * pages for the first 1 GB at HIGHER_HALF). PML4[0x1FF] covers the
     * full 512 GB of the higher half; PDP[0] is the existing 1 GB map
     * (physical 0..1 GB), and PDP[1..511] are unmapped. We pick
     * HIGHER_HALF + 1 GB (= 0xFFFF_8000_4000_0000) so ExRun's segments
     * land in a fresh PDP slot — no huge-page conflict in the forked
     * PML4, so the ELF loader can map PT_LOAD segments as 4 KB pages
     * without splitting existing huge pages. */
    . = 0xFFFF800040000000;

    .text : {
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note*)
    }
}
