# LodaxOS Architecture Reference

This document is automatically assembled from modular files in docs/architecture/.
To rebuild: .\docs\architecture\build-architecture.ps1

To edit a section, modify the corresponding file in docs/architecture/ and rebuild.


---

# 00 â€” System Overview

## Philosophy

LodaxOS is built on three architectural axioms:

1. **The kernel is absolute.** It owns scheduling, memory, IPC primitives, capability enforcement, and interrupt infrastructure. It contains zero policy logic, zero driver logic, and zero business logic.

2. **Everything outside the kernel is replaceable.** Filesystems, drivers, device management, user interfaces, and runtime environments are all implemented as processes managed by Secure Runtime. If any of them fail, the kernel remains intact and recovery is possible without reboot.

3. **Recovery is layered from bottom to top.** A failure in an application restarts only the application. A failure in PyI restarts the user runtime. A failure in Secure Runtime triggers kernel-assisted recovery. A kernel panic is the last resort.

## Current Implementation State

LodaxOS currently implements only the kernel layer and the boot chain. The Secure Runtime, PyI, Agent framework, and driver services are future work. This document describes both what exists today and what is planned.

### What Exists Today

- 5-crate Rust workspace producing UEFI-compatible binaries
- Two-stage UEFI boot chain (chainloader â†’ bootloader â†’ kernel)
- Bare-metal x86-64 kernel with full interrupt handling
- 4-level page tables with higher-half mapping
- Buddy-based physical page allocator (orders 0-10)
- SLUB-style slab heap allocator with demand-paged VMA support
- LAPIC/IOAPIC interrupt controller drivers
- ACPI RSDP/MADT/XSDT discovery and parsing
- Preemptive CFS (Completely Fair Scheduler) task scheduler with syscall interface
- Self-contained ext4 filesystem reader (bootloader only)
- UEFI GOP framebuffer with bitmap font rendering
- SMP support via INIT-SIPI-SIPI (up to 4 CPUs)
- Secure Runtime (planned): policy process with forked PML4 and shared mailbox for dynamic capability brokering

### What Is Planned (see 10-future-architecture.md)

- Secure Runtime: userspace service manager, policy engine, capability broker
- PyI: JIT-compiled Python/WASM runtime for userspace applications
- Agent model: first-class system domains with isolated userspace environments
- Driver services: device-specific logic outside the kernel
- PCI enumeration, MSI/MSI-X
- Layered recovery from application through kernel

## System Structure

```
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚                     User Applications                            â”‚
â”‚  (Editor, Browser, Terminal â€” each a PyI subprocess)             â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚                     PyI Runtime                                  â”‚
â”‚  (JIT WASM-backed Python, REPL, UI layer)                        â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚                   Secure Runtime                                 â”‚
â”‚  (Service manager, policy engine, capability broker, Agent mgmt)  â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚                   Kernel (Ring 0)                                â”‚
â”‚  (Scheduler, memory, IPC, capability enforcement, HAL)            â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚             UEFI Boot Chain (x86-64)                             â”‚
â”‚  (OVMF â†’ chainloader â†’ bootloader â†’ kernel)                      â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚                    Hardware                                      â”‚
â”‚  (CPU, LAPIC, IOAPIC, HPET/PIT, UART, PCI bus)                  â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

## Design Tenets

1. **Static linking for the kernel.** No runtime module loading. The kernel is a single ELF binary loaded at 0x100000. All subsystems are compiled in.

2. **No heap in interrupt context.** Interrupt handlers must not allocate. The `TrapFrame` lives on the interrupt stack. The scheduler modifies it in-place.

3. **Identity mapping until page tables are ready.** The bootloader and early kernel run on UEFI's identity mapping. The kernel builds its own page tables before switching CR3.

4. **Higher-half kernel.** All kernel code and data are mapped in the upper virtual address range starting at 0xFFFF_8000_0000_0000. The lower half is reserved for userspace.

5. **Spinlocks only.** The kernel uses spinlocks (atomic CAS) for all synchronization. No blocking locks, no IRQ-safe variants â€” interrupts are disabled during critical sections by convention.

6. **No floating point in the kernel.** x87, SSE, AVX registers are not saved or restored during context switches. FPU use requires explicit opt-in with save/restore (future work).

7. **Framebuffer is not double-buffered.** The kernel writes directly to the hardware framebuffer. No compositor, no window manager â€” text and splash output only.

## Namespace Convention

| Prefix | Meaning |
|---|---|
| `HF_` | Hard Fault â€” kernel-level halt required |
| `SF_` | Soft Fault â€” SR-level recovery possible |
| `KERNEL_` | Kernel internal interface |
| `SR_` | Secure Runtime interface |
| `CAP_` | Capability identifier |
| `AGENT_` | Agent domain constants |

Fault codes follow a flat hex numbering: `0x0x` = hard fault, `0x1x` = soft fault.

## File Layout

Each crate has its own `src/` directory with independent implementations:
- `system/src/` â€” shared type definitions (BootInfo, Caps, Mailbox)
- `chain/src/` â€” first-stage UEFI chainloader
- `boot/src/` â€” second-stage UEFI bootloader (ext4 parser, ELF loader)
- `kernel/src/` â€” bare-metal kernel


There is no shared `src/` root directory or `shared/` crate. Each crate is self-contained and depends only on `lodaxos-system` for type definitions.

---

# 01 â€” Crate Structure

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
      â†“
lodaxos-kernel  (depends on lodaxos-system)
lodaxos-boot    (depends on lodaxos-system)
lodaxos-chain   (depends on lodaxos-system)
```

Each crate is self-contained â€” there is no shared implementation crate. The kernel and bootloader both have their own independent copies of serial, logger, and other infrastructure code. This is intentional: they run at different privilege levels and different environments (UEFI vs bare metal), and the amount of shared code is small.

## Crate Purposes

### `lodaxos-system` (`system/`)

**Purpose**: Pure type definitions shared across all boot stages. Zero dependencies, `#![no_std]`.

**Contents**:
- `BootInfo` struct â€” the inter-stage communication structure (dynamically allocated; 8-byte pointer at `0x1000` = `BOOT_INFO_HANDOFF_ADDR`)
- `FramebufferInfo` â€” GOP framebuffer metadata (address, resolution, stride, pixel format)
- `MemoryRegion` â€” (phys_start, size) pair for free memory regions
- `Caps` / `CapOp` / `CapError` / `CapResponse` / `CapRequest` / `Mailbox` â€” capability-system types and kernelâ†”policy-process IPC page (reserved for future use)
- Constants: `BOOT_INFO_HANDOFF_ADDR` (`0x1000`), `MAX_MEMORY_REGIONS` (`128`), `MAX_CPUS` (`4`)

**Rationale**: Separating types into their own crate avoids circular dependencies and ensures that every boot stage agrees on the exact memory layout of the handoff struct. A single-byte misalignment between chainloader and kernel would cause silent data corruption.

### `lodaxos-chain` (`chain/`)

**Purpose**: First-stage UEFI chainloader. Its job is minimal: initialize the serial port, write a skeleton `BootInfo` at `0x1000`, read `Bootloader.efi` from the ESP, and chainload it via `uefi::boot::load_image` + `start_image`.

**Key design choices**:
- Own inline serial init (raw `out` instructions) â€” keeps the chainloader small and independent
- Does not parse ext4 â€” that's the bootloader's job
- Does not exit boot services â€” that's the bootloader's job
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
- Self-contained ext4 parser â€” no external crate dependency for filesystem reading
- Must capture RSDP *before* `exit_boot_services` â€” after that, UEFI runtime services are gone
- Must `cli` immediately after `exit_boot_services` â€” stale UEFI timer interrupts would triple-fault without our IDT
- UEFI MP Services is only used to *enumerate* APs â€” the kernel brings them up via INIT-SIPI-SIPI after ExitBootServices

### `lodaxos-kernel` (`kernel/`)

**Purpose**: The bare-metal operating system kernel. Compiled for a custom `x86_64-unknown-none` target with its own linker script and target specification.

**Key design choices**:
- `code-model = "kernel"` â€” allows the kernel to be linked at `0x100000` while accessing higher-half addresses via static relocations
- `disable-redzone = true` â€” essential for x86-64 interrupt handlers (the red zone would be corrupted if an interrupt fires between a function call and its stack frame adjustment)
- `relocation-model = "static"` â€” the kernel is loaded exactly at `0x100000`, no relocations needed
- Custom linker script (see 05-elf-boot-protocol.md)
- No `eh_frame`, no `comment`, no `note` sections â€” discarded to save space

## Build Targets

| Profile | Target triple | Uses std |
|---|---|---|
| Debug/Release (lodaxos-system) | host | yes (cargo default) |
| Debug/Release (lodaxos-kernel) | x86_64-unknown-none (custom) | no (`build-std`) |
| Debug/Release (lodaxos-boot) | x86_64-unknown-uefi | no |
| Debug/Release (lodaxos-chain) | x86_64-unknown-uefi | no |

---

# 02 â€” Memory Model

## Overview

LodaxOS uses a four-tier memory model:

1. **Physical memory** â€” managed by a buddy allocator with per-order free lists (orders 0â€“10, 4 KB â€“ 4 MB)
2. **Virtual memory** â€” 4-level page tables with higher-half kernel mapping
3. **Slab heap** â€” SLUB-style allocator with per-size caches (32 B â€“ 8 KB), backed by buddy pages
4. **VMA / Demand paging** â€” radix-tree-based VMA tracking with per-page fault resolution

## Physical Memory Layout

### Address Space (Standard x86-64)

```
0x0000_0000_0000 â€”â€”â€”â€” Reserved (real-mode IVT, BDA)
0x0000_0000_1000 â€”â€”â€”â€” BootInfo pointer (8 bytes, chainloader â†’ bootloader â†’ kernel)
0x0000_0000_2000 â€“ 0x0000_0009_FFFF â€”â€”â€”â€” Usable (below 640 KB)
0x0000_000A_0000 â€“ 0x0000_000F_FFFF â€”â€”â€”â€” Legacy hole (VGA, BIOS ROM)
0x0000_0010_0000 â€”â€”â€”â€” Kernel loaded here (0x100000 = 1 MB)
0x0000_0010_0000 â€“ 0x0000_00FF_FFFF â€”â€”â€”â€” Kernel segments (text, rodata, data, bss)
0x0000_0100_0000 â€“ 0x0000_07FF_FFFF â€”â€”â€”â€” Free (first 128 MB after kernel)
0x0000_0800_0000 â€“ 0xFFFF_FFFF_FFFF â€”â€”â€”â€” Free memory (up to 4 TB physical)
```

### Reserved Physical Pages

| Address | Purpose | Reason |
|---|---|---|
| `0x0000_0000` | Null guard | Rust UB on null pointer dereference |
| `0x0000_1000` | BootInfo handoff pointer | Inter-stage communication (8 bytes) |
| `boot_info_phys` page(s) | Dynamically-allocated `BootInfo` struct | The chainloader writes this address into `0x1000`; the buddy allocator reserves it during `init_from_regions` so it can never be re-issued |
| Buddy-allocated | All dynamic allocations | Pages acquired from buddy allocator |

The BootInfo struct itself is no longer at a fixed address. The chainloader allocates it dynamically via `Box::new(BootInfo)` (backed by UEFI's page allocator, which identity-maps the result) and stores the physical pointer at `0x1000`. The bootloader reads this pointer, updates the BootInfo fields, and passes the same pointer in RDI to the kernel. The kernel's `phys::init_from_regions` receives the pointer as its `boot_info_phys` argument and reserves the page(s) covering it from the buddy free lists; the same range is also checked by `is_reserved_page` on free paths.

## Physical Page Allocator â€” Buddy System (`kernel/src/mm/phys.rs`)

### Data Structure

A single `Zone` with 11 free lists (one per order):

```
Zone {
    base: u64,                    // lowest physical address managed
    top: u64,                     // highest physical address + 1
    free_lists: [FreeBlock; 11],  // linked list heads per order 0..10
    total_pages: AtomicUsize,
    free_pages: AtomicUsize,
    lock: SpinLock,
}
```

Each `FreeBlock` is stored inline within free pages (zero metadata overhead). The linked list is singly-linked via an `Option<&'static mut FreeBlock>` next pointer.

### Orders

| Order | Page Count | Block Size |
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

### Initialization (`init_from_regions`)

1. Given all `(phys_start, size)` usable memory regions from BootInfo, compute `base` (first region start) and `top` (last region end).
2. For each region, trim unaligned edges to enforce power-of-2 alignment.
3. Within each trimmed region, carve the largest possible power-of-2 blocks (greedy from high order down).
4. Insert each block into the appropriate free list.
5. Skip the first page (`0x0000_0000`) and the BootInfo handoff page (`0x0000_1000`) â€” these are always reserved.

### Allocation (`alloc_order(n)`, `alloc_page()`, `alloc_pages(count)`)

**Single order** (`alloc_order(n)`):
1. Pop from `free_lists[n]` if non-empty â†’ O(1).
2. If empty, search upward for the next non-empty order O(10 - n).
3. Split the found block: free the lower half at order-1, return the upper half. Repeat until the desired order is reached.
4. Result: a block of 2^n contiguous pages, aligned to 2^n boundaries.

**Single page** (`alloc_page()`): Calls `alloc_order(0)`.

**Multiple pages** (`alloc_pages(count)`): Rounds up to the smallest covering order, calls `alloc_order(n)`.

### Deallocation (`free_order(addr, n)`, `free_page(addr)`)

**`free_order(addr, n)`**:
1. Compute buddy address: `addr ^ (1 << (n + 12))` (XOR bit n in the page-number space).
2. If the buddy is also free and at order n, remove it from `free_lists[n]` and recurse at order n+1 (coalesce).
3. Otherwise, insert the current block into `free_lists[n]`.
4. Refuse to free reserved pages (0, 0x1000, and the dynamically-allocated `BootInfo` page(s) recorded in `BOOTINFO_RESERVED_BASE` / `BOOTINFO_RESERVED_PAGES`).

### Thread Safety

A `SpinLock` (implemented as `AtomicBool` with `compare_exchange_weak` + `pause` loop) protects all zone operations. The lock disables interrupts during critical sections via `cli; lock; cmpxchg; sti`. The allocator is callable from multiple CPU cores and from interrupt context (though interrupt handlers should not allocate by design).

### Performance Characteristics

- Allocation: O(orders) worst case (split chain), O(1) when target order is non-empty.
- Deallocation: O(orders) worst case (coalesce chain), O(1) when buddy is busy.
- Internal fragmentation: At most (2^n - 1) pages per allocation; worst case < 50% for misaligned sizes.
- External fragmentation: None within the buddy system (coalescing is greedy and complete).

## Virtual Memory (`kernel/src/mm/virt.rs`)

### Address Space Layout

```
0x0000_0000_0000_0000 â€“ 0xFFFF_7FFF_FFFF_FFFF â€”â€”â€”â€” Userspace (48-bit)
0xFFFF_8000_0000_0000 â€“ 0xFFFF_FFFF_FFFF_FFFF â€”â€”â€”â€” Kernel higher-half
   0xFFFF_8000_0000_0000 â€“ physical_memory_end â€”â€”â€”â€” Physical memory mapping
   0xFFFF_8080_0000_0000 â€“ 0xFFFF_8084_0000_0000 â€”â€”â€”â€” Heap / slab arena (up to 64 MB)
```

All physical memory is identity-mapped (PML4[0] â†’ 4 PDP entries covering 0â€“4 GB) and also mapped in the higher-half at `HIGHER_HALF + phys_addr`.

### Page Table Structure

4-level translation (PML4 â†’ PDP â†’ PD â†’ PT â†’ 4 KB page), plus support for 2 MB huge pages at the PD level and 1 GB huge pages at the PDP level.

```
PML4 (1 entry) â†’ PDP (512 entries)
  Each PDP entry covers 1 GB (512 Ã— 2 MB)
  PDP[0..3] = identity-map first 4 GB with 2 MB huge pages
  Other PDP entries = higher-half mappings

PDP (1 entry) â†’ PD (512 entries)
  Each PD entry covers 2 MB
  If bit 7 (PS) = 1: 2 MB huge page
  If bit 7 = 0: points to PT

PD (1 entry) â†’ PT (512 entries)
  Each PT entry covers 4 KB
  PT entries point to 4 KB physical pages
```

### Initialization (`virt::init`)

Phase 1â€“5: Allocate PML4, map higher-half for all boot regions (mix 2 MB huge + 4 KB), identity-map first 4 GB, map framebuffer, load CR3. See the `init` function and inline comments for details.

### Key API

| Function | Purpose |
|---|---|
| `translate(virt)` | Walk current page tables to resolve virtual â†’ physical |
| `unmap(virt)` | Clear PT entry, flush TLB with `invlpg` |
| `map_page(pml4, virt, phys, flags)` | Create a single 4 KB page table mapping |
| `map_page_explicit(pml4, virt, phys, flags)` | Public (non-`unsafe`) wrapper for guarded callers |
| `map_contiguous(pml4, virt, phys, num_pages, flags)` | Map many pages with batch PT walk |
| `map_region(pml4, phys, size, flags)` | Identity + higher-half mapping |
| `map_region_higher_half(pml4, phys, size, flags)` | Higher-half only (for MMIO) |

### Higher-Half Only for MMIO

The identity map uses 2 MB huge pages. Creating a 4 KB page at the same PD level would conflict (the CPU would see the 4 KB entry's flags, but the PD entry is marked as a huge page). Therefore, MMIO regions like LAPIC and IOAPIC are mapped only in the higher-half, with smaller pages that coexist at different virtual addresses referring to the same physical memory. This is a known workaround â€” the proper fix is to split the PDP entry into 512 PD entries and mark the MMIO 2 MB slot with cache-disable, but that has not been implemented yet.

## Slab Heap Allocator (`kernel/src/mm/heap.rs`)

### Design (SLUB-inspired)

A fixed set of `KmemCache`s, each dedicated to objects of a specific size:

| Cache Index | Object Size | Max Objects per Slab |
|---|---|---|
| 0 | 32 B | 128 |
| 1 | 64 B | 64 |
| 2 | 128 B | 32 |
| 3 | 256 B | 16 |
| 4 | 512 B | 8 |
| 5 | 1024 B | 4 |
| 6 | 2048 B | 2 |
| 7 | 4096 B | 1 |
| 8 | 8192 B | 1 (uses order 1 block) |

### Slab Structure

Each slab is a block of pages obtained from the buddy allocator. The slab metadata is stored at the beginning of the first page:

```c
struct Slab {
    cache: *mut KmemCache,    // back-pointer to owning cache
    next: *mut Slab,          // linked list within cache
    prev: *mut Slab,
    free_list: *mut u8,       // head of free object linked list
    free_count: usize,
    total_objects: usize,
}
```

Free objects within a slab are linked via embedded free-list pointers (stored in the first 8 bytes of each free object). When an object is allocated, the free list pointer is simply popped. When freed, the object is pushed back onto the free list. No external metadata structures are needed.

### Cache Structure

Each `KmemCache` maintains three linked lists of slabs:

```
KmemCache {
    name: &'static str,
    object_size: usize,
    slab_order: usize,        // buddy order for each slab
    partial: Option<&'static mut Slab>,   // slabs with free + used objects
    free: Option<&'static mut Slab>,      // fully free slabs
    full: Option<&'static mut Slab>,      // fully used slabs
    gfp_flags: u64,
    lock: SpinLock,
}
```

### Allocation (`kmalloc(size)`)

1. Round `size` up to the nearest cache size (32 B .. 8 KB). Sizes > 8 KB fall back to `alloc_pages` directly.
2. Lock the cache.
3. Pop an object from the first partial slab's free list.
   - If no partial slab exists, try to pop a free slab and move it to partial.
   - If no free slab exists either, allocate a new slab via `alloc_pages`.
4. Move the slab to `full` if its free count reaches zero.
5. Return the object pointer.

### Deallocation (`kfree(ptr, size)`)

1. Round `size` up to determine the owning cache.
2. The `ptr` itself encodes no cache information â€” size must match allocation.
3. Locate the slab by scanning `partial` and `full` lists (linear scan; acceptable for small lists).
   - Future optimization: embed a slab pointer in the object header area.
4. Push the object onto the slab's free list.
5. If the slab becomes fully free, move it to the `free` list.
6. Free slabs are not returned to the buddy â€” they remain cached for reuse. (Future: periodic reclamation.)

### Global Allocator

`GlobalAllocator` implements `core::alloc::GlobalAlloc`, delegating to `kmalloc/kfree`. It is installed as `#[global_allocator]`. A static `initialized` flag gates allocation before the slab system is ready; early `alloc` calls return null (the `alloc` crate panics gracefully).

### Virtual Address Range

The slab system does not pre-map a fixed heap arena. Each new slab allocates physical pages from the buddy allocator and maps them directly at `HEAP_VIRT_BASE + offset` using `map_contiguous`. The virtual arena is a contiguous 64 MB region starting at `0xFFFF_8080_0000_0000`.

### Thread Safety

Each `KmemCache` has its own `SpinLock`. The `GlobalAllocator` dispatches to the correct cache by size. Different-size allocations can proceed in parallel on different cores (fine-grained locking), but same-size allocations on different cores serialize.

## VMA / Demand Paging (`kernel/src/mm/vma.rs`)

### Radix Tree

The VMA tree uses a 4-level radix tree covering bits 12â€“51 of the virtual address (40 bits = 1 TB addressable per tree). Each level indexes 10 bits. The implementation labels levels from the leaf upward, matching the `radix_shift` formula `PAGE_SHIFT + level * RADIX_BITS`:

```
Level 0 (bits 21:12) â†’ Level 1 (bits 31:22) â†’ Level 2 (bits 41:32) â†’ Level 3 (bits 51:42)
```

Each node is a `RadixNode` with 1024 8-byte entries (8 KB per node). A `RadixEntry` is a tagged union: an interior entry is a `*mut RadixNode` (child), a leaf entry is a `*mut Vma`.

### VMA Struct

```rust
struct Vma {
    start: u64,         // virtual start address (page-aligned)
    end: u64,           // virtual end address (exclusive, page-aligned)
    perm: VmaPerm,      // permission bits (None, Read, Write, Execute, â€¦)
    flags: u64,         // VMA flags (reserved for future Kernel/User/Guard)
}
```

### VMA Tree Operations

| Operation | Description |
|---|---|
| `insert(vma)` | Walk the 4-level tree from level 3 down to level 1, allocate interior nodes as needed, then store the VMA pointer in the level-0 leaf slot. |
| `lookup(addr)` | Walk the tree to the level-0 leaf slot, return the VMA if the entry is non-null. |
| `find_covering(addr)` | Visit all VMAs in the tree (linear scan via `visit_all`) and return the first one where `addr âˆˆ [start, end)`. |
| `remove(start)` | Walk to the level-0 leaf slot for `start`, null out the entry, return the previous VMA pointer. |
| `visit_all(f)` | Recursively traverse all leaf slots, apply `f` to each VMA until it returns `Some`. |

`find_covering` does a linear scan of up to 1024 VMA entries in the target leaf. With <100 VMAs per process in practice, this is fast enough. The tree structure makes it O(1) to locate the correct leaf slot.

### Global Kernel VMA Tree

A single `static KERNEL_VMA_TREE: VmaTree` tracks all kernel-mode VMAs. Initialized by `init_kernel_vmas()`:

1. `kernel_code` â€” `0xFFFF_8000_0000_0000` to `kernel_end` (Read + Execute, backed by bootloader identity map)
2. `kernel_data` â€” `kernel_end` to `0xFFFF_8000_0020_0000` (Read + Write, backed by identity map)
3. `kernel_heap` â€” `HEAP_VIRT_BASE` to `HEAP_VIRT_BASE + 64 MB` (Read + Write, demand-paged)
4. `kernel_mmio` â€” `HIGHER_HALF + 0xF0000000` to `HIGHER_HALF + 0xFFFFFFFF` (Read + Write + Uncached, backed by identity map)

Additional VMAs may be inserted during kernel init (e.g., for framebuffer).

### Page Fault Handler (`handle_page_fault(addr, error_code)`)

Called from the #PF handler in `kernel/src/arch/idt.rs`:

1. Read CR2 (the faulting address).
2. If the fault originated in user mode (`error_code & 4`): walk the current process's `ProcessMemory` tree (future: per-process page tables).
3. If the fault originated in kernel mode: walk `KERNEL_VMA_TREE`.
4. If no covering VMA is found: panic/halt â€” this is an unhandled page fault (bug or access violation).
5. If a covering VMA exists:
   a. Allocate a physical page from the buddy allocator.
   b. Zero the page.
   c. Call `map_page_explicit(pml4, addr, phys, DATA_FLAGS)` to insert the page table entry.
   d. Return `true` to the IDT handler, which resumes execution at the faulting instruction.
6. The faulting instruction retries, now with the page mapped.

### Per-Process Memory (Future)

The `ProcessMemory` struct wraps a `VmaTree` and a `PML4` pointer:

```c
struct ProcessMemory {
    tree: VmaTree,
    pml4: u64,           // physical address of process PML4
}
```

Each process will have its own page tables and VMA tree. The scheduler will switch PML4 on context switch. User-mode page faults will walk the process's VMA tree.

### Performance Characteristics

- VMA lookup: O(1) tree walk + O(n) scan in leaf (n â‰¤ 1024, typically <100).
- Page fault resolution: O(1) buddy allocation + O(4) page table walks (4 levels).
- Memory overhead: Each VMA tree node is 8 KB (1024 Ã— 8-byte slots). A tree with 1 VMA uses ~8 KB for the root; a tree with 1000 VMAs uses ~32 KB (4 nodes at 3 levels Ã— 8 KB + ~16 KB for VMA Boxes).

## Page Table Entry Flags

| Flag | Bit | Purpose |
|---|---|---|
| `PRESENT` | 0 | Entry is valid |
| `WRITABLE` | 1 | Writes allowed (ring 0 only with WP=0) |
| `USER` | 2 | Accessible from ring 3 |
| `CACHE_DISABLE` (PCD) | 4 | Uncached (for MMIO) |
| `PS` | 7 | Page size (2 MB at PD, 1 GB at PDP) |
| `NX` | 63 | No execute |

## Thread Safety Summary

| Component | Lock Type | Granularity |
|---|---|---|
| Buddy allocator (`Zone`) | SpinLock with IRQ disable | Entire zone |
| Slab allocator (each `KmemCache`) | SpinLock with IRQ disable | Per cache (9 total) |
| VMA tree (`KERNEL_VMA_TREE`) | None (kernel init only; PS/2 disabled) | N/A |

The buddy allocator global lock is coarse but acceptable because buddy operations are extremely fast (pointer pops/pushes in the common case). The slab allocator's per-cache locking allows concurrent allocations at different sizes.

## Key File Reference

| File | Component |
|---|---|
| `kernel/src/mm/phys.rs` | Buddy allocator |
| `kernel/src/mm/heap.rs` | Slab allocator |
| `kernel/src/mm/vma.rs` | Radix tree, VMA, page fault handler |
| `kernel/src/mm/virt.rs` | Page table management |
| `kernel/src/mm/mod.rs` | Module declarations |
| `kernel/src/arch/idt.rs` | IDT entry points (including #PF) |

## Migration from Previous System

The previous memory model used:
- **Bitmap allocator** (`phys.rs`): O(n/64) linear scan, high external fragmentation with multi-page allocations.
- **Linked-list allocator** (`heap.rs`): First-fit with O(n) scan, no slab caching for small objects.
- **Fixed BootInfo at `0x1000`**: Hard-coded address limited BootInfo size and conflicted with kernel layout.

The new system replaces all three with minimal memory overhead and O(1) common-case allocation. The BootInfo handoff is fully dynamic, removing the fixed-address constraint.

---

# 03 â€” Interrupt Model

## Overview

LodaxOS uses a modern interrupt architecture based on the Local APIC and I/O APIC, bypassing the legacy 8259 PIC entirely. The interrupt flow is:

```
Hardware device â†’ IOAPIC (pin) â†’ LAPIC (vector) â†’ CPU (IDT entry) â†’ stub â†’ dispatcher â†’ handler
```

## Interrupt Controller Hierarchy

### Legacy PIC (8259)

The 8259 PIC is disabled. Immediately after `exit_boot_services`, the kernel writes `0xFF` to both PIC mask registers (ports `0x21` and `0xA1`), masking all 16 IRQs. This prevents the PIC from delivering interrupts that would collide with the kernel's IDT vectors (the PIC's default vector mappings, 0x08â€“0x0F, overlap with CPU exception vectors 8â€“15).

Despite being masked, the PIC still receives IRQ assertions from ISA devices. These go nowhere â€” the CPU does not acknowledge the PIC when the LAPIC is the sole interrupt controller.

### Local APIC

The LAPIC is discovered by reading the `IA32_APIC_BASE` MSR (0x1B) at boot. Its MMIO region (typically `0xFEE00000`) is mapped into the higher-half page tables with cache-disable flag.

Initialization sequence:
1. Read MSR to get physical base address
2. Map 4 KB MMIO region (`map_region_higher_half`, with PCD flag)
3. Mask LINT0 and LINT1 (prevent extINT deliveries from PIC)
4. Initialize LVT Error register (vector 0xFF, masked)
5. Set Task Priority Register to 0 (accept all interrupts)
6. Set Spurious Interrupt Vector Register with enable bit

### I/O APIC(s)

Discovered from the MADT (Multiple APIC Description Table), which is parsed from ACPI tables. Each IOAPIC has:
- MMIO base address (typically `0xFEC00000` for the first IOAPIC)
- GSI (Global System Interrupt) base â€” the starting GSI number handled by this IOAPIC
- Maximum redirection entry index (determined by the IOAPIC version register)

Initialization:
1. For each IOAPIC in the MADT, map its 4 KB MMIO region (higher-half, cache-disabled)
2. Read hardware ID from IOAPICID register
3. Read version from IOAPICVER register
4. Mask ALL redirection entries with safe values (vector 0xFF, masked)

## Interrupt Routing

### Vector Space

```
Vector 0â€“31:    CPU exceptions (#DE, #DB, NMI, #BP, #OF, ... #DF, #GP, #PF, ...)
Vector 32:      LAPIC timer (scheduler heartbeat)
Vector 33â€“63:   Device IRQs (PIT, keyboard, etc.)
Vector 64â€“127:  Reserved for future devices (MSI/MSI-X)
Vector 128â€“254: Reserved
Vector 255:     Spurious interrupt (LAPIC)
```

### Route Construction (from MADT)

The MADT contains Interrupt Source Override (ISO) entries that map ISA IRQ sources to GSIs. For example, on most modern systems:
- ISA IRQ 0 (PIT) â†’ GSI 2
- ISA IRQ 1 (keyboard) â†’ GSI 1

The interrupt routing table (`kernel/src/intr/mod.rs`) is built as follows:

1. For each ISO entry with `bus == 0`:
   - Record the ISA IRQ source â†’ GSI mapping
   - Allocate a unique vector (33â€“63)
   - Look up which IOAPIC handles this GSI and what pin it corresponds to
   - Create an `IrqRoute` with all this information

2. For ISA IRQs 0â€“15 without ISO entries:
   - Identity-map: GSI = ISA IRQ
   - Same vector allocation and IOAPIC lookup

3. All routes are initially installed as MASKED (bit 16 set in the IOAPIC redirection entry)

### IOAPIC Redirection Entry Format

```
Low DWORD (32 bits):
  Bits 7:0   â€” Vector (IDT entry number)
  Bits 10:8  â€” Delivery mode (000 = Fixed)
  Bit 11     â€” Destination mode (0 = Physical)
  Bit 13     â€” Polarity (0 = active-high, 1 = active-low)
  Bit 15     â€” Trigger (0 = edge, 1 = level)
  Bit 16     â€” Mask (1 = masked, 0 = unmasked)
  Bits 31:17 â€” Reserved

High DWORD (32 bits):
  Bits 63:56 â€” Destination APIC ID
```

## IDT Layout

256 entries, each 16 bytes (interrupt gate):

```
Bytes 1:0   â€” Offset[15:0]
Bytes 3:2   â€” Code segment selector (0x08)
Byte 4      â€” IST (0 = no IST, 1â€“7 = IST entry)
Byte 5      â€” Type/attributes (0x8E = interrupt gate, present)
Bytes 7:6   â€” Offset[31:16]
Bytes 11:8  â€” Offset[63:32]
Bytes 15:12 â€” Reserved
```

## Stub Generation

All 256 IDT stubs are generated by two macros:

```rust
define_stub_noerr!(name, vector)  // for exceptions without error codes + all IRQs
define_stub_err!(name, vector)    // for exceptions with error codes
```

### No-Error-Code Stub (typical IRQ)

```asm
push 0            ; dummy error code (for uniform TrapFrame)
push vector       ; vector number
push rdi          ; save all GPRs (15 pushes + 2 = 17 Ã— 8 = 136 bytes)
push rsi
...
push r15
mov rdi, rsp      ; arg1 = TrapFrame* (SysV ABI)
mov rcx, rsp      ; arg1 = TrapFrame* (Win64 ABI, unused in kernel)
sub rsp, 32       ; shadow space (Win64 ABI)
call dispatcher
add rsp, 32
pop r15           ; restore all GPRs
...
pop rdi
add rsp, 16       ; pop vector + error code
iretq
```

### Error-Code Stub (e.g., #PF, #GP)

Same but without the `push 0` â€” the CPU already pushed the error code.

## Interrupt Flow

### Hardware Interrupt

1. Device asserts interrupt pin on IOAPIC
2. IOAPIC looks up the pin's redirection entry, packages the vector + destination APIC ID
3. IOAPIC sends message to the target LAPIC
4. LAPIC accepts the interrupt, writes vector to the CPU's interrupt controller
5. CPU reads IDT entry for that vector, pushes interrupt frame (SS, RSP, RFLAGS, CS, RIP), checks descriptor privilege level
6. CPU jumps to stub
7. Stub saves all registers, calls dispatch
8. Dispatcher routes by vector:
   - 0â€“31: `exception_handler`
   - 32â€“63: `irq_handler`
   - 0x80: `syscall_handler`
   - 0xFF: no-op (spurious)

### IRQ Handler (vectors 32â€“63)

1. Send EOI to LAPIC (unmask further interrupts at the LAPIC level)
2. Dispatch by vector:
   - **Vector 32 (LAPIC timer)**: Increment tick counter, call `task::schedule(frame)` which may overwrite the TrapFrame with the next task's state for a preemptive context switch
   - **ISA IRQ 0 (PIT)**: Increment PIT tick counter
   - **ISA IRQ 1 (PS/2 keyboard)**: Read scancode byte from port 0x60, store in atomic variables
   - **Others**: No-op (silently ignored)

### Context Switch in Timer IRQ

The timer IRQ handler (vector 32) is where preemptive multitasking happens:

1. Increment global tick counter
2. Call `task::schedule(&mut frame)`
3. `schedule()` saves the current task's register state into its `Task` struct (copies `TrapFrame` + corrects RSP)
4. Finds the next ready task (CFS: minimum `vruntime`)
5. Overwrites `frame` with the next task's saved `TrapFrame`
6. Returns `true` indicating a switch was made
7. The handler executes `mov rsp, frame.rsp; push frame.cs; push frame.rip; push frame.rflags; popfq; retfq`
8. This restores the next task's register state and jumps to its next instruction

### Exception Handler

1. Log exception type, vector, RIP, error code
2. For #PF (14): read CR2 to get faulting address, log access type (read/write, user/kernel, present/not-present)
3. For #GP (13): log error code, decode segment selector if error code is non-zero
4. For #DF (8): log and halt (unrecoverable)
5. For #BP (3): log and return (breakpoint â€” continue execution)
6. All others: log full register state and halt

### Syscall Handler (vector 0x80)

Convention:
- `rax` = syscall number
- `rdi, rsi, rdx` = arguments
- Return value in `rax`

Implemented syscalls:
| NR | Name | Description |
|---|---|---|
| 0 | yield | No-op (preemptive timer handles scheduling) |
| 1 | exit | Block current task, reschedule |
| 2 | get_task_id | Return current task's ID |
| 3 | wake_task(task_id) | Unblock a blocked task |
| 4 | get_ticks | Return current tick count |

## Future Interrupt Architecture

### MSI/MSI-X

When PCI enumeration is implemented, MSI/MSI-X interrupts will be programmed by:
1. Allocating a unique vector from the device range (64â€“127)
2. Writing the message address (MSI: `0xFEE` + destination APIC ID) and message data (vector) to the PCI device's MSI capability
3. No IOAPIC involvement â€” MSIs go directly from the PCI bus to the LAPIC

### IPI (Inter-Processor Interrupts) â€” Implemented

IPIs are sent via the LAPIC's ICR (Interrupt Command Register) in xAPIC MMIO mode:
- Write destination APIC ID to `0x310` (ICR high)
- Write vector + delivery mode to `0x300` (ICR low)
- Poll for delivery status clear

Common IPI types:
- INIT (vector 0, ICR bit 11): Start a processor (used in INIT-SIPI-SIPI sequence)
- STARTUP (vector 0x467, bit 11): SIPI with startup address (used in INIT-SIPI-SIPI)
- Fixed: Deliver a specific vector (e.g., TLB shootdown, reschedule)

### NMI Handling

NMIs come through the LAPIC's LINT1 or via the IOAPIC as NMI delivery mode. The kernel currently has a stub at vector 2 that logs and halts. Future NMIs may include:
- Hardware watchdog expiration
- IOAPIC NMI pins (MADT entries of type 4)
- Performance counter overflow

---

# 04 â€” Task Scheduling

## Overview

LodaxOS implements preemptive multitasking with a **CFS (Completely Fair Scheduler)** style virtual-runtime scheduler. The scheduler is invoked from the LAPIC timer interrupt (vector 32, fired every 1 ms). Each task gets its own 8 KB kernel stack and runs in ring 0. There is no userspace isolation yet â€” all tasks run at the highest privilege level.

## CFS Constants

```rust
const VRUNTIME_TICK: u64 = 20;   // vruntime added per timer tick (~1 ms)
const VRUNTIME_BIAS: u64 = 8;    // subtracted from min vruntime for new tasks
```

All tasks are equal weight, so the `vruntime` field tracks the total time the task has spent on the CPU. `pick_next_ready` selects the task with the smallest `vruntime`, which approximates the Linux CFS "leftmost task" rule in the absence of nice values.

## Data Structures

### TrapFrame

The `TrapFrame` (176 bytes) captures the full CPU register state when an interrupt or exception occurs. It is laid out to match the push order of the assembly stubs:

```rust
#[repr(C)]
pub struct TrapFrame {
    r15, r14, r13, r12, r11, r10, r9, r8,  // callee-saved + temp
    rax, rbx, rcx, rdx,                      // general purpose
    rbp, rsi, rdi,                           // base, source, destination
    vector: u64,                             // pushed by stub
    error_code: u64,                         // pushed by stub (0 if no error)
    rip: u64,                                // CPU-pushed interrupt frame
    cs: u64,
    rflags: u64,
    rsp: u64,                                // only present if privilege change
    ss: u64,
}
```

The interrupt frame (RIP, CS, RFLAGS, RSP, SS) is pushed by the CPU hardware when an interrupt occurs. The stub pushes the vector, error code, and all GPRs on top of that.

### Task

```rust
pub struct Task {
    pub id: usize,
    pub saved_frame: TrapFrame,      // snapshot of registers when not running
    pub kernel_stack_base: u64,      // bottom of allocated 8 KB stack
    pub kernel_stack_top: u64,       // top of allocated 8 KB stack
    pub state: TaskState,            // Ready or Blocked
    pub vruntime: u64,               // CFS virtual runtime (lower = scheduled sooner)
}
```

### TaskManager

```rust
pub struct TaskManager {
    tasks: [Option<Task>; MAX_TASKS],  // max 32 tasks
    count: usize,                       // total tasks registered
    initialized: bool,
}

// Note: There is no `current` field â€” per-CPU tracking is in `percpu.rs`.
```

## Stack Layout

Each task (except task 0) gets an 8 KB kernel stack allocated from the physical page allocator and mapped into the higher-half virtual address space.

```
Kernel stack layout (high â†’ low addresses):

stack_base + 8192  â”€â”€â”€ top of allocated region
  [rflags]          â† RSP points here â†’ iretq pops this
  [cs]              â† iretq pops this
  [rip]             â† iretq pops this (task entry point)
  ...               â† usable stack space (grows down)
stack_base          â”€â”€â”€ bottom (TrapFrame stored here for save/restore)
```

When a task is created:
1. The bottom of the stack holds a synthetic `TrapFrame` (all registers zeroed, RIP = entry point, CS = 0x08, RFLAGS = 0x202 with IF enabled)
2. The top of the stack has an `iretq` frame: RIP (24 bytes below top), CS (16 bytes below top), RFLAGS (8 bytes below top)
3. The synthetic TrapFrame's RSP points to the iretq frame

When the scheduler first switches to this task, it restores the TrapFrame and executes `popfq` + `retfq`, which pops RFLAGS, CS, and RIP from the iretq frame, transferring control to the task's entry point with interrupts enabled.

## The Idle Task (Task 0)

Task 0 is the idle task, created during kernel initialization (`init_idle_task`). It has a special role:

1. It is registered as the current execution context
2. The BSP uses its own identity-mapped kernel stack (from page table init) rather than a separately allocated stack
3. Its entry point is the idle loop (`hlt` + periodic logging)
4. It runs whenever no other task is ready

Each AP also creates an idle task during its own `ap_entry()` init (one idle task per CPU).

The idle loop:
```rust
loop {
    hlt;                              // halt until next interrupt
    if ticks - last_log >= 1000 {     // every ~1 second
        log tick/pit/keyboard stats;
    }
}
```

Blocking task 0 is explicitly refused by `block_current` (logged as an error) â€” if no other task is ready and the running task is task 0, the scheduler leaves it in place rather than halting the system.

## Task Creation

`task::create_task(entry: u64) -> Option<usize>`

1. Check if `MAX_TASKS` (32) would be exceeded
2. Allocate 2 contiguous physical pages (8 KB) for the kernel stack
3. Map them at `HIGHER_HALF + phys_addr`
4. Zero the stack
5. Build the iretq frame (RIP, CS=0x08, RFLAGS=0x202) at the top of the stack
6. Build a synthetic TrapFrame at the bottom of the stack:
   - All GPRs = 0
   - RIP = entry point
   - CS = 0x08 (kernel code segment)
   - RFLAGS = 0x202 (IF = 1, always reserved)
   - RSP = address of iretq frame
   - SS = 0x10 (kernel data segment)
7. Compute the new task's `vruntime` as `min(current vruntime) - VRUNTIME_BIAS`, giving newly-created tasks a small startup boost.
8. Store the task in the task manager
9. Return the new task ID

## Preemptive Scheduling

### Timer Interrupt Flow

1. LAPIC timer fires, sending vector 32 to the CPU
2. Hardware pushes SS, RSP, RFLAGS, CS, RIP onto the interrupt stack
3. Stub saves all GPRs, pushes vector 32 and error code 0
4. Dispatcher calls `irq_handler(frame, 32)`
5. `irq_handler`:
   a. Sends EOI to LAPIC
   b. Increments the global tick counter via `crate::percpu::tick()`
      (which funnels into `arch::idt::tick()` â€” a single source of
      truth shared by BSP and AP idle logs and the `get_ticks`
      syscall 4)
   c. If this is the BSP and the task manager is initialised,
      calls `task::schedule(frame)`; the return path restores the
      next task's full GPR set (rdi, rsi, rbp, rbx, r12â€“r15) and
      uses `popfq + retfq` (not `iretq`) to avoid CS-descriptor
      checks rejecting the synthetic 0x08 selector. See audit S2.

### Schedule Algorithm (CFS)

```rust
pub fn schedule(frame: &mut TrapFrame) -> (bool, u64) {
    let cpu = crate::percpu::current_apic_id() as usize % MAX_CPUS;
    if crate::percpu::task_count(cpu) < 2 { return (false, 0); }

    // 1. Save current task's state
    let cur = crate::percpu::current_task();
    tasks[cur].saved_frame = *frame;
    // The original RSP is at TrapFrame offset 0xA0;
    // save it directly rather than recomputing from the frame address.
    tasks[cur].saved_frame.rsp = frame.rsp;
    let cur_vruntime = tasks[cur].vruntime;

    // 2. Find next ready task with smallest vruntime, preferring
    //    tasks from this CPU's ready queue.
    let next = find_least_loaded(cur, cpu);

    // 3. If no other ready task exists, leave current in place.
    if next == cur || tasks[next].state != TaskState::Ready {
        return (false, 0);
    }

    // 4. Advance current task's vruntime only on actual switch.
    tasks[cur].vruntime = cur_vruntime.saturating_add(VRUNTIME_TICK);

    // 5. Restore next task's state and switch. Update TSS.RSP0
    //    so ring-0 IRQs push onto the new task's stack.
    *frame = tasks[next].saved_frame;
    let next_stack_top = tasks[next].kernel_stack_top;
    if next_stack_top != 0 {
        unsafe { crate::arch::gdt::tss_set_rsp0_for_slot(cpu, next_stack_top); }
    }
    crate::percpu::set_current_task(next);
    let next_pml4 = tasks[next].pml4;
    (true, next_pml4)
}
```

### Context Switch Mechanics

When `schedule()` returns `true` (a switch was made), the timer IRQ handler does NOT use `iretq` to return. Instead it uses:

```asm
mov rsp, frame.rsp     ; switch to new task's stack
cmp next_pml4, 0       ; skip if PML4 unchanged
je 2f
mfence
mov cr3, next_pml4     ; switch page tables if needed
2:
mov r15, frame.r15     ; restore callee-saved regs
mov r14, frame.r14
mov r13, frame.r13
mov r12, frame.r12
mov rbx, frame.rbx
mov rbp, frame.rbp
mov rsi, frame.rsi
mov rdi, frame.rdi
push frame.rip          ; push new RIP
sti                     ; enable interrupts (WHPX-safe)
ret                     ; jump to the task's RIP
```

This sequence avoids `iretq`'s strict CS descriptor checks (canonicality, DPL vs CPL) and WHPX bugs with `popfq` at CPL=0 (which clears IF after emulation).

## Blocking and Wake

### Block

A task can block itself via syscall 1 (`exit`):

```rust
pub fn block_current(frame: &mut TrapFrame) {
    if current == 0 {
        log::error!("task: refused to block task 0 (idle/main)");
        return;
    }
    tasks[current].state = Blocked;
    schedule(frame);  // immediately reschedule
}
```

### Wake

Another task can wake a blocked task via syscall 3 (`wake_task(id)`):

```rust
pub fn wake(task_id: usize) {
    if tasks[task_id].state == Blocked {
        tasks[task_id].state = Ready;
    }
}
```

## Cooperative Yield

`task::yield_now()` triggers a software interrupt (`int 0x80` with `rax = 0`). The syscall handler (vector 0x80) treats this as a no-op â€” execution returns to the caller, and the task will be preempted on the next timer tick. The yield exists for future cooperative scheduling scenarios (e.g., a task that wants to hint that it's done with its current timeslice).

## Future Development

### Priority / Nice Levels

The current CFS implementation gives every task equal weight. A priority extension would:
- Add a `weight` / `nice` field to `Task`
- Scale the per-tick `vruntime` increment by `1024 / weight` (Linux-style)
- Pick the task with the minimum `vruntime` (the existing comparison is unchanged)

### SMP Scheduling

With multiple CPUs, each CPU needs its own scheduler:
- Per-CPU `TaskManager` with its own run queue
- Load balancing: steal tasks from other CPUs' run queues
- Locking: per-queue spinlocks instead of a global task manager lock

### Userspace Transition

When userspace is added:
- Tasks will run in ring 3 with separate page table mappings
- Context switches will need to switch CR3 (change page tables)
- Syscall interface will expand with proper argument passing
- `syscall`/`sysret` instructions will replace `int 0x80`

### Real-Time

For hard real-time constraints:
- Priority inheritance for mutexes
- Timer slack and deadline scheduling
- Interrupt handler top-half/bottom-half separation
- Per-task time budgets with enforcement

---

# 05 â€” ELF Loading and Boot Protocol

## Overview

The boot protocol defines how control and data pass between the three boot stages: chainloader, bootloader, and kernel. The protocol is centered on the `BootInfo` struct; only the 8-byte pointer lives at physical address `0x1000` (`BOOT_INFO_HANDOFF_ADDR`).

## BootInfo Protocol

### Location

Physical address `0x1000` (page 1) holds an **8-byte pointer** to a
dynamically-allocated `BootInfo` (chainloader `Box::new`s the struct
and writes the pointer at `0x1000`). The kernel reads the pointer,
then dereferences it. This removes the fixed-address constraint on
BootInfo itself (which is ~2 KB) â€” only the 8-byte pointer occupies
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
    pub max_cpus: u32,                      // MAX_CPUS (= 4)
    pub bsp_apic_id: u32,                   // BSP LAPIC ID
    pub ap_count: u32,                      // number of APs (0..MAX_CPUS)
    pub ap_apic_ids: [u32; MAX_CPUS],       // LAPIC ID of each AP
}
```

The SMP fields (`max_cpus`, `bsp_apic_id`, `ap_count`,
`ap_apic_ids`) are populated by the bootloader via UEFI MP
Services enumeration before `exit_boot_services`. The kernel
brings APs up via LAPIC INIT-SIPI-SIPI after ExitBootServices,
writing per-AP mailbox slots at 0x8400+ in the SIPI trampoline
page, NOT via `ApArg` (which no longer exists).

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
- `RDI` = physical address of the dynamically-allocated `BootInfo` struct
  (the pointer stored at `BOOT_INFO_HANDOFF_ADDR` (`0x1000`), NOT the fixed address itself)
- RSP must be mod 16 = 8 at entry (simulating the state after a `call` instruction)

The bootloader jumps with:
```asm
sub rsp, 8          ; align stack for SysV ABI (simulate missing call)
mov rdi, boot_info  ; pass BootInfo physical address (dynamically allocated)
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

**SectorReader** wraps UEFI's BlockIO protocol and handles arbitrary block sizes (512â€“4096 bytes) via a sector cache.

**ext4 structures parsed**:
- Superblock (at byte offset 1024) â€” block size, inode count, block count
- Block group descriptor table â€” bitmap locations, inode table locations
- Inodes (256 bytes each) â€” file metadata, data block pointers
- Directory entries â€” file names, inode numbers
- Extent tree â€” logical-to-physical block mapping

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
   - Direct blocks (indices 0â€“11)
   - Singly indirect block (index 12)
   - (Doubly/triply indirect are not implemented â€” ext4 rarely uses them for small files)
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

### Multi-Processor Boot (Implemented)

SMP is already implemented:
- `BootInfo.ap_apic_ids` carries the list of AP LAPIC IDs (populated by UEFI MP Services)
- Per-CPU kernel stacks are allocated by the kernel from the buddy allocator (not in BootInfo)
- SIPI trampoline is at fixed address `0x8000` (SIPI vector 0x08), loaded by `arch::smp::init()`
- AP mailbox slots at `0x8400+` carry per-AP GDT/IDT pointers, stack top, entry point, and PML4
- BSP broadcasts INIT to all APs â†’ 10ms â†’ broadcasts SIPI (vector 0x08) â†’ 1ms â†’ broadcasts second SIPI â†’ polls per-AP status bytes

### Device Tree Blob

On non-ACPI systems (e.g., RISC-V, ARM), the boot protocol should support passing a flattened device tree (FDT) instead of ACPI tables. The BootInfo could gain a `dtb_addr` field alongside `rsdp_addr`.

### Executive Runtime (Removed)

The Executive Runtime (`exrun`) crate has been removed from the workspace. It was a
`loop { hlt }` stub with its own forked PML4 and shared mailbox. A future Secure Runtime
may replace it with a full policy engine and capability broker.

---

# 06 â€” ACPI and Platform Discovery

## Overview

LodaxOS discovers hardware topology through ACPI (Advanced Configuration and Power Interface) tables. The kernel parses the RSDP, XSDT, and MADT to find CPUs, I/O APICs, and interrupt routing information.

## Discovery Order

```
Bootloader captures RSDP from UEFI config table
  â†’ stores physical address in BootInfo.rsdp_addr
Kernel reads RSDP address from BootInfo
  â†’ parses RSDP to find XSDT
    â†’ parses XSDT to find MADT ("APIC" signature)
      â†’ parses MADT to enumerate CPUs, IOAPICs, ISOs
```

## RSDP (Root System Description Pointer)

The RSDP is a 36-byte (v2.0+) or 20-byte (v1.0) structure. The bootloader captures the UEFI configuration table pointer into `BootInfo.rsdp_addr` before `ExitBootServices`; the kernel prefers that hint and only falls back to scanning firmware regions if the hint is missing or invalid.

Scanned regions, in order:
1. The hint from `BootInfo.rsdp_addr` (validated by signature and checksum)
2. EBDA (Extended BIOS Data Area) â€” word at `0x40E` points to segment
3. Standard BIOS ROM area (`0xE0000â€“0xFFFFF`)
4. OVMF/UEFI firmware area (`0xFEFF_0000â€“0xFF00_0000`)

The RSDP signature is `"RSD PTR "` (8 bytes with trailing space). Validation:
- Checksum over all bytes of the RSDP must sum to 0 (mod 256)

### RSDP Fields

| Offset | Size | Field | Description |
|---|---|---|---|
| 0 | 8 | signature | "RSD PTR " |
| 8 | 1 | checksum | Sum of bytes 0â€“19 = 0 |
| 9 | 6 | oem_id | OEM identifier |
| 15 | 1 | revision | 0 = v1.0, 2 = v2.0+ |
| 16 | 4 | rsdt_addr | RSDT physical address (v1.0) |
| 20 | 4 | length | Total RSDP length (v2.0+) |
| 24 | 8 | xsdt_addr | XSDT physical address (v2.0+) |
| 32 | 1 | ext_checksum | Sum of all bytes = 0 (v2.0+) |

## XSDT (Extended System Description Table)

The XSDT is an array of 64-bit physical addresses pointing to other ACPI tables. It is preceded by a standard SDT (System Description Table) header.

```
XSDT Header (36 bytes):
  signature[4] = "XSDT"
  length: u32
  revision: u8
  checksum: u8
  oem_id[6]
  oem_table_id[8]
  oem_revision: u32
  creator_id: u32
  creator_revision: u32

Entry array (8 bytes each):
  entry[0]: u64 (physical address of first table)
  entry[1]: u64 (physical address of second table)
  ...
```

The kernel scans XSDT entries looking for the `"APIC"` signature (MADT). Each table is validated by checksum (sum of all bytes in the table must equal 0 mod 256).

### RSDT Fallback

If the RSDP revision is 0 (v1.0), the RSDT is used instead. The RSDT entry array has 4-byte entries (32-bit physical addresses) instead of the XSDT's 8-byte entries.

## MADT (Multiple APIC Description Table)

The MADT describes the APIC topology of the system.

### Fixed Header

```
MADT:
  SDT Header (36 bytes, signature = "APIC")
  local_apic_addr: u32    â€” physical address of LAPIC (typically 0xFEE00000)
  flags: u32              â€” bit 0 = PC-AT compatibility (dual 8259s)
  entries...              â€” variable-length entry list
```

### Entry Types

| Type | Name | Length | Description |
|---|---|---|---|
| 0 | Local APIC | 8 | CPU core with LAPIC |
| 1 | I/O APIC | 12 | I/O APIC controller |
| 2 | ISO | 10 | Interrupt Source Override |
| 4 | NMI | 6 | NMI source |
| 5 | Local APIC Override | 12 | 64-bit LAPIC address |
| 6 | I/O APIC NMI | 10 | I/O APIC NMI routing |

### Entry 0: Local APIC

```
type: u8 = 0
length: u8 = 8
acpi_processor_id: u8
apic_id: u8
flags: u32 (bit 0 = enabled)
```

The kernel parses MADT to find the LAPIC and I/O APIC base addresses. The APIC IDs used for SMP boot come from `BootInfo.ap_apic_ids`, populated by the bootloader via UEFI MP Services enumeration (not from the MADT). The BSP kernel later starts APs via LAPIC INIT-SIPI-SIPI.

### Entry 1: I/O APIC

```
type: u8 = 1
length: u8 = 12
ioapic_id: u8
reserved: u8
ioapic_addr: u32   â€” MMIO base (typically 0xFEC00000)
gsi_base: u32      â€” starting GSI number
```

The IOAPIC driver maps each discovered IOAPIC's MMIO region, reads its version and max redirection entry count, then initializes all redirection entries to a masked state.

### Entry 2: Interrupt Source Override (ISO)

```
type: u8 = 2
length: u8 = 10
bus: u8              â€” bus source (0 = ISA)
source: u8           â€” ISA IRQ number
gsi: u32             â€” Global System Interrupt
flags: u16           â€” bit 1 = polarity, bit 3 = trigger mode
```

ISOs are how ACPI tells the OS about deviations from the standard ISA IRQâ†’GSI mapping. On modern hardware:
- ISA IRQ 0 usually overrides to GSI 2 (PIT)
- ISA IRQ 2 often cascades differently

The interrupt routing table (`kernel/src/intr/mod.rs`) is built from ISOs plus identity mappings for any ISA IRQ without an ISO.

## GSI (Global System Interrupt) Routing

```
ISA IRQ â†’ ISO lookup â†’ GSI â†’ IOAPIC lookup â†’ IOAPIC[index] + pin â†’ vector
```

Each step maps through a table:

1. **ISA â†’ GSI**: Look up ISO entries by `bus==0` and `source==IRQ`. If found, use the ISO's GSI. Otherwise, identity-map (GSI = IRQ).
2. **GSI â†’ IOAPIC**: Scan IOAPIC entries for `gsi_base â‰¤ GSI < gsi_base + max_redir`. The pin is `GSI - gsi_base`.
3. **GSI â†’ Vector**: Allocate a unique vector from the device range (33â€“63).

## Non-MADT Tables (Future)

The ACPI subsystem can be extended to parse additional tables. The codebase currently keeps the `XSDT_SIG` and `MADT_SIG` signature constants; other signatures (`"FACP"`, `"MCFG"`, `"HPET"`, `"DSDT"`, `"SSDT"`) are not declared and must be added when the corresponding parsers are written.

### FADT (Fixed ACPI Description Table)

| Signature | "FACP" |
|---|---|
| Purpose | Power management, reset register, sleep states |
| Use | System shutdown, reboot, S3/S4 sleep |

### DSDT/SSDT (Differentiated System Description Table)

| Signature | "DSDT", "SSDT" |
|---|---|
| Purpose | AML bytecode for device enumeration |
| Use | Device discovery, power management, battery/ACPI EC |

### MCFG (PCI Express Memory-Mapped Configuration)

| Signature | "MCFG" |
|---|---|
| Purpose | PCIe ECAM base address |
| Use | PCI enumeration via memory-mapped config space |

### HPET (High Precision Event Timer)

| Signature | "HPET" |
|---|---|
| Purpose | HPET base address and capabilities |
| Use | Alternative to PIT for timekeeping and event scheduling |

## Future Platform Support

### PCI Enumeration

PCI bus enumeration will use:
1. MCFG table for memory-mapped config access (ECAM)
2. For legacy PCI, I/O port config mechanism at `0xCF8/0xCFC`
3. Each discovered device gets a bus:device:function identifier
4. Device BARs (Base Address Registers) determine MMIO/I/O ranges
5. MSI/MSI-X capabilities are detected and configured

### Multi-Processor Startup â€” Implemented

Per the Intel Multiprocessor Specification, APs (Application Processors) are started via `arch::smp::smp_boot_aps()`:
1. Pre-load SIPI trampoline (compiled machine code) at `0x8000` (SIPI vector 0x08)
2. Prepare per-CPU mailbox slots at `0x8400+` (GDT/IDT ptrs, PML4, stack top, entry, status bytes)
3. Send INIT IPI to the AP via LAPIC ICR
4. Wait ~10 ms (PIT-based busy-wait)
5. Send STARTUP IPI with vector 0x08 (startup at `0x8000`)
6. Wait ~1 ms (pause-based busy-wait loop)
7. Send second STARTUP IPI
8. AP executes trampoline code that:
   a. Real-mode entry â†’ A20 gate â†’ protected mode â†’ PAE â†’ long mode
   b. Reads mailbox at `0x8400+` for PML4, GDT pointer, IDT pointer, stack top, entry point
   c. Loads kernel GDT/IDT, switches RSP to per-CPU kernel stack
   d. Reads APIC ID from LAPIC MMIO, then jumps to `ap_entry()`
   e. Per-CPU init: FPU/SSE, mark online, install GS base, LAPIC timer, idle task, `ltr`, `sti`
   f. Enters per-CPU idle/scheduling loop

### ACPI Namespace (AML)

The ACPI namespace and AML interpreter are a significant addition. AML is executed to:
- Discover devices not in the MADT/XSDT
- Evaluate _PRS (possible resources), _CRS (current resources), _SRS (set resources)
- Handle power management events
- Evaluate _OSC (OS capabilities)

AML requires a bytecode interpreter with memory management â€” a significant undertaking in `no_std`.

---

# 07 â€” Build System and Disk Image

## Overview

The build system produces three UEFI binaries (chainloader, bootloader, kernel) and assembles them into a GPT-partitioned disk image suitable for QEMU or physical hardware.

## Build Pipeline

```
Source code (Rust nightly)
  â”‚
  â”œâ”€ cargo build -p lodaxos-system       â†’ target/debug/ (library)
  â”œâ”€ cargo build -p lodaxos-kernel       â†’ kernel.elf (custom x86_64-unknown-none)
  â”œâ”€ cargo build -p lodaxos-boot         â†’ lodaxos-boot.efi (x86_64-unknown-uefi)
  â””â”€ cargo build -p lodaxos-chain        â†’ lodaxos-chain.efi (x86_64-unknown-uefi)
                                              â”‚
                                              â–¼
                                    create_disk_image.py
                                              â”‚
                                              â–¼
                                          disk.img
```

### Build Script (`build.bat`)

```bat
cargo +nightly build -p lodaxos-system
cargo +nightly build -p lodaxos-boot --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-chain --target x86_64-unknown-uefi
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zbuild-std=core,alloc
copy target\x86_64-unknown-uefi\debug\lodaxos-boot.efi staging\Bootloader.efi
copy target\x86_64-unknown-uefi\debug\lodaxos-chain.efi staging\lodaxos-chain.efi
copy target\kernel\debug\lodaxos-kernel staging\kernel.elf
```

Key points:
- Kernel uses `-Zbuild-std=core,alloc` to build Rust's core and alloc libraries from source for the custom target
- The kernel target outputs to `target/kernel/debug/`
- Each crate's target directory is configured via `build-std` and the JSON target spec in its subdirectory
- The 4-crate workspace is: system, chain, boot, kernel

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
LBA 2â€“33:        Partition Entry Array (128 entries Ã— 128 bytes)
LBA 34â€“2047:     Unused (GPT alignment)
LBA 2048â€“1050623: Partition 0 â€” ext4 (512 MB)
LBA 1050624â€“1181695: Partition 1 â€” ESP FAT32 (64 MB)
LBA 1181696â€“1228799: Backup GPT
```

### Partition 0 â€” ext4 (Partition Zero)

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

### Partition 1 â€” ESP (FAT32)

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

The ESP root also contains legacy copies of `Bootloader.efi` and `kernel.elf` for the temporary boot test where the chainloader reads them directly from the ESP.

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

A convenience script that runs build â†’ image creation â†’ QEMU in sequence.

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

---

# 08 â€” Fault Model and Recovery

## Philosophy

LodaxOS treats failures as normal operational events, not exceptional catastrophes. The system is designed so that every component except the kernel can fail without bringing down the entire system. Recovery is layered, with each layer responsible for handling failures at that level.

## Fault Classification

### Hard Faults (Kernel-Level)

A hard fault requires kernel intervention. The kernel halts or resets the CPU. These are always logged to serial before halting.

| Code | Name | Cause |
|---|---|---|
| `HF_KERNEL_PANIC` | 0x01 | Generic kernel panic |
| `HF_DOUBLE_FAULT` | 0x02 | CPU double fault (#DF) |
| `HF_TRIPLE_FAULT` | 0x03 | CPU reset (QEMU `-d cpu_reset`) |
| `HF_SR_UNRECOVERABLE` | 0x04 | SR failed N respawn attempts |
| `HF_SR_SPAWN_FAIL` | 0x05 | Kernel couldn't spawn SR at boot |
| `HF_MEMORY_EXHAUSTED` | 0x06 | PMM completely empty |

### Soft Faults (Service-Level)

A soft fault is handled by Secure Runtime (SR). The kernel is not involved.

| Code | Name | Cause |
|---|---|---|
| `SF_PYI_CRASH` | 0x10 | PyI process death |
| `SF_PYI_TIMEOUT` | 0x11 | PyI heartbeat missed |
| `SF_PYI_OOM` | 0x12 | PyI's memory cap exhausted |

## Current Fault Handling

### Panic Handler

Each boot stage has its own panic handler:

**Chainloader** (`chain/src/main.rs`):
- Writes `"PANIC"` to serial with polling timeout (100K retries per byte)
- Formats and writes the location (file name + line number) when `info.location()` is available
- Writes the panic message body
- Halts: `cli; hlt` loop

**Bootloader** (`boot/src/main.rs`):
- Writes `"PANIC"` to serial
- Formats and writes location (file name, line number) via manual decimal conversion
- Writes panic message via `core::fmt::Write`
- Halts: `cli; hlt` loop

**Kernel** (`kernel/src/main.rs`):
- Writes `"PANIC at "` + file name + line number to serial
- Writes panic message via `core::fmt::Write`
- Halts: `cli; hlt` loop

### Exception Handling

The kernel's exception handler (vector 0â€“31) logs detailed register state and halts for all exceptions except breakpoints (#BP, vector 3) and page faults (#PF, vector 14 â€” which the kernel resolves via `mm::vma::handle_page_fault` for kernel VMA regions).

Double Faults (#DF, vector 8) use IST1 (Interrupt Stack Table 1) â€” a dedicated 16 KB stack. This ensures that if the kernel's stack is corrupted, the double fault handler still has a valid stack. The handler logs and halts.

The spurious interrupt vector (0xFF) is a bare `iretq` with no EOI and no logging. The LAPIC may generate spurious interrupts due to bus noise or race conditions; the simplest correct response is to ignore them.

## Planned Recovery Architecture

### Recovery Layers

```
Application Failure
  â†“ (restart application)
PyI Failure
  â†“ (restart PyI runtime)
Agent Safe Mode
  â†“ (recover agent state)
Agent State Restoration
  â†“ (restore from snapshot)
Secure Runtime Recovery
  â†“ (re-spawn SR)
Kernel Recovery (future)
  â†“ (boot backup kernel)
```

### Application-Level Recovery

When an application crashes within PyI:
1. PyI detects the crash (signal handler, exception boundary)
2. PyI logs the failure
3. PyI restarts the application with its defined restart policy:
   - `"on-failure"`: restart automatically
   - `"always"`: restart regardless of exit code
   - `"never"`: don't restart
   - `"backoff"`: restart with exponential backoff
4. If restart fails N times, PyI reports to SR

### PyI-Level Recovery

When PyI itself crashes:
1. SR detects PyI crash via heartbeat miss or signal notification
2. SR checks PyI's defined memory cap (`memory="128mb"`)
3. If PyI exceeded memory: `SF_PYI_OOM` â€” re-spawn with larger cap or kill processes
4. If PyI crashed: `SF_PYI_CRASH` â€” re-spawn from known-good binary image
5. If PyI heartbeat is missing: `SF_PYI_TIMEOUT` â€” wait one interval, then re-spawn

### Agent Safe Mode

Safe Mode is a minimal runtime state that provides just enough functionality to debug and repair a corrupted agent:
- Minimal process management (ls, cd, read, write)
- No PyI, no UI, no device access
- Access to agent-local storage for diagnostics
- REPL access to the agent's state

Agent Safe Mode is entered when:
1. PyI crashes during recovery boot
2. Agent configuration is corrupted
3. User explicitly triggers it

### Secure Runtime Recovery

If SR itself fails:
1. The kernel detects SR failure (signal or heartbeat)
2. Kernel logs `HF_SR_UNRECOVERABLE`
3. Kernel attempts to re-spawn SR from the backup binary on Partition Zero
4. If re-spawn succeeds, SR enters Emergency Mode
5. If re-spawn fails N times, kernel halts with `HF_SR_UNRECOVERABLE`

### Emergency Mode

Emergency Mode is a minimal system state that runs directly on the kernel, bypassing SR and PyI entirely:

| Command | Description |
|---|---|
| `ls [path]` | List directory contents |
| `cd [path]` | Change directory |
| `read [file]` | Display file contents |
| `write [file] [content]` | Write content to file |
| `start-userspace` | Attempt to start SR â†’ PyI â†’ normal mode |
| `restart` | Warm reboot |
| `shutdown` | Power off |

Emergency Mode does not depend on SR, PyI, or any service. It is compiled into the kernel or loaded as a minimal initramfs-style binary.

### Kernel Recovery (Future)

A future kernel recovery mode may:
1. Validate the current kernel's integrity
2. Load a backup kernel from Partition Zero
3. Validate and restore Secure Runtime state
4. Reboot into the backup kernel

This is the deepest recovery layer and requires:
- A reserved Partition Zero region for backup kernel images
- Integrity checking (hash verification) of kernel and SR binaries
- State serialization and restoration protocol

## Fault Propagation Rules

1. **A failure in a lower layer always causes the upper layers to fail.** If the kernel panics, everything stops. If SR crashes, PyI and all agents lose access to services.

2. **Recovery starts at the layer of failure and rebuilds upward.** An SR crash triggers SR recovery, which then restarts PyI, which then restarts agents and applications. Layers below the failure point are unaffected.

3. **Hard faults are fatal.** A kernel panic, double fault, or triple fault is not recoverable below the kernel layer. The system must be rebooted (or, in the future, kernel recovery mode must be triggered).

4. **Soft faults are recoverable.** All soft faults (SF codes) are handled by SR. The kernel is never involved in soft fault recovery.

## Future Data Structures

### Fault Log

```
struct FaultRecord {
    timestamp: u64,          // ticks since boot
    code: u32,               // HF_ or SF_ code
    source_id: u16,          // task/process/agent ID
    details: [u8; 64],       // fault-specific data
    crc: u32,                // integrity check
}
```

Fault records would be stored in a ring buffer accessible from Emergency Mode for post-mortem analysis.

### Service Restart Policy

```
struct RestartPolicy {
    max_retries: u32,        // max consecutive restart attempts
    backoff_ms: u32,         // initial backoff in ms
    backoff_multiplier: f32, // exponential backoff multiplier
    action: enum {
        Restart,
        SafeMode,
        Emergency,
        Halt,
    },
}
```

This would be embedded in the SR's service definition metadata stored on Partition Zero.

---

# 09 â€” Subsystem Interfaces

## Overview

This document defines the API surfaces between kernel subsystems. The interfaces are designed to be minimal and flat â€” each subsystem exposes a small number of public functions, and subsystems interact through these narrow interfaces rather than through shared global state.

## Serial Subsystem (`kernel/src/serial.rs`)

### Public API

```rust
pub fn init();                           // Initialize COM1 at 115200 8N1
pub fn write_byte(byte: u8);             // Write single byte (poll THR)
pub fn write_str(s: &str);               // Write string (\n â†’ \r\n)
```

### Internals

- I/O port `0x3F8` (COM1) with divisor `0x01` (115200 baud from 1.8432 MHz clock)
- Line control register (LCR): 8N1 = `0x03`
- FIFO control register (FCR): enable, clear, 14-byte threshold = `0xC7`
- Modem control register (MCR): DTR + RTS + IRQ enable = `0x0B`
- Write polling: check LSR bit 5 (THR empty) before each byte
- No buffering, no interrupts â€” synchronous writes only

### Dependents

- Logger: calls `write_str` for log output
- Panic handler: calls `write_str` for error messages
- GDT loader: uses `com1_trace` for early debug output (single-byte writes with 100K retry timeout)

## Logger Subsystem (`kernel/src/logger.rs`)

### Public API

```rust
pub fn init() -> Result<(), SetLoggerError>;
```

### Registration

Implements `log::Log` trait:
```rust
fn enabled(&self, metadata: &LogMetadata) -> bool { true }
fn log(&self, record: &LogRecord);
fn flush(&self);
```

Log format: `[LEVEL] target: message\n`

Max log level: `LevelFilter::Trace` (all levels enabled)

Uses `core::fmt::write` to render arguments without heap allocation.

### Dependents

- All kernel code via `log::info!()`, `log::warn!()`, `log::error!()`, `log::debug!()`, `log::trace!()`
- Panic handler uses `write` directly for panic message formatting

## Font Subsystem (`kernel/src/font.rs`)

### Public API

```rust
pub const GLYPH_WIDTH: usize = 8;
pub const GLYPH_HEIGHT: usize = 16;
pub fn get_glyph(ch: char) -> &'static [u8; 16];
```

### Data

Bitmap font for ASCII 32â€“126 (95 glyphs). Each glyph is 16 bytes (16 rows Ã— 8 columns). MSB = leftmost pixel.

### Dependents

- Framebuffer (`kernel::Framebuffer`): calls `get_glyph` for text rendering in `put_char`, `write_str`, `write_str_centered`

## Physical Memory Allocator (`kernel/src/mm/phys.rs`)

### Public API

```rust
pub unsafe fn init_from_regions(regions: &[(u64, u64)], boot_info_phys: u64);
pub fn alloc_page() -> Option<u64>;        // returns physical address
pub fn alloc_pages(count: u64) -> Option<u64>;
pub fn free_page(addr: u64);
pub fn free_pages(addr: u64, count: u64);
```

### Interface Contract

- `init_from_regions` must be called once before any alloc/free
- Regions must be the free memory descriptors from BootInfo
- Allocators may be called from any kernel context (interrupts must be enabled or the caller must hold no locks that the dispatcher would contend on)
- Returns physical addresses (4 KB aligned)
- `alloc_pages(0)` = `None`
- Double-free detection: `free_page` on an already-free page logs a warning

### Dependents

- Page table builder (`virt.rs`): allocates pages for PML4, PDP, PD, PT tables
- Heap allocator (`heap.rs`): allocates pages for heap arena
- Task manager (`task.rs`): allocates pages for task kernel stacks
- IOAPIC/LAPIC MMIO mapping utilities

## Virtual Memory Manager (`kernel/src/mm/virt.rs`)

### Public API

```rust
pub const PRESENT: u64;
pub const WRITABLE: u64;
pub const USER: u64;
pub const CACHE_DISABLE: u64;
pub const NO_EXECUTE: u64;
pub const DATA: u64;                // PRESENT | WRITABLE | NO_EXECUTE
pub const HIGHER_HALF: u64;         // 0xFFFF_8000_0000_0000

pub unsafe fn init(regions: &[(u64, u64)], fb_phys: Option<(u64, u64)>);
pub fn translate(virt: u64) -> Option<u64>;
pub fn unmap(virt: u64);
pub fn pml4_address() -> u64;       // current PML4 physical address
pub unsafe fn map_contiguous(pml4, virt_start, phys_start, num_pages, flags);
pub fn map_region_higher_half(pml4, phys, size, flags);
```

### Interface Contract

- `init` must be called once after physical allocator init
- After `init`, CR3 points to kernel's own PML4
- All memory operations after init use higher-half virtual addresses
- MMIO regions must use `map_region_higher_half` to avoid 2 MB identity page conflict
- `pml4_address()` reads CR3 â€” valid only after `init`

### Dependents

- Heap: allocates + maps pages at heap virtual base
- IOAPIC init: maps MMIO regions
- LAPIC init: maps LAPIC MMIO region
- Task init: maps kernel stack pages
- (Future) Userspace: manages per-process page tables

## Heap Allocator (`kernel/src/mm/heap.rs`)

### Public API

```rust
pub fn init();

// Via GlobalAlloc impl:
#[global_allocator]
static ALLOCATOR: GlobalAllocator;
```

### Interface Contract

- `init` must be called after page table init
- SLUB-style: 9 caches, object sizes 32 B, 64 B, 128 B, 256 B, 512 B, 1 KB, 2 KB, 4 KB, 8 KB
- Each cache has its own spinlock; large allocations (> 8 KB) fall back to `phys::alloc_order`
- Caches derive their per-slab `order` and `objs_per_slab` from the actual `obj_size` at init time, so each cache uses the smallest buddy order that can hold at least one object
- Thread-safe via per-cache spinlock
- The virtual arena is a contiguous 64 MB region starting at `0xFFFF_8080_0000_0000`; each new slab maps the freshly-allocated buddy pages into that arena

### Dependents

- All code that uses `alloc::vec::Vec`, `alloc::boxed::Box`, `alloc::string::String`, `alloc::format!`, etc.
- Bootloader uses UEFI allocator (`uefi::allocator::Allocator`), not this kernel heap

## ACPI Subsystem (`kernel/src/acpi/mod.rs`)

### Public API

```rust
pub fn init(hint: Option<u64>) -> AcpiContext;
pub fn find_rsdp(hint: Option<u64>) -> Option<u64>;
pub fn find_sdt(xsdt_addr: u64, signature: &[u8; 4]) -> Option<u64>;
pub fn validate_table(addr: u64) -> bool;

pub const XSDT_SIG: [u8; 4];
pub const MADT_SIG: [u8; 4];

pub struct AcpiContext {
    pub revision: u8,
    pub rsdp_addr: u64,
    pub xsdt_addr: u64,
    pub madt_addr: Option<u64>,
}
```

### Interface Contract

- Kernel ACPI init must happen before page table switch (identity map needed) OR after with physical addresses
- `init(hint)` first validates the optional `hint` (typically `BootInfo.rsdp_addr`); if the hint is missing or invalid, it falls back to scanning EBDA, BIOS ROM area, and the OVMF region
- Currently only MADT is parsed; FADT, MCFG, HPET, DSDT, SSDT signatures are not declared in the kernel and must be added when the corresponding parsers are written

### Dependents

- Kernel main: calls `acpi::init(info.rsdp_addr)` â†’ parses MADT â†’ configures IOAPICs and interrupt routing
- MADT parser: called by ACPI subsystem with physical address

## Interrupt Routing (`kernel/src/intr/mod.rs`)

### Public API

```rust
pub fn init(madt: &MadtInfo);
pub fn alloc_vector() -> Option<u8>;
pub fn lookup_isa(isa_irq: u8) -> Option<&'static IrqRoute>;
pub fn lookup_gsi(gsi: u32) -> Option<&'static IrqRoute>;
pub fn lookup_vector_isa(vector: u8) -> Option<u8>;
pub fn install_route(route: &IrqRoute);
pub fn enable_route(route: &IrqRoute);
pub fn install_all_routes();         // install all routes, leave masked
pub fn install_all_masked() -> usize;  // returns count of programmed pins
```

### Data Flows

```
Input: MADT info (from acpi::madt::parse)
  â†’ walks ISO entries
  â†’ for each: ISA IRQ â†’ GSI â†’ IOAPIC lookup â†’ vector allocation â†’ IrqRoute
  â†’ identity maps remaining ISA IRQs
  â†’ stores in routing table

Output: IrqRoute instances used by:
  - IOAPIC driver for redirection entry programming
  - IDT handler for device IRQ dispatch
  - Kernel main for enabling device routes (PIT, keyboard)
```

### Dependents

- Kernel main: routes IOAPIC entries, enables PIT/keyboard
- IDT irq_handler: maps vector back to ISA source for PIT/keyboard handling

## IOAPIC Driver (`kernel/src/arch/ioapic.rs`)

### Public API

```rust
pub fn init(ioapic_infos: &[IoApicInfo]);
pub fn is_initialized() -> bool;
pub fn get(index: usize) -> Option<&'static IoApic>;
pub fn count() -> usize;
pub fn lookup_gsi(gsi: u32) -> Option<(usize, u8)>;

// IoApic methods:
pub fn set_entry(&self, pin: u8, low: u32, high: u32);
pub fn get_entry(&self, pin: u8) -> (u32, u32);
pub fn mask_entry(&self, pin: u8);
pub fn unmask_entry(&self, pin: u8);
pub fn make_redir_low(vector: u8, flags: u16, masked: bool) -> u32;
pub fn make_redir_high(apic_id: u8) -> u32;
```

### Dependents

- Interrupt routing: calls `set_entry`, `mask_entry`, `unmask_entry`
- Kernel main: calls `init` with IOAPIC info from MADT

## LAPIC Driver (`kernel/src/arch/apic.rs`)

### Public API

```rust
pub fn init_mmio();
pub fn enable();
pub fn is_initialized() -> bool;
pub fn configure_timer(divisor: u32, vector: u8, periodic: bool);
pub fn calibrate_pit();
pub fn set_timer_count(ms: u32);
pub fn pit_enable_periodic(freq_hz: u32);
pub fn send_eoi();
pub fn send_init_ipi(apic_id: u32);
pub fn send_sipi(apic_id: u32, vector: u8);
pub fn ap_enable_timer(apic_id: u32);
```

### Dependents

- Kernel main: calls `init_mmio` â†’ `enable` â†’ `calibrate_pit` â†’ `configure_timer` â†’ `set_timer_count`
- SMP AP boot: calls `send_init_ipi` / `send_sipi` for INIT-SIPI-SIPI sequence
- AP entry: calls `ap_enable_timer` for per-CPU LAPIC timer enable
- IDT irq_handler: calls `send_eoi` if LAPIC is initialized
- Kernel idle loop: relies on timer for scheduling

## GDT Subsystem (`kernel/src/arch/gdt.rs`)

### Public API

```rust
pub fn load();
pub fn set_ist1(addr: u64);
pub fn tss_set_rsp0(rsp0: u64);
pub fn tss_set_rsp0_for_slot(slot: usize, rsp0: u64);
pub fn init_tss_descriptor_for_slot(slot: usize);

// Exported selectors:
pub const KERNEL_CODE_SEL: u16 = 0x08;
```

### Dependents

- Kernel main: calls `load` (which also calls `set_ist1` from the IDT's perspective; `gdt::load` does not call it)
- IDT init: calls `set_ist1` to wire the double-fault IST1 stack into the TSS
- Task creation: uses `KERNEL_CODE_SEL` (0x08) for task CS
- AP boot: calls `init_tss_descriptor_for_slot` for each AP before release
- Scheduler: calls `tss_set_rsp0` on context switch to update per-CPU RSP0

## IDT Subsystem (`kernel/src/arch/idt.rs`)

### Public API

```rust
pub fn init();
pub fn mask_pic();
pub fn enable_interrupts();
pub fn disable_interrupts();
pub fn ticks() -> u64;
pub fn pit_ticks() -> u64;
pub fn key_count() -> u64;
pub fn key_scancode() -> u16;
```

### Internals

- `TrapFrame` struct shared with task manager (defines register save layout)
- `interrupt_dispatcher` called by all stubs, dispatches by vector
- `irq_handler` sends EOI, handles timer/PIT/keyboard, calls scheduler
- `exception_handler` logs details, halts on unrecoverable, attempts to resolve #PF via `mm::vma::handle_page_fault`
- `syscall_handler` dispatches syscalls by number
- `mask_pic` is the single point that disables the legacy 8259 PIC. `arch::apic::enable` does not call it; the kernel main loop invokes it once before the LAPIC is enabled.

### Dependents

- Kernel main: calls `init`, `mask_pic`
- Task scheduler: modifies TrapFrame in timer handler for context switch
- Syscall handlers: process syscall requests
- Idle loop: reads `ticks()`, `pit_ticks()`, `key_count()`

## Task Manager (`kernel/src/task.rs`)

### Public API

```rust
pub fn init();
pub fn init_idle_task();
pub fn is_initialized() -> bool;
pub fn current_task_id() -> usize;
pub fn task_count() -> usize;
pub fn create_task(entry: u64) -> Option<usize>;
pub fn schedule(frame: &mut TrapFrame) -> (bool, u64);
pub fn block_current(frame: &mut TrapFrame);
pub fn wake(task_id: usize);
pub fn yield_now();
pub fn steal_task(hungry_cpu: usize);
```

### Data Flow

```
Timer IRQ (vector 32)
  â†’ irq_handler()
    â†’ task::schedule(&mut TrapFrame)
      â†’ saves current task state in Task.saved_frame
      â†’ finds next ready task (CFS: minimum `vruntime`)
      â†’ overwrites TrapFrame with next task's state
      â†’ returns true
    â†’ context switch via mov rsp + sti + push rip + ret (WHPX-safe)

Syscall (int 0x80)
  â†’ syscall_handler()
    â†’ task::block_current(frame)  // syscall 1
    â†’ task::wake(id)             // syscall 3
    â†’ task::current_task_id()    // syscall 2
    â†’ task::yield_now()          // via int 0x80 nr=0
```

### Dependents

- IDT: calls `schedule` from timer IRQ handler
- Kernel main: calls `init`, `init_idle_task`, `create_task` for test tasks
- AP entry: calls `init_idle_task` for per-CPU idle task
- AP scheduling loop: calls `steal_task` for work stealing
- Syscall handlers: call block_current, wake, current_task_id

## Framebuffer (`kernel/src/main.rs`)

### Public API (per-crate implementation in kernel)

```rust
pub struct Framebuffer { ptr, width, height, stride, bytes_per_pixel, is_bgr }

impl Framebuffer {
    pub fn from_info(info: &FramebufferInfo) -> Self;
    pub fn set_pixel(&self, x, y, r, g, b);
    pub fn clear(&mut self, r, g, b);
    pub fn put_char(&mut self, ch, x, y, r, g, b);
    pub fn write_str(&mut self, s, x, y, r, g, b);
    pub fn write_str_centered(&mut self, s, y, r, g, b);
}
```

(Framebuffer is not a separate module â€” it is defined inline in `kernel/src/main.rs`. The earlier `src/main.rs` UEFI-app stub that mirrored this API was a dead artifact and has been removed.)

### Interface Contract

- Pixel writes are volatile (prevent compiler optimization of redundant writes)
- BGR/RGB handling: `is_bgr` flag read from GOP pixel format
- Bounds-checked: writes outside the visible area are silently dropped
- After CR3 switch: `ptr` must be updated to higher-half virtual address
- No double-buffering, no vsync, no compositing

### Dependents

- Kernel main: renders splash screen and status text
- (Future) GUI system: will render windows, widgets, and composited output

## Subsystem Initialization Order

The kernel initialization sequence in `_start` has strict ordering constraints:

```
Phase 0: Serial â†’ Logger                   (no dependencies)
Phase 1A: Memory regions from BootInfo      (no dependencies)
Phase 1B: Framebuffer init                  (no dependencies)
Phase 1C: Physical allocator init           (regions from 1A)
Phase 1D: ACPI init                        (regions from 1A, phys alloc)
Phase 1E: Page tables init                 (regions, phys alloc, optionally fb)
Phase 1F: Heap init                        (page tables, phys alloc)
Phase 1G: VMA tree init                    (heap)
Phase 2A: cli + mask PIC                    (no dependencies)
Phase 2B: LAPIC MMIO init                  (page tables)
Phase 2C: IOAPIC init + INTR routing       (page tables, ACPI/MADT)
Phase 2D: Reserve AP pages                 (phys alloc)
Phase 2E: SIPI trampoline init             (phys alloc â€” loads trampoline to 0x8000)
Phase 2F: Framebuffer re-map in higher-half (page tables)
Phase 3A: GDT load                         (page tables)
Phase 3B: IDT init                         (GDT)
Phase 3C: Percpu BSP init                  (mark online, install gs_base)
Phase 3D: Task init + init_idle_task       (IDT, page tables, phys alloc)
Phase 3E: Create test tasks                (task init)
Phase 3F: Install IOAPIC routes            (IOAPIC + INTR init)
Phase 3H: Enable LAPIC                     (LAPIC MMIO, IOAPIC)
Phase 3I: Calibrate LAPIC timer            (LAPIC)
Phase 3J: Configure LAPIC timer            (LAPIC calibrated)
Phase 3K: Enable PIT periodic              (IOAPIC routes)
Phase 3L: SMP AP boot (arch::smp::smp_boot_aps) â€” INIT-SIPI-SIPI via LAPIC ICR (page tables, LAPIC)
Phase 3M: release_all_aps                  â€” sets kernel_ready=true for all CPUs
Phase 3N: sti + int 32 test               (everything above)
Phase 3O: Unmask device routes             (IOAPIC routes)
Phase 4: Idle loop                         (all of the above)
```

This order ensures that each subsystem's dependencies are initialized before it runs. For example, heap depends on page tables (to map heap pages) which depends on the physical allocator (to allocate page table pages). APs are released after all kernel state is ready so they can immediately participate in scheduling.

---

# 10 â€” Future Architecture: Secure Runtime and Beyond

## Overview

The current LodaxOS kernel is phase 1 of a larger architecture. This document describes the planned Secure Runtime, PyI runtime, Agent model, and migration path from the current monolithic kernel to a capability-based microkernel system.

## Architecture Evolution

### Phase 1 (Current): Monolithic Boot Kernel

- Single address space (ring 0 only)
- All subsystems compiled into kernel ELF
- No process isolation
- No userspace
- Boot chain proven, hardware init complete

### Phase 2: Secure Runtime + Process Model

- Introduce kernel-managed processes with isolation
- Secure Runtime runs as the first userspace process (PID 1)
- Services (filesystem, drivers) run as separate processes managed by SR
- Capability-based IPC for inter-process communication
- Kernel retains scheduling, memory management, IPC primitives

### Phase 3: PyI Runtime

- PyI runs as a process under SR (PID 2+)
- Provides JIT-compiled Python/WASM execution environment
- All user-facing abstractions (files, windows, apps) are PyI objects
- REPL access to system state
- Application sandboxing within PyI's process

### Phase 4: Agent Model

- Multiple independent Agent domains
- Each Agent owns its own userspace environment
- Agent isolation via separate page table roots
- Secure Runtime manages Agent lifecycle and policies
- InstallerAuthority (IA) for system setup (destroyed after installation)
- InteUser (IU) for administrative operations

## Secure Runtime Architecture

### Position in the System

```
â”Œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”
â”‚ Kernel (Ring 0)                              â”‚
â”‚  Scheduler, Memory, IPC, HAL, Capabilities   â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ Secure Runtime (Ring 3, PID 1)               â”‚
â”‚  Service Manager, Policy Engine, Cap Broker  â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ Services (Ring 3, various PIDs)              â”‚
â”‚  Filesystem, Network, Audio, Display, etc.   â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ PyI Runtime (Ring 3, PID 2)                  â”‚
â”‚  App sandbox, UI, REPL, WASM JIT             â”‚
â”œâ”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”¤
â”‚ Agent 0 (Default User)                       â”‚
â”‚  Applications, desktop environment           â”‚
â””â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”˜
```

### Responsibilities

| Area | Responsibility |
|---|---|
| Service Management | Spawn, monitor, restart services per policy |
| Capability Issuance | Grant/revoke capabilities to processes |
| Policy Evaluation | Check every privileged operation against policy |
| Process Lifecycle | Define restart policies, handle failures |
| Permission Mutation | Modify process permissions at runtime |
| System Orchestration | Boot order, dependency resolution, health checks |

### Capability Model

Capabilities are the only way to access kernel resources. A process cannot do anything the kernel doesn't explicitly allow.

```
struct Capability {
    id: u64,              // unique capability identifier
    resource: Resource,   // what resource this grants access to
    rights: Rights,       // read, write, execute, manage
    expires: u64,         // optional expiry in ticks
    issuer: u64,          // SR process ID (only SR can issue caps)
}
```

### IPC Mechanism

IPC is built on capability-passing message channels:

```
Process A                          Process B
   â”‚                                  â”‚
   â”‚â”€â”€ send(channel, message, caps)â”€â”€â†’â”‚
   â”‚                                  â”‚â”€ receive(...) â†’ handle
   â”‚â†â”€â”€ reply(channel, response)â”€â”€â”€â”€â”€â”€â”‚
```

Channels are kernel objects created by SR. Each channel is identified by a capability. Message passing includes:
- Up to 64 bytes of inline data
- Up to 4 capability transfers
- Optional reply channel for request-response patterns

### Service Definition

Services are defined in metadata on Partition Zero (`/SecureRuntime/services/`):

```json
{
    "name": "filesystem",
    "binary": "/SecureRuntime/bin/fsd.elf",
    "type": "service",
    "restart": "on-failure",
    "memory": "64mb",
    "capabilities": ["block_io", "storage:read", "storage:write"],
    "depends_on": ["block"],
    "permissions": {
        "devices": ["ata", "nvme"],
        "paths": ["/system/*"]
    }
}
```

## PyI Runtime

### Architecture

PyI (Python Integral) is a self-contained process that provides:
1. A JIT-compiled Python runtime (WASM-backed for sandboxing)
2. System API library (`import system`) for application development
3. REPL access to all system functionality
4. Application isolation (each app is a sub-process or coroutine)

### Application Model

```python
import system

app = system.define(
    name="editor",
    permissions=["display", "storage"],
    restart="on-failure",
    memory="128mb"
)

@app.main
async def main():
    window = system.display.create_window(800, 600, "Editor")
    while True:
        event = await window.events.next()
        if event.type == "close":
            break
```

### REPL Accessibility

Every system function is accessible via the PyI REPL:

```
> system.processes.list()
[PID 1: Secure Runtime, PID 2: PyI, PID 3: fsd, PID 4: editor]

> system.display.list_modes()
[1920x1080@60, 1280x720@60, 1024x768@60]

> system.storage.mount("/dev/ata0", "/mnt/data")
```

### JIT Compilation

PyI compiles Python bytecode to WASM, which is then JIT-compiled to native code:
```
Python source â†’ bytecode â†’ WASM â†’ native code (via WASM runtime)
```

This provides:
- Sandboxed execution (WASM memory isolation)
- Near-native performance (JIT-compiled)
- Language-agnostic runtime (any WASM-compiling language can run)

## Agent Model

### Agent Definition

An Agent is a first-class system domain:

```rust
struct Agent {
    id: AgentId,
    name: String,
    state: AgentState,           // Active, SafeMode, Corrupted, Restoring
    processes: Vec<ProcessId>,
    runtime: AgentRuntime,       // PyI or other
    capabilities: CapabilitySet,
    storage: AgentStorage,       // agent-local persistent storage
}
```

### Agent Lifecycle

1. **Creation**: SR creates the agent, assigns an ID, allocates storage
2. **Boot**: SR starts PyI within the agent's domain
3. **Operation**: Agent runs normally, SR monitors heartbeat
4. **Safe Mode**: On failure, SR boots agent into Safe Mode (minimal REPL)
5. **Restoration**: SR restores agent from last known good state
6. **Deletion**: SR tears down agent, reclaims resources

### Agent Safe Mode

Safe Mode provides exactly 7 commands:

| Command | Purpose |
|---|---|
| `ls` | List files in agent storage |
| `cd` | Navigate agent storage |
| `read` | Display file contents |
| `write` | Write to a file |
| `start-userspace` | Exit Safe Mode, start normal runtime |
| `restart` | Warm restart the agent |
| `shutdown` | Halt the agent |

Safe Mode depends only on the kernel (serial, framebuffer, storage). It does NOT depend on PyI, the filesystem service, or any other service.

### Principal Invariant

LodaxOS requires at least one valid Agent definition at all times. If all agents are deleted or corrupted, the system enters an unrecoverable hard fault. This ensures there is always a principal capable of operating the system.

## Driver Architecture

### Philosophy

Drivers are services, not kernel modules. The kernel provides a hardware access layer (HAL), and driver services implement device-specific logic on top of it.

### Kernel HAL

The kernel provides:

| Interface | Purpose |
|---|---|
| PCI Enumeration | Discover devices, read config space |
| Interrupt Management | Allocate vectors, register handlers |
| DMA Management | Allocate DMA buffers, manage IOMMU |
| MMIO Mapping | Map device BARs into process address space |
| Device Ownership | Track which agent owns which device |

### Driver Service

A driver service is a regular process with additional capabilities:

```rust
struct DriverService {
    pci_device: PciAddress,      // which PCI device this driver manages
    interrupts: Vec<u8>,         // allocated interrupt vectors
    mmio_regions: Vec<MmioRegion>,  // mapped MMIO ranges
    dma_buffers: Vec<DmaBuffer>,    // allocated DMA memory
    ops: DriverOps,              // read, write, ioctl, etc.
}
```

### Device Sharing Models

**True Multiplex**: Hardware naturally supports multiple consumers (CPUs, network queues, audio mixing). The kernel allows direct access.

**Virtual Multiplex**: A service owns the physical device and virtualizes it (display server shares framebuffer, filesystem server shares storage). The service handles arbitration.

## System Boot Order (Future)

```
1. Firmware â†’ Bootloader â†’ Kernel
2. Kernel initializes (current Phase 1â€“4)
3. Kernel spawns Secure Runtime (load from Partition Zero)
4. SR loads service definitions from Partition Zero
5. SR spawns core services (block, filesystem, PCI)
6. SR spawns PyI
7. PyI initializes Agent 0's userspace
8. System ready â€” user login/REPL
9. SR monitors all services, handles failures
```

## State Storage

### Partition Zero Layout

```
Partition Zero (ext4, 512 MB):
  /kernel.elf                    â€” current kernel binary
  /Bootloader.efi                â€” current bootloader
  /sr.elf                        â€” current Secure Runtime stub
  /SecureRuntime/
    â”œâ”€â”€ bin/
    â”‚   â”œâ”€â”€ sr.elf               â€” Secure Runtime binary (future)
    â”‚   â”œâ”€â”€ fsd.elf              â€” filesystem daemon
    â”‚   â”œâ”€â”€ pci.elf              â€” PCI manager
    â”‚   â””â”€â”€ ...
    â”œâ”€â”€ config/
    â”‚   â”œâ”€â”€ order                â€” boot order definition
    â”‚   â”œâ”€â”€ policies/            â€” security policies
    â”‚   â””â”€â”€ services/            â€” service definitions
    â”œâ”€â”€ state/
    â”‚   â”œâ”€â”€ sr_state.bin         â€” serialized SR state
    â”‚   â””â”€â”€ recovery/            â€” recovery snapshots
    â””â”€â”€ backup/
        â”œâ”€â”€ kernel.elf           â€” backup kernel
        â””â”€â”€ sr.elf               â€” backup SR
  /Agents/
    â”œâ”€â”€ 0/
    â”‚   â”œâ”€â”€ config               â€” agent definition
    â”‚   â”œâ”€â”€ state/               â€” serialized agent state
    â”‚   â””â”€â”€ storage/             â€” agent-local files
    â””â”€â”€ ... (per agent)
  /System/
    â””â”€â”€ recovery/                â€” system-wide recovery metadata
```

## Migration Path

### Step 1: Process Abstraction (Current Kernel)

The kernel needs these additions before userspace can run:
- [ ] Ring 3 execution support (update GDT, IDT, page tables for user pages)
- [ ] Syscall dispatch via `syscall`/`sysret` instructions
- [ ] Process creation (allocate user page tables, map ELF segments)
- [ ] Basic IPC: kernel-level message channels

### Step 2: Secure Runtime (First Userspace)

- [ ] Write SR as a standalone ELF binary
- [ ] Implement capability system in kernel
- [ ] Kernel boots SR as PID 1
- [ ] SR implements service manager
- [ ] SR defines and enforces security policies

### Step 3: Filesystem Service

- [ ] Port ext4 parser from bootloader to a service
- [ ] Implement block device abstraction
- [ ] Filesystem service provides open/read/write/close via IPC
- [ ] Path resolution and permission checking in filesystem service

### Step 4: PyI Runtime

- [ ] Port or implement a WASM runtime
- [ ] Implement Python subset compiler â†’ WASM
- [ ] PyI runs as a process under SR
- [ ] System API library exposes system functions to Python

### Step 5: Agent Framework

- [ ] Implement agent creation/deletion in SR
- [ ] Per-agent page table management in kernel
- [ ] Agent state serialization/restoration
- [ ] Safe Mode implementation

## Capability-Based Security Model

### Principle

A process holds capabilities for exactly those resources it is allowed to access. There is no "root" or "superuser" â€” all privileges are explicit and granular.

### Capability Types

| Type | Resource | Rights |
|---|---|---|
| Memory | Physical pages, virtual ranges | Read, Write, Execute |
| IPC | Channel endpoints | Send, Receive, Reply |
| Device | PCI devices, MMIO regions | Read, Write, Interrupt |
| Storage | Partitions, filesystem paths | Read, Write, Create, Delete |
| Scheduling | CPU time, priorities | Set quantum, Set priority |
| Management | Process lifecycle | Create, Kill, Set policy |

### Policy Evaluation Flow

```
Process requests operation
  â†’ request routed through SR
    â†’ SR checks capability set
    â†’ SR evaluates policy rules
      â†’ if allowed: grant and cache capability
      â†’ if denied: return error (SIGCAPDENY)
    â†’ kernel enforces the capability
```

### Signal Injection

When SR needs to revoke a capability mid-execution:
1. SR asks the kernel to deliver `SIGCAPREVOKE` to the process
2. Process's signal handler can clean up gracefully
3. If no handler: default action (terminate)
4. Next syscall from the process fails with `CAPREVOKED`

This is softer than hard revocation (which would immediately fail the next memory access or syscall) but requires the process to opt into the signal protocol.

---

*Generated from 11 module files - 2741 total lines*

