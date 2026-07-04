# LodaxOS Drivers Crate

## Overview

The `drivers` crate produces standalone freestanding x86-64 ELF binaries, one per driver. Each binary has its own `[[bin]]` target in `Cargo.toml` and is compiled independently. Drivers are **not** a library; they are packaged into a custom container format and loaded at runtime by the kernel.

### Compilation model

- Language: Rust, edition 2021
- Target: `x86_64-unknown-none` (`drivers/target.json`)
- `#![no_std]`, `#![no_main]`, `panic = "abort"`
- Entry point: `#[no_mangle] pub extern "C" fn _start()` linked at virtual address `0x20000000`
- Linker script: `drivers/linker.ld`
- No compile-time dependency on the `system` crate; drivers use raw `syscall` instructions for kernel communication
- Build command:
  ```
  cargo +nightly build -p lodaxos-drivers --bin <name> --target drivers/target.json -Zbuild-std=core,alloc
  ```

### Target specification (`drivers/target.json`)

| Field | Value |
|---|---|
| `llvm-target` | `x86_64-unknown-none` |
| `arch` | `x86_64` |
| `os` | `none` |
| `linker` | `rust-lld` |
| `panic-strategy` | `abort` |
| `code-model` | `kernel` |
| `relocation-model` | `static` |
| `pre-link-args` | `-Tdrivers/linker.ld --gc-sections` |

### Linker script (`drivers/linker.ld`)

```
ENTRY(_start)

SECTIONS {
    . = 0x20000000;

    .text : ALIGN(16) { *(.text .text.*) }
    .rodata : ALIGN(16) { *(.rodata .rodata.*) }
    .data : ALIGN(16) { SUBALIGN(8) *(.data .data.*) }
    .bss : ALIGN(16) { SUBALIGN(8) *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : { *(.eh_frame) *(.comment) *(.note*) }
}
```

Key properties:
- Base virtual address: `0x20000000`
- Standard section layout with 16-byte alignment for text/rodata and 8-byte SUBALIGN for data/BSS
- Discards `.eh_frame`, `.comment`, and `.note*` sections to reduce binary size

---

## Driver Package Format

Individual driver ELF binaries are bundled into a single `drivers.elf` file using the custom package format described below. The packaging is performed by `drivers/pkg.py`.

### Format layout

```
Offset  | Size     | Field
--------|----------|-------------------------------
0       | 8        | Magic: b"LODAXPKG"
8       | 4        | Entry count (u32 LE)
12      | N * 44   | Driver entries
...     | padding  | Zero-padded to 8-byte boundary
...     | variable | Driver ELF data (concatenated)
```

### DriverPkgHeader (system/src/lib.rs)

```rust
pub struct DriverPkgHeader {
    pub magic: [u8; 8],
    pub count: u32,
}
```

### DriverPkgEntry (system/src/lib.rs)

```rust
pub struct DriverPkgEntry {
    pub name: [u8; 32],     // null-padded ASCII
    pub class: u32,          // 0 = Hardware, 1 = Abstraction
    pub elf_offset: u32,     // byte offset from end of manifest to ELF data
    pub elf_size: u32,       // size in bytes of the ELF binary
}
```

Each entry is 44 bytes (32 + 4 + 4 + 4). The offset stored in `elf_offset` is the absolute byte offset from the start of the file (computed as `12 + count * 44 + padding`). The data portion is padded to an 8-byte boundary to ensure ELF structures in the driver binaries are not misaligned when the kernel casts pointers into the buffer.

### Class values

| Value | Meaning |
|---|---|
| 0 | Hardware driver (interacts directly with hardware) |
| 1 | Abstraction driver (provides logical services on top of hardware drivers) |

### Packaging command

```
python drivers\pkg.py <output> <name:class:path> [<name:class:path> ...]
```

Example:
```
python drivers\pkg.py drivers.elf framebuffer:0:target/target/debug/framebuffer ahci:0:target/target/debug/ahci ext4:1:target/target/debug/ext4 ide:0:target/target/debug/ide
```

The script (`drivers/pkg.py`):
1. Reads each ELF binary from disk
2. Builds the manifest header and entries
3. Pads the entry area to an 8-byte boundary
4. Appends each ELF binary's raw bytes
5. Reports the total packaged size

---

## Font Bitmap (`genfont.py`)

The font generator script at `genfont.py` produces an 8x16 bitmap font used by the framebuffer driver.

### Specification

| Property | Value |
|---|---|
| Glyph dimensions | 8 x 16 pixels |
| Character count | 128 (ASCII 0-127) |
| Total size | 2048 bytes (128 * 16) |
| Output file | `drivers/font.bin` |

### Glyph encoding

Each glyph is 16 consecutive bytes, one byte per row from top to bottom. Each bit within a byte represents a pixel: bit 7 is the leftmost pixel, bit 0 is the rightmost pixel. A set bit (1) draws the foreground color; a cleared bit (0) draws the background color.

### Character coverage

| Range | Glyphs |
|---|---|
| 0-31 | Default block (filled rectangle with border: 0xFF, 0x81, ...) |
| 32 (space) | Blank (all zero) |
| 33-47 | `! " # $ % & ' ( ) * + , - . /` |
| 48-57 | `0 1 2 3 4 5 6 7 8 9` |
| 58-64 | `: ; < = > ? @` |
| 65-90 | `A B C D E F G H I J K L M N O P Q R S T U V W X Y Z` |
| 91-96 | `[ \ ] ^ _ backtick` |
| 97-122 | `a b c d e f g h i j k l m n o p q r s t u v w x y z` |
| 123-126 | `{ \| } ~` |
| 127 | Default block |

---

## Syscall ABI

All drivers use the same basic syscall mechanism.

### Instruction and register conventions

| Register | Purpose |
|---|---|
| `rax` | Syscall number (input) / return value (output) |
| `rdi` | Argument 0 |
| `rsi` | Argument 1 |
| `rdx` | Argument 2 |
| `r10` | Argument 3 |
| `r8`  | Argument 4 |
| `r9`  | Argument 5 |
| `rcx`, `r11` | Clobbered (saved RIP and RFLAGS by `syscall`) |

Return value is in `rax`. A return of `0` or `!0` (`0xFFFF_FFFF_FFFF_FFFF`) typically indicates failure, depending on the specific syscall.

### Syscall number table

| Nr | Name | Arguments | Description |
|---|---|---|---|
| 5 | `sys_mmap` | `rdi=0, rsi=size, rdx=0` | Allocate virtual memory, returns virtual address |
| 10 | `sys_mmap_phys` | `rdi=phys, rsi=size, rdx=0` | Map physical memory, returns virtual address |
| 13 | `sys_dma_alloc` | `rdi=size, rsi=0, rdx=0` | Allocate DMA-able physical memory, returns physical address |
| 15 | `sys_pci_rw` | `rdi=bdf, rsi=offset, rdx=width, r10=value, r8=write_flag` | PCI config space read/write |
| 20 | `sys_driver_recv` | `rdi=buf_ptr, rsi=0, rdx=0` | Non-blocking receive from mailbox (returns 0 on success) |
| 21 | `sys_driver_send` | `rdi=value, rsi=0, rdx=0` | Send result back to kernel (blocking) |
| 22 | `sys_driver_recv_block` | `rdi=buf_ptr, rsi=0, rdx=0` | Blocking receive from mailbox (returns 0 on success) |
| 30 | `sys_gdf_register` | `rdi=name_ptr, rsi=name_len, rdx=0` | Register driver with the GDF (Generic Driver Framework) |
| 31 | `sys_driver_call` | `rdi=name_ptr, rsi=name_len, rdx=cmd, r10=a0, r8=a1, r9=a2` | Call another driver's IPC command and wait for response |

### Mailbox message format

All driver IPC uses a fixed-size 4-element `[u64; 4]` array as the message buffer:

```
msg[0] = command (u32, upper 32 bits unused)
msg[1] = argument 0
msg[2] = argument 1
msg[3] = argument 2
```

For `sys_driver_recv` and `sys_driver_recv_block`, the buffer pointer is passed in `rdi`. For `sys_driver_send`, a single `u64` result value is sent back to the caller.

For `sys_driver_call`, the call blocks until the target driver responds to the IPC, and the return value is the result sent back by that driver via `sys_driver_send`.

---

## Driver: Framebuffer (`drivers/src/bin/framebuffer.rs`)

### Purpose

Text-mode framebuffer driver with 8x16 bitmap font rendering, double-buffering via shadow buffer, hardware cursor management, and vertical scrolling.

### Registration

- Name: `"framebuffer"`
- Class: Hardware (0)

### Syscalls used

| Nr | Name | Purpose |
|---|---|---|
| 5 | `sys_mmap` | Allocate shadow buffer (virtual memory) |
| 10 | `sys_mmap_phys` | Map framebuffer physical memory |
| 21 | `sys_driver_send` | Return command results |
| 22 | `sys_driver_recv_block` | Blocking receive of commands |
| 30 | `sys_gdf_register` | Register as "framebuffer" |

### Entry point flow

1. Call `sys_gdf_register(b"framebuffer")` to register with the kernel
2. Enter infinite loop:
   - Call `sys_driver_recv_block(&mut cmd_buf)` (blocking) to receive a command
   - Dispatch on `cmd_buf[0]` (the command ID)
   - Execute the command and call `sys_driver_send(result)` to reply

### Fb structure

The internal `Fb` struct holds:

| Field | Type | Description |
|---|---|---|
| `fb_ptr` | `*mut u8` | Virtual address of mapped framebuffer (physical via mmap_phys) |
| `shadow` | `*mut u8` | Virtual address of shadow buffer (allocated via mmap) |
| `w` | `usize` | Width in pixels |
| `h` | `usize` | Height in pixels |
| `stride` | `usize` | Bytes per row |
| `bpp` | `usize` | Bytes per pixel (must be 4) |
| `is_bgr` | `bool` | Pixel format: true = BGR, false = RGB |
| `fg_r/g/b` | `u8` | Foreground color components |
| `bg_r/g/b` | `u8` | Background color components |

### Commands

| Command | ID | Arguments | Result | Description |
|---|---|---|---|---|
| ACQUIRE | 0xFF | `arg0=fb_phys, arg1=fb_size, arg2=packed` | 0=ok, 1=already acquired, 2=error | Acquire the framebuffer. `arg2` packed: bits 0-15=width, 16-31=height, 32-47=stride, 48-55=bpp, bit 56=is_bgr |
| CLEAR | 2 | none | 0=ok, 2=error | Fill shadow buffer with zeroes |
| DRAW_TEXT | 5 | `arg0=text_phys, arg1=text_len, arg2=packed(x\<\<32\|y)` | 0=ok, 2=error | Render text at (x, y). Maps text physical pages, draws glyphs, handles newline/tab. `arg2` packed: upper 32 bits = x, lower 32 bits = y |
| SET_FG | 6 | `arg0=packed_rgb(0x00RRGGBB)` | 0=ok, 2=error | Set foreground color |
| SET_BG | 7 | `arg0=packed_rgb(0x00RRGGBB)` | 0=ok, 2=error | Set background color |
| SCROLL | 8 | `arg0=lines` | 0=ok, 2=error | Scroll up by `lines` character rows (each row = 18 pixels: 16 for glyph + 2 for spacing). Shifts shadow buffer data up, clears bottom |
| PRESENT | 10 | none | 0=ok, 2=error | Copy shadow buffer to physical framebuffer (`copy_nonoverlapping`) |

### Font rendering

- Font data embedded via `include_bytes!("../../font.bin")` at compile time
- Glyph size: `GLYPH_W = 8`, `GLYPH_H = 16`
- Each glyph is 16 bytes in the font array, indexed as `FONT[char_index * 16 + row]`
- For each set bit in a glyph row, a pixel is written in the foreground color
- Unset bits within the glyph area are filled with the background color via `fill_rect`
- Character range 32-127 is rendered; values above 127 are silently ignored
- Control characters:
  - `\n` (0x0A): carriage return + line feed (reset x to start_x, advance y by `GLYPH_H + 2`)
  - `\r` (0x0D): carriage return only (reset x to start_x)
  - `\t` (0x09): advance x to next 4-character tab stop

### Scrolling

- One scroll line = `GLYPH_H + 2 = 18` pixel rows
- The `scroll(lines)` method shifts the shadow buffer up by `lines * 18 * stride` bytes
- If the shift exceeds the total buffer size, the entire buffer is cleared
- Newly exposed area at the bottom is zero-filled

---

## Driver: AHCI SATA (`drivers/src/bin/ahci.rs`)

### Purpose

AHCI SATA controller driver that discovers an AHCI HBA via PCI enumeration, initializes the first available ATA port, and performs DMA-based disk read operations.

### Registration

- Name: `"ahci"`
- Class: Hardware (0)

### Syscalls used

| Nr | Name | Purpose |
|---|---|---|
| 10 | `sys_mmap_phys` | MMIO-map ABAR (AHCI base address register) |
| 13 | `sys_dma_alloc` | Allocate DMA buffers for command list, FIS, command table, PRDT |
| 15 | `sys_pci_rw` | PCI config space read/write for BDF scan, BAR5, bus master enable |
| 20 | `sys_driver_recv` | Non-blocking receive of commands |
| 21 | `sys_driver_send` | Return results |
| 30 | `sys_gdf_register` | Register as "ahci" |

### PCI enumeration

The driver scans bus 0, devices 0-31, functions 0-7 for a PCI device matching:

- Class code: `0x01` (Mass storage controller)
- Subclass: `0x06` (Serial ATA)

For each candidate, it reads the vendor/device ID at PCI config offset 0 and checks for all-ones or zero (empty slot). Matching devices are further filtered by function 0 logic (multi-function devices are scanned; if function 0 is absent, remaining functions on that device are skipped).

#### PCI access helper (syscall 15)

- `sys_pci_read(bdf, offset, width)`: returns the value read
- `sys_pci_write(bdf, offset, width, value)`: write flag = 1

The BDF encoding is:
```
bits 31-20: bus
bits 19-15: device
bits 14-12: function
```

#### BAR5 (ABAR) retrieval

- Read PCI config offset `0x24` (BAR5 lower 32 bits, with low 4 bits masked to 0 for address)
- Read PCI config offset `0x28` (BAR5 upper 32 bits)
- Combine: `(low & !0xF) | (high << 32)`

#### Bus master enable

- Read PCI command register at offset 4
- OR with `0x6` (Bus Master + Memory Space bits)
- Write back

### AHCI HBA registers (ABAR offsets)

All registers are 32-bit MMIO accessed via `read_volatile`/`write_volatile`.

| Offset | Name | Description |
|---|---|---|
| 0x00 | HBA_CAP | Capabilities register (bits 12-8: number of ports - 1) |
| 0x04 | HBA_GHC | Global HBA Control (bit 0 = HR reset, bit 31 = AE AHCI enable) |
| 0x0C | HBA_PI | Ports Implemented (bitmap of present ports) |

#### Port register block (0x80 bytes per port, starting at ABAR + 0x100)

| Offset | Name | Description |
|---|---|---|
| 0x00 | PORT_CLB | Command List Base (lower 32 bits) |
| 0x04 | PORT_CLBU | Command List Base (upper 32 bits) |
| 0x08 | PORT_FB | FIS Base (lower 32 bits) |
| 0x0C | PORT_FBU | FIS Base (upper 32 bits) |
| 0x14 | PORT_IE | Interrupt Enable |
| 0x18 | PORT_CMD | Command and Status |
| 0x24 | PORT_SIG | Device Signature |
| 0x28 | PORT_SSTS | Serial ATA Status (low 4 bits = device detection) |
| 0x38 | PORT_CI | Command Issue (write 1 to slot 0 to issue) |

#### Port command register bits (PORT_CMD, offset 0x18)

| Bit | Name | Description |
|---|---|---|
| 0 | CMD_ST | Start (enable DMA engine) |
| 1 | CMD_SUD | Spin-Up Device |
| 2 | CMD_POD | Power On Device |
| 4 | CMD_FRE | FIS Receive Enable |
| 14 | CMD_FR | FIS Receive Running (read-only) |
| 15 | CMD_CR | Command List Running (read-only) |

#### Device signature values (PORT_SIG)

| Value | Meaning |
|---|---|
| 0x00000101 | ATA device |
| 0xEB140101 | ATAPI device |

### Initialization sequence

1. Read CAP to determine number of ports
2. Read PI to determine which ports are implemented
3. Assert HBA reset (GHC bit 0), wait for self-clear, then deassert
4. Enable AHCI mode (GHC bit 31)
5. For each implemented port:
   - Check signature for ATA or ATAPI
   - Spin up: set POD and SUD bits
   - Wait for device presence (SSTS low nibble = 0x03)
   - Stop port DMA (clear ST, wait for CR=0; clear FRE, wait for FR=0)
   - Allocate 1024-byte DMA buffer for command list (page-aligned), set CLB/CLBU
   - Allocate 256-byte DMA buffer for received FIS, set FB/FBU
   - Disable interrupts
   - Start port DMA (set FRE, then ST)

### Disk read sequence (command 10)

The AHCI driver supports `READ_DMA_EXT` (ATA command 0x25) with 48-bit LBA addressing:

1. Read CLB/CLBU to get the command list physical address
2. Allocate a 256-byte DMA buffer for the command table
3. Clear the first command header slot (32 bytes at offset 0 of command list)

#### Command header structure (32 bytes, slot 0 at command_list + 0)

| Byte offset | Size | Field |
|---|---|---|
| 0 | 2 | CFL (command FIS length in DWORDS, bits 0-4) |
| 2 | 2 | PRDTL (PRDT entry count, bits 16-31) |
| 4 | 4 | PRDBC (byte count transferred, write-cleared) |
| 8 | 4 | CTBA (command table base address, lower 32) |
| 12 | 4 | CTBA (command table base address, upper 32) |
| 16 | 16 | Reserved |

CFL = 5 (5 DWORDS = 20 bytes for the Register H2D FIS).

#### PRDT (Physical Region Descriptor Table)

Written at bytes 128+ of the command table. Each entry is 16 bytes:

| Byte offset | Size | Field |
|---|---|---|
| 0 | 4 | DBA (data base address, lower 32 bits) |
| 4 | 4 | DBA (data base address, upper 32 bits) |
| 8 | 4 | Reserved |
| 12 | 4 | DBC (data byte count, bits 0-21; bit 31 = I interrupt-on-completion) |

- Max transfer per PRDT entry: 2 MiB (0x200000 bytes)
- PRDT count is calculated as `ceil(byte_count / 0x200000)`
- Last entry's DBC is set to `(remaining_bytes - 1)` unless it exactly fills 2 MiB
- I bit (bit 31 of DBC) is always set

#### Register H2D FIS (FIS type 0x27)

Written at bytes 0-19 of the command table:

| Byte | Field | Value/Encoding |
|---|---|---|
| 0 | FIS type | 0x27 |
| 1 | Flags | 0x80 (C bit = command) |
| 2 | Command | 0x25 (READ_DMA_EXT) |
| 3 | Features (low) | 0 |
| 4 | LBA low (7:0) | sector & 0xFF |
| 5 | LBA low (15:8) | (sector >> 8) & 0xFF |
| 6 | LBA low (23:16) | (sector >> 16) & 0xFF |
| 7 | Device | 0x40 (LBA mode bit set) |
| 8 | LBA high (7:0) | (sector >> 24) & 0xFF |
| 9 | LBA high (15:8) | (sector >> 32) & 0xFF |
| 10 | LBA high (23:16) | (sector >> 40) & 0xFF |
| 11 | Features (high) | 0 |
| 12 | Count (low) | count & 0xFF |
| 13 | Count (high) | (count >> 8) & 0xFF |
| 14 | Reserved | 0 |
| 15 | Control | 0 |
| 16-19 | Reserved | 0 |

4. Issue command: write 1 to PORT_CI (command issue register)
5. Wait for PORT_CI bit 0 to clear (polling, up to 1,000,000 iterations with PAUSE)

### Physical-to-virtual address translation

The driver accesses DMA buffers via the kernel's higher-half direct map at `0xFFFF_8000_0000_0000`. The `virt_from_phys(phys)` function returns `0xFFFF_8000_0000_0000 + phys`.

### IPC command protocol

| Command | Arguments | Result | Description |
|---|---|---|---|
| 10 | `buf[1]=sector, buf[2]=count, buf[3]=dma_phys_or_0` | Physical address on success, `!0` on error | Read `count` sectors starting at `sector` into the DMA buffer at `dma_phys`. If `buf[3]` is 0, the driver allocates its own DMA buffer of `align_up(count * 512, 4096)` bytes |
| 20 | `buf[1]=size` | Physical address on success, `!0` on error | Allocate a DMA buffer of `align_up(size, 4096)` bytes and return its physical address |

### Error handling

If the AHCI controller is not found, BAR5 is 0, mmap fails, or HBA initialization fails, the driver enters a `reject_loop` that responds `!0` to all incoming commands forever.

---

## Driver: ext4 (`drivers/src/bin/ext4.rs`)

### Purpose

Abstraction driver that implements ext4 filesystem read-only access. It sits on top of a block device driver (AHCI or IDE) and provides file-level IPC to the kernel.

### Registration

- Name: `"ext4"`
- Class: Abstraction (1)

### Syscalls used

| Nr | Name | Purpose |
|---|---|---|
| 20 | `sys_driver_recv` | Non-blocking receive of commands |
| 21 | `sys_driver_send` | Return results |
| 30 | `sys_gdf_register` | Register as "ext4" |
| 31 | `sys_driver_call` | Call block driver (ahci/ide) to read sectors or allocate DMA buffers |

### Initialization (mount)

1. Probe AHCI first: call `sys_driver_call(b"ahci", 10, PART_LBA + 2, 2, 0)` to read sectors 2050-2051 (the ext4 superblock, which is at LBA 2048 + 2 = 2050)
   - `PART_LBA = 2048` (partition offset)
2. Check magic at offset 56 of the superblock: must be `0xEF53`
   - If AHCI fails or magic is wrong, fall back to IDE with the same request
   - If IDE also fails, mount fails and the driver enters the reject loop
3. Parse superblock fields:
   - `s_log_block_size` (offset 24): block size = `1024 << s_log_block_size`
   - `s_inodes_per_group` (offset 40)
   - `s_inode_size` (offset 88)
   - `bg_blk`: block group descriptor block number (1 if block size > 1024, else 2)
4. Partition offset applied: `lba = PART_LBA + block * sec_per_blk`

### On-disk structures accessed

#### Superblock fields

| Offset | Size | Field |
|---|---|---|
| 24 | 4 | s_log_block_size (log2 of block size / 1024) |
| 40 | 4 | s_inodes_per_group |
| 56 | 2 | s_magic (must be 0xEF53) |
| 88 | 2 | s_inode_size |

#### Block group descriptor (one per block group)

| Offset | Size | Field |
|---|---|---|
| 8 | 4 | bg_inode_table_lo (low 32 bits of inode table block number) |
| 24 | 4 | bg_inode_table_hi (high 32 bits of inode table block number) |

#### Inode (128 or 256 bytes)

| Offset | Size | Field |
|---|---|---|
| 4 | 4 | i_size_lo |
| 32 | 4 | i_flags |
| 40 | 60 | i_block (extent tree root or block pointers) |
| 108 | 4 | i_size_hi |

#### Extent tree header (at i_block, offset 40 of inode)

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | eh_magic (0xF30A) |
| 2 | 2 | eh_entries |
| 6 | 2 | eh_depth (0 = leaf, >0 = index) |

#### Extent entry (leaf, depth 0)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | ee_block (logical block number) |
| 4 | 2 | ee_len (number of blocks, bit 15 = unwritten flag) |
| 6 | 2 | ee_start_hi |
| 8 | 4 | ee_start_lo |

#### Extent index (depth > 0)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | ei_block (logical block number) |
| 4 | 4 | ei_leaf_lo |
| 8 | 2 | ei_leaf_hi |
| 10 | 6 | Reserved |

#### Directory entry

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | inode |
| 4 | 2 | rec_len |
| 6 | 1 | name_len |
| 7 | 1 | file_type |
| 8 | name_len | name |

### File read algorithm

The `read_file(inode, dst_phys)` method walks the extent tree:

1. Read the inode's i_block (bytes 40-99, 60 bytes) into a local buffer
2. If `eh_magic != 0xF30A`, return failure
3. If `eh_depth == 0` (leaf):
   - For each extent entry, iterate `ee_len` blocks
   - Read each block into a temporary buffer, copy to destination at the appropriate offset
   - Stop when `dst >= file_size`
4. If `eh_depth > 0` (index):
   - Read the child extent block at `(ei_leaf_hi << 32) | ei_leaf_lo`
   - Repeat from step 2

### IPC command protocol

| Command | Arguments | Result | Description |
|---|---|---|---|
| 1 | none | Physical address of file data on success, `!0` on error | Look up `"file.txt"` in the root directory (inode 2) and read its full contents into a 4096-byte buffer. The physical address of the buffer is returned |
| 2 | none | Last file size | Return the `file_size` value from the most recent command 1 invocation |

The driver is currently hardcoded to read `"file.txt"` from the root directory only. It allocates a 4096-byte buffer for file data and a block-sized scratch buffer.

### Block device communication

The ext4 driver uses `sys_driver_call` (syscall 31) to communicate with the underlying block device driver:

| Target | Command | Arguments | Purpose |
|---|---|---|---|
| `ahci`/`ide` | 10 | `sector`, `count`, `0` | Read sectors (driver allocates buffer internally) |
| `ahci`/`ide` | 20 | `size`, `0`, `0` | Allocate DMA buffer |

The driver tries AHCI first and falls back to IDE.

---

## Driver: IDE PIO (`drivers/src/bin/ide.rs`)

### Purpose

Legacy IDE PIO mode driver for primary channel, supporting LBA28 read operations.

### Registration

- Name: `"ide"`
- Class: Hardware (0)

### Syscalls used

| Nr | Name | Purpose |
|---|---|---|
| 13 | `sys_dma_alloc` | Allocate DMA buffer for sector data |
| 20 | `sys_driver_recv` | Non-blocking receive of commands |
| 21 | `sys_driver_send` | Return results |
| 30 | `sys_gdf_register` | Register as "ide" |

### Port I/O

Uses x86 `in`/`out` instructions directly (no MMIO):

- `outb(port, val)`: `out dx, al`
- `inb(port)`: `in al, dx`
- `inw(port)`: `in ax, dx`

### Primary channel I/O port map

| Port | Name | Description |
|---|---|---|
| 0x1F0 | IDE_DATA | Data port (16-bit reads) |
| 0x1F2 | IDE_SEC_COUNT | Sector count |
| 0x1F3 | IDE_LBA_LO | LBA low byte |
| 0x1F4 | IDE_LBA_MI | LBA mid byte |
| 0x1F5 | IDE_LBA_HI | LBA high byte |
| 0x1F6 | IDE_DRIVE | Drive/head register |
| 0x1F7 | IDE_CMD | Command register (write) |
| 0x1F7 | IDE_STATUS | Status register (read) |

### Status register bits

| Bit | Name | Description |
|---|---|---|
| 0 | ERR | Error occurred |
| 3 | DRQ | Data request ready |
| 7 | BSY | Busy |

### PIO read LBA28 (`ide_read_sectors`)

1. Verify sector is within 28-bit LBA range (`<= 0x0FFFFFFF`)
2. Program the drive/head register:
   - `0xE0 | ((sector >> 24) & 0x0F)` — LBA mode, master drive, top 4 bits of LBA
3. Program sector count: `count & 0xFF`
4. Program LBA bytes 0-2: low, mid, high
5. Issue command `0x20` (ATA READ SECTORS) to port 0x1F7
6. For each sector:
   - Poll status until BSY=0 and DRQ=1 (or ERR) — up to 10,000,000 iterations
   - Read 256 words (512 bytes) via `inw(IDE_DATA)` into the DMA buffer
7. Buffer is accessed via the higher-half direct map: `0xFFFF_8000_0000_0000 + buf_phys`

### IPC command protocol

| Command | Arguments | Result | Description |
|---|---|---|---|
| 10 | `buf[1]=sector, buf[2]=count, buf[3]=dma_phys_or_0` | Physical address on success, `!0` on error | Read `count` sectors starting at `sector` via PIO. If `buf[3]` is 0, the driver allocates its own DMA buffer of `align_up(count * 512, 4096)` bytes |
| 20 | `buf[1]=size` | Physical address on success, `!0` on error | Allocate a DMA buffer of `align_up(size, 4096)` bytes |

### Limitations

- LBA28 only (max 128 GiB disk, sector limit 0x0FFFFFFF)
- Primary channel, master drive only
- PIO mode (no DMA) — relatively slow but simple
- No interrupt-driven operation (polling only)

---

## IPC Summary

### Framebuffer (class 0, blocking receive)

| Command | Code | Direction |
|---|---|---|
| ACQUIRE | 0xFF | Kernel -> Framebuffer |
| CLEAR | 2 | Kernel -> Framebuffer |
| DRAW_TEXT | 5 | Kernel -> Framebuffer |
| SET_FG | 6 | Kernel -> Framebuffer |
| SET_BG | 7 | Kernel -> Framebuffer |
| SCROLL | 8 | Kernel -> Framebuffer |
| PRESENT | 10 | Kernel -> Framebuffer |

### Block device protocol (shared by AHCI and IDE, class 0)

| Command | Code | Direction |
|---|---|---|
| READ_BLOCKS | 10 | Kernel/Abstraction -> Block driver |
| ALLOC_DMA | 20 | Kernel/Abstraction -> Block driver |

### ext4 (class 1)

| Command | Code | Direction |
|---|---|---|
| READ_FILE | 1 | Kernel -> ext4 |
| GET_LAST_SIZE | 2 | Kernel -> ext4 |

---

## File Index

| Path | Description |
|---|---|
| `drivers/src/bin/framebuffer.rs` | Text-mode framebuffer driver |
| `drivers/src/bin/ahci.rs` | AHCI SATA controller driver |
| `drivers/src/bin/ext4.rs` | ext4 filesystem abstraction driver |
| `drivers/src/bin/ide.rs` | Legacy IDE PIO driver |
| `drivers/pkg.py` | Driver packaging script |
| `drivers/target.json` | Rust target specification |
| `drivers/linker.ld` | Linker script (entry at 0x20000000) |
| `drivers/font.bin` | 8x16 bitmap font (2048 bytes, generated) |
| `genfont.py` | Font generator script |
| `system/src/lib.rs` | Shared types (DriverPkgHeader, DriverPkgEntry, FB_CMD constants) |
