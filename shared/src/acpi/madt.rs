#![allow(dead_code)]

use core::mem;

use crate::acpi::SdtHeader;

// ---- MADT entry type constants ----
const MADT_LOCAL_APIC: u8 = 0;
const MADT_IOAPIC: u8 = 1;
const MADT_ISO: u8 = 2;
const MADT_NMI: u8 = 4;
const MADT_LOCAL_APIC_OVERRIDE: u8 = 5;
const MADT_IOAPIC_NMI: u8 = 6;

// ---- Capacity limits ----
pub const MAX_IOAPICS: usize = 16;
pub const MAX_ISOS: usize = 32;
pub const MAX_CPUS: usize = 32;

// ---- Parsed structures ----

#[derive(Debug, Clone, Copy)]
pub struct CpuInfo {
    pub acpi_id: u8,
    pub apic_id: u8,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct IoApicInfo {
    pub ioapic_id: u8,
    pub addr: u32,
    pub gsi_base: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct IsoInfo {
    pub bus: u8,
    pub source: u8,
    pub gsi: u32,
    pub flags: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct MadtInfo {
    pub local_apic_addr: u32,
    pub flags: u32,
    pub cpus: [Option<CpuInfo>; MAX_CPUS],
    pub cpu_count: usize,
    pub ioapics: [Option<IoApicInfo>; MAX_IOAPICS],
    pub ioapic_count: usize,
    pub isos: [Option<IsoInfo>; MAX_ISOS],
    pub iso_count: usize,
    pub has_apic_addr_override: bool,
    pub apic_addr_override: u64,
}

/// Entry header that precedes every MADT entry.
#[repr(C, packed)]
struct MadtEntryHeader {
    entry_type: u8,
    record_length: u8,
}

#[repr(C, packed)]
struct MadtEntryLocalApic {
    header: MadtEntryHeader,
    acpi_processor_id: u8,
    apic_id: u8,
    flags: u32,
}

#[repr(C, packed)]
struct MadtEntryIoApic {
    header: MadtEntryHeader,
    ioapic_id: u8,
    reserved: u8,
    ioapic_addr: u32,
    gsi_base: u32,
}

#[repr(C, packed)]
struct MadtEntryIso {
    header: MadtEntryHeader,
    bus: u8,
    source: u8,
    gsi: u32,
    flags: u16,
}

#[repr(C, packed)]
struct MadtEntryLocalApicOverride {
    header: MadtEntryHeader,
    reserved: u16,
    apic_addr: u64,
}

#[repr(C, packed)]
struct MadtEntryNmi {
    header: MadtEntryHeader,
    acpi_processor_id: u8,
    flags: u16,
    lint: u8,
}

#[repr(C, packed)]
struct MadtEntryIoApicNmi {
    header: MadtEntryHeader,
    ioapic_id: u8,
    _reserved: u8,
    flags: u16,
    gsi: u32,
}

/// Parse the MADT (Multiple APIC Description Table) at `madt_addr`.
pub fn parse(addr: u64) -> Option<MadtInfo> {
    let header = unsafe { &*(addr as *const SdtHeader) };
    if &header.signature != b"APIC" {
        log::error!("MADT: bad signature");
        return None;
    }
    if !crate::acpi::validate_table(addr) {
        log::error!("MADT: checksum invalid");
        return None;
    }

    let madt_len = header.length as usize;
    let madt_base = addr as usize;
    let hdr_size = mem::size_of::<SdtHeader>();

    // MADT-specific fields start after SDT header.
    // struct MadtFixed {
    //   header: SdtHeader,        // 36 bytes (actually varies, but sizeof SdtHeader works)
    //   local_apic_addr: u32,     // +36
    //   flags: u32,               // +40
    // }
    let madt_ptr = madt_base as *const u8;
    let local_apic_addr = unsafe { (madt_ptr.add(hdr_size) as *const u32).read_volatile() };
    let flags = unsafe { (madt_ptr.add(hdr_size + 4) as *const u32).read_volatile() };

    let mut info = MadtInfo {
        local_apic_addr,
        flags,
        cpus: [None; MAX_CPUS],
        cpu_count: 0,
        ioapics: [None; MAX_IOAPICS],
        ioapic_count: 0,
        isos: [None; MAX_ISOS],
        iso_count: 0,
        has_apic_addr_override: false,
        apic_addr_override: 0,
    };

    // Walk entries
    let mut offset = madt_base + hdr_size + 8; // skip SDT header + madt-specific 8 bytes
    let end = madt_base + madt_len;
    while offset + 2 <= end {
        let entry_header = unsafe { &*(offset as *const MadtEntryHeader) };
        let entry_type = entry_header.entry_type;
        let record_len = entry_header.record_length as usize;
        if record_len < 2 {
            break;
        }
        if offset + record_len > end {
            break;
        }

        parse_entry(&mut info, entry_type, offset, record_len);

        offset += record_len;
    }

    log::info!(
        "MADT: {} CPUs, {} IOAPICs, {} ISOs",
        info.cpu_count,
        info.ioapic_count,
        info.iso_count
    );

    Some(info)
}

fn parse_entry(info: &mut MadtInfo, entry_type: u8, offset: usize, _len: usize) {
    match entry_type {
        MADT_LOCAL_APIC => {
            let entry = unsafe { &*(offset as *const MadtEntryLocalApic) };
            let cpu = CpuInfo {
                acpi_id: entry.acpi_processor_id,
                apic_id: entry.apic_id,
                enabled: entry.flags & 1 != 0,
            };
            if info.cpu_count < MAX_CPUS {
                info.cpus[info.cpu_count] = Some(cpu);
                info.cpu_count += 1;
            }
        }

        MADT_IOAPIC => {
            let entry = unsafe { &*(offset as *const MadtEntryIoApic) };
            let ioapic = IoApicInfo {
                ioapic_id: entry.ioapic_id,
                addr: entry.ioapic_addr,
                gsi_base: entry.gsi_base,
            };
            if info.ioapic_count < MAX_IOAPICS {
                info.ioapics[info.ioapic_count] = Some(ioapic);
                info.ioapic_count += 1;
            }
        }

        MADT_ISO => {
            let entry = unsafe { &*(offset as *const MadtEntryIso) };
            let iso = IsoInfo {
                bus: entry.bus,
                source: entry.source,
                gsi: entry.gsi,
                flags: entry.flags,
            };
            if info.iso_count < MAX_ISOS {
                info.isos[info.iso_count] = Some(iso);
                info.iso_count += 1;
            }
        }

        MADT_LOCAL_APIC_OVERRIDE => {
            let entry = unsafe { &*(offset as *const MadtEntryLocalApicOverride) };
            info.has_apic_addr_override = true;
            info.apic_addr_override = entry.apic_addr;
        }

        MADT_NMI => {
            let entry = unsafe { &*(offset as *const MadtEntryNmi) };
            let cpu = entry.acpi_processor_id;
            let flags = entry.flags;
            let lint = entry.lint;
            log::trace!("MADT: NMI cpu={} flags={:#x} lint={}", cpu, flags, lint);
        }

        MADT_IOAPIC_NMI => {
            let entry = unsafe { &*(offset as *const MadtEntryIoApicNmi) };
            let ioapic = entry.ioapic_id;
            let flags = entry.flags;
            let gsi = entry.gsi;
            log::trace!("MADT: IOAPIC NMI ioapic={} flags={:#x} gsi={}", ioapic, flags, gsi);
        }

        _ => {
            log::trace!("MADT: unknown entry type {} (len {})", entry_type, _len);
        }
    }
}

/// Given an ISA IRQ source, find the corresponding GSI from parsed ISOs.
/// If no ISO matches, the GSI is the same as the source (identity mapping).
pub fn lookup_gsi(info: &MadtInfo, isa_irq: u8) -> u32 {
    for iso in info.isos.iter().flatten() {
        if iso.bus == 0 && iso.source == isa_irq {
            return iso.gsi;
        }
    }
    // No ISO: identity mapping
    isa_irq as u32
}

/// Find which IOAPIC handles a given GSI.
pub fn lookup_ioapic(info: &MadtInfo, gsi: u32) -> Option<(usize, u8)> {
    for (i, ioapic) in info.ioapics.iter().enumerate() {
        if let Some(io) = ioapic {
            let pin = gsi.wrapping_sub(io.gsi_base);
            if pin < 256 {
                return Some((i, pin as u8));
            }
        }
    }
    None
}
