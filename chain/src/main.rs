#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

use lodaxos_system::{BootInfo, BOOT_INFO_HANDOFF_ADDR, MemoryRegion, FramebufferInfo, MAX_MEMORY_REGIONS};

use uefi::prelude::*;
use uefi::cstr16;
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode};
use uefi::proto::console::gop::GraphicsOutput;
use uefi::mem::memory_map::{MemoryMap, MemoryType};

#[global_allocator]
static ALLOCATOR: uefi::allocator::Allocator = uefi::allocator::Allocator;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    serial_init();
    log::info!("LodaxOS chainloader starting");

    // Dynamically allocate BootInfo, store pointer at handoff address
    let boot_info = alloc::boxed::Box::<BootInfo>::new(lodaxos_system::BootInfo {
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
        rsdp_addr: 0,
        madt_addr: 0,
        sr_image_addr: 0,
        sr_image_size: 0,
    });
    let boot_info_ptr = alloc::boxed::Box::into_raw(boot_info) as *mut BootInfo;
    let boot_info_phys = boot_info_ptr as u64;
    unsafe {
        *(BOOT_INFO_HANDOFF_ADDR as *mut u64) = boot_info_phys;
    }
    log::info!("BootInfo allocated at {:#x}, pointer at {:#x}",
        boot_info_phys, BOOT_INFO_HANDOFF_ADDR);
    let boot_info = unsafe { &mut *boot_info_ptr };

    // --- Collect basic info for BootInfo ---
    // Memory map
    if let Ok(memory_map) = uefi::boot::memory_map(MemoryType::LOADER_DATA) {
        let mut count = 0usize;
        for entry in memory_map.entries() {
            let is_free = matches!(
                entry.ty,
                MemoryType::CONVENTIONAL | MemoryType::LOADER_CODE | MemoryType::LOADER_DATA
            );
            if is_free && entry.page_count > 0 && count < MAX_MEMORY_REGIONS {
                boot_info.memory_regions[count] = MemoryRegion {
                    phys_start: entry.phys_start,
                    size: entry.page_count * 4096,
                };
                count += 1;
            }
        }
        boot_info.memory_region_count = count;
        log::info!("{} memory regions", count);
    }

    // Framebuffer (basic info — bootloader will refine)
    if let Ok(gop_handle) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if let Ok(mut gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle) {
            let mode = gop.current_mode_info();
            let (w, h) = mode.resolution();
            let stride = mode.stride();
            let is_bgr = matches!(
                mode.pixel_format(),
                uefi::proto::console::gop::PixelFormat::Bgr
            );
            let mut fb = gop.frame_buffer();
            boot_info.framebuffer = FramebufferInfo {
                phys_addr: fb.as_mut_ptr() as u64,
                width: w,
                height: h,
                stride,
                bytes_per_pixel: 4,
                is_bgr,
            };
            log::info!("Framebuffer: {}x{}", w, h);
        }
    }

    // --- Read bootloader from ESP root ---
    log::info!("Reading Bootloader.efi from ESP root");
    let bootloader_bytes = match read_file_from_image_root(cstr16!("Bootloader.efi")) {
        Some(data) => {
            log::info!("Bootloader.efi: {} bytes", data.len());
            data
        }
        None => {
            log::error!("Failed to read Bootloader.efi from ESP root");
            return Status::LOAD_ERROR;
        }
    };

    // kernel_image_addr/size are zero — bootloader reads kernel from ext4 itself

    // --- Load and start Bootloader.efi ---
    log::info!("Loading Bootloader.efi as UEFI image");
    let parent_handle = uefi::boot::image_handle();
    let source = uefi::boot::LoadImageSource::FromBuffer {
        buffer: &bootloader_bytes,
        file_path: None,
    };
    let image_handle = match uefi::boot::load_image(parent_handle, source) {
        Ok(h) => h,
        Err(e) => {
            log::error!("load_image failed: {:?}", e.status());
            return e.status();
        }
    };

    log::info!("Starting Bootloader.efi");
    match uefi::boot::start_image(image_handle) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

fn read_file_from_image_root(path: &uefi::CStr16) -> Option<alloc::vec::Vec<u8>> {
    let mut fs = uefi::boot::get_image_file_system(uefi::boot::image_handle()).ok()?;
    let mut root = fs.open_volume().ok()?;
    let file = root
        .open(path, FileMode::Read, FileAttribute::empty())
        .ok()?;
    let mut file = file.into_regular_file()?;

    let mut info_buf = [0u8; 1024];
    let info: &mut FileInfo = file.get_info(&mut info_buf).ok()?;
    let size = info.file_size() as usize;
    let mut data = alloc::vec![0u8; size];
    let read = file.read(&mut data).ok()?;
    data.truncate(read);
    Some(data)
}

// ---- Minimal serial driver ----

fn serial_init() {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") 0x00u8);
        core::arch::asm!("out dx, al", in("dx") 0x3FBu16, in("al") 0x80u8);
        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") 0x01u8);
        core::arch::asm!("out dx, al", in("dx") 0x3F9u16, in("al") 0x00u8);
        core::arch::asm!("out dx, al", in("dx") 0x3FBu16, in("al") 0x03u8);
        core::arch::asm!("out dx, al", in("dx") 0x3FAu16, in("al") 0xC7u8);
        core::arch::asm!("out dx, al", in("dx") 0x3F9u16, in("al") 0x0Bu8);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Write panic to serial
    for b in b"PANIC: " {
        unsafe {
            let mut r = 100_000u32;
            loop {
                let lsr: u8;
                core::arch::asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16);
                if lsr & 0x20 != 0 || r == 0 {
                    break;
                }
                r -= 1;
            }
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") *b);
        }
    }
    loop {
        unsafe { core::arch::asm!("cli; hlt") };
    }
}
