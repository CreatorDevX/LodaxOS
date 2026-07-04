# Memory Management

LodaxOS implements four layers of memory management:

1. **Physical Buddy Allocator** (`kernel/src/mm/phys.rs`) — manages physical
   page frames.
2. **Virtual Page Tables** (`kernel/src/mm/virt.rs`) — 4-level x86-64 paging.
3. **Slab Heap** (`kernel/src/mm/heap.rs`) — kernel heap via object caches.
4. **VMA Tree** (`kernel/src/mm/vma.rs`) — demand-paging and process memory.

---

## 1. Physical Buddy Allocator (`kernel/src/mm/phys.rs`)

### Constants

| Constant          | Value  | Description                       |
|-------------------|--------|-----------------------------------|
| `MAX_ORDER`       | `10`   | Largest order (2^10 = 1024 pages) |
| `PAGE_SHIFT`      | `12`   | Page shift                        |
| `PAGE_SIZE`       | `4096` | Bytes per page                    |
| `BOOTINFO_HANDOFF_PAGE` | `0x5000` | BootInfo handoff page |

### Zone Structure

A single `Zone` tracks free memory:

```c
struct Zone {
    base: u64;                                    // first managed address
    top: u64;                                     // last managed address
    free_lists: [*mut FreeBlock; MAX_ORDER + 1];  // per-order free lists
    total_pages: AtomicUsize;                     // total managed pages
    free_pages: AtomicUsize;                      // currently free pages
}
```

Each order `n` has its own `IrqSaveSpinLock` (defined as `LOCKS[n]`) to
allow parallel allocation from different orders.

### Free Block

```c
// 16 bytes
struct FreeBlock {
    next: *mut FreeBlock,   // offset 0x00
    order: usize,           // offset 0x08
}
```

Blocks are embedded in the free pages themselves. `block_size(order) = (1 << order) * PAGE_SIZE`.

### Internal Functions

| Function                      | Description                                          |
|-------------------------------|------------------------------------------------------|
| `add_block(zone, phys, order)`    | Insert a block at the head of `zone.free_lists[order]` |
| `pop_block(zone, order)`          | Remove and return the head block                     |
| `remove_block(zone, target, order)` | Remove a specific block from a free list          |
| `carve_range(zone, start, end)`   | Split a range into maximal power-of-2 buddy blocks   |
| `coalesce(zone, addr, order)`     | Try to merge freed block with its buddy, recursing upward |
| `remove_range(zone, rstart, rend, order)` | Remove all overlapping blocks, re-inserting non-overlapping parts |

Buddy address computation: `buddy = addr ^ block_size(order)`.

### Public API

| Function | Signature | Description |
|----------|-----------|-------------|
| `init_from_regions` | `(regions: &[(u64,u64)], boot_info_phys: u64, exclude_ranges: &[(u64,u64)])` | Initialise the allocator from boot memory regions. Excludes reserved pages (page 0, BootInfo pages, framebuffer, kernel image, drivers ELF, APIC MMIO). |
| `reserve_range` | `(start: u64, size_in_pages: usize)` | Remove a range from all free lists. Used for trampoline page. |
| `alloc_order` | `(order: usize) -> Option<u64>` | Allocate a block of `2^order` pages. Searches upward if target order empty, splitting larger blocks. |
| `free_order` | `(addr: u64, order: usize)` | Free a block, coalescing with buddy if possible. |
| `alloc_page` | `() -> Option<u64>` | Allocate 1 page (order 0). |
| `free_page` | `(addr: u64)` | Free 1 page. |
| `alloc_pages` | `(count: u64) -> Option<u64>` | Allocate `count` contiguous pages (rounded up to power-of-2, excess released). |
| `free_pages` | `(addr: u64, count: u64)` | Free `count` pages. |
| `free_pages_count` | `() -> usize` | Number of currently free pages. |
| `total_pages` | `() -> usize` | Total managed pages. |

### Reserved Page Tracking

Page 0 and `BOOTINFO_HANDOFF_PAGE` (`0x5000`) are permanently reserved.
The BootInfo struct pages are tracked by `BOOTINFO_RESERVED_PN` and
`BOOTINFO_RESERVED_PAGES`. Explicit exclude ranges (framebuffer, kernel
image, drivers ELF, APIC MMIO) are checked via `is_excluded()`.

### Locking

Each order (0..MAX_ORDER) has an independent `IrqSaveSpinLock`. The
allocator acquires locks from high→low order to avoid deadlock:
`coalesce` locks the current order, calls itself for higher orders only
after releasing.

---

## 2. Virtual Page Tables (`kernel/src/mm/virt.rs`)

### Layout

Standard 4-level x86-64 paging:

```
PML4[0..511]  →  PDP[0..511]  →  PD[0..511]  →  PT[0..511]  →  4 KB page
```

| Level | Name          | Bits        | Granularity     |
|-------|---------------|-------------|-----------------|
| 3     | PML4          | 47:39       | 512 GB          |
| 2     | PDP           | 38:30       | 1 GB (huge)     |
| 1     | PD            | 29:21       | 2 MB (huge)     |
| 0     | PT            | 20:12       | 4 KB            |

Index function: `index_for_addr(virt, level) = ((virt >> (12 + level * 9)) & 0x1FF)`.

### Constants

| Constant       | Value                        | Description              |
|----------------|------------------------------|--------------------------|
| `HIGHER_HALF`  | `0xFFFF_8000_0000_0000`      | Kernel space base        |
| `PAGE_SIZE`    | `0x1000`                     | Page size                |
| `PRESENT`      | `1 << 0`                     | PTE present bit          |
| `WRITABLE`     | `1 << 1`                     | PTE writable bit         |
| `USER`         | `1 << 2`                     | PTE user bit             |
| `CACHE_DISABLE`| `1 << 4`                     | PCD bit (MMIO)           |
| `NO_EXECUTE`   | `1 << 63`                    | NX bit                   |
| `DATA`         | `PRESENT\|WRITABLE\|NO_EXECUTE` | Kernel data mapping  |
| `COW`          | `1 << 11`                    | Software COW flag        |

### Page Table Structure

```c
struct PageTable {
    entries: [u64; 512];   // 4096 bytes, page-aligned
}
```

### Init Sequence (`virt::init`)

1. Allocate PML4 page, zero it.
2. Map each free memory region into higher-half (4 KB unaligned edges, 2 MB
   huge pages in the middle).
3. Identity-map the first 4 GB using 2 MB huge pages (PML4[0] → 4 PDPs, each
   PDP → PD with 512 huge-page entries). The LAPIC page (0xFEE00000) is
   mapped CACHE_DISABLE.
4. Map framebuffer in higher-half (4 KB pages).
5. Load CR3 with new PML4.

### Public API

| Function | Signature | Description |
|----------|-----------|-------------|
| `kernel_pml4` | `() -> u64` | Return kernel PML4 physical address |
| `init` | `(regions, fb_phys)` | Set up page tables, load CR3 |
| `translate` | `(virt: u64) -> Option<u64>` | Walk current page tables, return physical address |
| `unmap` | `(virt: u64)` | Remove mapping, free empty intermediate tables, TLB flush + shootdown |
| `current_pml4` | `() -> u64` | Read CR3 |
| `pml4_address` | `() -> u64` | Alias for `current_pml4()` |
| `switch_pml4` | `(pml4_phys: u64)` | Write CR3 (with MFENCE) |
| `fork_pml4` | `(src_phys: u64) -> Option<u64>` | Deep-copy PML4 hierarchy, COW for user pages |
| `free_pml4` | `(pml4_phys: u64)` | Free all page-table pages (not mapped pages) |
| `map_contiguous` | `(pml4, virt, phys, pages, flags)` | Map contiguous phys range (batched per PT) |
| `map_pages_from_phys` | `(pml4, virt, phys_pages[], flags)` | Map non-contiguous phys pages |
| `map_region` | `(pml4, phys, size, flags)` | Identity-map + higher-half map |
| `map_region_higher_half` | `(pml4, phys, size, flags)` | Higher-half only |
| `map_page_explicit` | `(pml4, virt, phys, flags)` | Single page, with TLB shootdown |
| `read_pte` | `(pml4, virt) -> Option<u64>` | Read PTE value |
| `write_pte` | `(pml4, virt, pte) -> Option<()>` | Write PTE value |

### TLB Shootdown

When a mapping is changed on a shared PML4, the kernel broadcasts an IPI
(vector `0x81`) to all other CPUs. Each CPU executes `invlpg` and writes
its `TLB_ACK[i]`. If a CPU does not respond within `1_000_000` spins, a
`pending_tlb_flush` flag is set in its `PerCpu` slot, processed on the
next interrupt entry.

### Copy-on-Write

`fork_pml4` deep-copies the PML4 hierarchy. For user-half entries (PML4
indices 0..255), writable leaf PTEs are marked `COW | read-only`. On a
write fault, `resolve_cow()` allocates a new page, copies content, and
maps it writable in the faulting PML4.

---

## 3. Slab Heap (`kernel/src/mm/heap.rs`)

### Slab Structure

```c
struct Slab {
    free_head: *mut u8,    // head of free object list
    slab_base: *mut u8,    // start of this slab's memory
    next: *mut Slab,       // next slab in cache list
    prev: *mut Slab,       // prev slab in cache list
    order: u8,             // page order (0..4)
    total_objs: u16,       // total objects in slab
    free_objs: u16,        // currently free objects
}
```

Slab initialisation: first allocation from `phys::alloc_order`, then
higher-half virtual mapping, then the free list is threaded through
the object space (each object's first 8 bytes point to the next).

### Cache Organization

| Cache Index | Object Size | Slab Order | Objects/Slab |
|-------------|-------------|------------|--------------|
| 0           | 32 B        | 0          | 127          |
| 1           | 64 B        | 0          | 63           |
| 2           | 128 B       | 0          | 31           |
| 3           | 256 B       | 0          | 15           |
| 4           | 512 B       | 0          | 7            |
| 5           | 1024 B      | 0          | 3            |
| 6           | 2048 B      | 0          | 1            |
| 7           | 4096 B      | 1          | 1            |
| 8           | 8192 B      | 2          | 1            |

Each cache has its own `IrqSaveSpinLock`. Allocations larger than 8192 B
fall through to direct page allocation via `phys::alloc_order`.

### Public API

| Function | Description |
|----------|-------------|
| `heap::init()` | Initialise all 9 caches with computed sizes |
| `GlobalAllocator` | Implements `GlobalAlloc` via `kmalloc_aligned` |

Lock order: heap locks are acquired after phys locks to avoid deadlock.
The slow path releases the cache lock before calling `phys::alloc_order`.

---

## 4. VMA Tree (`kernel/src/mm/vma.rs`)

### Radix Tree Structure

4-level radix tree, 10 bits per level (indexing bits 12..51):

```
Level 3: bits 51:42  (10 bits → 1024 entries)
Level 2: bits 41:32  (10 bits → 1024 entries)
Level 1: bits 31:22  (10 bits → 1024 entries)
Level 0: bits 21:12  (10 bits → 1024 entries) → *mut Vma
```

Each node is 2 pages (8 KB), allocated with `phys::alloc_order(1)`.

### Permissions

```c
enum VmaPerm : u64 {
    None           = 0,
    Read           = 1,
    Write          = 2,
    ReadWrite      = 3,
    Execute        = 4,
    ReadExecute    = 5,
    WriteExecute   = 6,
    ReadWriteExecute = 7,
}
```

### VMA

```c
struct Vma {
    start: u64,       // start virtual address
    end: u64,         // end virtual address (exclusive)
    perm: VmaPerm,     // access permissions
    flags: u64,       // software-defined flags
}
```

### VmaTree

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `() -> Self` | Create empty tree |
| `insert` | `(&mut self, vma: &mut Vma)` | Insert by `vma.start` |
| `lookup` | `(&mut self, addr: u64) -> Option<&mut Vma>` | Find at exact start address |
| `find_covering` | `(&mut self, addr: u64) -> Option<&mut Vma>` | Find VMA where `addr ∈ [start, end)` |
| `remove` | `(&mut self, start: u64) -> bool` | Remove and free VMA, free empty nodes |
| `visit_all` | `(&mut self, F) -> Option<R>` | Iterate all VMAs |

### ProcessMemory

```c
struct ProcessMemory {
    vma_tree: VmaTree,
    pml4_phys: u64,
}
```

| Method | Description |
|--------|-------------|
| `add_vma(start, end, perm)` | Create and insert a VMA |
| `handle_page_fault(addr, write)` | Demand-page: allocate phys page, map it; handle COW |

### Kernel VMA Tree

A global `KERNEL_VMA_TREE` (`IrqSaveSpinLock<VmaTree>`) is initialised with
a single VMA covering `0xFFFF_8080_0000_0000` .. `0xFFFF_8080_0400_0000`
(64 MB kernel heap region).

```c
pub fn handle_page_fault(fault_addr: u64, error_code: u64) -> bool
```

Called from the `#PF` handler (`kernel/src/arch/idt.rs:548`). Handles:
- COW faults (present + write + COW bit)
- Kernel-mode demand paging via `KERNEL_VMA_TREE`
- Returns `false` for user-mode faults (not yet handled)
