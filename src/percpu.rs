//! Per-CPU state.
//!
//! One `PerCpu` slot per LAPIC ID, indexed by LAPIC ID. The BSP fills in
//! its own slot at boot; APs fill in theirs as they come online.
//!
//! Most of the per-CPU state is initialized lazily (the slot is zero until
//! first use) to avoid pulling in 4× the static state of single-CPU
//! builds. The `init` functions are called by `ap_start` (per-CPU) and
//! by the BSP at boot.
//!
//! ## SMP bring-up
//!
//! The boot crate brings APs up via UEFI MP Services. The trampoline
//! switches the AP to the kernel's PML4/GDT/IDT/stack and sets
//! `ApArg.ready = 1`. The kernel main writes `ApArg.target_pml4_phys`,
//! `ApArg.target_gdt_ptr`, `ApArg.target_idt_ptr`, and
//! `ApArg.target_entry` for each AP, then sets `ApArg.go = 1`.
//! The trampoline then jumps into `ap_start::ap_entry`, which is the
//! per-CPU entry point in long mode with kernel state fully set up.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use lodaxos_system::MAX_CPUS;

/// Maximum number of AP kernel stacks we can hold in the kernel's
/// reserved region. The boot allocates AP stacks in UEFI Loader-Data
/// memory (which survives ExitBootServices); the kernel does not need
/// to allocate additional stacks for APs.
pub const PER_CPU_STACK_PAGES: usize = 4; // 16 KiB

/// Runqueue for the per-CPU scheduler. Minimal for now: a single
/// `current_task` index. Real CFS-style multi-task support per CPU
/// will be added in a follow-up.
pub struct Runqueue {
    pub current_task: AtomicUsize,
    pub vruntime: AtomicU64,
}

impl Runqueue {
    pub const fn empty() -> Self {
        Self {
            current_task: AtomicUsize::new(0),
            vruntime: AtomicU64::new(0),
        }
    }
}

/// One slot per LAPIC ID. `online` is `false` until the CPU's `ap_entry`
/// sets it. `kernel_ready` is set by the BSP once the kernel is fully
/// initialised and the AP may enter the scheduler.
pub struct PerCpu {
    pub apic_id: AtomicU32,
    pub online: AtomicBool,
    pub kernel_ready: AtomicBool,
    /// Top of this CPU's kernel stack (set by boot for APs; BSP uses its
    /// own initial stack and updates this when task 0 is registered).
    pub kernel_stack_top: AtomicU64,
    /// Per-CPU tick counter. Increments from the LAPIC timer ISR.
    pub ticks: AtomicU64,
    /// The currently-running task on this CPU. Index into the global
    /// `TASKS` table.
    pub current_task: AtomicUsize,
    /// Per-CPU runqueue.
    pub runqueue: Runqueue,
}

impl PerCpu {
    pub const fn new() -> Self {
        Self {
            apic_id: AtomicU32::new(u32::MAX),
            online: AtomicBool::new(false),
            kernel_ready: AtomicBool::new(false),
            kernel_stack_top: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            current_task: AtomicUsize::new(0),
            runqueue: Runqueue::empty(),
        }
    }
}

/// The per-CPU array. Indexed by LAPIC ID (a small integer, 0..MAX_CPUS-1).
pub static PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// Register a CPU as online. Called by `ap_start::ap_entry` (per CPU)
/// and by the BSP at the end of `_start` init.
pub fn mark_online(apic_id: u32) {
    let slot = (apic_id as usize) % MAX_CPUS;
    unsafe {
        let p = &PERCPU[slot] as *const PerCpu as *mut PerCpu;
        (*p).apic_id.store(apic_id, Ordering::Release);
        (*p).online.store(true, Ordering::Release);
    }
    log::info!("percpu: CPU {} online", apic_id);
}

/// Wait until `kernel_ready` is set on this CPU. APs call this after
/// `mark_online` and before entering the scheduler.
pub fn wait_for_kernel_ready(apic_id: u32) {
    let slot = (apic_id as usize) % MAX_CPUS;
    while !PERCPU[slot].kernel_ready.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// Release all APs by setting `kernel_ready` for every slot, even those
/// not yet online. `release_aps()` (which writes `go = 1` and wakes the
/// AP) runs *before* the APs have had time to boot through the trampoline
/// and call `mark_online` — by the time each AP reaches
/// `wait_for_kernel_ready` its flag will already be `true`.
pub fn release_all_aps() {
    for slot in 0..MAX_CPUS {
        PERCPU[slot].kernel_ready.store(true, Ordering::Release);
    }
}

/// Read the current CPU's LAPIC ID. We don't store the current CPU
/// index in a fast-accessible place (no GS base / `IA32_TSC_AUX` yet),
/// so we read it from the LAPIC. This is slow but correct.
pub fn current_apic_id() -> u32 {
    // Read LAPIC ID register (offset 0x20). The high byte is the LAPIC ID.
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

/// Increment the global LAPIC tick counter. Called from the LAPIC timer
/// ISR (vector 32). Returns the new value.
///
/// A single global counter is used (rather than the per-CPU `ticks`
/// field below) so the BSP and AP idle logs read consistent values
/// from `ticks()`. The per-CPU `ticks` field is kept for future
/// per-CPU-specific use (e.g. per-CPU load tracking).
pub fn tick() -> u64 {
    crate::arch::idt::tick()
}

/// Read the global LAPIC tick counter. Used by the idle loop's logging
/// (BSP and AP) and the `get_ticks` syscall. Delegates to the single
/// source of truth in `arch::idt` so cross-CPU logs don't diverge
/// (see audit A4).
pub fn ticks() -> u64 {
    crate::arch::idt::ticks()
}

/// BSP (Bootstrap Processor) LAPIC ID. Set by `set_bsp_apic_id` during
/// early init. The BSP is the CPU that boots first and runs the
/// kernel's main idle loop; APs run a separate per-CPU idle loop.
static BSP_APIC_ID: AtomicU32 = AtomicU32::new(u32::MAX);

/// Record the BSP's LAPIC ID. Must be called once during init.
pub fn set_bsp_apic_id(apic_id: u32) {
    BSP_APIC_ID.store(apic_id, Ordering::Release);
}

/// True if the current CPU is the BSP. Used by the LAPIC timer ISR
/// to decide whether to run the scheduler (only the BSP runs tasks).
pub fn is_bsp() -> bool {
    let bsp = BSP_APIC_ID.load(Ordering::Acquire);
    if bsp == u32::MAX {
        // Not yet set — assume BSP (only CPU at this point).
        return true;
    }
    current_apic_id() == bsp
}
