#![no_std]

pub const MAX_MEMORY_REGIONS: usize = 128;

/// Fixed physical address where the chainloader stores an 8-byte pointer
/// to the dynamically allocated BootInfo struct. The bootloader reads this
/// pointer, updates the BootInfo, and passes the pointer to the kernel.
/// This way the BootInfo itself (which is large — ~2 KB) lives at a
/// dynamically chosen address instead of a fragile fixed page.
pub const BOOT_INFO_HANDOFF_ADDR: u64 = 0x1000;

/// Passed from chainloader → bootloader → kernel.
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
    /// Physical address of the RSDP (Root System Description Pointer),
    /// captured from UEFI config table before exit_boot_services.
    pub rsdp_addr: u64,
    /// Physical address of the MADT (APIC) table, discovered by bootloader.
    pub madt_addr: u64,
    /// Physical address of the Secure Runtime ELF image in the staging buffer.
    pub sr_image_addr: u64,
    /// Size in bytes of the Secure Runtime ELF image.
    pub sr_image_size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub phys_start: u64,
    pub size: u64,
}

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
