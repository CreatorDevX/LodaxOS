use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use lodaxos_system::{BootInfo, MAX_CPUS};
use crate::mm;
use crate::consts;

use consts::TRAMPOLINE_PHYS as TRAMP_PHYS;

/// Offset from trampoline base to the first mailbox slot.
const MAILBOX_OFF: u64 = 0x400;

/// Size of each per-AP mailbox slot (0x80 bytes = 128 bytes).
const MAILBOX_SLOT_SIZE: u64 = 0x80;

/// Physical address of the APIC-ID-to-slot mapping table (256 bytes,
/// one u8 per APIC ID, 0xFF = unassigned).  Written by the BSP before
/// sending IPIs; read by the real-mode trampoline, and by
/// `ap_start::ap_entry` (long mode) to discover its pre-allocated
/// PERCPU slot.
pub(crate) const SLOT_MAP_PHYS: u64 = TRAMP_PHYS + 0x300;

/// Mailbox field offsets within each slot (must match smp_trampoline.S):
const MB_STACK:     u64 = 0x40;
const MB_GDT_LIMIT: u64 = 0x48;   // u16 (2 bytes) + 6 bytes padding
const MB_GDT_BASE:  u64 = 0x50;   // u64, 8-byte aligned
const MB_IDT_LIMIT: u64 = 0x58;   // u16 (2 bytes) + 6 bytes padding
const MB_IDT_BASE:  u64 = 0x60;   // u64, 8-byte aligned
const MB_ENTRY:     u64 = 0x68;
const MB_STATUS:    u64 = 0x70;   // u8 (1 byte) + 7 bytes padding
const MB_PML4:      u64 = 0x78;

/// The raw trampoline binary (assembled from smp_trampoline.S).
const TRAMPOLINE_BIN: &[u8] = include_bytes!("smp_trampoline.bin");

/// Copy the trampoline binary into the reserved page at TRAMP_PHYS.
fn load_trampoline() {
    assert!(
        TRAMPOLINE_BIN.len() as u64 <= MAILBOX_OFF,
        "trampoline binary {} bytes exceeds mailbox offset {}",
        TRAMPOLINE_BIN.len(),
        MAILBOX_OFF
    );
    unsafe {
        core::ptr::copy_nonoverlapping(
            TRAMPOLINE_BIN.as_ptr(),
            TRAMP_PHYS as *mut u8,
            TRAMPOLINE_BIN.len(),
        );
        for off in (0..TRAMPOLINE_BIN.len()).step_by(64) {
            crate::ap_start::clflush((TRAMP_PHYS + off as u64) as *const u8);
        }
    }
}

/// Return the physical address of mailbox slot `slot`.
fn slot_base(slot: usize) -> *mut u8 {
    (TRAMP_PHYS + MAILBOX_OFF + (slot as u64) * MAILBOX_SLOT_SIZE) as *mut u8
}

/// Write a per-AP mailbox slot.  All mailboxes are written before any
/// IPIs are sent.  The AP locates its slot via the pre-populated slot
/// mapping table at SLOT_MAP_PHYS.
unsafe fn write_mailbox_slot(
    slot: usize,
    stack_top: u64,
    gdt_limit: u16,
    gdt_base: u64,
    idt_limit: u16,
    idt_base: u64,
    entry: u64,
    pml4: u64,
) {
    let base = slot_base(slot);

    core::ptr::write_volatile(base.add(MB_STACK as usize) as *mut u64, stack_top);
    core::ptr::write_volatile(base.add(MB_GDT_LIMIT as usize) as *mut u16, gdt_limit);
    core::ptr::write_volatile(base.add(MB_GDT_BASE as usize) as *mut u64, gdt_base);
    core::ptr::write_volatile(base.add(MB_IDT_LIMIT as usize) as *mut u16, idt_limit);
    core::ptr::write_volatile(base.add(MB_IDT_BASE as usize) as *mut u64, idt_base);
    core::ptr::write_volatile(base.add(MB_ENTRY as usize) as *mut u64, entry);
    core::ptr::write_volatile(base.add(MB_STATUS as usize) as *mut u8, 0u8);
    core::ptr::write_volatile(base.add(MB_PML4 as usize) as *mut u64, pml4);

    // flush the slot so AP sees fresh data under WHPX
    for off in (0..MAILBOX_SLOT_SIZE).step_by(64) {
        crate::ap_start::clflush(base.add(off as usize));
    }
}

/// Non-blocking check: read an AP's status byte once.
/// Returns `true` if the AP has signalled ready (status != 0).
fn check_ap_slot(slot: usize) -> bool {
    let status_ptr = unsafe { slot_base(slot).add(MB_STATUS as usize) } as *const u8;
    unsafe { status_ptr.read_volatile() != 0 }
}

// ---- Non-blocking AP boot state machine ----

/// AP boot state: 0 = idle, 1 = IPIs sent, 2 = all APs ready, 3 = error.
static AP_BOOT_STATE: AtomicU8 = AtomicU8::new(0);
static AP_BOOT_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Per-AP slot numbers and APIC IDs, stored for polling.
static mut AP_BOOT_SLOTS: [usize; MAX_CPUS] = [0; MAX_CPUS];
static mut AP_BOOT_APIC_IDS: [u8; MAX_CPUS] = [0; MAX_CPUS];

/// Send INIT-SIPI-SIPI to all APs and return immediately.
/// The BSP must later call `poll_aps_ready()` from the idle loop to
/// check AP readiness cooperatively.
pub fn send_ipis(boot_info: &BootInfo, ap_stacks: &[u64], ap_slots: &[usize]) {
    let count = boot_info.ap_count as usize;
    if count == 0 {
        AP_BOOT_STATE.store(2, Ordering::Release); // nothing to wait for
        return;
    }
    if ap_stacks.len() < count || ap_slots.len() < count {
        log::error!("SMP: not enough stacks/slots for {} APs", count);
        AP_BOOT_STATE.store(3, Ordering::Release);
        return;
    }

    let pml4_phys = crate::mm::virt::pml4_address();
    let ap_entry_fn = crate::ap_start::ap_entry as *const () as u64;
    let count = count.min(MAX_CPUS - 1);

    AP_BOOT_TOTAL.store(count, Ordering::Release);

    // Store slot/apic_id for later polling.
    unsafe {
        for i in 0..count {
            AP_BOOT_SLOTS[i] = ap_slots[i];
            AP_BOOT_APIC_IDS[i] = boot_info.ap_apic_ids[i] as u8;
        }
    }

    // Phase 0: write slot mapping table.
    unsafe {
        core::ptr::write_bytes(SLOT_MAP_PHYS as *mut u8, 0xFF, 256);
    }

    // Phase 1: write all mailboxes.
    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let slot = ap_slots[i];
        if (apic_id as usize) < 256 {
            unsafe {
                core::ptr::write_volatile(
                    (SLOT_MAP_PHYS + apic_id as u64) as *mut u8,
                    slot as u8,
                );
            }
        }
        let (gdt_limit, gdt_base) = gdt_ptr_contents(slot);
        let (idt_limit, idt_base) = idt_ptr_contents(slot);
        log::info!(
            "SMP: mailbox slot {} AP[{}] lapic={} stack={:#x}",
            slot, i, apic_id, ap_stacks[i]
        );
        unsafe {
            write_mailbox_slot(
                slot, ap_stacks[i] - 8,
                gdt_limit, gdt_base, idt_limit, idt_base,
                ap_entry_fn, pml4_phys,
            );
        }
    }

    // Phase 2: INIT IPI.
    log::info!("SMP: sending INIT IPI to all APs");
    crate::arch::apic::send_init_ipi_all();
    // ~10ms delay, polling katerm once per ms to keep it responsive.
    // LAPIC timer is not yet firing (interrupts disabled), so we
    // must use iteration-based delays.  process_input() does port I/O
    // (~1µs per call), so calling it per-iteration would blow the delay.
    for _ in 0..10 {
        for _ in 0..crate::consts::BUSY_LOOP_PER_MS {
            core::hint::spin_loop();
        }
        #[cfg(debug_assertions)]
        crate::katerm::process_input();
    }

    // Phase 3: first SIPI.
    log::info!("SMP: sending SIPI[0] to all APs (vector=0x08)");
    crate::arch::apic::send_sipi_all(0x08);
    // ~1ms delay, polling katerm once.
    for _ in 0..crate::consts::BUSY_LOOP_PER_MS {
        core::hint::spin_loop();
    }
    #[cfg(debug_assertions)]
    crate::katerm::process_input();

    // Phase 4: second SIPI.
    log::info!("SMP: sending SIPI[1] to all APs (vector=0x08)");
    crate::arch::apic::send_sipi_all(0x08);

    AP_BOOT_STATE.store(1, Ordering::Release);
    log::info!("SMP: IPIs sent, polling from idle loop");
}

/// Cooperative poll: check all AP status bytes once, return true when all
/// ready.  Call from the idle loop on each timer tick.
pub fn poll_aps_ready() -> bool {
    let state = AP_BOOT_STATE.load(Ordering::Acquire);
    if state == 2 { return true; }  // already done
    if state != 1 { return false; } // not started or error

    let total = AP_BOOT_TOTAL.load(Ordering::Relaxed);

    // Re-count from scratch each time (status bytes are sticky).
    let mut ready = 0usize;
    for i in 0..total {
        let slot = unsafe { AP_BOOT_SLOTS[i] };
        if check_ap_slot(slot) {
            ready += 1;
        }
    }

    if ready >= total {
        AP_BOOT_STATE.store(2, Ordering::Release);
        for i in 0..total {
            let apic_id = unsafe { AP_BOOT_APIC_IDS[i] };
            log::info!("SMP: AP[{}] (apic_id={}) ready", i, apic_id);
        }
        log::info!("SMP: all {} AP(s) online", total);
        return true;
    }
    false
}

/// Legacy synchronous wrapper: send IPIs and busy-wait for all APs.
/// Used by the old boot path; new code should use send_ipis + poll_aps_ready.
pub fn start_aps(boot_info: &BootInfo, ap_stacks: &[u64], ap_slots: &[usize]) {
    send_ipis(boot_info, ap_stacks, ap_slots);
    // Busy-wait (fallback for pre-interrupt context).
    for _ in 0..10000 {
        if poll_aps_ready() { return; }
        for _ in 0..1000000 { core::hint::spin_loop(); }
    }
    log::error!("SMP: timed out waiting for APs");
}

/// Reserve the trampoline page and load the trampoline code.
/// Must be called once during kernel init, after the physical allocator
/// is ready.
pub fn init() {
    unsafe { mm::phys::reserve_range(TRAMP_PHYS, 1); }
    load_trampoline();
    log::info!("SMP: SIPI trampoline loaded at {:#x}", TRAMP_PHYS);
}

// ---- helper functions ----

/// Return the (limit, base) of the GDT pointer for the given CPU slot.
fn gdt_ptr_contents(slot: usize) -> (u16, u64) {
    crate::arch::gdt::gdt_ptr_limit_base(slot)
}

/// Return the (limit, base) of the IDT pointer for the given CPU slot.
fn idt_ptr_contents(slot: usize) -> (u16, u64) {
    crate::arch::idt::idt_ptr_limit_base(slot)
}

/// Crude busy-loop delay (approximate milliseconds using spin_loop).
fn delay_ms(ms: u32) {
    for _ in 0..ms {
        for _ in 0..1000000 {
            core::hint::spin_loop();
        }
    }
}
