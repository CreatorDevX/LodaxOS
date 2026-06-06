
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::mm::virt;

/// IA32_APIC_BASE MSR — bits 12–35 are the LAPIC base, bit 11 is global enable.
const IA32_APIC_BASE: u32 = 0x1B;

/// LAPIC MMIO register offsets.
pub const APIC_ID: usize = 0x20;
const APIC_LVR: usize = 0x30;
const APIC_TPR: usize = 0x80;
const APIC_EOI: usize = 0xB0;
const APIC_SVR: usize = 0xF0;
const APIC_ICR_LOW: usize = 0x300;
const APIC_ICR_HIGH: usize = 0x310;
const APIC_LVT_TIMER: usize = 0x320;
const APIC_LVT_LINT0: usize = 0x350;
const APIC_LVT_LINT1: usize = 0x360;
const APIC_TDCR: usize = 0x3E0;
const APIC_TICR: usize = 0x380;
const APIC_CCR: usize = 0x390;

/// ICR delivery mode
const ICR_FIXED: u32 = 0;
const ICR_INIT: u32 = 5 << 8;
const ICR_STARTUP: u32 = 6 << 8;

/// ICR destination shorthand
const ICR_DEST_PHYSICAL: u32 = 0;
const ICR_DEST_LOGICAL: u32 = 1 << 11;
const ICR_SELF: u32 = 1 << 16;
const ICR_ALL_INCLUDING_SELF: u32 = 2 << 16;
const ICR_ALL_EXCLUDING_SELF: u32 = 3 << 16;

/// ICR assert / level bits (must be 0 for edge-triggered fixed IPIs)
const ICR_ASSERT: u32 = 1 << 14;
const ICR_EDGE: u32 = 0;

/// Spurious vector — bit 8 enables LAPIC software, bits 0–7 = vector.
const APIC_SVR_ENABLE: u32 = 1 << 8;

/// LVT timer mode bits.
const APIC_LVT_PERIODIC: u32 = 1 << 17;

/// LINT0/1 delivery mode: fixed, active-high, edge-triggered.
const APIC_LVT_MASKED: u32 = 1 << 16;

/// LAPIC ticks per millisecond (calibrated at runtime). Public so
/// `ap_start` can re-use the BSP's calibration on each AP.
pub static mut TICKS_PER_MS: u32 = 0;

/// True once the LAPIC MMIO region is mapped and enabled.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Returns true if the LAPIC MMIO has been mapped and enabled.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Cached higher-half virtual address of the LAPIC base.
/// Exposed as `pub` so `percpu::current_apic_id` can read the LAPIC ID register.
pub static mut LAPIC_BASE: u64 = 0;

// ---- MSR helpers ----

/// Read a Model-Specific Register.
unsafe fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") lo,
        out("edx") hi,
    );
    ((hi as u64) << 32) | (lo as u64)
}

// ---- MMIO register access ----

/// Read a 32-bit value from a LAPIC MMIO register.
pub unsafe fn read32(offset: usize) -> u32 {
    let addr = LAPIC_BASE + offset as u64;
    unsafe { (addr as *const u32).read_volatile() }
}

/// Write a 32-bit value to a LAPIC MMIO register.
pub unsafe fn write32(offset: usize, val: u32) {
    let addr = LAPIC_BASE + offset as u64;
    unsafe { (addr as *mut u32).write_volatile(val) }
}

// ---- Public API ----

/// Read the IA32_APIC_BASE MSR and return the LAPIC physical base address.
pub fn read_apic_base() -> u64 {
    let msr = unsafe { read_msr(IA32_APIC_BASE) };
    (msr & 0x000F_FFFF_FFFF_F000) as u64
}

/// Map the LAPIC MMIO region into the higher-half virtual address space.
/// Must be called after page tables are initialized (uses phys::alloc_page
/// internally via the page table walkers).
///
/// No segment registers involved — pure memory mapping, so this is safe to
/// call before loading our own GDT.
pub fn init_mmio() {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    let phys = read_apic_base();
    log::info!("LAPIC: MMIO base physical = {:#x}", phys);

    let pml4 = virt::pml4_address();
    // MMIO: present, writable, no-execute + cache-disable.
    // Without CACHE_DISABLE (PCD), the CPU may cache LAPIC register writes
    // (e.g. EOI, timer count) and reads (e.g. CCR), returning stale values.
    // Use higher-half-only mapping — the identity map already covers this
    // physical range via 2MB huge pages, and creating 4KB entries at the
    // same PD level would conflict with the huge page entry.
    let flags = virt::PRESENT | virt::WRITABLE | virt::NO_EXECUTE | virt::CACHE_DISABLE;
    virt::map_region_higher_half(pml4, phys, 0x1000, flags);

    unsafe { LAPIC_BASE = virt::HIGHER_HALF + phys; }
    INITIALIZED.store(true, Ordering::Release);

    log::info!("LAPIC: MMIO mapped at virt={:#x}", unsafe { LAPIC_BASE });
}

/// LAPIC Error LVT register offset.
const APIC_LVT_ERROR: usize = 0x370;

/// BSP (Bootstrap Processor) LAPIC ID. Set by `set_bsp_lapic_id`
/// during early init. The BSP is the CPU that boots first and runs
/// the kernel's main idle loop; APs run a separate per-CPU idle loop
/// and don't participate in the scheduler.
static BSP_LAPIC_ID: AtomicU32 = AtomicU32::new(u32::MAX);

/// Record the BSP's LAPIC ID. Must be called once during init,
/// before the LAPIC timer ISR fires.
pub fn set_bsp_lapic_id(apic_id: u32) {
    BSP_LAPIC_ID.store(apic_id, Ordering::Release);
}

/// True if the current CPU is the BSP. Used by the LAPIC timer ISR
/// to decide whether to run the scheduler (only the BSP runs tasks).
/// If the BSP ID has not yet been set, assumes we're on the BSP
/// (the only CPU at that point).
pub fn is_bsp() -> bool {
    let bsp = BSP_LAPIC_ID.load(Ordering::Acquire);
    if bsp == u32::MAX {
        return true;
    }
    let raw: u32 = unsafe { read32(APIC_ID) };
    (raw >> 24) == bsp
}

/// Read the current LAPIC ID from the LAPIC ID register (offset 0x20).
/// High byte is the LAPIC ID. The lower bytes are reserved.
pub fn read_lapic_id() -> u32 {
    unsafe { read32(APIC_ID) >> 24 }
}

/// Enable the LAPIC and mask LINT0/LINT1 (required in symmetric mode).
///
/// The legacy 8259 PIC is masked separately by `arch::idt::mask_pic` —
/// callers should invoke that once before configuring the IOAPIC, so we
/// do not repeat the I/O here.
pub fn enable() {
    unsafe {
        // Mask LINT0 and LINT1 — prevents spurious vector delivery issues
        // when the LAPIC is not yet fully configured for external interrupts.
        write32(APIC_LVT_LINT0, APIC_LVT_MASKED);
        write32(APIC_LVT_LINT1, APIC_LVT_MASKED);

        // Initialize LVT Error with a valid vector (0xFF = spurious vector) and
        // mask it.  The reset default is vector 0, unmasked — if the LAPIC
        // detects any internal error (illegal APIC-bus message, etc.) it would
        // fire on vector 0, which QEMU prints as a warning and the CPU ignores.
        write32(APIC_LVT_ERROR, APIC_LVT_MASKED | 0xFF);

        // Set Task Priority to 0 — accept all interrupts.
        write32(APIC_TPR, 0);

        // Enable the LAPIC via the Spurious Interrupt Vector Register.
        // Vector 0xFF is conventional for the spurious interrupt.
        write32(APIC_SVR, APIC_SVR_ENABLE | 0xFF);

        log::info!("LAPIC: enabled (SVR={:#x})", read32(APIC_SVR));
    }
}

/// Configure the LAPIC timer.
///
/// - `divisor`: clock divisor (1, 2, 4, 8, 16, 32, 64, 128)
/// - `vector`: IDT vector number for timer interrupts
/// - `periodic`: true for periodic mode, false for one-shot
pub fn configure_timer(divisor: u32, vector: u8, periodic: bool) {
    unsafe {
        // Divide Configuration Register — bits [3:0] = divisor encoding.
        // Encoding: 0b0000=2, 0b0001=4, 0b0010=8, ... 0b1010=128, 0b1011=reserved.
        let dcr = match divisor {
            1 => 0b1011,
            2 => 0b0000,
            4 => 0b0001,
            8 => 0b0010,
            16 => 0b0011,
            32 => 0b1000,
            64 => 0b1001,
            128 => 0b1010,
            _ => 0b0011, // default to 16
        };
        write32(APIC_TDCR, dcr);

        // LVT Timer entry.
        let mut entry = vector as u32;
        if periodic {
            entry |= APIC_LVT_PERIODIC;
        }
        write32(APIC_LVT_TIMER, entry);

        // Read back to verify the write took effect.
        let readback = read32(APIC_LVT_TIMER);
        log::info!(
            "LAPIC timer: wrote LVT={:#x} readback={:#x} vector={} divisor={} mode={}",
            entry,
            readback,
            vector,
            divisor,
            if periodic { "periodic" } else { "one-shot" },
        );
    }
}

/// Calibrate the LAPIC timer against the PIT channel 0.
///
/// Programs the LAPIC timer with a large one-shot count, then waits 20 ms
/// by polling the PIT counter (Mode 0, 16‑bit, no reload). Reads the LAPIC
/// Current Count Register to derive ticks per millisecond.
///
/// Interrupts must be disabled for the entire measurement window (the caller
/// should have issued `cli` before calling). A save/restore guard is included
/// for robustness.
pub fn calibrate_pit() {
    // Save interrupt state, then ensure they're off for the measurement.
    let rflags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) rflags) };
    let interrupts_were_enabled = (rflags & 0x200) != 0;
    if interrupts_were_enabled {
        unsafe { asm!("cli") };
    }

    let ms = 20u32; // 20 ms: PIT target ≈ 23 863 (well within 16‑bit limit)
    const PIT_FREQ: u32 = 1_193_182;
    let pit_target = (PIT_FREQ * ms) / 1000;

    unsafe {
        // 1. Configure LAPIC timer: one‑shot, divisor 16, load max count.
        write32(APIC_LVT_TIMER, 0);
        write32(APIC_TDCR, 0b0011);
        let initial_count: u32 = 0xFFFF_FFFF;
        write32(APIC_TICR, initial_count);

        // 2. Reprogram PIT channel 0: lobyte/hibyte, Mode 0 (interrupt on
        //    terminal count).  Unlike Mode 2, Mode 0 does NOT auto‑reload
        //    when the counter reaches zero, so our polling loop cannot miss
        //    the window by scheduling jitter or SMI interrupts.
        let low = (pit_target & 0xFF) as u8;
        let high = ((pit_target >> 8) & 0xFF) as u8;
        asm!("out 0x43, al", in("al") 0x30u8); // Counter 0, lobyte/hibyte, Mode 0, binary
        asm!("out 0x40, al", in("al") low);
        asm!("out 0x40, al", in("al") high);

        // 3. Poll PIT counter until it counts down to ~0.
        let mut remaining = pit_target as u32;
        while remaining > 100 {
            asm!("out 0x43, al", in("al") 0x00u8); // latch counter 0
            let lo: u8;
            let hi: u8;
            asm!("in al, 0x40", out("al") lo);
            asm!("in al, 0x40", out("al") hi);
            remaining = ((hi as u32) << 8) | (lo as u32);
        }

        // 4. Read LAPIC Current Count Register.
        let current = read32(APIC_CCR);
        let elapsed = initial_count.wrapping_sub(current);

        // 5. Compute ticks per millisecond.
        let tpm = elapsed / ms;
        TICKS_PER_MS = tpm;

        log::info!(
            "LAPIC timer calibration: {} ticks in {}ms → {} ticks/ms",
            elapsed,
            ms,
            tpm,
        );
    }

    // Restore interrupt state.
    if interrupts_were_enabled {
        unsafe { asm!("sti") };
    }
}

/// Set the LAPIC timer initial count for the desired tick rate.
///
/// After calibration, call this to fire the timer at `ms`-millisecond intervals.
pub fn set_timer_count(ms: u32) {
    unsafe {
        let count = TICKS_PER_MS * ms;
        write32(APIC_TICR, count);
        log::info!("LAPIC timer: initial count = {} ({} ms interval)", count, ms);
    }
}

/// Per-AP timer setup. Each AP reprograms its own LAPIC timer to fire at
/// the same 1 ms interval as the BSP, using the BSP's calibrated
/// `TICKS_PER_MS`. The IDT is shared so the same vector 32 handler runs
/// on every CPU; the handler reads `percpu::current_apic_id()` to know
/// which CPU's tick counter to increment.
pub fn ap_enable_timer(_apic_id: u32) {
    unsafe {
        // Each AP must enable its own LAPIC (the BSP's enable() only
        // affects the BSP's LAPIC).
        write32(APIC_LVT_LINT0, APIC_LVT_MASKED);
        write32(APIC_LVT_LINT1, APIC_LVT_MASKED);
        write32(APIC_LVT_ERROR, APIC_LVT_MASKED | 0xFF);
        write32(APIC_SVR, APIC_SVR_ENABLE | 0xFF);
        write32(APIC_TPR, 0);

        let count = TICKS_PER_MS * 1; // 1 ms period
        write32(APIC_LVT_TIMER, 32 | APIC_LVT_PERIODIC);
        write32(APIC_TDCR, 0b0011); // divide by 16
        write32(APIC_TICR, count);
    }
}

/// Reprogram PIT channel 0 to Mode 2 (rate generator) so it fires periodic
/// interrupts at the given frequency (in Hz).  Call this after calibration
/// if you want PIT IRQs on vector 33 for testing.
pub fn pit_enable_periodic(freq_hz: u32) {
    unsafe {
        let reload = (1_193_182u64 / freq_hz as u64) as u16;
        let low = (reload & 0xFF) as u8;
        let high = ((reload >> 8) & 0xFF) as u8;
        // Counter 0, lobyte/hibyte, Mode 2 (rate generator), binary
        asm!("out 0x43, al", in("al") 0x34u8);
        asm!("out 0x40, al", in("al") low);
        asm!("out 0x40, al", in("al") high);
        log::info!("PIT: channel 0 set to {} Hz (reload = {})", freq_hz, reload);
    }
}

/// Send an IPI to a specific APIC ID using fixed delivery mode.
///
/// Blocks until the previous IPI has been delivered (delivery status clears).
pub fn send_ipi(dest_apic_id: u32, vector: u8) {
    unsafe {
        // Wait for delivery status to clear.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        // Write destination APIC ID to ICR high.
        write32(APIC_ICR_HIGH, (dest_apic_id as u32) << 24);
        // Write ICR low: vector + fixed + physical + edge (no assert).
        // Bit 14 (level/assert) must be 0 for edge-triggered fixed delivery.
        write32(APIC_ICR_LOW, vector as u32 | ICR_FIXED);
        // Wait for delivery to complete.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Broadcast IPI to all APs (excluding self).  Used for TLB shootdowns
/// and other cross-CPU coordination.
pub fn send_ipi_others(vector: u8) {
    unsafe {
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        write32(APIC_ICR_LOW, vector as u32 | ICR_ALL_EXCLUDING_SELF | ICR_FIXED);
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Send End-Of-Interrupt to the LAPIC. Must be called at the end of every
/// LAPIC interrupt handler to acknowledge the interrupt.
#[inline]
pub fn send_eoi() {
    unsafe {
        write32(APIC_EOI, 0);
    }
}

/// Send an INIT IPI to a specific APIC ID. The INIT IPI resets the target
/// CPU. After receiving INIT, the AP waits for a SIPI.
pub fn send_init_ipi(dest_apic_id: u32) {
    unsafe {
        // Wait for delivery status to clear.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        // Write destination APIC ID to ICR high.
        write32(APIC_ICR_HIGH, (dest_apic_id as u32) << 24);
        // INIT IPI: delivery mode = INIT, level assert, edge triggered.
        write32(APIC_ICR_LOW, ICR_INIT | ICR_ASSERT | ICR_EDGE);
        // Wait for delivery to complete.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Send a SIPI (Startup IPI) to a specific APIC ID. The vector specifies
/// the physical address where the AP starts executing (vector * 0x1000).
pub fn send_sipi(dest_apic_id: u32, vector: u8) {
    unsafe {
        // Wait for delivery status to clear.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        // Write destination APIC ID to ICR high.
        write32(APIC_ICR_HIGH, (dest_apic_id as u32) << 24);
        // STARTUP IPI: delivery mode = STARTUP, level assert, edge triggered.
        write32(APIC_ICR_LOW, (vector as u32) | ICR_STARTUP | ICR_ASSERT | ICR_EDGE);
        // Wait for delivery to complete.
        while read32(APIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
    }
}
