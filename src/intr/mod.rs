#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use lodaxos_system::{CapOp, Caps};

use crate::acpi::madt::{self, MadtInfo};
use crate::arch::ioapic;
use crate::cap;

// ---- Vector allocation ----
// Vectors 32 = LAPIC timer, 33..63 = device IRQs via IOAPIC.
// Vector 0xFF = spurious.

const FIRST_DEV_VECTOR: u8 = 33;
const LAST_DEV_VECTOR: u8 = 63;
const VECTOR_COUNT: u8 = LAST_DEV_VECTOR - FIRST_DEV_VECTOR + 1;

/// Simple incrementing vector allocator.
static NEXT_VECTOR: AtomicU8 = AtomicU8::new(FIRST_DEV_VECTOR);
static VECTOR_EXHAUSTED: AtomicBool = AtomicBool::new(false);

/// Allocate a free IDT vector for device interrupts.
pub fn alloc_vector() -> Option<u8> {
    let v = NEXT_VECTOR.fetch_add(1, Ordering::Relaxed);
    if v > LAST_DEV_VECTOR {
        VECTOR_EXHAUSTED.store(true, Ordering::Relaxed);
        None
    } else {
        Some(v)
    }
}

pub fn vectors_exhausted() -> bool {
    VECTOR_EXHAUSTED.load(Ordering::Relaxed)
}

// ---- Interrupt Routing Table ----

/// A single interrupt route: maps a device-level IRQ source
/// to a specific IOAPIC pin, LAPIC vector, and handler.
#[derive(Debug, Clone, Copy)]
pub struct IrqRoute {
    /// Original ISA IRQ source (0-15).
    pub isa_source: u8,
    /// Global System Interrupt number (from ACPI).
    pub gsi: u32,
    /// Index into the global IOAPIC array.
    pub ioapic_index: usize,
    /// Pin number on the IOAPIC.
    pub ioapic_pin: u8,
    /// IDT vector assigned to this route.
    pub vector: u8,
    /// MADT ISO flags (polarity bit 1, trigger mode bit 3).
    pub flags: u16,
}

pub const MAX_ROUTES: usize = 32;

struct IrqTable {
    routes: [Option<IrqRoute>; MAX_ROUTES],
    count: usize,
    initialized: bool,
}

struct SyncTable(UnsafeCell<IrqTable>);
unsafe impl Sync for SyncTable {}

static TABLE: SyncTable = SyncTable(UnsafeCell::new(IrqTable {
    routes: [None; MAX_ROUTES],
    count: 0,
    initialized: false,
}));

fn with_table<F, R>(f: F) -> R
where
    F: FnOnce(&mut IrqTable) -> R,
{
    f(unsafe { &mut *TABLE.0.get() })
}

/// Initialize the interrupt routing table from MADT data.
/// For each ISA IRQ (0..15), checks for an Interrupt Source Override.
/// If found, uses the ISO's GSI; otherwise uses identity mapping (GSI = IRQ).
/// Allocates a unique vector for each valid mapping.
pub fn init(madt: &MadtInfo) {
    with_table(|table| {
        if table.initialized {
            return;
        }

        // Track GSIs claimed by ISO routes — an ISO may remap a different ISA IRQ
        // to a GSI that another ISA IRQ's identity mapping would claim, causing
        // two routes to the same IOAPIC pin (the second overwrites the first).
        let mut gsi_claimed = [false; 256];

        for iso in madt.isos.iter().flatten() {
            if iso.bus != 0 {
                continue;
            }
            let gsi = iso.gsi;
            if (gsi as usize) >= gsi_claimed.len() {
                continue;
            }
            gsi_claimed[gsi as usize] = true;
            let flags = iso.flags;
            let route = build_route(madt, iso.source, gsi, flags);
            if let Some(r) = route {
                if table.count < MAX_ROUTES {
                    log::info!(
                        "IRQ route: ISA IRQ {} → GSI {} → IOAPIC[{}] pin {} vector {} (flags={:#x})",
                        iso.source,
                        gsi,
                        r.ioapic_index,
                        r.ioapic_pin,
                        r.vector,
                        flags,
                    );
                    table.routes[table.count] = Some(r);
                    table.count += 1;
                }
            }
        }

        for isa_irq in 0u8..16 {
            if has_iso(madt, isa_irq) {
                continue;
            }
            let gsi = isa_irq as u32;
            if (gsi as usize) < gsi_claimed.len() && gsi_claimed[gsi as usize] {
                continue;
            }
            let flags = 0u16;
            let route = build_route(madt, isa_irq, gsi, flags);
            if let Some(r) = route {
                if table.count < MAX_ROUTES {
                    log::info!(
                        "IRQ route: ISA IRQ {} → GSI {} (identity) → IOAPIC[{}] pin {} vector {}",
                        isa_irq,
                        gsi,
                        r.ioapic_index,
                        r.ioapic_pin,
                        r.vector,
                    );
                    table.routes[table.count] = Some(r);
                    table.count += 1;
                }
            }
        }

        table.initialized = true;
        log::info!("IRQ routing table: {} routes initialized", table.count);
    });
}

/// Check whether a MADT Interrupt Source Override exists for this ISA IRQ.
/// Used during identity-route generation to avoid duplicating ISO-mapped IRQs.
fn has_iso(madt: &MadtInfo, isa_irq: u8) -> bool {
    madt.isos.iter().flatten().any(|iso| iso.bus == 0 && iso.source == isa_irq)
}

fn build_route(madt: &MadtInfo, isa_source: u8, gsi: u32, flags: u16) -> Option<IrqRoute> {
    let (ioapic_idx, pin) = madt::lookup_ioapic(madt, gsi)?;
    let vector = alloc_vector()?;

    Some(IrqRoute {
        isa_source,
        gsi,
        ioapic_index: ioapic_idx,
        ioapic_pin: pin,
        vector,
        flags,
    })
}

// ---- Route lookup ----

/// Find the route for a given ISA IRQ.
pub fn lookup_isa(isa_irq: u8) -> Option<&'static IrqRoute> {
    let table = unsafe { &*TABLE.0.get() };
    for i in 0..table.count {
        if let Some(ref route) = table.routes[i] {
            if route.isa_source == isa_irq {
                return table.routes[i].as_ref();
            }
        }
    }
    None
}

pub fn lookup_gsi(gsi: u32) -> Option<&'static IrqRoute> {
    let table = unsafe { &*TABLE.0.get() };
    for i in 0..table.count {
        if let Some(ref route) = table.routes[i] {
            if route.gsi == gsi {
                return table.routes[i].as_ref();
            }
        }
    }
    None
}

/// Find the ISA source for a given vector.
pub fn lookup_vector_isa(vector: u8) -> Option<u8> {
    let table = unsafe { &*TABLE.0.get() };
    for i in 0..table.count {
        if let Some(ref route) = table.routes[i] {
            if route.vector == vector {
                return Some(route.isa_source);
            }
        }
    }
    None
}

/// Program an IOAPIC entry for a route (still masked by default).
pub fn install_route(route: &IrqRoute) {
    if let Err(e) = cap::check_and_authorize(
        cap::current_subject(),
        Caps::CAP_INTR_INSTALL,
        CapOp::IntrInstall { vector: route.vector },
    ) {
        log::warn!("intr::install_route: cap denied: {:?}", e);
        return;
    }
    if let Some(ioapic) = ioapic::get(route.ioapic_index) {
        let low = ioapic::IoApic::make_redir_low(route.vector, route.flags, true);
        let high = ioapic::IoApic::make_redir_high(0); // BSP APIC ID = 0
        ioapic.set_entry(route.ioapic_pin, low, high);
        log::debug!(
            "Installed IOAPIC[{}] pin {}: vector={} GSI={} (masked)",
            route.ioapic_index,
            route.ioapic_pin,
            route.vector,
            route.gsi,
        );
    }
}

/// Unmask a route's IOAPIC entry (enable delivery).
pub fn enable_route(route: &IrqRoute) {
    if let Some(ioapic) = ioapic::get(route.ioapic_index) {
        ioapic.unmask_entry(route.ioapic_pin);
        log::info!(
            "IOAPIC[{}] pin {} unmasked: vector {} enabled",
            route.ioapic_index,
            route.ioapic_pin,
            route.vector,
        );
    }
}

/// Install all routes into their respective IOAPICs (still masked).
pub fn install_all_routes() {
    let table = unsafe { &*TABLE.0.get() };
    for i in 0..table.count {
        if let Some(route) = &table.routes[i] {
            install_route(route);
        }
    }
}

/// Install all routes into their respective IOAPICs, leaving every entry
/// masked.  Callers must explicitly enable individual routes with
/// `enable_route` (e.g. once the device driver is ready to handle IRQs).
/// Returns the count of programmed pins.
pub fn install_all_masked() -> usize {
    let table = unsafe { &*TABLE.0.get() };
    let mut count = 0;
    for i in 0..table.count {
        if let Some(route) = &table.routes[i] {
            install_route(route);
            count += 1;
        }
    }
    count
}
