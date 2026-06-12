# 01 — Crate Structure

## Workspace Topology

The workspace `Cargo.toml` at root defines four members with resolver version 2:

```toml
[workspace]
members = ["system", "chain", "boot", "kernel"]
resolver = "2"
```

The dependency graph is a directed acyclic graph:

```
lodaxos-system  (no deps, pure types)
      ↓
lodaxos-kernel  (depends on lodaxos-system)
lodaxos-boot    (depends on lodaxos-system)
lodaxos-chain   (depends on lodaxos-system)
```

Each crate is self-contained — there is no shared implementation crate. The kernel and bootloader both have their own independent copies of serial, logger, and other infrastructure code. This is intentional: they run at different privilege levels and different environments (UEFI vs bare metal), and the amount of shared code is small.

## Crate Purposes

### `lodaxos-system` (`system/`)

**Purpose**: Pure type definitions shared across all boot stages. Zero dependencies, `#![no_std]`.

**Contents**:
- `BootInfo` struct — the inter-stage communication structure (dynamically allocated; 8-byte pointer at `0x1000` = `BOOT_INFO_HANDOFF_ADDR`)
- `FramebufferInfo` — GOP framebuffer metadata (address, resolution, stride, pixel format)
- `MemoryRegion` — (phys_start, size) pair for free memory regions
- `Caps` / `CapOp` / `CapError` / `CapResponse` / `CapRequest` / `Mailbox` — capability-system types and kernel↔policy-process IPC page (reserved for future use)
- Constants: `BOOT_INFO_HANDOFF_ADDR` (`0x1000`), `MAX_MEMORY_REGIONS` (`128`), `MAX_CPUS` (`4`)

**Rationale**: Separating types into their own crate avoids circular dependencies and ensures that every boot stage agrees on the exact memory layout of the handoff struct. A single-byte misalignment between chainloader and kernel would cause silent data corruption.

### `lodaxos-chain` (`chain/`)

**Purpose**: First-stage UEFI chainloader. Its job is minimal: initialize the serial port, write a skeleton `BootInfo` at `0x1000`, read `Bootloader.efi` from the ESP, and chainload it via `uefi::boot::load_image` + `start_image`.

**Key design choices**:
- Own inline serial init (raw `out` instructions) — keeps the chainloader small and independent
- Does not parse ext4 — that's the bootloader's job
- Does not exit boot services — that's the bootloader's job
- Captures memory map and framebuffer info, writes them to `BootInfo`, then hands off

**Why two-stage?** The chainloader is a simple PE32+ on FAT32. The bootloader is a larger binary that includes a full ext4 parser and ELF loader. Separating them lets the chainloader stay small (~386 KB) and reliable.

### `lodaxos-boot` (`boot/`)

**Purpose**: Second-stage UEFI bootloader. Runs after being loaded by the chainloader. Its responsibilities:
1. Refines the framebuffer via GOP (explicit mode set)
2. Re-collects the UEFI memory map (allocations from chainload may have changed it)
3. Loads `kernel.elf` from the ext4 partition using its own ext4 filesystem parser
4. Captures the ACPI RSDP from the UEFI configuration table
5. Enumerates AP LAPIC IDs via UEFI MP Services protocol
6. Writes the updated `BootInfo` back through the dynamic pointer
7. Calls `exit_boot_services()`
8. Loads the kernel ELF segments into physical memory
9. Jumps to the kernel entry point

**Key design choices**:
- Self-contained ext4 parser — no external crate dependency for filesystem reading
- Must capture RSDP *before* `exit_boot_services` — after that, UEFI runtime services are gone
- Must `cli` immediately after `exit_boot_services` — stale UEFI timer interrupts would triple-fault without our IDT
- UEFI MP Services is only used to *enumerate* APs — the kernel brings them up via INIT-SIPI-SIPI after ExitBootServices

### `lodaxos-kernel` (`kernel/`)

**Purpose**: The bare-metal operating system kernel. Compiled for a custom `x86_64-unknown-none` target with its own linker script and target specification.

**Key design choices**:
- `code-model = "kernel"` — allows the kernel to be linked at `0x100000` while accessing higher-half addresses via static relocations
- `disable-redzone = true` — essential for x86-64 interrupt handlers (the red zone would be corrupted if an interrupt fires between a function call and its stack frame adjustment)
- `relocation-model = "static"` — the kernel is loaded exactly at `0x100000`, no relocations needed
- Custom linker script (see 05-elf-boot-protocol.md)
- No `eh_frame`, no `comment`, no `note` sections — discarded to save space

## Build Targets

| Profile | Target triple | Uses std |
|---|---|---|
| Debug/Release (lodaxos-system) | host | yes (cargo default) |
| Debug/Release (lodaxos-kernel) | x86_64-unknown-none (custom) | no (`build-std`) |
| Debug/Release (lodaxos-boot) | x86_64-unknown-uefi | no |
| Debug/Release (lodaxos-chain) | x86_64-unknown-uefi | no |

