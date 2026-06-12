use core::arch::asm;

use lodaxos_system::{BootInfo, MAX_CPUS};

use crate::percpu;

/// Number of AP kernel stack pages (4 × 4 KiB = 16 KiB per AP).
const AP_STACK_PAGES: usize = 4;

/// Flush a single cache line (64 bytes).  Required on WHPX and some other
/// virtualisation backends: plain stores from the BSP to the SIPI mailbox,
/// GDT, or TSS may be cached in the BSP's L1 and not visible to the AP
/// after SIPI, even after `mfence`.  `clflush` evicts the line from all
/// cache levels, and the trailing `mfence` serialises the flush.
#[inline(always)]
pub unsafe fn clflush(ptr: *const u8) {
    asm!(
        "clflush [{addr}]",
        addr = in(reg) ptr,
        options(nostack, preserves_flags),
    );
    asm!("mfence", options(nostack, preserves_flags));
}

/// Flush all cache lines covering `[addr, addr + len)` from the local
/// CPU's cache, then issue `mfence`.
#[inline(always)]
pub unsafe fn clflush_range(addr: *const u8, len: usize) {
    let start = addr as usize;
    let end = start + len;
    let mut line = start & !63;
    while line < end {
        clflush(line as *const u8);
        line += 64;
    }
}

/// AP entry point.  Called by the SIPI trampoline after the AP has:
///   - switched to the kernel's PML4
///   - loaded the kernel GDT and IDT
///   - switched RSP to the per-CPU kernel stack
///   - signalled `status=1` in the mailbox
///
/// The trampoline does NOT pass an argument — LAPIC ID is read via CPUID.
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry() -> ! {
    // Adjust RSP by -8 so it's 8-mod-16 (SysV ABI convention).
    // The trampoline enters via `jmp` (no return address pushed),
    // so RSP would otherwise be 16-byte-aligned at entry, causing
    // SSE-aligned stores (MOVAPS) in called functions to #GP.
    unsafe { core::arch::asm!("sub rsp, 8", options(nostack, preserves_flags)); }

    // Read LAPIC ID via CPUID leaf 1 (no MMIO needed — avoids potential
    // page-table or LAPIC-model issues on the AP after SIPI).
    let apic_id: u32 = unsafe {
        let ebx: u32;
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
        );
        (ebx >> 24) & 0xFF
    };

    // 1. Install per-CPU TLS BEFORE any code that may access percpu
    //    or the allocator (which uses percpu on some backends).
    percpu::mark_online(apic_id);

    percpu::install_gs_base(apic_id as usize);

    // 2. Init FPU/SSE before any FPU/SSE-using code.
    unsafe {
        asm!("fninit", options(nostack, preserves_flags));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, preserves_flags));
        cr4 |= 1 << 9   // CR4.OSFXSR
             | 1 << 10; // CR4.OSXMMEXCPT
        if crate::has_xsave() {
            cr4 |= 1 << 18; // CR4.OSXSAVE
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, preserves_flags));
    }

    log::info!("AP[lapic={}] entered ap_entry, stack OK", apic_id);

    // 3. Initialise LAPIC timer on this CPU.
    crate::arch::apic::ap_enable_timer(apic_id);

    // 4. Wait for the BSP to release us.
    percpu::wait_for_kernel_ready(apic_id);

    // 5. Register idle task.
    crate::task::init_idle_task();

    // 5a. Load the per-CPU TSS into TR (SIPI reset TR to 0).
    unsafe {
        core::arch::asm!("ltr ax", in("ax") 0x28u16, options(nostack, preserves_flags));
    }

    // 6. Enable interrupts.
    unsafe { core::arch::asm!("sti") };

    // 7. Enter scheduling loop.
    ap_sched_loop(apic_id)
}

/// Per-CPU scheduling loop.  Pause-spins when idle, steals tasks from
/// overloaded peers when empty.
fn ap_sched_loop(apic_id: u32) -> ! {
    for _ in 0..100_000 {
        unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
    }
    let mut count = 0u64;
    loop {
        for _ in 0..1000 {
            unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
        }
        count += 1;
        if count % 100 == 0 {
            let cpu = apic_id as usize;
            if percpu::task_count(cpu) <= 1 {
                crate::task::steal_task(cpu);
            }
        }
    }
}

/// BSP-side: bring up all APs via LAPIC INIT-SIPI-SIPI.
///
/// For each AP:
///   1. Initialise the per-CPU GDT/TSS (TSS descriptor, IST1 stack).
///   2. WHPX: clflush the per-CPU GDT/TSS/IDT structures from BSP cache.
///   3. Allocate a kernel stack from the physical allocator.
///   4. Call `smp::start_aps` which sends INIT+SIPI and waits for ready.
///
/// After this returns, APs are running in `ap_entry` and waiting on
/// `kernel_ready`.  The BSP calls `percpu::release_all_aps()` next.
pub fn smp_boot_aps(boot_info: &BootInfo) {
    let count = boot_info.ap_count as usize;
    if count == 0 {
        log::info!("SMP: no APs to bring up");
        return;
    }

    // Pre-initialise every AP's per-CPU GDT/TSS.
    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let slot = (apic_id as usize) % MAX_CPUS;
        crate::arch::gdt::init_tss_descriptor_for_slot(slot);

        // WHPX: clflush the BSP's stores to the per-CPU GDT/TSS.
        unsafe {
            clflush_range(
                crate::arch::gdt::gdt_pointer_for_slot(slot) as *const u8,
                64,
            );
            clflush_range(
                crate::arch::gdt::gdt_table_address_for_slot(slot) as *const u8,
                64,
            );
            clflush_range(
                crate::arch::gdt::tss_address_for_slot(slot) as *const u8,
                128,
            );
            clflush_range(
                crate::arch::idt::idt_pointer_for_slot(slot) as *const u8,
                64,
            );
        }
    }

    // Allocate kernel stacks for each AP.
    let mut ap_stacks: [u64; MAX_CPUS] = [0; MAX_CPUS];
    for i in 0..count {
        let stack_phys: u64 = match crate::mm::phys::alloc_pages(AP_STACK_PAGES as u64) {
            Some(p) => p,
            None => {
                log::error!("SMP: failed to allocate stack for AP[{}]", i);
                return;
            }
        };
        let stack_top = stack_phys + (AP_STACK_PAGES * 4096) as u64;
        ap_stacks[i] = stack_top;

        // Reserve the stack pages so no other allocation stomps them.
        unsafe { crate::mm::phys::reserve_range(stack_phys, AP_STACK_PAGES); }

        log::info!(
            "SMP: AP[{}] stack phys={:#x} top={:#x}",
            i, stack_phys, stack_top
        );
    }

    // Start the APs via SIPI trampoline.
    crate::arch::smp::start_aps(boot_info, &ap_stacks[..count]);

    log::info!("SMP: all APs signalled ready");
}
