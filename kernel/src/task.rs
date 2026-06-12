use core::arch::asm;

use lodaxos_system::{Caps, MAX_CPUS};

use crate::arch::idt::TrapFrame;
use crate::mm::{phys, virt};
use crate::sync::IrqSaveSpinLock;
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
    /// Which CPU this task is assigned to.  `usize::MAX` = unassigned
    /// (will be assigned on first schedule).
    pub cpu: usize,
    pub saved_frame: TrapFrame,
    pub kernel_stack_base: u64,
    pub kernel_stack_top: u64,
    pub pml4: u64,
    pub state: TaskState,
    pub vruntime: u64,
    pub caps: Caps,
}

// ---- Global task pool ----

/// Maximum total tasks across all CPUs.
const MAX_TASKS: usize = 32;

fn current_apic_id_slow() -> u32 {
    let raw: u32;
    unsafe {
        core::arch::asm!(
            "mov eax, dword ptr [{addr}]",
            addr = in(reg) (crate::arch::apic::LAPIC_BASE + 0x20) as *const u32,
            out("eax") raw,
        );
    }
    raw >> 24
}

/// Return the CPU id of the current CPU (0..MAX_CPUS-1).
/// Uses the fast rdtscp path if TLS is set up; falls back to LAPIC MMIO.
fn current_cpu_id() -> usize {
    (crate::percpu::current_apic_id() as usize) % MAX_CPUS
}

pub struct TaskManager {
    /// IRQ-safe lock. Held on every state mutation. Read-only accessors
    /// (e.g. `task_count`) take it briefly.
    pub lock: IrqSaveSpinLock<()>,
    /// Protected by `lock`. Read/written through the lock guard.
    pub tasks: [Option<Task>; MAX_TASKS],
    pub count: usize,
    pub initialized: bool,
}

fn manager_ptr() -> *mut TaskManager {
    &raw mut MANAGER
}

static mut MANAGER: TaskManager = TaskManager {
    lock: IrqSaveSpinLock::new(()),
    tasks: [None; MAX_TASKS],
    count: 0,
    initialized: false,
};

/// Lock the global task manager and return a guard.  The lock is not
/// re-entrant, but our callers never nest (only `block_current` drops
/// before calling `schedule` which locks again).
unsafe fn lock_manager() -> crate::sync::IrqSaveGuard<'static, ()> {
    (*manager_ptr()).lock.lock()
}

pub fn init() {
    let _g = unsafe { lock_manager() };
    unsafe {
        (*manager_ptr()).count = 0;
        (*manager_ptr()).initialized = true;
    }
    log::info!("Task manager initialized (per-CPU runqueues)");
}

/// Return the minimum vruntime among all non-blocked tasks on this CPU.
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

/// Register the current execution context as the idle task for this CPU.
///
/// Called by the BSP during init and by each AP during `ap_entry`.
pub fn init_idle_task() {
    let pages = phys::alloc_pages(2)
        .expect("failed to allocate idle kernel stack");
    let stack_base = virt::HIGHER_HALF + pages;
    let stack_top = stack_base + KERNEL_STACK_SIZE;
    unsafe { virt::map_contiguous(virt::pml4_address(), stack_base, pages, 2, virt::DATA); }
    unsafe { core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize) };

    let current_rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) current_rsp) };

    let cpu_id = current_cpu_id();

    let m = manager_ptr();
    let _g = unsafe { lock_manager() };
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
        let task_id = (*m).count;
        (*m).tasks[task_id] = Some(Task {
            id: task_id,
            cpu: cpu_id,
            saved_frame: dummy,
            kernel_stack_base: stack_base,
            kernel_stack_top: stack_top,
            pml4: virt::pml4_address(),
            state: TaskState::Ready,
            vruntime: 0,
            caps: Caps::all(),
        });
        (*m).count += 1;
        crate::percpu::set_current(cpu_id, task_id);
        crate::percpu::set_task_count(cpu_id, 1);
        log::info!(
            "task: idle task {} registered for CPU {} (caps=all) pml4={:#x}",
            task_id, cpu_id, virt::pml4_address()
        );
    }
}

pub fn is_initialized() -> bool {
    unsafe { (*manager_ptr()).initialized }
}

pub fn current_task_id() -> usize {
    let cpu = current_cpu_id();
    crate::percpu::current_task(cpu)
}

pub fn task_count() -> usize {
    unsafe { (*manager_ptr()).count }
}

/// Number of tasks running on a specific CPU (public wrapper).
pub fn cpu_task_count(cpu: usize) -> usize {
    let slot = cpu % MAX_CPUS;
    crate::percpu::task_count(slot)
}

// ---- Task creation ----

const KERNEL_STACK_SIZE: u64 = 8192;
const IRETQ_FRAME_SIZE: u64 = 24;
const RFLAGS_IF: u64 = 0x202;

pub fn create_task(entry: u64) -> Option<usize> {
    create_task_in(entry, 0, virt::pml4_address())
}

/// Create a task and assign it to the least-loaded CPU.
pub fn create_task_in(entry: u64, arg: u64, pml4_phys: u64) -> Option<usize> {
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
    let best_cpu: usize;
    {
        let _g = unsafe { lock_manager() };
        unsafe {
            if (*m).count >= MAX_TASKS {
                log::error!("task: max tasks ({}) reached", MAX_TASKS);
                return None;
            }

            // Pick the CPU with the fewest tasks.
            best_cpu = crate::percpu::find_least_loaded();
            // Bump that CPU's task count so other CPUs don't race for it.
            crate::percpu::set_task_count(best_cpu, crate::percpu::task_count(best_cpu) + 1);
        }
        // _g drops here, releasing the lock
    }

    let pages = phys::alloc_pages(2)?;
    let stack_base = virt::HIGHER_HALF + pages;
    let stack_top = stack_base + KERNEL_STACK_SIZE;

    unsafe { core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize) };

    // Map the stack into the task's page tables so it is accessible
    // after the scheduler switches CR3 to this PML4.
    unsafe { virt::map_contiguous(pml4_phys, stack_base, pages, 2, virt::DATA); }

    let iretq_frame = stack_top - IRETQ_FRAME_SIZE;
    unsafe {
        (iretq_frame as *mut u64).write(entry);
        ((iretq_frame + 8) as *mut u64).write(0x08u64);
        ((iretq_frame + 16) as *mut u64).write(RFLAGS_IF);
    }

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
    unsafe { (stack_base as *mut TrapFrame).write(frame) };

    let min_v = unsafe { min_vruntime() };

    let _g = unsafe { lock_manager() };
    unsafe {
        let new_vruntime = min_v.saturating_sub(VRUNTIME_BIAS);
        let task_id = (*m).count;
        (*m).tasks[task_id] = Some(Task {
            id: task_id,
            cpu: best_cpu,
            saved_frame: frame,
            kernel_stack_base: stack_base,
            kernel_stack_top: stack_top,
            pml4: pml4_phys,
            state: TaskState::Ready,
            vruntime: new_vruntime,
            caps: Caps::empty(),
        });
        (*m).count += 1;

        // Push to the target CPU's per-CPU ready queue.
        crate::percpu::rq(best_cpu).push(task_id);

        log::info!(
            "task: created task {} on CPU {} entry={:#x} arg={:#x} stack={:#x} pml4={:#x} vruntime={}",
            task_id, best_cpu, entry, arg, stack_base, pml4_phys, new_vruntime
        );
        Some(task_id)
    }
}

// ---- CFS Scheduler (per-CPU) ----

/// Called from the LAPIC timer IRQ handler on EVERY CPU.
///
/// Returns `(true, next_pml4)` if a context switch occurred.
/// `next_pml4` is the physical address of the target task's PML4,
/// or 0 if no CR3 switch is needed.  The caller must switch CR3
/// **after** switching RSP to the new task's stack so that the
/// old stack remains valid during the log::info! calls that follow.
pub fn schedule(frame: &mut TrapFrame) -> (bool, u64) {
    let cpu_id = current_cpu_id();
    let m = manager_ptr();
    let _g = unsafe { lock_manager() };
    unsafe {
        if (*m).count < 2 {
            return (false, 0);
        }

        let original_rsp = frame.rsp;

        // ---- Save current task state ----
        let cur = crate::percpu::current_task(cpu_id);
        if let Some(task) = &mut (*m).tasks[cur] {
            task.saved_frame = *frame;
            task.saved_frame.rsp = original_rsp;
        }

        // ---- Pick next task from per-CPU ready queue ----
        let next = match crate::percpu::rq(cpu_id).pop() {
            Some(id) => id,
            // Nothing ready on this CPU — stay on current.
            None => return (false, 0),
        };

        if next == cur {
            // Push it back so it gets picked next tick.
            crate::percpu::rq(cpu_id).push(next);
            return (false, 0);
        }

        // ---- Advance current task's vruntime ----
        if let Some(task) = &mut (*m).tasks[cur] {
            task.vruntime = task.vruntime.saturating_add(VRUNTIME_TICK);
            // Re-queue the current task if it's still ready.
            if task.state == TaskState::Ready {
                crate::percpu::rq(cpu_id).push(cur);
            }
        }

        // ---- Switch to next task ----
        let next_pml4 = (*m).tasks[next].map(|t| t.pml4).unwrap_or(0);
        let next_stack_top = (*m).tasks[next].map(|t| t.kernel_stack_top).unwrap_or(0);
        if let Some(task) = &(*m).tasks[next] {
            *frame = task.saved_frame;
        }
        crate::percpu::set_current(cpu_id, next);

        if next_stack_top != 0 {
            let slot = cpu_id % MAX_CPUS;
            crate::arch::gdt::tss_set_rsp0_for_slot(slot, next_stack_top);
        }

        let cur_pml4 = virt::pml4_address();
        let need_switch = next_pml4 != cur_pml4 && next_pml4 != 0;
        if need_switch {
            log::trace!("sched: switch PML4 {:#x} → {:#x}", cur_pml4, next_pml4);
        }

        log::info!("sched: CPU{} task {} → {} (vruntime {} → {})",
            cpu_id, cur, next,
            if let Some(t) = &(*m).tasks[cur] { t.vruntime } else { 0 },
            if let Some(t) = &(*m).tasks[next] { t.vruntime } else { 0 });
        (true, if need_switch { next_pml4 } else { 0 })
    }
}

// ---- Blocking ----

pub fn block_current(frame: &mut TrapFrame) {
    let m = manager_ptr();
    let cpu_id = current_cpu_id();
    let cur = crate::percpu::current_task(cpu_id);
    let _g = unsafe { lock_manager() };
    unsafe {
        if cur == 0 {
            log::error!("task: refused to block idle task on CPU {}", cpu_id);
            return;
        }
        if let Some(task) = &mut (*m).tasks[cur] {
            task.state = TaskState::Blocked;
            task.cpu = usize::MAX; // no CPU assigned while blocked
            log::trace!("task {} blocked (vruntime={})", cur, task.vruntime);
        }
    }
    // Release lock before schedule (schedule locks internally).
    drop(_g);
    let (_, next_pml4) = schedule(frame);
    if next_pml4 != 0 {
        virt::switch_pml4(next_pml4);
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
    let _g = unsafe { lock_manager() };
    unsafe {
        let caller_cpu = current_cpu_id();
        if task_id < (*m).count {
            if let Some(task) = &mut (*m).tasks[task_id] {
                if task.state == TaskState::Blocked {
                    task.state = TaskState::Ready;
                    task.cpu = caller_cpu;
                    crate::percpu::set_task_count(caller_cpu, crate::percpu::task_count(caller_cpu) + 1);
                    crate::percpu::rq(caller_cpu).push(task_id);
                    log::trace!("task {} woken on CPU {} (vruntime={})",
                        task_id, caller_cpu, task.vruntime);
                }
            }
        }
    }
}

// ---- Capability accessors ----

/// Read the cap set of `task_id`.
pub fn task_caps(task_id: usize) -> Option<Caps> {
    let m = manager_ptr();
    let _g = unsafe { lock_manager() };
    unsafe {
        if task_id < (*m).count {
            (*m).tasks[task_id].map(|t| t.caps)
        } else {
            None
        }
    }
}

pub fn set_task_caps(task_id: usize, caps: Caps) -> bool {
    let m = manager_ptr();
    let _g = unsafe { lock_manager() };
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

pub fn grant_task_caps(task_id: usize, add: Caps) -> bool {
    set_task_caps(task_id, task_caps(task_id).unwrap_or(Caps::empty()) | add)
}

pub fn revoke_task_caps(task_id: usize, remove: Caps) -> bool {
    set_task_caps(task_id, task_caps(task_id).unwrap_or(Caps::empty()) & !remove)
}

// ---- Yield ----

pub fn yield_now() {
    unsafe { asm!("int 0x80", in("rax") 0u64) };
}

// ---- Load balancing ----

/// Try to steal a ready task from the most-loaded CPU and assign it to
/// `hungry_cpu`.  Returns `true` if a task was stolen.
///
/// Called by a CPU whose runqueue is empty (idle).
pub fn steal_task(hungry_cpu: usize) -> bool {
    // Find the CPU with the most ready tasks assigned to it.
    let mut fattest = None;
    let mut fattest_count = 0;
    for cpu in 0..MAX_CPUS {
        if cpu == hungry_cpu { continue; }
        let cnt = crate::percpu::task_count(cpu);
        if cnt > fattest_count && cnt >= 2 {
            fattest_count = cnt;
            fattest = Some(cpu);
        }
    }
    let Some(source_cpu) = fattest else { return false; };

    let m = manager_ptr();
    let _g = unsafe { lock_manager() };
    unsafe {
        // Pop from the fattest CPU's ready queue.
        let stolen = match crate::percpu::rq(source_cpu).pop() {
            Some(id) => id,
            None => return false,
        };
        // Update task's CPU assignment and push to hungry CPU's queue.
        if let Some(task) = &mut (*m).tasks[stolen] {
            task.cpu = hungry_cpu;
        }
        crate::percpu::rq(hungry_cpu).push(stolen);
        crate::percpu::set_task_count(source_cpu, crate::percpu::task_count(source_cpu).saturating_sub(1));
        crate::percpu::set_task_count(hungry_cpu, crate::percpu::task_count(hungry_cpu) + 1);
        log::info!("sched: stole task {} from CPU {} to CPU {}",
            stolen, source_cpu, hungry_cpu);
        true
    }
}
