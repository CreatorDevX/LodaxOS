use core::sync::atomic::Ordering;

use lodaxos_system::MAX_CPUS;

use crate::arch::idt::TrapFrame;
use crate::mm::{phys, virt};
use crate::sync::IrqSaveSpinLock;
use crate::consts;
use crate::vcpu::{self, VcpuId, VcpuType, VcpuState, GANG_UNSCHEDULED};

// ── Constants ────────────────────────────────────────────────────────

const VRUNTIME_TICK: u64 = 20;
const VRUNTIME_BIAS: u64 = 8;
const KERNEL_STACK_SIZE: u64 = consts::KERNEL_STACK_SIZE;
const IRETQ_FRAME_SIZE: u64 = 24;
const RFLAGS_IF: u64 = 0x202;

const MAX_GANGS: usize = 32;
const MAX_VCPUS_PER_GANG: usize = 8;

// ── Types ────────────────────────────────────────────────────────────

pub type GangId = u32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GangState {
    Active,
    Halted,
}

#[derive(Debug)]
pub struct Gang {
    pub id: GangId,
    pub name: [u8; 32],
    pub symtab_phys: u64,
    pub symtab_size: u64,
    pub vcpu_ids: [Option<VcpuId>; MAX_VCPUS_PER_GANG],
    pub vcpu_count: u32,
    pub vruntime: u64,
    pub running_count: u32,
    pub state: GangState,
}

pub struct GangTable {
    pub gangs: [Option<Gang>; MAX_GANGS],
    pub count: usize,
    pub initialized: bool,
}

pub static GANG_TABLE: IrqSaveSpinLock<GangTable> = IrqSaveSpinLock::new(GangTable {
    gangs: [const { None }; MAX_GANGS],
    count: 0,
    initialized: false,
});

fn lock_gangs() -> crate::sync::IrqSaveGuard<'static, GangTable> {
    GANG_TABLE.lock()
}

// ── CPU helpers ──────────────────────────────────────────────────────

pub fn current_cpu_slot() -> usize {
    crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id())
}

// ── Public API ───────────────────────────────────────────────────────

pub fn init() {
    let mut gt = GANG_TABLE.lock();
    gt.count = 0;
    gt.initialized = true;
    log::info!("seds: scheduler initialized (max {} gangs)", MAX_GANGS);
}

pub fn is_initialized() -> bool {
    unsafe { GANG_TABLE.unsafe_get().initialized }
}

/// Create the idle Vcpu for the current CPU.
/// Called once per CPU during boot (BSP and each AP).
pub fn init_idle_vcpu() {
    let cpu_slot = current_cpu_slot();
    let pml4 = crate::mm::virt::pml4_address();
    let current_rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) current_rsp) };

    // Allocate an idle Vcpu (gang_id = GANG_UNSCHEDULED → not part of any gang).
    let vcpu_id = vcpu::alloc(GANG_UNSCHEDULED, pml4, 1 << cpu_slot, VcpuType::Idle)
        .expect("seds: failed to allocate idle Vcpu");

    vcpu::with_mut(vcpu_id, |maybe_vcpu| {
        if let Some(vcpu) = maybe_vcpu {
            vcpu.state = VcpuState::Idle;
            vcpu.saved_frame = TrapFrame {
                r15: 0, r14: 0, r13: 0, r12: 0,
                r11: 0, r10: 0, r9: 0, r8: 0,
                rax: 0, rbx: 0, rcx: 0, rdx: 0,
                rbp: 0, rsi: 0, rdi: 0,
                vector: 0, error_code: 0,
                rip: 0, cs: 0x08, rflags: 0x202,
                rsp: current_rsp, ss: 0x10,
            };
        }
    });

    // Save the current kernel stack for the idle vCPU (used by syscall_entry).
    crate::percpu::PERCPU[cpu_slot].kernel_stack_top.store(current_rsp, Ordering::Release);

    crate::percpu::set_idle_vcpu(cpu_slot, vcpu_id);
    crate::percpu::set_current_vcpu(cpu_slot, vcpu_id as usize);
    crate::percpu::set_task_count(cpu_slot, 1);

    log::info!("seds: idle Vcpu {} registered for CPU {}", vcpu_id, cpu_slot);
}

pub fn current_vcpu_id() -> VcpuId {
    let cpu = current_cpu_slot();
    crate::percpu::current_vcpu(cpu) as VcpuId
}

/// Total number of vCPUs in the slab (for logging / stats).
pub fn task_count() -> usize {
    vcpu::count()
}

pub fn cpu_task_count(cpu: usize) -> usize {
    let slot = cpu % MAX_CPUS;
    crate::percpu::task_count(slot)
}

pub fn yield_now() {
    unsafe { core::arch::asm!("syscall", in("rax") 0u64, lateout("rcx") _, lateout("r11") _) };
}

// ── Gang creation ────────────────────────────────────────────────────

/// Create a gang and its vCPUs. Each vCPU gets a dedicated kernel stack.
pub fn create_gang(n_vcpus: u32, entry: u64, arg: u64, name: &[u8], syms: Option<crate::exec::LoadResult>) -> Option<GangId> {
    let mut gt = GANG_TABLE.lock();

    // Gang 0 is reserved for GANG_UNSCHEDULED (Bug 10).
    if gt.count == 0 {
        gt.count = 1;
    }

    if gt.count >= MAX_GANGS {
        log::error!("seds: gang table exhausted (max {})", MAX_GANGS);
        return None;
    }

    let gang_id = gt.count as GangId;
    let mut gang_name = [0u8; 32];
    let len = name.len().min(32);
    gang_name[..len].copy_from_slice(&name[..len]);
    
    let (symtab_phys, symtab_size, _strtab_phys) = if let Some(s) = syms {
        (s.symtab_phys, s.symtab_size, s.strtab_phys)
    } else {
        (0, 0, 0)
    };
    
    let n = (n_vcpus as usize).min(MAX_VCPUS_PER_GANG);

        let mut vcpu_ids = [const { None }; MAX_VCPUS_PER_GANG];
        for i in 0..n {
            let vcpu_id = match vcpu::alloc(gang_id, virt::kernel_pml4(), !0, VcpuType::Normal) {
                Some(id) => id,
                None => {
                    log::error!("seds: vcpu slab exhausted creating gang {}", gang_id);
                    for j in 0..i {
                        if let Some(id) = vcpu_ids[j] {
                            vcpu::free(id);
                        }
                    }
                    return None;
                }
            };

            let pages = match phys::alloc_pages(3) {
                Some(p) => p,
                None => {
                    log::error!("seds: out of memory for gang {} vCPU {} stack", gang_id, i);
                    for j in 0..i {
                        if let Some(id) = vcpu_ids[j] {
                            vcpu::free(id);
                        }
                    }
                    return None;
                }
            };

            let alloc_base = virt::HIGHER_HALF + pages;
            let stack_base = alloc_base + consts::PAGE_SIZE;
            let stack_top = stack_base + KERNEL_STACK_SIZE;

            unsafe {
                core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize);
                virt::map_contiguous(
                    virt::kernel_pml4(),
                    stack_base,
                    pages + consts::PAGE_SIZE,
                    2,
                    virt::DATA,
                );
            }

        // Set up iretq frame at the top of the stack
        let iretq_frame = stack_top - IRETQ_FRAME_SIZE;
        unsafe {
            (iretq_frame as *mut u64).write(entry);
            ((iretq_frame + 8) as *mut u64).write(0x1Bu64);
            ((iretq_frame + 16) as *mut u64).write(RFLAGS_IF);
        }

        let frame = TrapFrame {
            r15: 0, r14: 0, r13: 0, r12: 0,
            r11: 0, r10: 0, r9: 0, r8: 0,
            rax: 0, rbx: 0, rcx: 0, rdx: 0,
            rbp: 0, rsi: 0, rdi: arg,
            vector: 0, error_code: 0,
            rip: entry,
            cs: 0x1B,
            rflags: RFLAGS_IF,
            rsp: iretq_frame,
            ss: 0x23,
        };

        vcpu::with_mut(vcpu_id, |maybe_vcpu| {
            if let Some(vcpu) = maybe_vcpu {
                vcpu.state = VcpuState::Ready;
                vcpu.saved_frame = frame;
                vcpu.kernel_stack_top = stack_top;
                vcpu.pml4 = virt::kernel_pml4();
                vcpu.gang_id = gang_id;
            }
        });

        vcpu_ids[i] = Some(vcpu_id);
        log::trace!("seds: gang {} vCPU {} created stack={:#x}", gang_id, vcpu_id, stack_top);
    }

    // Place all vCPUs on the least-loaded CPU to start
    let best_cpu = crate::percpu::find_least_loaded();
    for i in 0..n {
        if let Some(vid) = vcpu_ids[i] {
            crate::percpu::rq(best_cpu).push(vid as usize);
        }
    }
    crate::percpu::add_task_count(best_cpu, n as isize);

    let min_v = min_gang_vruntime(&*gt);
    gt.gangs[gang_id as usize] = Some(Gang {
        id: gang_id,
        name: gang_name,
        symtab_phys,
        symtab_size,
        vcpu_ids,
        vcpu_count: n_vcpus,
        vruntime: min_v.saturating_sub(VRUNTIME_BIAS * n_vcpus as u64),
        running_count: 0,
        state: GangState::Active,
    });
    gt.count += 1;

    log::info!(
        "seds: created gang {} with {} vCPUs on CPU {} entry={:#x}",
        gang_id, n, best_cpu, entry
    );
    Some(gang_id)
}

/// Add an existing VCPU (already loaded with an ELF) to a new driver gang.
/// Allocates a kernel stack for syscall/interrupt handling and creates a
/// dedicated gang so the scheduler will pick this VCPU.
pub fn register_driver_vcpu(vcpu_id: VcpuId) -> bool {
    // 1. Allocate kernel stack outside the lock (heavy operation).
    let pages = match phys::alloc_pages(3) {
        Some(p) => p,
        None => {
            log::error!("seds: OOM for driver Vcpu {} kernel stack", vcpu_id);
            return false;
        }
    };
        let alloc_base = virt::HIGHER_HALF + pages;
        let stack_base = alloc_base + consts::PAGE_SIZE;
        let stack_top = stack_base + KERNEL_STACK_SIZE;

        unsafe {
            core::ptr::write_bytes(stack_base as *mut u8, 0, KERNEL_STACK_SIZE as usize);
            virt::map_contiguous(
                virt::kernel_pml4(),
                stack_base,
                pages + consts::PAGE_SIZE,
                2,
                virt::DATA,
            );
        }


    // 2. Lock only for the table update
    let mut gt = GANG_TABLE.lock();

    // Gang 0 is reserved for GANG_UNSCHEDULED — make sure we start at 1.
    if gt.count == 0 {
        gt.count = 1;
    }

    if gt.count >= MAX_GANGS {
        log::error!("seds: gang table exhausted registering driver Vcpu {}", vcpu_id);
        // Clean up allocated memory if we failed here
        for p in 0..3 {
            virt::unmap(alloc_base + p * consts::PAGE_SIZE);
        }
        phys::free_pages(pages, 3);
        return false;
    }

    // Join the VCPU to the new gang and set its kernel stack.
    let gang_id = gt.count as GangId;
    vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.kernel_stack_top = stack_top;
            vcpu.gang_id = gang_id;
        }
    });

    gt.count += 1;
    gt.gangs[gang_id as usize] = Some(Gang {
        id: gang_id,
        name: *b"driver_vcpu                     ",
        symtab_phys: 0,
        symtab_size: 0,
        vcpu_count: 1,
        running_count: 0,
        vruntime: 0,
        vcpu_ids: [const { None }; MAX_VCPUS_PER_GANG],
        state: GangState::Active,
    });
    if let Some(gang) = gt.gangs[gang_id as usize].as_mut() {
        gang.vcpu_ids[0] = Some(vcpu_id);
    }

    log::trace!("seds: driver Vcpu {} added to gang {}", vcpu_id, gang_id);
    true
}

// ── Core scheduler ───────────────────────────────────────────────────

/// Main schedule entry point. Called from the timer ISR on every tick.
/// `fpu_out` receives the next vCPU's FPU state when `switched == true`.
/// The caller owns the buffer and must keep it alive until the context switch.
pub fn schedule(frame: &mut TrapFrame, fpu_out: &mut crate::arch::FpuState) -> (bool, u64) {
    let cpu_id = current_cpu_slot();
    let _g = GANG_TABLE.lock();
    let (switched, next_pml4) = schedule_inner(frame, cpu_id, _g, fpu_out);
    (switched, next_pml4)
}

fn schedule_inner(frame: &mut TrapFrame, cpu_id: usize, mut gt: crate::sync::IrqSaveGuard<'_, GangTable>, fpu_out: &mut crate::arch::FpuState) -> (bool, u64) {
    if gt.count < 1 {
        return (false, 0);
    }

    let idle_id = crate::percpu::idle_vcpu(cpu_id);
    let cur_vcpu = crate::percpu::current_vcpu(cpu_id) as VcpuId;

    // ── 1. Save current vCPU state ──
    if cur_vcpu != idle_id {
        vcpu::with_mut(cur_vcpu, |maybe_vcpu| {
            if let Some(vcpu) = maybe_vcpu {
                vcpu.saved_frame = *frame;
                if (frame.cs & 3) != 0 {
                    vcpu.saved_frame.rsp = frame.rsp;
                }
                unsafe { crate::arch::fxsave(&mut vcpu.fpu_state); }

                let gid = vcpu.gang_id as usize;
                if gid > 0 && gid < MAX_GANGS {
                    if let Some(ref mut gang) = gt.gangs[gid] {
                        let w = gang.vcpu_count.max(1) as u64;
                        gang.vruntime = gang.vruntime.saturating_add(VRUNTIME_TICK * 100 / w);
                        vcpu.vruntime = vcpu.vruntime.saturating_add(VRUNTIME_TICK);
                        gang.running_count = gang.running_count.saturating_sub(1);
                    }
                }
            }
        });
    }

    // ── 2. Find best gang ──
    let mut best_gang_idx = None;
    let mut best_vruntime = u64::MAX;
    for i in 1..gt.count {
        if let Some(ref gang) = gt.gangs[i] {
            if gang.state != GangState::Active { continue; }
            let avail = gang.vcpu_count.saturating_sub(gang.running_count);
            if avail > 0 && gang.vruntime < best_vruntime {
                best_vruntime = gang.vruntime;
                best_gang_idx = Some(i);
            }
        }
    }

    let Some(gang_idx) = best_gang_idx else {
        crate::percpu::set_current_vcpu(cpu_id, idle_id as usize);
        return (false, 0);
    };

    let Some(gang) = gt.gangs[gang_idx].as_mut() else {
        crate::percpu::set_current_vcpu(cpu_id, idle_id as usize);
        return (false, 0);
    };
    let demand = gang.vcpu_count.saturating_sub(gang.running_count);

    // ── 3. Count free cores for escalation ──
    let free_cores = count_free_cores(idle_id);
    let per_core = if free_cores >= demand {
        1 // HARD: each free core gets 1 vCPU (we are one of them)
    } else if free_cores > 0 {
        (demand + free_cores - 1) / free_cores // EMULATED: ceil(demand / free)
    } else {
        1 // SEQUENTIAL: just run 1
    };

    // ── 4. Pick the next vCPU from this gang ──
    let next_id = match pick_vcpu(gang) {
        Some(id) => id,
        None => {
            crate::percpu::set_current_vcpu(cpu_id, idle_id as usize);
            return (false, 0);
        }
    };

    gang.running_count += 1;

    // ── 5. Emulated: push extra vCPUs onto this CPU's ready queue ──
    if per_core > 1 {
        let extra = (per_core - 1).min(gang.vcpu_count.saturating_sub(gang.running_count));
        for _ in 0..extra {
            if let Some(eid) = pick_vcpu(gang) {
                gang.running_count += 1;
                crate::percpu::rq(cpu_id).push(eid as usize);
            }
        }
    }

    // ── 6. Load next vCPU context ──
    let (next_pml4, next_stack_top, valid) = vcpu::with_mut(next_id, |maybe_vcpu| {
        let vcpu = match maybe_vcpu {
            Some(v) => v,
            None => return (0u64, 0u64, false),
        };

        vcpu.state = VcpuState::Running;
        *frame = vcpu.saved_frame;
        *fpu_out = vcpu.fpu_state.clone();

        let pml4 = vcpu.pml4;
        let st = vcpu.kernel_stack_top;
        (pml4, st, true)
    });

    if !valid {
        crate::percpu::set_current_vcpu(cpu_id, idle_id as usize);
        return (false, 0);
    }

    crate::percpu::set_current_vcpu(cpu_id, next_id as usize);

    let cur_pml4 = virt::pml4_address();
    let need_switch = next_pml4 != cur_pml4 && next_pml4 != 0;

    if next_stack_top != 0 {
        let slot = cpu_id % MAX_CPUS;
        unsafe { crate::arch::gdt::tss_set_rsp0_for_slot(slot, next_stack_top); }
        crate::percpu::PERCPU[slot].kernel_stack_top.store(next_stack_top, Ordering::Release);
    }

    log::trace!(
        "seds: CPU{} vcpu {} (gang {}) → vcpu {} (gang {}) vruntime={}",
        cpu_id, cur_vcpu, get_gang_id(cur_vcpu),
        next_id, get_gang_id(next_id), gang.vruntime
    );

    (true, if need_switch { next_pml4 } else { 0 })
}

// ── Block / Wake ─────────────────────────────────────────────────────

pub fn block_current(frame: &mut TrapFrame) {
    let cpu_id = current_cpu_slot();
    let cur = crate::percpu::current_vcpu(cpu_id) as VcpuId;
    let idle = crate::percpu::idle_vcpu(cpu_id);

    let mut gt = GANG_TABLE.lock();

    if cur == idle {
        log::error!("seds: refused to block idle Vcpu on CPU {}", cpu_id);
        return;
    }

    vcpu::with_mut(cur, |maybe_vcpu| {
        if let Some(vcpu) = maybe_vcpu {
            vcpu.state = VcpuState::Halted;
            let gid = vcpu.gang_id as usize;
            if gid > 0 && gid < MAX_GANGS {
                if let Some(gang) = gt.gangs[gid].as_mut() {
                    gang.running_count = gang.running_count.saturating_sub(1);
                }
            }
            log::trace!("seds: vcpu {} blocked (gang {})", cur, vcpu.gang_id);
        }
    });

    let mut dummy_fpu = crate::arch::FpuState([0u8; 512]);
    unsafe {
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        crate::arch::fxsave(&mut dummy_fpu);
    }
    let (switched, next_pml4) = schedule_inner(frame, cpu_id, gt, &mut dummy_fpu);

    if !switched {
        // No new task to switch to — stay on current (idle).
        return;
    }

    // Context switch to the next vCPU.
    unsafe { crate::arch::context_switch(frame, next_pml4, &dummy_fpu); }
}

pub fn wake(vcpu_id: u64) {
    let vcpu_id = vcpu_id as VcpuId;
    let _caller_cpu = current_cpu_slot();
    let _g = GANG_TABLE.lock();

    vcpu::with_mut(vcpu_id, |maybe_vcpu| {
        if let Some(vcpu) = maybe_vcpu {
            if vcpu.state == VcpuState::Halted {
                vcpu.state = VcpuState::Ready;
                vcpu.vruntime = 0;

                let target = crate::percpu::find_least_loaded();
                crate::percpu::rq(target).push(vcpu_id as usize);
                crate::percpu::add_task_count(target, 1);

                log::trace!("seds: vcpu {} woken on CPU {}", vcpu_id, target);
            }
        }
    });
}

// ── Steal (load balancing) ──────────────────────────────────────────

pub fn steal_task(hungry_cpu: usize) -> bool {
    // Forcefully prevent stealing work if interrupts are disabled
    if !x86_64::instructions::interrupts::are_enabled() {
        return false;
    }

    let Some(_g) = GANG_TABLE.try_lock() else {
        log::trace!("seds: steal_task failed (lock contention) on CPU {}", hungry_cpu);
        return false; 
    };
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
    let Some(source_cpu) = fattest else {
        log::trace!("seds: steal_task failed (no fat CPU) on CPU {}", hungry_cpu);
        return false;
    };

    let stolen = match crate::percpu::rq(source_cpu).pop() {
        Some(id) => id as VcpuId,
        None => {
            log::trace!("seds: steal_task failed (rq empty) on CPU {}", source_cpu);
            return false;
        }
    };
    crate::percpu::rq(hungry_cpu).push(stolen as usize);
    crate::percpu::add_task_count(source_cpu, -1);
    crate::percpu::add_task_count(hungry_cpu, 1);
    log::info!("seds: stole vcpu {} from CPU {} to CPU {}", stolen, source_cpu, hungry_cpu);
    true
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Count how many pCPUs are currently running their idle Vcpu.
/// Must be called under the GangTable lock.
fn count_free_cores(idle_id: VcpuId) -> u32 {
    let mut count = 0u32;
    for cpu in 0..MAX_CPUS {
        if crate::percpu::PERCPU[cpu].online.load(Ordering::Acquire) {
            let cur = crate::percpu::PERCPU[cpu].current_vcpu.load(Ordering::Relaxed) as u32;
            if cur == idle_id {
                count += 1;
            }
        }
    }
    count
}

/// Pick the next vCPU to run from a gang.
/// Prefers the vCPU with the lowest per-vcpu vruntime that is not
/// currently running elsewhere (vcpu.state == Ready).
fn pick_vcpu(gang: &Gang) -> Option<VcpuId> {
    let mut best = None;
    let mut best_vruntime = u64::MAX;
    for i in 0..MAX_VCPUS_PER_GANG {
        if let Some(vid) = gang.vcpu_ids[i] {
            vcpu::with_mut(vid, |maybe_vcpu| {
                if let Some(vcpu) = maybe_vcpu {
                    if vcpu.state == VcpuState::Ready && vcpu.vruntime < best_vruntime {
                        best_vruntime = vcpu.vruntime;
                        best = Some(vid);
                    }
                }
            });
        }
    }
    best
}

/// Get the gang ID for a vCPU (for logging).
fn get_gang_id(vcpu_id: VcpuId) -> u32 {
    vcpu::with_mut(vcpu_id, |v| v.map_or(0xFFFF, |vcpu| vcpu.gang_id))
}

/// Minimum vruntime across all active gangs (for bias on new gangs).
fn min_gang_vruntime(gt: &GangTable) -> u64 {
    let mut min = u64::MAX;
    for i in 0..gt.count {
        if let Some(ref gang) = gt.gangs[i] {
            if gang.state == GangState::Active && gang.vruntime < min {
                min = gang.vruntime;
            }
        }
    }
    if min == u64::MAX { 0 } else { min }
}
