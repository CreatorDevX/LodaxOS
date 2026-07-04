#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

mod acpi;
mod arch;
mod intr;
mod logger;
mod mm;
mod serial;

#[cfg(debug_assertions)]
mod serial2;
mod katerm;
mod sync;
mod scheduler;
mod vcpu;
mod exec;
mod service;
mod gdf;

mod ap_start;
mod percpu;
mod consts;

use core::sync::atomic::Ordering;
use lodaxos_system::{BootInfo, FB_CMD_ACQUIRE, FB_CMD_DRAW_TEXT};

/// Kernel entry point. Called by the bootloader after loading the ELF.
#[unsafe(no_mangle)]
extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    let info = unsafe { &*boot_info };

    // Initialize serial + logger first (for debug output)
    serial::init();
    logger::init().unwrap_or(());

    // Enable FPU and SSE on the BSP.
    unsafe {
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        let mut cr4 = x86_64::registers::control::Cr4::read();
        cr4 |= x86_64::registers::control::Cr4Flags::OSFXSR
             | x86_64::registers::control::Cr4Flags::OSXMMEXCPT_ENABLE;
        if has_xsave() {
            cr4 |= x86_64::registers::control::Cr4Flags::OSXSAVE;
        }
        x86_64::registers::control::Cr4::write(cr4);
    }

    log::info!("LodaxOS kernel booting");
    log::info!("BootInfo at {:#x}", boot_info as u64);

    let (regions, region_count, _free_mb, (kernel_start, kernel_size)) = build_memory_layout(info);

    log::info!("Phase 1: Memory initialization");

    // Build exclude ranges for the physical allocator: framebuffer pages,
    // kernel staging image, drivers ELF staging, and APIC MMIO.
    let mut exclude_list: [(u64, u64); 8] = [(0, 0); 8];
    let mut exclude_count = 0usize;
    {
        let fb_base = info.framebuffer.phys_addr & !0xFFFu64;
        let fb_size = (info.framebuffer.height as u64)
            .saturating_mul(info.framebuffer.stride as u64)
            .saturating_mul(info.framebuffer.bytes_per_pixel as u64);
        let fb_end = info.framebuffer.phys_addr.saturating_add(fb_size);
        let fb_bytes = fb_end - fb_base;
        if fb_bytes > 0 {
            exclude_list[exclude_count] = (fb_base, fb_bytes);
            exclude_count += 1;
        }
    }
    if info.kernel_image_addr != 0 && info.kernel_image_size != 0 {
        exclude_list[exclude_count] = (info.kernel_image_addr, info.kernel_image_size);
        exclude_count += 1;
    }
    if info.drivers_elf_addr != 0 && info.drivers_elf_size != 0 {
        exclude_list[exclude_count] = (info.drivers_elf_addr, info.drivers_elf_size);
        exclude_count += 1;
    }
    exclude_list[exclude_count] = (
        consts::APIC_MMIO_BASE,
        consts::APIC_MMIO_SIZE,
    );
    exclude_count += 1;

    log::info!("Initializing physical page allocator");
    unsafe { mm::phys::init_from_regions(&regions[..region_count], boot_info as u64, &exclude_list[..exclude_count]) };
    log::info!("Physical allocator ready");

    let (ioapic_infos, ioapic_count, madt_parsed) = discover_acpi(info);

    arch::smp::init();

    log::info!("Initializing 4-level page tables");
    let fb_phys = info.framebuffer.phys_addr;
    let fb_size = (info.framebuffer.height * info.framebuffer.stride * info.framebuffer.bytes_per_pixel) as u64;
    let kernel_phys_range = if kernel_size > 0 { Some((kernel_start, kernel_size)) } else { None };
    unsafe { mm::virt::init(&regions[..region_count], Some((fb_phys, fb_size)), kernel_phys_range) };
    log::info!("Page tables ready");

    log::info!("Initializing heap allocator");
    mm::heap::init();
    log::info!("Heap ready: slab allocator (32B..8KB caches)");

    log::info!("Initializing kernel VMA tree for demand paging");
    mm::vma::init_kernel_vmas();

    log::info!("Initializing VCPU slab");
    vcpu::init();

    log::info!("Initializing SEDS scheduler");
    scheduler::init();

    // Disable interrupts — UEFI may have left PIT/HPET active
    x86_64::instructions::interrupts::disable();

    // Mask the legacy 8259 PIC
    arch::idt::mask_pic();

    log::info!("Phase 2: Hardware init");

    // Map LAPIC MMIO
    log::info!("Mapping LAPIC MMIO region");
    arch::apic::init_mmio();

    if ioapic_count > 0 {
        arch::ioapic::init(&ioapic_infos[..ioapic_count]);
        if let Some(ref madt) = madt_parsed {
            intr::init(madt);
        }
    }

    log::info!("Loading GDT + TSS...");

    arch::apic::set_bsp_lapic_id(info.bsp_apic_id);
    percpu::set_bsp_apic_id(info.bsp_apic_id);
    percpu::mark_online(info.bsp_apic_id);
    let bsp_slot = percpu::apic_id_to_slot(info.bsp_apic_id);

    log::info!("Loading GDT and TSS");
    unsafe { arch::gdt::init_for_slot(bsp_slot); }
    log::info!("GDT and TSS loaded");

    log::info!("Initializing IDT");
    arch::idt::init();
    log::info!("IDT loaded — 256 vectors");

    percpu::install_gs_base(bsp_slot);
    crate::logger::BSP_PERCPU_READY.store(true, Ordering::Release);

    scheduler::init_idle_vcpu();

    // ── Compute framebuffer size for ACQUIRE command ────────────
    let fb_size = (info.framebuffer.height as u64)
        .saturating_mul(info.framebuffer.stride as u64)
        .saturating_mul(info.framebuffer.bytes_per_pixel as u64);

    log::info!("Framebuffer: {}x{} stride={} bpp={} is_bgr={} size={}",
        info.framebuffer.width as u64, info.framebuffer.height as u64,
        info.framebuffer.stride as u64 * info.framebuffer.bytes_per_pixel as u64,
        info.framebuffer.bytes_per_pixel, info.framebuffer.is_bgr, fb_size);

    // ── Load and start the preloaded drivers ELF ──────────────────────
    if info.drivers_elf_addr != 0 && info.drivers_elf_size != 0 {
        let binary: &'static [u8] = unsafe {
            core::slice::from_raw_parts(
                info.drivers_elf_addr as *const u8,
                info.drivers_elf_size as usize,
            )
        };
        log::info!("gdf: loading driver package ({} bytes)", binary.len());
        
        // Use try_lock to avoid blocking potential AP startup during GDF initialization
        let mut loaded = false;
        for _ in 0..1000 {
            if gdf::try_init_from_package(binary) {
                loaded = true;
                break;
            }
            for _ in 0..10000 { core::hint::spin_loop(); }
        }
        if !loaded {
            log::error!("gdf: timed out trying to init package");
        }
    } else {
        log::warn!("gdf: no drivers ELF in BootInfo — skipping");
    }

    // ── IOAPIC routes (after GDF so drivers can register IRQs) ────
    if ioapic_count > 0 {
        let routes = intr::install_all_masked();
        log::info!("IOAPIC: {} routes installed (masked)", routes);
    }

    #[cfg(debug_assertions)]
    {
        serial2::init();
        log::info!("COM2 debug serial init @ divisor=1 (115200 baud)");
    }

    log::info!("Enabling LAPIC");
    arch::apic::enable();

    log::info!("Calibrating LAPIC timer against PIT (20 ms window)");
    arch::apic::calibrate_pit();

    log::info!("Configuring LAPIC timer: vector 32, periodic, 1 ms interval");
    arch::apic::configure_timer(16, 32, true);
    arch::apic::set_timer_count(1);

    arch::apic::pit_enable_periodic(100);

    // Bring up APs
    log::info!("SMP: booting {} AP(s) via INIT-SIPI-SIPI", info.ap_count);
    percpu::release_all_aps();
    ap_start::smp_boot_aps(info);

    log::info!("Enabling interrupts");
    x86_64::instructions::interrupts::enable();

    log::info!("Triggering int 32 (software) to test IRQ stub...");
    unsafe { core::arch::asm!("int 32") };

    // Unmask PIT IOAPIC route
    if let Some(route) = intr::lookup_isa(0) {
        log::info!("PIT: GSI {} → IOAPIC[{}] pin {} → vector {}",
            route.gsi, route.ioapic_index, route.ioapic_pin, route.vector);
        intr::enable_route(route);
    }

    // ── Read /file.txt via ext4 ──────────────────────────────────
    log::info!("Waiting for ext4 driver...");
    let mut ext4_ready = false;
    for _ in 0..1000 {
        if gdf::find_by_name(b"ext4").is_some() { ext4_ready = true; break; }
        for _ in 0..10000 { core::hint::spin_loop(); }
    }

    let mut file_phys = 0u64;
    let mut file_size = 0u64;
    if ext4_ready {
        log::info!("Sending READ_FILE (cmd=1) to ext4...");
        if gdf::send_cmd(b"ext4", 1, 0, 0, 0) {
            for _ in 0..1000 {
                if let Some(res) = gdf::poll_result(b"ext4") {
                    if res != u64::MAX {
                        file_phys = res;
                        break;
                    }
                }
                for _ in 0..10000 { core::hint::spin_loop(); }
            }
        }

        if file_phys != 0 {
            log::info!("Sending GET_SIZE (cmd=2) to ext4...");
            if gdf::send_cmd(b"ext4", 2, 0, 0, 0) {
                for _ in 0..1000 {
                    if let Some(sz) = gdf::poll_result(b"ext4") {
                        file_size = sz;
                        break;
                    }
                    for _ in 0..10000 { core::hint::spin_loop(); }
                }
            }
            log::info!("ext4: file at {:#x}, {} bytes", file_phys, file_size);
            // Dump file content to serial for verification
            if file_size > 0 && file_size <= 8192 {
                let hh = mm::virt::HIGHER_HALF;
                let data = unsafe { core::slice::from_raw_parts((hh + file_phys) as *const u8, file_size as usize) };
                let s = core::str::from_utf8(data).unwrap_or("<invalid utf-8>");
                serial::write_str_unlocked("\n--- file.txt content ---\n");
                serial::write_str_unlocked(s);
                serial::write_str_unlocked("\n--- end ---\n");
            }
        } else {
            log::warn!("ext4: READ_FILE failed");
        }
    } else {
        log::warn!("ext4 driver did not register within timeout");
    }

    // ── Initialize framebuffer driver via capability protocol ────
    log::info!("Waiting for framebuffer driver...");
    let mut fb_ready = false;
    for _ in 0..1000 {
        if gdf::find_by_name(b"framebuffer").is_some() { fb_ready = true; break; }
        for _ in 0..10000 { core::hint::spin_loop(); }
    }

    if fb_ready {
        // Pack framebuffer geometry into arg2:
        //   bits  0-15: width
        //   bits 16-31: height
        //   bits 32-47: stride (bytes per row)
        //   bits 48-55: bpp
        //   bit     56: is_bgr
        //   bits 57-63: flags
        let w = info.framebuffer.width as u64;
        let h = info.framebuffer.height as u64;
        let stride = (info.framebuffer.stride * info.framebuffer.bytes_per_pixel) as u64;
        let packed = w | (h << 16) | (stride << 32)
            | ((info.framebuffer.bytes_per_pixel as u64) << 48)
            | ((info.framebuffer.is_bgr as u64) << 56);
        log::info!("Sending FB_CMD_ACQUIRE to framebuffer (phys={:#x}, size={}, packed={:#x})",
            info.framebuffer.phys_addr, fb_size, packed);
        if gdf::send_cmd(b"framebuffer", FB_CMD_ACQUIRE, info.framebuffer.phys_addr, fb_size, packed) {
            // Poll for ACK
            let mut acked = false;
            for _ in 0..1000 {
                if let Some(res) = gdf::poll_result(b"framebuffer") {
                    if res == 0 {
                        acked = true;
                        break;
                    }
                    break;
                }
                for _ in 0..10000 { core::hint::spin_loop(); }
            }
            if !acked {
                log::error!("framebuffer did not ACK ACQUIRE");
                fb_ready = false;
            }
        } else {
            log::error!("Failed to send ACQUIRE to framebuffer");
            fb_ready = false;
        }
    }

    // ── Send file content to framebuffer ─────────────────────────
    if fb_ready && file_phys != 0 {
        log::info!("Sending FB_CMD_DRAW_TEXT to framebuffer ({} bytes)", file_size);
        let sent = gdf::send_cmd(b"framebuffer", FB_CMD_DRAW_TEXT, file_phys, file_size, 0);
        if sent {
            log::info!("DRAW_TEXT sent to framebuffer driver");
        } else {
            log::error!("Failed to send DRAW_TEXT (mailbox busy?)");
        }
    } else if fb_ready {
        log::info!("No file content — sending empty DRAW_TEXT");
        gdf::send_cmd(b"framebuffer", FB_CMD_DRAW_TEXT, 0, 0, 0);
    } else {
        log::warn!("Framebuffer driver not available");
    }

    for _ in 0..50000 { core::hint::spin_loop(); }

    log::info!("LodaxOS initialization complete — entering idle loop (task 0)");
    bsp_idle_loop();
}

/// BSP idle loop: hlt-wait, periodic log.
fn bsp_idle_loop() -> ! {
    let mut last_log = 0u64;
    let bsp_cpu = percpu::current_apic_id() as usize;
    loop {
        katerm::process_input();

        x86_64::instructions::hlt();
        if percpu::task_count(bsp_cpu) <= 1 {
            scheduler::steal_task(bsp_cpu);
        }
        let now = arch::idt::ticks();
        if now - last_log >= 1000 {
            log::info!("[idle] tick: {} tasks: {}", now, scheduler::task_count());
            last_log = now;
        }
    }
}

/// Build the free-memory region list from BootInfo, excising the kernel
/// image range so the physical allocator never touches it.
fn build_memory_layout(info: &BootInfo) -> ([(u64, u64); 256], usize, u64, (u64, u64)) {
    unsafe extern "C" {
        static __kernel_start: u8;
        static __kernel_end: u8;
    }
    let kernel_start = &raw const __kernel_start as u64;
    let kernel_end = (&raw const __kernel_end as u64 + 0xFFF) & !0xFFFu64;
    let kernel_size = kernel_end - kernel_start;
    log::info!("Kernel image: {:#x}..{:#x} ({} KB)", kernel_start, kernel_end, kernel_size / 1024);

    let region_count = info.memory_region_count.min(lodaxos_system::MAX_MEMORY_REGIONS);
    let raw_regions: [(u64, u64); 128] = core::array::from_fn(|i| {
        if i < region_count {
            (info.memory_regions[i].phys_start, info.memory_regions[i].size)
        } else {
            (0, 0)
        }
    });

    let mut regions: [(u64, u64); 256] = [(0, 0); 256];
    let mut nregions = 0usize;
    for i in 0..region_count {
        let (rstart, rsize) = raw_regions[i];
        if rsize == 0 { continue; }
        let rend = rstart.saturating_add(rsize);
        let rstart = (rstart + 0xFFF) & !0xFFFu64;
        let rend = rend & !0xFFFu64;
        if rstart >= rend { continue; }
        if kernel_size > 0 && rstart < kernel_end && rend > kernel_start {
            if rstart < kernel_start && nregions < 256 {
                regions[nregions] = (rstart, kernel_start - rstart);
                nregions += 1;
            }
            if kernel_end < rend && nregions < 256 {
                regions[nregions] = (kernel_end, rend - kernel_end);
                nregions += 1;
            }
        } else {
            if nregions < 256 {
                regions[nregions] = (rstart, rsize);
                nregions += 1;
            }
        }
    }
    let free_mb = regions[..nregions].iter().map(|(_, s)| s).sum::<u64>() / (1024 * 1024);
    log::info!("Free memory: {} MB", free_mb);
    (regions, nregions, free_mb, (kernel_start, kernel_size))
}

/// Discover ACPI tables (MADT / IOAPIC info).
fn discover_acpi(info: &BootInfo) -> ([acpi::madt::IoApicInfo; acpi::madt::MAX_IOAPICS], usize, Option<acpi::madt::MadtInfo>) {
    log::info!("Reading ACPI tables");
    let madt_addr = if info.madt_addr != 0 {
        info.madt_addr
    } else {
        acpi::init(if info.rsdp_addr != 0 { Some(info.rsdp_addr) } else { None })
            .and_then(|ctx| ctx.madt_addr)
            .unwrap_or(0)
    };
    if madt_addr == 0 {
        log::warn!("No MADT found");
        return ([acpi::madt::IoApicInfo { ioapic_id: 0, addr: 0, gsi_base: 0 }; acpi::madt::MAX_IOAPICS], 0, None);
    }
    log::info!("MADT at {:#x}", madt_addr);
    let madt = match acpi::madt::parse(madt_addr) {
        Some(m) => m,
        None => {
            log::warn!("MADT parse failed");
            return ([acpi::madt::IoApicInfo { ioapic_id: 0, addr: 0, gsi_base: 0 }; acpi::madt::MAX_IOAPICS], 0, None);
        }
    };
    log::info!("MADT: {} CPUs, {} IOAPICs, {} ISOs", madt.cpu_count, madt.ioapic_count, madt.iso_count);
    let mut ioapic_infos = [acpi::madt::IoApicInfo { ioapic_id: 0, addr: 0, gsi_base: 0 }; acpi::madt::MAX_IOAPICS];
    let mut n = 0;
    for i in 0..madt.ioapic_count {
        if let Some(ioapic) = madt.ioapics[i] {
            ioapic_infos[n] = ioapic;
            n += 1;
        }
    }
    (ioapic_infos, n, Some(madt))
}

/// Check CPUID.1.ECX[26] for XSAVE support.
pub(crate) fn has_xsave() -> bool {
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {0:e}, ecx",
            "pop rbx",
            out(reg) ecx,
            out("eax") _,
            out("edx") _,
        );
    }
    (ecx & (1 << 26)) != 0
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(loc) = info.location() {
        serial::write_str_unlocked("PANIC at ");
        serial::write_str_unlocked(loc.file());
        serial::write_str_unlocked(":");
        let mut line_buf = [0u8; 10];
        let mut val = loc.line();
        let mut i = 0;
        if val == 0 {
            line_buf[0] = b'0';
            i = 1;
        } else {
            while val > 0 {
                line_buf[i] = b'0' + (val % 10) as u8;
                val /= 10;
                i += 1;
            }
        }
        for &b in line_buf[..i].iter().rev() {
            serial::write_str_unlocked(core::str::from_utf8(&[b]).unwrap_or("?"));
        }
        serial::write_str_unlocked("\n");
    }
    use core::fmt::Write;
    struct SerialWriter;
    impl Write for SerialWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            serial::write_str_unlocked(s);
            Ok(())
        }
    }
    serial::write_str_unlocked("  message: ");
    let _ = write!(SerialWriter, "{}", info.message());
    serial::write_str_unlocked("\n");
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}
