# Memory Layout — LodaxOS Runtime

> **Note:** All addresses assume x86-64 with 4-level paging. See
> `kernel/src/consts.rs`, `kernel/src/mm/virt.rs`, and `kernel/linker.ld`
> for authoritative definitions.

---

## 1. Physical Memory Overview

```
Legend:  [RES] Reserved  [BUD] Buddy-allocator managed pages  [MMIO] Memory-mapped I/O

 0x0000_0000 ┌──────────────────────────────────────┐  [RES]
             │ NULL guard (reserved, never mapped)   │  4 KB
 0x0000_1000 ├──────────────────────────────────────┤  [BUD]
             │ Real-mode IVT + BDA + EBDA            │  ~14 KB
 0x0000_5000 ├──────────────────────────────────────┤  [RES]
             │ BootInfo handoff pointer (8 bytes)    │  4 KB  ◄── BOOT_INFO_HANDOFF_ADDR
 0x0000_6000 ├──────────────────────────────────────┤  [BUD]
             │ (available)                           │  8 KB
 0x0000_8000 ├─┬────────────────────────────────────┤  [RES]
             │ │ SIPI trampoline (smp_trampoline.bin) │  ≤1 KB  ◄── TRAMPOLINE_PHYS (0x8000)
             │ │ Slot map table   (0x8000 + 0x300)   │  256 B
             │ │ Mailbox slots    (0x8000 + 0x400)   │  4×0x80 = 512 B  ◄── MAILBOX_OFF
 0x0000_9000 ├─┴────────────────────────────────────┤  [BUD]
             │ (available)                           │  ~28 KB
0x0001_0000 ├──────────────────────────────────────┤  [RES]
              │ Kernel image                           │  ◄── linker.ld entry (0x100000)
              │  ├─ .text     (code, ALIGN 16)        │
              │  ├─ .rodata   (read-only data)        │
              │  ├─ .data     (initialized data)      │
               │  ├─ .bss      (zero-initialized)      │
               │  │    └─ currently contains:          │
               │  │         IST stacks (4×16 KB each)  │
               │  │         PERCPU array               │
               │  │         GDT / TSS / IDT            │
               │  │         Dummy stacks               │
               │  │         Buddy zone metadata        │
               │  │    (layout is linker-script        │
               │  │     defined, not a fixed ABI)      │
              └─ __kernel_end ...
                    │
       ...          │    (buddy allocator managed pages)
                    │
 0x2000_0000 ┌─────┴─────────────────────────────────┐  [RES]
              │ Driver package (load address)        │  ◄── drivers/linker.ld
              │  ├─ .text     (code)                   │
              │  ├─ .rodata   (read-only data)         │
              │  ├─ .data     (initialized data)       │
              │  └─ .bss      (zero-initialized)       │
              └────────────────────────────────────────┘
                    │
       ...          │    (buddy allocator managed pages)
                    │
  0xFEC0_0000 ┌─────┴─────────────────────────────────┐  [MMIO]
             │ IOAPIC MMIO                            │  ◄── IOAPIC_PHYS
             └────────────────────────────────────────┘
 0xFEE0_0000 ┌────────────────────────────────────────┐  [MMIO]
             │ LAPIC MMIO                             │  ◄── LAPIC_PHYS
 0xFF00_0000 └────────────────────────────────────────┘
             │                   APIC_MMIO_SIZE = 4 MB
             │    (buddy allocator managed pages)
             └────────────────────────────────────────┘
```

### Reserved Physical Ranges (never added to buddy free lists)

| Start          | End            | Size      | Reason                        |
|----------------|----------------|-----------|-------------------------------|
| `0x0000_0000`  | `0x0000_0FFF`  | 4 KB      | Null-deref guard              |
| `0x0000_5000`  | `0x0000_5FFF`  | 4 KB      | BootInfo handoff page         |
| `0x0000_8000`  | `0x0000_8FFF`  | 4 KB      | SIPI trampoline               |
| `__kernel_start` | `__kernel_end` | varies  | Kernel image (.text+.rodata+.data+.bss) |
| `Framebuffer`  | `Framebuffer base + size` | varies    | GOP framebuffer pages         |
| `kernel_image_addr` | +size   | varies    | Kernel image staging buffer     |
| `drivers_pkg_addr`  | +size   | varies    | Driver package staging buffer |
| `0xFEC0_0000`  | `0xFF00_0000`  | 4 MB      | APIC MMIO (IOAPIC+LAPIC)      |
| BootInfo struct | +pages       | ~2 KB     | Chainloader-allocated BootInfo |

---

## 2. Virtual Memory Layout (after `virt::init`)

```
  0x0000_0000_0000_0000 ┌─────────────────────────────┐  PML4[0]
                       │                             │
                       │   USER HALF                  │
                       │   PML4[0..255]              │
                       │                             │
                        │   Identity map of physical 0..4 GiB         │
                        │   (2 MiB huge pages, never removed)         │
                        │   (PML4[0] → 4 PDPs → PDs                  │
                        │    → 2 MiB leaf entries)                     │
                       │                             │
                       │   LAPIC MMIO (0xFEE0_0000)  │  PCD bit set (cache-disabled)
                       │   IOAPIC MMIO (0xFEC0_0000) │  PCD bit set (cache-disabled)
                       │                             │
  0xFFFF_7FFF_FFFF_FFFF └─────────────────────────────┘  PML4[255]
  0xFFFF_8000_0000_0000 ┌─────────────────────────────┐  PML4[256] ◄── HIGHER_HALF
                       │                             │
                       │   KERNEL HALF                │
                       │   PML4[256..511]            │
                       │                             │
                        │   Direct physical map       │
                        │   (linear mapping):         │
                        │   VA = HIGHER_HALF + PA     │
                        │   (2 MB huge pages, NX)     │
                        │   phys_to_virtual()         │
                        │   virt = HIGHER_HALF + phys │
                       │                             │
                        │   Kernel image is           │
                        │   mapped through two virtual aliases:               │
                        │   identity (boot compatibility) and                 │
                        │   higher-half (linear mapping)                      │
                        │   for IST stacks, PERCPU, GDT, TSS, IDT access     │
                       │                             │
  0xFFFF_8080_0000_0000 ├─────────────────────────────┤  ◄── KERNEL HEAP VMA BASE
                       │                             │
                       │   KERNEL HEAP VMA REGION      │
                       │                             │
                        │   VMA region 0xFFFF_8080_0000_0000..+64MB │
                        │   Reserved lazily populated virtual region │
                        │                                          │
                        │   Currently, kmalloc() returns addresses  │
                        │   from the direct map (HIGHER_HALF + phys),│
                        │   not from this VMA region. The heap VMA  │
                        │   is reserved for a future fully virtual  │
                        │   heap with demand paging.                │
                       │                             │
  0xFFFF_8080_0400_0000 ├─────────────────────────────┤  ◄── KERNEL HEAP VMA END
                       │                             │
                       │   (unmapped / unused)       │
                       │                             │
  0xFFFF_FFFF_FFFF_FFFF └─────────────────────────────┘  PML4[511]
```

### Page-Table Access Flags (U/S and R/W)

The **User/Supervisor (U/S) bit** determines whether a page is accessible
from Ring 3:

| Region               | PML4 Slots     | U/S Bit | Driver (Ring 3) Access      |
|----------------------|----------------|---------|-----------------------------|
| Identity map         | PML4[0]        | **Set** | Read/write to low phys mem  |
| Driver ELF segments  | PML4[0..255]   | **Set** | Normal user-mode access     |
| Kernel higher-half   | PML4[256..511] | **Clear** | Fault on any access       |

**Identity map (PML4[0]):**
Because PML4[0] lies in the user-half virtual address space, its page-table
entries must set the U/S bit to 1 for the kernel itself to access them (the
kernel runs in Ring 0, and a Ring-0 access to a supervisor-only page in the
user half would also fault). Consequently, any Ring 3 driver that inherits the
identity map via `fork_pml4` can **read and write low physical memory**
(0x0000_0000 – 0xFFFF_FFFF). This is a known limitation — see the note in §6.

**Kernel higher-half (PML4[256..511]):**
All kernel-half entries have U/S = 0 (supervisor-only). Even though these
mappings are inherited by a driver's forked PML4, any Ring 3 access to them
triggers a page fault. The kernel half is therefore **not** merely "read-only
to the driver" — it is **entirely inaccessible** from Ring 3.

### Virtual ↔ Physical Translation

```text
 Given a physical address PA, its higher-half virtual address is:

    VA = HIGHER_HALF + PA   (0xFFFF_8000_0000_0000 + PA)

 Conversely, for a VA ≥ HIGHER_HALF:

    PA = VA - HIGHER_HALF
```

### Page Table Walk (4-level x86-64)

```
63                  48 47      39 38      30 29      21 20      12 11        0
+--------------------+----------+----------+----------+----------+------------+
| Sign Extension     | PML4 idx | PDP idx  | PD idx   | PT idx   | Offset     |
|                    | bit 47..39| bit 38..30| bit 29..21| bit 20..12| bit 11..0 |
|                    | (9 bits) | (9 bits) | (9 bits) | (9 bits) | (12 bits)  |
+--------------------+----------+----------+----------+----------+------------+

 index(virt, level) = ((virt >> (12 + level * 9)) & 0x1FF)
```

---

## 3. Kernel Heap / Slab Allocator Detail

The slab allocator (`kernel/src/mm/heap.rs`) manages 9 caches:

| Cache | Object Size | Slab Order | Objects/Slab | Backing Pages |
|-------|-------------|------------|--------------|---------------|
| 0     | 32 B        | 0          | 127          | 1 × 4 KB      |
| 1     | 64 B        | 0          | 63           | 1 × 4 KB      |
| 2     | 128 B       | 0          | 31           | 1 × 4 KB      |
| 3     | 256 B       | 0          | 15           | 1 × 4 KB      |
| 4     | 512 B       | 0          | 7            | 1 × 4 KB      |
| 5     | 1024 B      | 0          | 3            | 1 × 4 KB      |
| 6     | 2048 B      | 0          | 1            | 1 × 4 KB      |
| 7     | 4096 B      | 1          | 1            | 2 × 4 KB      |
| 8     | 8192 B      | 2          | 1            | 4 × 4 KB      |

Allocations > 8192 B fall through to direct `phys::alloc_order()` — the
physical backing is mapped at `HIGHER_HALF + phys` and returned.

Slab physical pages are allocated from the buddy allocator, then explicitly
mapped into the kernel PML4 at `HIGHER_HALF + phys`. The kernel VMA tree
covers the 64 MB heap region as a lazily populated virtual region.

### Address Flow Diagram

The diagram below shows how a physical page flows through the allocator stack:

```
Physical RAM

  0x100000
    Kernel image         (reserved, not allocatable)
  0x250000
    Buddy page           (allocated by buddy allocator)
      │
      ▼
Direct map (linear mapping)

  FFFF_8000_0025_0000   (HIGHER_HALF + 0x250000)
      │
      ▼
kmalloc()

  returns FFFF_8000_0025_0000  ← direct map address
      │
      │   Currently (no demand paging of heap):
      │   callers receive the direct map address directly.
      │
      ▼
  Future VMA (reserved)

  FFFF_8080_0000_1000   (heap VMA region)
      │
      │   VMA region exists in the page-table tree but is
      │   NOT yet used for kmalloc() return values.
      │   Intended for future: map physical pages here and
      │   hand out VMA addresses instead of direct-map addresses.
      ▼
  (optional remapping / demand paging)
```

---

## 4. Per-CPU Memory

### PerCpu Slot (`kernel/src/percpu.rs`)

```
 ┌─────────────────────────────────────────────┐
 │ PerCpu (one per LAPIC ID, MAX_CPUS = 4)      │
 │                                              │
 │  apic_id: AtomicU32                          │
 │  online: AtomicBool                          │
 │  kernel_ready: AtomicBool                    │
 │  kernel_stack_top: AtomicU64                 │
 │  ticks: AtomicU64                            │
 │  current_task: AtomicUsize                   │
 │  current_vcpu: AtomicUsize                   │
 │  idle_vcpu_id: AtomicU32                     │
 │  task_count: AtomicUsize                     │
 │  ready_queue: ReadyQueue  (256-entry FIFO)   │
 │  self_ptr: AtomicU64                         │
 │  need_resched: AtomicBool                    │
 │  timer_fires: AtomicU64                      │
 │  pending_tlb_flush: AtomicU64                │
 └─────────────────────────────────────────────┘
```

GS base → PerCpu:
```
  GS base
    ↓
  PerCpu slot (per LAPIC ID)
    ↓
  %gs:offset → field access
```

GS base points to each CPU's slot. Accessed via `%gs:offset` instructions.

### Interrupt Stacks

Each CPU gets dedicated IST stacks in the kernel BSS (at
`HIGHER_HALF + phys_offset_of_ist_stack`):

| Stack | Size  | IST# | Used By                                |
|-------|-------|------|----------------------------------------|
| IST1  | 16 KB | 1    | Double fault (#DF, vector 8)          |
| IST2  | 16 KB | 2    | All IRQs (vectors 32–63), IPI (0x81)  |
| IST3  | 16 KB | 3    | NMI (vector 2)                        |

### AP Kernel Stacks

Each AP gets a dedicated kernel stack allocated from the buddy allocator:

- Size: 16 KB (4 pages) per AP — `AP_STACK_PAGES`
- APs begin execution with paging disabled and early identity mappings,
  so stacks ≤ 4 GB are identity-mapped by the pre-existing 2 MB huge pages
  and are covered by the boot-time identity map (PML4[0]).
- If > 4 GB, a separate identity + higher-half mapping is created.

### GDT Layout (per CPU)

```
 Null      [0x00]  selector 0x00
 Kernel CS [0x08]  selector 0x08  (ring 0 code, 64-bit)
 Kernel DS [0x10]  selector 0x10  (ring 0 data)
 User CS   [0x1B]  selector 0x18  (ring 3 code)
 User DS   [0x23]  selector 0x20  (ring 3 data)
 TSS low   [0x28]  selector 0x28  (TSS descriptor low)
 TSS high  [0x30]                 (TSS descriptor high)
```

---

## 5. Boot Chain Memory Flow

### Phase 1 — Chainloader (`chain/src/main.rs`)

```
 UEFI allocates:
   ┌──────────┐  0x5000         ← BOOT_INFO_HANDOFF_ADDR
   │ (8 bytes)│  stores pointer to BootInfo
   └──────────┘
   ┌──────────┐  (some UEFI-allocated addr)
   │ BootInfo │  struct (≈2 KB), dynamically allocated
   └──────────┘

 Stores: BOOT_INFO_HANDOFF_PTR → BootInfo addr
 Then:   Loads and jumps to Bootloader.efi
```

### Phase 2 — Bootloader (`boot/src/main.rs`)

```
 1. Reads BootInfo pointer from 0x5000
 2. Opens GOP → sets framebuffer → stores phys_addr in BootInfo
 3. Loads kernel ELF from ext4 into UEFI-allocated buffer
      ─ stores phys addr in BootInfo.kernel_image_addr
 4. Loads driver package from ext4 into UEFI-allocated buffer
      ─ stores phys addr in BootInfo.drivers_pkg_addr
 5. Captures RSDP from UEFI config table
 6. Enumerates APs via UEFI MP Services
 7. Collects usable memory regions from UEFI memory map
 8. Loads kernel ELF segments (parses ELF, copies .text/.rodata/.data/.bss
     to linker-specified addresses starting at 0x100000)
 9. Calls ExitBootServices → UEFI runtime disabled
    ⚠ After this point, only addresses already established by the bootloader remain valid;
    UEFI memory allocation/services become unavailable.
10. Jumps to kernel entry (_start) with:
     RDI = BootInfo physical address
```

### Phase 3 — Kernel Init (`kernel/src/main.rs`)

```
 _start(boot_info):
   │
   ├─ serial::init() + logger::init()
   │
   ├─ build_memory_layout(info)
   │    └─ extracts usable regions from BootInfo, excises kernel range
   │
   ├─ phys::init_from_regions(regions, boot_info_phys, excludes)
   │    └─ builds buddy free lists over all usable RAM
   │
   ├─ smp::init()
   │    └─ reserves 0x8000 page, copies trampoline binary
   │
   ├─ virt::init(regions, fb_phys, kernel_phys)
   │    ├─ allocates PML4, zeroes it
   │    ├─ maps all free regions in higher-half (2 MB + 4 KB)
   │    ├─ identity-maps 0..4 GiB (2 MiB huge pages)
   │    ├─ maps framebuffer in higher-half (4 KB pages)
   │    └─ loads CR3 → new page tables active
   │
   ├─ heap::init()
   │    └─ initialises 9 slab caches (32 B – 8 KB)
   │
   ├─ vma::init_kernel_vmas()
   │    └─ registers 0xFFFF_8080_0000_0000..+64MB as demand-paged
   │
   ├─ GDT + TSS + IDT loaded (IST stacks configured)
   ├─ LAPIC enabled + timer calibrated
   ├─ APs booted via INIT-SIPI-SIPI
   ├─ Drivers loaded via GDF (driver package segments mapped via per-vCPU PML4)
   └─ Idle loop
```

---

## 6. Driver Address Space

### Driver Package (`drivers.pkg`)

The drivers package is a custom container (LODAXPKG format, not an ELF), loaded by the
bootloader into a staging buffer. The package itself is never executed — it is purely a
container that stores multiple ELF binaries in a single contiguous blob:

```
 ┌──────────────────────────────┐
 │ DriverPkgHeader (12 bytes)   │
 │  ┌──────┬──────────────────┐ │
 │  │magic │ "LODAXPKG"      │ │
 │  │count │ N               │ │
 │  └──────┴──────────────────┘ │
 ├──────────────────────────────┤
 │ DriverPkgEntry × N (40 B ea) │
 │  name, class, elf_offset,    │
 │  elf_size                    │
 ├──────────────────────────────┤
 │ Driver ELF 0                 │
 ├──────────────────────────────┤
 │ Driver ELF 1                 │
 ├──────────────────────────────┤
 │ ...                          │
 └──────────────────────────────┘
```

### Driver Virtual Layout

Each driver runs as a **user-mode vCPU** (ring 3) with its own **forked PML4**. The PML4
is a deep copy of the kernel PML4 (`fork_pml4`), so every driver inherits:
- the identity map (PML4[0], see §2)
- all kernel higher-half mappings (PML4[256..511])

Driver ELF segments are mapped into the **user half** (PML4[0..255]) at the virtual
addresses specified by the ELF program headers:

```
  Driver PML4 (per-vCPU fork of kernel PML4)

  ┌───────────────────────────────────────┐  PML4[0]
  │ Identity map 0..4 GiB (COW, from      │
  │ kernel PML4 — never written by driver) │
  ├───────────────────────────────────────┤
  │ Driver ELF segments (user mode):      │
  │   .text       e.g. 0x0000_4000_0000   │
  │   .rodata     e.g. 0x0000_5000_0000   │
  │   .data       e.g. 0x0000_6000_0000   │
  │   .bss        e.g. 0x0000_7000_0000   │
  ├───────────────────────────────────────┤
  │ Driver stack (user mode):             │
  │   0x0000_7FFF_FFFF_0000 - size        │
  ├───────────────────────────────────────┤  PML4[255]
  ├═══════════════════════════════════════┤
  │ Kernel higher-half (U/S=0 —           │  PML4[256]
  │ inaccessible from Ring 3, via fork):  │
  │   Kernel .text / .rodata / .data      │
  │   Direct physical map                 │
  │   Kernel heap / VMA region            │
  │   MMIO (APIC, framebuffer)            │
  ├───────────────────────────────────────┤
  │   (unmapped)                          │
  └───────────────────────────────────────┘  PML4[511]
```

Key points:
- **Drivers are PIC** — they are relocatable ELF objects; the kernel honours their ELF
  `p_vaddr` fields when mapping.
- **Driver virtual base** is determined by the ELF entry-point address, not fixed by the
  kernel.
- **Drivers execute in ring 3** — all driver mappings carry the USER flag.
- **Drivers inherit kernel higher-half PTEs via `fork_pml4`, but those entries have
  U/S = 0 (supervisor-only).** Any Ring 3 access to the kernel half triggers a page
  fault — it is **not** accessible, not even as read-only.
- **Drivers share kernel mappings** — fork is a deep copy of page-table entries, but uses
  the same underlying physical pages (COW for user-half entries, shared for kernel-half).

### Driver Service Stacks

Drivers run as user-mode vCPUs with per-vCPU kernel stacks:

- Each gets a dedicated stack (referenced in `kernel/src/service.rs`)
- Stacks are mapped into the kernel PML4 so the kernel can context-switch
  to them

---

## 7. Complete Boot-Time Memory Snapshot

```
  Before ExitBootServices:

     ┌── UEFI reserved ──┐   0x000000 – 0x0FFFFF   UEFI firmware, ACPI, SMBIOS
     ├────────────────────┤
     │  0x5000            │   BootInfo pointer
     │  0x8000            │   (free before kernel reserves it)
     │  0x100000          │   (free before kernel loaded)
     │  ~0x20000000       │   (~511 MiB free)
     │  0x20000000        │   (free before driver package loaded)
     │  0xFEC0_0000       │   APIC MMIO
     │  0xFEE0_0000       │   LAPIC MMIO
     ├────────────────────┤
     │ UEFI Loader Data   │   kernel ELF staging buffer
     │                    │   driver package staging buffer
     │                    │   Kernel image loaded at 0x100000..
     └────────────────────┘

  After ExitBootServices + kernel init:

     ┌────────────────────┐   0x000000   null guard
     │  0x005000          │   BootInfo handoff
     │  0x008000          │   SIPI trampoline + mailbox
     │  0x010000          │   Kernel image (.text, .rodata, .data, .bss)
     │  ~0x100000..0x1FFFFFFF   │   (~511 MiB free)
     │  0x20000000        │   Driver package (code + data)
     ├────────────────────┤
     │  Buddy allocator   │   All other free physical pages
     │  managed pages     │
     │                    │   (includes HEAP allocations, AP stacks,
     │                    │    page tables, slab memory, DMA buffers)
     ├────────────────────┤
     │  0xFEC0_0000       │   IOAPIC MMIO
     │  0xFEE0_0000       │   LAPIC MMIO
     └────────────────────┘
```

---

## 8. Key Constants Reference

| Constant           | Value                        | Defined In                     |
|--------------------|------------------------------|--------------------------------|
| `TRAMPOLINE_PHYS`  | `0x8000`                     | `kernel/src/consts.rs`         |
| `LAPIC_PHYS`       | `0xFEE0_0000`                | `kernel/src/consts.rs`         |
| `IOAPIC_PHYS`      | `0xFEC0_0000`                | `kernel/src/consts.rs`         |
| `APIC_MMIO_BASE`   | `0xFEC0_0000`                | `kernel/src/consts.rs`         |
| `APIC_MMIO_SIZE`   | `0x40_0000` (4 MB)           | `kernel/src/consts.rs`         |
| `PAGE_SHIFT`       | `12`                         | `kernel/src/consts.rs`         |
| `PAGE_SIZE`        | `0x1000` (4 KB)              | `kernel/src/consts.rs`         |
| `KERNEL_STACK_SIZE`| `8192` (8 KB)                | `kernel/src/consts.rs`         |
| `AP_STACK_PAGES`   | `4` (16 KB)                  | `kernel/src/consts.rs`         |
| `HIGHER_HALF`      | `0xFFFF_8000_0000_0000`      | `kernel/src/mm/virt.rs`        |
| `heap_virt_base`   | `0xFFFF_8080_0000_0000`      | `kernel/src/mm/vma.rs`         |
| `heap_size`        | `0x400_0000` (64 MB)         | `kernel/src/mm/vma.rs`         |
| `MAX_ORDER`        | `10`                         | `kernel/src/mm/phys.rs`        |
| `MAX_CPUS`         | `4`                          | `system/src/lib.rs`            |
| `MAX_MEMORY_REGIONS`| `128`                       | `system/src/lib.rs`            |
| `BOOT_INFO_HANDOFF_ADDR`| `0x5000`                | `system/src/lib.rs`            |
| `__kernel_start`   | `0x100000`                   | `kernel/linker.ld`             |
| `DRIVERS_PKG_LOAD_ADDR`| `0x20000000` (physical)   | `drivers/linker.ld`            |

> **`DRIVERS_PKG_LOAD_ADDR` (0x2000_0000):** This is a **physical address** used as the
> load target during the staging phase (pre-ExitBootServices). Because the bootloader
> identity-maps 0..4 GiB via PML4[0] (see §2), the driver package data is also accessible
> at virtual address `0x2000_0000` during early boot. After `virt::init`, the higher-half
> direct map gives a second alias at `HIGHER_HALF + 0x2000_0000`.

---

## 9. Diagram Legend

```text
 ┌──────────┐  ──  Fixed/known address range
 │          │
 ├──────────┤  ──  Boundary between regions
 │          │
 └──────────┘
 ◄──       ──  Symbol/define reference
 ...       ──  Variable extent (size depends on system)
```
