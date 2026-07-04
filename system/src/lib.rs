#![no_std]

pub const MAX_MEMORY_REGIONS: usize = 128;
pub const MAX_CPUS: usize = 4;

// ── Framebuffer driver commands ───────────────────────────────────
pub const FB_CMD_ACQUIRE: u32 = 0xFF;
pub const FB_CMD_SHOW_TEXT: u32 = 1;
pub const FB_CMD_CLEAR: u32 = 2;
pub const FB_CMD_SET_PIXEL: u32 = 3;
pub const FB_CMD_FILL_RECT: u32 = 4;
pub const FB_CMD_DRAW_TEXT: u32 = 5;
pub const FB_CMD_SET_FG: u32 = 6;
pub const FB_CMD_SET_BG: u32 = 7;
pub const FB_CMD_SCROLL: u32 = 8;
pub const FB_CMD_GET_INFO: u32 = 9;
pub const FB_CMD_PRESENT: u32 = 10;

/// Fixed physical address where the bootloader stores an 8-byte pointer
/// to the dynamically allocated BootInfo struct. The bootloader passes
/// this pointer to the kernel via RDI, but the address is also stored
/// at 0x5000 for debugging / fallback access.
///
/// 0x5000 is chosen to avoid the real-mode IVT (0x0–0x3FF), the BDA
/// (0x400–0x4FF), and the typical EBDA range, while still being below
/// the 1 MB mark for easy identity-map access during early boot.
pub const BOOT_INFO_HANDOFF_ADDR: u64 = 0x5000;

/// Passed from bootloader → kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    pub memory_region_count: usize,
    pub framebuffer: FramebufferInfo,
    /// Start LBA of the ext4 partition (Partition Zero).
    pub partition_zero_lba: u64,
    /// Size in bytes of the ext4 partition.
    pub partition_zero_size: u64,
    /// Kernel image loaded by bootloader from ext4 (staging buffer).
    pub kernel_image_addr: u64,
    /// Size in bytes of the kernel image.
    pub kernel_image_size: u64,
    /// Preloaded drivers ELF loaded by bootloader from ext4.
    pub drivers_elf_addr: u64,
    /// Size in bytes of the drivers ELF.
    pub drivers_elf_size: u64,
    /// Physical address of the RSDP (Root System Description Pointer),
    /// captured from UEFI config table before exit_boot_services.
    pub rsdp_addr: u64,
    /// Physical address of the MADT (APIC) table, discovered by kernel from RSDP.
    pub madt_addr: u64,
    /// Maximum number of CPUs the kernel will bring up.
    pub max_cpus: u32,
    /// BSP LAPIC ID (always 0 on x86).
    pub bsp_apic_id: u32,
    /// Number of enabled application processors (APs) reported by UEFI MP Services.
    pub ap_count: u32,
    /// LAPIC ID of each AP, indexed 0..ap_count.
    pub ap_apic_ids: [u32; MAX_CPUS],
}



#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub phys_start: u64,
    pub size: u64,
}

// ── Driver package manifest ───────────────────────────────────────
//
// The drivers.elf file on disk is NOT an ELF — it's a custom package
// format that bundles one or more standalone driver ELFs with a manifest.
//
// Layout:
//   [DriverPkgHeader]         8 + 4 = 12 bytes
//   [DriverPkgEntry × N]      N * 40 bytes
//   [driver ELF data 0]
//   [driver ELF data 1]
//   ...

pub const DRIVER_PKG_MAGIC: [u8; 8] = *b"LODAXPKG";

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DriverPkgHeader {
    pub magic: [u8; 8],
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DriverPkgEntry {
    pub name: [u8; 32],
    /// 0 = Hardware, 1 = Abstraction
    pub class: u32,
    /// Byte offset from end of manifest (i.e., after the last entry)
    pub elf_offset: u32,
    /// Size in bytes of the driver ELF
    pub elf_size: u32,
}

/// Max drivers that can be bundled in a single package.
pub const MAX_DRIVER_PKG_ENTRIES: usize = 32;

/// Driver package class mapping ─────────────────────────────────────
///
/// Class 0 = Hardware (device driver)
/// Class 1 = Abstraction (filesystem, higher-level service)
/// Class 2 = Loadable binary module
/// Class 3 = Plug 'n' play module
///
/// Legacy: Only 0 and 1 were previously supported.
pub const MAX_DRIVER_CLASSES: u32 = 4;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub phys_addr: u64,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub bytes_per_pixel: usize,
    pub is_bgr: bool,
}


