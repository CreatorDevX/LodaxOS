#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use lodaxos_system::MAX_CPUS;

use crate::acpi::madt::{self, MadtInfo};
use crate::arch::ioapic;

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

        // Track GSIs claimed by ISO routes -- an ISO may remap a different ISA IRQ
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
                        "IRQ route: ISA IRQ {} -> GSI {} -> IOAPIC[{}] pin {} vector {} (flags={:#x})",
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
                        "IRQ route: ISA IRQ {} -> GSI {} (identity) -> IOAPIC[{}] pin {} vector {}",
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

/// Round-robin counter for distributing device IRQs across available CPUs.
static IRQ_CPU_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Program an IOAPIC entry for a route (still masked by default).
pub fn install_route(route: &IrqRoute) {
    if let Some(ioapic) = ioapic::get(route.ioapic_index) {
        let low = ioapic::IoApic::make_redir_low(route.vector, route.flags, true);
        // Distribute IRQs across online APs via round-robin.
        // Skip offline CPUs to avoid delivering to a non-existent APIC ID.
        let n = IRQ_CPU_NEXT.fetch_add(1, Ordering::Relaxed);
        let mut target_apic_id: u8 = 0; // fallback to BSP
        let mut found = false;
        for i in 0..MAX_CPUS {
            let cpu = (n + i) % MAX_CPUS;
            if crate::percpu::PERCPU[cpu].online.load(Ordering::Acquire) {
                target_apic_id = crate::percpu::PERCPU[cpu].apic_id.load(Ordering::Relaxed) as u8;
                if target_apic_id != u8::MAX {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            log::warn!("install_route: no online CPU with valid APIC ID, using BSP");
            target_apic_id = 0;
        }
        let high = ioapic::IoApic::make_redir_high(target_apic_id);
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

// ---- Runtime IRQ registration for drivers ----

use crate::vcpu::VcpuId;

/// Maximum number of runtime-registered IRQ handlers.
const MAX_RUNTIME_IRQS: usize = 32;

/// A runtime-registered IRQ handler: maps a vector to a driver vCPU.
#[derive(Clone, Copy)]
struct RuntimeIrq {
    vector: u8,
    gsi: u32,
    driver_vcpu: VcpuId,
    used: bool,
}

struct RuntimeIrqTable {
    entries: [RuntimeIrq; MAX_RUNTIME_IRQS],
    count: usize,
}

struct SyncRuntimeTable(UnsafeCell<RuntimeIrqTable>);
unsafe impl Sync for SyncRuntimeTable {}

static RUNTIME_IRQS: SyncRuntimeTable = SyncRuntimeTable(UnsafeCell::new(RuntimeIrqTable {
    entries: [RuntimeIrq { vector: 0, gsi: 0, driver_vcpu: 0, used: false }; MAX_RUNTIME_IRQS],
    count: 0,
}));

/// Register a runtime IRQ handler. The driver requests a GSI (or 0 for any
/// free vector) and provides its vCPU ID. Returns the assigned vector on
/// success, or None on failure.
pub fn register_irq(gsi: u32, driver_vcpu: VcpuId) -> Option<u8> {
    let table = unsafe { &mut *RUNTIME_IRQS.0.get() };

    // Find the GSI route to determine the IOAPIC pin and vector.
    let route = lookup_gsi(gsi)?;

    // Check for duplicate registration
    for i in 0..table.count {
        if table.entries[i].used && table.entries[i].driver_vcpu == driver_vcpu {
            log::warn!("intr: driver vcpu {} already has a registered IRQ", driver_vcpu);
            return None;
        }
    }

    if table.count >= MAX_RUNTIME_IRQS {
        log::error!("intr: runtime IRQ table full");
        return None;
    }

    let vector = route.vector;

    // Program the IOAPIC to deliver this GSI to the driver's CPU.
    // We use the existing install_route + enable_route path.
    install_route(route);
    enable_route(route);

    // Store the registration
    let idx = table.count;
    table.entries[idx] = RuntimeIrq {
        vector,
        gsi,
        driver_vcpu,
        used: true,
    };
    table.count += 1;

    log::info!("intr: registered runtime IRQ vector={} gsi={} for driver vcpu={}", vector, gsi, driver_vcpu);
    Some(vector)
}

/// Look up the driver vCPU for a given vector. Returns the vCPU ID if
/// a runtime handler is registered for this vector, or 0 if none.
pub fn lookup_vector_driver(vector: u8) -> VcpuId {
    let table = unsafe { &*RUNTIME_IRQS.0.get() };
    for i in 0..table.count {
        if table.entries[i].used && table.entries[i].vector == vector {
            return table.entries[i].driver_vcpu;
        }
    }
    0
}
