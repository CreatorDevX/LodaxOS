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
//! The kernel brings APs up via LAPIC INIT-SIPI-SIPI using a real-mode
//! trampoline at physical address 0x8000.  The trampoline switches the AP
//! to the kernel's PML4/GDT/IDT/stack and jumps into `ap_start::ap_entry`,
//! which is the per-CPU entry point in long mode with kernel state fully
//! set up.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use crate::sync::IrqSaveSpinLock;
use lodaxos_system::MAX_CPUS;

/// Capacity of each per-CPU ready queue (entries per pCPU).
pub const QUEUE_CAPACITY: usize = 256;

struct QueueInner {
    buf: [usize; QUEUE_CAPACITY],
    head: usize,
    tail: usize,
}

/// Per-CPU ready queue: fixed-size circular buffer of entity IDs.
/// Protected by a per-queue spinlock to avoid ABA issues inherent in
/// lock-free MPMC designs.  The lock is IRQ-safe so it can be used
/// from both normal context and timer ISRs.
pub struct ReadyQueue {
    inner: IrqSaveSpinLock<QueueInner>,
}

impl ReadyQueue {
    pub const fn empty() -> Self {
        Self {
            inner: IrqSaveSpinLock::new(QueueInner {
                buf: [0; QUEUE_CAPACITY],
                head: 0,
                tail: 0,
            }),
        }
    }

    pub fn push(&self, id: usize) -> bool {
        let mut q = self.inner.lock();
        let next = q.tail.wrapping_sub(q.head);
        if next >= QUEUE_CAPACITY {
            return false;
        }
        let idx = q.tail % QUEUE_CAPACITY;
        q.buf[idx] = id;
        q.tail = q.tail.wrapping_add(1);
        true
    }

    pub fn pop(&self) -> Option<usize> {
        let mut q = self.inner.lock();
        if q.head == q.tail {
            return None;
        }
        let idx = q.head % QUEUE_CAPACITY;
        let id = q.buf[idx];
        q.head = q.head.wrapping_add(1);
        Some(id)
    }

    pub fn peek(&self) -> Option<usize> {
        let q = self.inner.lock();
        if q.head == q.tail {
            return None;
        }
        Some(q.buf[q.head % QUEUE_CAPACITY])
    }
}

/// Return a reference to the ready queue for `cpu`.
#[inline]
pub fn rq(cpu: usize) -> &'static ReadyQueue {
    &PERCPU[cpu % MAX_CPUS].ready_queue
}

/// IA32_GS_BASE MSR — base address of GS. Loading this with the address
/// of our per-CPU slot is what gives us cheap per-CPU access (the
/// kernel can use `%gs:offset` to reach its own state).
const IA32_GS_BASE: u32 = 0xC000_0102;

/// IA32_KERNEL_GS_BASE MSR — the value swapped in by `swapgs`.  We do not
/// use `swapgs` in this kernel (we never run user code), but the MSR is
/// defined for completeness.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0101;

/// IA32_TSC_AUX MSR — auxiliary value returned in ECX by `rdtscp`.
/// Linux uses this to cache the current CPU index / LAPIC ID, since
/// `rdtscp` is faster than reading the LAPIC ID register (one
/// instruction vs. an MMIO read).
const IA32_TSC_AUX: u32 = 0xC000_0103;

/// Maximum number of AP kernel stacks we can hold in the kernel's
/// reserved region. The boot allocates AP stacks in UEFI Loader-Data
/// memory (which survives ExitBootServices); the kernel does not need
/// to allocate additional stacks for APs.
pub const PER_CPU_STACK_PAGES: usize = 4; // 16 KiB



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
    /// `TASKS` table (legacy — will be replaced by current_vcpu).
    pub current_task: AtomicUsize,
    /// The currently-running vCPU on this CPU (replaces current_task in SEDS).
    pub current_vcpu: AtomicUsize,
    /// The idle Vcpu for this CPU (the hlt-loop Vcpu).
    pub idle_vcpu_id: AtomicU32,
    /// Number of ready+blocked tasks assigned to this CPU.
    pub task_count: AtomicUsize,
    /// Per-CPU ready queue (lock-free circular buffer of task IDs).
    pub ready_queue: ReadyQueue,
    /// Pointer to this slot in `PERCPU` (cached so we can verify the
    /// GS-base round-trip without re-deriving it).  Set once by
    /// `install_gs_base` and never changes for the lifetime of the slot.
    pub self_ptr: AtomicU64,
    /// Set true by another CPU when a reschedule IPI is received.
    /// The timer ISR checks this flag and may call schedule() early.
    pub need_resched: AtomicBool,
    /// Per-CPU timer fire count for rate-limited logging.
    pub timer_fires: AtomicU64,
    /// Address of a TLB entry that still needs flushing on this CPU.
    /// Set by a remote CPU when a TLB shootdown IPI times out.
    /// Checked and cleared on the next interrupt entry.
    pub pending_tlb_flush: AtomicU64,
    /// CR3 saved at interrupt entry (before any scheduler can change it).
    /// Used by the fault diagnostic dump to probe the faulting context's
    /// page tables instead of the (possibly switched) current CR3.
    pub saved_cr3: AtomicU64,
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
            current_vcpu: AtomicUsize::new(0),
            idle_vcpu_id: AtomicU32::new(u32::MAX),
            task_count: AtomicUsize::new(0),
            ready_queue: ReadyQueue::empty(),
            self_ptr: AtomicU64::new(0),
            need_resched: AtomicBool::new(false),
            timer_fires: AtomicU64::new(0),
            pending_tlb_flush: AtomicU64::new(0),
            saved_cr3: AtomicU64::new(0),
        }
    }
}

/// The per-CPU array. Indexed by LAPIC ID (a small integer, 0..MAX_CPUS-1).
pub static PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// APIC-ID-to-slot lookup table. Initialised with 0xFF (invalid).
/// When a CPU is marked online, the BSP or the AP itself writes
/// `APIC_TO_SLOT[apic_id] = slot`. This gives O(1) lookup from APIC ID
/// to slot without modulo collisions.
pub static APIC_TO_SLOT: [AtomicU8; 256] = [const { AtomicU8::new(0xFF) }; 256];

/// Fast slot lookup: map APIC ID to PERCPU index using the lookup table.
/// Falls back to a linear search if the APIC ID is ≥ 256.
#[inline]
pub fn apic_id_to_slot(apic_id: u32) -> usize {
    if apic_id < 256 {
        let slot = APIC_TO_SLOT[apic_id as usize].load(Ordering::Relaxed);
        if slot != 0xFF {
            return slot as usize;
        }
    }
    // Fallback: linear search (rare, only for APIC IDs ≥ 256).
    slot_for(apic_id)
}

/// Find a PERCPU slot for the given APIC ID using linear search.
/// First tries to match an existing online slot with the same APIC ID;
/// if none found, atomically claims the first offline slot. Returns
/// `None` if all slots are occupied (should not happen if caller
/// respects MAX_CPUS).
///
/// This avoids collisions that would occur with `apic_id % MAX_CPUS`
/// when APIC IDs differ by multiples of MAX_CPUS.
///
/// Thread-safe: multiple APs can call this concurrently during SIPI boot.
pub fn find_slot(apic_id: u32) -> Option<usize> {
    use core::sync::atomic::Ordering;

    // Phase 1: match an existing slot with this APIC ID (re-boot of same CPU).
    for slot in 0..MAX_CPUS {
        if PERCPU[slot].apic_id.load(Ordering::Relaxed) == apic_id {
            return Some(slot);
        }
    }
    // Phase 2: atomically claim the first offline slot.
    for slot in 0..MAX_CPUS {
        if PERCPU[slot]
            .online
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(slot);
        }
    }
    None
}

/// Return the preferred slot for an APIC ID (the one currently assigned
/// or the first free slot). Panics if all slots are exhausted.
pub fn slot_for(apic_id: u32) -> usize {
    find_slot(apic_id).expect("PERCPU slots exhausted")
}

/// Register a CPU as online at a caller-specified slot (bypasses
/// `find_slot` / `slot_for`).  Used by APs during SIPI boot where
/// the BSP has already pre-allocated the slot and written it into
/// the AP's mailbox/SLOT_MAP table, avoiding the race of multiple
/// APs calling `find_slot` concurrently.
pub fn mark_online_for_slot(apic_id: u32, slot: usize) {
    if apic_id < 256 {
        APIC_TO_SLOT[apic_id as usize].store(slot as u8, Ordering::Release);
    }
    unsafe {
        let p = &PERCPU[slot] as *const PerCpu as *mut PerCpu;
        (*p).apic_id.store(apic_id, Ordering::Release);
        (*p).online.store(true, Ordering::Release);
    }
}

/// Register a CPU as online. Called by `ap_start::ap_entry` (per CPU)
/// and by the BSP at the end of `_start` init.
pub fn mark_online(apic_id: u32) {
    let slot = slot_for(apic_id);
    mark_online_for_slot(apic_id, slot);
}

pub fn is_online(cpu: usize) -> bool {
    PERCPU[cpu % MAX_CPUS].online.load(Ordering::Acquire)
}

/// Wait until `kernel_ready` is set on this CPU. APs call this after
/// `mark_online` and before entering the scheduler.
///
/// Returns `false` on timeout (~10 s) so the caller can halt instead
/// of entering the scheduler unreleased.
pub fn wait_for_kernel_ready(apic_id: u32) -> bool {
    let slot = apic_id_to_slot(apic_id);
    for _ in 0..10_000_000 {
        if PERCPU[slot].kernel_ready.load(Ordering::Acquire) {
            return true;
        }
        core::hint::spin_loop();
    }
    log::error!("AP[lapic={}]: kernel_ready timeout — BSP may have crashed", apic_id);
    false
}

/// Release all APs by setting `kernel_ready` for every slot, even those
/// not yet online.  `smp_boot_aps()` runs *before* the APs have had time
/// to boot through the SIPI trampoline and call `mark_online` — by the
/// time each AP reaches `wait_for_kernel_ready` its flag will already
/// be `true`.
pub fn release_all_aps() {
    for slot in 0..MAX_CPUS {
        PERCPU[slot].kernel_ready.store(true, Ordering::Release);
    }
}

// ---- Per-CPU TLS (GS base + IA32_TSC_AUX) ----

/// Write a 64-bit value to an MSR.
unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nostack, preserves_flags),
        );
    }
}

/// Install per-CPU TLS on the *current* CPU: set IA32_GS_BASE to the
/// address of `PERCPU[slot]`, store the same address in the slot's
/// `self_ptr` (so a `%gs:0` round-trip is verifiable), and write the
/// slot's LAPIC ID into IA32_TSC_AUX so `rdtscp` returns it cheaply.
///
/// Must be called once per CPU, *after* the CPU has registered its
/// `apic_id` in the slot (call `mark_online` first).
///
/// `slot` is the index into `PERCPU` (== LAPIC ID, modulo MAX_CPUS).
pub fn install_gs_base(slot: usize) {
    let slot = slot % MAX_CPUS;
    let ptr = &PERCPU[slot] as *const PerCpu as u64;
    let apic_id = PERCPU[slot].apic_id.load(Ordering::Acquire);

    // Cache self-pointer in the slot (verifies GS base round-trip).
    PERCPU[slot].self_ptr.store(ptr, Ordering::Release);

    // Write GS base. After this, `%gs:0` resolves to `&PERCPU[slot]`.
    unsafe {
        wrmsr(IA32_GS_BASE, ptr);
        // Mirror into kernel-GS-base too.  We never `swapgs` (no user
        // mode), but writing both keeps the MSRs in a defined state
        // and matches the Linux convention.
        wrmsr(IA32_KERNEL_GS_BASE, ptr);
        // Cache the LAPIC ID in TSC_AUX for `rdtscp`-based lookups.
        // Only write if the CPU supports RDTSCP — `qemu64` lacks it.
        if has_rdtscp() {
            wrmsr(IA32_TSC_AUX, apic_id as u64);
        }
    }
}

/// Cached result of CPUID.80000001H.EDX[27] for RDTSCP support.
/// 0 = unchecked, 1 = supported, 2 = not supported.
static RDTSCP_SUPPORTED: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(0);

/// Check CPUID.80000001H.EDX[27] for RDTSCP support (cached).
fn has_rdtscp() -> bool {
    use core::sync::atomic::Ordering;
    let cached = RDTSCP_SUPPORTED.load(Ordering::Relaxed);
    if cached != 0 {
        return cached == 1;
    }
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 0x80000001",
            "cpuid",
            "mov {0:e}, edx",
            "pop rbx",
            out(reg) edx,
            out("eax") _,
            out("ecx") _,
            options(nostack, preserves_flags),
        );
    }
    let supported = (edx & (1 << 27)) != 0;
    RDTSCP_SUPPORTED.store(if supported { 1u8 } else { 2u8 }, Ordering::Relaxed);
    supported
}

/// Read the current CPU's LAPIC ID via `rdtscp` (IA32_TSC_AUX) if supported,
/// otherwise fall back to LAPIC MMIO (raw physical address 0xFEE00000).
#[inline]
pub fn current_apic_id() -> u32 {
    if has_rdtscp() {
        let aux: u32;
        unsafe {
            core::arch::asm!(
                "rdtscp",
                out("ecx") aux,
                options(nostack, preserves_flags),
            );
        }
        aux
    } else {
        let lapic_base = crate::arch::apic::read_apic_base();
        unsafe {
            let addr = (lapic_base + 0x20) as *const u32;
            let raw = core::ptr::read_volatile(addr);
            raw >> 24
        }
    }
}

/// Pointer to the calling CPU's per-CPU slot, obtained by reading
/// IA32_GS_BASE.  Returns a raw pointer so the caller can use
/// `%gs:offset`-style access without going through Rust's
/// per-CPU-table indirection.
#[inline]
pub fn self_slot() -> *mut PerCpu {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_GS_BASE,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
    }
    (((hi as u64) << 32) | (lo as u64)) as *mut PerCpu
}

/// Fallback LAPIC-ID-based lookup (slow path, used during very early
/// boot before `install_gs_base` has run, or when an ISR fires on a
/// CPU that hasn't finished bring-up).
///
/// Reads the LAPIC ID register directly via MMIO.  Kept exported so
/// `ap_start` can probe the APIC ID before TLS is installed.
pub fn current_apic_id_lapic() -> u32 {
    let raw: u32;
    unsafe {
        let addr = (*crate::arch::apic::LAPIC_BASE.get()) + 0x20;
        raw = core::ptr::read_volatile(addr as *const u32);
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

// ---- Per-CPU scheduler state ----

/// Return the currently-running task ID for `cpu`.
pub fn current_task(cpu: usize) -> usize {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].current_task.load(Ordering::Acquire)
}

/// Set the currently-running task ID for `cpu`.
pub fn set_current(cpu: usize, id: usize) {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].current_task.store(id, Ordering::Release);
}

/// Return the currently-running vCPU ID for `cpu` (SEDS).
pub fn current_vcpu(cpu: usize) -> usize {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].current_vcpu.load(Ordering::Acquire)
}

/// Set the currently-running vCPU ID for `cpu` (SEDS).
pub fn set_current_vcpu(cpu: usize, id: usize) {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].current_vcpu.store(id, Ordering::Release);
}

/// Return the number of tasks assigned to `cpu`.
pub fn task_count(cpu: usize) -> usize {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].task_count.load(Ordering::Acquire)
}

/// Set the number of tasks assigned to `cpu`.
pub fn set_task_count(cpu: usize, count: usize) {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].task_count.store(count, Ordering::Release);
}

/// Atomically add `delta` to the task count for `cpu` (Bug 7).
pub fn add_task_count(cpu: usize, delta: isize) {
    let slot = cpu % MAX_CPUS;
    if delta >= 0 {
        PERCPU[slot].task_count.fetch_add(delta as usize, Ordering::AcqRel);
    } else {
        PERCPU[slot].task_count.fetch_sub((-delta) as usize, Ordering::AcqRel);
    }
}

/// Set the idle Vcpu for `cpu`.
pub fn set_idle_vcpu(cpu: usize, id: u32) {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].idle_vcpu_id.store(id, Ordering::Relaxed);
}

/// Return the idle Vcpu for `cpu`.
pub fn idle_vcpu(cpu: usize) -> u32 {
    let slot = cpu % MAX_CPUS;
    PERCPU[slot].idle_vcpu_id.load(Ordering::Relaxed)
}

/// Find the CPU with the fewest tasks. Used by `create_task_in` for
/// automatic load balancing.
pub fn find_least_loaded() -> usize {
    let mut best = 0;
    let mut best_count = usize::MAX;
    let mut found = false;
    for cpu in 0..MAX_CPUS {
        if !PERCPU[cpu].online.load(Ordering::Acquire) {
            continue;
        }
        found = true;
        let cnt = PERCPU[cpu].task_count.load(Ordering::Relaxed);
        if cnt < best_count {
            best_count = cnt;
            best = cpu;
        }
    }
    // If no CPUs are online (should not happen in normal operation),
    // return CPU 0 as a safe default.
    if !found { 0 } else { best }
}
