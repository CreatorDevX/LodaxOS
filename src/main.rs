#![no_main]
#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

extern crate alloc;

mod acpi;
mod arch;
mod font;
mod intr;
mod logger;
mod mm;
mod serial;
mod task;

use uefi::boot::MemoryType;
use uefi::mem::memory_map::MemoryMap;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::prelude::*;

struct Framebuffer {
    ptr: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
    is_bgr: bool,
}

impl Framebuffer {
    fn new(gop: &mut GraphicsOutput) -> Self {
        let mode = gop.current_mode_info();
        let (width, height) = mode.resolution();
        let stride = mode.stride();
        let is_bgr = matches!(
            mode.pixel_format(),
            uefi::proto::console::gop::PixelFormat::Bgr
        );
        let mut fb = gop.frame_buffer();
        let ptr = fb.as_mut_ptr() as *mut u8;
        Self {
            ptr,
            width,
            height,
            stride,
            bytes_per_pixel: 4,
            is_bgr,
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

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    serial::init();
    logger::init().unwrap_or(());

    log::info!("LodaxOS booting");
    log::info!("Serial logger active on COM1 (115200 8N1)");

    // Clear UEFI text console
    if let Ok(h) = uefi::boot::get_handle_for_protocol::<uefi::proto::console::text::Output>()
    {
        if let Ok(mut out) = uefi::boot::open_protocol_exclusive::<
            uefi::proto::console::text::Output,
        >(h)
        {
            let _ = out.clear();
        }
    }

    // Get GOP
    let gop_handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>().expect("no GOP");
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .expect("failed to open GOP");

    if let Some(mode) = gop.modes().next() {
        let _ = gop.set_mode(&mode);
    }

    let mut fb = Framebuffer::new(&mut gop);

    let fb_phys_addr = fb.ptr as u64;
    let mode = gop.current_mode_info();
    let (fb_w, fb_h) = mode.resolution();
    let fb_stride = mode.stride();
    let fb_size = (fb_h * fb_stride * 4) as u64;

    log::debug!("GOP mode set: {}x{} stride={} {}", fb_w, fb_h, fb_stride, if fb.is_bgr { "BGR" } else { "RGB" });
    log::debug!("Framebuffer phys={:#x} size={} KB", fb_phys_addr, fb_size / 1024);

    log::info!("Phase 1: Memory initialization");

    fb.clear(0, 0, 30);
    fb.write_str_centered("LodaxOS", 10, 255, 255, 255);
    fb.write_str_centered("Initializing memory...", 30, 180, 180, 180);

    log::debug!("Collecting UEFI memory map");
    let memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA)
        .expect("failed to get memory map");

    static mut REGIONS: [(u64, u64); 128] = [(0, 0); 128];
    static mut REGION_COUNT: usize = 0;
    unsafe {
        for entry in memory_map.entries() {
            let is_free = matches!(
                entry.ty,
                MemoryType::CONVENTIONAL
                    | MemoryType::LOADER_CODE
                    | MemoryType::LOADER_DATA
            );
            if is_free && entry.page_count > 0 {
                if REGION_COUNT < 128 {
                    REGIONS[REGION_COUNT] = (entry.phys_start, entry.page_count * 4096);
                    REGION_COUNT += 1;
                }
            }
        }
    }

    let region_count = unsafe { REGION_COUNT };
    log::info!("{} usable memory regions found", region_count);

    unsafe {
        let count = region_count.min(10);
        for i in 0..count {
            let (start, size) = REGIONS[i];
            log::trace!("  region[{}]: phys={:#010x} size={} KB", i, start, size / 1024);
        }
        if region_count > 10 {
            log::trace!("  ... and {} more regions", region_count - 10);
        }
    }

    let (_total_free, free_mb) = unsafe {
        let total: u64 = REGIONS[..REGION_COUNT].iter().map(|(_, s)| s).sum();
        (total, total / (1024 * 1024))
    };
    log::info!("Free memory: {} MB", free_mb);

    log::debug!("Initializing physical page allocator (bitmap, up to 4 GB)");
    fb.write_str_centered("Physical allocator...", 50, 0, 255, 0);
    unsafe { mm::phys::init_from_regions(&REGIONS[..region_count], 0) };
    log::info!("Physical allocator ready");

    log::debug!("Initializing 4-level page tables (higher-half at {:#x})", 0xFFFF_8000_0000_0000u64);
    fb.write_str_centered("Page tables...", 70, 0, 255, 0);
    unsafe { mm::virt::init(&REGIONS[..region_count], Some((fb_phys_addr, fb_size))) };

    // After CR3 switch: framebuffer is only mapped in the higher half.
    fb.ptr = (0xFFFF_8000_0000_0000u64 + fb_phys_addr) as *mut u8;

    log::info!("Page tables ready");

    log::debug!("Initializing heap allocator (linked-list, up to 64 MB at {:#x})", 0xFFFF_8080_0000_0000u64);
    fb.write_str_centered("Heap allocator...", 90, 0, 255, 0);
    mm::heap::init();

    let heap_kb = mm::heap::heap_size() / 1024;
    log::info!("Heap ready: {} KB", heap_kb);

    // --- Phase 2: Escape UEFI ---
    log::warn!("Phase 2: Exiting UEFI boot services");
    fb.write_str_centered("Exiting boot services...", 110, 255, 255, 0);

    drop(gop);
    drop(memory_map);

    let _memory_map = unsafe { uefi::boot::exit_boot_services(None) };
    log::info!("UEFI boot services exited successfully");

    // Disable interrupts immediately — UEFI may have left PIT/HPET active,
    // and a timer interrupt before our IDT is loaded causes a triple fault.
    unsafe { core::arch::asm!("cli") };

    // Mask the legacy 8259 PIC — we use the LAPIC exclusively.
    // Without this, the PIC can deliver IRQs on vectors that collide
    // with CPU exceptions (e.g., IRQ 0 → vector 0x08 = #DF).
    arch::idt::mask_pic();

    // --- Phase 3: Post-UEFI ---
    log::info!("Phase 3: System running");

    // Map LAPIC MMIO first — this is pure page-table work, no segments needed.
    // Safe to do before loading our own GDT.
    log::info!("Mapping LAPIC MMIO region");
    arch::apic::init_mmio();

    // --- ACPI discovery + IOAPIC initialization ---
    log::info!("Initializing ACPI");
    let acpi_info = match acpi::init() {
        Some(info) => info,
        None => {
            log::error!("ACPI init failed");
            loop { unsafe { core::arch::asm!("cli; hlt") }; }
        }
    };

    let mut ioapic_infos = [acpi::madt::IoApicInfo {
        ioapic_id: 0,
        addr: 0,
        gsi_base: 0,
    }; acpi::madt::MAX_IOAPICS];
    let mut ioapic_count = 0usize;

    if let Some(madt_addr) = acpi_info.madt_addr {
        if let Some(madt) = acpi::madt::parse(madt_addr) {
            log::info!(
                "MADT: {} CPUs, {} IOAPICs, {} ISOs",
                madt.cpu_count,
                madt.ioapic_count,
                madt.iso_count
            );
            let mut n = 0;
            for i in 0..madt.ioapic_count {
                if let Some(info) = madt.ioapics[i] {
                    ioapic_infos[n] = info;
                    n += 1;
                }
            }
            ioapic_count = n;
            arch::ioapic::init(&ioapic_infos[..ioapic_count]);
            intr::init(&madt);
        }
    }

    // Build splash screen
    fb.clear(0, 0, 0);

    let mut y = 10;
    let line_h = font::GLYPH_HEIGHT + 4;

    fb.write_str_centered("LodaxOS", y, 0, 200, 255);
    y += line_h * 2;

    fb.write_str("Escaped UEFI boot services!", 20, y, 0, 255, 0);
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
    fb.write_str("[x] Physical allocator (bitmap)", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Page tables (4-level, higher-half)", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] Heap allocator (linked list)", 20, y, 0, 255, 0);
    y += line_h;
    let mut buf2 = [0u8; 16];
    let kb_str = format_heap_kb(heap_kb, &mut buf2);
    fb.write_str("Heap size: ", 20, y, 180, 180, 180);
    fb.write_str(kb_str, 20 + 11 * font::GLYPH_WIDTH, y, 0, 255, 0);
    fb.write_str(" KB", 20 + (11 + kb_str.len()) * font::GLYPH_WIDTH, y, 180, 180, 180);
    y += line_h;

    fb.write_str("[x] Boot services escaped", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] LAPIC MMIO mapped", 20, y, 0, 255, 0);
    y += line_h;
    fb.write_str("[x] ACPI tables", 20, y, if acpi_info.madt_addr.is_some() { 0 } else { 255 }, 255, 0);
    y += line_h;
    fb.write_str("[x] IOAPIC init", 20, y, if ioapic_count > 0 { 0 } else { 255 }, 255, 0);
    y += line_h;
    fb.write_str("[x] IRQ routing table", 20, y, if ioapic_count > 0 { 0 } else { 255 }, 255, 0);
    y += line_h + 4;
    fb.write_str("Loading GDT + TSS...", 20, y, 180, 180, 180);

    log::info!("Loading GDT and TSS");
    arch::gdt::load();
    // Set up IST1 for double-fault handler (vector 8, IST=1)
    let ist1_stack_top = unsafe { &raw mut arch::idt::IST1_STACK } as u64 + 16384;
    arch::gdt::set_ist1(ist1_stack_top);
    log::info!("GDT, TSS, and IST1 loaded");

    log::info!("Initializing IDT");
    fb.write_str("Loading IDT...", 20, y + line_h, 180, 180, 180);
    arch::idt::init();
    log::info!("IDT loaded — 256 vectors");

    // Initialize task system
    log::info!("Initializing task manager");
    task::init();
    task::init_main_task();

    // Switch the current RSP to task 0's allocated kernel stack.
    // main() never returns, so it is safe to abandon the old UEFI
    // boot‑services stack — locals are reachable through RBP.
    {
        let stack_top = task::task0_stack_top();
        unsafe {
            core::arch::asm!("mov rsp, {}", in(reg) stack_top, options(nostack));
        }
    }

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

    if ioapic_count > 0 {
        let routes = intr::install_and_enable_all();
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

    // Enable PIT channel 0 in rate‑generator mode so its interrupts fire on
    // vector 33 — used as a second device IRQ source alongside LAPIC timer.
    arch::apic::pit_enable_periodic(100);
    if let Some(route) = intr::lookup_isa(0) {
        log::info!("PIT: GSI {} → IOAPIC[{}] pin {} → vector {}",
            route.gsi, route.ioapic_index, route.ioapic_pin, route.vector);
        intr::enable_route(route);
        log::info!("PIT IOAPIC entry unmasked");
    }

    // Route keyboard (ISA IRQ 1) and unmask for Phase 1 validation
    if ioapic_count > 0 {
        if let Some(route) = intr::lookup_isa(1) {
            log::info!("Keyboard: GSI {} → IOAPIC[{}] pin {} → vector {}",
                route.gsi, route.ioapic_index, route.ioapic_pin, route.vector);
            intr::enable_route(route);
            log::info!("Keyboard IOAPIC entry unmasked");
        }
    }

    log::info!("Enabling interrupts");
    unsafe { core::arch::asm!("sti") };

    // Software interrupt test: trigger int 32 to verify stub independently of LAPIC
    log::info!("Triggering int 32 (software) to test IRQ stub...");
    unsafe { core::arch::asm!("int 32") };
    // The handler should have logged the vector before returning here.
    // Halt briefly to let any pending timer IRQ fire too.
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

fn format_heap_kb(kb: usize, buf: &mut [u8; 16]) -> &str {
    let mut tmp = [0u8; 12];
    let mut i = 0;
    let mut val = kb;
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

/// Test task 1: busy-loop counter, preemption handles switching
unsafe fn simple_task1() {
    let mut counter = 0u64;
    loop {
        counter += 1;
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
