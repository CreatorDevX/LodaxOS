# 01 — Crate Structure

## Workspace Topology

The workspace `Cargo.toml` at root defines six members with resolver version 2:

```toml
[workspace]
members = ["system", "shared", "chain", "boot", "kernel", "sr"]
resolver = "2"
```

The dependency graph is a directed acyclic graph:

```
lodaxos-system  (no deps)
      ↓
lodaxos-core    (depends on lodaxos-system)
      ↓
lodaxos-kernel  (depends on lodaxos-core, lodaxos-system)
      ↓
lodaxos-boot    (depends on lodaxos-core, lodaxos-system)
      ↓
lodaxos-chain   (depends on lodaxos-system)
lodaxos-sr      (depends on lodaxos-system)
```

`lodaxos-chain` depends only on `lodaxos-system` (not `lodaxos-core`) because the chainloader has its own inline serial driver — it does not need the full shared subsystem library.

`lodaxos-sr` is the Secure Runtime stub. It is built as a bare-metal `x86_64-unknown-none` ELF (custom `sr/target.json`, base `0xFFFF_9000_0000_0000`, code-model=large) and is loaded by the kernel into a higher-half staging buffer. The current implementation is a `loop { hlt }` placeholder that logs its own entry point and never executes.

## Crate Purposes

### `lodaxos-system` (`system/`)

**Purpose**: Pure type definitions shared across all boot stages. Zero dependencies, `#![no_std]`.

**Contents**:
- `BootInfo` struct — the inter-stage communication structure at physical address `0x1000`
- `FramebufferInfo` — GOP framebuffer metadata (address, resolution, stride, pixel format)
- `MemoryRegion` — (phys_start, size) pair for free memory regions
- Constants: `BOOT_INFO_ADDR` (`0x1000`), `MAX_MEMORY_REGIONS` (`128`)

**Rationale**: Separating types into their own crate avoids circular dependencies and ensures that every boot stage agrees on the exact memory layout of the handoff struct. A single-byte misalignment between chainloader and kernel would cause silent data corruption.

### `lodaxos-core` (`shared/`)

**Purpose**: Canonical implementation of all kernel subsystems, re-exported for use by both the kernel and the bootloader.

**Contents**: Re-exports via one-liner wrappers:
- `serial.rs` — `include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/serial.rs"));`
- `logger.rs` — thin wrapper re-exporting `src/logger.rs`
- `task.rs` — `include!()` macro includes `src/task.rs` directly
- `font.rs` — thin wrapper
- `arch/` module — re-exports `src/arch/*`
- `acpi/` module — re-exports `src/acpi/*`
- `mm/` module — re-exports `src/mm/*`
- `intr/` module — re-exports `src/intr/*`

**Rationale**: The bootloader and kernel need the same serial driver, the same logger, the same memory allocators. Rather than compile the same code twice (or worse, maintain two copies), the shared crate uses `include!` to pull in the canonical source. Each target crate then re-exports from `lodaxos_core::*`.

### `lodaxos-chain` (`chain/`)

**Purpose**: First-stage UEFI chainloader. Its job is minimal: initialize the serial port, write a skeleton `BootInfo` at `0x1000`, read `Bootloader.efi` from the ESP, and chainload it via `uefi::boot::load_image` + `start_image`.

**Key design choices**:
- Own inline serial init (raw `out` instructions) instead of depending on `lodaxos-core` — keeps the chainloader small and independent
- Does not parse ext4 — that's the bootloader's job
- Does not exit boot services — that's the bootloader's job
- Captures memory map and framebuffer info, writes them to `BootInfo`, then hands off

**Why two-stage?** The chainloader is a simple PE32+ on FAT32. The bootloader is a larger binary that includes a full ext4 parser and ELF loader. Separating them lets the chainloader stay small (386 KB) and reliable.

### `lodaxos-boot` (`boot/`)

**Purpose**: Second-stage UEFI bootloader. Runs after being loaded by the chainloader. Its responsibilities:
1. Refines the framebuffer via GOP (explicit mode set)
2. Re-collects the UEFI memory map (allocations from chainload may have changed it)
3. Loads `kernel.elf` from the ext4 partition using its own ext4 filesystem parser
4. Captures the ACPI RSDP from the UEFI configuration table
5. Writes the updated `BootInfo` back to `0x1000`
6. Calls `exit_boot_services()`
7. Loads the kernel ELF segments into physical memory
8. Jumps to the kernel entry point

**Key design choices**:
- Self-contained ext4 parser — no external crate dependency for filesystem reading
- Must capture RSDP *before* `exit_boot_services` — after that, UEFI runtime services are gone
- Must `cli` immediately after `exit_boot_services` — stale UEFI timer interrupts would triple-fault without our IDT

### `lodaxos-kernel` (`kernel/`)

**Purpose**: The bare-metal operating system kernel. Compiled for a custom `x86_64-unknown-none` target with its own linker script and target specification.

**Key design choices**:
- `code-model = "kernel"` — allows the kernel to be linked at `0x100000` while accessing higher-half addresses via static relocations
- `disable-redzone = true` — essential for x86-64 interrupt handlers (the red zone would be corrupted if an interrupt fires between a function call and its stack frame adjustment)
- `relocation-model = "static"` — the kernel is loaded exactly at `0x100000`, no relocations needed
- Custom linker script (see 05-elf-boot-protocol.md)
- No `eh_frame`, no `comment`, no `note` sections — discarded to save space

## Module Wrapper Pattern

Each target crate has thin module stubs that re-export from the shared crate:

```rust
// kernel/src/serial.rs
pub use lodaxos_core::serial::*;
```

```rust
// kernel/src/mm/mod.rs
pub use lodaxos_core::mm::*;
```

This lets kernel code write `crate::serial::write_str(...)` or `crate::mm::phys::alloc_page()` without caring whether the implementation lives in `kernel/src/` or `shared/src/`. The bootloader uses the exact same pattern with `boot/src/` stubs.

For `task.rs`, which is tightly coupled to the kernel's `TrapFrame` and `mm::phys`/`mm::virt`, the shared crate uses `include!` directly:

```rust
// shared/src/task.rs
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/task.rs"));
```

This means the canonical `src/task.rs` is compiled as part of `lodaxos-core`, and both `kernel::task` and `boot::task` re-export from it. The bootloader does not actually use the task scheduler, but the code compiles because the linker discards unused symbols.

## Build Targets

| Profile | Target triple | Uses std |
|---|---|---|
| Debug/Release (lodaxos-system) | host | yes (cargo default) |
| Debug/Release (lodaxos-core) | host | yes |
| Debug/Release (lodaxos-kernel) | x86_64-unknown-none (custom) | no (`build-std`) |
| Debug/Release (lodaxos-boot) | x86_64-unknown-uefi | no |
| Debug/Release (lodaxos-chain) | x86_64-unknown-uefi | no |

The kernel and bootloader both compile `lodaxos-core`. This is intentional: they run at different privilege levels and different environments, and the shared code (serial, logging, memory algorithms) is designed to work in both contexts. The kernel links against `lodaxos-core` via Rust's standard crate resolution. The bootloader does the same.
