use core::sync::atomic::{AtomicBool, Ordering};

use crate::mm::virt;

// ---- IOAPIC MMIO register offsets ----
const IOAPIC_IOREGSEL: u64 = 0x00;
const IOAPIC_IOWIN: u64 = 0x10;
const IOAPIC_ID: u32 = 0x00;
const IOAPIC_VER: u32 = 0x01;
const IOAPIC_ARB: u32 = 0x02;
const IOAPIC_REDIR_BASE: u32 = 0x10;

// ---- Redirection entry low DWORD bit fields ----
const IOAPIC_LOW_VECTOR: u32 = 0x0000_00FF;
const IOAPIC_LOW_DELIVERY_FIXED: u32 = 0;
const IOAPIC_LOW_DEST_PHYSICAL: u32 = 0;
const IOAPIC_LOW_POLARITY_HIGH: u32 = 0;
const IOAPIC_LOW_POLARITY_LOW: u32 = 0x0000_2000;
const IOAPIC_LOW_TRIGGER_EDGE: u32 = 0;
const IOAPIC_LOW_TRIGGER_LEVEL: u32 = 0x0000_8000;
const IOAPIC_LOW_MASKED: u32 = 0x0001_0000;

// ---- Capacity ----
pub const MAX_IOAPICS: usize = crate::acpi::madt::MAX_IOAPICS;

/// A discovered and mapped IOAPIC instance.
#[derive(Debug, Clone, Copy)]
pub struct IoApic {
    pub virt_base: u64,
    pub id: u8,
    pub version: u8,
    pub max_redir: u8,
    pub gsi_base: u32,
}

impl IoApic {
    unsafe fn read_reg(&self, index: u32) -> u32 {
        let sel = self.virt_base as *mut u32;
        sel.write_volatile(index);
        let win = (self.virt_base + IOAPIC_IOWIN) as *const u32;
        win.read_volatile()
    }

    unsafe fn write_reg(&self, index: u32, value: u32) {
        let sel = self.virt_base as *mut u32;
        sel.write_volatile(index);
        let win = (self.virt_base + IOAPIC_IOWIN) as *mut u32;
        win.write_volatile(value);
    }

    /// Read the IOAPIC ID, version, and max redirection entry index.
    unsafe fn read_hardware_id(&self) -> u8 {
        (self.read_reg(IOAPIC_ID) >> 24) as u8
    }

    unsafe fn read_version(&self) -> (u8, u8) {
        let ver = self.read_reg(IOAPIC_VER);
        ((ver & 0xFF) as u8, ((ver >> 16) & 0xFF) as u8)
    }

    /// Mask a single redirection entry (set bit 16).
    pub fn mask_entry(&self, pin: u8) {
        let reg = IOAPIC_REDIR_BASE + (pin as u32) * 2;
        unsafe {
            let low = self.read_reg(reg);
            self.write_reg(reg, low | IOAPIC_LOW_MASKED);
        }
    }

    /// Unmask a single redirection entry (clear bit 16).
    pub fn unmask_entry(&self, pin: u8) {
        let reg = IOAPIC_REDIR_BASE + (pin as u32) * 2;
        unsafe {
            let low = self.read_reg(reg);
            self.write_reg(reg, low & !IOAPIC_LOW_MASKED);
        }
    }

    /// Program a redirection entry.
    /// `low` contains vector, delivery mode, polarity, trigger, etc.
    /// `high` contains destination APIC ID in bits 56-63.
    pub fn set_entry(&self, pin: u8, low: u32, high: u32) {
        let reg = IOAPIC_REDIR_BASE + (pin as u32) * 2;
        unsafe {
            self.write_reg(reg, low);
            self.write_reg(reg + 1, high);
        }
    }

    /// Read back a redirection entry's raw low + high values.
    pub fn get_entry(&self, pin: u8) -> (u32, u32) {
        let reg = IOAPIC_REDIR_BASE + (pin as u32) * 2;
        unsafe {
            let low = self.read_reg(reg);
            let high = self.read_reg(reg + 1);
            (low, high)
        }
    }

    /// Build the low DWORD for a redirection entry.
    pub fn make_redir_low(vector: u8, flags: u16, masked: bool) -> u32 {
        let vector_part = (vector as u32) & IOAPIC_LOW_VECTOR;
        let polarity = if flags & (1 << 1) != 0 {
            IOAPIC_LOW_POLARITY_LOW
        } else {
            IOAPIC_LOW_POLARITY_HIGH
        };
        let trigger = if flags & (1 << 3) != 0 {
            IOAPIC_LOW_TRIGGER_LEVEL
        } else {
            IOAPIC_LOW_TRIGGER_EDGE
        };
        let mask = if masked {
            IOAPIC_LOW_MASKED
        } else {
            0
        };
        vector_part | IOAPIC_LOW_DELIVERY_FIXED | IOAPIC_LOW_DEST_PHYSICAL | polarity | trigger
            | mask
    }

    /// Build the high DWORD for a redirection entry (destination APIC ID = 0 for BSP).
    pub fn make_redir_high(apic_id: u8) -> u32 {
        (apic_id as u32) << 24
    }
}

// ---- Global IOAPIC array ----

struct IoApicCell(core::cell::UnsafeCell<Option<IoApic>>);
unsafe impl Sync for IoApicCell {}

const IOAPIC_NONE: IoApicCell = IoApicCell(core::cell::UnsafeCell::new(None));

static IOAPICS: [IoApicCell; MAX_IOAPICS] = [IOAPIC_NONE; MAX_IOAPICS];

static IOAPIC_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize all IOAPICs discovered from MADT.
/// Maps MMIO regions, reads hardware ID/version, masks all entries.
pub fn init(ioapic_infos: &[crate::acpi::madt::IoApicInfo]) {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    let pml4 = virt::pml4_address();
    let mut count = 0;

    for info in ioapic_infos {
        let phys = info.addr as u64;
        let gsi_base = info.gsi_base;

        // Map IOAPIC MMIO (4 KB, higher-half only)
        let flags =
            virt::PRESENT | virt::WRITABLE | virt::NO_EXECUTE | virt::CACHE_DISABLE;
        virt::map_region_higher_half(pml4, phys, 0x1000, flags);

        let virt_base = virt::HIGHER_HALF + phys;
        let ioapic = IoApic {
            virt_base,
            id: 0,
            version: 0,
            max_redir: 0,
            gsi_base,
        };

        unsafe {
            let id = ioapic.read_hardware_id();
            let (ver, max_redir) = ioapic.read_version();
            let id_reg = ioapic.read_reg(IOAPIC_ID);
            let hw_ioapic = IoApic {
                virt_base,
                id,
                version: ver,
                max_redir,
                gsi_base,
            };

            log::info!(
                "IOAPIC[{}]: id={} ver={} max_redir={} addr={:#x} gsi_base={} (raw ID reg={:#x})",
                count,
                id,
                ver,
                max_redir,
                phys,
                gsi_base,
                id_reg,
            );

            // Program all redirection entries to a safe vector (spurious 0xFF)
            // and mask them.  The reset / UEFI state may leave unused pins with
            // vector 0 and unmasked — if any device asserts such a pin, the
            // IOAPIC delivers an interrupt with vector 0, which QEMU prints as
            // a warning and the CPU ignores (but the LAPIC may also fire its
            // uninitialised Error LVT).
            for pin in 0..=max_redir {
                hw_ioapic.set_entry(pin, IoApic::make_redir_low(0xFF, 0, true), 0);
            }

            log::debug!("IOAPIC[{}]: initialised {} redirection entries (masked, vector 0xFF)", count, max_redir + 1);

            *IOAPICS[count].0.get() = Some(hw_ioapic);
        }

        count += 1;
    }

    IOAPIC_COUNT.store(count, Ordering::Relaxed);
    INITIALIZED.store(true, Ordering::Release);

    log::info!("IOAPIC: {} controller(s) initialized", count);
}

/// Returns true if IOAPICs are initialized.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Get a reference to an IOAPIC by index.
pub fn get(index: usize) -> Option<&'static IoApic> {
    if index >= MAX_IOAPICS {
        return None;
    }
    unsafe { (*IOAPICS[index].0.get()).as_ref() }
}

/// Number of IOAPICs discovered.
pub fn count() -> usize {
    IOAPIC_COUNT.load(Ordering::Relaxed)
}

/// Find the IOAPIC that handles a given GSI, and return (ioapic_index, pin).
pub fn lookup_gsi(gsi: u32) -> Option<(usize, u8)> {
    let count = IOAPIC_COUNT.load(Ordering::Relaxed);
    for i in 0..count {
        if let Some(ioapic) = get(i) {
            let pin = gsi.wrapping_sub(ioapic.gsi_base);
            if pin <= ioapic.max_redir as u32 {
                return Some((i, pin as u8));
            }
        }
    }
    None
}
