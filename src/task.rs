use core::arch::asm;

use lodaxos_system::Caps;

use crate::arch::idt::TrapFrame;
use crate::mm::{phys, virt};

// ---- CFS constants ----

/// Virtual runtime added per timer tick (~1 ms). All tasks have equal weight.
const VRUNTIME_TICK: u64 = 20;

/// Bias subtracted from min vruntime for new tasks, giving them a slight
/// scheduling advantage so they start promptly.
const VRUNTIME_BIAS: u64 = 8;

// ---- Task states ----

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState {
    Ready,
    Blocked,
}

// ---- Task structure ----

#[derive(Debug, Clone, Copy)]
pub struct Task {
    pub id: usize,
    pub saved_frame: TrapFrame,
    pub kernel_stack_base: u64,
    /// Top of the kernel stack (initial RSP value when this task is
    /// switched to). For task 0 this is its existing stack; for
    /// spawned tasks (e.g. ExRun) the loader allocates a fresh
    /// 16 KiB stack and records the top here.
    pub kernel_stack_top: u64,
    /// Physical address of this task's PML4. The scheduler switches
    /// CR3 to this on every context switch. For task 0 (= kernel
    /// "main"), this is the kernel's PML4.
    pub pml4: u64,
    pub state: TaskState,
    pub vruntime: u64,
    /// Capability bitfield. See `src/cap.rs`. Read/written via the
    /// `cap::grant_caps` / `cap::revoke_caps` API (which does the lock).
    pub caps: Caps,
}

// ---- Global task manager ----

const MAX_TASKS: usize = 16;

pub struct TaskManager {
    tasks: [Option<Task>; MAX_TASKS],
    current: usize,
    count: usize,
    initialized: bool,
}

fn manager_ptr() -> *mut TaskManager {
    &raw mut MANAGER
}

static mut MANAGER: TaskManager = TaskManager {
    tasks: [None; MAX_TASKS],
    current: 0,
    count: 0,
    initialized: false,
};

pub fn init() {
    unsafe {
        (*manager_ptr()).current = 0;
        (*manager_ptr()).count = 0;
        (*manager_ptr()).initialized = true;
        log::info!("Task manager initialized");
    }
}

/// Return the minimum vruntime among all non-blocked tasks.
unsafe fn min_vruntime() -> u64 {
    let m = &*manager_ptr();
    let mut min = u64::MAX;
    for i in 0..m.count {
        if let Some(task) = &m.tasks[i] {
            if task.state == TaskState::Ready && task.vruntime < min {
                min = task.vruntime;
            }
        }
    }
    if min == u64::MAX { 0 } else { min }
}

/// Register the current execution context as task 0 (the "idle" / main task).
pub fn init_main_task() {
    let pages = phys::alloc_pages(2).expect("failed to allocate task 0 kernel stack");
    let stack_base = virt::HIGHER_HALF + pages;
    let stack_top = stack_base + KERNEL_STACK_SIZE;
    unsafe { core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize) };

    let current_rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) current_rsp) };

    let m = manager_ptr();
    unsafe {
        let dummy = TrapFrame {
            r15: 0, r14: 0, r13: 0, r12: 0,
            r11: 0, r10: 0, r9: 0, r8: 0,
            rax: 0, rbx: 0, rcx: 0, rdx: 0,
            rbp: 0, rsi: 0, rdi: 0,
            vector: 0, error_code: 0,
            rip: 0, cs: 0x08, rflags: 0x202,
            rsp: current_rsp, ss: 0x10,
        };
        (*m).tasks[0] = Some(Task {
            id: 0,
            saved_frame: dummy,
            kernel_stack_base: stack_base,
            kernel_stack_top: stack_top,
            pml4: virt::pml4_address(),
            state: TaskState::Ready,
            vruntime: 0,
            caps: Caps::all(),
        });
        (*m).current = 0;
        (*m).count = 1;
        log::info!("task: main task registered as task 0 (caps=all) pml4={:#x}", virt::pml4_address());
    }
}

pub fn is_initialized() -> bool {
    unsafe { (*manager_ptr()).initialized }
}

pub fn current_task_id() -> usize {
    unsafe { (*manager_ptr()).current }
}

pub fn task_count() -> usize {
    unsafe { (*manager_ptr()).count }
}

// ---- Task creation ----

const KERNEL_STACK_SIZE: u64 = 8192;
const IRETQ_FRAME_SIZE: u64 = 24;
const RFLAGS_IF: u64 = 0x202;

pub fn create_task(entry: u64) -> Option<usize> {
    // Default: spawn in the kernel's current PML4. The caller can use
    // `create_task_in` to spawn in a forked PML4.
    create_task_in(entry, 0, virt::pml4_address())
}

/// Create a task in a specific PML4. `arg` is passed in `RDI` when the
/// task starts. `pml4_phys` is the physical address of the task's
/// PML4 (use `virt::pml4_address()` for tasks in the kernel's
/// address space). `kernel_stack_pages` is the number of 4 KiB pages
/// to allocate for the kernel stack (default 4 = 16 KiB if 0).
pub fn create_task_in(
    entry: u64,
    arg: u64,
    pml4_phys: u64,
) -> Option<usize> {
    use lodaxos_system::{CapOp, Caps};
    use crate::cap;

    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        Caps::CAP_TASK_CREATE,
        CapOp::TaskCreate { parent: Some(cap::current_subject()) },
    ) {
        log::warn!("task::create_task_in: cap denied: {:?}", e);
        return None;
    }
    let m = manager_ptr();
    unsafe {
        if (*m).count >= MAX_TASKS {
            log::error!("task: max tasks ({}) reached", MAX_TASKS);
            return None;
        }

        let pages = phys::alloc_pages(2)?;
        let stack_base = virt::HIGHER_HALF + pages;
        let stack_top = stack_base + KERNEL_STACK_SIZE;

        core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize);

        let iretq_frame = stack_top - IRETQ_FRAME_SIZE;
        (iretq_frame as *mut u64).write(entry);
        ((iretq_frame + 8) as *mut u64).write(0x08u64);
        ((iretq_frame + 16) as *mut u64).write(RFLAGS_IF);

        let frame = TrapFrame {
            r15: 0, r14: 0, r13: 0, r12: 0,
            r11: 0, r10: 0, r9: 0, r8: 0,
            rax: 0, rbx: 0, rcx: 0, rdx: 0,
            rbp: 0, rsi: 0, rdi: arg,
            vector: 0,
            error_code: 0,
            rip: entry,
            cs: 0x08,
            rflags: RFLAGS_IF,
            rsp: iretq_frame,
            ss: 0x10,
        };
        (stack_base as *mut TrapFrame).write(frame);

        // CFS: give the new task a slightly lower vruntime than the current
        // minimum, so it gets scheduled sooner (a small "startup boost").
        let min_v = min_vruntime();
        let new_vruntime = min_v.saturating_sub(VRUNTIME_BIAS);

        let task_id = (*m).count;
        (*m).tasks[task_id] = Some(Task {
            id: task_id,
            saved_frame: frame,
            kernel_stack_base: stack_base,
            kernel_stack_top: stack_top,
            pml4: pml4_phys,
            state: TaskState::Ready,
            vruntime: new_vruntime,
            caps: Caps::empty(),
        });
        (*m).count += 1;

        log::info!(
            "task: created task {} entry={:#x} arg={:#x} stack={:#x} pml4={:#x} vruntime={}",
            task_id, entry, arg, stack_base, pml4_phys, new_vruntime
        );
        Some(task_id)
    }
}

// ---- CFS Scheduler ----

/// Find the ready task with the smallest vruntime.
/// Returns its index, or `None` if no ready tasks exist besides `current`.
unsafe fn pick_next_ready(m: &TaskManager, current: usize) -> Option<usize> {
    let mut best = None;
    let mut best_vruntime = u64::MAX;

    for i in 0..m.count {
        if i == current {
            continue;
        }
        if let Some(task) = &m.tasks[i] {
            if task.state == TaskState::Ready && task.vruntime < best_vruntime {
                best_vruntime = task.vruntime;
                best = Some(i);
            }
        }
    }

    // If no other ready task, re-select current (only if it's still ready)
    if best.is_none() {
        if let Some(task) = &m.tasks[current] {
            if task.state == TaskState::Ready {
                return Some(current);
            }
        }
    }

    best
}

/// Called from the LAPIC timer IRQ handler (vector 32).
///
/// CFS behaviour:
///   1. Advance the current task's vruntime by VRUNTIME_TICK.
///   2. Pick the ready task with the smallest vruntime.
///   3. If it's different from the current task, perform a context
///      switch (CR3 + RSP + RIP via the modified TrapFrame).
///
/// Returns true if a switch occurred.
pub fn schedule(frame: &mut TrapFrame) -> bool {
    let m = manager_ptr();
    unsafe {
        if (*m).count < 2 {
            return false;
        }

        // Compute the real interrupted RSP.
        let original_rsp = (frame as *const TrapFrame as u64) + 0xA0;

        // ---- Save current task state ----
        let cur = (*m).current;
        if let Some(task) = &mut (*m).tasks[cur] {
            task.saved_frame = *frame;
            task.saved_frame.rsp = original_rsp;
        }

        // ---- Pick next task (CFS: minimum vruntime) ----
        let next = match pick_next_ready(&*m, cur) {
            Some(id) => id,
            None => return false,
        };

        if next == cur {
            return false;
        }

        // ---- Advance current task's vruntime only when we actually switch
        //      away from it. Advancing before the pick (the previous behaviour)
        //      double-credits the current task whenever schedule() is called
        //      and we stay on it: once on the no-switch path, and again on the
        //      next tick that does switch. See audit A5.
        if let Some(task) = &mut (*m).tasks[cur] {
            let old = task.vruntime;
            task.vruntime = old.saturating_add(VRUNTIME_TICK);
        }

        // ---- Switch to next task ----
        let next_pml4 = (*m).tasks[next].map(|t| t.pml4).unwrap_or(0);
        let next_stack_top = (*m).tasks[next].map(|t| t.kernel_stack_top).unwrap_or(0);
        if let Some(task) = &(*m).tasks[next] {
            *frame = task.saved_frame;
        }
        (*m).current = next;

        // Point TSS.rsp0 at the new task's kernel stack so ring-0 IRQs
        // taken while the new task is running push their iretq frame
        // onto the new task's stack, not the 4 KiB boot DUMMY_STACK.
        if next_stack_top != 0 {
            crate::arch::gdt::tss_set_rsp0(next_stack_top);
        }

        // Switch PML4 if the next task has a different one. The new
        // PML4 must already include the kernel's higher-half code/data
        // (it's a fork of the kernel PML4), so the IDT handler's
        // iretq can complete normally.
        let cur_pml4 = virt::pml4_address();
        if next_pml4 != cur_pml4 && next_pml4 != 0 {
            log::trace!("sched: switch PML4 {:#x} → {:#x}", cur_pml4, next_pml4);
            virt::switch_pml4(next_pml4);
        }

        log::info!("sched: task {} → {} (vruntime {} → {})",
            cur, next,
            if let Some(t) = &(*m).tasks[cur] { t.vruntime } else { 0 },
            if let Some(t) = &(*m).tasks[next] { t.vruntime } else { 0 });
        true
    }
}

// ---- Blocking ----

pub fn block_current(frame: &mut TrapFrame) {
    let m = manager_ptr();
    unsafe {
        let cur = (*m).current;
        // Blocking the idle/main task (task 0) would leave no eligible
        // candidate for the scheduler; refuse and return so the caller can
        // log or panic instead of hanging the entire system.
        if cur == 0 {
            log::error!("task: refused to block task 0 (idle/main)");
            return;
        }
        if let Some(task) = &mut (*m).tasks[cur] {
            task.state = TaskState::Blocked;
            log::trace!("task {} blocked (vruntime={})", cur, task.vruntime);
        }
        schedule(frame);
    }
}

pub fn wake(task_id: usize) {
    use lodaxos_system::{CapOp, Caps};
    use crate::cap;

    let caller = cap::current_subject();
    let required = if (task_id as u32) == caller {
        Caps::CAP_TASK_SCHED
    } else {
        Caps::CAP_TASK_WAKE_OTHER
    };
    if let Err(e) = cap::check_and_authorize(
        caller,
        required,
        CapOp::CapGrant { target: task_id as u32, cap: 0 },
    ) {
        log::warn!("task::wake: cap denied: {:?}", e);
        return;
    }
    let m = manager_ptr();
    unsafe {
        if task_id < (*m).count {
            if let Some(task) = &mut (*m).tasks[task_id] {
                if task.state == TaskState::Blocked {
                    task.state = TaskState::Ready;
                    log::trace!("task {} woken (vruntime={})", task_id, task.vruntime);
                }
            }
        }
    }
}

// ---- Capability accessors (used by `src/cap.rs`) ----
//
// These run on BSP only for now; SMP will wrap them in `SpinLockIrq`.

/// Read the cap set of `task_id`. Returns `None` if out of range.
pub fn task_caps(task_id: usize) -> Option<Caps> {
    let m = manager_ptr();
    unsafe {
        if task_id < (*m).count {
            (*m).tasks[task_id].map(|t| t.caps)
        } else {
            None
        }
    }
}

/// Replace the cap set of `task_id` (used by tests and by the cap system
/// when applying a new default cap set). Returns `false` if out of range.
pub fn set_task_caps(task_id: usize, caps: Caps) -> bool {
    let m = manager_ptr();
    unsafe {
        if task_id < (*m).count {
            if let Some(t) = &mut (*m).tasks[task_id] {
                t.caps = caps;
                return true;
            }
        }
        false
    }
}

/// Atomically OR `add` into the cap set of `task_id` (within the BSP
/// critical section; with SMP this becomes `fetch_or` under a lock).
pub fn grant_task_caps(task_id: usize, add: Caps) -> bool {
    set_task_caps(task_id, task_caps(task_id).unwrap_or(Caps::empty()) | add)
}

/// Atomically AND-NOT `remove` from the cap set of `task_id`.
pub fn revoke_task_caps(task_id: usize, remove: Caps) -> bool {
    set_task_caps(task_id, task_caps(task_id).unwrap_or(Caps::empty()) & !remove)
}

// ---- Yield (cooperative) ----

pub fn yield_now() {
    unsafe { asm!("int 0x80", in("rax") 0u64) };
}
