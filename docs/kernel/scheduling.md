# Scheduling

LodaxOS uses the **SEDS** (Semi-Egalitarian Distributed Scheduler) — a
gang-scheduling framework with vruntime-based fairness, per-CPU ready
queues, and cross-CPU load stealing.

---

## 1. VCPU Slab (`kernel/src/vcpu.rs`)

### Constants

| Constant           | Value  | Description              |
|--------------------|--------|--------------------------|
| `MAX_VCPUS`        | `128`  | Maximum VCPUs            |
| `GANG_UNSCHEDULED` | `0`    | Gang ID for unscheduled  |

### VcpuId

```rust
pub type VcpuId = u32;
```

### VcpuState

```rust
pub enum VcpuState {
    Ready,      // available to run
    Running,    // currently executing on a pCPU
    Halted,     // blocked, waiting for wake
    Blocked,    // blocked on driver mailbox
    Idle,       // idle task (hlt loop)
}
```

### VcpuType

```rust
pub enum VcpuType {
    Normal,
    HardwareDriver,
    AbstractionDriver,
    Idle,
}
```

### Vcpu

```c
struct Vcpu {
    id: VcpuId,                       // 0..127
    gang_id: u32,                     // gang membership
    vcpu_type: VcpuType,
    state: VcpuState,
    affinity: u64,                    // CPU affinity mask
    saved_frame: TrapFrame,           // full register save (0xB0 bytes)
    kernel_stack_top: u64,            // top of kernel stack
    pml4: u64,                        // PML4 physical address
    vruntime: u64,                    // per-VCPU virtual runtime
    fpu_state: FpuState,              // 512-byte FXSAVE area (16-byte aligned)
}
```

### Public API

| Function | Description |
|----------|-------------|
| `vcpu_init()` | Initialise VCPU slab (128 slots) |
| `vcpu_alloc(gang_id, pml4, affinity, type) -> Option<VcpuId>` | Allocate and initialise |
| `vcpu_free(id)` | Free a VCPU slot |
| `vcpu_get(id) -> Option<&Vcpu>` | Immutable reference |
| `vcpu_with_mut(id, f)` | Mutable access via closure |
| `vcpu_get_mut(id) -> Option<&mut Vcpu>` | Unsafe mutable access (caller must hold GangTable lock) |
| `vcpu_count() -> usize` | Number of allocated VCPUs |
| `get_vcpu_type(id) -> VcpuType` | Return VCPU type (Idle fallback) |

---

## 2. Gang Scheduler (`kernel/src/scheduler.rs`)

### Constants

| Constant            | Value  | Description                        |
|---------------------|--------|------------------------------------|
| `VRUNTIME_TICK`     | `20`   | Base vruntime increment per tick   |
| `VRUNTIME_BIAS`     | `8`    | Bias for new gang initial vruntime |
| `KERNEL_STACK_SIZE` | `8192` | Bytes per kernel stack             |
| `IRETQ_FRAME_SIZE`  | `24`   | RIP+CS+RFLAGS for initial stack    |
| `RFLAGS_IF`         | `0x202`| Initial rflags (IF set)            |
| `MAX_GANGS`         | `32`   | Maximum concurrent gangs           |
| `MAX_VCPUS_PER_GANG`| `8`    | Maximum VCPUs in one gang          |

### GangState

```rust
pub enum GangState {
    Active,     // eligible for scheduling
    Halted,     // all VCPUs blocked
}
```

### Gang

```c
struct Gang {
    id: GangId,                                    // 0..31
    vcpu_ids: [Option<VcpuId>; MAX_VCPUS_PER_GANG], // 0..7
    vcpu_count: u32,                                // number of VCPUs
    vruntime: u64,                                  // gang vruntime
    running_count: u32,                             // currently running VCPUs
    state: GangState,
}
```

### Gang Table

```c
struct GangTable {
    gangs: [Option<Gang>; MAX_GANGS],   // 32 entries
    count: usize,
    initialized: bool,
}
```

Protected by `IrqSaveSpinLock`.

### Scheduler Algorithm (`schedule_inner`)

Called from the timer ISR (vector 32) on every tick:

```
schedule_inner(frame, cpu_id, gang_table_lock):
  1. Save current VCPU state:
     - Copy TrapFrame
     - fxsave FPU state
     - Update gang vruntime: gang.vruntime += VRUNTIME_TICK * 100 / weight
     - Update per-VCPU vruntime: vcpu.vruntime += VRUNTIME_TICK
     - Decrement gang.running_count

  2. Find best gang:
     - Scan active gangs (index >= 1)
     - Skip gangs where all VCPUs are running
     - Select gang with lowest vruntime

  3. If no gang available → switch to idle VCPU (return false)

  4. Compute escalation mode:
     - free_cores = count of CPUs running idle VCPU
     - HARD (free_cores >= demand): 1 VCPU per free core
     - EMULATED (free_cores > 0): ceil(demand / free_cores)
     - SEQUENTIAL (free_cores == 0): run 1 VCPU

  5. Pick next VCPU from gang (lowest per-VCPU vruntime, state=Ready)

  6. If emulated, push extra VCPUs onto this CPU's ready queue

  7. Load next VCPU:
     - Copy saved_frame → current TrapFrame
     - Set current_vcpu
     - Update TSS.rsp0
     - Return (true, next_pml4, fpu_ptr)
```

### Context Switch

Performed in inline assembly in the timer ISR handler
(`kernel/src/arch/idt.rs:639`):

```
  r8  = &TrapFrame
  r9  = next_pml4 (0 if unchanged)
  r10 = next_fpu  (*const FpuState)

  1. cmp r9, 0; je skip_cr3
  2. mfence; mov cr3, r9
  3. mov rsp, [r8 + 0xA0]    (TrapFrame.rsp)
  4. fxrstor [r10]
  5. Restore r15..r10 from [r8 + offset]
  6. Restore r9..rdi from [r8 + offset]
  7. push [r8 + 0x88]        (TrapFrame.rip)
  8. sti
  9. mov r8, [r8 + 0x38]     (restore r8 last)
  10. ret
```

This avoids `iretq` entirely (WHPX workaround), using `push rip + ret`
for the return.

### Idle VCPU

Each CPU has a dedicated idle VCPU (type `Idle`, state `Idle`). Its
saved frame captures the current RSP at the time of creation. When no
gang VCPUs are available, `schedule_inner` sets `current_vcpu = idle_id`
and returns `false` (no switch is performed).

### Stealing

```c
pub fn steal_task(hungry_cpu: usize) -> bool
```

Called from the idle loop when `task_count <= 1`. Finds the most loaded
CPU with `count >= 2` and moves one VCPU from its ready queue to the
hungry CPU's ready queue.

### Public API

| Function | Description |
|----------|-------------|
| `sched_init()` | Initialise gang table |
| `sched_yield()` | `syscall(nr=0)` — yield current VCPU |
| `init_idle_vcpu()` | Create idle VCPU for calling CPU |
| `current_vcpu_id() -> VcpuId` | VCPU running on this CPU |
| `current_cpu_slot() -> usize` | PERCPU slot of this CPU |
| `task_count() -> usize` | Total VCPUs in slab |
| `cpu_task_count(cpu) -> usize` | VCPUs assigned to a CPU |
| `create_gang(n_vcpus, entry, arg) -> Option<GangId>` | Create gang with N VCPUs |
| `register_driver_vcpu(vcpu_id) -> bool` | Add VCPU to new driver gang |
| `schedule(frame) -> (bool, u64, *const u8)` | Main schedule entry (timer ISR) |
| `block_current(frame)` | Block current VCPU (halt state) |
| `wake(vcpu_id)` | Wake a halted VCPU, place on least-loaded CPU |
| `steal_task(hungry_cpu) -> bool` | Load-balancing steal |
| `yield_now()` | `syscall(0)` — yield |
| `is_initialized() -> bool` | True after `sched_init` |

### Syscall: create_gang (nr 7)

See `kernel/src/arch/idt.rs:923`. Calls `scheduler::create_gang(n_vcpus, entry, 0)`
and returns the `GangId` or `u64::MAX`.

---

## 3. Scheduler Tick

The LAPIC timer fires every 1 ms (periodic, divisor 16, vector 32).
On every tick:

1. `percpu::tick()` increments the global tick counter.
2. Every 200 ticks, a per-CPU rate-limited log message is emitted.
3. If the scheduler is initialised, `schedule(frame)` is called.
4. If `schedule` returns `switched=true`, the context-switch asm runs.

## Syscall ABI

The `syscall` instruction (IA32_LSTAR set in `gdt::init_syscall_msrs`)
enters `syscall_entry` which builds a `TrapFrame` on the kernel stack
and calls `syscall_handler`. Return is via `iretq`.

See `kernel/src/arch/idt.rs:740` for the syscall stubs and handler.

```
Register  Role
────────  ────
rax       syscall number
rdi       arg0
rsi       arg1
rdx       arg2
r10       arg3
r8        arg4
r9        arg5
syscall   instruction (clobbers rcx, r11)
rax       return value
```
