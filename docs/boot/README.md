# LodaxOS Bootloader (Stage 2)

**Crate:** `boot/` — UEFI application

The bootloader is the second-stage boot component, loaded and started by the chainloader. It receives a partially filled `BootInfo` (memory map and framebuffer from stage 1), then:

1. Re-initialises the framebuffer (GOP with preferred resolution).
2. Reads `kernel.elf` and `drivers.elf` from an ext4 partition via raw UEFI Block I/O.
3. Loads ELF64 segments into physical memory.
4. Captures the ACPI RSDP from UEFI configuration tables.
5. Enumerates APs via UEFI MP Services.
6. Calls `ExitBootServices`, then long-jumps to the kernel entry point.

---

## Module Map

| Module | File | Responsibility |
|--------|------|----------------|
| `main` | `boot/src/main.rs` | Orchestration, BootInfo update, ExitBootServices, kernel jump |
| `load_kernel` | `boot/src/load_kernel.rs` | GPT scan, ext4 filesystem driver, ELF64 loader |
| `mp` | `boot/src/mp.rs` | UEFI MP Services AP enumeration |
| `serial` | `boot/src/serial.rs` | COM1 UART 16550 driver |
| `logger` | `boot/src/logger.rs` | `log` facade over serial |

---

## 1. Startup and Handoff (`boot/src/main.rs`)

The bootloader entry point reads the `BootInfo` pointer from `BOOT_INFO_HANDOFF_ADDR` (`0x5000`):

```rust
let boot_info_addr = unsafe { *(BOOT_INFO_HANDOFF_ADDR as *const u64) };
let boot_info_ptr = boot_info_addr as *mut BootInfo;
let mut boot_info = unsafe { *boot_info_ptr };
```

Initialises serial (`serial::init()`), then the `log` facade (`logger::init()`).

---

## 2. Framebuffer (`boot/src/main.rs:67`)

Opens `GraphicsOutput`, then:

1. **First pass:** looks for a mode with exactly 1024x768 resolution.
2. **Second pass:** if no exact match, picks the mode with the highest pixel count.
3. Sets the chosen mode (or warns on failure).
4. Populates `boot_info.framebuffer`:

```rust
boot_info.framebuffer = FramebufferInfo {
    phys_addr: ptr,
    width: w, height: h, stride,
    bytes_per_pixel: 4,
    is_bgr,
};
```

---

## 3. Kernel Loading from ext4 (`boot/src/main.rs:120`)

```rust
let kernel_elf_data = match load_kernel::load_kernel_from_ext4() {
    Some(data) => data,
    None => { ... return Status::LOAD_ERROR; }
};
boot_info.kernel_image_addr = kernel_elf_data.as_ptr() as u64;
boot_info.kernel_image_size = kernel_elf_data.len() as u64;
```

`load_kernel_from_ext4` (`boot/src/load_kernel.rs:1054`) is a convenience wrapper that calls `load_file_from_ext4(b"kernel.elf")`.

The raw kernel ELF data is kept in UEFI-allocated memory (the "staging buffer"). **After `ExitBootServices`, the UEFI allocator is defunct** — the kernel must copy the image or mark this region as reserved.

---

## 4. Drivers Loading (`boot/src/main.rs:138`)

```rust
let drivers_elf_data = load_kernel::load_file_from_ext4(b"drivers.elf");
```

Same ext4 read path. If found, the address and size are stored in `boot_info.drivers_elf_addr` / `boot_info.drivers_elf_size` and the buffer is leaked. If not found, the bootloader continues with zeros in those fields.

---

## 5. ACPI Discovery (`boot/src/main.rs:153`)

Walks the UEFI configuration table looking for `ACPI2_GUID` (RSDP v2) or `ACPI_GUID` (RSDP v1):

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

The address is captured **before** `ExitBootServices` — UEFI virtual addressing is not used.

---

## 6. AP Enumeration (`boot/src/main.rs:181`)

Calls `mp::enumerate_aps(&mut boot_info)`. See [MP Services (`boot/src/mp.rs`)](#mp-services-bootsrcmprs) for details.

The bootloader does **not** start APs — it only collects LAPIC IDs. The kernel handles INIT-SIPI-SIPI via its own real-mode trampoline after `ExitBootServices`.

---

## 7. Memory Region Collection (`boot/src/main.rs:23`)

As in the chainloader, iterates the UEFI memory map for `CONVENTIONAL` entries. This is called a **third** time (after all bootloader allocations) to produce the final, accurate usable map:

```rust
fn collect_usable_memory_regions(boot_info: &mut BootInfo) {
    boot_info.memory_regions = [MemoryRegion { phys_start: 0, size: 0 }; MAX_MEMORY_REGIONS];
    boot_info.memory_region_count = 0;
    if let Ok(memory_map) = uefi::boot::memory_map(MemoryType::LOADER_DATA) {
        ...
    }
}
```

---

## 8. ELF Loading (`boot/src/main.rs:199`)

```rust
let entry = match load_kernel::load_elf(
    &kernel_elf_data,
    &boot_info.memory_regions,
    boot_info.memory_region_count,
) {
    Some(addr) => addr,
    None => { ... halt(); }
};
```

See [ELF64 Loader (`boot/src/load_kernel.rs`)](#elf64-loader-bootsrcload_kernelrs) for details.

---

## 9. ExitBootServices and Kernel Jump (`boot/src/main.rs:209`)

```rust
let _mmap = unsafe { uefi::boot::exit_boot_services(None) };
unsafe { core::arch::asm!("cli") };
```

Then the long jump to the kernel:

```rust
unsafe {
    core::arch::asm!(
        "and rsp, -16",
        "sub rsp, 8",
        "mov rdi, {boot_info}",
        "jmp {entry}",
        boot_info = in(reg) boot_info_addr,
        entry = in(reg) entry,
        options(noreturn)
    );
}
```

- `RSP` is 16-byte aligned, then decremented by 8 so that at kernel entry `RSP mod 16 = 8` (SysV AMD64 ABI: before a call, RSP is 16-byte aligned; after the call, RSP mod 16 = 8).
- `RDI` = `BootInfo` physical address.
- `JMP` (not `CALL`) — the kernel never returns.

---

## ELF64 Loader (`boot/src/load_kernel.rs`)

### Constants (`boot/src/load_kernel.rs:18`)

| Name | Value | Description |
|------|-------|-------------|
| `ELF_MAGIC` | `[0x7f, 'E', 'L', 'F']` | ELF identification magic |
| `EI_CLASS` | `4` | Offset of `ei_class` byte in `e_ident` |
| `ELFCLASS64` | `2` | 64-bit ELF class value |
| `ET_EXEC` | `2` | Executable type |
| `PT_LOAD` | `1` | Loadable segment type |
| `KERNEL_LOAD_PHYS_LIMIT` | `0x4000_0000` | Max physical address for segments (1 GiB) |

### ELF Header Field Offsets

| Constant | Offset | Size | Field |
|----------|--------|------|-------|
| `E_IDENT` | `0` | `16` | `e_ident[16]` |
| `E_TYPE` | `16` | `2` | `e_type` |
| `E_ENTRY` | `24` | `8` | `e_entry` |
| `E_PHOFF` | `32` | `8` | `e_phoff` |
| `E_PHENTSIZE` | `54` | `2` | `e_phentsize` |
| `E_PHNUM` | `56` | `2` | `e_phnum` |

### Program Header Field Offsets

| Constant | Offset | Size | Field |
|----------|--------|------|-------|
| `P_TYPE` | `0` | `4` | `p_type` |
| `P_FLAGS` | `4` | `4` | `p_flags` |
| `P_OFFSET` | `8` | `8` | `p_offset` |
| `P_VADDR` | `16` | `8` | `p_vaddr` |
| `P_PADDR` | `24` | `8` | `p_paddr` |
| `P_FILESZ` | `32` | `8` | `p_filesz` |
| `P_MEMSZ` | `40` | `8` | `p_memsz` |

### `load_elf()` (`boot/src/load_kernel.rs:58`)

Signature:

```rust
pub fn load_elf(
    data: &[u8],
    memory_regions: &[super::MemoryRegion],
    region_count: usize,
) -> Option<u64>
```

Returns the kernel entry point on success, `None` on failure.

Validation steps:

1. **Minimum size:** `data.len() >= 64` (ELF header minimum).
2. **Magic:** `data[0..4] == ELF_MAGIC`.
3. **Class:** `data[EI_CLASS] == ELFCLASS64 (2)`.
4. **Endianness:** `data[5] == 1` (little-endian).
5. **Type:** `e_type == ET_EXEC (2)`.
6. **Program header bounds:** `phoff + phnum * phentsize <= data.len()`.

For each program header where `p_type == PT_LOAD`:

```
seg_end = paddr.checked_add(memsz).unwrap_or(u64::MAX)
```

- **Physical limit check:** `seg_end <= KERNEL_LOAD_PHYS_LIMIT (0x4000_0000)` — segments must land within the first 1 GiB of physical memory (staying well within UEFI's identity-mapped 4 GiB).
- **Memory region validation:** `addr_in_any_region(paddr, memsz, memory_regions, region_count)` — the segment must be entirely contained within a single `MemoryRegion` entry.
- **Data bounds:** `seg_offset + filesz <= data.len()` — the segment source data fits in the buffer.

Loading:

```rust
let dst = paddr as *mut u8;
unsafe {
    core::ptr::copy_nonoverlapping(data[seg_offset..].as_ptr(), dst, filesz);
    if memsz > filesz {
        core::ptr::write_bytes(dst.add(filesz), 0, memsz - filesz);
    }
}
```

- Program header data is copied to `paddr` (identity-mapped by UEFI).
- The remaining `memsz - filesz` bytes are zeroed (`.bss`).

---

## ext4 Filesystem Driver (`boot/src/load_kernel.rs`)

The ext4 driver implements a read-only ext4 filesystem over UEFI `BlockIO` protocol. It supports:

- GPT partition table scanning
- ext4 superblock parsing (rev 0/1, 32-bit and 64-bit block group descriptors)
- Extent-based file reads (depth 0, 1+)
- Legacy indirect block fallback (single indirect)
- Ext4 directory walking
- Block size up to 4096 bytes

### On-Disk Structures (`boot/src/load_kernel.rs:181`)

#### Superblock (`#[repr(C, packed)]`, at byte 1024)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| `+0x00` | `4` | `s_inodes_count` | Total inode count |
| `+0x04` | `4` | `s_blocks_count_lo` | Total block count (low 32) |
| `+0x08` | `4` | `s_r_blocks_count_lo` | Reserved block count |
| `+0x0C` | `4` | `s_free_blocks_count_lo` | Free block count |
| `+0x10` | `4` | `s_free_inodes_count` | Free inode count |
| `+0x14` | `4` | `s_first_data_block` | First data block (0 or 1) |
| `+0x18` | `4` | `s_log_block_size` | Log2(block_size / 1024) |
| `+0x1C` | `4` | `s_log_cluster_size` | Log2(cluster_size / 1024) |
| `+0x20` | `4` | `s_blocks_per_group` | Blocks per block group |
| `+0x24` | `4` | `s_clusters_per_group` | Clusters per group |
| `+0x28` | `4` | `s_inodes_per_group` | Inodes per group |
| `+0x2C` | `4` | `s_mtime` | Mount time |
| `+0x30` | `4` | `s_wtime` | Write time |
| `+0x34` | `2` | `s_mnt_count` | Mount count |
| `+0x36` | `2` | `s_max_mnt_count` | Max mounts before check |
| `+0x38` | `2` | `s_magic` | Magic `0xEF53` |
| `+0x3A` | `2` | `s_state` | Filesystem state |
| `+0x3C` | `2` | `s_errors` | Error handling mode |
| `+0x3E` | `2` | `s_minor_rev_level` | Minor revision |
| `+0x40` | `4` | `s_lastcheck` | Last check time |
| `+0x44` | `4` | `s_checkinterval` | Check interval |
| `+0x48` | `4` | `s_creator_os` | Creator OS |
| `+0x4C` | `4` | `s_rev_level` | Revision level (0=orig, 1=v2) |
| `+0x50` | `2` | `s_def_resuid` | Default reserved UID |
| `+0x52` | `2` | `s_def_resgid` | Default reserved GID |
| `+0x54` | `4` | `s_first_ino` | First non-reserved inode |
| `+0x58` | `2` | `s_inode_size` | Inode struct size (rev >= 1) |
| `+0x5A` | `2` | `s_block_group_nr` | Block group number of this superblock copy |
| `+0x5C` | `4` | `s_feature_compat` | Compatible features |
| `+0x60` | `4` | `s_feature_incompat` | Incompatible features |
| `+0x64` | `4` | `s_feature_ro_compat` | Read-only compatible features |

Key constants:

| Name | Value |
|------|-------|
| `SB_MAGIC` | `0xEF53` |
| `SB_OFFSET` | `1024` |
| `EXT4_FEATURE_RO_COMPAT_64BIT` | `0x80` |
| `EXT4_64BIT_BGDT_SIZE` | `64` |
| `EXT4_32BIT_BGDT_SIZE` | `32` |

Block size formula: `block_size = 1024 << s_log_block_size` (`boot/src/load_kernel.rs:497`).

#### Block Group Descriptor (`#[repr(C, packed)]`)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| `+0x00` | `4` | `bg_block_bitmap_lo` | Block bitmap block (low 32) |
| `+0x04` | `4` | `bg_inode_bitmap_lo` | Inode bitmap block (low 32) |
| `+0x08` | `4` | `bg_inode_table_lo` | Inode table block (low 32) |
| `+0x0C` | `2` | `bg_free_blocks_count_lo` | Free block count |
| `+0x0E` | `2` | `bg_free_inodes_count_lo` | Free inode count |
| `+0x10` | `2` | `bg_used_dirs_count_lo` | Used directory count |
| `+0x12` | `2` | `bg_flags` | Flags |
| `+0x14` | `4` | `bg_exclude_bitmap_lo` | Exclude bitmap |
| `+0x18` | `2` | `bg_block_bitmap_csum_lo` | Block bitmap checksum |
| `+0x1A` | `2` | `bg_inode_bitmap_csum_lo` | Inode bitmap checksum |
| `+0x1C` | `2` | `bg_itable_unused_lo` | Unused inode count |
| `+0x1E` | `2` | `bg_checksum` | Group checksum |

For 64-bit filesystems (`s_feature_ro_compat & 0x80`), additional high-32 fields are read from bytes `+0x20`–`+0x2B`:

| Offset | Size | Field |
|--------|------|-------|
| `+0x20` | `4` | `bg_block_bitmap_hi` |
| `+0x28` | `4` | `bg_inode_table_hi` |

The BGDT starts at block 2 (if `s_log_block_size == 0`, i.e. 1024-byte blocks) or block 1 (larger blocks).

#### Inode (`#[repr(C, packed)]`, 128 bytes for rev >= 1)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| `+0x00` | `2` | `i_mode` | File mode |
| `+0x02` | `2` | `i_uid` | Owner UID |
| `+0x04` | `4` | `i_size_lo` | File size low 32 |
| `+0x08` | `4` | `i_atime` | Access time |
| `+0x0C` | `4` | `i_ctime` | Creation time |
| `+0x10` | `4` | `i_mtime` | Modification time |
| `+0x14` | `4` | `i_dtime` | Deletion time |
| `+0x18` | `2` | `i_gid` | Group ID |
| `+0x1A` | `2` | `i_links_count` | Hard link count |
| `+0x1C` | `4` | `i_blocks_lo` | Block count (low 32) |
| `+0x20` | `4` | `i_flags` | Inode flags |
| `+0x24` | `4` | `i_osd1` | OS-dependent field 1 |
| `+0x28` | `60` | `i_block[15]` | Block pointers / extent tree root |
| `+0x64` | `4` | `i_generation` | File generation |
| `+0x68` | `4` | `i_file_acl_lo` | Extended attribute block |
| `+0x6C` | `4` | `i_size_high` | File size high 32 |

`i_block[15]` at offset `+0x28` is interpreted as the **extent tree root** when `i_flags & EXT4_EXTENTS_FL` is set (`EXT4_EXTENTS_FL = 0x80000`). Otherwise it contains direct/indirect block pointers:

- `i_block[0..11]` — direct blocks
- `i_block[12]` — single indirect
- `i_block[13]` — double indirect
- `i_block[14]` — triple indirect

File size (64-bit): `(i_size_high << 32) | i_size_lo`

#### Directory Entry (`#[repr(C, packed)]`)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| `+0x00` | `4` | `inode` | Inode number (0 = unused) |
| `+0x04` | `2` | `rec_len` | Directory entry length |
| `+0x06` | `1` | `name_len` | File name length |
| `+0x07` | `1` | `file_type` | File type indicator |

The file name follows immediately after `file_type` (at offset `+0x08`) for `name_len` bytes.

#### Extent Entry (`struct`, `boot/src/load_kernel.rs:273`)

| Field | Size | Description |
|-------|------|-------------|
| `ee_block` | `4` | First logical block number |
| `ee_len` | `4` | Number of blocks (unwrapped: `raw_len & 0x7FFF`) |
| `ee_start` | `8` | Physical block number (hi 16 from offset `+6`, lo 32 from offset `+8`) |

#### Extent Header (at start of every extent block, including the root in `i_block`)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| `+0x00` | `2` | `eh_magic` | `0xF30A` |
| `+0x02` | `2` | `eh_entries` | Number of entries |
| `+0x04` | `2` | `eh_max` | Max entries |
| `+0x06` | `2` | `eh_depth` | Tree depth (0 = leaf) |
| `+0x08` | `4` | `eh_generation` | Generation |

At depth 0, entries are leaf `ExtentEntry` (12 bytes each). At depth > 0, entries are index nodes:

| Offset | Size | Index entry field |
|--------|------|------------------|
| `+0x00` | `4` | `ei_block` (logical block covered by child) |
| `+0x04` | `4` | `ei_leaf_lo` (child block number low 32) |
| `+0x08` | `2` | `ei_leaf_hi` (child block number high 16) |
| `+0x0A` | `2` | `ei_unused` |

---

### SectorReader (`boot/src/load_kernel.rs:286`)

A wrapper around UEFI `BlockIO` with a direct-mapped sector cache.

```rust
struct SectorReader {
    proto: ScopedProtocol<BlockIO>,
    block_size: u32,
    media_id: u32,
    partition_offset: u64,
    cache_sectors: [u64; CACHE_SIZE],    // 8 entries
    cache_data: [[u8; SECTOR_SIZE]; CACHE_SIZE],  // 8 * 512 bytes
}
```

- **Cache size:** `CACHE_SIZE = 8` entries.
- **Index:** `idx = sector % CACHE_SIZE` (direct-mapped).
- **Hit:** `cache_sectors[idx] == sector` — returns immediately.
- **Miss:** reads via `proto.read_blocks()`, handles block size alignment automatically.

`SECTOR_SIZE = 512` (`boot/src/load_kernel.rs:8`).

**`read_sector()`** (`boot/src/load_kernel.rs:312`):

- Computes absolute sector: `partition_offset + sector`.
- Converts byte offset to device-native blocks.
- If `block_size >= 512`: reads one block, copies 512 bytes from `byte_offset % block_size`.
- If `block_size < 512`: reads `ceil(512 / block_size)` blocks.

**`read_ext4_sectors()`** (`boot/src/load_kernel.rs:423`):

- Multi-sector version of `read_sector()`.
- When `block_size == SECTOR_SIZE`: reads aligned blocks directly.
- Otherwise: reads per-sector, handling misalignment.

---

### GPT Partition Scan (`boot/src/load_kernel.rs:340`)

`find_ext4_partition()` (`boot/src/load_kernel.rs:407`):

1. Enumerate all UEFI handles with `BlockIO` protocol.
2. For each handle, call `try_handle_for_ext4()`.
3. Return the first handle + partition LBA where an ext4 partition is found.

`try_handle_for_ext4()` (`boot/src/load_kernel.rs:366`):

1. Read LBA 0 (protective MBR — not validated, just consumed to advance).
2. Read LBA 1 (GPT header).
3. Verify signature `"EFI PART"` at bytes 0..7.
4. Read GPT header fields at:

   | Offset | Size | Field |
   |--------|------|-------|
   | `+72` | `8` | `entries_lba` (partition entry array start) |
   | `+80` | `4` | `num_entries` (number of entries) |
   | `+84` | `4` | `entry_size` (bytes per entry) |

5. Compute `entries_per_sector = SECTOR_SIZE / entry_size`.
6. Iterate sectors of the partition entry array.
7. For each entry, compare the 16-byte partition type GUID against `EXT4_GUID`:

   ```
   EXT4_GUID = [0x3DAF, 0x0FC6, 0x8483, 0x4772,
                0x798E, 0x693D, 0x47D8, 0xE47D]
   ```

8. On match, read `first_lba` (at offset `+32`, 8 bytes) and `last_lba` (at offset `+40`, 8 bytes).

---

### ext4 Read Path

`load_file_from_ext4(filename)` (`boot/src/load_kernel.rs:867`):

```
load_file_from_ext4(filename: &[u8]) -> Option<Vec<u8>>
```

The full call chain:

```
load_file_from_ext4
  +-- find_ext4_partition()              -> (handle, partition_lba)
  +-- SectorReader::new(handle, partition_lba)
  +-- read superblock (sector = 1024/512 = 2)
  |   +-- s_rev_level, s_inode_size, s_feature_ro_compat
  |   +-- s_magic == 0xEF53
  |   +-- block_size = 1024 << s_log_block_size
  +-- compute num_groups = ceil(s_blocks_count_lo / s_blocks_per_group)
  +-- read Block Group Descriptor Table
  |   +-- bgdt_block = (s_log_block_size == 0) ? 2 : 1
  |   +-- bgdt_sector = bgdt_block * block_size / 512
  |   +-- bgdt_entry_size = (s_feature_ro_compat & 0x80) ? 64 : 32
  |   +-- parse per-group: bg_inode_table_lo (with hi bits if 64-bit)
  +-- read_inode(&reader, &sb, &descs, 2)   -> root inode
  |   +-- inode_table_sector = bg_inode_table_lo * block_size / 512
  |   +-- group = (inode_num - 1) / inodes_per_group
  |   +-- index = (inode_num - 1) % inodes_per_group
  |   +-- inode_offset = index * inode_size
  +-- read root directory data via read_file_into
  +-- scan directory entries for filename:
  |   +-- DirEntry { inode, rec_len, name_len, file_type }
  |   +-- name at offset +8 for name_len bytes
  |   +-- entry with inode == 0 -> deleted/unused, skip
  |   +-- entry with rec_len == 0 -> corrupt, abort
  +-- read_inode(&reader, &sb, &descs, file_inode_num)  -> file inode
  +-- read_file(&reader, &sb, &inode)   -> Vec<u8>
      +-- file_size = (i_size_high << 32) | i_size_lo
      +-- read_file_into(reader, sb, inode, &mut data)
          +-- if i_flags & EXT4_EXTENTS_FL -> parse_extents
          |   +-- iterate extents, read_ext4_sectors per extent
          +-- else (indirect): resolve_block + read_data_block per block
```

#### `read_inode()` (`boot/src/load_kernel.rs:468`)

Parameters: `reader`, `sb`, `block_group_descs[]`, `inode_num`.

- `inode_size = (s_rev_level >= 1 && s_inode_size != 0) ? s_inode_size : 128`
- `group = (inode_num - 1) / s_inodes_per_group`
- `index = (inode_num - 1) % s_inodes_per_group`
- `inode_table_sector = desc.bg_inode_table_lo * block_size / SECTOR_SIZE`
- `inode_offset = index * inode_size`
- Reads raw bytes from the inode table, then manually deserialises all fields (not a `#[repr(C, packed)]` cast — raw bytes are copied into a `Vec` then parsed field by field).

#### Extent parsing

`parse_extents()` (`boot/src/load_kernel.rs:607`):

1. Copy raw 60 bytes of `i_block[15]` into a local buffer.
2. Read extent header magic `0xF30A` at offset 0.
3. If `eh_depth == 0` (leaf): iterate up to `eh_entries` entries from offset 12.
4. If `eh_depth > 0` (index): for each entry, recurse into `collect_leaf_extents()` to descend.

`collect_leaf_extents()` (`boot/src/load_kernel.rs:560`):

- Reads an extent block (up to 4096 bytes) via `read_extent_block()`.
- At depth 0: collect `ExtentEntry` values (12 bytes each from offset 12).
- At depth > 0: recurse into child index blocks.

`resolve_block_extents()` (`boot/src/load_kernel.rs:663`):

- Single-block resolution (for indirect fallback path): descends the extent tree to find the physical block for a given `logical_block`.

#### Extent-to-sector mapping

An `ExtentEntry` covers:
- Logical block range: `[ee_block, ee_block + ee_len - 1]`
- Physical start block: `ee_start`
- Sector address for I/O: `ee_start * sectors_per_block` where `sectors_per_block = block_size / SECTOR_SIZE`.

#### Legacy indirect blocks (`boot/src/load_kernel.rs:775`)

When `i_flags & EXT4_EXTENTS_FL == 0` and `logical_block >= 12`:

- Read the single-indirect block at `inode.i_block[12]`.
- Treat the indirect block as an array of `block_size / 4` little-endian `u32` block pointers.
- Index: `indirect_idx = logical_block - 12`.
- Read the pointed-to block with `read_data_block()`.

Direct blocks `i_block[0..11]` are used for logical blocks 0-11.

---

## Serial Driver (`boot/src/serial.rs`)

COM1 UART 16550 via port I/O.

### Initialisation (`boot/src/serial.rs:3`)

| Port | Value | Purpose |
|------|-------|---------|
| `COM1 + 3` (`0x3FB`) | `0x80` | Set DLAB |
| `COM1 + 0` (`0x3F8`) | `0x01` | Divisor low = 1 (115200 baud) |
| `COM1 + 1` (`0x3F9`) | `0x00` | Divisor high = 0 |
| `COM1 + 3` (`0x3FB`) | `0x03` | 8N1, DLAB cleared |
| `COM1 + 2` (`0x3FA`) | `0xC7` | FIFO: enable, clear, 14-byte threshold |
| `COM1 + 4` (`0x3FC`) | `0x0B` | MCR: DTR=1, RTS=1, OUT2=1 |

### Write (`boot/src/serial.rs:19`)

`write_byte(byte)` spin-waits on LSR bit 5 (THR empty) with a `0xFFFFFF`-iteration timeout, then writes to `COM1 + 0`.

`write_str(s)` writes each byte; `\n` is expanded to `\r\n`.

All inline assembly uses `options(nostack, nomem)` for optimisation.

---

## Logger (`boot/src/logger.rs`)

A `log::Log` implementation that formats output as:

```
[LEVEL ] target: message
```

Levels are padded to 5 characters:

| Level | Output |
|-------|--------|
| `Error` | `ERROR` |
| `Warn` | `WARN ` |
| `Info` | `INFO ` |
| `Debug` | `DEBUG` |
| `Trace` | `TRACE` |

Uses `core::fmt::Write` for format-string support. Initialised with `LevelFilter::Trace`.

---

## MP Services (`boot/src/mp.rs`)

`enumerate_aps(boot_info)` reads the UEFI `MpServices` protocol.

```rust
pub fn enumerate_aps(boot_info: &mut BootInfo) -> uefi::Result<()>
```

1. `mp.get_number_of_processors()` -> `{ total, enabled }`.
2. `to_record = min(enabled, MAX_CPUS)`.
3. `ap_slots = MAX_CPUS - 1` (one slot reserved conceptually for BSP).
4. For each processor from `0..total`:
   - `mp.get_processor_info(proc_num)` -> status.
   - Skip if not `is_enabled()` or not `is_healthy()`.
   - If `is_bsp()`: record `boot_info.bsp_apic_id`.
   - Else: store LAPIC ID in `boot_info.ap_apic_ids[ap_index]`, increment `ap_index`.
5. `boot_info.ap_count = ap_index`.

Does **not** start APs — the kernel uses LAPIC INIT-SIPI-SIPI.

---

## Execution Flow Summary

```
Chainloader (chain/)
  +-- Box::new BootInfo, pointer -> [0x5000]
  +-- Read Bootloader.efi from ESP
  +-- LoadImage + StartImage(Bootloader.efi)
      |
      v
Bootloader (boot/src/main.rs)
  +-- Read BootInfo pointer from [0x5000]
  +-- serial::init()
  +-- logger::init()
  +-- GOP: set 1024x768 (or highest), populate boot_info.framebuffer
  +-- load_kernel::load_kernel_from_ext4()
  |   +-- find_ext4_partition() -> scan GPT for ext4 GUID
  |   +-- load_file_from_ext4("kernel.elf")
  |       +-- read superblock (byte 1024)
  |       +-- parse BGDT
  |       +-- read root inode -> walk directory -> find kernel.elf
  |       +-- read file via extents -> Vec<u8>
  +-- boot_info.kernel_image_addr/size = staging buffer
  +-- load_kernel::load_file_from_ext4("drivers.elf")
  |   +-- boot_info.drivers_elf_addr/size
  +-- Capture RSDP from UEFI config table
  +-- Write BootInfo back to [boot_info_ptr]
  +-- mp::enumerate_aps()
  |   +-- collect LAPIC IDs from UEFI MP Services
  +-- Write BootInfo back
  +-- collect_usable_memory_regions()
  |   +-- final UEFI memory map -> boot_info.memory_regions[]
  +-- Write BootInfo back
  +-- load_kernel::load_elf(kernel_elf_data, memory_regions)
  |   +-- validate ELF64 header
  |   +-- for each PT_LOAD segment:
  |       +-- check addr < KERNEL_LOAD_PHYS_LIMIT (1 GiB)
  |       +-- check within memory region
  |       +-- copy_nonoverlapping to paddr
  |       +-- zero .bss
  |   +-- return entry point
  +-- ExitBootServices
  +-- CLI
  +-- JMP kernel(entry) with RDI = BootInfo address
```
