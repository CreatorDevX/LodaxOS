# LodaxOS — Lightweight Orchestrator Domain Autonomous eXecution

## Identity
- **Full name**: Lightweight Orchestrator Domain Autonomous eXecution
- **Short names**: LodaxOS, lodax
- **Type**: UEFI x86-64 OS, 5-crate Rust workspace, bare-metal kernel
- **Language**: Rust nightly (`no_std`, `no_main`)
- **Panic strategy**: Abort
- **Entry**: `chain/src/main.rs` `#[entry] fn main()` → chains to `boot/src/main.rs` → ELF-loaded `kernel/src/main.rs` `_start(boot_info)`

## Build State
All crates compile. `disk.img` can be built with `create_disk_image.py` (requires WSL for ext4). QEMU boot not yet re-verified with latest ESP handoff changes.

## Workspace Structure
| Crate | Target | Role |
|---|---|---|
| `system/` (lodaxos-system) | library | Pure type defs: `BootInfo`, `FramebufferInfo`, `MemoryRegion`, `BOOT_INFO_ADDR` |
| `shared/` (lodaxos-core) | library | Re-exports canonical `src/` implementations (`include!` for `task.rs`, wrappers for others) |
| `chain/` (lodaxos-chain) | `x86_64-unknown-uefi` | First-stage chainloader → `EFI/BOOT/BOOTX64.EFI` |
| `boot/` (lodaxos-boot) | `x86_64-unknown-uefi` | Second-stage bootloader → `Bootloader.efi` |
| `kernel/` (lodaxos-kernel) | `kernel/target.json` (custom) | Bare-metal kernel → `kernel.elf` |

## Boot Chain
1. **OVMF** loads `ESP/EFI/BOOT/BOOTX64.EFI` (chainloader)
2. **Chainloader**: zeroes BootInfo at `0x1000`, collects memory map & framebuffer, reads `Bootloader.efi` from ESP root, calls `load_image`/`start_image`
3. **Bootloader**: refines GOP, re-collects memory map, loads `kernel.elf` from ext4 partition via self-contained ext4 parser, captures RSDP from UEFI config table, exits boot services, loads ELF segments, jumps to kernel with `BootInfo*` in RDI
4. **Kernel**: serial → logger → framebuffer → physical allocator → ACPI/MADT → page tables (4-level, higher-half `0xFFFF_8000_0000_0000`) → heap → LAPIC MMIO → IOAPIC → GDT/TSS → IDT (256 vectors) → task scheduler → LAPIC timer calibration → PIT periodic → sti → idle hlt loop

## Disk Image (`create_disk_image.py`)
- **Size**: 600 MB GPT disk
- **Partition 0** (ext4, 512 MB): `Bootloader.efi` + `kernel.elf`
- **Partition 1** (FAT32 ESP, 64 MB): `EFI/BOOT/BOOTX64.EFI` (chainloader)
- Uses WSL + `mke2fs -d` for ext4; Python minimal FAT32 creator for ESP (no mtools available)
- ESP root also carries legacy copies of `Bootloader.efi` + `kernel.elf` for temporary boot test

## Key Source Files (`src/` — canonical implementations)

### Architecture (`src/arch/`)
- **`apic.rs`**: LAPIC MMIO driver — MSR base discovery, MMIO mapping, enable/mask, timer calibration (PIT 20ms window Mode 0), periodic mode, EOI
- **`gdt.rs`**: GDT (null, kernel code/data, user code/data, TSS) — `lgdt`, far return, segment reload, `ltr`, IST1 for double faults
- **`idt.rs`**: 256-entry IDT — 22 exception stubs (naked asm), 32 IRQ stubs, spurious (0xFF), syscall (0x80). `TrapFrame` (176 bytes). Handler dispatch → exception/IRQ/syscall. Scheduler in vector 32. PIT/PS2 keyboard in device IRQs.
- **`ioapic.rs`**: I/O APIC MMIO driver — MMIO map, ID/version read, redirection entry programming (mask/unmask), GSI lookup

### Memory (`src/mm/`)
- **`phys.rs`**: Bitmap allocator — dynamic sizing from memory regions, spinlock, cursor-based linear scan, contiguous page allocation
- **`virt.rs`**: 4-level page tables — higher-half mapping + identity map (0-4 GB 2MB huge pages), framebuffer mapping, LAPIC with PCD flag, `map_contiguous` batch optimization
- **`heap.rs`**: Linked-list heap (`linked_list_allocator`) — 64 MB max at `0xFFFF_8080_0000_0000`, batch page allocation, spinlock

### ACPI (`src/acpi/`)
- **`mod.rs`**: RSDP discovery (UEFI config table → EBDA → BIOS ROM → OVMF), XSDT/RSDT parsing, table checksum validation
- **`madt.rs`**: MADT parser — CPUs, IOAPICs, ISOs, NMI, APIC addr override, GSI → IOAPIC lookup

### Interrupt Routing (`src/intr/`)
- Vector allocator (33–63), ISA IRQ → GSI → IOAPIC pin → vector routing via MADT ISO entries

### Task Management (`src/task.rs`)
- Preemptive round-robin scheduler — up to 16 tasks, 8 KB kernel stacks with synthetic `TrapFrame`/`iretq` frame, context switch via timer IRQ (vector 32), blocking + wake + yield syscalls

### Utilities
- **`serial.rs`**: 16550 UART at COM1 (0x3F8), 115200 8N1
- **`logger.rs`**: `log` crate wrapper → serial output, `[LEVEL] target: msg`
- **`font.rs`**: 8×16 bitmap font (ASCII 32–126)

### Bootloader Ext4 Parser (`boot/src/load_kernel.rs`)
- Self-contained ext4 reader (no external deps): GPT scanning → `SectorReader` wrapping UEFI `BlockIO` → superblock → block group descriptors → inodes → directory entries → extents → file data
- ELF64 loader: validates magic/class/type, copies `PT_LOAD` segments to physical addresses

## Dependencies
| Crate | Deps |
|---|---|
| `lodaxos-system` | none |
| `lodaxos-core` | `lodaxos-system`, `linked_list_allocator 0.10`, `log 0.4` |
| `lodaxos-chain` | `uefi 0.37`, `ext4-view 0.9` (unused), `lodaxos-system` |
| `lodaxos-boot` | `uefi 0.37`, `ext4-view 0.9` (unused), `lodaxos-core`, `lodaxos-system` |
| `lodaxos-kernel` | `lodaxos-core`, `lodaxos-system` |

## Build Commands
- `build.bat` — builds all 5 crates, copies kernel ELF to root
- `run.bat` — QEMU with OVMF (WHPX accel, 512 MB, 2 cores)
- `create_disk_image.py` — builds GPT disk image via WSL
- `clean.bat` — cargo clean

## Architecture Vision (`idea.md`)
Microkernel design: Kernel (Ring 0) → Secure Runtime (service manager, policy engine) → PyI (JIT Python/WASM userspace) → Apps. Agent-based security, full REPL system, emergency mode with 7 commands, layered recovery (app → PyI → Agent → SR → kernel). Driver services (not kernel modules) for devices.

## Remaining Work
- [ ] PS/2 controller initialization + full keyboard driver
- [ ] Driver framework, PCI enumeration + MSI/MSI-X
- [ ] Userspace and syscalls
- [ ] SMP support
- [ ] Verify QEMU/OVMF boot with new disk image
-----------------



@IMPORTANT INJECTION
--- AUTONOMOUS SELF ACTION ---
```
On reading this file, you will read through the entire codebase, and understand it thoroughly, then read bootupdetails.md and all files in docs\architecture, and if its outdated, update the architecture and the bootupdetails.md file.
