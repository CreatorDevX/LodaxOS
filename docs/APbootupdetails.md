# LodaxOS AP (Application Processor) Boot — Complete Deep Dive

## Table of Contents
1. [Overview & Architecture](#1-overview--architecture)
2. [Data Structures](#2-data-structures)
3. [Phase 0: Platform Discovery (Bootloader)](#3-phase-0-platform-discovery-bootloader)
4. [Phase 1: Kernel AP Resource Reservation](#4-phase-1-kernel-ap-resource-reservation)
5. [Phase 2: SIPI Trampoline Preparation](#5-phase-2-sipi-trampoline-preparation)
6. [Phase 3: INIT-SIPI-SIPI Sequence](#6-phase-3-init-sipi-sipi-sequence)
7. [Phase 4: SIPI Trampoline Execution](#7-phase-4-sipi-trampoline-execution)
8. [Phase 5: ap_entry() Kernel Init](#8-phase-5-ap_entry-kernel-init)
9. [Phase 6: Per-CPU Scheduling Loop](#9-phase-6-per-cpu-scheduling-loop)
10. [UEFI MP Services Path (Enumeration Only)](#10-uefi-mp-services-path-enumeration-only)
11. [Per-CPU Infrastructure](#11-per-cpu-infrastructure)
12. [WHPX-Specific Workarounds](#12-whpx-specific-workarounds)
13. [Synchronization](#13-synchronization)
14. [Memory Map for APs](#14-memory-map-for-aps)
15. [Failure Modes](#15-failure-modes)

---

## 1. Overview & Architecture

### 1.1 What is an AP?

An **Application Processor** (AP) is any CPU core other than the **Bootstrap Processor** (BSP). The BSP runs UEFI firmware, chainloader, bootloader, and enters the kernel first. APs are brought up later by the BSP kernel.

### 1.2 AP Startup Flow

LodaxOS uses **INIT-SIPI-SIPI** as the active AP startup mechanism:

| Path | Status | Mechanism |
|------|--------|-----------|
| **INIT-SIPI-SIPI** | **ACTIVE** | BSP kernel broadcasts INIT to all APs → 10 ms → broadcasts SIPI → 1 ms → broadcasts second SIPI via LAPIC ICR. AP starts at real-mode trampoline (0x8000), transitions to long mode, enters `ap_entry()`. |
| **UEFI MP Services** | **Enumeration only** | Bootloader uses `StartupThisAP` only to count CPUs and collect APIC IDs. No AP startup via UEFI. |

### 1.3 High-Level Flow

```
UEFI Firmware Boot
  │
  ├── Chainloader (BSP)
  ├── Bootloader (BSP)
  │   └── MP Services enumeration: count CPUs, get APIC IDs → BootInfo
  │
  └── Kernel starts on BSP
      ├── Reserve trampoline page at 0x8000
      ├── AP pages reserved (mailbox region)
      ├── ... (full kernel init: serial, mm, heap, acpi, gdt, idt, lapic, ioapic, task) ...
      ├── arch::smp::smp_boot_aps():
      │   ├── Write all mailbox slots at 0x8400+
      │   ├── clflush per-AP mailbox data
      │   ├── Broadcast INIT IPI → pause busy-wait 10 ms
      │   ├── Broadcast SIPI (vector 0x08) → pause busy-wait 1 ms
      │   ├── Broadcast second SIPI
      │   └── Poll per-AP status bytes until all online
      ├── release_all_aps() (kernel_ready=true)
      ├── sti
      │
      └── AP enters ap_entry():
          ├── Raw COM1: "AP ENTRY REACHED"
          ├── Read LAPIC ID
          ├── FPU/SSE init
          ├── mark_online(), install_gs_base()
          ├── Enable LAPIC timer
          ├── wait_for_kernel_ready()
          ├── init_idle_task()
          ├── ltr
          ├── sti
          └── ap_sched_loop()
```

---

## 2. Data Structures

### 2.1 Mailbox (Trampoline Page at 0x8000)

The SIPI trampoline occupies physical pages `0x8000–0x8FFF`. Mailbox slots for per-AP data start at `0x8400` (`MAILBOX_OFF = 0x400`):

| Offset | Field | Size | Written by | Read by | Purpose |
|--------|-------|------|------------|---------|---------|
| `0x000–0x3FF` | Trampoline binary | 1 KB | Kernel | AP | Real-mode → long mode code |
| `0x400+` | AP mailbox slots | 0.75 KB | Kernel | AP | Per-AP boot data (slot-based) |

Each per-AP slot occupies `0x80` bytes at `MAILBOX_OFF + slot * 0x80`:

| Slot offset | Field | Size | Purpose |
|-------------|-------|------|---------|
| `0x40` | Stack top | 8 B | Per-AP RSP |
| `0x48` | GDT limit | 2 B | For `lgdt` |
| `0x50` | GDT base | 8 B | For `lgdt` |
| `0x58` | IDT limit | 2 B | For `lidt` |
| `0x60` | IDT base | 8 B | For `lidt` |
| `0x68` | Entry address | 8 B | Jump target (`ap_entry`) |
| `0x70` | Status byte | 1 B | `CPUSTARTED` flag |
| `0x78` | PML4 phys | 8 B | Kernel PML4 physical address |

### 2.2 BootInfo SMP Fields (`system/src/lib.rs`)

```rust
pub struct BootInfo {
    // ...
    pub max_cpus: u32,               // MAX_CPUS (currently 4)
    pub bsp_apic_id: u32,           // BSP LAPIC ID
    pub ap_count: u32,              // Number of APs
    pub ap_apic_ids: [u32; MAX_CPUS], // LAPIC IDs [index 0..ap_count]
}
```

Populated by the bootloader via UEFI MP Services enumeration. No `ap_arg_phys` or `ap_trampoline_phys` fields — the kernel manages all AP boot data internally.

### 2.3 PerCpu (`kernel/src/percpu.rs`)

```rust
pub struct PerCpu {
    pub apic_id: AtomicU32,
    pub online: AtomicBool,
    pub kernel_ready: AtomicBool,
    pub kernel_stack_top: AtomicU64,
    pub ticks: AtomicU64,
    pub current_task: AtomicUsize,
    pub task_count: AtomicUsize,
    pub ready_queue: ReadyQueue,
    pub self_ptr: AtomicU64,
    pub need_resched: AtomicBool,
    pub timer_fires: AtomicU64,
}
```

One slot per LAPIC ID modulo `MAX_CPUS` (4). Indexed by `apic_id % MAX_CPUS`.

### 2.4 Per-CPU GDT/TSS/IDT

Each CPU has its own:
- **GDT**: 7 entries (null, kernel code, kernel data, user code, user data, TSS low, TSS high)
- **TSS**: 104-byte Task State Segment with per-CPU IST1 stack (16 KiB)
- **IST1 stack**: 16 KiB for double-fault recovery
- **GDT pointer**: `GdtPtr` struct for `lgdt`
- **IDTR slot**: `Idtr` struct — all point to the **same shared IDT** (256 entries × 16 B)

### 2.5 ReadyQueue (`kernel/src/percpu.rs`)

Lock-free per-CPU circular buffer of task IDs:

```rust
pub struct ReadyQueue {
    pub buf: [AtomicUsize; MAX_TASKS],
    pub head: AtomicUsize,
    pub tail: AtomicUsize,
    pub count: AtomicUsize,
}
```

Single-producer (timer ISR), single-consumer (schedule). Cross-CPU operations use the global task table lock.

---

## 3. Phase 0: Platform Discovery (Bootloader)

**Source:** `boot/src/mp.rs`

### 3.1 MP Services Enumeration

The bootloader opens UEFI MP Services protocol to **enumerate** CPUs only:

```rust
let mp = uefi::boot::open_protocol_exclusive::<MpServices>(mp_handle)?;
let count = mp.get_number_of_processors()?;
```

For each processor index:
1. Get processor info via `get_processor_info()`
2. Filter: skip if not enabled, not healthy, or is BSP
3. Cap at `MAX_CPUS - 1` (3 APs)
4. Record LAPIC ID in `boot_info.ap_apic_ids[ap_index]`

NO `StartupThisAP` is called — the kernel handles all AP booting.

### 3.2 BootInfo Recording

```rust
boot_info.ap_apic_ids[ap_index] = info.processor_id as u32;
boot_info.ap_count = ap_index;
boot_info.max_cpus = ap_index + 1;
boot_info.bsp_apic_id = bsp_id;
```

---

## 4. Phase 1: Kernel AP Resource Reservation

**Source:** `kernel/src/main.rs` (init sequence)

Before page table initialization, the kernel reserves AP-related pages:

1. **SIPI trampoline page** at `0x8000–0x8FFF` — `phys::reserve_range(0x8000, 1)`

This must happen before `virt::init()` because page table init allocates ~1600 pages from the buddy allocator.

---

## 5. Phase 2: SIPI Trampoline Preparation

**Source:** `kernel/src/arch/smp.rs`

### 5.1 Trampoline Code

A pre-compiled machine code array is copied to `0x8000`. The trampoline executes in:

1. **Real mode (0x8000)**: `cli`, set up segments, enable A20 via port `0x92`, `lgdt` with transition GDT at `0x8F80`, enable protected mode (`CR0.PE=1`), far jump to `0x8100`

2. **Protected mode (0x8100)**: Set data segments, enable PAE (`CR4.PAE=1`), load CR3 with kernel PML4 from mailbox slot (offset `0x78`), enable long mode (`EFER.LME=1`), enable paging (`CR0.PG=1`), far jump to `0x8200`

3. **Long mode (0x8200)**: Load kernel GDT from slot offset `0x50`, load kernel IDT from slot offset `0x60`, load per-CPU stack from slot offset `0x40`, read LAPIC ID from higher-half `0xFFFF8000FEE00020`, load `ap_entry` address from slot offset `0x68`, jump to it

### 5.2 Mailbox Initialization

Before sending SIPI, the BSP writes each AP's mailbox fields:

```rust
for i in 0..ap_count {
    let apic_id = boot_info.ap_apic_ids[i];
    let slot = apic_id as usize % MAX_CPUS;

    // Write per-AP mailbox at trampoline_page + 0x400 + slot * 0x80
    write_per_ap_mailbox(slot, pml4_phys, gdt_limit, gdt_base,
                         idt_limit, idt_base, stack_top,
                         ap_entry_addr);

    // clflush every cache line of this AP's mailbox region
    // mfence after each clflush
}
```

The `clflush` + `mfence` protocol is critical for WHPX, which can cache stores in the BSP's L1 without making them visible to AP reads.

---

## 6. Phase 3: INIT-SIPI-SIPI Sequence

**Source:** `kernel/src/arch/smp.rs:smp_boot_aps()`

### 6.1 Broadcast Startup (Not Per-AP)

The BSP does NOT iterate per AP. Instead it broadcasts INIT and SIPI to **all APs simultaneously**:

```rust
// 1. Send INIT IPI to all APs (excluding self)
apic::send_init_ipi_all();
// ICR: destination shorthand = all-excluding-self, INIT + ASSERT

// 2. Wait ~10 ms (pause-based busy-wait, not PIT Mode 0)
delay_ms(10);
// Simple loop: pause instruction × calibrated count

// 3. Broadcast SIPI #1 to all APs
apic::send_sipi_all(0x08);  // vector 0x08 → startup at 0x8000

// 4. Wait ~1 ms
delay_ms(1);

// 5. Broadcast SIPI #2 (some CPUs miss the first)
apic::send_sipi_all(0x08);

// 6. Poll each AP's status byte until CPUSTARTED or timeout
```

### 6.2 Busy-Wait Timing

The delays are implemented as simple `pause`-based spin loops (`delay_ms()`), NOT PIT Mode 0 reprogramming. The PIT remains in Mode 2 (rate generator) at 100 Hz throughout AP boot. This avoids the complexity of temporarily reprogramming and restoring the PIT.

### 6.3 LAPIC ICR Programming

**INIT IPI:**
```rust
write32(APIC_ICR_HIGH, dest_apic_id << 24);
write32(APIC_ICR_LOW, ICR_INIT | ICR_ASSERT);
// Poll until delivery status clear
```

**SIPI:**
```rust
write32(APIC_ICR_HIGH, dest_apic_id << 24);
write32(APIC_ICR_LOW, vector | ICR_STARTUP | ICR_ASSERT);
// Poll until delivery status clear
```

### 6.4 Synchronization

The BSP polls the per-AP status byte in the trampoline mailbox. Each AP sets its status to `CPUSTARTED` when it reaches `ap_entry()`. If any AP fails to respond within the timeout, the BSP logs a warning and continues.

---

## 7. Phase 4: SIPI Trampoline Execution

When the AP receives SIPI with vector `0x08`:
- CPU reset to real mode, starts executing at `0x8000`
- Trampoline performs: real → protected (PAE) → long mode transition
- Reads mailbox: PML4, GDT, IDT, stack top, entry point
- Loads kernel PML4 into CR3
- Loads kernel GDT via `lgdt`, reloads CS via `retfq`
- Loads kernel IDT via `lidt`
- Switches RSP to per-CPU kernel stack (16 KiB)
- Reads LAPIC ID from MMIO (`0xFFFF8000FEE00020`)
- Jumps to `ap_entry()`

### 7.1 Register State After Trampoline

| Register | Value |
|----------|-------|
| RDI | ap_entry arg (unused) |
| CS | 0x08 (kernel code) |
| DS/ES/FS/GS/SS | 0x10 (kernel data) |
| RSP | Per-CPU kernel stack (16 KiB) |
| CR3 | Kernel PML4 physical address |
| GDTR | Per-CPU GDT (with per-CPU TSS descriptor) |
| IDTR | Shared IDT (per-CPU IDTR slot) |
| RFLAGS.IF | 0 (interrupts disabled) |

---

## 8. Phase 5: ap_entry() Kernel Init

**Source:** `kernel/src/ap_start.rs:ap_entry()`

### 8.1 Entry

```rust
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry() -> !
```

Called with no arguments (RDI ignored). Never returns.

### 8.2 Init Sequence

1. **Raw COM1 diagnostic**: `for &byte in b"AP ENTRY REACHED\r\n" { ... }` — proves the AP reached kernel code with a working stack.

2. **Read LAPIC ID**: Reads `0xFEE00020` (LAPIC ID register via identity map), shifts right 24 bits.

3. **FPU/SSE init**:
   ```rust
   asm!("fninit");
   cr4 |= 1<<9 | 1<<10 | 1<<18;  // OSFXSR, OSXMMEXCPT, OSXSAVE
   ```

4. **mark_online(apic_id)**: Sets `PERCPU[slot].online = true`, logs "percpu: CPU N online".

5. **install_gs_base(slot)**: Writes three MSRs:
   - `IA32_GS_BASE` = `&PERCPU[slot]`
   - `IA32_KERNEL_GS_BASE` = same
   - `IA32_TSC_AUX` = `apic_id` (enables `rdtscp`-based fast LAPIC ID)

6. **ap_enable_timer()**: Enables per-CPU LAPIC timer:
   - Mask LINT0/LINT1, mask error LVT
   - Set SVR enable bit
   - Program timer: vector 32, periodic, divider 16
   - Write `ticks_per_ms * 1` to TICR (1 ms period)

7. **wait_for_kernel_ready()**: Spin on `PERCPU[slot].kernel_ready` until BSP sets it.

8. **init_idle_task()**: Allocates 2 pages (8 KB) for per-CPU idle task stack, zeroes it, builds synthetic `TrapFrame` with current RSP, registers as Ready.

9. **ltr**: `asm!("ltr ax", in("ax") 0x28u16)` — loads per-CPU TSS with IST1 and RSP0. Without this, the first interrupt would #GP.

10. **sti**: Enables interrupts (LAPIC timer starts firing at 1 ms).

11. **ap_sched_loop(apic_id)**: Enters the per-CPU scheduling loop (never returns).

---

## 9. Phase 6: Per-CPU Scheduling Loop

**Source:** `kernel/src/ap_start.rs:ap_sched_loop()`

```rust
fn ap_sched_loop(apic_id: u32) -> ! {
    // Brief pause for BSP to finish boot
    for _ in 0..100_000 { pause(); }
    let mut count = 0u64;
    loop {
        for _ in 0..1000 { pause(); }
        count += 1;
        if count % 100 == 0 {
            let cpu = apic_id as usize;
            if percpu::task_count(cpu) <= 1 {
                crate::task::steal_task(cpu);
            }
        }
    }
}
```

**Why `pause` instead of `hlt`?** WHPX does not handle `hlt` from AP vCPUs (reports "Unexpected VP exit code 4").

**How APs get work:**
- Initially each AP has only its idle task
- Timer ISR fires every 1 ms, calls `task::schedule()` — finds only idle task, returns false
- Every ~100 million pause cycles, calls `steal_task()` — looks for a CPU with ≥2 tasks, moves one

**Timer ISR context switch:**
Instead of `iretq` (which WHPX mishandles for CS=0x08), the scheduler uses:
```asm
mov rsp, {new_rsp}
sti
push {new_rip}
ret
```

---

## 10. UEFI MP Services Path (Enumeration Only)

The UEFI MP Services protocol is used **only for CPU enumeration**, not AP startup:

1. `GetNumberOfProcessors()` → get total/enabled CPU count
2. `GetProcessorInfo()` per CPU → get LAPIC ID, BSP/AP status
3. No `StartupThisAP` call
4. APIC IDs are stored in `BootInfo.ap_apic_ids` for the kernel's INIT-SIPI-SIPI path

The INIT-SIPI-SIPI code path is the sole active AP boot mechanism. The old UEFI MP trampoline path (with `ApArg` struct, `go`/`ready` flags, `target_*` fields) has been removed.

The INIT-SIPI-SIPI path is fully self-contained within the kernel:
- No dependency on UEFI boot services (which are gone by this point)
- Works with WHPX (though AP `hlt` is avoided in the scheduling loop)
- Works with TCG, KVM, and bare metal

---

## 11. Per-CPU Infrastructure

### 11.1 PERCPU Array

```rust
pub static PERCPU: [PerCpu; MAX_CPUS] = ...;  // MAX_CPUS = 4
```

Indexed by `apic_id % MAX_CPUS`. Each slot is accessed via `GS_BASE` after `install_gs_base()`.

### 11.2 Per-CPU GDT

```
[0] null:          0x0000000000000000
[1] kernel_code:   make_descriptor(0, 0xFFFFF, 0x9A, 0xA)  → sel 0x08
[2] kernel_data:   make_descriptor(0, 0xFFFFF, 0x92, 0xA)  → sel 0x10
[3] user_code:     make_descriptor(0, 0xFFFFF, 0xFA, 0xA)  → sel 0x18
[4] user_data:     make_descriptor(0, 0xFFFFF, 0xF2, 0xA)  → sel 0x20
[5] tss_low:       per-CPU TSS base [15:0] + access + base[31:24]
[6] tss_high:      per-CPU TSS base [63:32]
```

### 11.3 Per-CPU TSS

```rust
struct Tss {
    reserved0: u32,
    rsp0: u64,         // Updated by scheduler on context switch
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist1: u64,         // Per-CPU double-fault stack (16 KiB)
    ist2..ist7: u64,
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,   // = 104 (no I/O bitmap)
}
```

### 11.4 LAPIC ID Access Methods

| Method | Source | Speed | When Available |
|--------|--------|-------|---------------|
| `rdtscp` (TSC_AUX) | MSR `0xC000_0103` | ~10 cycles | After `install_gs_base()` |
| LAPIC MMIO register | `0xFEE00020` | ~100 cycles | After page table init |
| BootInfo array | Memory | Fast | Always (cached) |

The trampoline reads LAPIC ID from MMIO (higher-half). After `install_gs_base()`, the `rdtscp` path is used.

---

## 12. WHPX-Specific Workarounds

### 12.1 INIT-SIPI-SIPI Support

Unlike the old UEFI MP path, INIT-SIPI-SIPI **does work** under WHPX for AP startup. The limitation is:
- `hlt` from AP vCPUs causes "Unexpected VP exit code 4" — APs use `pause`-based spin loops instead
- PIT busy-wait timing is used instead of `acpi::stall` after ExitBootServices

### 12.2 clflush for Cache Coherence

WHPX can cache BSP stores in L1 without making them visible to AP reads. The protocol:
1. Write all mailbox fields (plain stores)
2. `clflush` every 64-byte cache line of the mailbox region
3. `mfence` after each clflush
4. AP reads mailbox fields from its own cache/memory (no `lock xchg` needed for INIT-SIPI-SIPI path since AP starts from reset)

### 12.3 STI + RET Instead of IRETQ

WHPX mishandles:
- `popfq` at CPL=0 (clears IF after emulation)
- `iretq` with CS=0x08 (#GP error code 0x08)

The scheduler uses `sti; push {rip}; ret` instead.

### 12.4 No INIT-SIPI-SIPI Old Path

The previous UEFI MP Services-based AP startup (with `lock xchg` for `go` flag) has been removed. The INIT-SIPI-SIPI path is the sole active mechanism.

---

## 13. Synchronization

### 13.1 AP Boot Timing

| Step | Duration | Notes |
|------|----------|-------|
| INIT IPI send (broadcast) | ~1 µs | LAPIC ICR write + poll |
| pause busy-wait (after INIT) | ~10 ms | Simple `pause` loop, no PIT reprogramming |
| SIPI #1 send (broadcast) | ~1 µs | LAPIC ICR write + poll |
| pause busy-wait (after SIPI) | ~1 ms | Simple `pause` loop |
| SIPI #2 send (broadcast) | ~1 µs | LAPIC ICR write + poll |
| Trampoline execution | ~5 µs | Real-mode + protected + long mode |
| ap_entry() to idle | ~100 µs | LAPIC ID read, FPU, mark_online, TLS, timer, idle task |
| Total per AP | ~10.5 ms | Dominated by 10 ms INIT→SIPI delay |

### 13.2 Memory Ordering

| Operation | Ordering |
|-----------|----------|
| Mailbox field writes → SIPI | Stores visible before AP starts executing (AP starts from reset, no stale cache) |
| BSP `kernel_ready=true` → AP reads | `Release`/`Acquire` semantics on `AtomicBool` |
| Per-CPU data writes → AP reads | Sequentially consistent (x86 TSO + no stale cache after reset) |

### 13.3 PIT Not Reprogrammed During AP Boot

Unlike the old per-AP path, the PIT is **not** temporarily reprogrammed to Mode 0 during AP boot. It remains in Mode 2 (rate generator) at 100 Hz throughout. The busy-wait delays use simple `pause`-based loops (`delay_ms()`), avoiding the complexity of PIT save/restore.

---

## 14. Memory Map for APs

### 14.1 Physical Memory

| Address | Content | Owner |
|---------|---------|-------|
| `0x00000000` | Null guard | Reserved |
| `0x00001000` | BootInfo pointer | Reserved |
| `0x00008000–0x00008FFF` | SIPI trampoline + mailbox | Reserved by kernel |
| `0x00100000+` | Kernel ELF segments | Kernel binary |
| `0x01000000+` | Free memory | Buddy allocator |
| `0xFEC00000+` | IOAPIC MMIO | Cache-disabled |
| `0xFEE00000+` | LAPIC MMIO | Cache-disabled |

### 14.2 Virtual Memory (After CR3 Switch)

| Address | Content |
|---------|---------|
| `0xFFFF8000_0000_0000 + phys` | All physical memory |
| `0xFFFF8000_FEE0_0020` | LAPIC ID register |
| `0xFFFF8000_FEC0_XXXX` | IOAPIC MMIO |
| `0xFFFF8080_0000_0000` | Kernel heap base |

### 14.3 Per-CPU Data

PERCPU array, per-CPU GDT tables, and per-CPU IST1 stacks are statically allocated at linker-defined addresses in the higher half. Each AP's `GS_BASE` points to `PERCPU[slot]`.

---

## 15. Failure Modes

### 15.1 AP Never Starts

**Symptom**: No "AP ENTRY REACHED" on COM1

**Causes**:
- INIT-SIPI-SIPI not supported by hypervisor (rare — TCG, KVM, WHPX all support it)
- PIT busy-wait timing too short
- LAPIC ICR not programming correctly (bad destination APIC ID)
- SIPI vector 0x08 doesn't point to valid code at 0x8000
- Trampoline page not reserved (buddy allocator reused it)

**Check**: `-d int,cpu_reset` in QEMU to see if AP receives SIPI

### 15.2 AP Triple-Faults During Trampoline

**Symptom**: QEMU "CPU Reset" for CPU N

**Causes**:
- Bad PML4 physical address in mailbox
- Stack address not mapped in kernel page tables
- GDT/IDT descriptor non-canonical
- Real-mode code at 0x8000 corrupted

### 15.3 AP Reaches ap_entry() Then Faults

**Symptom**: "AP ENTRY REACHED" but no further output

**Causes**:
- LAPIC MMIO not accessible (identity map not covering 0xFEE00000)
- `fninit` or CR4 bits cause #GP (unsupported CPU features)
- `wrmsr(IA32_GS_BASE)` with non-canonical address
- Logger globals not yet initialized (but ap_entry uses raw COM1 writes)

### 15.4 AP Spins on kernel_ready

**Symptom**: AP stuck in `wait_for_kernel_ready()`

**Cause**: `release_all_aps()` not called or called with wrong slot index.

**Fix**: `release_all_aps()` sets `kernel_ready=true` for ALL slots before `sti`.

### 15.5 AP #DF on First Interrupt

**Cause**: `ltr` not executed before `sti` — TR is 0 or stale, TSS missing.

**Fix**: Ensure `ltr` (selector 0x28) executes in `ap_entry()` before `sti`.

### 15.6 WHPX: Unexpected VP Exit Code 4

**Cause**: `hlt` from AP vCPU.

**Fix**: AP scheduling loop uses `pause` instead of `hlt`.

---

## Appendix A: Key File Reference

| File | Purpose |
|------|---------|
| `kernel/src/arch/smp.rs` | `smp_boot_aps()` — INIT-SIPI-SIPI sequence, mailbox init |
| `kernel/src/ap_start.rs` | `ap_entry()`, `ap_sched_loop()`, SIPI trampoline bytecode |
| `kernel/src/percpu.rs` | `PerCpu` struct, `PERCPU` array, `ReadyQueue`, GS base |
| `kernel/src/arch/gdt.rs` | Per-CPU GDT/TSS, `init_tss_descriptor_for_slot()`, IST1 |
| `kernel/src/arch/idt.rs` | Shared IDT, `IDTR_TABLE`, `irq_handler()`, scheduler switch |
| `kernel/src/arch/apic.rs` | LAPIC MMIO, `send_init_ipi()`, `send_sipi()`, `ap_enable_timer()` |
| `kernel/src/task.rs` | Task manager, `init_idle_task()`, `schedule()`, `steal_task()` |
| `boot/src/mp.rs` | UEFI MP Services enumeration (APIC ID collection) |
| `system/src/lib.rs` | `BootInfo`, `MAX_CPUS`, constants |
| `kernel/src/mm/phys.rs` | `reserve_range()` — protects trampoline page |

## Appendix B: Key Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `MAX_CPUS` | 4 | Maximum CPUs (PERCPU slots, BootInfo arrays) |
| `TRAMPOLINE_PHYS` | 0x8000 | SIPI trampoline physical address |
| `AP_STACK_PAGES` | 4 | Per-AP kernel stack = 16 KiB |
| `IA32_GS_BASE` | 0xC000_0102 | MSR for per-CPU TLS base |
| `IA32_TSC_AUX` | 0xC000_0103 | MSR for rdtscp-based LAPIC ID |
| `ICR_INIT` | 5 << 8 | INIT IPI delivery mode |
| `ICR_STARTUP` | 6 << 8 | SIPI delivery mode |
| `AlignedIstStack` | 16,384 B | Per-CPU IST1 double-fault stack |
