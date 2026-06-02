#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

use lodaxos_system::{BootInfo, BOOT_INFO_HANDOFF_ADDR, MAX_MEMORY_REGIONS, MemoryRegion, FramebufferInfo};

mod load_kernel;
mod serial;
mod logger;

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::table::cfg::ConfigTableEntry;

#[global_allocator]
static ALLOCATOR: uefi::allocator::Allocator = uefi::allocator::Allocator;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    serial::init();
    logger::init().unwrap_or(());
    log::info!("LodaxOS bootloader starting");

    // Read BootInfo pointer from handoff address, then dereference
    let boot_info_addr = unsafe { *(BOOT_INFO_HANDOFF_ADDR as *const u64) };
    log::info!("BootInfo pointer from handoff: {:#x}", boot_info_addr);
    let boot_info_ptr = boot_info_addr as *mut BootInfo;
    let mut boot_info = unsafe { *boot_info_ptr };

    // --- Framebuffer via GOP ---
    if let Ok(gop_handle) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
            if let Some(mode) = gop.modes().next() {
                let _ = gop.set_mode(&mode);
            }
            let mode = gop.current_mode_info();
            let (w, h) = mode.resolution();
            let stride = mode.stride();
            let is_bgr = matches!(
                mode.pixel_format(),
                uefi::proto::console::gop::PixelFormat::Bgr
            );
            let mut fb = gop.frame_buffer();
            let ptr = fb.as_mut_ptr() as u64;

            boot_info.framebuffer = FramebufferInfo {
                phys_addr: ptr,
                width: w,
                height: h,
                stride,
                bytes_per_pixel: 4,
                is_bgr,
            };
            log::info!("Framebuffer: {}x{}", w, h);
        }
    }

    // --- Collect UEFI memory map ---
    let memory_map_result = uefi::boot::memory_map(MemoryType::LOADER_DATA);
    if let Ok(memory_map) = memory_map_result {
        let mut region_count = 0usize;
        for entry in memory_map.entries() {
            let is_free = matches!(
                entry.ty,
                MemoryType::CONVENTIONAL | MemoryType::LOADER_CODE | MemoryType::LOADER_DATA
            );
            if is_free && entry.page_count > 0 && region_count < MAX_MEMORY_REGIONS {
                boot_info.memory_regions[region_count] = MemoryRegion {
                    phys_start: entry.phys_start,
                    size: entry.page_count * 4096,
                };
                region_count += 1;
            }
        }
        boot_info.memory_region_count = region_count;
        log::info!("{} usable memory regions", region_count);
    }

    // --- Load kernel ELF from ext4 partition ---
    log::info!("Loading kernel.elf from ext4 partition");
    let kernel_elf_data = match load_kernel::load_kernel_from_ext4() {
        Some(data) => data,
        None => {
            log::error!("Failed to load kernel.elf from ext4");
            return Status::LOAD_ERROR;
        }
    };
    log::info!("kernel.elf: {} bytes", kernel_elf_data.len());

    // Store kernel image in BootInfo staging (accessible after exit_boot_services)
    boot_info.kernel_image_addr = kernel_elf_data.as_ptr() as u64;
    boot_info.kernel_image_size = kernel_elf_data.len() as u64;

    // --- Load SR ELF from ext4 partition ---
    log::info!("Loading sr.elf from ext4 partition");
    let sr_elf_data = match load_kernel::load_sr_from_ext4() {
        Some(data) => data,
        None => {
            log::warn!("sr.elf not found; continuing without Secure Runtime");
            alloc::vec::Vec::new()
        }
    };
    if !sr_elf_data.is_empty() {
        boot_info.sr_image_addr = sr_elf_data.as_ptr() as u64;
        boot_info.sr_image_size = sr_elf_data.len() as u64;
        log::info!("sr.elf: {} bytes, at {:#x}", sr_elf_data.len(), boot_info.sr_image_addr);
    }

    // --- Capture RSDP from UEFI config table (before exit_boot_services — UEFI addresses valid) ---
    log::info!("Capturing RSDP from UEFI config table");
    let rsdp_addr: u64 = {
        let mut addr = 0u64;
        uefi::system::with_config_table(|entries| {
            for entry in entries {
                if entry.guid == ConfigTableEntry::ACPI2_GUID
                    || entry.guid == ConfigTableEntry::ACPI_GUID
                {
                    addr = entry.address as u64;
                    break;
                }
            }
        });
        addr
    };
    boot_info.rsdp_addr = rsdp_addr;

    // --- Write updated BootInfo before exit_boot_services ---
    unsafe {
        (*boot_info_ptr) = boot_info;
    }
    log::info!("BootInfo updated at {:#x}", boot_info_addr);

    // --- Exit boot services ---
    log::warn!("Exiting UEFI boot services");
    let _mmap = unsafe { uefi::boot::exit_boot_services(None) };
    log::info!("Boot services exited");

    unsafe { core::arch::asm!("cli") };

    // --- Load kernel ELF ---
    log::info!("Loading kernel ELF into memory");
    let entry = match load_kernel::load_elf(&kernel_elf_data) {
        Some(addr) => addr,
        None => {
            log::error!("Failed to parse kernel ELF");
            halt();
        }
    };
    log::info!("Kernel entry: {:#x}", entry);

    // --- Jump to kernel ---
    log::info!("Jumping to kernel at {:#x} with BootInfo at {:#x}", entry, boot_info_addr);
    unsafe {
        core::arch::asm!(
            // Simulate the missing call (SysV ABI expects RSP mod 16 = 8 at entry)
            "sub rsp, 8",
            "mov rdi, {boot_info}",
            "jmp {entry}",
            boot_info = in(reg) boot_info_addr,
            entry = in(reg) entry,
            options(noreturn)
        );
    }
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt") };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    struct SerialWriter;
    impl Write for SerialWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            serial::write_str(s);
            Ok(())
        }
    }

    serial::write_str("PANIC");
    if let Some(loc) = info.location() {
        serial::write_str(" at ");
        serial::write_str(loc.file());
        serial::write_str(":");
        let mut buf = [0u8; 10];
        let mut n = loc.line();
        if n == 0 {
            serial::write_str("0");
        } else {
            let mut i = 0usize;
            while n > 0 {
                buf[i] = b'0' + (n % 10) as u8;
                n /= 10;
                i += 1;
            }
            for b in buf[..i].iter().rev() {
                serial::write_str(core::str::from_utf8(&[*b]).unwrap_or("?"));
            }
        }
    }
    serial::write_str(" ");
    let _ = write!(SerialWriter, "{}", info.message());
    serial::write_str("\n");
    halt();
}
