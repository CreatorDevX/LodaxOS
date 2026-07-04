#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

use lodaxos_system::{BootInfo, BOOT_INFO_HANDOFF_ADDR, MAX_MEMORY_REGIONS, MemoryRegion, FramebufferInfo};

mod load_kernel;
mod mp;
mod serial;
mod logger;

use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::table::cfg::ConfigTableEntry;

#[global_allocator]
static ALLOCATOR: uefi::allocator::Allocator = uefi::allocator::Allocator;

fn collect_usable_memory_regions(boot_info: &mut BootInfo) {
    boot_info.memory_regions = [MemoryRegion { phys_start: 0, size: 0 }; MAX_MEMORY_REGIONS];
    boot_info.memory_region_count = 0;

    if let Ok(memory_map) = uefi::boot::memory_map(MemoryType::LOADER_DATA) {
        let mut region_count = 0usize;
        for entry in memory_map.entries() {
            if matches!(entry.ty, MemoryType::CONVENTIONAL)
                && entry.page_count > 0
                && region_count < MAX_MEMORY_REGIONS
            {
                boot_info.memory_regions[region_count] = MemoryRegion {
                    phys_start: entry.phys_start,
                    size: entry.page_count * 4096,
                };
                region_count += 1;
            }
        }
        boot_info.memory_region_count = region_count;
        log::info!("{} usable memory regions", region_count);
    } else {
        log::error!("Failed to collect UEFI memory map");
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    serial::init();
    if logger::init().is_err() {
        serial::write_str("LOGGER INIT FAILED\n");
    }
    log::info!("LodaxOS bootloader starting");

    // Allocate BootInfo and store pointer at handoff address
    let boot_info = alloc::boxed::Box::<BootInfo>::new(BootInfo {
        memory_regions: [MemoryRegion { phys_start: 0, size: 0 }; MAX_MEMORY_REGIONS],
        memory_region_count: 0,
        framebuffer: FramebufferInfo {
            phys_addr: 0,
            width: 0,
            height: 0,
            stride: 0,
            bytes_per_pixel: 4,
            is_bgr: false,
        },
        partition_zero_lba: 0,
        partition_zero_size: 0,
        kernel_image_addr: 0,
        kernel_image_size: 0,
        drivers_elf_addr: 0,
        drivers_elf_size: 0,
        rsdp_addr: 0,
        madt_addr: 0,
        max_cpus: lodaxos_system::MAX_CPUS as u32,
        bsp_apic_id: 0,
        ap_count: 0,
        ap_apic_ids: [0u32; lodaxos_system::MAX_CPUS],
    });
    let boot_info_ptr = alloc::boxed::Box::into_raw(boot_info) as *mut BootInfo;
    let boot_info_addr = boot_info_ptr as u64;
    unsafe {
        *(BOOT_INFO_HANDOFF_ADDR as *mut u64) = boot_info_addr;
    }
    log::info!("BootInfo allocated at {:#x}, pointer at {:#x}",
        boot_info_addr, BOOT_INFO_HANDOFF_ADDR);
    let mut boot_info = unsafe { *boot_info_ptr };

    // --- Framebuffer via GOP (prefer 1024x768, fall back to highest resolution) ---
    if let Ok(gop_handle) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
            // First pass: look for exactly 1024x768.
            const TARGET_W: usize = 1024;
            const TARGET_H: usize = 768;
            let mut best_mode = None;
            for mode in gop.modes() {
                let (w, h) = mode.info().resolution();
                if w == TARGET_W && h == TARGET_H {
                    best_mode = Some(mode);
                    break;
                }
            }
            // Second pass: if no exact match, pick the highest pixel count.
            if best_mode.is_none() {
                let mut best_pixels = 0u64;
                for mode in gop.modes() {
                    let (w, h) = mode.info().resolution();
                    let pixels = (w as u64) * (h as u64);
                    if pixels > best_pixels {
                        best_pixels = pixels;
                        best_mode = Some(mode);
                    }
                }
            }
            if let Some(mode) = best_mode {
                if gop.set_mode(&mode).is_err() {
                    log::warn!("GOP: failed to set best mode, using current");
                }
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

    // Store kernel image in BootInfo staging (accessible after exit_boot_services).
    // WARNING: This memory was allocated by the UEFI allocator. After ExitBootServices
    // the UEFI allocator is non-functional. The kernel must copy the image or mark
    // this region as reserved before allowing any allocator to touch it.
    boot_info.kernel_image_addr = kernel_elf_data.as_ptr() as u64;
    boot_info.kernel_image_size = kernel_elf_data.len() as u64;

    // --- Load drivers.elf from ext4 partition ---
    log::info!("Loading drivers.elf from ext4 partition");
    let drivers_elf_data = load_kernel::load_file_from_ext4(b"drivers.elf");
    if let Some(data) = drivers_elf_data {
        log::info!("drivers.elf: {} bytes", data.len());
        boot_info.drivers_elf_addr = data.as_ptr() as u64;
        boot_info.drivers_elf_size = data.len() as u64;
        // Leaked: never dropped because we jump to kernel (noreturn).
        core::mem::forget(data);
    } else {
        log::warn!("drivers.elf not found on ext4 — continuing without it");
        boot_info.drivers_elf_addr = 0;
        boot_info.drivers_elf_size = 0;
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

    // --- Enumerate APs via UEFI MP Services ---
    //
    // We only read the LAPIC IDs — the kernel brings APs up via
    // LAPIC INIT-SIPI-SIPI after ExitBootServices.
    log::info!("Enumerating APs via UEFI MP Services");
    if let Err(e) = mp::enumerate_aps(&mut boot_info) {
        log::warn!("MP Services enumeration failed: {:?} (continuing with BSP only)", e.status());
    }
    unsafe {
        (*boot_info_ptr) = boot_info;
    }
    log::info!("BootInfo updated with AP info: {} APs", boot_info.ap_count);

    // Collect the final usable map after all bootloader allocations. Earlier
    // snapshots can mark pages as free before the ext4 reader, kernel staging
    // buffer, or MP services allocate them.
    collect_usable_memory_regions(&mut boot_info);
    unsafe {
        (*boot_info_ptr) = boot_info;
    }

    // --- Load kernel ELF into memory (before ExitBootServices — UEFI identity map still active) ---
    log::info!("Loading kernel ELF into memory");
    let entry = match load_kernel::load_elf(&kernel_elf_data, &boot_info.memory_regions, boot_info.memory_region_count) {
        Some(addr) => addr,
        None => {
            log::error!("Failed to parse kernel ELF");
            halt();
        }
    };
    log::info!("Kernel entry: {:#x}", entry);

    // --- Exit boot services ---
    log::warn!("Exiting UEFI boot services");
    let _mmap = unsafe { uefi::boot::exit_boot_services(None) };
    log::info!("Boot services exited");

    x86_64::instructions::interrupts::disable();

    // --- Jump to kernel ---
    log::info!("Jumping to kernel at {:#x} with BootInfo at {:#x}", entry, boot_info_addr);
    unsafe {
        core::arch::asm!(
            // Align RSP to 16 bytes, then push a fake return address so that
            // RSP mod 16 = 8 at kernel entry (SysV ABI requirement: right
            // before a call, RSP is 16-byte aligned; after a call, RSP mod 16 = 8).
            "and rsp, -16",
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
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
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
