# 04 — Task Scheduling

## Overview

LodaxOS implements preemptive multitasking with a **CFS (Completely Fair Scheduler)** style virtual-runtime scheduler. The scheduler is invoked from the LAPIC timer interrupt (vector 32, fired every 1 ms). Each task gets its own 8 KB kernel stack and runs in ring 0. There is no userspace isolation yet — all tasks run at the highest privilege level.

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
    pub state: TaskState,            // Ready or Blocked
    pub vruntime: u64,               // CFS virtual runtime (lower = scheduled sooner)
}
```

### TaskManager

```rust
pub struct TaskManager {
    tasks: [Option<Task>; MAX_TASKS],  // max 16 tasks
    current: usize,                     // current running task index
    count: usize,                       // total tasks registered
    initialized: bool,
}
```

## Stack Layout

Each task (except task 0) gets an 8 KB kernel stack allocated from the physical page allocator and mapped into the higher-half virtual address space.

```
Kernel stack layout (high → low addresses):

stack_base + 8192  ─── top of allocated region
  [rflags]          ← RSP points here → iretq pops this
  [cs]              ← iretq pops this
  [rip]             ← iretq pops this (task entry point)
  ...               ← usable stack space (grows down)
stack_base          ─── bottom (TrapFrame stored here for save/restore)
```

When a task is created:
1. The bottom of the stack holds a synthetic `TrapFrame` (all registers zeroed, RIP = entry point, CS = 0x08, RFLAGS = 0x202 with IF enabled)
2. The top of the stack has an `iretq` frame: RIP (24 bytes below top), CS (16 bytes below top), RFLAGS (8 bytes below top)
3. The synthetic TrapFrame's RSP points to the iretq frame

When the scheduler first switches to this task, it restores the TrapFrame and executes `popfq` + `retfq`, which pops RFLAGS, CS, and RIP from the iretq frame, transferring control to the task's entry point with interrupts enabled.

## The Idle Task (Task 0)

Task 0 is the idle task, created during kernel initialization (`init_main_task`). It has a special role:

1. It is registered as the current execution context
2. It gets its own 8 KB kernel stack (the kernel switches RSP to this stack after initialization)
3. Its entry point is the idle loop (`hlt` + periodic logging)
4. It runs whenever no other task is ready

The idle loop:
```rust
loop {
    hlt;                              // halt until next interrupt
    if ticks - last_log >= 1000 {     // every ~1 second
        log tick/pit/keyboard stats;
    }
}
```

Blocking task 0 is explicitly refused by `block_current` (logged as an error) — if no other task is ready and the running task is task 0, the scheduler leaves it in place rather than halting the system.

## Task Creation

`task::create_task(entry: u64) -> Option<usize>`

1. Check if `MAX_TASKS` (16) would be exceeded
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
   b. Increments global tick counter (`TICKS.fetch_add(1, Relaxed)`)
   c. Calls `task::schedule(frame)`

### Schedule Algorithm (CFS)

```rust
pub fn schedule(frame: &mut TrapFrame) -> bool {
    if count < 2 { return false; }  // nothing to schedule

    // 1. Save current task's state
    let cur = current;
    tasks[cur].saved_frame = *frame;
    tasks[cur].saved_frame.rsp = frame_addr + 0xA0;  // correct RSP
    tasks[cur].vruntime = tasks[cur].vruntime.saturating_add(VRUNTIME_TICK);

    // 2. Find the next ready task with the smallest vruntime.
    let next = pick_next_ready(&*m, cur);

    // 3. If no other ready task exists, leave the current task in place.
    if next == cur { return false; }

    // 4. Restore next task's state and switch to it.
    *frame = tasks[next].saved_frame;
    current = next;
    true
}
```

The RSP correction is critical: `frame_addr + 0xA0` is the address of the RSP field in the TrapFrame, which is where the hardware pushed the original RSP on ring-0 interrupt entry. The saved_frame's RSP field contains this value, but after the stub pushes all GPRs, `frame.rsp` is simply a memory location, not the actual stack pointer.

### Context Switch Mechanics

When `schedule()` returns `true` (a switch was made), the timer IRQ handler does NOT use `iretq` to return. Instead it uses:

```asm
mov rsp, frame.rsp     ; switch to new task's stack
push frame.cs          ; push new CS
push frame.rip         ; push new RIP
push frame.rflags      ; push new RFLAGS
popfq                  ; restore RFLAGS (enables interrupts if IF=1)
retfq                  ; far return: pop RIP and CS
```

This sequence avoids `iretq`'s strict CS descriptor checks (canonicality, DPL vs CPL), which sometimes reject a valid `0x08` kernel selector when reached via a synthetic frame path.

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

`task::yield_now()` triggers a software interrupt (`int 0x80` with `rax = 0`). The syscall handler (vector 0x80) treats this as a no-op — execution returns to the caller, and the task will be preempted on the next timer tick. The yield exists for future cooperative scheduling scenarios (e.g., a task that wants to hint that it's done with its current timeslice).

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
