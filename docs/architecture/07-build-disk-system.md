# 07 — Build System and Disk Image

## Overview

The build system produces three UEFI binaries (chainloader, bootloader, kernel) and assembles them into a GPT-partitioned disk image suitable for QEMU or physical hardware.

## Build Pipeline

```
Source code (Rust nightly)
  │
  ├─ cargo build -p lodaxos-system       → system/target/ (library)
  ├─ cargo build -p lodaxos-core         → shared/target/ (library)
  ├─ cargo build -p lodaxos-kernel       → kernel.elf (custom target)
  ├─ cargo build -p lodaxos-boot         → lodaxos-boot.efi (x86_64-unknown-uefi)
  └─ cargo build -p lodaxos-chain        → lodaxos-chain.efi (x86_64-unknown-uefi)
                                              │
                                              ▼
                                    create_disk_image.py
                                              │
                                              ▼
                                          disk.img
```

### Build Script (`build.bat`)

```bat
cargo +nightly build -p lodaxos-system
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zbuild-std=core,alloc
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
copy target\target\debug\deps\lodaxos_kernel-* kernel.elf
```

Key points:
- Kernel uses `-Zbuild-std=core,alloc` to build Rust's core and alloc libraries from source for the custom target
- `-Zbuild-std-features=compiler-builtins-mem` enables compiler memory intrinsics (memcpy, memset, memcmp)
- `-Zjson-target-spec` allows using the JSON target specification without installing it globally
- The kernel.elf is found by wildcard in `target/target/` (the subdirectory name matches the target filename)

### Build Targets Output

| Build Artifact | File | Size |
|---|---|---|
| lodaxos-kernel | `kernel.elf` | ~3.9 MB |
| lodaxos-boot | `target/x86_64-unknown-uefi/debug/lodaxos-boot.efi` | ~493 KB |
| lodaxos-chain | `target/x86_64-unknown-uefi/debug/lodaxos-chain.efi` | ~386 KB |

## Disk Image Architecture

### GPT Layout (600 MB total)

```
LBA 0:           Protective MBR
LBA 1:           GPT Header
LBA 2–33:        Partition Entry Array (128 entries × 128 bytes)
LBA 34–2047:     Unused (GPT alignment)
LBA 2048–1050623: Partition 0 — ext4 (512 MB)
LBA 1050624–1181695: Partition 1 — ESP FAT32 (64 MB)
LBA 1181696–1228799: Backup GPT
```

### Partition 0 — ext4 (Partition Zero)

- **Type GUID**: `0FC63DAF-8483-4772-8E79-3D69D8477DE4` (Linux filesystem)
- **Label**: "LodaxOS"
- **Contents**: `Bootloader.efi`, `kernel.elf`
- **Size**: 512 MB

Created via `mke2fs -d` which populates the filesystem from a staging directory without requiring loop device mounting. This is critical because WSL2 does not support loop devices.

```
dd if=/dev/zero of=ext4_part.img bs=1M count=512
mkdir -p /tmp/lodaxos_staging
cp kernel.elf /tmp/lodaxos_staging/
cp lodaxos-boot.efi /tmp/lodaxos_staging/Bootloader.efi
mke2fs -t ext4 -d /tmp/lodaxos_staging -L LodaxOS ext4_part.img
```

The resulting ext4 image is written into the disk image at the partition's byte offset.

### Partition 1 — ESP (FAT32)

- **Type GUID**: `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` (EFI System Partition)
- **Contents**: `EFI/BOOT/BOOTX64.EFI` (chainloader)
- **Size**: 64 MB

Created by a Python minimal FAT32 implementation. The fallback is used when `mtools` and `mkfs.fat` are not available in WSL.

The Python FAT32 creator constructs:
1. **BPB (BIOS Parameter Block)**: Jump instruction, OEM name, bytes per sector (512), sectors per cluster (8), reserved sectors (32), number of FATs (2), media type (0xF8)
2. **FSInfo sector**: Free cluster count, next free cluster hint
3. **FAT (File Allocation Table)**: Cluster chain for the file (start cluster 3, entries 0=0x0FFFFFF8, 1=0x0FFFFFFF, 2=EOC marker, 3+ = chain)
4. **Root directory cluster** (cluster 2): Directory entry for `BOOTX64.EFI` (short name, extension, attributes, cluster, file size)
5. **Data cluster 3+**: Chainloader binary data

The ESP root also contains legacy copies of `Bootloader.efi` and `kernel.elf` for the temporary boot test where the chainloader reads them directly from the ESP instead of from ext4.

### GPT Header Construction

Custom GPT builder in `create_disk_image.py`:
- Protective MBR at LBA 0 (partition type 0xEE, covering entire disk)
- GPT header at LBA 1 with signature `"EFI PART"`, revision 1.0
- Partition entry array at LBA 2 (128 entries, each 128 bytes)
- Backup GPT at the end of the disk

### Partition Entry Format (128 bytes)

| Offset | Size | Field |
|---|---|---|
| 0 | 16 | Partition type GUID |
| 16 | 16 | Unique partition GUID |
| 32 | 8 | Starting LBA |
| 40 | 8 | Ending LBA |
| 48 | 8 | Attributes |
| 56 | 72 | Partition name (UTF-16LE) |

## QEMU Launch (`run.bat`)

```bat
"C:\Program Files\qemu\qemu-system-x86_64.exe" ^
    -drive if=pflash,format=raw,readonly=on,file="C:\Program Files\qemu\share\edk2-x86_64-code.fd" ^
    -drive file="disk.img",format=raw,if=ide ^
    -serial stdio ^
    -accel whpx ^
    -m 512M ^
    -smp 2
```

| Flag | Purpose |
|---|---|
| `-drive if=pflash,...edk2-x86_64-code.fd` | Load OVMF (UEFI firmware) |
| `-drive file=disk.img,if=ide` | Present disk image as IDE drive |
| `-serial stdio` | Redirect COM1 to terminal for debug output |
| `-accel whpx` | Windows Hypervisor Platform for hardware acceleration |
| `-m 512M` | 512 MB RAM |
| `-smp 2` | 2-CPU symmetric multiprocessing topology |

### OVMF Boot Path

OVMF follows the UEFI specification's fallback boot path:
1. Scan all partitions for FAT filesystems
2. Look for `\EFI\BOOT\BOOTX64.EFI`
3. Load and execute it

The `esp/startup.nsh` script provides an alternative boot path via the UEFI shell:
```
FS0:
EFI\BOOT\BOOTX64.EFI
```

## Clean Script (`clean.bat`)

```bat
cargo +nightly clean
```

Removes all build artifacts (target directory). The disk image (`disk.img`) is preserved.

## Full Run (`fullrun.bat`)

A convenience script that runs build → image creation → QEMU in sequence.

## Future Build Improvements

### Caching and Incremental Builds

- sccache for distributed compilation of Rust crates
- Pre-built toolchain cache for the kernel's custom target
- Incremental ELF segment loading for faster feedback loops

### Image Creation

- Support for writing to physical USB drives (dd to \\.\PhysicalDriveX)
- Support for network boot (PXE/TFTP)
- Multi-image support (debug image, release image, minimal image)

### Debugging

- QEMU GDB stub integration (`-s -S` flags)
- Automated QEMU testing with expect scripts
- Serial log capture and analysis
- Boot time measurement and profiling
