# LodaxOS Boot Sequence — Complete In-Order Detail

## Table of Contents
1. [Project Structure Overview](#1-project-structure-overview)
2. [Disk Image Layout & Creation](#2-disk-image-layout--creation)
3. [UEFI Firmware Boot](#3-uefi-firmware-boot)
4. [The Chainloader (BOOTX64.EFI)](#4-the-chainloader-bootx64efi)
5. [The Bootloader (Bootloader.efi)](#5-the-bootloader-bootloaderefi)
6. [The Kernel (_start entry)](#6-the-kernel-_start-entry)
7. [Subsystem Reference](#7-subsystem-reference)
8. [Build System & Toolchain](#8-build-system--toolchain)

---

## 1. Project Structure Overview

### 1.1 Workspace Layout

The project is a Rust workspace with four crates (no `shared/` or `sr/` crate):

| Crate | Package Name | Target | Description |
|-------|-------------|--------|-------------|
| `system/` | `lodaxos-system` | Library (host) | Pure type definitions: `BootInfo`, `FramebufferInfo`, `MemoryRegion`, constants |
| `chain/` | `lodaxos-chain` | `x86_64-unknown-uefi` | Stage-1 UEFI chainloader (`EFI/BOOT/BOOTX64.EFI` on ESP) |
| `boot/` | `lodaxos-boot` | `x86_64-unknown-uefi` | Stage-2 UEFI bootloader (`Bootloader.efi` on ext4) |
| `kernel/` | `lodaxos-kernel` | Custom `x86_64-unknown-none` | Bare-metal kernel (`kernel.elf` on ext4) |

### 1.2 Crate Relationship

Each crate has its own `src/` directory — there is no shared root `src/`. The `system` crate provides type definitions consumed by `chain`, `boot`, and `kernel`. There is no re-export pattern; each crate imports `lodaxos-system` as a dependency.

### 1.3 BootInfo (`system/src/lib.rs`)

The `BootInfo` struct is the central communication structure passed from chainloader → bootloader → kernel. It is **dynamically allocated** by the chainloader via `Box::new(BootInfo)`. An 8-byte pointer to the BootInfo is stored at physical address `0x1000` (`BOOT_INFO_HANDOFF_ADDR`).

Key fields:
- `memory_regions: [MemoryRegion; 128]` — up to 128 free memory region descriptors
- `memory_region_count: usize` — number of valid entries
- `framebuffer: FramebufferInfo` — GOP framebuffer details
- `partition_zero_lba: u64` — start LBA of the ext4 partition
- `partition_zero_size: u64` — size in bytes
- `kernel_image_addr: u64` — physical address of loaded kernel ELF
- `kernel_image_size: u64` — size in bytes
- `rsdp_addr: u64` — ACPI RSDP physical address
- `madt_addr: u64` — MADT physical address (discovered by kernel from RSDP)
- `max_cpus: u32`, `bsp_apic_id: u32`, `ap_count: u32`, `ap_apic_ids: [u32; MAX_CPUS]` — SMP topology

---

## 2. Disk Image Layout & Creation

### 2.1 Physical Disk Layout

The disk image is 600 MB with GPT partitioning:

- **Protective MBR** at LBA 0 (partition type 0xEE)
- **GPT header** at LBA 1
- **Partition entry array** at LBA 2 (128 entries × 128 bytes)
- **Partition 0 (ext4)**: 512 MB, LBA 2048–1,050,623
- **Partition 1 (ESP FAT32)**: 64 MB, LBA 1,050,624–1,181,695
- **Backup GPT** at end of disk

### 2.2 Partition Contents

**Partition 0 — ext4** (Linux filesystem GUID `0FC63DAF-...`):
- `kernel.elf` (~3.9 MB)
- `Bootloader.efi` (~493 KB)

Created via `mke2fs -d` from a staging directory (no loop device needed).

**Partition 1 — ESP FAT32** (EFI System GUID `C12A7328-...`):
- `EFI/BOOT/BOOTX64.EFI` (~386 KB)

Created via Python minimal FAT32 implementation.

---

## 3. UEFI Firmware Boot

### 3.1 OVMF Initialization

QEMU loads OVMF (EDK II UEFI firmware), which scans the GPT disk, finds the ESP, and loads `\EFI\BOOT\BOOTX64.EFI` (the chainloader).

State at chainloader entry:
- CPU in 64-bit long mode with UEFI page tables (identity-mapped)
- UEFI boot services active (can call `AllocatePool`, `LocateProtocol`, etc.)
- Interrupts enabled (UEFI timer active)
- 8259 PIC active with UEFI's vector mappings
- GOP framebuffer initialized
- ACPI tables accessible via UEFI config table (RSDP available)
- RSP on UEFI boot services stack (will vanish after `exit_boot_services`)

---

## 4. The Chainloader (BOOTX64.EFI)

**Source:** `chain/src/main.rs`

### 4.1 Entry Flow

1. **Initialize UEFI helpers** — `uefi::helpers::init()`
2. **Initialize serial** — COM1 at 115200 8N1 (inline init, raw I/O port writes)
3. **Allocate BootInfo** dynamically via `Box::new(BootInfo)`, store physical pointer at `0x1000` (`BOOT_INFO_HANDOFF_ADDR`)
4. **Collect UEFI memory map** — iterate entries, store free regions (`CONVENTIONAL`, `LOADER_CODE`, `LOADER_DATA`) into `BootInfo.memory_regions`
5. **Capture framebuffer** — open GOP protocol, get physical address, resolution, stride, pixel format
6. **Read Bootloader.efi** from ESP root via UEFI `FileProtocol`
7. **Chainload** via `load_image` + `start_image` — transfers control to Bootloader.efi

---

## 5. The Bootloader (Bootloader.efi)

**Source:** `boot/src/main.rs`, `boot/src/load_kernel.rs`

### 5.1 Entry Flow

1. **Initialize subsystems** — UEFI helpers, serial, logger
2. **Read BootInfo** from `BOOT_INFO_HANDOFF_ADDR` via the 8-byte pointer
3. **Set GOP video mode** — explicitly set the first available mode for known-good state
4. **Re-collect UEFI memory map** — the map may have changed since chainloader ran
5. **Load kernel.elf from ext4** via `load_kernel::load_kernel_from_ext4()`:
   - Parse GPT to find ext4 partition via UEFI Block I/O
   - Read ext4 superblock, block group descriptors, inode tables, root directory
   - Walk ext4 directory entries to find `kernel.elf`
   - Read file via extents (depth-0 only, leaf extents)
   - Allocate `Vec<u8>` with file data
6. **Enumerate CPUs** via UEFI MP Services — get total/enabled count, per-CPU LAPIC IDs, determine BSP vs AP — **enumeration only, no AP startup**
8. **Capture RSDP** from UEFI config table (MUST be done before `exit_boot_services`)
9. **Write BootInfo** back through the dynamic pointer
10. **Exit boot services** — `exit_boot_services(None)` — point of no return
11. **Disable interrupts** — `cli`
12. **Load kernel ELF** — parse ELF64 program headers, copy `PT_LOAD` segments to target physical addresses (entry at `_start`)
13. **Jump to kernel** — `sub rsp, 8; mov rdi, boot_info_phys; jmp _start` (SysV ABI: RDI = BootInfo pointer)

### 5.2 Ext4 Parsing Details

The ext4 reader is self-contained with no external dependencies:

- **Superblock**: at byte offset 1024 from partition start, magic `0xEF53`
- **Block group descriptors**: 32-byte entries, number of groups = `s_blocks_count_hi / s_blocks_per_group`
- **Inode table**: read inode 2 (root directory), parse `i_block[]` for extent tree
- **Extents**: magic `0xF30A`, depth 0 only (leaf entries), each entry = `ee_block` + `ee_len` + `ee_start`
- **Block I/O**: `SectorReader` wrapping UEFI `BlockIO` protocol, handles arbitrary sector sizes (512–4096)

---

## 6. The Kernel (_start entry)

**Source:** `kernel/src/main.rs`, `kernel/src/ap_start.rs`, `kernel/src/arch/smp.rs`

### 6.1 Linker Script

The kernel is linked at `0x100000` (1 MB) with contiguous `.text`, `.rodata`, `.data`, `.bss` sections. `eh_frame`, `comment`, and `note` sections are discarded.

### 6.2 Target Specification

Custom `x86_64-unknown-none` target:
- `disable-redzone: true` — critical for interrupt safety
- `code-model: "kernel"` — allows static addressing at 1 MB with higher-half mapping
- `relocation-model: "static"` — no relocations

### 6.3 Kernel Entry Point

```rust
#[unsafe(no_mangle)]
extern "C" fn _start(boot_info: *const BootInfo) -> !
```

Called with RDI = BootInfo physical pointer. The function never returns.

### 6.4 Init Sequence

```
Phase 0:  Serial → Logger
Phase 1A: Extract memory regions from BootInfo
Phase 1B: Framebuffer init (clear, splash text)
Phase 1C: Physical allocator init (buddy: init_from_regions)
Phase 1D: ACPI init → MADT parse → IOAPIC/LAPIC/CPU info
Phase 1E: Page tables init (virt::init: PML4, identity + higher-half, CR3 switch)
Phase 1F: Heap init (slab allocator)
Phase 1G: VMA tree init (kernel heap demand-paging)
Phase 2A: cli + mask PIC (8259 fully masked)
Phase 2B: LAPIC MMIO map
Phase 2C: IOAPIC init + interrupt routing table
Phase 2D: Reserve AP pages (SIPI trampoline at 0x8000)
Phase 2E: Load SIPI trampoline to 0x8000 (arch::smp::init)
Phase 2F: Re-map framebuffer pointer to higher-half
Phase 3A: GDT load (kernel code/data, user code/data, TSS)
Phase 3B: IDT init (256 vectors, IST1 for #DF)
Phase 3C: Per-CPU BSP init (mark online, install GS base)
Phase 3D: Task init + init_idle_task (BSP idle task)
Phase 3E: Create test tasks (simple_task1, simple_task2)
Phase 3F: Install IOAPIC routes (all masked)
Phase 3H: Enable LAPIC (SVR, mask LINT0/LINT1, TPR)
Phase 3I: Calibrate LAPIC timer vs PIT (20 ms measurement)
Phase 3J: Configure LAPIC timer (1 ms periodic, vector 32)
Phase 3K: Enable PIT periodic (100 Hz, ISA IRQ 0)
Phase 3L: SMP boot (arch::smp::smp_boot_aps):
           INIT → 10 ms → SIPI → 200 µs → SIPI → poll mailboxes
Phase 3M: release_all_aps (kernel_ready=true for all CPUs)
Phase 3N: sti + int 32 test
Phase 3O: Unmask PIT + keyboard IOAPIC routes
Phase 4:  Idle loop (hlt + periodic stats logging)
```

### 6.5 AP Boot Details (INIT-SIPI-SIPI)

APs are started by the BSP kernel after all core subsystems are initialized:

1. **SIPI trampoline init**: A pre-compiled machine code array is copied to physical address `0x8000` (SIPI vector 0x08). The trampoline performs: real-mode entry → A20 gate → protected mode (PAE) → long mode → reads mailbox data at `0x8400+` → loads kernel GDT/IDT → switches stack → jumps to `ap_entry()`.

2. **Mailbox slots at `0x8400+`**: Within the 4 KB trampoline page, offset 0x400 (`MAILBOX_OFF`) holds per-AP data in slot-based layout:
   - PML4 physical address (8 B at slot offset `0x78`)
   - GDT pointer base+limit (10 B at slot offset `0x50`)
   - IDT pointer base+limit (10 B at slot offset `0x60`)
   - Kernel stack top (8 B at slot offset `0x40`)
   - ap_entry address (8 B at slot offset `0x68`)
   - CPU status byte (1 B at slot offset `0x70`)

3. **INIT-SIPI-SIPI sequence** (`arch::smp::smp_boot_aps`):
   - Broadcast INIT IPI to all APs via LAPIC ICR (destination shorthand = all-excluding-self)
   - Wait ~10 ms (pause-based busy-wait loop)
   - Broadcast SIPI (vector 0x08) to all APs
   - Wait ~1 ms (pause-based busy-wait, NOT PIT Mode 0)
   - Broadcast second SIPI
   - Poll each AP's status byte until ready or timeout

4. **ap_entry()** (`kernel/src/ap_start.rs`):
   - Raw COM1 diagnostic ("AP ENTRY REACHED")
   - Read LAPIC ID from MMIO
   - FPU/SSE init (fninit, CR4.OSFXSR/OSXMMEXCPT/OSXSAVE)
   - mark_online() → per-CPU slot
   - install_gs_base() → GS base + TSC_AUX MSRs
   - Enable LAPIC timer (1 ms, vector 32)
   - wait_for_kernel_ready() → spin on flag set by BSP
   - init_idle_task() → per-CPU idle task
   - ltr (per-CPU TSS)
   - sti
   - ap_sched_loop() → pause-based spin, periodic work stealing

---

## 7. Subsystem Reference

The kernel subsystem implementations live under `kernel/src/`:

| Path | Component |
|------|-----------|
| `kernel/src/serial.rs` | 16550 UART driver |
| `kernel/src/logger.rs` | `log` crate wrapper |
| `kernel/src/font.rs` | 8×16 bitmap font |
| `kernel/src/task.rs` | Preemptive multitasking scheduler |
| `kernel/src/arch/gdt.rs` | GDT, TSS, segment loading |
| `kernel/src/arch/idt.rs` | IDT (256 vectors), exception/IRQ/syscall handlers |
| `kernel/src/arch/apic.rs` | Local APIC MMIO driver, timer, IPI |
| `kernel/src/arch/ioapic.rs` | I/O APIC driver |
| `kernel/src/arch/smp.rs` | SMP boot (INIT-SIPI-SIPI, mailbox protocol) |
| `kernel/src/ap_start.rs` | ap_entry(), SIPI trampoline bytecode |
| `kernel/src/percpu.rs` | PerCpu state, ready queue, GS base |
| `kernel/src/mm/phys.rs` | Buddy physical allocator |
| `kernel/src/mm/virt.rs` | 4-level page tables, higher-half mapping |
| `kernel/src/mm/heap.rs` | SLUB-style slab allocator |
| `kernel/src/mm/vma.rs` | Radix tree VMA tracker, demand paging |
| `kernel/src/acpi/mod.rs` | ACPI RSDP/XSDT/MADT parsing |
| `kernel/src/intr/mod.rs` | Interrupt routing table, vector allocation |
| `kernel/src/main.rs` | _start entry, full init sequence |

See `docs/architecture/` for detailed subsystem documentation.

---

## 8. Build System & Toolchain

### 8.1 Build Sequence

```bat
cargo +nightly build -p lodaxos-system
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zbuild-std=core,alloc
```

Output artifacts:
- `target/x86_64-unknown-uefi/debug/lodaxos-boot.efi` → `Bootloader.efi`
- `target/x86_64-unknown-uefi/debug/lodaxos-chain.efi`
- `target/kernel/debug/lodaxos-kernel` → `kernel.elf`

### 8.2 Disk Image Creation

`create_disk_image.py` assembles the 600 MB GPT image:
1. Creates empty disk image (`dd`)
2. Writes GPT header + partition table
3. Creates ext4 partition via `mke2fs -d` with kernel.elf, Bootloader.efi
4. Creates FAT32 ESP via Python minimal FAT writer with BOOTX64.EFI

### 8.3 QEMU Launch

```bat
qemu-system-x86_64.exe ^
    -drive if=pflash,format=raw,readonly=on,file="edk2-x86_64-code.fd" ^
    -drive file=disk.img,format=raw,if=ide ^
    -serial stdio ^
    -accel whpx ^
    -m 512M ^
    -smp 4
```
