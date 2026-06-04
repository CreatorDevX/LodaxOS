#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

mod acpi;
mod arch;
mod exec;
mod font;
mod intr;
mod logger;
mod mm;
mod serial;
mod task;

#[path = "../../src/ap_start.rs"]
mod ap_start;
#[path = "../../src/percpu.rs"]
mod percpu;

use lodaxos_system::BootInfo;

struct Framebuffer {
    ptr: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
    is_bgr: bool,
}

impl Framebuffer {
    fn from_info(info: &lodaxos_system::FramebufferInfo) -> Self {
        Self {
            ptr: info.phys_addr as *mut u8,
            width: info.width,
            height: info.height,
            stride: info.stride,
            bytes_per_pixel: info.bytes_per_pixel,
            is_bgr: info.is_bgr,
        }
    }

    fn set_pixel(&self, x: usize, y: usize, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let row_bytes = self.stride * self.bytes_per_pixel;
        let offset = y * row_bytes + x * self.bytes_per_pixel;
        unsafe {
            let p = self.ptr.add(offset);
            if self.is_bgr {
                p.write_volatile(b);
                p.add(1).write_volatile(g);
                p.add(2).write_volatile(r);
            } else {
                p.write_volatile(r);
                p.add(1).write_volatile(g);
                p.add(2).write_volatile(b);
            }
            p.add(3).write_volatile(0);
        }
    }

    fn clear(&mut self, r: u8, g: u8, b: u8) {
        let color: u32 = if self.is_bgr {
            b as u32 | (g as u32) << 8 | (r as u32) << 16
        } else {
            r as u32 | (g as u32) << 8 | (b as u32) << 16
        };
        let row_bytes = self.stride * self.bytes_per_pixel;
        for y in 0..self.height {
            let offset = y * row_bytes;
            unsafe {
                let base = self.ptr.add(offset) as *mut u32;
                for x in 0..self.width {
                    base.add(x).write_volatile(color);
                }
            }
        }
    }

    fn put_char(&mut self, ch: char, x: usize, y: usize, r: u8, g: u8, b: u8) {
        let glyph = font::get_glyph(ch);
        for gy in 0..font::GLYPH_HEIGHT {
            let row_bits = glyph[gy];
            for gx in 0..font::GLYPH_WIDTH {
                if (row_bits >> (7 - gx)) & 1 == 1 {
                    let px = x + gx;
                    let py = y + gy;
                    if px < self.width && py < self.height {
                        self.set_pixel(px, py, r, g, b);
                    }
                }
            }
        }
    }

    fn write_str(&mut self, s: &str, mut x: usize, mut y: usize, r: u8, g: u8, b: u8) {
        for ch in s.chars() {
            if ch == '\n' {
                x = 0;
                y += font::GLYPH_HEIGHT + 2;
                continue;
            }
            self.put_char(ch, x, y, r, g, b);
            x += font::GLYPH_WIDTH;
        }
    }

    fn write_str_centered(&mut self, s: &str, y: usize, r: u8, g: u8, b: u8) {
        let text_width = s.chars().count() * font::GLYPH_WIDTH;
        let x = (self.width.saturating_sub(text_width)) / 2;
        self.write_str(s, x, y, r, g, b);
    }
}

fn format_free_mb(mb: u64, buf: &mut [u8; 32]) -> &str {
    let mut tmp = [0u8; 20];
    let mut i = 0;
    let mut val = mb;
    if val == 0 {
        tmp[0] = b'0';
        i = 1;
    } else {
        while val > 0 {
            tmp[i] = b'0' + (val % 10) as u8;
            val /= 10;
            i += 1;
        }
    }
    let bytes = &tmp[..i];
    let len = bytes.len();
    buf[..len].copy_from_slice(bytes);
    buf[..len].reverse();
    core::str::from_utf8(&buf[..len]).unwrap_or("?")
}

/// Kernel entry point. Called by the bootloader after loading the ELF.
/// `boot_info` is a pointer to the BootInfo struct at physical address 0x1000.
#[unsafe(no_mangle)]
extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    let info = unsafe { &*boot_info };

    // Initialize serial + logger first (for debug output)
    serial::init();
    logger::init().unwrap_or(());

    log::info!("LodaxOS kernel booting");
    log::info!("BootInfo at {:#x}", boot_info as u64);

    // Build regions array from BootInfo
    let region_count = info.memory_region_count;
    let regions: [(u64, u64); 128] = core::array::from_fn(|i| {
        if i < region_count {
            (info.memory_regions[i].phys_start, info.memory_regions[i].size)
        } else {
            (0, 0)
        }
    });

    let (_total_free, free_mb) = {
        let total: u64 = regions[..region_count].iter().map(|(_, s)| s).sum();
        (total, total / (1024 * 1024))
    };
    log::info!("Free memory: {} MB", free_mb);

    // Initialize framebuffer from BootInfo
    let mut fb = Framebuffer::from_info(&info.framebuffer);

    log::info!("Phase 1: Memory initialization");
    fb.clear(0, 0, 30);
    fb.write_str_centered("LodaxOS", 10, 255, 255, 255);
    fb.write_str_centered("Kernel starting...", 30, 180, 180, 180);

    log::debug!("Initializing physical page allocator");
    fb.write_str_centered("Physical allocator...", 50, 0, 255, 0);
    unsafe { mm::phys::init_from_regions(&regions[..region_count], boot_info as u64) };
    log::info!("Physical allocator ready");

    // --- ACPI discovery (before CR3 switch — identity map covers physical addresses) ---
    log::info!("Reading ACPI tables");
    let madt_addr = if info.madt_addr != 0 {
        info.madt_addr
    } else {
        acpi::init(if info.rsdp_addr != 0 { Some(info.rsdp_addr) } else { None })
            .and_then(|ctx| ctx.madt_addr)
            .unwrap_or(0)
    };
    // `madt_addr` already contains a valid pointer; we just used the helper to
    // make sure the RSDP was reachable. Suppress unused-warning noise:

    let (ioapic_infos, ioapic_count, madt_parsed) = if madt_addr != 0 {
        log::info!("MADT at {:#x}", madt_addr);
        if let Some(madt) = acpi::madt::parse(madt_addr) {
            log::info!(
                "MADT: {} CPUs, {} IOAPICs, {} ISOs",
                madt.cpu_count,
                madt.ioapic_count,
                madt.iso_count
            );
            let mut ioapic_infos = [acpi::madt::IoApicInfo {
                ioapic_id: 0,
                addr: 0,
                gsi_base: 0,
            }; acpi::madt::MAX_IOAPICS];
            let mut n = 0;
            for i in 0..madt.ioapic_count {
                if let Some(info) = madt.ioapics[i] {
                    ioapic_infos[n] = info;
                    n += 1;
                }
            }
            (ioapic_infos, n, Some(madt))
        } else {
            log::warn!("MADT parse failed");
            ([acpi::madt::IoApicInfo { ioapic_id: 0, addr: 0, gsi_base: 0 }; acpi::madt::MAX_IOAPICS], 0, None)
        }
    } else {
        log::warn!("No MADT found");
        ([acpi::madt::IoApicInfo { ioapic_id: 0, addr: 0, gsi_base: 0 }; acpi::madt::MAX_IOAPICS], 0, None)
    };

    log::debug!("Initializing 4-level page tables");
    fb.write_str_centered("Page tables...", 70, 0, 255, 0);
    let fb_phys = info.framebuffer.phys_addr;
    let fb_size = (info.framebuffer.height * info.framebuffer.stride * info.framebuffer.bytes_per_pixel) as u64;
    unsafe { mm::virt::init(&regions[..region_count], Some((fb_phys, fb_size))) };

    // After CR3 switch: framebuffer is only mapped in the higher half.
    fb.ptr = (0xFFFF_8000_0000_0000u64 + fb_phys) as *mut u8;

    log::info!("Page tables ready");

    log::debug!("Initializing heap allocator");
    fb.write_str_centered("Heap allocator...", 90, 0, 255, 0);
    mm::heap::init();

    log::info!("Heap ready: slab allocator (32B..8KB caches)");

    log::debug!("Initializing kernel VMA tree for demand paging");
    fb.write_str_centered("Kernel VMAs...", 110, 0, 255, 0);
    mm::vma::init_kernel_vmas();

    // Disable interrupts — UEFI may have left PIT/HPET active
    unsafe { core::arch::asm!("cli") };

    // Mask the legacy 8259 PIC
    arch::idt::mask_pic();

    log::info!("Phase 2: Hardware init");

    // Map LAPIC MMIO
    log::info!("Mapping LAPIC MMIO region");
    fb.write_str_centered("LAPIC...", 150, 0, 255, 0);
    arch::apic::init_mmio();

    // --- IOAPIC + INTR init (after CR3 switch — MMIO mapped into new table's higher-half) ---
    if ioapic_count > 0 {
        arch::ioapic::init(&ioapic_infos[..ioapic_count]);
        if let Some(ref madt) = madt_parsed {
            intr::init(madt);
        }
    }

    // Build status screen
    fb.clear(0, 0, 0);

    let mut y = 10;
    let line_h = font::GLYPH_HEIGHT + 4;

    fb.write_str_centered("LodaxOS", y, 0, 200, 255);
    y += line_h * 2;

    fb.write_str("Kernel running!", 20, y, 0, 255, 0);
    y += line_h;

    fb.write_str("Physical memory:", 20, y, 180, 180, 180);
    y += line_h;
    let mut buf = [0u8; 32];
    let mb_str = format_free_mb(free_mb, &mut buf);
    fb.write_str("Total free: ", 20, y, 180, 180, 180);
    fb.write_str(mb_str, 20 + 12 * font::GLYPH_WIDTH, y, 0, 255, 0);
    fb.write_str(" MB", 20 + (12 + mb_str.len()) * font::GLYPH_WIDTH, y, 180, 180, 180);
    y += line_h;

    fb.write_str("Subsystems:", 20, y, 180, 180, 180);
    y += line_h;
    fb.write_str("[x] Physical allocator (buddy O(1))", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Page tables (4-level, higher-half)", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Slab allocator (SLUB, 32B..8KB)", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Kernel VMA tree (demand paging)", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Executive Runtime loaded", 20, y, 0, 255, 0);
    y += line_h;

    fb.write_str("[x] LAPIC MMIO mapped", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] ACPI tables", 20, y, if madt_parsed.is_some() { 0 } else { 255 }, 255, 0);
    y += line_h;
    fb.write_str("[x] IOAPIC init", 20, y, if ioapic_count > 0 { 0 } else { 255 }, 255, 0);
    y += line_h;
    fb.write_str("[x] IRQ routing table", 20, y, if ioapic_count > 0 { 0 } else { 255 }, 255, 0);
    y += line_h + 4;
    fb.write_str("Loading GDT + TSS...", 20, y, 180, 180, 180);

    log::info!("Loading GDT and TSS");
    arch::gdt::load();
    log::info!("GDT and TSS loaded");

    log::info!("Initializing IDT");
    fb.write_str("Loading IDT...", 20, y + line_h, 180, 180, 180);
    arch::idt::init();
    log::info!("IDT loaded — 256 vectors");

    // Initialize task system
    log::info!("Initializing task manager");
    task::init();
    task::init_main_task();

    // Create test kernel tasks
    let task1_entry = simple_task1 as *const () as u64;
    if let Some(task_id) = task::create_task(task1_entry) {
        log::info!("Created test task 1 with ID {}", task_id);
    }

    let task2_entry = simple_task2 as *const () as u64;
    if let Some(task_id) = task::create_task(task2_entry) {
        log::info!("Created test task 2 with ID {}", task_id);
    }

    log::info!("Task system ready — {} tasks registered", task::task_count());

    // Spawn Executive Runtime as a separate ring-0 process. Must be
    // AFTER task::init_main_task() so the task table is initialised
    // and the scheduler can find a slot. exec::load forks the kernel's
    // PML4, maps the ExRun ELF + mailbox into the new PML4, and
    // registers a Task. The next schedule() will time-slice it.
    log::info!("Spawning Executive Runtime as separate ring-0 process");
    fb.write_str("[ ] Executive Runtime...", 20, y, 180, 180, 180);
    match exec::load(&info) {
        Some(task_id) => {
            log::info!("ExRun: spawned as task {} (own PML4, shared mailbox)", task_id);
            fb.write_str("[x] Executive Runtime (separate PML4)", 20, y, 0, 255, 0);
        }
        None => {
            log::warn!("ExRun: not spawned (no image or cap denied)");
            fb.write_str("[!] Executive Runtime not spawned", 20, y, 255, 200, 0);
        }
    }
    y += line_h;

    if ioapic_count > 0 {
        let routes = intr::install_all_masked();
        log::info!("IOAPIC: {} routes installed (masked)", routes);
        fb.write_str("[x] IOAPIC routes", 20, y + line_h * 2, 0, 255, 0);
    }

    log::info!("Enabling LAPIC");
    arch::apic::enable();

    log::info!("Calibrating LAPIC timer against PIT (20 ms window)");
    arch::apic::calibrate_pit();

    log::info!("Configuring LAPIC timer: vector 32, periodic, 1 ms interval");
    arch::apic::configure_timer(16, 32, true);
    arch::apic::set_timer_count(1);

    arch::apic::pit_enable_periodic(100);

    // Mark the BSP online in the per-CPU table.
    arch::apic::set_bsp_lapic_id(info.bsp_apic_id);
    percpu::set_bsp_apic_id(info.bsp_apic_id);
    percpu::mark_online(info.bsp_apic_id);

    // Release APs brought up by the boot MP Services. Each AP will
    // enter `ap_entry` and block on `kernel_ready`.
    log::info!("SMP: releasing APs (ap_count={})", info.ap_count);
    ap_start::release_aps(info);

    // All CPUs (BSP + APs) may now enter the scheduler. APs are
    // spinning on `kernel_ready` in `ap_entry`; this releases them.
    percpu::release_all_aps();

    log::info!("Enabling interrupts");
    unsafe { core::arch::asm!("sti") };

    log::info!("Triggering int 32 (software) to test IRQ stub...");
    unsafe { core::arch::asm!("int 32") };

    // Unmask PIT + keyboard IOAPIC routes AFTER the int 32 test
    if let Some(route) = intr::lookup_isa(0) {
        log::info!("PIT: GSI {} → IOAPIC[{}] pin {} → vector {}",
            route.gsi, route.ioapic_index, route.ioapic_pin, route.vector);
        intr::enable_route(route);
        log::info!("PIT IOAPIC entry unmasked");
    }

    if ioapic_count > 0 {
        if let Some(route) = intr::lookup_isa(1) {
            log::info!("Keyboard: GSI {} → IOAPIC[{}] pin {} → vector {}",
                route.gsi, route.ioapic_index, route.ioapic_pin, route.vector);
            intr::enable_route(route);
            log::info!("Keyboard IOAPIC entry unmasked");
        }
    }

    for _ in 0..50000 {
        unsafe { core::arch::asm!("pause") };
    }

    log::info!("LodaxOS initialization complete — entering idle loop (task 0)");
    let mut last_log = 0u64;
    let mut last_key = 0u64;

    loop {
        unsafe { core::arch::asm!("hlt") };
        let now = arch::idt::ticks();
        if now - last_log >= 1000 {
            let pit = arch::idt::pit_ticks();
            let keys = arch::idt::key_count();
            if keys > last_key {
                last_key = keys;
                log::info!(
                    "[idle] tick: {} PIT: {} keys: {} (scancode {:#04x}) tasks: {}",
                    now, pit, keys, arch::idt::key_scancode(), task::task_count(),
                );
            } else {
                log::info!("[idle] tick: {} PIT: {} tasks: {}", now, pit, task::task_count());
            }
            last_log = now;
        }
    }
}

/// Test task 1: busy-loop counter, preemption handles switching
unsafe fn simple_task1() {
    let mut counter = 0u64;
    loop {
        counter += 1;
        if counter == 1_000_000 {
            // Diagnostic: read LAPIC timer registers to check if timer is alive
            const APIC_LVT_TIMER: usize = 0x320;
            const APIC_TICR: usize = 0x380;
            const APIC_CCR: usize = 0x390;
            let lvt = arch::apic::read32(APIC_LVT_TIMER);
            let ticr = arch::apic::read32(APIC_TICR);
            let ccr = arch::apic::read32(APIC_CCR);
            log::info!("[task1] LAPIC diag: LVT={:#x} TICR={} CCR={}", lvt, ticr, ccr);
        }
        if counter % 500_000 == 0 {
            log::info!("[task1] counter={}", counter);
        }
    }
}

/// Test task 2: busy-loop counter, preemption handles switching
unsafe fn simple_task2() {
    let mut counter = 0u64;
    loop {
        counter += 1;
        if counter % 750_000 == 0 {
            log::info!("[task2] counter={}", counter);
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(loc) = info.location() {
        serial::write_str("PANIC at ");
        serial::write_str(loc.file());
        serial::write_str(":");
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
            serial::write_str(core::str::from_utf8(&[b]).unwrap_or("?"));
        }
        serial::write_str("\n");
    }
    use core::fmt::Write;
    struct SerialWriter;
    impl Write for SerialWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            serial::write_str(s);
            Ok(())
        }
    }
    serial::write_str("  message: ");
    let _ = write!(SerialWriter, "{}", info.message());
    serial::write_str("\n");
    loop {
        unsafe { core::arch::asm!("cli; hlt") };
    }
}
