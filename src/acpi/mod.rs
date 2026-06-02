#![allow(dead_code)]

use core::mem;

pub mod madt;

/// Standard ACPI SDT header — every system description table starts with this.
#[repr(C, packed)]
pub struct SdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

/// RSDP (Root System Description Pointer) — ACPI v2.0+ extended format.
#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_addr: u32,
    length: u32,
    xsdt_addr: u64,
    ext_checksum: u8,
    reserved: [u8; 3],
}

pub const XSDT_SIG: [u8; 4] = *b"XSDT";
pub const MADT_SIG: [u8; 4] = *b"APIC";

/// Parsed ACPI context — populated once during boot.
pub struct AcpiContext {
    pub revision: u8,
    pub rsdp_addr: u64,
    pub xsdt_addr: u64,
    pub madt_addr: Option<u64>,
}

// ---- RSDP validation ----

fn rsdp_checksum_valid(rsdp: &Rsdp) -> bool {
    let len = if rsdp.revision >= 2 {
        mem::size_of::<Rsdp>()
    } else {
        20
    };
    let bytes = unsafe { core::slice::from_raw_parts(rsdp as *const Rsdp as *const u8, len) };
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
}

fn sdt_checksum_valid(addr: u64) -> bool {
    let header = unsafe { &*(addr as *const SdtHeader) };
    let len = header.length as usize;
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, len) };
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
}

fn check_rsdp_at(addr: u64) -> Option<u64> {
    let sig = unsafe { *(addr as *const [u8; 8]) };
    if &sig == b"RSD PTR " {
        let rsdp = unsafe { &*(addr as *const Rsdp) };
        if rsdp_checksum_valid(rsdp) {
            return Some(addr);
        }
    }
    None
}

/// Scan memory for the RSDP table.
///
/// The chainloader captures the UEFI config-table RSDP address into BootInfo
/// before ExitBootServices, so the kernel prefers that. If for some reason
/// the pointer is missing (e.g. legacy BIOS boot), we fall back to scanning
/// firmware-known regions:
///
/// 1. EBDA (Extended BIOS Data Area)
/// 2. Standard BIOS ROM area (0xE0000–0xFFFFF)
/// 3. Common OVMF/UEFI firmware locations near top of 4 GB
pub fn find_rsdp(hint: Option<u64>) -> Option<u64> {
    if let Some(addr) = hint {
        if check_rsdp_at(addr).is_some() {
            return Some(addr);
        }
    }

    // 1. EBDA
    unsafe {
        let ebda_seg = *(0x40E as *const u16) as u64;
        if ebda_seg != 0 {
            let ebda_start = ebda_seg << 4;
            let mut addr = ebda_start;
            while addr < ebda_start + 0x400 {
                if let Some(a) = check_rsdp_at(addr) {
                    return Some(a);
                }
                addr += 16;
            }
        }
    }

    // 2. Standard BIOS ROM area
    let mut addr = 0xE_0000u64;
    while addr < 0x10_0000 {
        if let Some(a) = check_rsdp_at(addr) {
            return Some(a);
        }
        addr += 16;
    }

    // 3. Common OVMF/UEFI firmware RSDP location near top of 4 GB.
    //    QEMU + OVMF places the RSDP at 0xFEFF_Cxxx or nearby.
    //    Scan 0xFEFF_0000..0xFF00_0000 (64 KB, 4096 iterations @ 16-byte stride).
    addr = 0xFEFF_0000u64;
    while addr < 0xFF00_0000u64 {
        if let Some(a) = check_rsdp_at(addr) {
            return Some(a);
        }
        addr += 16;
    }

    None
}

/// Parse the RSDP and return an AcpiContext with addresses of found tables.
pub fn init(hint: Option<u64>) -> AcpiContext {
    let rsdp_addr = find_rsdp(hint).expect("ACPI: RSDP not found");
    log::info!("ACPI: RSDP at {:#x}", rsdp_addr);

    let rsdp = unsafe { &*(rsdp_addr as *const Rsdp) };
    let rev = rsdp.revision;
    let xsdt_addr = if rev >= 2 && rsdp.xsdt_addr != 0 {
        rsdp.xsdt_addr
    } else {
        rsdp.rsdt_addr as u64
    };

    log::info!("ACPI: revision={} XSDT at {:#x}", rev, xsdt_addr);

    let madt_addr = find_sdt(xsdt_addr, &MADT_SIG);
    if let Some(addr) = madt_addr {
        log::info!("ACPI: MADT at {:#x}", addr);
    } else {
        log::warn!("ACPI: MADT not found");
    }

    AcpiContext {
        revision: rev,
        rsdp_addr,
        xsdt_addr,
        madt_addr,
    }
}

/// Find a system description table by signature within the XSDT.
pub fn find_sdt(xsdt_addr: u64, signature: &[u8; 4]) -> Option<u64> {
    let header = unsafe { &*(xsdt_addr as *const SdtHeader) };
    let entry_count = (header.length as usize - mem::size_of::<SdtHeader>()) / 8;

    let entries = xsdt_addr + mem::size_of::<SdtHeader>() as u64;
    for i in 0..entry_count {
        let entry_addr = unsafe { (entries as *const u64).add(i).read_unaligned() };

        if entry_addr == 0 {
            continue;
        }

        let entry_sig = unsafe { (*(entry_addr as *const SdtHeader)).signature };
        if &entry_sig == signature && sdt_checksum_valid(entry_addr) {
            return Some(entry_addr);
        }
    }
    None
}

/// Validate that a table's checksum covers the full table.
pub fn validate_table(addr: u64) -> bool {
    sdt_checksum_valid(addr)
}

/// RSDP (Root System Description Pointer) — ACPI v2.0+ extended format.
#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_addr: u32,
    length: u32,
    xsdt_addr: u64,
    ext_checksum: u8,
    reserved: [u8; 3],
}

pub const XSDT_SIG: [u8; 4] = *b"XSDT";
pub const MADT_SIG: [u8; 4] = *b"APIC";

/// Parsed ACPI context — populated once during boot.
pub struct AcpiContext {
    pub revision: u8,
    pub rsdp_addr: u64,
    pub xsdt_addr: u64,
    pub madt_addr: Option<u64>,
}

// ---- RSDP validation ----

fn rsdp_checksum_valid(rsdp: &Rsdp) -> bool {
    let len = if rsdp.revision >= 2 {
        mem::size_of::<Rsdp>()
    } else {
        20
    };
    let bytes = unsafe { core::slice::from_raw_parts(rsdp as *const Rsdp as *const u8, len) };
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
}

fn sdt_checksum_valid(addr: u64) -> bool {
    let header = unsafe { &*(addr as *const SdtHeader) };
    let len = header.length as usize;
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, len) };
    bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
}

fn check_rsdp_at(addr: u64) -> Option<u64> {
    let sig = unsafe { *(addr as *const [u8; 8]) };
    if &sig == b"RSD PTR " {
        let rsdp = unsafe { &*(addr as *const Rsdp) };
        if rsdp_checksum_valid(rsdp) {
            return Some(addr);
        }
    }
    None
}

/// Try to find the RSDP via the UEFI configuration table.
fn find_rsdp_from_uefi() -> Option<u64> {
    use uefi::system::with_config_table;
    use uefi::table::cfg::ConfigTableEntry;

    let mut rsdp = None;
    with_config_table(|entries| {
        for entry in entries {
            if entry.guid == ConfigTableEntry::ACPI2_GUID {
                rsdp = Some(entry.address as u64);
                break;
            }
        }
    });
    rsdp
}

/// Scan memory for the RSDP table.
///
/// Order:
/// 1. UEFI configuration table
/// 2. EBDA (Extended BIOS Data Area)
/// 3. Standard BIOS ROM area (0xE0000–0xFFFFF)
/// 4. Common OVMF/UEFI firmware locations near top of 4 GB
pub fn find_rsdp() -> Option<u64> {
    // 1. UEFI configuration table (most reliable on UEFI hardware)
    if let Some(addr) = find_rsdp_from_uefi() {
        return Some(addr);
    }

    // 2. EBDA
    unsafe {
        let ebda_seg = *(0x40E as *const u16) as u64;
        if ebda_seg != 0 {
            let ebda_start = ebda_seg << 4;
            let mut addr = ebda_start;
            while addr < ebda_start + 0x400 {
                if let Some(a) = check_rsdp_at(addr) {
                    return Some(a);
                }
                addr += 16;
            }
        }
    }

    // 3. Standard BIOS ROM area
    let mut addr = 0xE_0000u64;
    while addr < 0x10_0000 {
        if let Some(a) = check_rsdp_at(addr) {
            return Some(a);
        }
        addr += 16;
    }

    // 4. Common OVMF/UEFI firmware RSDP location near top of 4 GB.
    //    QEMU + OVMF places the RSDP at 0xFEFF_Cxxx or nearby.
    //    Scan 0xFEFF_0000..0xFF00_0000 (64 KB, 4096 iterations @ 16-byte stride).
    addr = 0xFEFF_0000u64;
    while addr < 0xFF00_0000u64 {
        if let Some(a) = check_rsdp_at(addr) {
            return Some(a);
        }
        addr += 16;
    }

    None
}

/// Parse the RSDP and return an AcpiContext with addresses of found tables.
pub fn init() -> AcpiContext {
    let rsdp_addr = find_rsdp().expect("ACPI: RSDP not found");
    log::info!("ACPI: RSDP at {:#x}", rsdp_addr);

    let rsdp = unsafe { &*(rsdp_addr as *const Rsdp) };
    let rev = rsdp.revision;
    let xsdt_addr = if rev >= 2 && rsdp.xsdt_addr != 0 {
        rsdp.xsdt_addr
    } else {
        rsdp.rsdt_addr as u64
    };

    log::info!("ACPI: revision={} XSDT at {:#x}", rev, xsdt_addr);

    let madt_addr = find_sdt(xsdt_addr, &MADT_SIG);
    if let Some(addr) = madt_addr {
        log::info!("ACPI: MADT at {:#x}", addr);
    } else {
        log::warn!("ACPI: MADT not found");
    }

    AcpiContext {
        revision: rev,
        rsdp_addr,
        xsdt_addr,
        madt_addr,
    }
}

/// Find a system description table by signature within the XSDT.
pub fn find_sdt(xsdt_addr: u64, signature: &[u8; 4]) -> Option<u64> {
    let header = unsafe { &*(xsdt_addr as *const SdtHeader) };
    let entry_count = (header.length as usize - mem::size_of::<SdtHeader>()) / 8;

    let entries = xsdt_addr + mem::size_of::<SdtHeader>() as u64;
    for i in 0..entry_count {
        let entry_addr = unsafe { (entries as *const u64).add(i).read_unaligned() };

        if entry_addr == 0 {
            continue;
        }

        let entry_sig = unsafe { (*(entry_addr as *const SdtHeader)).signature };
        if &entry_sig == signature && sdt_checksum_valid(entry_addr) {
            return Some(entry_addr);
        }
    }
    None
}

/// Validate that a table's checksum covers the full table.
pub fn validate_table(addr: u64) -> bool {
    sdt_checksum_valid(addr)
}
