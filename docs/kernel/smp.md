# SMP (Symmetric Multiprocessing)

LodaxOS supports up to `MAX_CPUS` (defined in `lodaxos_system` crate)
logical processors. The BSP boots first; APs are brought up via the
INIT-SIPI-SIPI protocol.

---

## 1. Trampoline (`kernel/src/arch/smp_trampoline.S`)

A NASM-assembled flat binary placed at `TRAMPOLINE_PHYS = 0x8000`.
The SI PI vector is `0x08`, so the AP starts at `0x08 * 0x1000 = 0x8000`.

### Execution Flow (real-mode → long mode)

```
sipi_entry (BITS 16):
  1. cli, cld
  2. Set DS=ES=SS=0, SP = 0x8000 - 16
  3. lgdt [gdtr32]       (minimal GDT: null, 32-bit code, 32-bit data, 64-bit code)
  4. Set CR0.PE = 1
  5. Far jump 0x08:pm_entry

pm_entry (BITS 32):
  1. Set DS=ES=SS = 0x10
  2. CPUID leaf 1 → EBX[31:24] = initial APIC ID
  3. Look up slot: cl = [SLOT_MAP + apic_id]
     - Fallback if 0xFF: cl = apic_id & (MAX_CPUS - 1)
  4. slot_offset = cl * 0x80 (MAILBOX_SLOT_SIZE)
  5. Enable PAE (CR4.PAE = 1)
  6. Set EFER: LME=1, SCE=1, NXE=1
  7. Load PML4 from mailbox slot (MAILBOX_BASE + slot_offset + MB_PML4)
  8. Enable paging (CR0.PG = 1)
  9. Far jump 0x18:lm_entry

lm_entry (BITS 64):
  1. Load kernel GDT from mailbox slot (lgdt)
  2. Load kernel IDT from mailbox slot (lidt)
  3. RSP = [mailbox + MB_STACK]
  4. Far return to kernel CS (0x08)
  5. Set DS=ES=SS = 0x10
  6. Write MB_STATUS = 1
  7. JMP to [mailbox + MB_ENTRY] (= ap_entry)
```

### Trampoline GDT

```c
// Physical addresses (trampoline runs from 0x8000)
gdt32:
  0x00: null
  0x08: 32-bit code, base=0, limit=0xFFFFF, DPL=0
  0x10: 32-bit data, base=0, limit=0xFFFFF, DPL=0
  0x18: 64-bit code (L=1), base=0, limit=0xFFFFF, DPL=0
```

---

## 2. Mailbox Slots

Located at `TRAMPOLINE_PHYS + 0x400` (physical `0x8400`).
Each slot is `0x80` (128) bytes. Slots are written by the BSP before any
IPIs are sent.

### Slot Layout (must match `kernel/src/arch/smp.rs` and `smp_trampoline.S`)

```
Offset  Size  Field         Description
──────  ────  ─────         ───────────
0x00    0x40  (reserved)    space for trampoline stack
0x40    8     MB_STACK      AP kernel stack top (physical)
0x48    2     MB_GDT_LIMIT  GDT limit (u16)
0x4A    6     (padding)
0x50    8     MB_GDT_BASE   GDT base (virtual/higher-half)
0x58    2     MB_IDT_LIMIT  IDT limit (u16)
0x5A    6     (padding)
0x60    8     MB_IDT_BASE   IDT base (virtual/higher-half)
0x68    8     MB_ENTRY     AP entry function pointer
0x70    1     MB_STATUS    Status byte (0=booting, 1=ready)
0x71    7     (padding)
0x78    8     MB_PML4      PML4 physical address
```

Total: 0x80 bytes per slot. MAX_CPUS slots fit in 0x80 * MAX_CPUS bytes.

### Slot Map

At physical `SLOT_MAP_PHYS = 0x8000 + 0x300 = 0x8300`. 256 bytes, one `u8`
per APIC ID. Pre-populated by the BSP with the pre-allocated PERCPU slot
number. A value of `0xFF` means unassigned (the trampoline falls back to
`apic_id & (MAX_CPUS - 1)`).

---

## 3. SM P Init (`kernel/src/arch/smp.rs`)

### Phase Sequence

```
start_aps(boot_info, ap_stacks, ap_slots):
  Phase 0: Zero SLOT_MAP_PHYS to 0xFF
  Phase 1: For each AP:
           - Write SLOT_MAP[apic_id] = pre-allocated slot
           - Write mailbox slot (stack, GDT/LDT limit+base, entry, PML4)
           - clflush each cache line of the slot
  Phase 2: send_init_ipi_all()       (INIT IPI broadcast)
           delay_ms(10)
  Phase 3: send_sipi_all(0x08)       (first SIPI broadcast)
           delay_ms(1)
  Phase 4: send_sipi_all(0x08)       (second SIPI broadcast)
  Phase 5: Poll each AP's MB_STATUS until == 1 (timeout ~10 s)
```

### Public API

| Function | Description |
|----------|-------------|
| `smp_start_aps(boot_info, ap_stacks, ap_slots)` | Execute INIT-SIPI-SIPI sequence |
| `smp_init()` | Reserve trampoline page, load binary |
| `smp_init_for_slot(slot)` | Finalise per-CPU state |

---

## 4. Per-CPU State (`kernel/src/percpu.rs`)

### PerCpu Structure

```c
struct PerCpu {
    apic_id: AtomicU32,              // LAPIC ID
    online: AtomicBool,              // CPU is running
    kernel_ready: AtomicBool,        // BSP set: AP may enter scheduler
    kernel_stack_top: AtomicU64,     // per-CPU kernel stack top
    ticks: AtomicU64,                // per-CPU tick count
    current_task: AtomicUsize,       // legacy task ID
    current_vcpu: AtomicUsize,       // current VCPU index
    idle_vcpu_id: AtomicU32,         // idle VCPU for this CPU
    task_count: AtomicUsize,         // tasks assigned to this CPU
    ready_queue: ReadyQueue,         // circular buffer (256 entries)
    self_ptr: AtomicU64,             // GS-base verification
    need_resched: AtomicBool,        // reschedule IPI flag
    timer_fires: AtomicU64,          // rate-limited logging
    pending_tlb_flush: AtomicU64,    // TLB shootdown fallback
}
```

### APIC-ID-to-Slot Mapping

```c
pub static APIC_TO_SLOT: [AtomicU8; 256]  // fast table
pub static PERCPU: [PerCpu; MAX_CPUS]     // per-CPU array
```

`apic_id_to_slot(apic_id)`:
1. If `apic_id < 256`, look up `APIC_TO_SLOT[apic_id]`.
2. If that entry is `0xFF` (unassigned), fall back to linear search via
   `slot_for(apic_id)`.

### Per-CPU TLS

IA32_GS_BASE MSR is set to `&PERCPU[slot]`. IA32_TSC_AUX is set to the
LAPIC ID if `rdtscp` is supported.

`current_apic_id()`:
1. If `rdtscp` available: read ECX (IA32_TSC_AUX).
2. Otherwise: read LAPIC ID register via MMIO.

### ReadyQueue

Per-CPU fixed-size circular buffer of entity IDs:

```c
struct QueueInner {
    buf: [usize; 256],
    head: usize,
    tail: usize,
}
```

| Method  | Description                       |
|---------|-----------------------------------|
| `push(id)` | Enqueue (returns false if full) |
| `pop() -> Option<usize>` | Dequeue |
| `peek() -> Option<usize>` | Head without removing |

Protected by `IrqSaveSpinLock`.

### Public API (percpu.rs)

| Function | Description |
|----------|-------------|
| `percpu_init()` | Initialise PERCPU array |
| `this_cpu() -> *mut PerCpu` | Read IA32_GS_BASE |
| `current_apic_id() -> u32` | Get current LAPIC ID |
| `current_apic_id_lapic() -> u32` | LAPIC ID via MMIO |
| `apic_id_to_slot(apic_id) -> usize` | LAPIC ID → PERCPU index |
| `slot_for(apic_id) -> usize` | Find or allocate slot |
| `find_slot(apic_id) -> Option<usize>` | Match existing or claim offline |
| `mark_online(apic_id)` | Register CPU online |
| `mark_online_for_slot(apic_id, slot)` | Register at pre-allocated slot |
| `is_online(cpu) -> bool` | Check if CPU is online |
| `wait_for_kernel_ready(apic_id) -> bool` | Spin until BSP releases |
| `release_all_aps()` | Set kernel_ready for all slots |
| `install_gs_base(slot)` | Set IA32_GS_BASE + IA32_TSC_AUX |
| `rq(cpu) -> &ReadyQueue` | Get CPU's ready queue |
| `set_bsp_apic_id(id)` | Record BSP's LAPIC ID |
| `is_bsp() -> bool` | True if current CPU is BSP |
| `current_vcpu(cpu) -> usize` | Read current VCPU index |
| `set_current_vcpu(cpu, id)` | Write current VCPU index |
| `task_count(cpu) -> usize` | Tasks assigned to CPU |
| `set_task_count(cpu, count)` | Update task count |
| `set_idle_vcpu(cpu, id)` | Record idle VCPU ID |
| `idle_vcpu(cpu) -> u32` | Get idle VCPU ID |
| `find_least_loaded() -> usize` | CPU with fewest tasks |
| `tick() -> u64` | Increment global tick |
| `ticks() -> u64` | Read global tick |

---

## 5. AP Entry (`kernel/src/ap_start.rs`)

### ap_entry

Called by the trampoline from long mode. Receives no argument — LAPIC ID is
read via CPUID.

```c
pub extern "C" fn ap_entry() -> ! {
  1. sub rsp, 8                (align to 16 bytes)
  2. CPUID leaf 1 → EBX[31:24] = LAPIC ID
  3. Read pre-allocated slot from SLOT_MAP_PHYS
  4. percpu::mark_online_for_slot(apic_id, slot)
  5. percpu::install_gs_base(slot)
  6. Enable FPU/SSE/XSAVE (same as BSP)
  7. Wait for kernel_ready (timeout ~10 s)
  8. ltr 0x28                  (load per-CPU TSS)
  9. init_syscall_msrs()
  10. scheduler::init_idle_vcpu()
  11. arch::apic::ap_enable_timer(apic_id)   (physical LAPIC MMIO)
  12. sti
  13. ap_sched_loop(apic_id)    (pause-spin, steal tasks)
}
```

### AP Scheduling Loop

```c
fn ap_sched_loop(apic_id: u32) -> ! {
    loop {
        pause 1000 times
        count++
        if count % 100 == 0:
            if task_count(cpu) <= 1:
                steal_task(cpu)
    }
}
```

APs do not have a periodic timer ISR that runs the scheduler on every tick;
they run a cooperative pause loop that steals tasks from overloaded CPUs.

### BSP-Side Boot (`smp_boot_aps`)

Called from `main.rs:198`:

```c
pub fn smp_boot_aps(boot_info: &BootInfo) {
  0. Clamp count to MAX_CPUS - 1
  1. Pre-allocate PERCPU slots (serial, no race)
  2. For each AP:
     - Initialise per-CPU GDT/TSS (gdt::init_tss_descriptor_for_slot)
     - clflush GDT, TSS, IDT structures
     - Allocate kernel stack (AP_STACK_PAGES=4 pages, 16 KB)
  3. Call smp::start_aps() which sends IPIs
}
```

### Cache Line Flushing

WHPX and some hypervisors cache BSP stores to the SIPI mailbox. The BSP
calls `clflush_range()` on each mailbox slot, GDT/TSS, and IDT after writing.

```c
pub unsafe fn clflush(ptr: *const u8)     // single cache line (64 bytes)
pub unsafe fn clflush_range(ptr, len)     // range, 64-byte stride
```

Each `clflush` is followed by `mfence`.
