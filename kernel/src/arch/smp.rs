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

/// Poll an AP's status byte until it signals ready (status == 1).
/// Returns `true` if the AP responded, `false` on timeout (~10 s).
fn poll_ap_slot(slot: usize) -> bool {
    let status_ptr = unsafe { slot_base(slot).add(MB_STATUS as usize) } as *const u8;
        for _ in 0..1000 {
        let status = unsafe { status_ptr.read_volatile() };
        if status != 0 {
            return true;
        }
        for _ in 0..1000000 { core::hint::spin_loop(); }
    }
    false
}

/// Start all APs in parallel via INIT-SIPI-SIPI.
///
/// Phase 0 — zero the slot map table and write pre-allocated PERCPU
///           slot numbers (assigned sequentially by `smp_boot_aps`).
/// Phase 1 — write all AP mailboxes.
/// Phase 2 — send INIT to all APs.
/// Phase 3 — single 10 ms wait.
/// Phase 4 — send first SIPI to all APs.
/// Phase 5 — 1 ms wait.
/// Phase 6 — send second SIPI to all APs.
/// Phase 7 — poll each AP for ready.
///
/// `ap_stacks` — physical address of each AP's kernel stack top (one per
/// enabled AP, indexed by AP index from BootInfo).
/// `ap_slots`  — pre-allocated PERCPU slot for each AP (set by the BSP
/// in `smp_boot_aps` to avoid the race of APs calling `find_slot`
/// concurrently).
pub fn start_aps(boot_info: &BootInfo, ap_stacks: &[u64], ap_slots: &[usize]) {
    let count = boot_info.ap_count as usize;
    if count == 0 {
        return;
    }
    if ap_stacks.len() < count {
        log::error!("SMP: not enough stacks ({}) for {} APs", ap_stacks.len(), count);
        return;
    }
    if ap_slots.len() < count {
        log::error!("SMP: not enough slots ({}) for {} APs", ap_slots.len(), count);
        return;
    }

    let pml4_phys = crate::mm::virt::pml4_address();
    let ap_entry_fn = crate::ap_start::ap_entry as *const () as u64;

    // ---- Phase 0: write the slot mapping table ----
    // The BSP has already pre-allocated PERCPU slots via `find_slot`
    // (serial, no race).  We write those into the physical SLOT_MAP
    // table so the real-mode trampoline and the long-mode `ap_entry`
    // can discover the slot for their APIC ID.
    unsafe {
        core::ptr::write_bytes(SLOT_MAP_PHYS as *mut u8, 0xFF, 256);
    }

    // Clamp count to MAX_CPUS to avoid overflowing stack arrays.
    let count = count.min(MAX_CPUS - 1);

    // ---- Phase 1: write all mailboxes ----
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
                slot,
                ap_stacks[i],
                gdt_limit,
                gdt_base,
                idt_limit,
                idt_base,
                ap_entry_fn,
                pml4_phys,
            );
        }
    }

    // ---- Phase 2: broadcast INIT to all APs ----
    log::info!("SMP: sending INIT IPI to all APs");
    crate::arch::apic::send_init_ipi_all();
    delay_ms(10);

    // ---- Phase 3: broadcast first SIPI ----
    log::info!("SMP: sending SIPI[0] to all APs (vector=0x08)");
    crate::arch::apic::send_sipi_all(0x08);
    delay_ms(1);

    // ---- Phase 4: broadcast second SIPI ----
    log::info!("SMP: sending SIPI[1] to all APs (vector=0x08)");
    crate::arch::apic::send_sipi_all(0x08);

    // ---- Phase 5: poll all APs in parallel ----
    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let slot = ap_slots[i];
        log::info!("SMP: waiting for AP[{}] (slot {})...", i, slot);
        if poll_ap_slot(slot) {
            log::info!("SMP: AP[{}] ready (apic_id={})", i, apic_id);
        } else {
            log::error!("SMP: AP[{}] (apic_id={}) did not respond", i, apic_id);
        }
    }

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
