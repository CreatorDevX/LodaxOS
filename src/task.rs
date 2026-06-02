use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::idt::TrapFrame;
use crate::mm::{phys, virt};

// ---- CFS constants ----

/// Virtual runtime added per timer tick (~1 ms). All tasks have equal weight.
const VRUNTIME_TICK: u64 = 20;

/// Bias subtracted from min vruntime for new tasks, giving them a slight
/// scheduling advantage so they start promptly.
const VRUNTIME_BIAS: u64 = 8;

/// Maximum vruntime delta before we risk overflow in signed comparisons.
/// If the gap exceeds this, we clamp to avoid pathological starvation.
const MAX_VRUNTIME_DELTA: u64 = 1_000_000;

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
    pub state: TaskState,
    pub vruntime: u64,
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

/// Holds the top of task 0's allocated kernel stack so that
/// `enter_task0_stack()` can switch to it.
static TASK0_STACK_TOP: AtomicU64 = AtomicU64::new(0);

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

    TASK0_STACK_TOP.store(stack_top, Ordering::Release);

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
            state: TaskState::Ready,
            vruntime: 0,
        });
        (*m).current = 0;
        (*m).count = 1;
        log::info!("task: main task registered as task 0 (vruntime=0)");
    }
}

pub fn task0_stack_top() -> u64 {
    TASK0_STACK_TOP.load(Ordering::Acquire)
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
            rbp: 0, rsi: 0, rdi: 0,
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
            state: TaskState::Ready,
            vruntime: new_vruntime,
        });
        (*m).count += 1;

        log::info!("task: created task {} entry={:#x} stack={:#x} vruntime={}",
            task_id, entry, stack_base, new_vruntime);
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
///   3. If it's different from the current task, perform a context switch.
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

            // Advance vruntime
            let old = task.vruntime;
            task.vruntime = old.saturating_add(VRUNTIME_TICK);
        }

        // ---- Pick next task (CFS: minimum vruntime) ----
        let next = match pick_next_ready(&*m, cur) {
            Some(id) => id,
            None => return false,
        };

        if next == cur {
            return false;
        }

        // ---- Switch to next task ----
        if let Some(task) = &(*m).tasks[next] {
            *frame = task.saved_frame;
        }
        (*m).current = next;

        log::trace!("sched: {} → {} (vruntime {})", cur, next,
            if let Some(t) = &(*m).tasks[next] { t.vruntime } else { 0 });
        true
    }
}

// ---- Blocking ----

pub fn block_current(frame: &mut TrapFrame) {
    let m = manager_ptr();
    unsafe {
        let cur = (*m).current;
        if let Some(task) = &mut (*m).tasks[cur] {
            task.state = TaskState::Blocked;
            log::trace!("task {} blocked (vruntime={})", cur, task.vruntime);
        }
        schedule(frame);
    }
}

pub fn wake(task_id: usize) {
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

// ---- Yield (cooperative) ----

pub fn yield_now() {
    unsafe { asm!("int 0x80", in("rax") 0u64) };
}
