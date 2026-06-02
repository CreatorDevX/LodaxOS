# 02 — Memory Model

## Overview

LodaxOS uses a four-tier memory model:

1. **Physical memory** — managed by a buddy allocator with per-order free lists (orders 0–10, 4 KB – 4 MB)
2. **Virtual memory** — 4-level page tables with higher-half kernel mapping
3. **Slab heap** — SLUB-style allocator with per-size caches (32 B – 8 KB), backed by buddy pages
4. **VMA / Demand paging** — radix-tree-based VMA tracking with per-page fault resolution

## Physical Memory Layout

### Address Space (Standard x86-64)

```
0x0000_0000_0000 ———— Reserved (real-mode IVT, BDA)
0x0000_0000_1000 ———— BootInfo pointer (8 bytes, chainloader → bootloader → kernel)
0x0000_0000_2000 – 0x0000_0009_FFFF ———— Usable (below 640 KB)
0x0000_000A_0000 – 0x0000_000F_FFFF ———— Legacy hole (VGA, BIOS ROM)
0x0000_0010_0000 ———— Kernel loaded here (0x100000 = 1 MB)
0x0000_0010_0000 – 0x0000_00FF_FFFF ———— Kernel segments (text, rodata, data, bss)
0x0000_0100_0000 – 0x0000_07FF_FFFF ———— Free (first 128 MB after kernel)
0x0000_0800_0000 – 0xFFFF_FFFF_FFFF ———— Free memory (up to 4 TB physical)
```

### Reserved Physical Pages

| Address | Purpose | Reason |
|---|---|---|
| `0x0000_0000` | Null guard | Rust UB on null pointer dereference |
| `0x0000_1000` | BootInfo handoff pointer | Inter-stage communication (8 bytes) |
| Buddy-allocated | All dynamic allocations | Pages acquired from buddy allocator |

The BootInfo struct itself is no longer at a fixed address. The chainloader allocates it dynamically via `Box::new(BootInfo)` (backed by UEFI's page allocator, which identity-maps the result) and stores the physical pointer at `0x1000`. The bootloader reads this pointer, updates the BootInfo fields, and passes the same pointer in RDI to the kernel.

## Physical Page Allocator — Buddy System (`src/mm/phys.rs`)

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
5. Skip the first page (`0x0000_0000`) and the BootInfo handoff page (`0x0000_1000`) — these are always reserved.

### Allocation (`alloc_order(n)`, `alloc_page()`, `alloc_pages(count)`)

**Single order** (`alloc_order(n)`):
1. Pop from `free_lists[n]` if non-empty → O(1).
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
4. Refuse to free reserved pages (0 and 0x1000).

### Thread Safety

A `SpinLock` (implemented as `AtomicBool` with `compare_exchange_weak` + `pause` loop) protects all zone operations. The lock disables interrupts during critical sections via `cli; lock; cmpxchg; sti`. The allocator is callable from multiple CPU cores and from interrupt context (though interrupt handlers should not allocate by design).

### Performance Characteristics

- Allocation: O(orders) worst case (split chain), O(1) when target order is non-empty.
- Deallocation: O(orders) worst case (coalesce chain), O(1) when buddy is busy.
- Internal fragmentation: At most (2^n - 1) pages per allocation; worst case < 50% for misaligned sizes.
- External fragmentation: None within the buddy system (coalescing is greedy and complete).

## Virtual Memory (`src/mm/virt.rs`)

### Address Space Layout

```
0x0000_0000_0000_0000 – 0xFFFF_7FFF_FFFF_FFFF ———— Userspace (48-bit)
0xFFFF_8000_0000_0000 – 0xFFFF_FFFF_FFFF_FFFF ———— Kernel higher-half
   0xFFFF_8000_0000_0000 – physical_memory_end ———— Physical memory mapping
   0xFFFF_8080_0000_0000 – 0xFFFF_8084_0000_0000 ———— Heap / slab arena (up to 64 MB)
```

All physical memory is identity-mapped (PML4[0] → 4 PDP entries covering 0–4 GB) and also mapped in the higher-half at `HIGHER_HALF + phys_addr`.

### Page Table Structure

4-level translation (PML4 → PDP → PD → PT → 4 KB page), plus support for 2 MB huge pages at the PD level and 1 GB huge pages at the PDP level.

```
PML4 (1 entry) → PDP (512 entries)
  Each PDP entry covers 1 GB (512 × 2 MB)
  PDP[0..3] = identity-map first 4 GB with 2 MB huge pages
  Other PDP entries = higher-half mappings

PDP (1 entry) → PD (512 entries)
  Each PD entry covers 2 MB
  If bit 7 (PS) = 1: 2 MB huge page
  If bit 7 = 0: points to PT

PD (1 entry) → PT (512 entries)
  Each PT entry covers 4 KB
  PT entries point to 4 KB physical pages
```

### Initialization (`virt::init`)

Phase 1–5: Allocate PML4, map higher-half for all boot regions (mix 2 MB huge + 4 KB), identity-map first 4 GB, map framebuffer, load CR3. See the `init` function and inline comments for details.

### Key API

| Function | Purpose |
|---|---|
| `translate(virt)` | Walk current page tables to resolve virtual → physical |
| `unmap(virt)` | Clear PT entry, flush TLB with `invlpg` |
| `map_page(pml4, virt, phys, flags)` | Create a single 4 KB page table mapping |
| `map_page_explicit(pml4, virt, phys, flags)` | Public (non-`unsafe`) wrapper for guarded callers |
| `map_contiguous(pml4, virt, phys, num_pages, flags)` | Map many pages with batch PT walk |
| `map_region(pml4, phys, size, flags)` | Identity + higher-half mapping |
| `map_region_higher_half(pml4, phys, size, flags)` | Higher-half only (for MMIO) |

### Higher-Half Only for MMIO

The identity map uses 2 MB huge pages. Creating a 4 KB page at the same PD level would conflict (the CPU would see the 4 KB entry's flags, but the PD entry is marked as a huge page). Therefore, MMIO regions like LAPIC and IOAPIC are mapped only in the higher-half, with smaller pages that coexist at different virtual addresses referring to the same physical memory. This is a known workaround — the proper fix is to split the PDP entry into 512 PD entries and mark the MMIO 2 MB slot with cache-disable, but that has not been implemented yet.

## Slab Heap Allocator (`src/mm/heap.rs`)

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
2. The `ptr` itself encodes no cache information — size must match allocation.
3. Locate the slab by scanning `partial` and `full` lists (linear scan; acceptable for small lists).
   - Future optimization: embed a slab pointer in the object header area.
4. Push the object onto the slab's free list.
5. If the slab becomes fully free, move it to the `free` list.
6. Free slabs are not returned to the buddy — they remain cached for reuse. (Future: periodic reclamation.)

### Global Allocator

`GlobalAllocator` implements `core::alloc::GlobalAlloc`, delegating to `kmalloc/kfree`. It is installed as `#[global_allocator]`. A static `initialized` flag gates allocation before the slab system is ready; early `alloc` calls return null (the `alloc` crate panics gracefully).

### Virtual Address Range

The slab system does not pre-map a fixed heap arena. Each new slab allocates physical pages from the buddy allocator and maps them directly at `HEAP_VIRT_BASE + offset` using `map_contiguous`. The virtual arena is a contiguous 64 MB region starting at `0xFFFF_8080_0000_0000`.

### Thread Safety

Each `KmemCache` has its own `SpinLock`. The `GlobalAllocator` dispatches to the correct cache by size. Different-size allocations can proceed in parallel on different cores (fine-grained locking), but same-size allocations on different cores serialize.

## VMA / Demand Paging (`src/mm/vma.rs`)

### Radix Tree

The VMA tree uses a 4-level radix tree covering bits 12–51 of the virtual address (40 bits = 1 TB addressable per tree). Each level indexes 10 bits:

```
Level 0 (bits 51:42) → Level 1 (bits 41:32) → Level 2 (bits 31:22) → Level 3 (bits 21:12)
```

Each node is a `VmaNode` holding a tagged union: either a slab node (slot array of 1024 `Option<Box<VmaNode>>`) or a leaf node (slot array of 1024 `Option<Box<Vma>>`).

### VMA Struct

```c
struct Vma {
    start: u64,         // virtual start address (page-aligned)
    end: u64,           // virtual end address (exclusive, page-aligned)
    perm: u8,           // permission bits (Read, Write, Execute)
    flags: u8,          // VMA flags (Kernel, User, Guard, etc.)
}
```

### VMA Tree Operations

| Operation | Description |
|---|---|
| `insert(vma)` | Walk the 4-level tree, allocate leaf/tree nodes as needed, insert VMA into the correct leaf slot. |
| `remove(start)` | Walk the tree to the leaf slot covering `start`, remove the VMA, and clean up empty nodes. |
| `find_covering(addr)` | Walk the tree to the leaf slot, scan up to 1024 VMAs for one covering `addr`. |
| `find(start, end)` | Walk the tree to the leaf slot, scan for a VMA at exact `start..end`. |
| `visit_all(f)` | Recursively traverse all leaf slots, apply `f` to each VMA. |

`find_covering` does a linear scan of up to 1024 VMA entries in the target leaf. With <100 VMAs per process in practice, this is fast enough. The tree structure makes it O(1) to locate the correct leaf slot.

### Global Kernel VMA Tree

A single `static KERNEL_VMA_TREE: VmaTree` tracks all kernel-mode VMAs. Initialized by `init_kernel_vmas()`:

1. `kernel_code` — `0xFFFF_8000_0000_0000` to `kernel_end` (Read + Execute, backed by bootloader identity map)
2. `kernel_data` — `kernel_end` to `0xFFFF_8000_0020_0000` (Read + Write, backed by identity map)
3. `kernel_heap` — `HEAP_VIRT_BASE` to `HEAP_VIRT_BASE + 64 MB` (Read + Write, demand-paged)
4. `kernel_mmio` — `HIGHER_HALF + 0xF0000000` to `HIGHER_HALF + 0xFFFFFFFF` (Read + Write + Uncached, backed by identity map)

Additional VMAs may be inserted during kernel init (e.g., for framebuffer).

### Page Fault Handler (`handle_page_fault(addr, error_code)`)

Called from the #PF handler in `src/arch/idt.rs`:

1. Read CR2 (the faulting address).
2. If the fault originated in user mode (`error_code & 4`): walk the current process's `ProcessMemory` tree (future: per-process page tables).
3. If the fault originated in kernel mode: walk `KERNEL_VMA_TREE`.
4. If no covering VMA is found: panic/halt — this is an unhandled page fault (bug or access violation).
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

- VMA lookup: O(1) tree walk + O(n) scan in leaf (n ≤ 1024, typically <100).
- Page fault resolution: O(1) buddy allocation + O(4) page table walks (4 levels).
- Memory overhead: Each VMA tree node is 8 KB (1024 × 8-byte slots). A tree with 1 VMA uses ~8 KB for the root; a tree with 1000 VMAs uses ~32 KB (4 nodes at 3 levels × 8 KB + ~16 KB for VMA Boxes).

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
| `src/mm/phys.rs` | Buddy allocator |
| `src/mm/heap.rs` | Slab allocator |
| `src/mm/vma.rs` | Radix tree, VMA, page fault handler |
| `src/mm/virt.rs` | Page table management |
| `src/mm/mod.rs` | Module declarations |
| `src/arch/idt.rs` | IDT entry points (including #PF) |

## Migration from Previous System

The previous memory model used:
- **Bitmap allocator** (`phys.rs`): O(n/64) linear scan, high external fragmentation with multi-page allocations.
- **Linked-list allocator** (`heap.rs`): First-fit with O(n) scan, no slab caching for small objects.
- **Fixed BootInfo at `0x1000`**: Hard-coded address limited BootInfo size and conflicted with kernel layout.

The new system replaces all three with minimal memory overhead and O(1) common-case allocation. The BootInfo handoff is fully dynamic, removing the fixed-address constraint.
