# lodaxos-system

A `#![no_std]` library crate that defines shared types, constants, and data structures used across all other LodaxOS crates. It has **zero dependencies**.

---

## Constants

| Name | Type | Value | Description |
|---|---|---|---|
| `MAX_MEMORY_REGIONS` | `usize` | `128` | Maximum number of memory-map entries the bootloader can report. |
| `MAX_CPUS` | `usize` | `4` | Maximum number of CPUs the kernel supports bringing up. |
| `MAX_DRIVER_PKG_ENTRIES` | `usize` | `32` | Maximum number of drivers that can be bundled in a single driver package. |
| `BOOT_INFO_HANDOFF_ADDR` | `u64` | `0x5000` | Fixed physical address where the chainloader stores an 8-byte pointer to the dynamically allocated `BootInfo` struct. |
| `DRIVER_PKG_MAGIC` | `[u8; 8]` | `*b"LODAXPKG"` | Magic bytes that identify the driver package file format. |

### `BOOT_INFO_HANDOFF_ADDR` Rationale

`0x5000` is chosen to avoid:
- Real-mode IVT (`0x0` – `0x3FF`)
- BDA (`0x400` – `0x4FF`)
- Typical EBDA range

while staying below the 1 MB mark for easy identity-map access during early boot.

---

## Framebuffer Driver Commands

The framebuffer driver uses a channel-based command interface. Each command is encoded as a `u32` opcode.

| Constant | Value | Description |
|---|---|---|
| `FB_CMD_ACQUIRE` | `0xFF` | Acquire exclusive access to the framebuffer. |
| `FB_CMD_SHOW_TEXT` | `1` | Display a text string on the framebuffer. |
| `FB_CMD_CLEAR` | `2` | Clear the framebuffer (fill with background color). |
| `FB_CMD_SET_PIXEL` | `3` | Set the color of a single pixel at (x, y). |
| `FB_CMD_FILL_RECT` | `4` | Fill a rectangular region with a solid color. |
| `FB_CMD_DRAW_TEXT` | `5` | Draw a text string at a specific (x, y) position. |
| `FB_CMD_SET_FG` | `6` | Set the foreground text color. |
| `FB_CMD_SET_BG` | `7` | Set the background text color. |
| `FB_CMD_SCROLL` | `8` | Scroll the visible framebuffer contents by a given offset. |
| `FB_CMD_GET_INFO` | `9` | Query the framebuffer's current mode information. |
| `FB_CMD_PRESENT` | `10` | Present (flip / blit) the back buffer to the display. |

---

## Struct Layouts

All structs are annotated with `#[repr(C)]`, `#[derive(Debug, Clone, Copy)]`.

### `FramebufferInfo` — 48 bytes

Describes a UEFI GOP framebuffer mode.

```
Offset  Size  Type    Field             Description
------  ----  ------  ----------------  -----------------------------------------
     0     8  u64     phys_addr         Physical base address of the framebuffer.
     8     8  usize   width             Width in pixels.
    16     8  usize   height            Height in pixels.
    24     8  usize   stride            Pixels per scanline (may exceed width).
    32     8  usize   bytes_per_pixel   Bytes per pixel (e.g., 4 for BGRA).
    40     1  bool    is_bgr            True if pixel format is BGR (not RGB).
    41     7  —       [padding]         Padding to 48-byte struct alignment.
```

### `MemoryRegion` — 16 bytes

Describes a contiguous region of physical memory discovered by the bootloader.

```
Offset  Size  Type  Field        Description
------  ----  ----  -----------  -----------------------------------------
     0     8  u64   phys_start   Starting physical address of the region.
     8     8  u64   size         Size of the region in bytes.
```

**Note:** Unlike the UEFI memory-map descriptor, this struct does not carry a type field. The bootloader is expected to filter and only pass usable (free) regions. If type information is needed in the future, the struct can be extended.

### `BootInfo` — 2200 bytes (~2.15 KB)

The master handoff structure passed from the chainloader through the bootloader to the kernel.

```
Offset   Size  Type                      Field                Description
-------  -----  ------------------------  -------------------  -----------------------------------------
 0x0000   2048  [MemoryRegion; 128]       memory_regions       Array of usable physical memory regions.
 0x0800      8  usize                     memory_region_count  Number of valid entries in `memory_regions`.
 0x0808     48  FramebufferInfo           framebuffer          UEFI GOP framebuffer description.
 0x0838      8  u64                       partition_zero_lba   Start LBA of the ext4 partition (partition zero).
 0x0840      8  u64                       partition_zero_size  Size in bytes of the ext4 partition.
 0x0848      8  u64                       kernel_image_addr    Physical address of the kernel loaded into memory.
 0x0850      8  u64                       kernel_image_size    Size of the kernel image in bytes.
 0x0858      8  u64                       drivers_elf_addr     Physical address of the preloaded drivers ELF blob.
 0x0860      8  u64                       drivers_elf_size     Size of the drivers ELF blob in bytes.
 0x0868      8  u64                       rsdp_addr            Physical address of the ACPI RSDP (from UEFI).
 0x0870      8  u64                       madt_addr            Physical address of the MADT / APIC table.
 0x0878      4  u32                       max_cpus             Maximum number of CPUs the kernel will bring up.
 0x087C      4  u32                       bsp_apic_id          LAPIC ID of the bootstrap processor (always 0 on x86).
 0x0880      4  u32                       ap_count             Number of enabled application processors (APs).
 0x0884     16  [u32; 4]                  ap_apic_ids          LAPIC IDs of each AP, indexed `0..ap_count`.
 0x0894      4  —                         [padding]            Padding to next 8-byte boundary.
```

**Total struct size:** 0x898 = 2200 bytes.

**Note on field ordering:** The layout places the largest field (`memory_regions`, 2048 bytes) first, followed by `memory_region_count` and `framebuffer`, then the ext4 partition and kernel image fields, and finally the ACPI / APIC fields. This ordering was chosen to group related fields and minimise padding.

### `DriverPkgHeader` — 12 bytes

Header of the custom driver package file format (`drivers.elf` on disk is **not** an ELF — it uses this manifest).

```
Offset  Size  Type       Field    Description
------  ----  --------  -------  -----------------------------------------
     0     8  [u8; 8]    magic    Must equal `DRIVER_PKG_MAGIC` (`b"LODAXPKG"`).
     8     4  u32        count    Number of driver entries in the manifest.
```

### `DriverPkgEntry` — 44 bytes

Describes a single driver ELF bundled within the package file.

```
Offset  Size  Type       Field        Description
------  ----  --------  ------------  -----------------------------------------
     0    32  [u8; 32]   name          Null-padded ASCII driver name.
    32     4  u32        class         0 = Hardware driver, 1 = Abstraction driver.
    36     4  u32        elf_offset    Byte offset of this driver's ELF data from the end of the manifest.
    40     4  u32        elf_size      Size of the driver ELF in bytes.
```

### Driver Package File Layout

```
Offset              Content
-----------------  -----------------------------------------
 0                 DriverPkgHeader  (12 bytes)
12                 DriverPkgEntry[0]
12 + 44            DriverPkgEntry[1]
...
12 + N * 44        DriverPkgEntry[N-1]
                   [driver ELF data 0]
                   [driver ELF data 1]
                   ...
```

The `elf_offset` field in each entry is an absolute byte offset from the start of the package file to the beginning of the corresponding driver ELF data. The `elf_size` field gives the exact byte count of that ELF.

---

## Size Summary

| Struct | Size |
|---|---|
| `FramebufferInfo` | 48 bytes |
| `MemoryRegion` | 16 bytes |
| `BootInfo` | 2200 bytes |
| `DriverPkgHeader` | 12 bytes |
| `DriverPkgEntry` | 44 bytes |
