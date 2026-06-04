# 05 — ELF Loading and Boot Protocol

## Overview

The boot protocol defines how control and data pass between the three boot stages: chainloader, bootloader, and kernel. The protocol is centered on the `BootInfo` struct stored at physical address `0x1000`.

## BootInfo Protocol

### Location

Physical address `0x1000` (page 1) holds an **8-byte pointer** to a
dynamically-allocated `BootInfo` (chainloader `Box::new`s the struct
and writes the pointer at `0x1000`). The kernel reads the pointer,
then dereferences it. This removes the fixed-address constraint on
BootInfo itself (which is ~2 KB) — only the 8-byte pointer occupies
`0x1000`. The address was chosen because:
- It is page-aligned (must be for the kernel to read the pointer
  with a single 8-byte load after the page-table switch)
- It is not page 0 (which would trigger Rust null-pointer UB)
- It is within the first 4 GB (always identity-mapped by the
  kernel's page tables)
- It survives `exit_boot_services` (it is in conventional memory)
- The chainloader reserves the page at `0x1000` so the buddy
  allocator does not hand it out

### Struct Definition (`system/src/lib.rs`)

```rust
#[repr(C)]
pub struct BootInfo {
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS], // 128 free memory descriptors
    pub memory_region_count: usize,         // number of valid entries
    pub framebuffer: FramebufferInfo,       // GOP framebuffer details
    pub partition_zero_lba: u64,            // ext4 partition LBA
    pub partition_zero_size: u64,           // ext4 partition size
    pub kernel_image_addr: u64,             // physical addr of kernel ELF buffer
    pub kernel_image_size: u64,             // size of kernel ELF buffer
    pub rsdp_addr: u64,                     // ACPI RSDP physical address
    pub madt_addr: u64,                     // MADT physical address
    pub exrun_image_addr: u64,              // physical addr of Executive Runtime ELF buffer
    pub exrun_image_size: u64,              // size of ExRun ELF buffer
    pub max_cpus: u32,                      // MAX_CPUS (= 4)
    pub bsp_apic_id: u32,                   // BSP LAPIC ID
    pub ap_count: u32,                      // number of APs (0..MAX_CPUS)
    pub ap_apic_ids: [u32; MAX_CPUS],       // LAPIC ID of each AP
    pub ap_arg_phys: [u64; MAX_CPUS],       // physical addr of each ApArg
}
```

The SMP handoff fields (`max_cpus`, `bsp_apic_id`, `ap_count`,
`ap_apic_ids`, `ap_arg_phys`) are populated by the bootloader
after `StartupThisAP` brings the APs up but before
`exit_boot_services`. The kernel uses them in the BSP release
loop to set each AP's `go = 1`.

### Lifecycle

1. **Chainloader** zeroes the structure, fills in memory regions and framebuffer info, then chains to bootloader
2. **Bootloader** reads it, refines framebuffer, updates memory regions, adds RSDP address, writes it back before `exit_boot_services`
3. **Kernel** reads it at entry, uses all fields to initialize subsystems

## Kernel ELF Specification

### Linker Script (`kernel/linker.ld`)

```
ENTRY(_start)

SECTIONS {
    . = 0x100000;                        // load at 1 MB

    .text : { *(.text .text.*) }         // code
    .rodata : { *(.rodata .rodata.*) }   // read-only data
    .data : { *(.data .data.*) }         // initialized data
    .bss : { *(.bss .bss.*) *(COMMON) }  // zero-initialized data

    /DISCARD/ : {
        *(.eh_frame)                     // exception handling frames (not needed)
        *(.comment)                      // compiler comments
        *(.note*)                        // ELF notes
    }
}
```

All sections are sequential starting at 1 MB. The BSS section covers zero-initialized globals (GDT, IDT, task manager, allocator state).

### Program Headers

Each `PT_LOAD` segment specifies:
- `p_paddr`: target physical address (where the bootloader copies segment data)
- `p_vaddr`: virtual address (same as paddr for static relocation)
- `p_filesz`: size of segment data in the ELF file
- `p_memsz`: size in memory (may be larger than filesz for BSS)
- `p_offset`: offset of segment data within the ELF file

All segments must be within the first 128 MB (`0x800_0000`). This is a safety check in the bootloader's ELF loader.

### Entry Point Convention

The kernel entry point (`_start`) uses the System V AMD64 ABI calling convention:
- `RDI` = `BOOT_INFO_ADDR` (0x1000) — pointer to BootInfo
- RSP must be mod 16 = 8 at entry (simulating the state after a `call` instruction)

The bootloader jumps with:
```asm
sub rsp, 8          ; align stack for SysV ABI (simulate missing call)
mov rdi, BOOT_INFO  ; pass BootInfo address
jmp entry           ; never returns
```

## Bootloader ELF Loader

The ELF loader in `boot/src/load_kernel.rs` performs these steps:

1. **Validate header**: check magic (`0x7F 45 4C 46`), class (64-bit), endianness (little), type (ET_EXEC)
2. **Parse program headers**: iterate `PT_LOAD` segments
3. **Load each segment**: `copy_nonoverlapping` from ELF buffer to `p_paddr`
4. **Clear BSS**: `write_bytes(dst + filesz, 0, memsz - filesz)` for segments where `memsz > filesz`
5. **Return entry point**: the `e_entry` field

## Bootloader Ext4 Parser

The ext4 filesystem reader in `boot/src/load_kernel.rs` is a complete, self-contained implementation. It does not depend on any external ext4 crate.

### Design

**SectorReader** wraps UEFI's BlockIO protocol and handles arbitrary block sizes (512–4096 bytes) via a sector cache.

**ext4 structures parsed**:
- Superblock (at byte offset 1024) — block size, inode count, block count
- Block group descriptor table — bitmap locations, inode table locations
- Inodes (256 bytes each) — file metadata, data block pointers
- Directory entries — file names, inode numbers
- Extent tree — logical-to-physical block mapping

### Extent-Based Reading (Fast Path)

For files with the `EXT4_EXTENTS_FL` flag:
1. Parse the extent header from `i_block[0..15]`
2. Validate extent magic (0xF30A), ensure depth = 0 (leaf extents only)
3. For each extent entry:
   - `ee_block`: first logical block number
   - `ee_len`: number of contiguous blocks
   - `ee_start`: 48-bit physical block number
4. Read contiguous physical blocks directly

### Fallback Block-By-Block (Slow Path)

If extent parsing fails or the file uses indirect blocks:
1. For each logical block, resolve the physical block via:
   - Direct blocks (indices 0–11)
   - Singly indirect block (index 12)
   - (Doubly/triply indirect are not implemented — ext4 rarely uses them for small files)
2. Read each physical block individually

## Kernel Custom Target

The kernel uses a custom target specification (`kernel/target.json`):

```json
{
    "llvm-target": "x86_64-unknown-none",
    "arch": "x86_64",
    "os": "none",
    "executables": true,
    "linker-flavor": "ld.lld",
    "linker": "rust-lld",
    "panic-strategy": "abort",
    "disable-redzone": true,
    "code-model": "kernel",
    "relocation-model": "static",
    "pre-link-args": {
        "ld.lld": ["-Tkernel/linker.ld", "--gc-sections"]
    }
}
```

### Why disable-redzone?

In x86-64, the red zone is 128 bytes below RSP that the compiler can use for temporary data without adjusting RSP. When an interrupt fires, the CPU pushes SS, RSP, RFLAGS, CS, RIP onto the stack, potentially corrupting the red zone. With `disable-redzone: true`, the compiler never uses the red zone, making interrupt entry safe.

### Why code-model=kernel?

The "kernel" code model allows the kernel to be linked at `0x100000` (which is in the lower 2 GB of the address space) while still using absolute addresses for the higher-half mapping (`0xFFFF_8000_0000_0000`). The compiler generates code that can reach both ranges via RIP-relative addressing.

### Why relocation-model=static?

The kernel is loaded at exactly `0x100000` by the bootloader. No relocation processing is needed. Static relocations are resolved at link time.

## Future Protocol Extensions

### Multi-Processor Boot

For SMP support, the BootInfo will need:
- Per-CPU stack pointers
- APIC ID list
- SIPI trampoline location

The boot protocol will be extended without breaking backward compatibility by adding optional fields at the end of the BootInfo struct, using `size` or `version` fields to detect which fields are present.

### Device Tree Blob

On non-ACPI systems (e.g., RISC-V, ARM), the boot protocol should support passing a flattened device tree (FDT) instead of ACPI tables. The BootInfo could gain a `dtb_addr` field alongside `rsdp_addr`.

### Secure Runtime Boot

When Secure Runtime is implemented, the bootloader will need to load SR as a separate binary alongside the kernel. This could be:
- Another entry in the kernel ELF file (as a separate segment)
- A second file loaded from the ext4 partition
- Embedded in the kernel binary and extracted at boot
