# LodaxOS Boot Sequence — Complete In-Order Detail

## Table of Contents
1. [Project Structure Overview](#1-project-structure-overview)
2. [Disk Image Layout & Creation](#2-disk-image-layout--creation)
3. [UEFI Firmware Boot](#3-uefi-firmware-boot)
4. [The Chainloader (BOOTX64.EFI)](#4-the-chainloader-bootx64efi)
5. [The Bootloader (Bootloader.efi)](#5-the-bootloader-bootloaderefi)
6. [The Kernel (_start entry)](#6-the-kernel-_start-entry)
7. [Subsystem Reference](#7-subsystem-reference)
    - [7.1 Serial Port and Logger](#71-serial-port-and-logger)
    - [7.2 Physical Memory Allocator — Buddy System](#72-physical-memory-allocator--buddy-system)
    - [7.3 Virtual Memory Page Tables](#73-virtual-memory-page-tables)
    - [7.4 Slab Heap Allocator](#74-slab-heap-allocator)
    - [7.5 VMA and Demand Paging](#75-vma-and-demand-paging)
    - [7.6 ACPI Sub-system](#76-acpi-sub-system)
    - [7.7 Local APIC Driver](#77-local-apic-driver)
    - [7.8 I/O APIC Driver](#78-io-apic-driver)
    - [7.9 Interrupt Routing](#79-interrupt-routing)
    - [7.10 GDT and TSS](#710-gdt-and-tss)
    - [7.11 IDT and Interrupt Handling](#711-idt-and-interrupt-handling)
    - [7.12 Task Manager and Preemptive Scheduling](#712-task-manager-and-preemptive-scheduling)
    - [7.13 Framebuffer and Font](#713-framebuffer-and-font)
8. [Build System & Toolchain](#8-build-system--toolchain)
9. [Appendix: Key Data Structures](#9-appendix-key-data-structures)

---

## 1. Project Structure Overview

### 1.1 Workspace Layout (`Cargo.toml`)

The entire project is a Rust workspace with five crates, defined at `Cargo.toml:2`:
- `system/` — `lodaxos-system`: pure type definitions with zero dependencies. Houses `BootInfo`, `FramebufferInfo`, `MemoryRegion`, constants like `BOOT_INFO_HANDOFF_ADDR` and `MAX_MEMORY_REGIONS`
- `shared/` — `lodaxos-core`: shared implementations of all kernel subsystems (serial, logger, arch, mm, acpi, intr, font, task). These are re-export wrappers around the canonical source in `src/`
- `chain/` — `lodaxos-chain`: the first-stage UEFI chainloader. Compiled to `x86_64-unknown-uefi`, placed as `EFI/BOOT/BOOTX64.EFI` on the ESP
- `boot/` — `lodaxos-boot`: the second-stage UEFI bootloader. Compiled to `x86_64-unknown-uefi`, stored as `Bootloader.efi` on the ext4 partition
- `kernel/` — `lodaxos-kernel`: the bare-metal kernel. Compiled to `x86_64-unknown-none` (custom target `kernel/target.json`), stored as `kernel.elf` on ext4

### 1.2 The `src/` Canonical Source Tree

The actual implementation code lives in `src/` at the workspace root. The `shared/` crate re-exports everything from `src/` via `#[path]` attributes, `include!` macros, and `pub use` re-exports. This avoids code duplication while keeping the kernel and bootloader using the same subsystems.

- `src/serial.rs` — 16550 UART driver
- `src/logger.rs` — `log` crate wrapper for serial output
- `src/font.rs` — 8×16 bitmap font
- `src/task.rs` — preemptive multitasking scheduler
- `src/arch/mod.rs` — declares `apic`, `gdt`, `idt`, `ioapic` submodules
- `src/arch/gdt.rs` — GDT, TSS, segment loading
- `src/arch/idt.rs` — IDT (256 vectors), exception handlers, IRQ dispatcher
- `src/arch/apic.rs` — Local APIC MMIO driver, timer calibration
- `src/arch/ioapic.rs` — I/O APIC driver, redirection entry programming
- `src/mm/mod.rs` — declares `phys`, `virt`, `heap`, `vma` submodules + global allocator
- `src/mm/phys.rs` — buddy-based physical page allocator (orders 0–10, split/coalesce)
- `src/mm/virt.rs` — 4-level page table manager, higher-half mapping
- `src/mm/heap.rs` — SLUB-style slab heap allocator (9 caches, 32 B–8 KB)
- `src/mm/vma.rs` — radix tree VMA tracker + demand paging page fault handler
- `src/acpi/mod.rs` — ACPI RSDP discovery, XSDT/RSDT parsing
- `src/acpi/madt.rs` — MADT (Multiple APIC Description Table) parser
- `src/intr/mod.rs` — Interrupt routing table, vector allocation, IOAPIC route management

### 1.3 Key Types (`system/src/lib.rs`)

The `BootInfo` struct at `system/src/lib.rs:13` is the central communication structure passed from chainloader → bootloader → kernel. It is **dynamically allocated** by the chainloader via `Box::new(BootInfo)`. An 8-byte pointer to the BootInfo struct is stored at physical address `0x1000` (defined as `BOOT_INFO_HANDOFF_ADDR` at `system/src/lib.rs:10`).

Fields:
- `memory_regions: [MemoryRegion; 128]` — up to 128 free memory region descriptors
- `memory_region_count: usize` — number of valid entries
- `framebuffer: FramebufferInfo` — GOP framebuffer details (phys_addr, width, height, stride, bytes_per_pixel, is_bgr)
- `partition_zero_lba: u64` — start LBA of the ext4 partition
- `partition_zero_size: u64` — size in bytes
- `kernel_image_addr: u64` — physical address of loaded kernel ELF
- `kernel_image_size: u64` — size in bytes
- `rsdp_addr: u64` — ACPI RSDP physical address (captured before exit_boot_services)
- `madt_addr: u64` — MADT physical address (may be discovered by kernel from RSDP)

---

## 2. Disk Image Layout & Creation

### 2.1 Physical Disk Layout (`create_disk_image.py`)

The `create_disk_image.py` script at the workspace root creates a 600 MB raw disk image with GPT partitioning. Details at `create_disk_image.py:95-100`:

- Total size: 600 MB (1,228,800 sectors of 512 bytes)
- Partition 0 (ext4 "Partition Zero"): 512 MB, LBA 2048–1,050,623
- Partition 1 (ESP, FAT32): 64 MB, LBA 1,050,624–1,181,695

### 2.2 GPT Header (`create_disk_image.py:44-81`)

Written by `write_gpt()`:
- Protective MBR at LBA 0 with partition type `0xEE` covering the entire disk
- GPT header at LBA 1 with signature `"EFI PART"`, revision 1.0, header size 92 bytes
- Partition entries at LBA 2 (128 entries, each 128 bytes)
- Backup GPT at the end of the disk
- Ext4 partition GUID: `0FC63DAF-8483-4772-8E79-3D69D8477DE4`
- ESP partition GUID: `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`

### 2.3 Partition Contents

**Partition 0 (ext4):**
- `kernel.elf` (compiled kernel binary, ~3.9 MB)
- `Bootloader.efi` (second-stage UEFI bootloader, ~493 KB)

Created via `mke2fs -d /tmp/lodaxos_staging -L LodaxOS` which populates the ext4 filesystem from a staging directory without requiring loop device mounting. This is critical because WSL2 does not support loop devices.

**Partition 1 (ESP, FAT32):**
- `EFI/BOOT/BOOTX64.EFI` (the chainloader, ~386 KB)
- _Note: For legacy compatibility, the ESP root also contains `Bootloader.efi` and `kernel.elf`_

Created via a Python-based minimal FAT32 implementation (`create_minimal_fat32` at `create_disk_image.py` — fallback when mtools is unavailable). This function manually constructs:
- A FAT32 BIOS Parameter Block (BPB)
- An FSInfo sector
- Two FATs (File Allocation Tables)
- A root directory cluster with the `BOOTX64.EFI` directory entry
- Data clusters containing the chainloader binary

### 2.4 Startup Script (`esp/startup.nsh`)

A minimal UEFI shell script at `esp/startup.nsh:1-2`:
```
FS0:
EFI\BOOT\BOOTX64.EFI
```
This tells the UEFI firmware to mount the first filesystem and execute the chainloader.

---

## 3. UEFI Firmware Boot

### 3.1 OVMF (UEFI) Initialization

When QEMU starts via `run.bat:2`:
```powershell
"C:\Program Files\qemu\qemu-system-x86_64.exe" -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" -drive file="disk.img",format=raw,if=ide -serial stdio -accel whpx -m 512M -smp 2
```

- `-drive if=pflash,...edk2-x86_64-code.fd` loads OVMF (EDK II UEFI firmware) as the firmware
- `-drive file=disk.img,if=ide` presents the disk image as an IDE drive
- `-serial stdio` redirects COM1 to the terminal for debug output
- `-accel whpx` uses Windows Hypervisor Platform for hardware acceleration
- `-m 512M` provides 512 MB of RAM
- `-smp 2` creates a 2-CPU symmetric multiprocessing topology

OVMF scans the disk for a GPT header, finds the ESP partition (Type `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`, FAT32), and attempts to boot `\EFI\BOOT\BOOTX64.EFI` per the UEFI specification's fallback boot path. OVMF then loads this PE32+ image and transfers control to its entry point.

### 3.2 Firmware-to-Bootloader Handoff State

When the chainloader starts, the system is in this state:
- CPU is in 64-bit long mode (protected mode with paging enabled)
- UEFI page tables are active (identity-mapping all physical memory)
- UEFI boot services are still active (can call `AllocatePool`, `LocateProtocol`, etc.)
- Interrupts are enabled (UEFI timer interrupts may be firing)
- The 8259 PIC is active with UEFI's vector mappings
- The framebuffer is in whatever mode GOP was initialized to
- COM1 serial port may or may not be initialized
- RSP is on the UEFI boot services stack (which will vanish after `exit_boot_services`)
- ACPI tables are accessible via the UEFI configuration table (RSDP pointer available)

---

## 4. The Chainloader (BOOTX64.EFI)

### 4.1 Entry Point (`chain/src/main.rs:19-126`)

The chainloader is a standard UEFI application with `#[entry] fn main() -> Status`.

**Step 1: Initialize UEFI helpers** (`chain/src/main.rs:21`)
```rust
uefi::helpers::init().unwrap();
```
This sets up the UEFI Rust crate's internal state (memory allocation, protocol access, etc.).

**Step 2: Initialize serial port** (`chain/src/main.rs:147-157`)
```rust
fn serial_init() {
    // COM1 at 0x3F8: 115200 baud, 8N1, FIFO enabled
    // DLAB=1 → set divisor (1 = 115200)
    // DLAB=0 → 8N1, enable FIFO, assert DTR/RTS
}
```
The chainloader has its own inline serial init because it imports `serial` differently from the shared crate. It uses raw `out dx, al` instructions to program the 16550 UART:
- `0x3F8` (data), `0x3F9` (IER), `0x3FA` (FCR), `0x3FB` (LCR), `0x3FC` (MCR)

**Step 3: Dynamically allocate BootInfo** (`chain/src/main.rs:27-52`)
```rust
let boot_info = alloc::boxed::Box::<BootInfo>::new(lodaxos_system::BootInfo { ... zeroed ... });
let boot_info_ptr = alloc::boxed::Box::into_raw(boot_info) as *mut BootInfo;
let boot_info_phys = boot_info_ptr as u64;
unsafe {
    *(BOOT_INFO_HANDOFF_ADDR as *mut u64) = boot_info_phys;
}
```
Instead of writing the BootInfo struct directly at a fixed address, the chainloader allocates BootInfo dynamically via `Box::new()` (backed by UEFI's `AllocatePool`, which identity-maps the address). The physical pointer is then stored as a single 8-byte value at `BOOT_INFO_HANDOFF_ADDR` (`0x1000`). This removes the fixed-address constraint on BootInfo (which is ~2 KB) — the struct lives wherever the allocator chooses, and only the 8-byte pointer occupies `0x1000`.

**Step 4: Collect memory map** (`chain/src/main.rs:49-66`)
```rust
if let Ok(memory_map) = uefi::boot::memory_map(MemoryType::LOADER_DATA) {
    for entry in memory_map.entries() {
        let is_free = matches!(entry.ty,
            MemoryType::CONVENTIONAL | MemoryType::LOADER_CODE | MemoryType::LOADER_DATA
        );
        if is_free && entry.page_count > 0 && count < MAX_MEMORY_REGIONS {
            boot_info.memory_regions[count] = MemoryRegion {
                phys_start: entry.phys_start,
                size: entry.page_count * 4096,
            };
            count += 1;
        }
    }
}
```
UEFI provides a memory map with entry types. Only `CONVENTIONAL`, `LOADER_CODE`, and `LOADER_DATA` regions are considered free/usable. Each entry's `page_count` (in 4 KB pages) is multiplied by 4096 to get the size in bytes. Up to 128 regions are stored.

**Step 5: Collect framebuffer info** (`chain/src/main.rs:69-89`)
```rust
if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
    let mode = gop.current_mode_info();
    let (w, h) = mode.resolution();
    let stride = mode.stride();
    let is_bgr = matches!(mode.pixel_format(), PixelFormat::Bgr);
    let mut fb = gop.frame_buffer();
    boot_info.framebuffer = FramebufferInfo {
        phys_addr: fb.as_mut_ptr() as u64,
        width: w, height: h, stride,
        bytes_per_pixel: 4, is_bgr,
    };
}
```
The chainloader captures the current GOP mode's framebuffer. Note that only physical address, resolution, stride, and pixel format are stored. The kernel will later map this into its own page tables. The `is_bgr` flag is critical for correct pixel rendering since UEFI can use either RGB or BGR pixel layouts.

**Step 6: Read Bootloader.efi from ESP root** (`chain/src/main.rs:93-102`)
```rust
let bootloader_bytes = match read_file_from_image_root(cstr16!("Bootloader.efi")) {
    Some(data) => data,
    None => { return Status::LOAD_ERROR; }
};
```
Uses the UEFI `FileProtocol` to read `Bootloader.efi` from the ESP root directory. The `read_file_from_image_root` function (`chain/src/main.rs:128-143`) opens the filesystem from the image handle, opens the volume, opens the file, gets its info to determine size, allocates a buffer, and reads.

**Step 7: Load and start the bootloader** (`chain/src/main.rs:107-125`)
```rust
let source = uefi::boot::LoadImageSource::FromBuffer {
    buffer: &bootloader_bytes,
    file_path: None,
};
let image_handle = uefi::boot::load_image(parent_handle, source)?;
uefi::boot::start_image(image_handle)?;
```
This is the critical chainloading step. The chainloader:
1. Calls `load_image` with `LoadImageSource::FromBuffer`, passing the raw bytes of `Bootloader.efi`
2. This causes UEFI to validate the PE32+ format, relocate if needed, and allocate resources
3. Returns an `image_handle` representing the loaded image
4. Calls `start_image` which transfers control to the bootloader's entry point

The bootloader (Bootloader.efi) now runs as a child UEFI image. The BootInfo at `0x1000` is accessible because UEFI's identity mapping is still active.

### 4.2 Chainloader Panic Handler (`chain/src/main.rs:159-179`)

If the chainloader panics (e.g., file not found, protocol unavailable):
```rust
fn panic(_info: &core::panic::PanicInfo) -> ! {
    for b in b"PANIC: " {
        // Poll LSR bit 5 (THR empty) with 100K retry timeout
        // Write byte to THR
    }
    loop { unsafe { core::arch::asm!("cli; hlt") }; }
}
```
Writes `"PANIC: "` to serial with timeout, then halts.

---

## 5. The Bootloader (Bootloader.efi)

### 5.1 Entry Point (`boot/src/main.rs:22-153`)

The bootloader is also a `#[entry] fn main() -> Status` UEFI application.

**Step 1: Initialize subsystems** (`boot/src/main.rs:24-28`)
```rust
uefi::helpers::init().unwrap();
serial::init();
logger::init().unwrap_or(());
log::info!("LodaxOS bootloader starting");
```
Initializes the UEFI crate, the 16550 serial port (via `lodaxos_core::serial`), and the serial logger (via `lodaxos_core::logger`). The logging system uses `log::set_logger` with `LevelFilter::Trace`.

**Step 2: Read BootInfo** (`boot/src/main.rs:31-34`)
```rust
let boot_info_addr = unsafe { *(BOOT_INFO_HANDOFF_ADDR as *const u64) };
let boot_info_ptr = boot_info_addr as *mut BootInfo;
let mut boot_info = unsafe { *boot_info_ptr };
```
Reads the BootInfo pointer from the handoff address (`0x1000`), then dereferences it to get the BootInfo struct. The BootInfo may be at any physical address — only the pointer lives at `0x1000`.

**Step 3: Refine framebuffer via GOP** (`boot/src/main.rs:35-60`)
```rust
if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
    if let Some(mode) = gop.modes().next() {
        let _ = gop.set_mode(&mode);
    }
    // ... read resolution, stride, pixel format
    // ... write to boot_info.framebuffer
}
```
The bootloader opens GOP and explicitly sets the first available video mode (which may differ from the chainloader's mode). This ensures a known-good mode. The framebuffer physical address, resolution, stride, and pixel format are written to BootInfo.

**Step 4: Collect UEFI memory map** (`boot/src/main.rs:63-81`)
```rust
let memory_map_result = uefi::boot::memory_map(MemoryType::LOADER_DATA);
for entry in memory_map.entries() {
    let is_free = matches!(entry.ty,
        MemoryType::CONVENTIONAL | MemoryType::LOADER_CODE | MemoryType::LOADER_DATA
    );
    if is_free && entry.page_count > 0 && region_count < MAX_MEMORY_REGIONS {
        boot_info.memory_regions[region_count] = MemoryRegion {
            phys_start: entry.phys_start,
            size: entry.page_count * 4096,
        };
        region_count += 1;
    }
}
boot_info.memory_region_count = region_count;
```
The bootloader re-collects the UEFI memory map. This is necessary because the chainloader's memory map may have changed (e.g., the `load_image`/`start_image` calls allocated new regions for the bootloader itself). Free regions are filtered the same way as in the chainloader.

**Step 5: Load kernel.elf from ext4 partition** (`boot/src/main.rs:84-96`)
```rust
let kernel_elf_data = match load_kernel::load_kernel_from_ext4() {
    Some(data) => data,
    None => { return Status::LOAD_ERROR; }
};
boot_info.kernel_image_addr = kernel_elf_data.as_ptr() as u64;
boot_info.kernel_image_size = kernel_elf_data.len() as u64;
```
This is the most complex operation in the bootloader. The `load_kernel_from_ext4()` function at `boot/src/load_kernel.rs:671-823` performs full ext4 filesystem traversal over raw UEFI Block I/O. It does NOT use the `ext4-view` crate (which is declared as a dependency but unused in the bootloader — it is used only in the chainloader for reference).

**Step 5a: Find ext4 partition via GPT** (`boot/src/load_kernel.rs:306-360`)

`find_ext4_partition()`:
1. Gets the first Block I/O handle via `boot::get_handle_for_protocol::<BlockIO>`
2. Reads LBA 0 (protective MBR) into a 512-byte buffer
3. Reads LBA 1 (GPT header) and checks for `"EFI PART"` signature
4. Parses the GPT header fields at known byte offsets:
   - Offset 72: partition entry array LBA (8 bytes)
   - Offset 80: number of partition entries (4 bytes)
5. Iterates GPT partition entries (128 bytes each, up to 8 sectors or 128 entries)
6. For each entry, extracts the partition type GUID (bytes 0–15) and compares against `EXT4_GUID` (`0FC63DAF-8483-4772-8E79-3D69D8477DE4`)
7. When found, extracts `first_lba` (bytes 32–39) and `last_lba` (bytes 40–47)

**Step 5b: Create SectorReader** (`boot/src/load_kernel.rs:238-278`)

`SectorReader::new(handle, partition_offset)`:
- Opens the BlockIO protocol exclusively
- Reads the media's block size (may be larger than 512, e.g., 4096 for advanced format drives)
- Caches the last sector read to avoid redundant I/O

The `read_sector` method handles arbitrary block sizes:
- If `block_size ≥ 512`: reads one block, extracts the relevant 512-byte portion
- If `block_size < 512`: reads multiple blocks, extracts the 512-byte portion spanning them

**Step 5c: Read ext4 superblock** (`boot/src/load_kernel.rs:678-713`)

The ext4 superblock is at byte offset 1024 from the partition start. The code:
1. Reads sector containing byte 1024
2. Parses every field manually from raw bytes (no packed struct copy — uses `u32::from_le_bytes` etc.)
3. Validates the ext4 magic number (`0xEF53`)
4. Calculates `block_size = 1024 << s_log_block_size`

**Step 5d: Read block group descriptor table** (`boot/src/load_kernel.rs:719-771`)

The block group descriptor table starts at:
- Block 2 if `s_log_block_size == 0` (1 KB blocks)
- Block 1 if `s_log_block_size > 0` (larger blocks)

Each `BlockGroupDesc` is 32 bytes. The number of block groups is calculated as:
```rust
let num_groups = (s_blocks_count_lo + s_blocks_per_group - 1) / s_blocks_per_group;
```

Each descriptor contains:
- `bg_block_bitmap_lo` — block number of the block bitmap
- `bg_inode_bitmap_lo` — block number of the inode bitmap
- `bg_inode_table_lo` — block number of the inode table
- Various counts and checksums

**Step 5e: Read root inode** (`boot/src/load_kernel.rs:774-775`)

The root directory is always inode 2. `read_inode()` at `load_kernel.rs:407-461`:
1. Calculates the block group and index: `group = (inode_num - 1) / inodes_per_group`, `index = (inode_num - 1) % inodes_per_group`
2. Gets the block group descriptor for that group
3. Calculates the sector of the inode table entry: `sector = bg_inode_table_lo * block_size / 512 + (index * 256) / 512`
4. Reads 128 bytes from the inode structure (ext4 standard inode size is 256 bytes, but only the first 128 are used for standard fields)
5. Parses: `i_mode`, `i_uid`, `i_size_lo`, `i_size_high`, `i_flags`, `i_block[15]`, etc.

**Step 5f: Scan root directory for kernel.elf** (`boot/src/load_kernel.rs:780-814`)

The root directory data (size from `i_size_lo`) is read into a buffer. Directory entries have the format:
```rust
struct DirEntry {
    inode: u32,       // offset 0
    rec_len: u16,     // offset 4
    name_len: u8,     // offset 6
    file_type: u8,    // offset 7
    // name follows at offset 8
}
```
The code walks through entries by adding `rec_len` until it finds one with name `b"kernel.elf"`.

**Step 5g: Read kernel file** (`boot/src/load_kernel.rs:817-822`)

Reads the kernel inode, then calls `read_file()` which:
1. Gets the file size from `(i_size_high << 32) | i_size_lo`
2. Allocates a `Vec<u8>` of that size
3. Checks `i_flags & EXT4_EXTENTS_FL` — if extents flag is set, uses the extent-based read path

**Extent-based reading** (`load_kernel.rs:618-637`):
1. Parses the extent tree from `i_block[0..15]` (60 bytes)
2. Validates extent header: magic `0xF30A`, depth must be 0 (leaf only, no tree traversal)
3. Each extent entry (12 bytes): `ee_block` (first logical block), `ee_len` (length in blocks, masked with `0x7FFF`), `ee_start` (48-bit physical block number)
4. For each extent, reads the contiguous physical blocks directly:
   ```
   sector = ee_start * (block_size / 512)
   read_ext4_sectors(reader, sector, (copy_len + 511) / 512, &mut data[file_off..])
   ```

**Fallback block-by-block reading** (`load_kernel.rs:640-653`):
If extent parsing fails or extents flag is not set, the code falls back to `resolve_block()` which handles:
- Direct blocks (indices 0–11): read `i_block[logical_block]`
- Extents (`EXT4_EXTENTS_FL`): `resolve_block_extents()` at line 506
- Singly indirect blocks: reads the indirect block, then the data block pointer

**Step 6: Capture RSDP from UEFI config table** (`boot/src/main.rs:100-114`)
```rust
let rsdp_addr: u64 = {
    let mut addr = 0u64;
    uefi::system::with_config_table(|entries| {
        for entry in entries {
            if entry.guid == ConfigTableEntry::ACPI2_GUID
                || entry.guid == ConfigTableEntry::ACPI_GUID
            {
                addr = entry.address as u64;
                break;
            }
        }
    });
    addr
};
boot_info.rsdp_addr = rsdp_addr;
```
This MUST be done before `exit_boot_services` because after exit, UEFI runtime services are no longer available. The RSDP address is captured from the UEFI configuration table using either the ACPI v2 GUID or the original ACPI GUID.

**Step 7: Write BootInfo** (`boot/src/main.rs:118-121`)
```rust
unsafe {
    (*boot_info_ptr) = boot_info;
}
```
The updated BootInfo is written back through the dynamically-allocated pointer. The BootInfo struct was originally allocated by the chainloader and its address was stored at `BOOT_INFO_HANDOFF_ADDR`. The bootloader reads that pointer, updates the fields, and writes the whole struct back in place.

**Step 8: Exit boot services** (`boot/src/main.rs:123-125`)
```rust
log::warn!("Exiting UEFI boot services");
let _mmap = unsafe { uefi::boot::exit_boot_services(None) };
```
This is a point of no return. After this call:
- UEFI boot services are terminated
- The UEFI stack is no longer valid
- No UEFI protocols can be called
- The framebuffer and ACPI tables remain accessible (they are standard memory)
- The kernel ELF data buffer is still in memory (allocated via `alloc::vec::Vec` which uses the UEFI allocator, but the memory itself remains)

**Step 9: Disable interrupts** (`boot/src/main.rs:127`)
```rust
unsafe { core::arch::asm!("cli") };
```
Immediately disables interrupts. UEFI may have left PIT or HPET timer interrupts active, and without our own interrupt handlers loaded, they would cause a triple fault.

**Step 10: Load kernel ELF** (`boot/src/main.rs:130-137`)

`load_elf(&kernel_elf_data)` at `load_kernel.rs:40-122`:

1. **ELF header validation**:
   - Minimum size: 64 bytes (sizeof ELF64 header)
   - Magic: `[0x7f, 'E', 'L', 'F']`
   - Class: must be 2 (64-bit)
   - Endianness: must be 1 (little-endian)
   - Type: must be 2 (`ET_EXEC`)

2. **Parse program headers**:
   - `e_phoff` (offset 32): offset to program header table
   - `e_phentsize` (offset 54): size of each entry (56 bytes for ELF64)
   - `e_phnum` (offset 56): number of entries
   - For each `PT_LOAD` segment:
     - `p_offset`: file offset of segment data
     - `p_vaddr`: virtual address (not used for our static relocation)
     - `p_paddr`: physical address — this is where we load the segment
     - `p_filesz`: size in the file
     - `p_memsz`: size in memory (may be larger than filesz for BSS)
     - `p_flags`: permissions (not enforced since we're in ring 0)

3. **Segment loading** (`load_kernel.rs:111-117`):
   ```rust
   let dst = paddr as *mut u8;
   core::ptr::copy_nonoverlapping(data[seg_offset..].as_ptr(), dst, filesz);
   if memsz > filesz {
       core::ptr::write_bytes(dst.add(filesz), 0, memsz - filesz);
   }
   ```
   Each segment is copied from the ELF buffer to its target physical address. If `memsz > filesz`, the remainder is zeroed (BSS). There's a safety check that no segment exceeds `0x800_0000` (128 MB — the kernel is loaded in low memory).

4. **Return entry point**: The kernel's entry point (`_start`) from `e_entry` is returned.

**Step 11: Jump to kernel** (`boot/src/main.rs:142-154`)
```rust
unsafe {
    core::arch::asm!(
        // Simulate the missing call (SysV ABI expects RSP mod 16 = 8 at entry)
        "sub rsp, 8",
        "mov rdi, {boot_info}",
        "jmp {entry}",
        boot_info = in(reg) boot_info_addr,
        entry = in(reg) entry,
        options(noreturn)
    );
}
```
The System V ABI (which the kernel uses) expects RSP mod 16 = 8 at function entry (because `call` pushes 8 bytes, making RSP mod 16 = 0 inside the callee). Since we use `jmp` instead of `call`, we need to manually adjust RSP. The `sub rsp, 8` fixes the alignment.

RDI receives the BootInfo physical pointer (the dynamically allocated address, not `0x1000`) as the first argument to `_start`. The kernel sees this as `boot_info: *const BootInfo`.

### 5.2 Bootloader Ext4 Parsing Detail

The ext4 filesystem reader (`boot/src/load_kernel.rs`) is a complete, self-contained implementation. Here's every structure and function:

**On-disk structures:**
- `Superblock` (lines 143-169): Standard ext4 superblock fields through offset 84 bytes. The superblock is at byte offset 1024 from partition start.
- `BlockGroupDesc` (lines 175-189): 32-byte block group descriptor with bitmap, inode table, and free counts.
- `Inode` (lines 193-210): 128 bytes covering standard ext4 inode fields. Note: does NOT include the extended attribute area (bytes 128-255).
- `DirEntry` (lines 213-219): On-disk directory entry (8-byte header + variable name).
- `ExtentEntry` (lines 221-225): Parsed extent: `ee_block` (logical block), `ee_len` (length), `ee_start` (48-bit physical block).

**Block I/O:**
- `SectorReader` (lines 229-278): Wraps UEFI `BlockIO` protocol. Handles arbitrary block sizes (512, 1024, 2048, 4096) by reading the correct number of blocks and extracting the relevant 512-byte window.
- `read_sector_raw()` (lines 282-304): Stateless sector reader for GPT scanning.
- `read_ext4_sectors()` (lines 364-405): Reads arbitrary byte ranges through the sector abstraction. Handles unaligned reads across sector boundaries.

**Directory traversal:**
- `read_inode()` (lines 407-461): Reads an inode from the inode table. The inode table location is calculated from `bg_inode_table_lo * block_size`. Inode size is assumed to be 256 bytes (ext4 default).
- `parse_extents()` (lines 468-504): Parses the extent tree from the inode's `i_block` array. Validates extent header magic (0xF30A). Only depth 0 (leaf extents) is supported.
- `resolve_block_extents()` (lines 506-552): Given a logical block number, finds the physical block by walking extent entries. Returns the physical block number.
- `resolve_block()` (lines 554-591): Routes to extent-based or indirect-block resolution based on `i_flags & EXT4_EXTENTS_FL`.
- `read_data_block()` (lines 593-607): Reads a single block's data into a buffer.
- `read_file_into()` (lines 609-654): Reads a file's complete data using either extent-based (fast path) or block-by-block (fallback) strategy.
- `read_file()` (lines 656-668): Wrapper that allocates a `Vec<u8>` and calls `read_file_into`.

### 5.3 Bootloader Panic Handler (`boot/src/main.rs:162-197`)

Similar to the chainloader's handler, but more detailed:
- Writes `"PANIC"` to serial
- Formats the file name and line number (manual decimal conversion with digit extraction)
- Uses `core::fmt::Write` trait to format the panic message
- Falls into an infinite `cli; hlt` loop

---

## 6. The Kernel (_start entry)

### 6.1 Linker Script (`kernel/linker.ld`)

The kernel is linked at `0x100000` (1 MB). Sections:
```
ENTRY(_start)
. = 0x100000;
.text : { *(.text .text.*) }
.rodata : { *(.rodata .rodata.*) }
.data : { *(.data .data.*) }
.bss : { *(.bss .bss.*) *(COMMON) }
/DISCARD/ : { *(.eh_frame) *(.comment) *(.note*) }
```

All sections are sequential starting at 1 MB. `eh_frame`, `comment`, and `note` sections are discarded (not needed in a freestanding environment).

### 6.2 Custom Target Specification (`kernel/target.json`)

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

Key settings:
- `disable-redzone: true` — essential for x86-64 interrupt handlers. The red zone is 128 bytes below RSP that the compiler can use without adjusting RSP. But when an interrupt fires, the CPU pushes to the current stack, potentially corrupting the red zone.
- `code-model: "kernel"` — allows the kernel to be linked at the 1 MB mark (negative 2 GB from the kernel code model's range) while the higher-half mapping at `0xFFFF_8000_0000_0000` is accessible via static addresses after page table switch.
- `relocation-model: "static"` — no relocations needed, the kernel is loaded at exactly 0x100000.

### 6.3 Kernel Entry Point (`kernel/src/main.rs:158-418`)

```rust
#[unsafe(no_mangle)]
extern "C" fn _start(boot_info: *const BootInfo) -> ! {
```

The kernel entry point is `_start`, called by the bootloader with RDI = the BootInfo physical pointer (dynamically allocated, pointer stored at `0x1000`). The function never returns (`!`).

**Phase 0: Initialize serial and logger** (`kernel/src/main.rs:162-166`)
```rust
serial::init();
logger::init().unwrap_or(());
log::info!("LodaxOS kernel booting");
log::info!("BootInfo at {:#x}", boot_info as u64);
```
The kernel brings up its own serial port and logger immediately. Even though the bootloader already initialized serial, the kernel re-initializes it to ensure a known state.

**Phase 1A: Extract memory regions** (`kernel/src/main.rs:169-183`)
```rust
let region_count = info.memory_region_count;
let regions: [(u64, u64); 128] = core::array::from_fn(|i| {
    if i < region_count {
        (info.memory_regions[i].phys_start, info.memory_regions[i].size)
    } else {
        (0, 0)
    }
});
let total_free: u64 = regions[..region_count].iter().map(|(_, s)| s).sum();
let free_mb = total_free / (1024 * 1024);
```
The BootInfo's memory regions are copied into a fixed-size array for use during initialization. Total free memory is summed and logged.

**Phase 1B: Initialize framebuffer** (`kernel/src/main.rs:186-191`)
```rust
let mut fb = Framebuffer::from_info(&info.framebuffer);
fb.clear(0, 0, 30);  // Dark blue background
fb.write_str_centered("LodaxOS", 10, 255, 255, 255);
fb.write_str_centered("Kernel starting...", 30, 180, 180, 180);
```
The framebuffer is constructed from BootInfo's `FramebufferInfo`. The `Framebuffer` struct (`kernel/src/main.rs:19-112`) wraps the raw framebuffer pointer and provides:
- `set_pixel(x, y, r, g, b)` — bounds-checked volatile pixel write with BGR/RGB handling
- `clear(r, g, b)` — fast fill using 32-bit writes
- `put_char(ch, x, y, r, g, b)` — renders 8x16 glyph from bitmap font
- `write_str(s, x, y, r, g, b)` — text with `\n` handling
- `write_str_centered(s, y, r, g, b)` — centered text display

**Phase 1C: Initialize physical page allocator** (`kernel/src/main.rs:174`)
```rust
unsafe { mm::phys::init_from_regions(&regions[..region_count], boot_info as u64) };
```
This sets up the buddy-based physical page allocator. The `boot_info` pointer is passed so the allocator can reserve the BootInfo's physical page(s) — otherwise the UEFI memory map would donate them to the free lists as LOADER_DATA. Details in [Section 7.2](#72-physical-memory-allocator).

**Phase 1D: ACPI discovery** (`kernel/src/main.rs:199-237`)
```rust
let madt_addr = if info.madt_addr != 0 {
    info.madt_addr
} else {
    acpi::init(if info.rsdp_addr != 0 { Some(info.rsdp_addr) } else { None })
        .and_then(|ctx| ctx.madt_addr)
        .unwrap_or(0)
};
```
If the bootloader already discovered the MADT address and stored it in BootInfo, it's used directly. Otherwise, the kernel performs its own ACPI discovery:
1. Uses the RSDP address from BootInfo (captured from UEFI config table)
2. Parses the RSDP to find the XSDT
3. Scans XSDT entries for the MADT (`"APIC"` signature)

The MADT is parsed at `kernel/src/main.rs:210`:
```rust
if let Some(madt) = acpi::madt::parse(madt_addr) {
    // Extract IOAPIC info, ISO info, CPU info
}
```
Details in [Section 7.5](#75-acpi-sub-system).

**Phase 1E: Initialize page tables** (`kernel/src/main.rs:240-247`)
```rust
let fb_phys = info.framebuffer.phys_addr;
let fb_size = (info.framebuffer.height * info.framebuffer.stride * info.framebuffer.bytes_per_pixel) as u64;
unsafe { mm::virt::init(&regions[..region_count], Some((fb_phys, fb_size))) };
// After CR3 switch: framebuffer is only mapped in the higher half.
fb.ptr = (0xFFFF_8000_0000_0000u64 + fb_phys) as *mut u8;
```
This is a critical transition point. After `virt::init()`:
- The CPU's CR3 register points to the kernel's own PML4 table
- All physical memory is mapped in the higher half (starting at `0xFFFF_8000_0000_0000`)
- The first 4 GB is identity-mapped (keeping access to ACPI tables, framebuffer, etc.)
- The framebuffer pointer is updated to use the higher-half virtual address

Details in [Section 7.3](#73-virtual-memory-page-tables).

**Phase 1F: Initialize slab heap allocator** (`kernel/src/main.rs:229-233`)
```rust
mm::heap::init();
```
The slab heap is initialized after page tables, since it may need to allocate page-table pages for new slab mappings. Details in [Section 7.4](#74-heap-allocator).

**Phase 1G: Initialize kernel VMA tree** (`kernel/src/main.rs:235-237`)
```rust
mm::vma::init_kernel_vmas();
```
Initializes the kernel VMA (Virtual Memory Area) tree for demand paging. Registers the kernel heap range (`0xFFFF_8080_0000_0000` – `0xFFFF_8084_0000_0000`, 64 MB) as a demand-paged VMA. The page fault handler (`handle_page_fault`) will check this tree when a #PF occurs in kernel mode. Details in [Section 7.5](#75-vma-and-demand-paging).

**Phase 2A: Disable interrupts and mask PIC** (`kernel/src/main.rs:258-261`)
```rust
unsafe { core::arch::asm!("cli") };
arch::idt::mask_pic();
```
Interrupts are disabled. The legacy 8259 PIC is masked by writing `0xFF` to both master (`0x21`) and slave (`0xA1`) interrupt mask registers. This prevents the PIC from delivering interrupts that could collide with our vectors.

**Phase 2B: Map LAPIC MMIO** (`kernel/src/main.rs:266-268`)
```rust
arch::apic::init_mmio();
```
The Local APIC's MMIO region (typically at `0xFEE00000`) is mapped into the higher-half page tables. This is pure page-table work and does NOT require segment registers, so it's safe to do before loading the GDT. Details in [Section 7.6](#76-local-apic-driver).

**Phase 2C: Initialize IOAPIC** (`kernel/src/main.rs:271-276`)
```rust
arch::ioapic::init(&ioapic_infos[..ioapic_count]);
if let Some(ref madt) = madt_parsed {
    intr::init(madt);
}
```
Each discovered IOAPIC's MMIO is mapped (higher-half, cache-disabled). All redirection entries are programmed to safe values (vector 0xFF, masked). The interrupt routing table is built from MADT ISO entries, mapping ISA IRQs to GSIs to IOAPIC pins to vectors. Details in [Section 7.7](#77-io-apic-driver) and [Section 7.8](#78-interrupt-routing).

**Phase 3A: Load GDT and TSS** (`kernel/src/main.rs:325-326`)
```rust
arch::gdt::load();
```
A new GDT is loaded with:
- Null descriptor (index 0)
- Kernel code: 64-bit long mode, ring 0, readable (selector 0x08)
- Kernel data: ring 0, writable (selector 0x10)
- User code: 64-bit long mode, ring 3, readable (selector 0x18)
- User data: ring 3, writable (selector 0x20)
- TSS descriptor (selector 0x28)

The TSS (Task State Segment) is loaded via `ltr` with:
- `rsp0` pointing to a 4 KB aligned stack (dump stack for ring-0 entries)
- `ist1` pointing to the double-fault stack (set up after IDT init)

The `cli; lgdt; far return; set DS/ES/FS/GS/SS; ltr` sequence is executed. Details in [Section 7.9](#79-gdt-and-tss).

**Phase 3B: Initialize IDT** (`kernel/src/main.rs:330-331`)
```rust
arch::idt::init();
```
The IDT is populated with 256 entries:
- Vectors 0–31: CPU exceptions (debug, NMI, breakpoint, page fault, GPF, double fault, etc.)
  - Vector 8 (Double Fault) uses IST1 (Interrupt Stack Table 1) — a separate 16 KB stack
- Vectors 32–63: IRQ handlers (hardware interrupts via IOAPIC)
  - Vector 32: LAPIC timer (scheduler heartbeat)
  - Vectors 33–63: Device IRQs (PIT, keyboard, etc.)
- Vector 0x80: Syscall interrupt gate
- Vector 0xFF: Spurious interrupt (bare `iretq`, no EOI)

Each entry is an interrupt gate (`type_attr = 0x8E`) with the kernel code selector. Details in [Section 7.10](#710-idt-and-interrupt-handling).

**Phase 3C: Initialize task system** (`kernel/src/main.rs:335-336`)
```rust
task::init();
task::init_main_task();
```
The task manager is initialized with the current execution context as task 0 (the "idle" task). An 8 KB kernel stack is allocated and the current RSP is saved. Details in [Section 7.11](#711-task-manager-and-preemptive-scheduling).

**Phase 3D: Create test tasks** (`kernel/src/main.rs:339-348`)
```rust
let task1_entry = simple_task1 as *const () as u64;
if let Some(task_id) = task::create_task(task1_entry) { ... }
let task2_entry = simple_task2 as *const () as u64;
if let Some(task_id) = task::create_task(task2_entry) { ... }
```
Two test tasks are created:
- `simple_task1` (kernel/src/main.rs:421-429): busy-loop incrementing a counter, logs every 500,000 iterations
- `simple_task2` (kernel/src/main.rs:432-439): busy-loop incrementing a counter, logs every 750,000 iterations

Each task gets its own 8 KB kernel stack with a synthetic `TrapFrame` at the bottom and an `iretq` frame at the top. Details in [Section 7.11](#711-task-manager-and-preemptive-scheduling).

**Phase 3E: Install IOAPIC routes** (`kernel/src/main.rs:351-355`)
```rust
let routes = intr::install_and_enable_all();
```
All routes in the interrupt routing table are programmed into their respective IOAPIC redirection entries. All are initially masked.

**Phase 3F: Enable LAPIC** (`kernel/src/main.rs:358`)
```rust
arch::apic::enable();
```
The Local APIC is enabled:
1. Mask LINT0 and LINT1 (prevents spurious extINT deliveries from the legacy PIC)
2. Initialize the LVT Error register with vector 0xFF (spurious) and mask it
3. Set Task Priority Register (TPR) to 0 (accept all interrupts)
4. Enable the LAPIC via the Spurious Interrupt Vector Register (SVR) with vector 0xFF and the enable bit

**Phase 3G: Calibrate LAPIC timer** (`kernel/src/main.rs:360-361`)
```rust
arch::apic::calibrate_pit();
```
A critical measurement: the LAPIC timer is calibrated against the PIT (Programmable Interval Timer) channel 0. The process:
1. Save RFLAGS.IF state, then disable interrupts
2. Configure LAPIC timer: one-shot mode, divisor 16, load `0xFFFF_FFFF` as initial count
3. Program PIT channel 0 in Mode 0 (interrupt on terminal count), which does NOT auto-reload — this is important because it eliminates measurement errors from scheduling jitter
4. Poll the PIT counter until it counts down from `pit_target` to ~0
5. Read the LAPIC Current Count Register (CCR)
6. Calculate `elapsed = initial_count - current`
7. Compute `ticks_per_ms = elapsed / 20` (20 ms measurement window)
8. Restore interrupt state

The PIT runs at 1,193,182 Hz. For a 20 ms window, the target count is `1,193,182 * 20 / 1000 ≈ 23,864` — well within the 16-bit counter limit.

**Phase 3H: Configure LAPIC timer** (`kernel/src/main.rs:363-365`)
```rust
arch::apic::configure_timer(16, 32, true);
arch::apic::set_timer_count(1);
```
The LAPIC timer is configured:
- Divisor: 16 (encoding `0b0011`)
- Vector: 32 (the first IRQ vector, reserved for the scheduler)
- Mode: periodic
- Timer count: `ticks_per_ms * 1` → fires every 1 ms

**Phase 3I: Enable PIT in periodic mode** (`kernel/src/main.rs:367`)
```rust
arch::apic::pit_enable_periodic(100);
```
PIT channel 0 is reprogrammed to Mode 2 (rate generator) at 100 Hz (`reload = 1,193,182 / 100 ≈ 11,932`). This makes the PIT fire periodic interrupts as a second device IRQ source for testing.

**Phase 3J: Enable interrupts and test** (`kernel/src/main.rs:369-373`)
```rust
log::info!("Enabling interrupts");
unsafe { core::arch::asm!("sti") };
log::info!("Triggering int 32 (software) to test IRQ stub...");
unsafe { core::arch::asm!("int 32") };
```
Interrupts are enabled via `sti`. Then vector 32 is triggered via software `int 32` instruction to verify the IRQ stub works independently of the LAPIC.

**Phase 3K: Unmask device routes** (`kernel/src/main.rs:376-390`)
```rust
if let Some(route) = intr::lookup_isa(0) {
    intr::enable_route(route);
}
if let Some(route) = intr::lookup_isa(1) {
    intr::enable_route(route);
}
```
ISA IRQ 0 (PIT) and ISA IRQ 1 (keyboard) routes are unmasked in the IOAPIC, enabling actual hardware interrupt delivery.

**Phase 4: Idle loop** (`kernel/src/main.rs:397-417`)
```rust
loop {
    unsafe { core::arch::asm!("hlt") };
    let now = arch::idt::ticks();
    if now - last_log >= 1000 {
        let pit = arch::idt::pit_ticks();
        let keys = arch::idt::key_count();
        // Log tick/PIT/keyboard stats
        last_log = now;
    }
}
```
The kernel enters an infinite idle loop:
1. Executes `hlt` to halt the CPU until the next interrupt
2. On wake (from LAPIC timer IRQ), checks tick count
3. Every 1000 ticks (~1 second), logs: LAPIC tick count, PIT counter, keyboard scancode count, number of tasks

### 6.4 Kernel Panic Handler (`kernel/src/main.rs:443-480`)

Writes panic location and message to serial, then halts with `cli; hlt`.

---

## 7. Subsystem Reference

### 7.1 Serial Port and Logger

**IMPORTANT NOTE:** The `src/serial.rs`, `src/logger.rs`, `src/task.rs`, and `src/font.rs` files are the canonical source files. The `shared/src/serial.rs`, `shared/src/logger.rs`, `shared/src/task.rs`, and `shared/src/font.rs` files are wrappers that use `#[path]`, `include!()`, or `pub use` to re-export the implementations from `src/`.

The kernel and boot crates then re-export from the shared crate via one-liner wrapper modules:
- `kernel/src/serial.rs:1` → `pub use lodaxos_core::serial::*;`
- `boot/src/serial.rs:1` → `pub use lodaxos_core::serial::*;`

**Serial initialization** (`src/serial.rs:3-16`):
```rust
COM1 = 0x3F8 (standard x86 COM1 I/O port)
LCR (0x3FB): DLAB=1 → set divisor (0x01 = 115200 baud for 1.8432 MHz clock)
IER (0x3F9): disable all interrupts
LCR (0x3FB): 8N1 (0x03)
FCR (0x3FA): enable FIFO, clear, 14-byte threshold (0xC7)
MCR (0x3FC): assert DTR + RTS + enable IRQ (0x0B)
```

**Serial write** (`src/serial.rs:18-38`):
- `write_byte(byte)`: polls LSR (0x3FD) bit 5 (THR empty), then writes to THR (0x3F8)
- `write_str(s)`: iterates bytes, converts `\n` to `\r\n`

**Logger** (`src/logger.rs`):
- Implements `log::Log` trait
- Format: `[LEVEL] target: message\n`
- Max log level: Trace
- Registered via `log::set_logger(&LOGGER)`

### 7.2 Physical Memory Allocator — Buddy System (`src/mm/phys.rs`)

**Data structure** (`phys.rs:37-57`):
A single `Zone` with 11 per-order free lists and atomic counters:
```rust
struct Zone {
    base: u64,
    top: u64,
    free_lists: [*mut FreeBlock; 11],  // orders 0..10
    total_pages: AtomicUsize,
    free_pages: AtomicUsize,
}
```
Each `FreeBlock` is stored inline within free pages (zero metadata overhead). Free lists are singly-linked via raw `*mut FreeBlock` pointers.

**Orders:**
| Order | Pages | Block Size |
|---|---|---|
| 0 | 1 | 4 KB |
| 1 | 2 | 8 KB |
| 2 | 4 | 16 KB |
| 3 | 8 | 32 KB |
| 4 | 16 | 64 KB |
| 5 | 32 | 128 KB |
| 6 | 64 | 256 KB |
| 7 | 128 | 512 KB |
| 8 | 256 | 1 MB |
| 9 | 512 | 2 MB |
| 10 | 1024 | 4 MB |

**Initialization** (`phys.rs:155-197`):
`init_from_regions(regions: &[(u64, u64)], boot_info_phys: u64)`:
1. Scans all regions to find `min_base` and `max_top`.
2. Reserves the BootInfo pages by splitting any covering buddy block around them.
2. For each region, calls `carve_blocks()` which:
   - Skips reserved pages (page 0, BootInfo handoff page at `0x1000`)
   - Calls `max_order_at(addr, remaining)` to find the largest power-of-2 block that fits (bounded by alignment, size, and `MAX_ORDER` = 10)
   - Inserts each block into the corresponding free list
3. Sets `total_pages` and `free_pages` atomic counters.

**Allocation** (`phys.rs:199-230`):
- `alloc_order(order)`: Pops from `free_lists[order]` if non-empty (O(1)). If empty, searches upward for the next non-empty order, splits the found block via `split_and_enqueue()` (O(orders) worst case).
- `alloc_page()`: calls `alloc_order(0)`.
- `alloc_pages(count)`: rounds up to the smallest covering order, allocates, and frees any excess pages as individual order-0 blocks.
- All operations are protected by a `SpinLock`.

**Deallocation** (`phys.rs:232-286`):
- `free_order(addr, order)`: Computes the buddy address via `addr ^ (1 << (order + 12))`. If the buddy is also free and at the same order, removes it from the free list and recurses at `order + 1` (coalesce). Otherwise, inserts the block into `free_lists[order]`. Refuses to free reserved pages.
- `free_page(addr)`: calls `free_order(addr, 0)`.
- `free_pages(addr, count)`: frees each page individually (each triggers coalescing).

**Thread safety:**
A `SpinLock` (`AtomicBool` with `compare_exchange_weak` + `pause` loop) protects all zone operations. The zone is accessed through `fn zone_ptr() -> *mut Zone` which uses `&raw mut ZONE` (Rust 2024 does not allow direct `&mut` references to `static mut`).

**Performance:**
- Allocation: O(1) when target order is non-empty; O(orders) worst case (split chain).
- Deallocation: O(1) when buddy is busy; O(orders) worst case (coalesce chain).
- Internal fragmentation: at most (2^n - 1) pages per allocation.

### 7.3 Virtual Memory Page Tables (`src/mm/virt.rs`)

**Initialization** (`virt.rs:50-140`):
`init(regions, fb_phys)`:
1. Allocates a 4 KB page for the PML4 table and zeroes it
2. For each free memory region:
   - Maps leading unaligned portion with 4 KB pages (to next 2 MB boundary)
   - Maps aligned middle portion with 2 MB huge pages
   - Maps trailing portion with 4 KB pages
   - All mappings are in the higher half: `virt = HIGHER_HALF + phys`, with `PRESENT | WRITABLE | NO_EXECUTE` flags
3. Identity-maps the first 4 GB: PML4[0] → PDP[0..3] → PD tables with 2 MB huge pages
   - The 2 MB page containing `0xFEE00000` (LAPIC) uses additional flags: `PCD` (cache disable) and `PS` (page size)
4. Maps the framebuffer in the higher half with 4 KB pages
5. Loads CR3 with the new PML4 physical address

**Page table walking:**
- 4-level translation: PML4 → PDP → PD → PT → 4 KB page
- Huge pages: 1 GB at PDP level (bit 7 set), 2 MB at PD level (bit 7 set)
- Entry flags: `PRESENT` (bit 0), `WRITABLE` (bit 1), `USER` (bit 2), `CACHE_DISABLE/PCD` (bit 4), `NO_EXECUTE` (bit 63)

**Key functions:**
- `translate(virt)` — walks the current page table hierarchy to resolve a virtual address
- `unmap(virt)` — clears a page table entry and flushes TLB with `invlpg`
- `map_page(pml4, virt, phys, flags)` — creates a 4 KB mapping
- `map_contiguous(...)` — maps a range with batch processing (O(N/512) PT table walks)
- `map_region(pml4, phys, size, flags)` — identity + higher-half mapping
- `map_region_higher_half(...)` — higher-half only (for MMIO, avoids conflict with identity huge pages)
- `pml4_address()` — reads CR3

**Constants:**
- `HIGHER_HALF = 0xFFFF_8000_0000_0000`
- `PAGE_SIZE = 0x1000`

### 7.4 Slab Heap Allocator (`src/mm/heap.rs`)

**Data structure — Slab** (`heap.rs:29-100`):
Each slab is a block of pages from the buddy allocator with the metadata header embedded at the base:
```rust
struct Slab {
    free_head: *mut u8,    // head of free-object linked list
    slab_base: *mut u8,    // base address of the slab
    next: *mut Slab,       // linked list within cache
    prev: *mut Slab,
    order: u8,             // buddy order of this slab
    total_objs: u16,
    free_objs: u16,
}
```
Free objects within a slab are linked via embedded `*mut u8` pointers stored in the first 8 bytes of each free object. On allocation, the head is popped; on free, the object is pushed onto the head.

**Data structure — KmemCache** (`heap.rs:102-311`):
```rust
struct KmemCache {
    obj_size: usize,       // size of each object
    slab_order: u8,        // buddy order for new slabs
    objs_per_slab: u16,
    partial: *mut Slab,    // partially filled slabs
    free: *mut Slab,       // fully free slabs
    full: *mut Slab,       // fully allocated slabs
    lock: SpinLock,
}
```

**Cache configuration** (`heap.rs:337-353`):
Nine fixed caches covering sizes 32 B through 8 KB:
| Index | Object Size | Slab Order | Objects/Slab |
|---|---|---|---|
| 0 | 32 B | 0 (4 KB) | 127 |
| 1 | 64 B | 0 (4 KB) | 63 |
| 2 | 128 B | 0 (4 KB) | 31 |
| 3 | 256 B | 0 (4 KB) | 15 |
| 4 | 512 B | 0 (4 KB) | 7 |
| 5 | 1024 B | 0 (4 KB) | 3 |
| 6 | 2048 B | 0 (4 KB) | 1 |
| 7 | 4096 B | 1 (8 KB) | 1 |
| 8 | 8192 B | 2 (16 KB) | 1 |

Sizes are recalculated at runtime by `cache_params()` to account for the `Slab` header overhead.

**Initialization** (`heap.rs:423-428`):
`init()`: Calls `CacheAllocator::init()` which recalculates slab params for each cache. No pre-mapping of heap pages — each slab is allocated on demand.

**Allocation** (`heap.rs:125-176`):
`kmalloc(size)`:
1. Rounds size up to the nearest cache size (32 B..8 KB) via `CacheAllocator::cache_index()`.
2. Locks the target cache's spinlock.
3. Pops from the first partial slab's free list (O(1)).
4. If no partial slab: pops from the free list (moves slab to partial or full).
5. If no free slab: allocates a new slab from the buddy allocator (`phys::alloc_order`), initializes its free list.
6. If the slab becomes full, moves it to the full list.
7. Sizes > 8 KB: allocates buddy pages directly.

**Deallocation** (`heap.rs:178-204`):
`kfree(ptr, size)`:
1. Determines the owning cache by size rounding.
2. Locates the slab containing `ptr` by scanning the partial and full lists (linear scan; acceptable with few slabs per cache).
3. Pushes the object onto the slab's free list.
4. If the slab becomes fully free, moves it to the free list (cached for reuse).

**GlobalAllocator** (`heap.rs:430-447`):
`GlobalAllocator` implements `core::alloc::GlobalAlloc`, delegating to `kmalloc/kfree`. Installed as `#[global_allocator]`. An `initialized` atomic flag gates allocation before init; early `alloc` calls return null.

**Virtual address range:**
No fixed heap arena is pre-mapped. Each new slab allocates physical pages from the buddy allocator, which are then mapped into virtual memory via `map_contiguous` at addresses within `0xFFFF_8080_0000_0000` (the heap VMA registered by `init_kernel_vmas` covers 64 MB at this base).

**Thread safety:**
Each `KmemCache` has its own `SpinLock`. Different-size allocations on different cores proceed in parallel; same-size allocations serialize per cache.

### 7.5 VMA and Demand Paging (`src/mm/vma.rs`)

**Radix tree** (`vma.rs:56-57`):
The VMA tree uses a 4-level radix tree covering bits 12–51 of the virtual address (40 bits = 1 TB per tree). Each level indexes 10 bits:
```
Level 0 (bits 51:42) → Level 1 (bits 41:32) → Level 2 (bits 31:22) → Level 3 (bits 21:12)
```
Nodes are allocated from the buddy allocator (one page = 1024 × 8-byte entries). Leaf slots hold `*mut Vma` pointers; tree-level slots hold `*mut RadixNode` child pointers.

**VMA struct** (`vma.rs:27-33`):
```rust
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub perm: VmaPerm,
    pub flags: u64,
}
```

**Key tree operations** (`vma.rs:68-225`):
- `insert(vma)`: walks 4-level tree, allocates missing nodes, stores VMA pointer in leaf slot at index derived from `vma.start`.
- `find_covering(addr)`: walks to the leaf covering `addr`, does a linear scan of all VMAs in that leaf (up to 1024, but typically <100).
- `lookup(addr)`: exact-match lookup via tree walk (no scan).
- `remove(start)`: zeros the leaf slot; does not reclaim tree nodes (acceptable for infrequent VMA removal).

**Global kernel VMA tree** (`vma.rs:288-315`):
```rust
static mut KERNEL_VMA_TREE: VmaTree = VmaTree::new_const();
```
`init_kernel_vmas()` registers the kernel heap virtual range (`0xFFFF_8080_0000_0000` – `0xFFFF_8084_0000_0000`, 64 MB) as a `ReadWrite` VMA.

**Per-process memory** (`vma.rs:228-284`):
```rust
pub struct ProcessMemory {
    pub vma_tree: VmaTree,
    pub pml4_phys: u64,
}
```
`ProcessMemory::add_vma(start, end, perm)` allocates a VMA from the buddy allocator and inserts it.
`ProcessMemory::handle_page_fault(addr, write)` checks the VMA for coverage, allocates a physical page, and calls `map_page_explicit()` to create the page table entry.

**Page fault handler** (`vma.rs:317-359`):
`handle_page_fault(fault_addr, error_code)`:
1. If `error_code & 1` (present flag) is set: protection violation → return false (unhandled).
2. If `error_code & 4` (user mode) is set: user fault → return false (per-process VMA tree not yet wired into scheduler).
3. Otherwise (kernel mode, not-present): walk `KERNEL_VMA_TREE` via `find_covering()`.
4. If no VMA covers the fault address: return false (unhandled → triple fault/panic).
5. If VMA found: allocate a zeroed physical page via `phys::alloc_page()`, read CR3 to get current PML4, call `map_page_explicit(cr3, page_addr, phys_page, DATA)`.
6. Return true → the IDT handler resumes execution at the faulting instruction, which retries with the page now mapped.

**IDT wiring** (`src/arch/idt.rs:508`):
```rust
14 => {
    let cr2: u64;
    unsafe { asm!("mov {cr2}, cr2", cr2 = out(reg) cr2) };
    resolved = crate::mm::vma::handle_page_fault(cr2, error);
    if resolved {
        log::info!("  -> Resolved via demand paging");
    } else {
        log::error!("  -> Unresolved page fault");
    }
}
```

**Thread safety:**
The kernel VMA tree is initialized once during boot and currently has no concurrent access (kernel init is single-threaded, and the scheduler is inactive during init). Future SMP support will require a lock. Per-process VMA trees will be protected by the process lock.

### 7.6 ACPI Sub-system (`src/acpi/mod.rs`)

**RSDP Discovery** (`acpi.rs:102-144`):
`find_rsdp()` scans in order:
1. UEFI configuration table (`find_rsdp_from_uefi()` — uses `uefi::system::with_config_table` to find `ACPI2_GUID` entry)
2. EBDA (Extended BIOS Data Area): reads word at `0x40E` as a segment pointer, scans 1 KB from `segment << 4`
3. BIOS ROM area: scans `0xE_0000` to `0x10_0000` (128 KB)
4. OVMF/UEFI firmware area: scans `0xFEFF_0000` to `0xFF00_0000` (64 KB)

Each candidate location checks for the `"RSD PTR "` signature at 16-byte-aligned addresses.

**RSDP Validation** (`acpi.rs:50-58`):
`rsdp_checksum_valid(rsdp)`: sums all bytes of the RSDP structure (20 bytes for v1, 36 bytes for v2+), must equal 0.

**XSDT/RSDT Parsing** (`acpi.rs:165-195`):
`find_sdt(xsdt_addr, signature)`:
1. Reads the SDT header (signature, length)
2. Determines if XSDT (signature = `"XSDT"`, 8-byte entries) or RSDT (4-byte entries)
3. Iterates entries, validates each table's checksum
4. Returns the address of the matching table

**MADT Parsing** (`src/acpi/madt.rs:116-181`):
`parse(addr)`:
1. Validates signature (`"APIC"`) and checksum
2. Reads MADT-specific header: local APIC address (4 bytes) and flags (4 bytes)
3. Walks entry structures starting at offset `sizeof(SdtHeader) + 8`
4. Each entry has a 2-byte header: type (1 byte) + length (1 byte)
5. Entry types parsed:
   - Type 0 (Local APIC): ACPI processor ID, APIC ID, enable flag
   - Type 1 (IOAPIC): IOAPIC ID, address, GSI base
   - Type 2 (ISO/Interrupt Source Override): bus, source, GSI, flags
   - Type 4 (NMI): processor ID, flags, LINT pin
   - Type 5 (Local APIC Address Override): 64-bit APIC address
   - Type 6 (IOAPIC NMI): IOAPIC ID, flags, GSI

### 7.7 Local APIC Driver (`src/arch/apic.rs`)

**Initialization** (`apic.rs:92-114`):
`init_mmio()`:
1. Reads IA32_APIC_BASE MSR (0x1B) to get the LAPIC physical base address
2. Maps the 4 KB MMIO region into the higher-half page tables with `CACHE_DISABLE` flag
3. Caches the virtual address

**MSR Access** (`apic.rs:52-62`):
```rust
unsafe fn read_msr(msr: u32) -> u64 {
    // rdmsr from ecx → eax:edx → combine to u64
}
```

**MMIO Register Access** (`apic.rs:67-76`):
```rust
unsafe fn read32(offset: usize) -> u32 { /* volatile read */ }
unsafe fn write32(offset: usize, val: u32) { /* volatile write */ }
```

**Enable** (`apic.rs:120-149`):
`enable()`:
1. Mask all 8259 PIC IRQs (write 0xFF to ports 0x21 and 0xA1)
2. Mask LINT0 and LINT1 (APIC_LVT_LINT0 at offset 0x350, APIC_LVT_LINT1 at 0x360)
3. Initialize LVT Error (offset 0x370) with vector 0xFF, masked
4. Set TPR (offset 0x80) to 0
5. Set SVR (offset 0xF0) with enable bit (1<<8) and spurious vector 0xFF

**EOI** (`apic.rs:295-299`):
```rust
pub fn send_eoi() { write32(0xB0, 0); }
```

**Timer Calibration** (`apic.rs:202-263`):
`calibrate_pit()` — described in Phase 3G above.

**Timer Configuration** (`apic.rs:156-191`):
`configure_timer(divisor, vector, periodic)`:
1. Sets the Divide Configuration Register (TDCR, offset 0x3E0)
2. Programs the LVT Timer entry (offset 0x320) with vector + periodic bit
3. Reads back to verify

**Timer Count** (`apic.rs:268-274`):
`set_timer_count(ms)`: writes `ticks_per_ms * ms` to TICR (offset 0x380)

**PIT Periodic Mode** (`apic.rs:279-290`):
`pit_enable_periodic(freq_hz)`:
1. Programs PIT control register (0x43) with Mode 2 (rate generator)
2. Writes reload value to PIT data port (0x40)

### 7.8 I/O APIC Driver (`src/arch/ioapic.rs`)

**IOAPIC MMIO Access** (`ioapic.rs:42-54`):
```rust
unsafe fn read_reg(&self, index: u32) -> u32 {
    // Write index to IOREGSEL (base+0x00)
    // Read from IOWIN (base+0x10)
}
unsafe fn write_reg(&self, index: u32, value: u32) {
    // Write index to IOREGSEL
    // Write value to IOWIN
}
```

**Initialization** (`ioapic.rs:148-219`):
`init(ioapic_infos: &[IoApicInfo])`:
1. For each discovered IOAPIC, maps its 4 KB MMIO region (higher-half, cache-disabled)
2. Reads hardware ID from IOAPICID register (bits 27:24)
3. Reads version and max redirection entry from IOAPICVER register
4. Masks ALL redirection entries (up to `max_redir`) with safe values: vector 0xFF, masked

**Redirection Entry Programming** (`ioapic.rs:87-93`):
```rust
pub fn set_entry(&self, pin: u8, low: u32, high: u32) {
    let reg = IOAPIC_REDIR_BASE + (pin as u32) * 2;
    self.write_reg(reg, low);
    self.write_reg(reg + 1, high);
}
```

**Entry Building** (`ioapic.rs:106-131`):
`make_redir_low(vector, flags, masked)`:
- Vector bits 7:0
- Delivery mode = fixed (bits 10:8 = 000)
- Destination mode = physical (bit 11 = 0)
- Polarity from flags bit 1 (0 = active-high, 1 = active-low)
- Trigger from flags bit 3 (0 = edge, 1 = level)
- Mask from parameter (bit 16)

`make_redir_high(apic_id)`: destination APIC ID in bits 56:63

### 7.9 Interrupt Routing (`src/intr/mod.rs`)

**Vector Allocation** (`intr.rs:22-30`):
```rust
static NEXT_VECTOR: AtomicU8 = AtomicU8::new(FIRST_DEV_VECTOR);  // 33
```
Vectors 33 through 63 (31 vectors total) are available for device IRQs. Vector 32 is reserved for the LAPIC timer. Vector 0xFF is the spurious vector.

**Route Building** (`intr.rs:160-172`):
`build_route(madt, isa_source, gsi, flags)`:
1. Finds the IOAPIC that handles the GSI via `madt::lookup_ioapic()`
2. Allocates a unique vector
3. Returns an `IrqRoute` with all mappings

**Route Table Initialization** (`intr.rs:84-152`):
`init(madt)`:
1. For each ISO (Interrupt Source Override) in the MADT with `bus == 0`:
   - Records the GSI as claimed
   - Builds a route with the ISO's flags (polarity, trigger mode)
2. For each ISA IRQ 0-15 without an ISO:
   - Uses identity mapping (GSI = IRQ)
   - Builds a route with default flags (high polarity, edge trigger)
3. Maximum: 32 routes

**Route Assignment** (`intr.rs:214-263`):
- `install_route(route)`: programs the IOAPIC redirection entry (masked)
- `enable_route(route)`: unmasks the IOAPIC entry
- `install_and_enable_all()`: installs all routes (returns count)

**Lookups:**
- `lookup_isa(isa_irq)` → `Option<&IrqRoute>`
- `lookup_gsi(gsi)` → `Option<&IrqRoute>`
- `lookup_vector_isa(vector)` → `Option<u8>` (ISA source for a given vector)

### 7.10 GDT and TSS (`src/arch/gdt.rs`)

**GDT Layout** (`gdt.rs:107-115`):
7 entries (56 bytes):
| Index | Selector | Content |
|-------|----------|---------|
| 0 | 0x00 | Null descriptor |
| 1 | 0x08 | Kernel code (ring 0, 64-bit, readable) |
| 2 | 0x10 | Kernel data (ring 0, writable) |
| 3 | 0x18 | User code (ring 3, 64-bit, readable) |
| 4 | 0x20 | User data (ring 3, writable) |
| 5 | 0x28 | TSS low (8 bytes) |
| 6 | 0x30 | TSS high (8 bytes) |

**Descriptor Encoding** (`gdt.rs:52-66`):
```rust
const fn make_descriptor(base: u32, limit: u32, access: u8, granularity: u8) -> u64
```
Standard 8-byte format: Limit[15:0], Base[15:0], Base[23:16], Access, Flags+Limit[19:16], Base[31:24].

**TSS Descriptor** (`gdt.rs:82-103`):
16-byte system descriptor spanning two GDT entries:
- Low 8 bytes: Limit[15:0], Base[15:0], Base[23:16], Access=0x89, Flags=0, Base[31:24]
- High 8 bytes: Base[63:32]

**Load Sequence** (`gdt.rs:190-238`):
`load()`:
1. Set `TSS.rsp0` to top of the dummy kernel stack
2. Verify TSS address is canonical (bits 48-63 match bit 47)
3. Build TSS descriptor from the static TSS's address
4. Assembly sequence:
   - `cli` (disable interrupts)
   - `lgdt [gdt_ptr]` (load GDT register)
   - Far return via `push cs; lea rax, [3f]; push rax; retfq` to reload CS
   - `mov ax, 0x10; mov ds, ax; mov es, ax; mov fs, ax; mov gs, ax; mov ss, ax` (data segments)
   - `mov ax, 0x28; ltr ax` (load TSS)

**Debug Tracing** (`gdt.rs:167-186`):
`com1_trace(ch)`: writes single bytes to COM1 during GDT loading to debug early boot. The letters 'A' through 'D' are emitted at key stages.

### 7.11 IDT and Interrupt Handling (`src/arch/idt.rs`)

**IDT Layout** (`idt.rs:285-288`):
256 entries, each 16 bytes:
- Type/Attribute = 0x8E (interrupt gate, present, ring 0, 64-bit)
- Selector = 0x08 (kernel code)

**Exception Vectors 0-31** (`idt.rs:358-388`):
- 0: #DE (Divide Error) — no error code
- 1: #DB (Debug) — no error code
- 2: NMI — no error code
- 3: #BP (Breakpoint) — no error code, returns instead of halts
- 4: #OF (Overflow) — no error code
- 5: #BR (Bound Range) — no error code
- 6: #UD (Invalid Opcode) — no error code
- 7: #NM (Device Not Available) — no error code
- 8: #DF (Double Fault) — HAS error code, uses IST1
- 9: Coprocessor Segment Overrun — no error code
- 10: #TS (Invalid TSS) — HAS error code
- 11: #NP (Segment Not Present) — HAS error code
- 12: #SS (Stack Fault) — HAS error code
- 13: #GP (General Protection) — HAS error code, decodes selector index
- 14: #PF (Page Fault) — HAS error code, reads CR2
- 16: #MF (x87 FPU) — no error code
- 17: #AC (Alignment Check) — HAS error code
- 18: #MC (Machine Check) — no error code
- 19: #XM (SIMD) — no error code
- 20: #VE (Virtualization) — no error code
- 21: #CP (Control Protection) — HAS error code
- 22-31: Reserved — no error code

**IRQ Vectors 32-63** (`idt.rs:391-422`):
- 32: LAPIC timer (scheduler heartbeat)
- 33: First device IRQ (e.g., PIT)
- 34-63: Additional device IRQs
- 255: Spurious interrupt
- 0x80: Syscall

**Stub Generation** (`idt.rs:109-281`):
Two macros generate naked assembly stubs:

`define_stub_noerr!` (used for no-error-code exceptions and all IRQs):
```asm
push 0              ; dummy error code
push <vector>       ; vector number
push rdi..r15       ; save all GPRs
mov rdi, rsp        ; arg1 = &TrapFrame (SysV ABI)
mov rcx, rsp        ; arg1 = &TrapFrame (Win64 ABI)
sub rsp, 32         ; shadow space
call interrupt_dispatcher
add rsp, 32
pop r15..rdi        ; restore all GPRs
add rsp, 16         ; remove vector + error code
iretq
```

`define_stub_err!` (used for error-code exceptions):
Same but without `push 0` (CPU already pushed the error code), and the vector is pushed instead.

**TrapFrame Layout** (`idt.rs:56-105`):
```rust
struct TrapFrame {
    r15, r14, ..., rdi: u64,    // saved GPRs (last pushed = first in struct)
    vector: u64,                // pushed by stub
    error_code: u64,            // pushed by CPU (or stub for no-err)
    rip: u64,                   // CPU-pushed interrupt frame
    cs: u64,
    rflags: u64,
    rsp: u64,                   // only if privilege-level change
    ss: u64,
}
```

**Interrupt Dispatcher** (`idt.rs:445-464`):
`interrupt_dispatcher(frame)`:
- Routes by vector:
  - 0-31: `exception_handler`
  - 32-63: `irq_handler`
  - 0xFF: no-op (spurious)
  - 0x80: `syscall_handler`
  - Others: `exception_handler`

**Exception Handler** (`idt.rs:466-531`):
Logs the exception type, vector number, RIP, and full register state. For #PF, also reads CR2 for the faulting address. For #GP, decodes the error code to extract selector index. For #DF, logs and halts immediately.

**IRQ Handler** (`idt.rs:535-596`):
1. Sends EOI to LAPIC (if initialized)
2. Vector 32 (LAPIC timer):
   - Increments `TICKS` atomic counter
   - If task system is initialized, calls `task::schedule(frame)` for preemptive context switch
   - The switch uses `popfq + retfq` instead of `iretq` to avoid strict CS descriptor checks
3. Other vectors:
   - Looks up ISA source via `intr::lookup_vector_isa`
   - ISA 0 (PIT): increments `PIT_TICKS`
   - ISA 1 (keyboard): reads scancode from port 0x60, stores in `KEY_SCANCODE`, increments `KEY_COUNT`

**Syscall Handler** (`idt.rs:612-646`):
- rax = syscall number
- 0: yield (no-op, preemptive timer handles it)
- 1: exit (blocks current task + reschedules)
- 2: get_task_id (returns current task ID in rax)
- 3: wake_task(task_id) (wakes a blocked task)
- 4: get_ticks (returns TICKS counter)

### 7.12 Task Manager and Preemptive Scheduling (`src/task.rs`)

**Data Structures** (`task.rs:9-41`):
```rust
enum TaskState { Ready, Blocked }
struct Task {
    id: usize,
    saved_frame: TrapFrame,     // full register state
    kernel_stack_base: u64,     // bottom of 8 KB stack
    state: TaskState,
}
struct TaskManager {
    tasks: [Option<Task>; 16],  // max 16 tasks
    current: usize,
    count: usize,
    initialized: bool,
}
```

**Initialization** (`task.rs:47-92`):
`init()`: resets the manager state.
`init_main_task()`:
1. Allocates 2 physical pages (8 KB) for task 0's kernel stack
2. Maps them at `HIGHER_HALF + phys` and zeroes them
3. Creates a synthetic TrapFrame for task 0 with the current RSP and 0x08 CS
4. Registers task 0 as "Ready"

**Task Creation** (`task.rs:134-188`):
`create_task(entry: u64) -> Option<usize>`:
1. Checks if maximum tasks (16) reached
2. Allocates 2 pages (8 KB) for the new task's kernel stack
3. Maps at `HIGHER_HALF + phys`, zeroes the stack
4. Builds an iretq frame at `stack_top - 24`:
   - `[stack_top - 24]` = entry point (RIP)
   - `[stack_top - 16]` = 0x08 (kernel code selector)
   - `[stack_top - 8]` = 0x202 (RFLAGS with IF=1)
5. Builds a TrapFrame at the stack bottom with RSP pointing to the iretq frame
6. Registers the task as "Ready" and returns its ID

**Scheduler** (`task.rs:197-245`):
`schedule(frame: &mut TrapFrame) -> bool`:
Called from the LAPIC timer IRQ handler:
1. If fewer than 2 tasks, returns false (nothing to switch to)
2. Computes the real interrupted RSP: `(frame_address + 0xA0)` — this is where the CPU pushed return RIP, CS, RFLAGS
3. Saves the current task's full register state (including corrected RSP)
4. Scans for the next READY task using round-robin (wraps around)
5. If found, overwrites the interrupt stack's TrapFrame with the next task's saved state
6. When the IRQ stub returns via iretq/popfq+retfq, it restores the new task's state

**The popfq+retfq workaround** (`kernel/src/main.rs:553-571`):
Instead of `iretq`, the scheduler uses:
```asm
push {cs}     ; 0x08
push {rip}    ; next task's RIP
push {rflags} ; next task's RFLAGS
popfq         ; restore RFLAGS
retfq          ; far return to next task
```
This avoids `iretq`'s strict descriptor checks which can reject a valid 0x08 selector when reached on a different privilege-level path.

**Task Blocking and Waking** (`task.rs:251-274`):
- `block_current(frame)`: marks current task as Blocked, reschedules
- `wake(task_id)`: marks a blocked task as Ready

**Cooperative Yield** (`task.rs:281-283`):
```rust
pub fn yield_now() {
    unsafe { asm!("int 0x80", in("rax") 0u64) };  // syscall 0 = yield
}
```

### 7.13 Framebuffer and Font (`kernel/src/main.rs`, `src/font.rs`)

**Framebuffer** (`kernel/src/main.rs:19-112`):
Wraps the raw framebuffer pointer with:
- `set_pixel(x, y, r, g, b)`: bounds-checked, volatile write (handles BGR vs RGB)
- `clear(r, g, b)`: fast 32-bit fill (writes `u32` colors directly)
- `put_char(ch, x, y, r, g, b)`: renders an 8x16 bitmap glyph
- `write_str(s, x, y, r, g, b)`: renders a string with newline handling
- `write_str_centered(s, y, r, g, b)`: centers text horizontally

**Font** (`src/font.rs`):
- 8x16 pixel bitmap font for ASCII 32-126 (95 characters)
- `get_glyph(ch)`: returns `&[u8; 16]` — each byte is a row of 8 pixels (MSB = leftmost)

---

## 8. Build System & Toolchain

### 8.1 Toolchain (`rust-toolchain.toml`)

```toml
[toolchain]
channel = "nightly"
targets = ["x86_64-unknown-uefi"]
```

### 8.2 Build Sequence (`build.bat`)

1. Build `lodaxos-system` (library, no special target)
2. Build `lodaxos-kernel` with custom target `kernel/target.json`, unstable flags: `-Zbuild-std=core,alloc`, `-Zbuild-std-features=compiler-builtins-mem`
3. Build `lodaxos-boot` for `x86_64-unknown-uefi`
4. Build `lodaxos-chain` for `x86_64-unknown-uefi`
5. Copy kernel ELF from `target/target/debug/deps/lodaxos_kernel-*` to `kernel.elf`

The build uses:
- `-Zjson-target-spec` for the custom kernel target
- `-Zbuild-std=core,alloc` to build `core` and `alloc` from source for the custom target
- `-Zbuild-std-features=compiler-builtins-mem` for built-in memcpy/memset/etc.

### 8.3 Full Run Sequence (`fullrun.bat`)

```bat
build.bat && python create_disk_image.py && run.bat
```

### 8.4 QEMU Configuration (`run.bat`)

```
qemu-system-x86_64.exe
  -drive if=pflash,format=raw,readonly=on,file="...edk2-x86_64-code.fd"  (OVMF firmware)
  -drive file=disk.img,format=raw,if=ide                                  (disk image)
  -serial stdio                                                            (COM1 to terminal)
  -accel whpx                                                              (WHPX acceleration)
  -m 512M                                                                  (512 MB RAM)
  -smp 2                                                                   (2 CPUs)
```

---

## 9. Appendix: Key Data Structures

### 9.1 BootInfo (at physical address 0x1000)

```rust
#[repr(C)]
pub struct BootInfo {
    pub memory_regions: [MemoryRegion; 128],     // 128 × 16 = 2048 bytes
    pub memory_region_count: usize,              // 8 bytes
    pub framebuffer: FramebufferInfo,            // 40 bytes
    pub partition_zero_lba: u64,                 // 8 bytes
    pub partition_zero_size: u64,                // 8 bytes
    pub kernel_image_addr: u64,                  // 8 bytes
    pub kernel_image_size: u64,                  // 8 bytes
    pub rsdp_addr: u64,                          // 8 bytes
    pub madt_addr: u64,                          // 8 bytes
}

pub struct MemoryRegion {
    pub phys_start: u64,  // 8 bytes
    pub size: u64,        // 8 bytes
}

pub struct FramebufferInfo {
    pub phys_addr: u64,       // 8 bytes
    pub width: usize,         // 8 bytes
    pub height: usize,        // 8 bytes
    pub stride: usize,        // 8 bytes
    pub bytes_per_pixel: usize, // 8 bytes
    pub is_bgr: bool,         // 1 byte (+ 7 padding)
}
```

### 9.2 TrapFrame (on interrupt stack)

```rust
#[repr(C)]
pub struct TrapFrame {
    pub r15: u64,         // offset 0x00
    pub r14: u64,         // offset 0x08
    pub r13: u64,         // offset 0x10
    pub r12: u64,         // offset 0x18
    pub r11: u64,         // offset 0x20
    pub r10: u64,         // offset 0x28
    pub r9: u64,          // offset 0x30
    pub r8: u64,          // offset 0x38
    pub rax: u64,         // offset 0x40
    pub rbx: u64,         // offset 0x48
    pub rcx: u64,         // offset 0x50
    pub rdx: u64,         // offset 0x58
    pub rbp: u64,         // offset 0x60
    pub rsi: u64,         // offset 0x68
    pub rdi: u64,         // offset 0x70
    pub vector: u64,      // offset 0x78 (pushed by stub)
    pub error_code: u64,  // offset 0x80 (pushed by CPU or stub)
    pub rip: u64,         // offset 0x88 (CPU-pushed interrupt frame)
    pub cs: u64,          // offset 0x90
    pub rflags: u64,      // offset 0x98
    pub rsp: u64,         // offset 0xA0 (only on privilege change)
    pub ss: u64,          // offset 0xA8
}
```

### 9.3 GDT

```
Offset 0x00: Null descriptor (all zeros)
Offset 0x08: Kernel code  — base=0, limit=0xFFFFF, access=0x9A (P=1, DPL=0, code, readable), flags=0xA (G=1, L=1)
Offset 0x10: Kernel data  — base=0, limit=0xFFFFF, access=0x92 (P=1, DPL=0, data, writable), flags=0xA (G=1, L=1)
Offset 0x18: User code    — base=0, limit=0xFFFFF, access=0xFA (P=1, DPL=3, code, readable), flags=0xA (G=1, L=1)
Offset 0x20: User data    — base=0, limit=0xFFFFF, access=0xF2 (P=1, DPL=3, data, writable), flags=0xA (G=1, L=1)
Offset 0x28: TSS low      — 16-byte system descriptor
Offset 0x30: TSS high
```

### 9.4 Memory Map

```
Physical Memory Layout:
  0x00000000 - 0x00000FFF:  Reserved (null guard, page 0)
  0x00001000 - 0x00001FFF:  BootInfo pointer (8 bytes at 0x1000, page reserved)
  0x00100000 - 0x001XXXXX:  Kernel binary (.text, .rodata, .data, .bss)
  0x01000000 - 0xXXXXXXXX:  Free memory (managed by buddy allocator, higher-half mapped)
  0xFEE00000 - 0xFEE00FFF:  LAPIC MMIO (2 MB page with PCD, higher-half at 0xFFFF_8000_FEE0_XXXX)
  0xFEC00000 - 0xFECXXXXX:  IOAPIC MMIO (4 KB pages, higher-half, cache-disabled)

Higher-Half Addresses:
  0xFFFF_8000_0000_0000 + phys: All physical memory (data pages, 4 KB + 2 MB)
  0xFFFF_8080_0000_0000 - 0xFFFF_8084_0000_0000: Kernel heap (up to 64 MB)
  0xFFFF_8000_FEE0_XXXX: LAPIC MMIO
  0xFFFF_8000_FEC0_XXXX: IOAPIC MMIO
```

---

## 10. Complete Boot Flow Summary (Timeline)

```
Power On / Reset
  │
  ├── CPU begins executing at 0xFFFFFFF0 (reset vector)
  │   (UEFI firmware / OVMF takes over)
  │
  ├── OVMF Platform Initialization (PEI phase → DXE phase → BDS)
  │   • Enumerates hardware (PCI, memory, CPUs)
  │   • Sets up page tables (identity map, long mode)
  │   • Initializes GOP (Graphics Output Protocol)
  │   • Loads UEFI drivers
  │   • Scans boot options → finds ESP/EFI/BOOT/BOOTX64.EFI
  │
  ├── [0] UEFI loads BOOTX64.EFI
  │   • Loads PE32+ image, relocates
  │   • Allocates UEFI stack
  │   • Transfers control to entry point
  │
  ├── [1] CHAINLOADER (chain/src/main.rs:19)
  │   • 1.1: uefi::helpers::init()
  │   • 1.2: Serial init (chain/src/main.rs:147-157)
  │   • 1.3: Write BootInfo to 0x1000 (chain/src/main.rs:27-46)
  │   • 1.4: Collect memory map (chain/src/main.rs:49-66)
  │   • 1.5: Collect framebuffer (chain/src/main.rs:69-89)
  │   • 1.6: Read Bootloader.efi from ESP (chain/src/main.rs:93-102)
  │   • 1.7: load_image + start_image (chain/src/main.rs:107-125)
  │       → Bootloader.efi now runs
  │
  ├── [2] BOOTLOADER (boot/src/main.rs:22)
  │   • 2.1: uefi::helpers::init()
  │   • 2.2: Serial + logger init (boot/src/main.rs:26-27)
  │   • 2.3: Read BootInfo from 0x1000 (boot/src/main.rs:31-32)
  │   • 2.4: GOP mode set + framebuffer capture (boot/src/main.rs:35-60)
  │   • 2.5: Re-collect memory map (boot/src/main.rs:63-81)
  │   • 2.6: Scan GPT for ext4 partition (boot/src/load_kernel.rs:306-360)
  │   • 2.7: Read ext4 superblock + block groups (boot/src/load_kernel.rs:678-771)
  │   • 2.8: Walk root dir for kernel.elf (boot/src/load_kernel.rs:774-814)
  │   • 2.9: Read kernel.elf via extents (boot/src/load_kernel.rs:468-653)
  │   • 2.10: Capture RSDP (boot/src/main.rs:100-114)
  │   • 2.11: Write BootInfo (boot/src/main.rs:117-119)
  │   • 2.12: exit_boot_services() (boot/src/main.rs:124)
  │       ⚠ Point of no return! UEFI services gone.
  │   • 2.13: cli (boot/src/main.rs:127)
  │   • 2.14: Parse + load ELF segments (boot/src/load_kernel.rs:40-122)
  │   • 2.15: jmp to kernel entry with BootInfo* in RDI (boot/src/main.rs:142-152)
  │
  ├── [3] KERNEL START (kernel/src/main.rs:158)
  │   • 3.1: Serial + logger init (kernel/src/main.rs:163-164)
  │   • 3.2: Extract memory regions, calculate free MB (kernel/src/main.rs:169-183)
  │   • 3.3: Init framebuffer, draw splash (kernel/src/main.rs:186-191)
  │   • 3.4: Physical allocator init (kernel/src/main.rs:195, src/mm/phys.rs:94)
  │   • 3.5: ACPI discovery / MADT parse (kernel/src/main.rs:200-237, src/acpi/)
  │   • 3.6: Page tables init → CR3 switch (kernel/src/main.rs:243, src/mm/virt.rs:50)
  │       ⚠ Virtual address space now active
  │       ⚠ Framebuffer pointer updated to higher-half
  │   • 3.7: Heap allocator init (kernel/src/main.rs:252, src/mm/heap.rs:67)
  │   • 3.8: cli + mask PIC (kernel/src/main.rs:258-261, src/arch/idt.rs:342)
  │   • 3.9: LAPIC MMIO map (kernel/src/main.rs:268, src/arch/apic.rs:92)
  │   • 3.10: IOAPIC init (kernel/src/main.rs:272, src/arch/ioapic.rs:148)
  │   • 3.11: Interrupt routing table init (kernel/src/main.rs:274, src/intr/mod.rs:84)
  │   • 3.12: Draw status screen (kernel/src/main.rs:279-323)
  │   • 3.13: GDT + TSS load (kernel/src/main.rs:325, src/arch/gdt.rs:190)
  │   • 3.14: IDT init (kernel/src/main.rs:330, src/arch/idt.rs:351)
  │   • 3.15: Task manager init (kernel/src/main.rs:335-336, src/task.rs:47-92)
  │   • 3.16: Create test tasks (kernel/src/main.rs:339-348, src/task.rs:134)
  │   • 3.17: Install IOAPIC routes (kernel/src/main.rs:352, src/intr/mod.rs:254)
  │   • 3.18: Enable LAPIC (kernel/src/main.rs:358, src/arch/apic.rs:120)
  │   • 3.19: Calibrate LAPIC timer against PIT (kernel/src/main.rs:361, src/arch/apic.rs:202)
  │   • 3.20: Configure LAPIC timer 1ms periodic (kernel/src/main.rs:363-365)
  │   • 3.21: Enable PIT 100 Hz (kernel/src/main.rs:367, src/arch/apic.rs:279)
  │   • 3.22: sti + int 32 test (kernel/src/main.rs:369-373)
  │   • 3.23: Unmask PIT + keyboard routes (kernel/src/main.rs:376-390)
  │
  ├── [4] IDLE LOOP (kernel/src/main.rs:400-417)
  │   • hlt until next interrupt
  │   • Every ~1 second: log tick/PIT/keyboard/task stats
  │   • Preemptive scheduling runs (task switching on LAPIC timer IRQ)
  │
  └── [∞] System running
      • Task 0 (idle): hlt loop
      • Task 1 (simple_task1): busy-loop counter
      • Task 2 (simple_task2): busy-loop counter
      • LAPIC timer fires every 1 ms → preemptive context switch
      • PIT fires at 100 Hz → increments PIT tick counter
      • Keyboard IRQ on keypress → stores scancode
```

## 11. Error Handling and Edge Cases

### 11.1 Boot Chain Failures

| Failure Point | Behavior |
|---|---|
| ESP path not found | OVMF enters UEFI shell or reports `Boot0001` failure |
| Chainloader not valid PE32+ | UEFI `load_image` returns error, chainloader's `start_image` returns error status |
| ext4 partition not found | `find_ext4_partition()` returns None, bootloader returns `LOAD_ERROR` |
| kernel.elf not found in root dir | `load_kernel_from_ext4()` returns None, bootloader returns `LOAD_ERROR` |
| ELF parsing fails | `load_elf()` returns None, bootloader calls `halt()` (infinite `cli; hlt`) |
| Kernel segment > 128 MB | `load_elf()` returns None, bootloader halts |
| MADT not found | IOAPIC init skipped, kernel runs without interrupt controllers |

### 11.2 Kernel Initialization Failures

| Failure Point | Behavior |
|---|---|
| Physical allocator init | `init_from_regions()` carves buddy blocks; panics if no free regions exist |
| Page table allocation | `alloc_page()` panics on OOM during PT creation |
| Heap allocation | `init()` silently stops if no physical pages available |
| ACPI RSDP not found | `acpi::init()` panics with "ACPI: RSDP not found" |
| TSS address non-canonical | `assert!` panics in `gdt::load()` |

### 11.3 Exception Handling

| Exception | Handler Action |
|---|---|
| #DE (0) | Log, dump registers, halt |
| #DB (1) | Log, dump registers, halt |
| #BP (3) | Log, return (continues execution) |
| #UD (6) | Log, dump registers, halt |
| #DF (8) | Log, dump registers, halt (IST1 stack) |
| #GP (13) | Log (with selector decode), dump registers, halt |
| #PF (14) | Log CR2 + error code bits, dump registers, halt |
| Others | Log vector + RIP + registers, halt |

### 11.4 Initialization Guards

Every subsystem has an `INITIALIZED` atomic flag (or similar) that prevents double-initialization:
- `mm::phys::INITIALIZED` (`phys.rs:86`)
- `mm::virt::PT_INITIALIZED` (`virt.rs:19`)
- `mm::heap::ALLOCATOR.initialized` (`heap.rs:58`)
- `arch::apic::INITIALIZED` (`apic.rs:39`)
- `arch::ioapic::INITIALIZED` (`ioapic.rs:144`)
- `intr::TABLE.initialized` (`intr.rs:62`)
- `task::MANAGER.initialized` (`task.rs:40`)
