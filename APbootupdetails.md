# LodaxOS AP (Application Processor) Boot — Complete Deep Dive

## Table of Contents
1. [Overview & Two-Path Architecture](#1-overview--two-path-architecture)
2. [Key Data Structures](#2-key-data-structures)
3. [Phase 0: Platform Discovery (Bootloader)](#3-phase-0-platform-discovery-bootloader)
4. [Phase 1: UEFI MP Trampoline — Phase 1 (Spin)](#4-phase-1-uefi-mp-trampoline--phase-1-spin)
5. [Phase 2: Kernel AP Resource Reservation](#5-phase-2-kernel-ap-resource-reservation)
6. [Phase 3: BSP Pre-Release Per-CPU Setup](#6-phase-3-bsp-pre-release-per-cpu-setup)
7. [Phase 4: release_aps() in Detail](#7-phase-4-release_aps-in-detail)
8. [Phase 5: UEFI MP Trampoline — Phase 2 (Full Switch)](#8-phase-5-uefi-mp-trampoline--phase-2-full-switch)
9. [Phase 6: ap_entry() — Kernel AP Init](#9-phase-6-ap_entry--kernel-ap-init)
10. [Phase 7: AP Scheduling Loop](#10-phase-7-ap-scheduling-loop)
11. [Phase 8: INIT-SIPI-SIPI Path (Preserved, Disabled)](#11-phase-8-init-sipi-sipi-path-preserved-disabled)
12. [Per-CPU Infrastructure Deep Dive](#12-per-cpu-infrastructure-deep-dive)
13. [WHPX-Specific Workarounds](#13-whpx-specific-workarounds)
14. [Synchronization & Memory Ordering](#14-synchronization--memory-ordering)
15. [Scheduler Integration](#15-scheduler-integration)
16. [Common Failure Modes & Debugging](#16-common-failure-modes--debugging)
17. [Memory Map for APs](#17-memory-map-for-aps)
18. [Register State at Each Transition](#18-register-state-at-each-transition)
19. [Timing Expectations & Timeouts](#19-timing-expectations--timeouts)
20. [Complete AP Lifecycle Timeline](#20-complete-ap-lifecycle-timeline)

---

## 1. Overview & Two-Path Architecture

### 1.1 What is an AP?

An **Application Processor** (AP) is any CPU core in an x86 SMP system other than the **Bootstrap Processor** (BSP). The BSP is the CPU that boots first from reset, runs the UEFI firmware, the chainloader, the bootloader, and enters the kernel first. APs are brought up later by the BSP.

### 1.2 Two AP Startup Paths

LodaxOS supports **two** mechanisms for AP startup, selectable at compile/logic time:

| Path | Status | Hypervisor/Platform | Mechanism |
|------|--------|-------------------|-----------|
| **UEFI MP Services** | **ACTIVE (default)** | WHPX (Windows Hypervisor Platform) | UEFI `MpServices` protocol dispatches a trampoline to each AP. AP spins on `go` in ApArg; BSP writes fields + `go=1` from the kernel. |
| **INIT-SIPI-SIPI** | **Disabled** (preserved code) | TCG, KVM, bare metal | BSP kernel sends INIT IPI → 10 ms wait → two SIPIs to each AP. AP starts at real-mode trampoline at 0x8000, transitions to long mode, reaches same ap_entry. |

The INIT-SIPI-SIPI path is **disabled** because WHPX does not support the INIT-SIPI-SIPI sequence (it causes QEMU error "Unexpected VP exit code 4" during the post-INIT PIT busy-wait). The UEFI MP Services path is used instead.

### 1.3 High-Level Flow

```
UEFI Firmware Boot
  │
  ├── Chainloader runs on BSP
  ├── Bootloader runs on BSP
  │   └── bring_up_aps() via UEFI MP Services
  │       ├── For each AP:
  │       │   ├── Allocate ApArg + kernel stack (UEFI LOADER_DATA)
  │       │   ├── mp.startup_this_ap(ap_trampoline, arg)
  │       │   │   └── AP enters ap_trampoline():
  │       │   │       ├── Phase 1: set ready=1, spin on go
  │       │   │       └── [AP NOW SPINNING in trampoline]
  │       │   └── BSP records ap_arg_phys, ap_apic_ids
  │       └── (BSP continues boot)
  │
  └── Kernel starts on BSP
      ├── Reserve AP pages (before page tables)
      ├── ... (full kernel init) ...
      ├── release_aps(): write target_* + go=1 into each ApArg
      │   └── AP wakes from go spin, enters Phase 2 of trampoline:
      │       ├── Switch RSP to kernel stack
      │       ├── Switch CR3 to kernel PML4
      │       ├── Load kernel GDT + IDT
      │       └── jmp ap_entry()
      ├── release_all_aps(): set kernel_ready=true globally
      ├── sti (BSP enables interrupts)
      │
      └── AP enters ap_entry():
          ├── "AP ENTRY REACHED" to COM1
          ├── Read LAPIC ID from MMIO
          ├── FPU/SSE init per-CPU
          ├── mark_online()
          ├── install_gs_base() (per-CPU TLS)
          ├── Enable LAPIC timer (1 ms periodic)
          ├── wait_for_kernel_ready()
          ├── init_idle_task()
          ├── ltr (load per-CPU TSS)
          ├── sti
          └── ap_sched_loop() — never returns
```

---

## 2. Key Data Structures

### 2.1 ApArg (`system/src/lib.rs:74-85`)

The `ApArg` struct is the **cross-firmware/kernel handoff block** for each AP. It is allocated by the bootloader in UEFI LOADER_DATA memory (survives ExitBootServices). Both the trampoline (naked asm in bootloader space) and the kernel (BSP writes, AP reads) access this struct.

```
Offset  Field                Size  Written by     Read by            Purpose
------  -------------------  ----  -------------  -----------------  -------------------------------
0x00    target_pml4_phys     8 B   kernel BSP     trampoline (Phase2)  Physical address of kernel PML4
0x08    target_gdt_ptr       8 B   kernel BSP     trampoline (Phase2)  Pointer to GDT descriptor (16 B)
0x10    target_idt_ptr       8 B   kernel BSP     trampoline (Phase2)  Pointer to IDT descriptor (16 B)
0x18    target_kernel_stack  8 B   bootloader     trampoline (Phase2)  Top of per-CPU kernel stack
0x20    target_entry         8 B   kernel BSP     trampoline (Phase2)  ap_entry function address
0x28    ready                4 B   trampoline     BSP (poll)          Set to 1 when AP starts spinning
0x2C    go                   4 B   kernel BSP     trampoline (Phase1)  Set to 1 to release AP from spin
0x30    lapic_id             4 B   bootloader     (informational)     AP's LAPIC ID
0x34    _pad                 4 B   —               —                   Padding to 0x38 bytes
```

Compile-time guards verify layout:
```rust
const _: [(); 1] = [(); (core::mem::offset_of!(ApArg, go) == 0x2C) as usize];
const _: [(); 1] = [(); (core::mem::offset_of!(ApArg, lapic_id) == 0x30) as usize];
const _: [(); 1] = [(); (core::mem::size_of::<ApArg>() == 0x38) as usize];
```

### 2.2 BootInfo SMP Fields (`system/src/lib.rs:39-53`)

```rust
pub struct BootInfo {
    // ...
    pub max_cpus: u32,              // MAX_CPUS from system (currently 4)
    pub bsp_apic_id: u32,           // Always 0 on x86 (BSP is LAPIC ID 0)
    pub ap_trampoline_phys: u64,    // Physical address of ap_trampoline() function
    pub ap_count: u32,              // Number of APs brought up by MP Services
    pub ap_apic_ids: [u32; MAX_CPUS],   // LAPIC IDs of each AP
    pub ap_arg_phys: [u64; MAX_CPUS],   // Physical addresses of each ApArg block
}
```

### 2.3 PerCpu (`kernel/src/percpu.rs:137-162`)

One `PerCpu` slot per LAPIC ID (modulo MAX_CPUS = 4). Indexed by `apic_id % MAX_CPUS`.

```rust
pub struct PerCpu {
    pub apic_id: AtomicU32,          // This CPU's LAPIC ID
    pub online: AtomicBool,          // Set true by mark_online() in ap_entry
    pub kernel_ready: AtomicBool,    // Set true by release_all_aps()
    pub kernel_stack_top: AtomicU64, // Top of this CPU's kernel stack
    pub ticks: AtomicU64,            // Per-CPU tick counter
    pub current_task: AtomicUsize,   // Index of running task in global table
    pub task_count: AtomicUsize,     // Number of tasks assigned to this CPU
    pub ready_queue: ReadyQueue,     // Lock-free circular buffer of ready task IDs
    pub self_ptr: AtomicU64,         // Cached pointer to this slot (GS base verification)
    pub need_resched: AtomicBool,    // Set by IPI handler
    pub timer_fires: AtomicU64,      // Rate-limited timer fire counter
}
```

### 2.4 Per-CPU GDT/TSS/IDT (`kernel/src/arch/gdt.rs`, `arch/idt.rs`)

Each LAPIC ID slot has its **own**:
- **GDT**: 7-entry table (null, kernel code, kernel data, user code, user data, TSS low, TSS high)
- **TSS**: 104-byte Task State Segment with per-CPU IST1 stack (16 KiB)
- **IST1 stack**: 16 KiB aligned stack for double-fault recovery
- **GDT pointer**: `GdtPtr` struct (limit + base) for `lgdt`
- **IDTR slot**: `Idtr` struct (limit + base) for `lidt` — all point to the **same shared IDT**

The IDT contents (256 interrupt gate entries) are **shared** across all CPUs. Only the IDTR register value is per-CPU (each CPU loads the same base address via its own slot).

### 2.5 ReadyQueue (`kernel/src/percpu.rs:31-102`)

Lock-free per-CPU circular buffer of task IDs:

```rust
pub struct ReadyQueue {
    pub buf: [AtomicUsize; MAX_TASKS],  // 32 slots
    pub head: AtomicUsize,               // Dequeue position
    pub tail: AtomicUsize,               // Enqueue position
    pub count: AtomicUsize,              // Number of entries
}
```

- Single-producer (timer ISR on this CPU), single-consumer (schedule on this CPU)
- Cross-CPU wake/steal uses the global task table lock (`IrqSaveSpinLock`)

---

## 3. Phase 0: Platform Discovery (Bootloader)

**Source:** `boot/src/mp.rs:43-166` — `bring_up_aps()`

### 3.1 MP Services Protocol Opening

```rust
let mp_handle = uefi::boot::get_handle_for_protocol::<MpServices>()?;
let mp = uefi::boot::open_protocol_exclusive::<MpServices>(mp_handle)?;
let count = mp.get_number_of_processors()?;
```

UEFI PI (Platform Initialization) specification Volume 2 defines the MP Services Protocol. It provides:
- `GetNumberOfProcessors()` — returns total and enabled processor count
- `GetProcessorInfo()` — per-processor status (BSP/AP, enabled/healthy)
- `StartupThisAP()` — dispatches a function to a specific AP

### 3.2 Processor Enumeration

For each processor index `0..count.total`:

1. **Get processor info**: `mp.get_processor_info(proc_num)`
2. **Filter**: Skip if not enabled, not healthy, or is BSP
3. **Cap at MAX_CPUS**: Maximum AP count is `MAX_CPUS - 1` (MAX_CPUS = 4, so up to 3 APs)
4. **Allocate ApArg page** (UEFI `allocate_pages`, 1 page = 4 KiB, `MemoryType::LOADER_DATA`):
   ```rust
   let ap_arg_ptr = uefi::boot::allocate_pages(
       AllocateType::AnyPages,
       MemoryType::LOADER_DATA,
       1,  // 1 page
   )?;
   ```
5. **Initialize ApArg**:
   ```rust
   ap_arg.ready.store(0, Ordering::Release);
   ap_arg.go.store(0, Ordering::Release);
   ap_arg.lapic_id = info.processor_id as u32;
   ```
6. **Allocate 16 KiB kernel stack** (4 pages, `MemoryType::LOADER_DATA`):
   ```rust
   let stack_top = stack_phys + (AP_STACK_PAGES * 4096);
   ap_arg.target_kernel_stack = stack_top;
   ```

### 3.3 AP Startup via UEFI

```rust
mp.startup_this_ap(
    proc_num,           // AP processor number
    ap_trampoline,      // NAKED function pointer
    arg,                // ApArg pointer
    None,               // No timeout (use timeout parameter)
    Some(Duration::from_millis(100)),  // 100 ms timeout
);
```

`StartupThisAP` causes UEFI to:
1. Send a **SIPI** (Startup IPI) to the target AP
2. The AP wakes from its HLT state and enters the trampoline function at its current CS:RIP
3. UEFI sets up a temporary stack for the AP
4. The AP is in **long mode** with **UEFI page tables** active (identity-mapped)

The function **always** times out because the trampoline never returns (it spins on `go`). The timeout is expected and harmless:
```rust
Err(e) if e.status() == Status::TIMEOUT => {
    // Expected — the AP is alive, running the trampoline.
}
```

After the timeout (or immediately if `ready` is already 1), the BSP polls for `ready == 1`:
```rust
if ap_arg.ready.load(Ordering::Acquire) == 0 {
    for _ in 0..500 {  // 5 seconds total
        uefi::boot::stall(Duration::from_millis(10));
        if ap_arg.ready.load(Ordering::Acquire) != 0 {
            break;
        }
    }
}
```

### 3.4 BootInfo Recording

```rust
boot_info.ap_apic_ids[ap_index] = info.processor_id as u32;
boot_info.ap_arg_phys[ap_index] = ap_arg_phys;
boot_info.ap_count = ap_index;
boot_info.bsp_apic_id = 0;  // BSP is always LAPIC ID 0
```

The bootloader writes `BootInfo` back to the chainloader-allocated struct at `boot_info_ptr`, then exits boot services and jumps to the kernel.

---

## 4. Phase 1: UEFI MP Trampoline — Phase 1 (Spin)

**Source:** `boot/src/mp.rs:236-301` — `ap_trampoline()`

This is a **naked** function (no compiler-generated prologue/epilogue). The Microsoft x64 ABI passes the first argument (`arg: *mut c_void` = ApArg pointer) in **RCX**. The inline asm reads it directly.

### 4.1 Entry State

When the AP enters `ap_trampoline`:
- **RCX** = ApArg physical address (pointer to `ApArg`)
- **CPU mode**: 64-bit long mode
- **Page tables**: UEFI identity map (all physical memory)
- **GDT**: UEFI GDT
- **IDT**: UEFI IDT (or loaded by MP Services)
- **RSP**: Temporary UEFI stack (may be above 4 GB!)
- **Interrupts**: enabled (UEFI MP Services may have left them on)
- **TR**: UEFI TSS
- **Segment registers**: UEFI values

### 4.2 Phase 1 Assembly (Complete Walkthrough)

```asm
cli                                    ; Disable interrupts immediately
mov dword ptr [rcx + 0x28], 1          ; ApArg.ready = 1
mfence                                  ; Ensure ready write is globally visible
push rcx                                ; Save ApArg pointer
lea rcx, [rip + CP1_MSG]               ; RCX = "AP GO RELEASED\r\n\0"
call trampoline_puts                     ; Write diagnostic to COM1
pop rcx                                 ; Restore ApArg pointer

2:                                      ; Spin loop
pause                                   ; SMT-friendly yield
xor eax, eax                           ; EAX = 0
lock xchg [rcx + 0x2C], eax            ; Atomically: read go into EAX, write 0
test eax, eax                          ; Was go == 1?
jz 2b                                  ; If not, keep spinning
                                        ; --- FALL THROUGH TO PHASE 2 ---
```

### 4.3 The Lock XCHG Protocol

This is the **critical** WHPX-safe cross-VP wakeup primitive:

**Why not a simple `mov` + `cmp`?** WHPX does **not** maintain per-VP L1 cache coherence on plain loads. A `mov` from the BSP's store may be stuck in the BSP's L1 cache, and the AP's `mov` load reads a stale value from its own L1.

**Why `lock xchg`?** On x86, `xchg` with a memory operand is **implicitly locked** (the LOCK prefix is automatic). A locked operation:
1. Flushes the store buffer
2. Bypasses the local L1 cache
3. Goes directly to the cache-coherency protocol (MESI)
4. Provides full memory ordering (sequential consistency for the locked operation)

The `lock xchg` reads `go` and **clears it to 0** atomically. This serves as a "consume-and-clear" pattern — the AP consumes the release signal and resets it for the next potential use.

### 4.4 Diagnostic COM1 Output

Three checkpoint messages are defined and emitted during trampoline execution:

| Message | Static | When | Purpose |
|---------|--------|------|---------|
| `CP1_MSG` | `"AP GO RELEASED\r\n\0"` | Entering Phase 1 spin | Proves the AP reached the trampoline |
| `CP2_MSG` | `"AP CR3 SWITCHED\r\n\0"` | After CR3 load (Phase 2) | Proves page table switch succeeded |
| `CP3_MSG` | `"AP JUMPING TO ENTRY\r\n\0"` | Before `jmp ap_entry` (Phase 2) | Proves all switch steps completed |

These use `trampoline_puts()` (`boot/src/mp.rs:212-234`), a simple function that polls LSR bit 5 and writes raw bytes to COM1.

### 4.5 Trampoline Function Address Registration

```rust
boot_info.ap_trampoline_phys = mp::ap_trampoline as *const () as u64;
```

The kernel records this physical address so it can **reserve the page** — the trampoline code lives in the bootloader's UEFI memory, which the buddy allocator would otherwise treat as free LOADER_DATA memory.

---

## 5. Phase 2: Kernel AP Resource Reservation

**Source:** `kernel/src/main.rs:244-270`

The kernel reserves AP-related physical pages **BEFORE** page table initialization (`virt::init()`). This is critical because `virt::init()` allocates ~1600 page-table pages from the buddy allocator, which would overwrite AP data if those pages were in the free list.

### 5.1 Reservation Sequence

```rust
if info.ap_count > 0 {
    // 1. Reserve the SIPI trampoline page at 0x8000
    mm::phys::reserve_range(ap_start::TRAMPOLINE_PHYS, 1);
    // 2. Reserve the UEFI MP trampoline code page
    let tramp_page = info.ap_trampoline_phys & !0xFFF;
    if tramp_page != 0 {
        mm::phys::reserve_range(tramp_page, 1);
    }
    // 3. For each AP: reserve ApArg page + 4 kernel stack pages
    for i in 0..(info.ap_count as usize) {
        let arg_phys = info.ap_arg_phys[i];
        mm::phys::reserve_range(arg_phys, 1);
        let stack_top = (*ap).target_kernel_stack;
        if stack_top > 0 {
            let stack_base = stack_top - (ap_stack_pages as u64) * 4096;
            mm::phys::reserve_range(stack_base, ap_stack_pages);
        }
    }
}
```

### 5.2 What Gets Reserved

| Resource | Size | Location | Why Reserve |
|----------|------|----------|-------------|
| SIPI trampoline | 4 KB | 0x8000–0x8FFF | Real-mode trampoline for INIT-SIPI-SIPI path |
| UEFI trampoline code | 4 KB | ap_trampoline phys page | APs may still be executing this code |
| ApArg per AP | 4 KB × ap_count | UEFI LOADER_DATA | APs read/write go/ready; BSP writes target_* fields |
| Kernel stack per AP | 16 KB × ap_count | UEFI LOADER_DATA | AP switches RSP here before CR3 switch |

Each `reserve_range()` call splits the buddy block containing the target address, removing the target pages from the free lists.

---

## 6. Phase 3: BSP Pre-Release Per-CPU Setup

**Source:** `kernel/src/main.rs:421-440`

Before releasing APs, the BSP must set up:

### 6.1 Per-CPU GDT/TSS Deskriptor Pre-Initialization

The trampoline does `lgdt [target_gdt_ptr]` — it loads a GDT from memory. If the GDT's TSS descriptor (entries 5+6, selector 0x28) is not valid (zero), then:
1. The AP's eventual `ltr` instruction will #GP (error code = selector index)
2. Even before `ltr`, the AP's first interrupt will try to consult the TSS for ring transitions or IST entry

Therefore, the BSP must initialize **each AP's** GDT TSS descriptor **before** releasing the AP:

```rust
for i in 0..count {
    let apic_id = boot_info.ap_apic_ids[i];
    let slot = (apic_id as usize) % MAX_CPUS;
    crate::arch::gdt::init_tss_descriptor_for_slot(slot);
}
```

#### `init_tss_descriptor_for_slot()` (`gdt.rs:324-339`):

```rust
pub fn init_tss_descriptor_for_slot(slot: usize) {
    TSS_TABLE[slot].rsp0 = dummy_rsp0_for_slot(slot);       // Per-CPU dummy stack
    TSS_TABLE[slot].ist1 = ist1_top_for_slot(slot);         // Per-CPU IST1 stack (16 KiB)

    let tss_addr = &raw const TSS_TABLE[slot] as u64;
    let tss_limit = (core::mem::size_of::<Tss>() - 1) as u32;
    let (tss_lo, tss_hi) = make_tss_descriptor(tss_addr, tss_limit);
    GDT_TABLE[slot].tss_low = tss_lo;
    GDT_TABLE[slot].tss_high = tss_hi;

    GDT_PTR_TABLE[slot].limit = (core::mem::size_of::<Gdt>() - 1) as u16;
    GDT_PTR_TABLE[slot].base = &raw const GDT_TABLE[slot] as u64;
}
```

This does **NOT** `lgdt` or `ltr` on the BSP — it only writes the GDT and TSS data into the per-CPU tables so the AP will load a valid GDT when it executes `lgdt`.

### 6.2 Per-CPU TLS Setup for BSP

```rust
arch::apic::set_bsp_lapic_id(info.bsp_apic_id);  // Record BSP LAPIC ID for APIC driver
percpu::set_bsp_apic_id(info.bsp_apic_id);        // Record BSP LAPIC ID for scheduler
percpu::mark_online(info.bsp_apic_id);            // Mark BSP slot online
percpu::install_gs_base(info.bsp_apic_id as usize); // Set GS base + TSC_AUX
```

`install_gs_base()` (`percpu.rs:247-265`):
```rust
wrmsr(IA32_GS_BASE, ptr);           // GS = &PERCPU[slot]
wrmsr(IA32_KERNEL_GS_BASE, ptr);    // Swap target = same (no swapgs used)
wrmsr(IA32_TSC_AUX, apic_id);       // rdtscp returns LAPIC ID in ECX
```

### 6.3 IDT Initialization

The IDT is initialized **once** (shared among all CPUs). Each CPU slot gets its own IDTR slot (all pointing to the same IDT base):
```rust
for slot in 0..MAX_CPUS {
    IDTR_TABLE[slot].limit = ...;
    IDTR_TABLE[slot].base = &raw const IDT as u64;
}
```

Vector 8 (Double Fault) uses **IST1** — a per-CPU stack. The BSP's IST1 is set during `init()`; each AP's IST1 is set in its own GDT/TSS (via `init_tss_descriptor_for_slot`).

### 6.4 Sequence Check

```
Phase 3 Ordering (kernel/src/main.rs:421-468):
  1. gdt::init_for_slot(bsp_slot)         — BSP loads its own GDT + ltr
  2. idt::init()                          — Shared IDT, BSP loads IDTR
  3. percpu::mark_online(bsp_apic_id)     — BSP slot online
  4. percpu::install_gs_base(bsp_slot)    — BSP per-CPU TLS
  5. task::init() + init_idle_task()      — Task system
  6. create_task (test tasks)             — Task 1, Task 2 on BSP
  7. exec::load (ExRun)                   — Optional ring-0 process
  8. intr::install_all_masked()           — IOAPIC routes programmed
  9. apic::enable()                       — LAPIC enabled
  10. apic::calibrate_pit()               — Measure LAPIC timer rate
  11. apic::configure_timer(...)          — 1 ms periodic timer
  12. apic::pit_enable_periodic(100)      — PIT 100 Hz
  13. ap_start::release_aps(info)         — **Release APs from trampoline**
  14. percpu::release_all_aps()           — **Set kernel_ready for all APs**
  15. sti                                 — BSP enables interrupts
  16. int 32 test                         — Software-trigger timer IRQ
  17. Unmask PIT + keyboard routes        — Enable device IRQs
```

---

## 7. Phase 4: release_aps() in Detail

**Source:** `kernel/src/ap_start.rs:550-743`

This function is called by the BSP to release all APs from the UEFI MP trampoline's `go` spin loop.

### 7.1 Function Signature

```rust
pub fn release_aps(boot_info: &BootInfo)
```

Caller must have disabled interrupts (the function expects `cli` — it uses PIT polling for timing, which is sensitive to interrupt jitter).

### 7.2 Collect Kernel State

```rust
let pml4_phys = crate::mm::virt::pml4_address();          // Current CR3
let gdt_desc_addr = crate::arch::gdt::gdt_pointer_address(); // BSP's GDT ptr addr
let idt_desc_addr = crate::arch::idt::idt_pointer_address(); // BSP's IDT ptr addr
let ap_entry_addr = ap_entry as *const () as u64;          // ap_entry() address
```

Note: `gdt_pointer_address()` and `idt_pointer_address()` return the BSP's own slots. Each AP's `target_gdt_ptr` and `target_idt_ptr` are overwritten with per-CPU pointers later.

### 7.3 Per-AP Pre-Init

```rust
for i in 0..count {
    let apic_id = boot_info.ap_apic_ids[i];
    let slot = (apic_id as usize) % MAX_CPUS;
    crate::arch::gdt::init_tss_descriptor_for_slot(slot);
}
```

This ensures the AP's GDT has a valid TSS descriptor **before** the AP loads `lgdt`. Without this, the AP's GDT would have a zero TSS descriptor (from the static initializer), and the AP would #GP on the first `ltr` or interrupt.

### 7.4 Per-AP Release (The Core Loop)

```rust
for i in 0..count {
    let apic_id = boot_info.ap_apic_ids[i];
    let arg_phys = boot_info.ap_arg_phys[i];
    let slot = (apic_id as usize) % MAX_CPUS;
    let per_cpu_gdt = gdt_pointer_for_slot(slot);
    let per_cpu_idt = idt_pointer_for_slot(slot);

    let ap = arg_phys as *mut ApArg;

    // Step A: Write target fields
    unsafe {
        (*ap).target_pml4_phys = pml4_phys;     // Kernel PML4
        (*ap).target_gdt_ptr = per_cpu_gdt;     // Per-CPU GDT pointer
        (*ap).target_idt_ptr = per_cpu_idt;     // Per-CPU IDT pointer (shared content)
        (*ap).target_entry = ap_entry_addr;     // ap_entry() address
    }

    // Step B: clflush every 64-byte cache line of ApArg
    for off in (0..core::mem::size_of::<ApArg>()).step_by(64) {
        clflush((ap as *const u8).add(off));
    }

    // Step C: Atomic release — lock xchg go=1
    let ap_ptr = ap as *const u8;
    core::arch::asm!(
        "mov eax, 1",
        "lock xchg [{}], eax",
        in(reg) ap_ptr.add(0x2C),  // &go field
        out("eax") _,
        options(nostack, preserves_flags),
    );

    // Step D: Final clflush of go field
    clflush(&(*ap).go as *const _ as *const u8);
}
```

### 7.5 The clflush Protocol

WHPX (and some KVM configurations) can cache stores in the BSP's L1 cache and **not** make them visible to the AP's memory accesses, even after `mfence`. The workaround:

1. **Write** all fields normally (plain `mov`)
2. **clflush** each 64-byte cache line of ApArg — forces the cache line out of the BSP's L1
3. **mfence** after each clflush — ensures the clflush completes before any later instruction
4. **lock xchg** for `go` — uses a locked atomic that bypasses caches entirely
5. **Another clflush** of the `go` field — pushes the new value out of the BSP's L1

Without these steps, WHPX can return stale values to the AP's `lock xchg` read, leaving the AP stuck forever.

### 7.6 The INIT-SIPI-SIPI Bypass

After setting `go=1`, `release_aps` **immediately returns**:

```rust
log::info!("SMP: skipping INIT-SIPI-SIPI (UEFI MP path in use)");
return;
```

The INIT-SIPI-SIPI code path (lines 646-742) is:
- Preserved for non-WHPX backends (TCG, KVM, bare metal)
- Disabled by the `return` above (the code is inside `#[allow(unreachable_code)] { ... }`)
- Uses the real-mode trampoline at 0x8000 with full real→protected→long mode transition

### 7.7 After release_aps() Returns

Control returns to `_start` in `kernel/src/main.rs`:

```rust
ap_start::release_aps(info);           // Step 13: Write go=1 for all APs

// All CPUs (BSP + APs) may now enter the scheduler.
percpu::release_all_aps();             // Step 14: Set kernel_ready=true for ALL slots

log::info!("Enabling interrupts");
unsafe { core::arch::asm!("sti") };    // Step 15: BSP enables interrupts
```

#### `release_all_aps()` (`percpu.rs:215-219`):

```rust
pub fn release_all_aps() {
    for slot in 0..MAX_CPUS {
        PERCPU[slot].kernel_ready.store(true, Ordering::Release);
    }
}
```

**Every** slot gets `kernel_ready=true`, even slots not yet online. This ensures that by the time an AP reaches `wait_for_kernel_ready()`, the flag is already set (the write happened before the AP even started executing through the trampoline).

---

## 8. Phase 5: UEFI MP Trampoline — Phase 2 (Full Switch)

When the AP's `lock xchg` reads `go=1`, execution falls through to Phase 2 of the trampoline. The AP is still in long mode on the UEFI stack with UEFI page tables.

### 8.1 Phase 2 Assembly (Complete Walkthrough)

```asm
; --- Phase 2: Full environment switch ---
; RCX still holds ApArg pointer (preserved from Phase 1)

mov rax, [rcx + 0x00]               ; RAX = target_pml4_phys
mov rsp, [rcx + 0x18]               ; RSP = target_kernel_stack (per-CPU 16 KiB)
                                     ; *** STACK SWITCH BEFORE CR3 ***
                                     ; The UEFI stack may be above 4 GB, where
                                     ; the kernel identity map (PML4[0] → 0..4 GB)
                                     ; does not reach. We need a mapped stack
                                     ; for all subsequent push/pop/ret operations.
mov cr3, rax                         ; CR3 = kernel PML4
                                     ; *** PAGE TABLE SWITCH ***
                                     ; From now on: kernel higher-half map active
                                     ; Identity map covers 0..4 GB (2 MB huge pages)
                                     ; LAPIC at 0xFEE00000 accessible via identity + higher-half

; --- Checkpoint 2: CR3 switched ---
push rcx
lea rcx, [rip + CP2_MSG]             ; "AP CR3 SWITCHED\r\n\0"
call trampoline_puts
pop rcx

; --- Load kernel GDT ---
mov rbx, [rcx + 0x08]               ; RBX = target_gdt_ptr (per-CPU GDT ptr)
lgdt [rbx]                           ; Load GDTR with per-CPU GDT
                                     ; This GDT has the AP's own TSS descriptor!

; --- Reload CS via far return ---
push 0x08                            ; Kernel code selector
lea rax, [4f]                        ; Local label 4
push rax
retfq                                ; Far return: loads CS from stack
                                     ; CS.base forced to 0 in long mode
4:

; --- Reload data segments ---
mov ax, 0x10                         ; Kernel data selector
mov ds, ax
mov es, ax
mov fs, ax
mov gs, ax
mov ss, ax                           ; *** STACK SEGMENT NOW KERNEL ***

; --- Load kernel IDT ---
mov rbx, [rcx + 0x10]               ; RBX = target_idt_ptr (per-CPU IDT ptr)
lidt [rbx]                           ; Load IDTR with shared IDT

; --- Checkpoint 3: About to jump to entry ---
push rcx
lea rcx, [rip + CP3_MSG]            ; "AP JUMPING TO ENTRY\r\n\0"
call trampoline_puts
pop rcx

; --- Set up and jump to ap_entry ---
mov rdi, rcx                         ; RDI = ApArg pointer (SysV ABI first arg)
mov rax, [rcx + 0x20]               ; RAX = target_entry (ap_entry address)
jmp rax                              ; Jump (never returns)
```

### 8.2 Critical Details

**Why switch RSP before CR3?**
The UEFI stack may be allocated above 4 GB (e.g., at 0x100_000_000 or higher). The kernel's identity map (PML4[0]) covers only 0..4 GB. If we switched CR3 first while RSP pointed above 4 GB, the next `push` (from the `retfq` or any other instruction) would #PF because the stack address is not mapped.

**Why per-CPU GDT?**
Each GDT has its own TSS descriptor pointing to the AP's own TSS (with its own IST1 stack). If all APs shared the BSP's GDT:
- The AP's `ltr` would load the BSP's TSS
- The BSP's IST1 stack would be used for the AP's double faults
- A #DF on the AP would corrupt the BSP's IST1 stack
- The first ring transition (interrupt) on the AP would use the BSP's RSP0

**Why per-CPU IDTR?**
The IDT contents are shared, but the IDTR register is CPU-local. Giving each AP its own IDTR slot (all pointing to the same IDT base) keeps the boot MP path simple: the boot code writes `target_idt_ptr` per-AP from `idt_pointer_for_slot(slot)`, and the AP loads it.

### 8.3 Register State After Trampoline

| Register | Value |
|----------|-------|
| RDI | ApArg physical address (kernel's first arg) |
| CS | 0x08 (kernel code selector) |
| DS/ES/FS/GS/SS | 0x10 (kernel data selector) |
| RSP | Per-CPU kernel stack (16 KiB, top) |
| CR3 | Kernel PML4 physical address |
| GDTR | Per-CPU GDT (with per-CPU TSS descriptor) |
| IDTR | Shared IDT (all 256 vectors) |
| RFLAGS.IF | 0 (interrupts disabled — `cli` at trampoline entry) |

---

## 9. Phase 6: ap_entry() — Kernel AP Init

**Source:** `kernel/src/ap_start.rs:54-145`

### 9.1 Entry

```rust
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry(arg: u64) -> ! {
```

Called with the **ApArg physical address** in RDI (SysV ABI). The function never returns.

### 9.2 Step 1: COM1 Diagnostic

```rust
for &byte in b"AP ENTRY REACHED\r\n" {
    // Poll LSR bit 5, write to THR 0x3F8
}
```

This write happens **before any kernel state is accessed** — no LAPIC, no logger, no globals. It proves the AP reached `ap_entry` with a working stack and code execution.

### 9.3 Step 2: Read LAPIC ID

```rust
let apic_id: u32 = unsafe {
    let raw: u32;
    let lapic_id_addr = crate::arch::apic::LAPIC_BASE + crate::arch::apic::APIC_ID as u64;
    core::arch::asm!(
        "mov eax, dword ptr [{addr}]",
        addr = in(reg) lapic_id_addr as *const u32,
        out("eax") raw,
    );
    raw >> 24
};
let _ = arg; // suppress unused
```

Reads the LAPIC ID register (offset 0x20 from LAPIC base). The LAPIC MMIO is accessible because:
- The kernel identity map covers 0..4 GB
- The 2 MB page containing 0xFEE00000 is mapped with PRESENT | WRITABLE | PCD
- The higher-half mapping was created during `init_mmio()` (but that's a higher-half mapping — the identity map is used here)

Note: The trampoline passed the ApArg pointer in RDI, but this function **ignores** it (`let _ = arg`). The LAPIC ID is read from the MMIO register instead. This is because the trampoline sets RDI = ApArg pointer, but the ApArg's `lapic_id` field could also be used.

### 9.4 Step 3: FPU/SSE Initialization

```rust
asm!("fninit", options(nostack, preserves_flags));

let mut cr4: u64;
asm!("mov {}, cr4", out(reg) cr4, options(nomem, preserves_flags));
cr4 |= 1 << 9    // CR4.OSFXSR — enable SSE instructions
     | 1 << 10   // CR4.OSXMMEXCPT — enable SIMD floating-point exceptions
     | 1 << 18;  // CR4.OSXSAVE — enable XSAVE/XSAVEC/XSAVES
asm!("mov cr4, {}", in(reg) cr4, options(nomem, preserves_flags));
```

Each AP has its own FPU/SSE state (the x87 FPU state is reset by `fninit`). The CR4 bits enable SSE support — without these, any SSE/SSE2 instruction (e.g., `movaps`, `sqrtss`) would cause #UD.

The BSP does this same init at the very beginning of `_start` (kernel/src/main.rs:155-163). APs must do it themselves because:
1. INIT-SIPI-SIPI resets CR4 to its power-on value
2. UEFI MP Services preserves CR4 from the firmware, but the kernel cannot rely on it

### 9.5 Step 4: Mark Online + TLS

```rust
percpu::mark_online(apic_id);
log::info!("AP[lapic={}] entered ap_entry, stack OK", apic_id);

percpu::install_gs_base(apic_id as usize);
```

#### `mark_online()` (`percpu.rs:187-195`):
```rust
pub fn mark_online(apic_id: u32) {
    let slot = (apic_id as usize) % MAX_CPUS;
    let p = &PERCPU[slot] as *const PerCpu as *mut PerCpu;
    (*p).apic_id.store(apic_id, Ordering::Release);
    (*p).online.store(true, Ordering::Release);
    log::info!("percpu: CPU {} online", apic_id);
}
```

#### `install_gs_base()` (`percpu.rs:247-265`):
Three MSR writes:
1. **IA32_GS_BASE** (0xC000_0102) = virtual address of `PERCPU[slot]` — enables `%gs:offset` access to per-CPU data
2. **IA32_KERNEL_GS_BASE** (0xC000_0101) = same address — `swapgs` would swap to this, but the kernel doesn't use `swapgs`
3. **IA32_TSC_AUX** (0xC000_0103) = `apic_id` — enables `rdtscp` to return the LAPIC ID in ECX

After this, `percpu::current_apic_id()` uses the fast path:
```rust
pub fn current_apic_id() -> u32 {
    let aux: u32;
    unsafe {
        core::arch::asm!("rdtscp", out("ecx") aux, options(nostack, preserves_flags));
    }
    aux
}
```

This is ~10× faster than reading the LAPIC ID MMIO register.

### 9.6 Step 5: Enable AP LAPIC Timer

```rust
crate::arch::apic::ap_enable_timer(apic_id);
```

#### `ap_enable_timer()` (`apic.rs:326-341`):
```rust
pub fn ap_enable_timer(_apic_id: u32) {
    // Each AP must enable its own LAPIC separately
    write32(APIC_LVT_LINT0, APIC_LVT_MASKED);   // Mask legacy LINT0
    write32(APIC_LVT_LINT1, APIC_LVT_MASKED);   // Mask legacy LINT1
    write32(APIC_LVT_ERROR, APIC_LVT_MASKED | 0xFF);  // Error LVT: masked, vector 0xFF
    write32(APIC_SVR, APIC_SVR_ENABLE | 0xFF);  // Enable LAPIC via SVR
    write32(APIC_TPR, 0);                        // Accept all interrupt priorities

    let count = TICKS_PER_MS * 1;                // BSP-calibrated ticks per millisecond
    write32(APIC_LVT_TIMER, 32 | APIC_LVT_PERIODIC); // Vector 32, periodic
    write32(APIC_TDCR, 0b0011);                  // Divide by 16
    write32(APIC_TICR, count);                   // Start timer (1 ms period)
}
```

Key points:
- Uses the BSP's **calibrated** `TICKS_PER_MS` value (global static)
- Programs the same vector 32 as the BSP → same ISR runs on all CPUs
- The ISR reads `percpu::current_apic_id()` to know which CPU's tick counter to increment
- Even though `TICKS_PER_MS` is calibrated by the BSP, it's the same bus clock on all CPUs in an SMP system

### 9.7 Step 6: Wait for kernel_ready

```rust
percpu::wait_for_kernel_ready(apic_id);
```

#### `wait_for_kernel_ready()` (`percpu.rs:203-208`):
```rust
pub fn wait_for_kernel_ready(apic_id: u32) {
    let slot = (apic_id as usize) % MAX_CPUS;
    while !PERCPU[slot].kernel_ready.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}
```

This spin loop ensures the AP does **not** enter the scheduler until the BSP has finished all initialization:
- IOAPIC routes installed
- LAPIC enabled and calibrated
- Test tasks created
- ExRun spawned

The BSP calls `release_all_aps()` which sets `kernel_ready=true` for **all** CPU slots. Because this happens **before** the AP has had time to boot through the trampoline and reach this point, the check passes immediately (or after a very short spin).

### 9.8 Step 7: Register Idle Task

```rust
crate::task::init_idle_task();
```

#### `init_idle_task()` (`task.rs:119-163`):

1. **Allocate kernel stack**: `phys::alloc_pages(2)` returns 2 pages (8 KiB) of physical memory
2. **Calculate virtual address**: `stack_base = HIGHER_HALF + phys`, `stack_top = stack_base + 8192`
3. **Zero the stack**: `ptr::write_bytes(stack_base, 0, 8192)`
4. **Read current RSP**: `mov {}, rsp` — captures the AP's current stack pointer
5. **Get CPU ID**: `cpu_id = current_cpu_id()`
6. **Build TrapFrame**: Creates a dummy frame with:
   - All GPRs = 0
   - CS = 0x08 (kernel code)
   - RFLAGS = 0x202 (IF=1, reserved bit 1 always set)
   - RSP = current RSP (the stack the AP is currently using)
   - SS = 0x10 (kernel data)
7. **Register task**: Allocates task ID = `manager.count` (sequential), stores Task with:
   - `caps = Caps::all()` (idle task has all capabilities)
   - `pml4 = virt::pml4_address()` (kernel's PML4)
   - `state = TaskState::Ready`
8. **Bump counters**: `manager.count += 1`, `percpu::set_current(cpu_id, task_id)`, `percpu::set_task_count(cpu_id, 1)`

Each AP creates its own idle task as the first (and initially only) task on its runqueue. The BSP also creates an idle task during its own init. There is one idle task per CPU.

### 9.9 Step 8: Load Per-CPU TSS

```rust
unsafe {
    core::arch::asm!("ltr ax", in("ax") 0x28u16, options(nostack, preserves_flags));
}
```

**Why this is critical:**

The AP arrives via either:
- **INIT-SIPI-SIPI**: Resets TR (Task Register) to 0 — no TSS loaded
- **UEFI MP Services**: TR may contain UEFI's TSS (which references UEFI's IST1 stack)

Neither is correct for the kernel. The AP must `ltr` **its own** TSS (selector 0x28, which points to GDT entries 5+6 = the per-CPU TSS descriptor created by `init_tss_descriptor_for_slot` before release).

**Without this `ltr`:**
1. The first ring transition (interrupt) would consult the TSS for RSP0 — which either doesn't exist (TR=0 → #GP) or points to the wrong stack
2. The first #DF would use the IST1 entry from whatever TSS is in TR — possibly the BSP's IST1 stack
3. #DF on vector 8 uses IST1 (set in IDT entry). If TR points to the BSP's TSS, the #DF handler runs on the BSP's IST1 stack, corrupting BSP data

The `ltr` must happen **after** task init (so the per-CPU TSS is ready) and **before** `sti` (so the first interrupt finds a valid TSS).

### 9.10 Step 9: Enable Interrupts

```rust
unsafe { core::arch::asm!("sti") };
```

Interrupts are enabled:
- The LAPIC timer (programmed in step 5) starts firing at 1 ms intervals
- Vector 32 ISR runs on this AP
- The ISR calls `task::schedule()` which checks the per-CPU ready queue
- If tasks have been stolen from other CPUs, they start executing

### 9.11 Step 10: Enter Scheduling Loop

```rust
ap_sched_loop(apic_id)
```

**The AP never returns from here.**

---

## 10. Phase 7: AP Scheduling Loop

**Source:** `kernel/src/ap_start.rs:151-175`

```rust
fn ap_sched_loop(apic_id: u32) -> ! {
    // Brief pause so the BSP can finish boot before we start stealing
    for _ in 0..100_000 {
        unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
    }
    let mut count = 0u64;
    loop {
        // Pause to let the timer ISR fire (1000 pause instructions ≈ a few µs)
        for _ in 0..1000 {
            unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
        }
        count += 1;
        // Every ~100 batches, try to steal work if only idle task remains
        if count % 100 == 0 {
            let cpu = apic_id as usize;
            if percpu::task_count(cpu) <= 1 {
                crate::task::steal_task(cpu);
            }
        }
    }
}
```

### 10.1 Why `pause` Instead of `hlt`

WHPX does **not** handle `hlt` from AP vCPUs correctly (reports "Unexpected VP exit code 4"). Therefore, APs must use a `pause`-based spin loop instead of the more efficient `sti; hlt;` pattern that the BSP uses.

### 10.2 How APs Get Work

Initially, each AP has only its idle task. In this state:
1. The LAPIC timer fires every 1 ms
2. The timer ISR calls `task::schedule(frame)`
3. `schedule()` checks the per-CPU ready queue — it's empty (only idle task)
4. `schedule()` returns false (no context switch)
5. Control returns to the idle loop

Every ~100 batches (each batch is ~1000 pauses), the AP calls `steal_task()`. This looks for a CPU with ≥2 tasks and moves one to the hungry AP's runqueue. Eventually, the AP picks up the stolen task on the next timer tick.

### 10.3 AP Timer ISR Behavior

The LAPIC timer ISR (vector 32) runs identically on all CPUs. The critical path in `irq_handler()` at `kernel/src/arch/idt.rs:589-648`:

```rust
32 => {
    let t = crate::percpu::tick();                             // Global tick counter
    let cpu = crate::percpu::current_apic_id() as usize;       // Per-CPU identifier
    let fires = crate::percpu::PERCPU[cpu].timer_fires.fetch_add(1, Ordering::Relaxed);

    if crate::task::is_initialized() {
        if crate::task::schedule(frame) {
            // Context switch occurred — return to new task's context
            core::arch::asm!(
                "mov rsp, {rsp}",
                "mov r15, {r15}", "mov r14, {r14}", ...,
                "sti",
                "push {rip}",
                "ret",
                // ...
            );
        }
    }
}
```

The `sti` + `ret` sequence is used instead of `iretq` because WHPX mishandles `popfq` at CPL=0 and `iretq` with CS=0x08.

---

## 11. Phase 8: INIT-SIPI-SIPI Path (Preserved, Disabled)

**Source:** `kernel/src/ap_start.rs:216-743`

This code is preserved but **disabled** by the `return` statement at line 641. It exists for non-WHPX backends (TCG, KVM, bare metal).

### 11.1 SIPI Trampoline (`kernel/src/ap_start.rs:217-425`)

A hand-crafted 4096-byte `static` array encoded as raw x86 machine code. Loaded at physical address **0x8000** (SIPI vector 0x08).

#### Layout (within the 4 KB page at 0x8000):

| Offset | Physical | Content |
|--------|----------|---------|
| 0x0000 | 0x8000 | Real-mode (16-bit) entry |
| 0x0100 | 0x8100 | Protected mode (32-bit) code |
| 0x0200 | 0x8200 | Long mode (64-bit) code |
| 0x0F00 | 0x8F00 | Data area (filled by BSP before each SIPI) |
| 0x0F40 | 0x8F40 | GDT descriptor (10 B) |
| 0x0F50 | 0x8F50 | IDT descriptor (10 B) |
| 0x0F60 | 0x8F60 | Kernel stack top (8 B) |
| 0x0F68 | 0x8F68 | ApArg physical address (8 B) |
| 0x0F70 | 0x8F70 | ap_entry address (8 B) |
| 0x0F80 | 0x8F80 | GDT for real→protected transition (32 B) |

#### Real-mode Entry (0x8000):

```asm
cli
xor ax, ax
mov ds, ax / mov es, ax / mov ss, ax
mov sp, 0x8F00          ; Stack within page, below data area

; Enable A20 via fast A20 port 0x92
in al, 0x92
or al, 2
out 0x92, al

lgdt [0x8F80]           ; Load transition GDT

; Enable protected mode
mov eax, cr0
or eax, 1
mov cr0, eax

; Far jump to 32-bit code
jmp dword 0x08:0x8100   ; CS.base = 0x8000 → linear 0x8100
```

#### Protected Mode Entry (0x8100):

```asm
mov ax, 0x10 / mov ds, ax / mov es, ax / mov ss, ax

; Enable PAE
mov eax, cr4
or eax, 0x20            ; CR4.PAE
mov cr4, eax

; Load CR3 with kernel PML4
mov eax, [0x8F00]       ; PML4 physical address (from data area)
mov cr3, eax

; Enable long mode (IA32_EFER.LME)
mov ecx, 0xC0000080
rdmsr
or eax, 0x100           ; EFER.LME = 1
wrmsr

; Enable paging
mov eax, cr0
or eax, 0x80000000      ; CR0.PG = 1
mov cr0, eax

; Far jump to 64-bit code
jmp 0x18:0x8200         ; Selector → GDT[3] code64 (base=0, L=1)
```

#### Long Mode Entry (0x8200):

```asm
mov ax, 0x10 / mov ds, ax / mov es, ax / mov ss, ax

lgdt [0x8F40]           ; Load kernel GDT descriptor
lidt [0x8F50]           ; Load kernel IDT descriptor

mov rsp, [0x8F60]       ; Load per-CPU kernel stack

; Read LAPIC ID (higher-half virtual address)
mov rax, 0xFFFF8000FEE00020
mov eax, [rax]
shr eax, 24

; Load ApArg pointer and entry address, then jump
mov rdi, [0x8F68]       ; ApArg physical address → RDI (SysV arg1)
mov rax, [0x8F70]       ; ap_entry address → RAX
jmp rax
```

### 11.2 INIT-SIPI-SIPI Send Sequence (`send_init_sipi()` at `ap_start.rs:464-536`)

```rust
unsafe fn send_init_sipi(apic_id: u32, vector: u8) {
    // 1. Send INIT IPI
    apic::send_init_ipi(apic_id);

    // 2. Wait ~10 ms using PIT channel 0, Mode 0
    //    PIT at 1.193182 MHz, target = 11,932 (~10 ms)
    //    Poll until counter reaches ~0

    // 3. Send SIPI #1
    apic::send_sipi(apic_id, vector);

    // 4. Wait ~200 µs using PIT (target = 239)

    // 5. Send SIPI #2 (some CPUs miss the first)
    apic::send_sipi(apic_id, vector);

    // 6. Wait ~200 µs
}
```

#### `send_init_ipi()` (`apic.rs:404-419`):
```rust
write32(APIC_ICR_HIGH, (dest_apic_id as u32) << 24);  // Destination
write32(APIC_ICR_LOW, ICR_INIT | ICR_ASSERT);          // INIT, assert
// Wait for delivery status clear
```

#### `send_sipi()` (`apic.rs:423-438`):
```rust
write32(APIC_ICR_HIGH, (dest_apic_id as u32) << 24);
write32(APIC_ICR_LOW, (vector as u32) | ICR_STARTUP | ICR_ASSERT);
// Wait for delivery status clear
```

### 11.3 PIT Restoration (after INIT-SIPI-SIPI)

After sending INIT-SIPI-SIPI, the PIT must be restored to Mode 2 (rate generator) at 100 Hz — `send_init_sipi` reprogrammed it to Mode 0 for precise timing, and without restoration the PIT stops generating periodic interrupts:

```rust
let reload = (1_193_182 / 100) as u16;  // ~11,932 for 100 Hz
// Counter 0, lobyte/hibyte, Mode 2 (rate generator), binary
asm!("out 0x43, al", in("al") 0x34u8);
asm!("out 0x40, al", in("al") low);
asm!("out 0x40, al", in("al") high);
```

---

## 12. Per-CPU Infrastructure Deep Dive

### 12.1 PERCPU Array Indexing

```rust
pub static PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];
```

**MAX_CPUS = 4.** Indexing is always `apic_id % MAX_CPUS`. The design assumes LAPIC IDs are small integers (0, 1, 2, ...) which is true for QEMU/OVMF.

### 12.2 Per-CPU GDT (`kernel/src/arch/gdt.rs`)

```
Per-CPU GDT[slot]:
  [0]  null:         0x0000000000000000
  [1]  kernel_code:  make_descriptor(0, 0xFFFFF, 0x9A, 0xA)
  [2]  kernel_data:  make_descriptor(0, 0xFFFFF, 0x92, 0xA)
  [3]  user_code:    make_descriptor(0, 0xFFFFF, 0xFA, 0xA)
  [4]  user_data:    make_descriptor(0, 0xFFFFF, 0xF2, 0xA)
  [5]  tss_low:      Encoded TSS base [15:0] + access + base[31:24]
  [6]  tss_high:     Encoded TSS base [63:32]
```

Selector map:
- 0x08 = kernel code (index 1)
- 0x10 = kernel data (index 2)
- 0x18 = user code (index 3)
- 0x20 = user data (index 4)
- 0x28 = TSS (index 5+6)

### 12.3 Per-CPU TSS

```rust
struct Tss {
    reserved0: u32,
    rsp0: u64,        // Kernel stack for ring 0 entries (updated by scheduler)
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist1: u64,        // Double-fault stack (16 KiB per CPU)
    ist2..ist7: u64,  // Unused
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,  // = sizeof(Tss) = 104 (no I/O bitmap)
}
```

`rsp0` is updated by the scheduler on every context switch via `tss_set_rsp0_for_slot()`. This ensures that when an interrupt fires on this CPU, it uses the current task's kernel stack for ring→ring0 transitions.

### 12.4 Per-CPU IST1 Stacks

```rust
pub struct AlignedIstStack(pub [u8; 16384]);  // 16 KiB
static mut IST1_STACKS: [AlignedIstStack; MAX_CPUS];
```

Each CPU gets its own 16 KiB IST1 stack for double-fault handling. Without per-CPU IST1 stacks, a #DF on an AP would use the BSP's IST1 stack (from the BSP's TSS), corrupting BSP data and likely triple-faulting.

### 12.5 IDT Sharing

The IDT contents (256 `IdtEntry` × 16 bytes = 4 KB) are a single static array:

```rust
static mut IDT: [IdtEntry; 256] = { const EMPTY: IdtEntry = IdtEntry::empty(); [EMPTY; 256] };
```

Each CPU has its own `IDTR_TABLE[slot]` (base + limit), but all point to the same `IDT` array. This is correct because:
- All handlers are CPU-agnostic (they read `percpu::current_apic_id()` for per-CPU state)
- The IDT entries contain the same handler addresses for all CPUs
- Only the IST1 field for vector 8 is the same (the per-CPU IST1 stack address is in the per-CPU TSS, not the IDT)

The IDT entry for vector 8 (Double Fault) has `ist = 1` (IST1), meaning the CPU reads the IST1 address from the **current** TSS (which is per-CPU). This is how the same IDT works with different per-CPU IST1 stacks.

### 12.6 LAPIC ID Access Methods

| Method | Source | Speed | When Available |
|--------|--------|-------|---------------|
| `rdtscp` (TSC_AUX) | MSR 0xC000_0103 | ~10 cycles | After `install_gs_base()` |
| LAPIC MMIO register | 0xFEE00020 | ~100 cycles | After page table init + LAPIC MMIO map |
| ApArg.lapic_id | Memory | Fast (cached) | Only in trampoline (before ap_entry ignores it) |

The trampoline reads LAPIC ID from MMIO. `ap_entry` also reads from MMIO (before TLS is installed). After `install_gs_base()` in `ap_entry`, the `rdtscp` path is used.

---

## 13. WHPX-Specific Workarounds

### 13.1 No INIT-SIPI-SIPI

WHPX does not support the INIT-SIPI-SIPI sequence. The BSP kernel's `release_aps()` skips INIT-SIPI-SIPI and relies on the UEFI MP Services path that was set up by the bootloader.

QEMU error when INIT-SIPI-SIPI is attempted under WHPX:
```
Unexpected VP exit code 4
```

This occurs during the post-INIT PIT busy-wait in `send_init_sipi()`.

### 13.2 Lock XCHG for Cross-VP Communication

Plain `mov` stores and loads are **not** sufficient for cross-VP wakeup under WHPX because WHPX does not maintain per-VP L1 cache coherence. Both the BSP (writing `go=1`) and the AP (reading `go`) use **locked** `xchg`:

- **BSP**: `lock xchg [ApArg.go], eax` with EAX=1 (stores 1, reads previous value)
- **AP**: `lock xchg [ApArg.go], eax` with EAX=0 (reads go into EAX, clears to 0)

A `lock`-prefixed instruction is **globally visible** — it bypasses the local L1 cache and goes through the MESI protocol or hypervisor inter-VP coherence mechanism.

### 13.3 clflush for Cache Coherence

Even after `lock xchg`, WHPX may cache the ApArg's other fields (`target_pml4_phys`, `target_gdt_ptr`, etc.) in the BSP's L1. The AP reads these in Phase 2 after the `go` wakeup. Without `clflush`, the AP may see stale values.

The protocol:
1. Write all target fields (plain stores)
2. `clflush` every 64-byte cache line of the ApArg
3. `mfence` after each clflush
4. `lock xchg` for the `go` field
5. Another `clflush` of the `go` field

The AP also uses `lock xchg` to read `go`, which is the WHPX-safe primitive.

### 13.4 No HLT from AP vCPUs

WHPX does not handle `hlt` from AP vCPUs (reports "Unexpected VP exit code 4"). The AP scheduling loop uses `pause` instead of `sti; hlt;`.

### 13.5 STI + RET Instead of IRETQ

WHPX mishandles:
- `popfq` at CPL=0 (clears IF after emulation)
- `iretq` with CS=0x08 (#GP error code 0x08)

The workaround in the scheduler's context switch:
```asm
mov rsp, {new_rsp}
sti
push {new_rip}
ret
```

---

## 14. Synchronization & Memory Ordering

### 14.1 ready/go Protocol (Bootloader → Kernel)

```
Time     BSP                                      AP
----     ---                                      --
t0       allocate ApArg + stack                   (halted in UEFI)
t1       mp.startup_this_ap(ap_trampoline, arg) ──→ SIPI delivered
t2                                                 AP enters trampoline
t3                                                 cli
t4                                                 ApArg.ready = 1 (mov + mfence)
t5                                                 AP diagnostic: "AP GO RELEASED"
t6                                                 spin: lock xchg [go], eax
t7       while ready == 0 { poll }                (continues spinning)
t8       ready == 1 → record ap_arg_phys
t9       exit_boot_services()
t10      bootloader exits, kernel starts
...      (kernel init)
tN       release_aps():
tN+1       write target_pml4_phys, etc.
tN+2       clflush + mfence (entire ApArg)
tN+3       lock xchg [go], eax = 1
tN+4       clflush + mfence (go field)
           │                              lock xchg [go], eax → reads 1
tN+5                                      falls through to Phase 2
tN+6                                      mov cr3, kernel_pml4
tN+7                                      lgdt [target_gdt_ptr]
tN+8                                      lidt [target_idt_ptr]
tN+9                                      jmp ap_entry()
```

### 14.2 kernel_ready Barrier (Kernel BSP → Kernel AP)

```
Time     BSP (in _start)                        AP (in ap_entry)
----     ---------------                        ----------------
tM       release_aps()                          
tM+1     release_all_aps():
           for each slot: kernel_ready = true
tM+2     sti                                   
                                                 (AP still in trampoline or
                                                  just entering ap_entry)
tM+3                                            ap_entry():
                                                  mark_online()
                                                  install_gs_base()
                                                  ap_enable_timer()
                                                  wait_for_kernel_ready()
                                                    → reads kernel_ready == true
                                                    → exits spin loop immediately
                                                  init_idle_task()
                                                  ltr
                                                  sti
                                                  ap_sched_loop()
```

The `release_all_aps()` call sets `kernel_ready=true` for ALL slots (including those not yet online). This means:
- Even if the AP hasn't finished the trampoline yet, the flag is already set
- When the AP reaches `wait_for_kernel_ready()`, it passes immediately
- No AP can spin on `kernel_ready` for more than a few microseconds

### 14.3 Memory Ordering Summary

| Store/Load | Ordering | Why |
|-----------|----------|-----|
| `target_*` writes → `go=1` | No explicit ordering (x86 TSO guarantees stores from the same writer are visible in program order) | All `target_*` stores happen before the `lock xchg` for `go=1` on the BSP. The `lock` instruction provides a full memory barrier. |
| `go=1` on BSP → `lock xchg` reads 1 on AP | Sequential consistency (locked ops are globally visible) | The `lock xchg` by the AP reads the BSP's value. |
| `lock xchg` reads `go=1` → `target_*` reads on AP | The `lock xchg` acts as an acquire operation | After the AP reads `go=1`, all subsequent reads see the BSP's writes. |
| `kernel_ready=true` → AP reads it | Release/acquire semantics | `release_all_aps` uses `Release` ordering; `wait_for_kernel_ready` uses `Acquire`. |

### 14.4 The clflush + mfence Sequence

The `clflush` instruction is **weakly ordered** — a subsequent `mfence` ensures the flush completes before any later instruction:

```rust
unsafe fn clflush(ptr: *const u8) {
    asm!("clflush [{addr}]", addr = in(reg) ptr, options(nostack, preserves_flags));
    asm!("mfence", options(nostack, preserves_flags));
}
```

Without `mfence`, the `lock xchg` for `go=1` could execute before the `clflush` of the ApArg's target fields completes, and the AP might read stale target fields.

---

## 15. Scheduler Integration

### 15.1 Per-CPU Runqueues

Each CPU has a per-CPU `ReadyQueue` in its `PerCpu` slot:

```rust
PERCPU[cpu].ready_queue: ReadyQueue
```

The `ReadyQueue` is a lock-free circular buffer of task IDs (AtomicUsize entries + AtomicUsize head/tail/count). It uses `compare_exchange_weak` on the count for atomic push/pop.

**Single-producer, single-consumer design:**
- **Producer**: The timer ISR on this CPU (after `schedule()` re-queues the current task)
- **Consumer**: `schedule()` on this CPU (pops the next task)

Cross-CPU operations (wake, steal) acquire the global `task::MANAGER.lock` (an `IrqSaveSpinLock`).

### 15.2 CFS Virtual Runtime

The scheduler uses a simple CFS-like algorithm:

```rust
const VRUNTIME_TICK: u64 = 20;   // Virtual runtime added per timer tick
const VRUNTIME_BIAS: u64 = 8;    // Startup boost for new tasks
```

On each timer tick, the current task's `vruntime` increases by `VRUNTIME_TICK`. The scheduler picks the ready task with the **smallest** `vruntime` (Linux-style "leftmost" task in a red-black tree, though this implementation uses linear scan since MAX_TASKS = 32).

### 15.3 Task Initialization Per CPU

| CPU | Tasks Created | Task IDs |
|-----|--------------|----------|
| BSP | Idle task + Task 1 + Task 2 + ExRun (optional) | 0, 1, 2, 3 |
| AP 0 | Idle task only | 4 (or next sequential) |
| AP 1 | Idle task only | 5 (or next sequential) |
| AP 2 | Idle task only | 6 (or next sequential) |

All task IDs are sequential from a global counter (`manager.count`). Each task is assigned to the CPU that created it.

### 15.4 Work Stealing

When an AP has only its idle task (task_count <= 1), it calls `steal_task()` every ~100 million pause cycles:

```rust
pub fn steal_task(hungry_cpu: usize) -> bool {
    // Find the CPU with the most ready tasks (>= 2)
    // Pop from that CPU's ready queue under the global lock
    // Push to the hungry CPU's ready queue
    // Update task's cpu assignment + task counts
}
```

The global `MANAGER.lock` protects the steal operation, ensuring no race with the source CPU's scheduler.

### 15.5 BSP Idle Loop (`kernel/src/main.rs:538-559`)

The BSP uses `hlt` in its idle loop (safe under WHPX because the BSP vCPU does support `hlt`):

```rust
loop {
    unsafe { core::arch::asm!("hlt") };
    if percpu::task_count(bsp_cpu) <= 1 {
        task::steal_task(bsp_cpu);
    }
    // Log stats every ~1 second
}
```

### 15.6 Timer ISR Context Switch (`kernel/src/arch/idt.rs:589-648`)

```rust
32 => {
    super::apic::send_eoi();                        // Acknowledge interrupt
    let t = crate::percpu::tick();                  // Increment global tick
    if crate::task::is_initialized() {
        if crate::task::schedule(frame) {
            // Context switch: sti + ret to new task's RIP
            // (instead of iretq, due to WHPX bugs)
        }
    }
}
```

On an AP with only an idle task, `schedule()` returns false (no other ready task), and execution returns to `ap_sched_loop()`.

---

## 16. Common Failure Modes & Debugging

### 16.1 AP Never Reaches Trampoline

**Symptom**: No "AP GO RELEASED" diagnostic on COM1

**Possible causes**:
- UEFI MP Services not available (missing in firmware)
- `startup_this_ap` returns error (check bootloader log)
- AP count is 0 in BootInfo (firmware didn't enumerate CPUs)
- QEMU `-smp` flag not set or set to 1

**Check**: Bootloader log should show:
```
MP Services: total=2 enabled=2
MP Services: starting AP proc=1 apic_id=1 ...
```

### 16.2 AP Spins Forever on go (Never Enters Phase 2)

**Symptom**: "AP GO RELEASED" appears but no "AP CR3 SWITCHED"

**Possible causes**:
- WHPX cache coherence: ApArg writes not visible to AP's `lock xchg`
- BSP didn't call `release_aps()` (kernel panic before that point)
- `go` field address wrong (ApArg layout mismatch)
- ApArg physical page not identity-mapped (above 4 GB and not covered by kernel identity map)

**Debugging**:
1. Check kernel log for `"SMP: releasing"` messages
2. Verify ApArg physical addresses are < 4 GB
3. Check the `clflush` + `lock xchg` sequence is working
4. The AP eventually times out the `lock xchg`? (It doesn't — it spins forever)

### 16.3 AP Triple-Faults During Phase 2 (CR3/GDT/IDT Switch)

**Symptom**: QEMU reports "CPU Reset" or "Triple fault" for CPU N

**Possible causes**:
- Stack switch before CR3: UEFI stack was above 4 GB, kernel identity map doesn't cover it
- GDT pointer: non-canonical address, points to unmapped memory
- IDT pointer: same issue
- Per-CPU GDT has zero TSS descriptor → `ltr` later would #GP

**Debugging**: Check which checkpoint appears:
- "AP CR3 SWITCHED" → CR3 switch succeeded, fault is in GDT/IDT load
- No "AP CR3 SWITCHED" → fault is in `mov cr3` itself (bad page table address)

### 16.4 AP Reaches ap_entry() Then Triple-Faults

**Symptom**: "AP ENTRY REACHED" appears, then QEMU CPU reset

**Possible causes**:
- LAPIC MMIO not accessible: reads from `0xFEE00020` #PF
- Logger uses globals not yet initialized: `log::info!()` call faults
- Stack overflow: per-CPU stack too small (16 KiB)
- `fninit` or `mov cr4` causes #GP (CR4 bits not supported)
- `wrmsr` for GS base faults (non-canonical address)

**Debugging**: The diagnostic "AP ENTRY REACHED" uses raw COM1 writes. The next operation that fails is:
1. Reading LAPIC ID from MMIO (line 83) — check `LAPIC_BASE` value
2. `log::info!("AP[lapic={}]")` — the logger calls `serial::write_str` which uses COM1. If serial wasn't initialized, this is fine (COM1 is initialized by the BSP and shared).
3. FPU init (`fninit`, `mov cr4`) — check if CPU supports SSE/XSAVE
4. `wrmsr(IA32_GS_BASE)` — verify `&PERCPU[slot]` is a canonical address

### 16.5 AP Enters Scheduler But Never Runs Tasks

**Symptom**: "AP[lapic=X] entered ap_entry" but no task execution logs from that AP

**Possible causes**:
- `kernel_ready` never set (`release_all_aps()` not called or called too late)
- AP stuck in `wait_for_kernel_ready` spin loop
- LAPIC timer not firing (timer setup failed, no vector 32 delivery)
- `ltr` not executed → first interrupt #GP because TR is invalid
- Work stealing never succeeds: all other CPUs have only 1 task

**Debugging**:
1. Check global tick counter: does it increment? If no, LAPIC timer isn't firing
2. Check `timer_fires` per-CPU: does the AP's counter increment?
3. Check `task_count` per-CPU: is the AP's count > 1?
4. Enable verbose scheduler logging to see `steal_task` calls

### 16.6 WHPX-Specific: Unexpected VP Exit Code 4

**Symptom**: QEMU error log shows multiple CPU resets with this error

**Causes**:
- INIT-SIPI-SIPI sequence (sending IPIs from LAPIC ICR)
- `hlt` instruction from AP vCPUs

**Fixes**:
- Use UEFI MP Services path instead of INIT-SIPI-SIPI (already done in `release_aps`)
- Use `pause`-based loops instead of `hlt` on APs (done in `ap_sched_loop`)

### 16.7 AP #DF on First Interrupt

**Symptom**: "EXCEPTION #8 DOUBLE FAULT" followed by halt

**Causes**:
- `ltr` not executed → TR is 0 or points to UEFI TSS
- IST1 stack pointer in TSS is invalid (non-canonical, unmapped, or below 4 KB from top)
- Per-CPU TSS not pre-initialized (zero TSS descriptor in GDT)

**Fix**: Ensure `init_tss_descriptor_for_slot()` is called for the AP's slot before `release_aps()`, and `ltr` is executed in `ap_entry` before `sti`.

### 16.8 AP Reads Stale Target Fields

**Symptom**: AP jumps to garbage address, executes garbage code

**Causes**:
- WHPX L1 cache coherence: `target_pml4_phys`, `target_gdt_ptr`, etc. not flushed from BSP's cache
- AP reads stale values from its Phase 2 `mov` instructions

**Fix**: `clflush` + `mfence` on every 64-byte cache line of ApArg, plus `lock xchg` protocol.

---

## 17. Memory Map for APs

### 17.1 Physical Memory (Below 4 GB)

```
Address           Content                          Owner
────────────────  ───────────────────────────────  ─────────────────────
0x00000000         Reserved (null guard)           Kernel buddy skip
0x00001000         BootInfo pointer (8 bytes)      Kernel buddy skip
0x00001000+        BootInfo struct (dynamically    Kernel buddy skip
                   allocated, ~2 KB)
0x00008000–0x00008FFF  SIPI trampoline page        Reserved by kernel
0x00100000+        Kernel ELF (.text, .rodata,     Kernel binary
                   .data, .bss)
0x01000000+        Free memory (buddy managed)     Kernel buddy allocator
0x0F000000+        Potential AP ApArg pages        Reserved by kernel
0x0F000000+        Potential AP kernel stacks      Reserved by kernel
                   (16 KB each)
0xFEC00000–0xFEC01FFF  IOAPIC MMIO                Cache-disabled, higher-half
0xFEE00000–0xFEE00FFF  LAPIC MMIO                 Cache-disabled, higher-half
```

### 17.2 Virtual Memory (Higher Half)

```
Address               Content                           Access
────────────────────  ────────────────────────────────  ──────────
0xFFFF8000_0000_0000  Higher-half base (kernel code)    Code
0xFFFF8000_0000_0000 + phys: All physical memory        Data (NX)
0xFFFF8000_FEC0_XXXX  IOAPIC MMIO (higher-half alias)  MMIO
0xFFFF8000_FEE0_XXXX  LAPIC MMIO (higher-half alias)   MMIO
0xFFFF8080_0000_0000  Kernel heap base (64 MB VMA)     Heap
```

After AP's `mov cr3, kernel_pml4`:
- All code runs from the higher-half
- Physical memory accessed as `HIGHER_HALF + phys`
- LAPIC MMIO at `0xFFFF8000_FEE0_0020` for ID register
- Identity map (PML4[0]) still active for the trampoline's final accesses

### 17.3 Per-CPU Data (Virtual)

```
PERCPU[0] at some higher-half address (dynamically placed by linker)
PERCPU[1] at PERCPU[0] + sizeof(PerCpu)
PERCPU[2] at PERCPU[0] + 2 * sizeof(PerCpu)
PERCPU[3] at PERCPU[0] + 3 * sizeof(PerCpu)
```

Each AP's `GS_BASE` points to its own slot via `wrmsr(IA32_GS_BASE, &PERCPU[slot])`.

Per-CPU GDT table (higher-half virtual):
```
GDT_TABLE[0] at &GDT_TABLE[0] (some higher-half address)
GDT_TABLE[1] at &GDT_TABLE[1]
...
```

Per-CPU IST1 stacks (higher-half virtual):
```
IST1_STACKS[0].0[0..16384]  →  IST1 top = &base + 16384
IST1_STACKS[1].0[0..16384]
...
```

---

## 18. Register State at Each Transition

### 18.1 At UEFI MP Trampoline Entry

| Register | Value |
|----------|-------|
| RCX | ApArg physical address |
| RIP | ap_trampoline (bootloader code) |
| RSP | UEFI temporary stack (may be > 4 GB) |
| RFLAGS | IF=1 (interrupts enabled by UEFI) |
| CR0 | PE=1, PG=1, EM=0, WP=1 |
| CR3 | UEFI page tables (identity map) |
| CR4 | PAE=1, PGE=1, OSFXSR may be set |
| EFER | LME=1, LMA=1 (long mode active) |
| CS | UEFI code selector (long mode) |
| DS/ES/FS/GS/SS | UEFI data selectors |
| TR | UEFI TSS (may be valid or stale) |

### 18.2 At Phase 2 Start (After go=1)

Same as above, except:
- RCX still holds ApArg pointer
- RFLAGS.IF = 0 (after `cli`)

### 18.3 After CR3 Switch

| Register | Value |
|----------|-------|
| CR3 | Kernel PML4 physical address |
| RSP | Per-CPU kernel stack (from ApArg.target_kernel_stack) |

All subsequent memory accesses go through the kernel's page tables.

### 18.4 After GDT Load + Retfq

| Register | Value |
|----------|-------|
| CS | 0x08 (kernel code selector) |
| DS/ES/FS/GS/SS | 0x10 (kernel data selectors) |
| GDTR | Per-CPU GDT (with per-CPU TSS descriptor) |

### 18.5 After IDT Load

| Register | Value |
|----------|-------|
| IDTR | Shared IDT (per-CPU IDTR slot, same content) |

### 18.6 At ap_entry() Entry

| Register | Value |
|----------|-------|
| RDI | ApArg physical address (SysV ABI first arg) |
| RSP | Per-CPU kernel stack (16 KiB) |
| RIP | ap_entry (kernel code) |
| CR3 | Kernel PML4 |
| CS/DS/ES/FS/GS/SS | Kernel selectors |
| GDTR | Per-CPU GDT |
| IDTR | Shared IDT |
| RFLAGS | IF=0 |

### 18.7 At ap_sched_loop() Entry

| Register | Value |
|----------|-------|
| RDI | Irrelevant (loop ignores it) |
| RSP | Per-CPU kernel stack |
| RIP | ap_sched_loop (kernel code) |
| CR3 | Kernel PML4 |
| CR4 | OSFXSR=1, OSXMMEXCPT=1, OSXSAVE=1 |
| GS_BASE | &PERCPU[slot] |
| TSC_AUX | LAPIC ID |
| TR | Per-CPU TSS with per-CPU IST1 |
| RFLAGS | IF=1 (after `sti`) |
| LAPIC | Enabled, timer firing at 1 ms |

---

## 19. Timing Expectations & Timeouts

### 19.1 UEFI MP Services Phase

| Step | Duration | Notes |
|------|----------|-------|
| `startup_this_ap` call | < 1 ms | UEFI sends SIPI, AP wakes |
| Wait for ready==1 | 0–5 s | BSP polls with 10 ms stalls, up to 500 iterations |
| Per-AP total in bootloader | < 1 s | Typically completes in < 100 ms |

### 19.2 Kernel AP Release Phase

| Step | Duration | Notes |
|------|----------|-------|
| `release_aps()` loop (per AP) | ~10 µs | Write 4 fields, clflush × 2, lock xchg |
| AP trampoline Phase 2 execution | ~1 µs | CR3 + GDT + IDT switch, far jump |
| `ap_entry()` to `wait_for_kernel_ready()` | ~10 µs | LAPIC ID read, FPU init, TLS, timer setup |
| `kernel_ready` check | 0–1 ns | Already set by BSP before AP reached this point |
| `init_idle_task()` | ~50 µs | Buddy allocation (2 pages), zero, register |
| Total AP release to idle loop | ~100 µs | Dominated by allocator and logging |

### 19.3 AP Timer ISR Latency

| Step | Duration | Notes |
|------|----------|-------|
| LAPIC timer fire | 1 ms | Configured by `ap_enable_timer()` |
| ISR entry (stub) | ~50 ns | push all GPRs |
| `irq_handler()` | ~100 ns | EOI + tick + schedule check |
| `schedule()` with 1 task | ~100 ns | Returns false immediately |
| ISR return | ~50 ns | pop all GPRs, sti+ret |
| Total per tick | ~300 ns | Negligible CPU overhead |

---

## 20. Complete AP Lifecycle Timeline

```
Time    Component       Event
────    ─────────       ─────
T+0ms   UEFI            System power-on, BSP starts at reset vector
T+10ms  UEFI            OVMF PEI → DXE → BDS phases
T+500ms UEFI            Boot device selected: ESP/EFI/BOOT/BOOTX64.EFI
T+500ms Chainloader     BOOTX64.EFI entry, serial init
T+510ms Chainloader     BootInfo allocated, stored at 0x1000
T+520ms Chainloader     Memory map + framebuffer collected
T+530ms Chainloader     Bootloader.efi loaded from ESP, start_image()
T+530ms Bootloader      Bootloader entry, serial + logger init
T+540ms Bootloader      BootInfo read from 0x1000
T+550ms Bootloader      GOP mode set, memory map re-collected
T+600ms Bootloader      kernel.elf + exrun.elf loaded from ext4
T+700ms Bootloader      RSDP captured from UEFI config table
T+710ms Bootloader      ┌─────────────────────────────────────────┐
                        │ PHASE 0: AP PLATFORM DISCOVERY         │
T+720ms Bootloader      │ MpServices protocol opened             │
T+730ms Bootloader      │ AP[0] arg/stacks allocated             │
T+731ms Bootloader      │ mp.startup_this_ap(ap_trampoline, AP0) │
T+731ms AP0             │  ── Enters trampoline:
T+731ms AP0             │     cli, ready=1, spin on go           │
T+732ms Bootloader      │ AP0 ready==1, recording                │
T+740ms Bootloader      │ ─ ─ ─ second AP (if -smp 4) ─ ─ ─ ─  │
T+750ms Bootloader      └─────────────────────────────────────────┘
T+760ms Bootloader      BootInfo written, ExitBootServices
T+770ms Bootloader      Kernel ELF loaded at 0x100000
T+771ms Bootloader      jmp _start (RDI = BootInfo*)
T+771ms Kernel          ┌─────────────────────────────────────────┐
                        │ KERNEL AP RESOURCE RESERVATION          │
T+771ms Kernel          serial + logger init
T+772ms Kernel          FPU/SSE init, memory regions extracted
T+773ms Kernel          Physical allocator init
T+774ms Kernel          ACPI/MADT parsed
T+775ms Kernel          AP pages reserved (trampoline, arg, stacks)
T+776ms Kernel          Page tables initialized, CR3 switched
T+778ms Kernel          Heap + VMA init
T+779ms Kernel          cli, mask PIC
T+780ms Kernel          LAPIC MMIO mapped, IOAPICs initialized
T+785ms Kernel          IRQ routing table built
T+790ms Kernel          ┌─────────────────────────────────────────┐
                        │ BSP PER-CPU SETUP                       │
T+791ms Kernel          │ GDT + TSS loaded for BSP slot          │
T+792ms Kernel          │ IDT initialized (256 vectors)          │
T+793ms Kernel          │ BSP marked online, GS base installed   │
T+795ms Kernel          │ Task system: idle task created         │
T+796ms Kernel          │ Test tasks 1+2 created                 │
T+798ms Kernel          │ ExRun spawned (if exrun.elf present)   │
T+800ms Kernel          │ IOAPIC routes installed (masked)       │
T+801ms Kernel          │ LAPIC enabled                          │
T+802ms Kernel          │ LAPIC timer calibrated (20 ms window)  │
T+803ms Kernel          │ LAPIC timer configured (1 ms periodic) │
T+804ms Kernel          └─────────────────────────────────────────┘
T+804ms Kernel          ┌─────────────────────────────────────────┐
                        │ PHASE 4: AP RELEASE                     │
T+805ms Kernel          │ release_aps():                          │
T+805ms Kernel          │  Pre-init TSS for AP0's slot           │
T+806ms Kernel          │  PML4/GDT/IDT/entry → ApArg.target_*   │
T+806ms Kernel          │  clflush + mfence (ApArg)              │
T+806ms Kernel          │  lock xchg ApArg.go = 1                │
T+806ms Kernel          │  clflush + mfence (go field)           │
T+806ms Kernel          │  AP0 released!                         │
                         │
T+806ms AP0             │  ── lock xchg reads go=1
T+806ms AP0             │  ── Phase 2: mov rsp, kernel_stack
T+806ms AP0             │  ── mov cr3, kernel_pml4
T+806ms AP0             │  ── lgdt per-CPU GDT, retfq (CS=0x08)
T+806ms AP0             │  ── lidt shared IDT
T+806ms AP0             │  ── jmp ap_entry()
T+806ms AP0             │  ┌─────────────────────────────────────┐
                         │  │ PHASE 6: ap_entry()                 │
T+806ms AP0             │  │ "AP ENTRY REACHED" → COM1           │
T+806ms AP0             │  │ Read LAPIC ID from MMIO             │
T+807ms AP0             │  │ FPU/SSE init (fninit, CR4 bits)     │
T+807ms AP0             │  │ mark_online(apic_id)                │
T+807ms AP0             │  │ install_gs_base() (GS + TSC_AUX)    │
T+808ms AP0             │  │ ap_enable_timer()                   │
T+808ms AP0             │  │ wait_for_kernel_ready() → immediate │
T+808ms AP0             │  │ init_idle_task()                    │
T+809ms AP0             │  │ ltr (per-CPU TSS, selector 0x28)    │
T+809ms AP0             │  │ sti (LAPIC timer starts firing)     │
T+809ms AP0             │  │ ap_sched_loop() — never returns     │
T+809ms AP0             │  └─────────────────────────────────────┘
                         │
T+810ms Kernel          │ release_aps() done
T+810ms Kernel          │ release_all_aps() → kernel_ready=true
T+811ms Kernel          │ sti (BSP interrupts enabled)
T+812ms Kernel          │ int 32 test + unmask PIT/keyboard
                         │
T+812ms BSP+Kernel      ┌─────────────────────────────────────────┐
                        │ NORMAL OPERATION                        │
T+813ms BSP             │ Idle loop: hlt, wake on timer
T+813ms AP0             │ Idle loop: pause × 1000, check steal
T+814ms AP0             │ ─ ─ steal_task(): may acquire work ─ ─
                         │
T+1000ms BSP/Kernel     │ "LodaxOS initialization complete"
                         │
...                     │ System running:
                        │  • LAPIC timer fires every 1 ms on all CPUs
                        │  • BSP: hlt → timer ISR → schedule → test tasks
                        │  • AP0: pause spin → timer ISR → schedule → stolen tasks
                        │  • PIT fires at 100 Hz on BSP
                        │  • Keyboard IRQs on key press
```

---

## Appendix A: Key File Reference

| File | Purpose |
|------|---------|
| `boot/src/mp.rs` | UEFI MP Services AP bring-up, AP trampoline (naked asm) |
| `kernel/src/ap_start.rs` | `ap_entry()`, `release_aps()`, `TRAMPOLINE` bytecode, `send_init_sipi()` |
| `kernel/src/percpu.rs` | `PerCpu` struct, `PERCPU` array, `mark_online()`, `install_gs_base()`, `ReadyQueue` |
| `kernel/src/arch/gdt.rs` | Per-CPU GDT/TSS, `init_for_slot()`, `init_tss_descriptor_for_slot()`, IST1 stacks |
| `kernel/src/arch/idt.rs` | Shared IDT, `IDTR_TABLE`, `irq_handler()`, scheduler context switch |
| `kernel/src/arch/apic.rs` | LAPIC MMIO, `ap_enable_timer()`, `send_init_ipi()`, `send_sipi()` |
| `kernel/src/arch/ioapic.rs` | IOAPIC programming, redirection entries |
| `kernel/src/intr/mod.rs` | IRQ routing table, vector allocation |
| `kernel/src/task.rs` | Task manager, `init_idle_task()`, `schedule()`, `steal_task()` |
| `system/src/lib.rs` | `ApArg`, `BootInfo`, `BootInfo SMP fields`, `MAX_CPUS` |
| `kernel/src/main.rs` | `_start()` — the complete init sequence including AP phases |
| `kernel/src/mm/phys.rs` | `reserve_range()` — protects AP pages from buddy allocator |
| `kernel/src/sync.rs` | `IrqSaveSpinLock` — used for all SMP-safe critical sections |

## Appendix B: Compile-Time Layout Guards

```rust
// ApArg layout (system/src/lib.rs:89-91):
const _: [(); 1] = [(); (core::mem::offset_of!(ApArg, go) == 0x2C) as usize];
const _: [(); 1] = [(); (core::mem::offset_of!(ApArg, lapic_id) == 0x30) as usize];
const _: [(); 1] = [(); (core::mem::size_of::<ApArg>() == 0x38) as usize];
```

These guards ensure the inline assembly in:
- `boot/src/mp.rs` `ap_trampoline` (offset 0x28 for ready, 0x2C for go)
- `kernel/src/ap_start.rs` `release_aps` (offset 0x2C for go)

matches the Rust struct layout. If any field is added, removed, or reordered, the build fails here.

## Appendix C: Key Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `MAX_CPUS` | 4 | Maximum CPUs in the system (PERCPU slots, BootInfo arrays) |
| `MAX_IOAPICS` | 16 | Maximum IOAPIC controllers |
| `MAX_ISOS` | 32 | Maximum interrupt source overrides |
| `AP_STACK_PAGES` | 4 | Per-AP kernel stack = 16 KiB |
| `TRAMPOLINE_PHYS` | 0x8000 | Physical address of the SIPI real-mode trampoline |
| `PER_CPU_STACK_PAGES` | 4 | Per-CPU dummy stack for TSS.rsp0 = 16 KiB |
| `AlignedIstStack` | 16,384 B | Per-CPU IST1 double-fault stack size |
| `IA32_GS_BASE` | 0xC000_0102 | MSR for per-CPU TLS base |
| `IA32_KERNEL_GS_BASE` | 0xC000_0101 | MSR for swapgs target |
| `IA32_TSC_AUX` | 0xC000_0103 | MSR for rdtscp-based LAPIC ID |
| `APIC_LVT_TIMER` | 0x320 | LAPIC timer LVT offset |
| `APIC_ICR_LOW` | 0x300 | Interrupt Command Register (low) |
| `APIC_ICR_HIGH` | 0x310 | Interrupt Command Register (high) |
| `ICR_INIT` | 5 << 8 | INIT IPI delivery mode |
| `ICR_STARTUP` | 6 << 8 | SIPI delivery mode |
| `IPI_VECTOR` | 0x81 | Vector used for cross-CPU IPIs (TLB shootdown, wake) |

## Appendix D: QEMU Command Line

From `run.bat`:
```
qemu-system-x86_64.exe
  -drive if=pflash,format=raw,readonly=on,file="edk2-x86_64-code.fd"  (OVMF)
  -drive file=disk.img,format=raw,if=ide                                (disk)
  -serial stdio                                                          (COM1)
  -accel whpx                                                            (WHPX)
  -m 512M                                                                (512 MB)
  -smp 4                                                                 (4 CPUs total)
  -d int,cpu_reset                                                       (log interrupts + resets)
  -D qemu_smp.log                                                        (log file)
```

The `-d int,cpu_reset` flag is critical for debugging AP startup — it logs all CPU resets (triple faults) and interrupts to `qemu_smp.log`.

## Appendix E: Debug Checklist

When an AP fails to start:

1. [ ] Check `-smp N` in QEMU command (N ≥ 2)
2. [ ] Check bootloader log for "MP Services: total=..." (UEFI MP Services found?)
3. [ ] Check bootloader log for "starting AP proc=..." (AP startup attempted?)
4. [ ] Check bootloader log for "ready" (AP reached trampoline?)
5. [ ] Check kernel log for "SIPI trampoline page" reservation (pages reserved?)
6. [ ] Check kernel log for "SMP: releasing" (release_aps called?)
7. [ ] Check kernel log for "go=1" (go flag written?)
8. [ ] Check kernel log for "SMP: AP handoff go=1 written"
9. [ ] Check COM1 for "AP ENTRY REACHED" (ap_entry reached?)
10. [ ] Check COM1 for "AP[lapic=...]" (ap_entry past diagnostics?)
11. [ ] Check COM1 for "percpu: CPU ... online" (mark_online called?)
12. [ ] Check COM1 for timer logs from AP CPU
13. [ ] Check `qemu_smp.log` for "CPU Reset" or "Triple fault" entries
14. [ ] If using WHPX, verify no INIT-SIPI-SIPI attempted (check log for "skipping")
15. [ ] Verify ApArg physical addresses < 4 GB (identity map coverage)

---

*End of APbootupdetails.md — comprehensive documentation of Application Processor startup in LodaxOS.*
