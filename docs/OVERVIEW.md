# LodaxOS Microkernel Architecture

> A from-scratch x86-64 hobby OS written in Rust. UEFI-booted, no legacy BIOS support.

---

## 1. System Architecture Overview

LodaxOS is a microkernel operating system for x86-64. The kernel provides minimal services (scheduling, physical/virtual memory, interrupts, GDF IPC) while hardware and filesystem logic runs in **driver services** — user-space processes isolated by separate page tables.

**Design philosophy:**
- **Microkernel**: The kernel handles only scheduling, memory, and IPC. Drivers are isolated processes (separate PML4) communicating via GDF mailbox IPC.
- **Gang scheduling (SEDS)**: vCPUs are grouped into gangs; the scheduler picks the gang with the lowest vruntime and runs its vCPUs across idle cores.
- **Buddy allocator** for physical memory (order 0–10, max 4 MB blocks).
- **4-level page tables** with a `0xFFFF_8000_0000_0000` higher-half and identity-mapped low 4 GB.
- **No standard library** — every crate is `#![no_std]`.

---

## 2. Crate Dependency Graph

Workspace members (from `Cargo.toml:2`):

```
system ──→ (no deps — shared types only)
   ↑
   ├── chain ──→ uefi, log, lodaxos-system
   ├── boot  ──→ uefi, log, lodaxos-system
   ├── kernel ──→ lodaxos-system, (optional) acpi
   └── drivers ──→ lodaxos-system
```

| Crate | Path | Type | Description |
|-------|------|------|-------------|
| `lodaxos-system` | `system/` | lib (`#![no_std]`) | Shared types: `BootInfo`, `MemoryRegion`, `FramebufferInfo`, `DriverPkgHeader`, `DriverPkgEntry`, constants |
| `lodaxos-chain` | `chain/` | UEFI app (`x86_64-unknown-uefi`) | Chainloader: allocates `BootInfo`, reads `Bootloader.efi` from ESP, starts it |
| `lodaxos-boot` | `boot/` | UEFI app (`x86_64-unknown-uefi`) | Bootloader: reads ext4 partition, loads `kernel.elf` + `drivers.elf`, collects ACPI/MP tables, exits boot services, jumps to kernel |
| `lodaxos-kernel` | `kernel/` | freestanding ELF (`x86_64-unknown-none`, code-model=kernel) | The OS kernel: scheduling, memory, interrupts, GDF driver framework |
| `lodaxos-drivers` | `drivers/` | freestanding ELFs (same target as kernel) | Individual driver binaries, packaged into `drivers.elf` by `drivers/pkg.py` |

---

## 3. Boot Flow

### 3.1 Boot Sequence Diagram

```
UEFI Firmware
    │
    │ Reads GPT → ESP → EFI/BOOT/BOOTX64.EFI
    ▼
Chainloader (lodaxos-chain)
    │
    │ 1. Allocate BootInfo (Box<BootInfo>), store pointer at 0x5000
    │ 2. Collect UEFI memory map into BootInfo
    │ 3. Capture GOP framebuffer info
    │ 4. Read Bootloader.efi from ESP root
    │ 5. load_image() + start_image(Bootloader.efi)
    │ 6. Leak BootInfo + bootloader bytes (memory passed to next stage)
    ▼
Bootloader (lodaxos-boot)
    │
    │ 1. Read BootInfo pointer from 0x5000
    │ 2. Set GOP mode (prefer 1024×768, fallback highest)
    │ 3. Scan GPT for ext4 partition (GUID 0FC63DAF-...)
    │ 4. Parse ext4 superblock, block groups, root inode
    │ 5. Read kernel.elf + drivers.elf from ext4 root
    │ 6. Capture RSDP from UEFI config table → BootInfo.rsdp_addr
    │ 7. Enumerate APs via UEFI MP Services → BootInfo.ap_apic_ids
    │ 8. Collect final usable memory regions
    │ 9. Parse kernel ELF64 → copy PT_LOAD segments to physical pages
    │ 10. ExitBootServices(no map)
    │ 11. cli
    │ 12. RSP aligned, push fake return → mov rdi, boot_info_addr → jmp kernel
    ▼
Kernel (_start at kernel/src/main.rs:30)
    │
    │ Phase 1: Memory init
    │   ├── serial + logger init
    │   ├── Enable FPU/SSE/XSAVE
    │   ├── build_memory_layout() → excise kernel image from free regions
    │   ├── phys::init_from_regions() → buddy allocator
    │   ├── ACPI discovery (MADT parse)
    │   ├── arch::smp::init()
    │   ├── mm::virt::init() → 4-level paging (higher-half + identity map)
    │   ├── mm::heap::init() → slab allocator (32B–8KB caches)
    │   ├── mm::vma::init_kernel_vmas()
    │   ├── vcpu::init() → VCPU slab (128 slots)
    │   └── scheduler::init() → SEDS gang table
    │
    │ Phase 2: Hardware init
    │   ├── Mask 8259 PIC, cli
    │   ├── apic::init_mmio() → map LAPIC MMIO (UC, higher-half)
    │   ├── ioapic::init() → parse MADT IOAPIC entries
    │   ├── intr::init() → IOAPIC route setup
    │   ├── percpu → set BSP APIC ID, mark online
    │   ├── arch::gdt::init_for_slot() → GDT + TSS per CPU
    │   ├── arch::idt::init() → 256-vector IDT
    │   ├── percpu::install_gs_base() → GS base for percpu data
    │   ├── scheduler::init_idle_vcpu() → idle VCPU per CPU
    │   ├── gdf::init_from_package() → load drivers.elf, start driver services
    │   ├── intr::install_all_masked() → IOAPIC routes
    │   ├── apic::enable() → LAPIC SVN, mask LINT0/1
    │   ├── apic::calibrate_pit() → calibrate LAPIC timer against PIT
    │   ├── apic::configure_timer(16, 32, periodic)
    │   ├── pit_enable_periodic(100 Hz)
    │   ├── smp_boot_aps() → INIT-SIPI-SIPI for each AP
    │   ├── sti
    │   ├── Ext4 driver → READ_FILE (cmd=1) → GET_SIZE (cmd=2)
    │   ├── Framebuffer driver → FB_CMD_ACQUIRE → FB_CMD_DRAW_TEXT
    │   └── bsp_idle_loop() → hlt + periodic stats log
```

### 3.2 Memory Handoff Details

| Stage | Key Action | Addresses |
|-------|-----------|-----------|
| **Chainloader** | `Box::new(BootInfo)` via UEFI allocator → store 8-byte pointer at `0x5000` | `BOOT_INFO_HANDOFF_ADDR = 0x5000` (`system/src/lib.rs:28`) |
| **Bootloader** | Read pointer from `0x5000`, dereference `*mut BootInfo`, update fields, write back | Same address, `BootInfo` struct lives at dynamically allocated UEFI memory |
| **Kernel entry** | `_start(boot_info: *const BootInfo)` — RDI = boot_info_addr (`boot/src/main.rs:226`) | Physical address of BootInfo passed in RDI; kernel reads it, then marks BootInfo pages as reserved in buddy allocator (`kernel/src/mm/phys.rs:252-258`) |

---

## 4. Memory Layout Overview

### 4.1 Key Physical Addresses

| Address | Size | Stage | Purpose |
|---------|------|-------|---------|
| `0x0000_0000` – `0x0000_0FFF` | 4 KB | Boot | Page 0 — reserved (null-deref guard) |
| `0x0000_5000` – `0x0000_5FFF` | 4 KB | Chain/Boot/Kernel | BootInfo handoff — 8-byte pointer to dynamically allocated BootInfo (`system/src/lib.rs:28`) |
| `0x0000_8000` – `0x0000_8FFF` | 4 KB | Kernel | SIPI trampoline — real-mode stub for AP startup (`kernel/src/consts.rs:4`) |
| `0x0010_0000` | — | Kernel | Kernel base load address (`kernel/linker.ld:4`) |
| `0x2000_0000` | — | Drivers | Drivers ELF base load address (`drivers/linker.ld:4`) |
| `0xFEE0_0000` | 4 KB | Kernel | LAPIC MMIO — architecturally fixed (`kernel/src/consts.rs:7`) |
| `0xFEC0_0000` | 4 KB | Kernel | IOAPIC MMIO — architecturally fixed (`kernel/src/consts.rs:10`) |
| `0xFEC0_0000` – `0xFF00_0000` | 4 MB | Kernel | APIC MMIO region (LAPIC + IOAPIC, 2 MB aligned) (`kernel/src/consts.rs:13-14`) |

The kernel identities-maps the first 4 GB of physical memory using 2 MB huge pages (PML4[0] → PDP[0..3] → PD tables), providing direct physical access for early boot. All memory is also mapped in the higher-half at `virt = 0xFFFF_8000_0000_0000 + phys`.

### 4.2 Virtual Memory Layout

| Range | Type | Description |
|-------|------|-------------|
| `0x0000_0000_0000_0000` – `0x0000_7FFF_FFFF_FFFF` | User | Lower half — user-space / driver mappings |
| `0xFFFF_8000_0000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | Kernel | Higher half — kernel mappings (`kernel/src/mm/virt.rs:23`) |

Each driver service gets its own PML4 (forked from kernel PML4 via `fork_pml4`). The kernel higher-half entries (PML4 entries 256–511) are shared; user entries (0–255) are COW-forked so drivers cannot see each other's memory.

### 4.3 Stack Layout

| Stack | Size | Location |
|-------|------|----------|
| Kernel task stack | 8 KB (`KERNEL_STACK_SIZE = 8192`, `kernel/src/consts.rs:22`) | Higher-half, allocated per vCPU |
| AP kernel stacks | 16 KB each (`AP_STACK_PAGES = 4`, `kernel/src/consts.rs:25`) | Higher-half, allocated per AP |
| Driver stack | 16 KB (`SERVICE_STACK_SIZE = 16384`, `kernel/src/gdf.rs:29`) | User-space, at `0x0000_7FFF_FFFF_0000` – stack size |

---

## 5. Syscall ABI

### 5.1 Instruction and Registers

- **Instruction**: `syscall`
- **Syscall number**: `rax`
- **Arguments**: `rdi` (arg0), `rsi` (arg1), `rdx` (arg2), `r10` (arg3), `r8` (arg4), `r9` (arg5) — standard SystemV ABI
- **Return value**: `rax`
- **Clobbers**: `rcx` (set to return RIP), `r11` (set to RFLAGS)
- **Stack**: On entry, `rsp` is saved to `r10`; the kernel pushes a `TrapFrame` and swaps to the kernel stack (via TSS.IST or RSP0)

### 5.2 Syscall Table

Syscall handler and dispatch at `kernel/src/arch/idt.rs:691-853`.

| Nr | Name | Args | Return | Access | Description |
|----|------|------|--------|--------|-------------|
| 0 | `yield` | — | — | ALL | Yield the current vCPU via `syscall` re-entry (`scheduler.rs:128`) |
| 1 | `exit` | — | — | ALL | Halt (block) current vCPU |
| 2 | `get_vcpu_id` | — | VcpuId | ALL | Return current vCPU ID |
| 3 | `wake` | rdi=vcpu_id | — | ALL | Wake a halted vCPU |
| 4 | `get_ticks` | — | ticks | ALL | Return uptime tick count |
| 5 | `mmap` | rdi=hint, rsi=size | virt_addr │ u64::MAX | ALL | Allocate physical pages + map into caller's address space |
| 6 | `munmap` | rdi=addr, rsi=size | 0 │ u64::MAX | ALL | Unmap and free pages from caller's address space |
| 7 | `create_gang` | rdi=entry, rsi=n_vcpus | gang_id │ u64::MAX | ALL | Create a new vCPU gang at the given entry point |
| 10 | `mmap_phys` | rdi=phys, rsi=size | phys │ u64::MAX | HW | Map a physical MMIO region (UC) into driver address space |
| 11 | `register_intr` | rdi=vector, rsi=handler_vaddr | 0 │ u64::MAX | HW | Register IRQ handler vector (stub — not yet implemented) |
| 12 | `intr_ack` | rdi=vector | 0 │ u64::MAX | HW | Send LAPIC EOI |
| 13 | `dma_alloc` | rdi=size | phys │ u64::MAX | HW | Allocate DMA-able physical pages (zeroed) |
| 14 | `dma_free` | rdi=phys, rsi=size | 0 │ u64::MAX | HW | Free DMA-able physical pages |
| 15 | `pci_config` | rdi=bdf, rsi=offset, rdx=width, r10=value, r8=is_write | value │ u64::MAX | HW | PCI config space read/write via PIO (0xCF8/0xCFC) |
| 20 | `driver_recv` | rdi=buf_ptr | 0 │ u64::MAX | HW, AB | Read kernel→driver mailbox; writes `[cmd, arg0, arg1, arg2]` to `buf_ptr` |
| 21 | `driver_send` | rdi=result | 0 │ u64::MAX | HW, AB | Write driver→kernel response (sets mailbox to `MAILBOX_IDLE`) |
| 22 | `driver_recv_block` | rdi=buf_ptr | 0 │ u64::MAX | HW, AB | Block until a message arrives; message written directly to `buf_ptr` by `send_cmd` |
| 30 | `gdf_register` | rdi=name_ptr, rsi=name_len | 0 │ u64::MAX | HW, AB | Register this vCPU as a named GDF driver |
| 31 | `driver_call` | rdi=name_ptr, rsi=name_len, rdx=cmd, r10=arg0, r8=arg1, r9=arg2 | result │ u64::MAX | HW, AB | Send command to another driver and wait for response |

**Access key**: ALL = Normal, HardwareDriver, AbstractionDriver. HW = HardwareDriver only. HW, AB = HardwareDriver + AbstractionDriver.

### 5.3 Yield Implementation

The `yield` syscall is a special case in the scheduler (`kernel/src/scheduler.rs:128`):

```rust
pub fn yield_now() {
    unsafe { asm!("syscall", in("rax") 0u64, lateout("rcx") _, lateout("r11") _) };
}
```

---

## 6. Driver Framework (GDF)

### 6.1 Overview

GDF (**Generic Driver Framework**) is a service-oriented driver model. Drivers are freestanding ELF binaries loaded from the `drivers.elf` package file. Each driver runs as a **service** with its own:
- **VCPU** in the SEDS scheduler
- **PML4** (forked from kernel PML4 via `fork_pml4` :: `kernel/src/mm/virt.rs:609`)
- **Kernel stack** for syscall/interrupt handling (3 pages: guard + 2 × 4 KB stack)

### 6.2 Driver Package Format

The `drivers.elf` file on disk is **not** an ELF — it is a custom package format defined in `system/src/lib.rs:74-104`.

```
Offset  │ Content
────────┼─────────────────────────────────────────────
 0      │ DriverPkgHeader.magic    [8 bytes] = b"LODAXPKG"
 8      │ DriverPkgHeader.count    [4 bytes, u32]
12      │ DriverPkgEntry[0].name   [32 bytes]
44      │ DriverPkgEntry[0].class  [4 bytes, u32]  — 0=Hardware, 1=Abstraction
48      │ DriverPkgEntry[0].elf_offset [4 bytes, u32]
52      │ DriverPkgEntry[0].elf_size   [4 bytes, u32]
56      │ DriverPkgEntry[1] ...
        │ ...
12+N*40 │ Driver ELF data 0
        │ Driver ELF data 1
        │ ...
```

**Types** (`system/src/lib.rs`):

```rust
#[repr(C)]
pub struct DriverPkgHeader {          // Size: 12 bytes
    pub magic: [u8; 8],               // "LODAXPKG"
    pub count: u32,
}

#[repr(C)]
pub struct DriverPkgEntry {           // Size: 40 bytes
    pub name: [u8; 32],               // Null-terminated driver name
    pub class: u32,                   // 0 = Hardware, 1 = Abstraction
    pub elf_offset: u32,             // Byte offset from end of manifest
    pub elf_size: u32,               // Size of the driver ELF in bytes
}
```

Max entries: `MAX_DRIVER_PKG_ENTRIES = 32` (`system/src/lib.rs:107`).

### 6.3 Kernel–Driver IPC: Mailboxes

Communication uses a shared **Mailbox** structure (`kernel/src/gdf.rs:48-57`):

```rust
#[repr(C)]
pub struct Mailbox {                  // Size: 40 bytes
    pub cmd: u32,                     // Command number
    pub flags: u32,                   // MAILBOX_IDLE=0, MAILBOX_PENDING=1, MAILBOX_RESPONSE=2
    pub arg0: u64,                    // Argument 0 (e.g., physical address)
    pub arg1: u64,                    // Argument 1 (e.g., size)
    pub arg2: u64,                    // Argument 2 (e.g., packed geometry)
    pub result: u64,                  // Return value from driver
}
```

**IPC flow (kernel → driver)**:

1. Kernel calls `gdf::send_cmd(name, cmd, arg0, arg1, arg2)` which sets `mailbox.flags = PENDING` and wakes the driver's VCPU.
2. Driver calls `sys_driver_recv` (nr=20) or `sys_driver_recv_block` (nr=22) to read `[cmd, arg0, arg1, arg2]` into a user buffer.
3. Driver processes the command and calls `sys_driver_send` (nr=21) with a result value.
4. Kernel calls `gdf::send_response()` which sets `mailbox.flags = IDLE` and stores the result.
5. If a caller was blocked in `driver_call`, it is woken and `rax` is set to the result.

**IPC flow (driver → driver)**:

A driver can call another driver via `sys_driver_call` (nr=31), which invokes `gdf::driver_call()`. This sends a mailbox command to the target driver and polls (with yield) for the response.

### 6.4 Driver Lifecycle

1. **Loading**: `gdf::init_from_package()` (`kernel/src/gdf.rs:377`) parses the package, creates a service + VCPU for each driver, loads the ELF via `exec::load_elf()` into the forked PML4, and pushes the VCPU onto the ready queue.
2. **Registration**: Each driver calls `sys_gdf_register` (nr=30) to register its name in the GDF driver table (max 16 drivers, `kernel/src/gdf.rs:28`).
3. **Crash handling**: `gdf::handle_crash()` (`kernel/src/gdf.rs:532`) checks the service's `RestartPolicy`:
   - `Always` → always restart
   - `OnFailure(n)` → restart up to n times
   - `Never` → stop permanently
4. **Cleanup**: MMIO mappings, DMA pages, and IRQ vectors tracked in `ServiceResources` (`kernel/src/service.rs:25-33`) are freed on stop/crash.

### 6.5 Built-in Drivers

Drivers are built as individual ELF binaries in the `drivers/` crate and packaged by `drivers/pkg.py`:

| Driver | Binary | Class | Description |
|--------|--------|-------|-------------|
| framebuffer | `framebuffer` | Hardware (0) | GOP framebuffer compositor |
| ahci | `ahci` | Hardware (0) | AHCI SATA controller |
| ext4 | `ext4` | Abstraction (1) | ext4 filesystem |
| ide | `ide` | Hardware (0) | Legacy IDE controller |

### 6.6 Framebuffer Commands

Defined in `system/src/lib.rs:7-17`:

| Cmd | Value | Description |
|-----|-------|-------------|
| `FB_CMD_ACQUIRE` | 0xFF | Acquire framebuffer (args: phys_addr, size, packed geometry) |
| `FB_CMD_SHOW_TEXT` | 1 | Show text string |
| `FB_CMD_CLEAR` | 2 | Clear framebuffer |
| `FB_CMD_SET_PIXEL` | 3 | Set a single pixel |
| `FB_CMD_FILL_RECT` | 4 | Fill a rectangle |
| `FB_CMD_DRAW_TEXT` | 5 | Draw text from buffer (args: phys_addr, size) |
| `FB_CMD_SET_FG` | 6 | Set foreground color |
| `FB_CMD_SET_BG` | 7 | Set background color |
| `FB_CMD_SCROLL` | 8 | Scroll framebuffer |
| `FB_CMD_GET_INFO` | 9 | Get framebuffer info |
| `FB_CMD_PRESENT` | 10 | Present / flip buffers |

---

## 7. Key Data Types

### 7.1 `BootInfo` (`system/src/lib.rs:33-62`)

```rust
#[repr(C)]
pub struct BootInfo {                   // Total size: 128*16 + 32 + 8*5 + 4*5 + 4*4 = ~2384 bytes
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],   // 128 * 16 = 2048 bytes
    pub memory_region_count: usize,                           // 8 bytes
    pub framebuffer: FramebufferInfo,                         // 32 bytes
    pub partition_zero_lba: u64,                              // 8 bytes
    pub partition_zero_size: u64,                             // 8 bytes
    pub kernel_image_addr: u64,                               // 8 bytes
    pub kernel_image_size: u64,                               // 8 bytes
    pub drivers_elf_addr: u64,                                // 8 bytes
    pub drivers_elf_size: u64,                                // 8 bytes
    pub rsdp_addr: u64,                                       // 8 bytes
    pub madt_addr: u64,                                       // 8 bytes
    pub max_cpus: u32,                                        // 4 bytes
    pub bsp_apic_id: u32,                                     // 4 bytes
    pub ap_count: u32,                                        // 4 bytes
    pub ap_apic_ids: [u32; MAX_CPUS],                         // 4 * 4 = 16 bytes
}
```

`MAX_MEMORY_REGIONS = 128` (`system/src/lib.rs:3`), `MAX_CPUS = 4` (`system/src/lib.rs:4`).

### 7.2 `MemoryRegion` (`system/src/lib.rs:66-71`)

```rust
#[repr(C)]
pub struct MemoryRegion {                // Size: 16 bytes
    pub phys_start: u64,
    pub size: u64,
}
```

### 7.3 `FramebufferInfo` (`system/src/lib.rs:109-118`)

```rust
#[repr(C)]
pub struct FramebufferInfo {             // Size: 32 + 4 + 3*4 = 40 bytes (with padding)
    pub phys_addr: u64,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub bytes_per_pixel: usize,
    pub is_bgr: bool,
}
```

### 7.4 `Vcpu` (`kernel/src/vcpu.rs:34-46`)

```rust
#[derive(Debug, Clone)]
pub struct Vcpu {
    pub id: VcpuId,                      // u32
    pub gang_id: u32,
    pub vcpu_type: VcpuType,             // Normal | HardwareDriver | AbstractionDriver | Idle
    pub state: VcpuState,                // Ready | Running | Halted | Blocked | Idle
    pub affinity: u64,                   // CPU affinity bitmap
    pub saved_frame: TrapFrame,          // Saved register state
    pub kernel_stack_top: u64,           // Kernel stack pointer for syscall/irq
    pub pml4: u64,                       // Physical address of this VCPU's PML4
    pub vruntime: u64,                   // Per-VCPU vruntime for scheduling
    pub fpu_state: FpuState,             // 512-byte FXSAVE/FXRSTOR area
}
```

Max VCPUs: `MAX_VCPUS = 128` (`kernel/src/vcpu.rs:48`).

### 7.5 `Gang` (`kernel/src/scheduler.rs:34-41`)

```rust
#[derive(Debug)]
pub struct Gang {
    pub id: GangId,                      // u32
    pub vcpu_ids: [Option<VcpuId>; MAX_VCPUS_PER_GANG],  // 8 slots
    pub vcpu_count: u32,
    pub vruntime: u64,
    pub running_count: u32,
    pub state: GangState,                // Active | Halted
}
```

Max gangs: `MAX_GANGS = 32`, max VCPUs per gang: `MAX_VCPUS_PER_GANG = 8` (`kernel/src/scheduler.rs:20-21`). Gang 0 is reserved for `GANG_UNSCHEDULED` (idle VCPUs).

### 7.6 `Service` (`kernel/src/service.rs:48-58`)

```rust
#[derive(Debug, Clone, Copy)]
pub struct Service {
    pub id: u32,
    pub name: [u8; 32],
    pub state: ServiceState,              // Loaded | Running | Crashed | Restarting | Stopped
    pub vcpu_id: VcpuId,
    pub pml4: u64,
    pub restart_policy: RestartPolicy,    // Always | OnFailure(u32) | Never
    pub restart_count: u32,
}
```

Max services: `MAX_SERVICES = 32` (`kernel/src/service.rs:4`). Default restart policy: `OnFailure(3)`.

### 7.7 `ServiceResources` (`kernel/src/service.rs:25-33`)

```rust
#[derive(Debug, Clone, Copy)]
pub struct ServiceResources {
    pub mmio: [(u64, u64, u64); MAX_MMIO],   // 16 entries: (phys, virt, pages)
    pub mmio_count: usize,
    pub irq: [u8; MAX_IRQ],                  // 8 entries: vector numbers
    pub irq_count: usize,
    pub dma: [(u64, u64); MAX_DMA],          // 8 entries: (phys, pages)
    pub dma_count: usize,
}
```

`MAX_MMIO = 16`, `MAX_IRQ = 8`, `MAX_DMA = 8` (`kernel/src/service.rs:5-7`).

### 7.8 `TrapFrame` (implicit, `kernel/src/arch/idt.rs:1` region)

The trap frame is defined by the `push` order in `syscall_entry` (`kernel/src/arch/idt.rs:741-789`), which pushes a layout compatible with `iretq`:

```rust
// Fields in push order (stack grows down):
struct TrapFrame {                        // Total: 15*8 + 2*8 + 5*8 = 176 bytes
    // Pushed by syscall_entry (in order):
    ss: u64,       // 0x10
    rsp: u64,      // original RSP (saved as R10)
    rflags: u64,   // from R11
    cs: u64,       // 0x08
    rip: u64,      // return address (from RCX)
    // Pushed manually:
    error_code: u64,
    vector: u64,   // 0x80
    // Callee-saved + arguments:
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}
```

### 7.9 `GangTable` / `Scheduler` (`kernel/src/scheduler.rs:43-53`)

```rust
struct GangTable {
    gangs: [Option<Gang>; MAX_GANGS],     // 32 gang slots
    count: usize,
    initialized: bool,
}
```

Protected by `IrqSaveSpinLock`. The SEDS scheduler:
- On each timer tick (vector 32), `schedule()` is called.
- The gang with the lowest `vruntime` that has available VCPUs is selected.
- `vruntime` is incremented by `VRUNTIME_TICK * 100 / vcpu_count` per tick (`VRUNTIME_TICK = 20`, `kernel/src/scheduler.rs:14`).
- Free cores (running idle VCPU) are counted and the selected gang's VCPUs are distributed.

---

## 8. Physical Memory Allocator

Uses a **buddy allocator** defined in `kernel/src/mm/phys.rs`.

| Constant | Value | Description |
|----------|-------|-------------|
| `MAX_ORDER` | 10 | Largest allocation order (2^10 = 1024 pages = 4 MB) |
| `PAGE_SIZE` | 0x1000 (4096) | Page size in bytes |
| `BOOTINFO_HANDOFF_PAGE` | 0x5000 | Reserved page for BootInfo pointer |

**Free lists**: Per-order singly-linked list of `FreeBlock` nodes stored inline in free pages.

**Allocation**: `alloc_pages(count)` → rounds up to next power of 2, allocates from that order, returns excess to lower-order free lists.

**Coalescing**: On `free_order`, the buddy (addr XOR `block_size(order)`) is checked. If free, the pair is merged and recursively coalesced at `order + 1`.

**Reserved ranges**: Page 0, the BootInfo handoff page, BootInfo struct pages, framebuffer pages, kernel staging image, drivers ELF staging, and the APIC MMIO region (0xFEC0_0000, 4 MB) are excluded from the free lists.

---

## 9. Virtual Memory Manager

Defined in `kernel/src/mm/virt.rs`.

| Constant | Value | Description |
|----------|-------|-------------|
| `HIGHER_HALF` | `0xFFFF_8000_0000_0000` | Base of kernel higher-half mappings |
| `PRESENT` | bit 0 | Page present |
| `WRITABLE` | bit 1 | Page writable |
| `USER` | bit 2 | User-accessible |
| `CACHE_DISABLE` | bit 4 (PCD) | Uncacheable (for MMIO) |
| `NO_EXECUTE` | bit 63 | Execute disable |
| `COW` | bit 11 | Software-defined Copy-On-Write flag |

**Identity map**: The first 4 GB of physical memory are identity-mapped via 2 MB huge pages (4 PDP entries × 512 PD entries). The LAPIC page at 0xFEE0_0000 is marked with PCD (cache-disable).

**fork_pml4**: Deep-copies a PML4 hierarchy. User entries (PML4 0–255) are COW-forked (writable pages become read-only with the COW bit set). Kernel entries (256–511) are shared.

**TLB shootdown**: When `unmap` or `map_page_explicit` modifies a present PTE, a per-CPU IPI (vector `0x81`) + `invlpg` is broadcast. If interrupts are disabled, pending TLB flush addresses are stored in per-CPU memory and processed on the next interrupt.

---

## 10. Scheduler (SEDS)

**S**cheduler for **E**nforced **D**river **S**eparation — a gang-based, vruntime-driven scheduler.

### 10.1 Key Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `VRUNTIME_TICK` | 20 | Base vruntime increment per timer tick |
| `VRUNTIME_BIAS` | 8 | Bias subtracted from new gang vruntime |
| `MAX_GANGS` | 32 | Maximum number of gangs |
| `MAX_VCPUS_PER_GANG` | 8 | Maximum VCPUs per gang |

### 10.2 Schedule Algorithm

Performed on every LAPIC timer tick (vector 32, 1 ms interval):

1. **Save** current VCPU state (registers, FPU, vruntime update).
2. **Select gang** with lowest `vruntime` that has `vcpu_count > running_count`.
3. **Count free cores** (CPUs currently running the idle VCPU).
4. **Select VCPU** from the chosen gang with lowest per-VCPU vruntime.
5. **Load** next VCPU context (switch PML4 if needed, restore FPU state, set TSS RSP0).
6. **return** via `iretq`.

### 10.3 Load Balancing

`steal_task()` on idle CPUs: finds the most loaded online CPU and moves one VCPU from its runqueue to the idle CPU.

---

## 11. Interrupt System

### 11.1 IDT Layout

256-vector IDT loaded at `kernel/src/arch/idt.rs`. Vector assignments:

| Vector(s) | Handler | Description |
|-----------|---------|-------------|
| 0–19 | `exception_handler` | CPU exceptions (divide-by-zero, page fault, GPF, etc.) |
| 32 | `irq_handler` | LAPIC timer (scheduler tick) |
| 33 | `irq_handler` | PIT channel 0 (100 Hz, for testing) |
| 0x80 | `syscall_entry` | Software syscall entry (via `syscall` instruction) |
| 0x81 | `ipi_handler` | Cross-CPU IPI (TLB shootdown, wake) |
| 0xFF | spurious | LAPIC spurious interrupt vector |
| Others | `irq_handler` | IOAPIC interrupt routes |

### 11.2 Exception Handling

Exceptions (vectors 0–19) jump to `exception_handler` which dumps the TrapFrame and calls `halt_loop`.

---

## 12. APIC

### 12.1 LAPIC (Local APIC)

Defined in `kernel/src/arch/apic.rs`.

| Register | Offset | Description |
|----------|--------|-------------|
| `APIC_ID` | 0x020 | LAPIC ID (high byte) |
| `APIC_LVR` | 0x030 | Local Version Register |
| `APIC_TPR` | 0x080 | Task Priority Register |
| `APIC_EOI` | 0x0B0 | End-Of-Interrupt |
| `APIC_SVR` | 0x0F0 | Spurious Vector Register |
| `APIC_ICR_LOW` | 0x300 | Interrupt Command Register (low) |
| `APIC_ICR_HIGH` | 0x310 | Interrupt Command Register (high) |
| `APIC_LVT_TIMER` | 0x320 | LVT Timer Register |
| `APIC_LVT_LINT0` | 0x350 | LVT LINT0 Register |
| `APIC_LVT_LINT1` | 0x360 | LVT LINT1 Register |
| `APIC_LVT_ERROR` | 0x370 | LVT Error Register |
| `APIC_TDCR` | 0x3E0 | Timer Divide Configuration |
| `APIC_TICR` | 0x380 | Timer Initial Count |
| `APIC_CCR` | 0x390 | Current Count Register |

**MMIO base**: Read from `IA32_APIC_BASE` MSR (0x1B), typically `0xFEE0_0000`. Mapped to higher-half with UC (cache-disable) flag.

**Calibration**: LAPIC timer is calibrated against PIT channel 0 in Mode 0 (one-shot, no reload) over a 20 ms window. Result stored in `TICKS_PER_MS`.

**Timer**: Configured at vector 32, divisor 16, periodic mode, 1 ms interval.

### 12.2 ICR Delivery Modes

| Mode | Value | Description |
|------|-------|-------------|
| `ICR_FIXED` | 0 | Deliver vector to target |
| `ICR_INIT` | 5 << 8 | INIT IPI |
| `ICR_STARTUP` | 6 << 8 | Startup IPI (SIPI) |

### 12.3 IOAPIC

Parsed from the MADT (Multiple APIC Description Table). Each IOAPIC entry has:
- `ioapic_id: u32`
- `addr: u64` (MMIO base, typically 0xFEC0_0000)
- `gsi_base: u32` (Global System Interrupt base)

---

## 13. SMP Boot

### 13.1 AP Enumeration

APs are enumerated by the bootloader using UEFI MP Services (`boot/src/mp.rs:18`). The BSP LAPIC ID and up to 3 AP LAPIC IDs are stored in `BootInfo`.

### 13.2 AP Startup (`kernel/src/ap_start.rs`)

The kernel brings APs up via the standard Intel INIT-SIPI-SIPI sequence:

1. Copy trampoline code to `0x8000` (SIPI vector 0x08).
2. For each AP: send INIT IPI → wait → send SIPI to vector 0x08 → wait → send SIPI again → wait.
3. The AP runs the real-mode trampoline at `0x8000`, enters protected mode → long mode, loads its own GDT/IDT, enables its LAPIC timer, marks itself online, and enters its idle loop.

Trampoline: `kernel/src/arch/smp_trampoline.S` (assembled by NASM → `smp_trampoline.bin`).

---

## 14. ACPI

### 14.1 RSDP (Root System Description Pointer)

Captured by the bootloader from UEFI config tables before `ExitBootServices`. Matched against `ACPI2_GUID` (for XSDT) or `ACPI_GUID` (for RSDT). Stored in `BootInfo.rsdp_addr`.

### 14.2 MADT (Multiple APIC Description Table)

Parsed by the kernel from the RSDP/XSDT chain. Provides:
- LAPIC entries (CPU count, LAPIC IDs)
- IOAPIC entries (count, base addresses, GSI bases)
- ISO (Interrupt Source Override) entries

---

## 15. Build & Deploy

### 15.1 Toolchain

From `rust-toolchain.toml`:
```
channel = "nightly"
targets = ["x86_64-unknown-uefi"]
```

The kernel and drivers use a custom target spec (`kernel/target.json`, `drivers/target.json`):
```json
{
    "llvm-target": "x86_64-unknown-none",
    "code-model": "kernel",
    "relocation-model": "static",
    "linker-flavor": "ld.lld",
    "linker": "rust-lld",
    "pre-link-args": {
        "ld.lld": ["-Tkernel/linker.ld", "--gc-sections"]
    }
}
```

### 15.2 Build Script (`build.bat`)

1. **`cargo build -p lodaxos-system`** — shared types crate (no target spec needed)
2. **`nasm`** — assemble `smp_trampoline.S` → `smp_trampoline.bin`
3. **`cargo build -p lodaxos-kernel --target kernel/target.json -Zbuild-std=core,alloc`** — kernel ELF
4. **`cargo build -p lodaxos-boot --target x86_64-unknown-uefi`** — bootloader UEFI app
5. **`cargo build -p lodaxos-chain --target x86_64-unknown-uefi`** — chainloader UEFI app
6. **`python genfont.py`** — generate font bitmap
7. **Build each driver** individually (`framebuffer`, `ahci`, `ext4`, `ide`) using `drivers/target.json`
8. **`python drivers/pkg.py drivers.elf`** — package driver ELFs into the `drivers.elf` package
9. **Copy** `kernel` → `kernel.elf`

### 15.3 Disk Image (`create_disk.py`)

Creates a 600 MB disk image (GPT):

| Partition | Type | Size | LBA Range | Contents |
|-----------|------|------|-----------|----------|
| Partition 0 | ext4 (GUID `0FC63DAF-...`) | 512 MB | 2048–1050623 | `kernel.elf`, `Bootloader.efi`, `drivers.elf`, `file.txt` |
| Partition 1 | ESP FAT32 (GUID `C12A7328-...`) | 64 MB | 1050624–1181695 | `EFI/BOOT/BOOTX64.EFI` (chainloader), `Bootloader.efi`, `kernel.elf` |

Supports incremental updates via hash caching (`.disk_cache.json`):
- Ext4 partition updated via WSL `debugfs -w`
- ESP updated via WSL `mcopy -o`
- Partitions spliced into `disk.img` at the correct offsets

### 15.4 Run Script (`run.bat`)

```
qemu-system-x86_64.exe
    -drive if=pflash,format=raw,readonly=on,file=edk2-x86_64-code.fd
    -drive file=disk.img,format=raw,if=ide
    -serial stdio
    -accel whpx
    -machine q35
    -m 128M
    -smp 4
    -vga std
```

Uses UEFI (edk2), WHPX acceleration, Q35 chipset, 4 CPUs, 128 MB RAM, serial console output.

---

## 16. Constants Reference

| Constant | File | Value |
|----------|------|-------|
| `MAX_MEMORY_REGIONS` | `system/src/lib.rs:3` | 128 |
| `MAX_CPUS` | `system/src/lib.rs:4` | 4 |
| `BOOT_INFO_HANDOFF_ADDR` | `system/src/lib.rs:28` | `0x5000` |
| `DRIVER_PKG_MAGIC` | `system/src/lib.rs:85` | `b"LODAXPKG"` |
| `MAX_DRIVER_PKG_ENTRIES` | `system/src/lib.rs:107` | 32 |
| `TRAMPOLINE_PHYS` | `kernel/src/consts.rs:4` | `0x8000` |
| `LAPIC_PHYS` | `kernel/src/consts.rs:7` | `0xFEE0_0000` |
| `IOAPIC_PHYS` | `kernel/src/consts.rs:10` | `0xFEC0_0000` |
| `APIC_MMIO_BASE` | `kernel/src/consts.rs:13` | `0xFEC0_0000` |
| `APIC_MMIO_SIZE` | `kernel/src/consts.rs:14` | `0x40_0000` (4 MB) |
| `PAGE_SIZE` | `kernel/src/consts.rs:19` | `0x1000` (4096) |
| `KERNEL_STACK_SIZE` | `kernel/src/consts.rs:22` | 8192 |
| `AP_STACK_PAGES` | `kernel/src/consts.rs:25` | 4 |
| `IDLE_TASK_ID` | `kernel/src/consts.rs:28` | 0 |
| `HIGHER_HALF` | `kernel/src/mm/virt.rs:23` | `0xFFFF_8000_0000_0000` |
| `MAX_ORDER` | `kernel/src/mm/phys.rs:10` | 10 |
| `MAX_VCPUS` | `kernel/src/vcpu.rs:48` | 128 |
| `MAX_GANGS` | `kernel/src/scheduler.rs:20` | 32 |
| `MAX_VCPUS_PER_GANG` | `kernel/src/scheduler.rs:21` | 8 |
| `MAX_DRIVERS` | `kernel/src/gdf.rs:28` | 16 |
| `SERVICE_STACK_SIZE` | `kernel/src/gdf.rs:29` | 16384 |
| `MAX_RESTARTS` | `kernel/src/gdf.rs:30` | 3 |
| `MAX_SERVICES` | `kernel/src/service.rs:4` | 32 |
| `IPI_VECTOR` | `kernel/src/arch/idt.rs:1148` | `0x81` |
