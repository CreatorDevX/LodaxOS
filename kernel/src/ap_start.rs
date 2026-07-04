use lodaxos_system::{BootInfo, MAX_CPUS};

use crate::percpu;
use crate::consts;

/// Flush a single cache line (64 bytes).  Required on WHPX and some other
/// virtualisation backends: plain stores from the BSP to the SIPI mailbox,
/// GDT, or TSS may be cached in the BSP's L1 and not visible to the AP
/// after SIPI, even after `mfence`.  `clflush` evicts the line from all
/// cache levels, and the trailing `mfence` serialises the flush.
#[inline(always)]
pub unsafe fn clflush(ptr: *const u8) {
    unsafe {
        core::arch::asm!(
            "clflush [{addr}]",
            addr = in(reg) ptr,
            options(nostack, preserves_flags),
        );
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
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

    // 1. Read the pre-allocated PERCPU slot from the physical SLOT_MAP
    //    table (written by the BSP before sending IPIs — serial, no
    //    race).  We use this directly instead of calling find_slot(),
    //    which would race with other APs arriving concurrently after
    //    the SIPI broadcast.
    let slot = unsafe {
        let raw = core::ptr::read_volatile(
            (crate::arch::smp::SLOT_MAP_PHYS + apic_id as u64) as *const u8,
        );
        raw as usize
    };

    // 2. Install per-CPU TLS at the pre-allocated slot.
    percpu::mark_online_for_slot(apic_id, slot);
    percpu::install_gs_base(slot);
    // Ensure the GS base / TSC_AUX write is globally visible before
    // enabling the LAPIC timer (which could fire an interrupt that calls
    // current_apic_id() → rdtscp → IA32_TSC_AUX).
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); }

    // 2. Init FPU/SSE before any FPU/SSE-using code.
    unsafe {
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        let mut cr4 = x86_64::registers::control::Cr4::read();
        cr4 |= x86_64::registers::control::Cr4Flags::OSFXSR
             | x86_64::registers::control::Cr4Flags::OSXMMEXCPT_ENABLE;
        if crate::has_xsave() {
            cr4 |= x86_64::registers::control::Cr4Flags::OSXSAVE;
        }
        x86_64::registers::control::Cr4::write(cr4);
    }

    log::info!("AP[lapic={}] entered ap_entry, stack OK", apic_id);

    // 3. Wait for the BSP to release us (timeout ~10 s).
    if !percpu::wait_for_kernel_ready(apic_id) {
        // BSP may have crashed — halt this AP.
        loop { x86_64::instructions::interrupts::disable(); x86_64::instructions::hlt(); }
    }

    // 4. Load the per-CPU TSS into TR (SIPI reset TR to 0).
    // Must happen before init_idle_vcpu (which may set TSS RSP0) and
    // before enabling interrupts (which could use IST — Bug 75).
    unsafe {
        core::arch::asm!("ltr ax", in("ax") 0x28u16, options(nostack, preserves_flags));
        crate::arch::gdt::init_syscall_msrs();
    }

    // 5. Register idle Vcpu (must be after ltr — Bug 75).
    crate::scheduler::init_idle_vcpu();

    // 6. Initialise LAPIC timer on this CPU (must be after init_idle_task
    //    so a timer interrupt doesn't find no idle task — Bug 74).
    // NOTE: ap_enable_timer uses physical LAPIC addresses directly (not
    // the higher-half LAPIC_BASE), so it works regardless of whether the
    // per-CPU higher-half mapping is installed on this AP.
    crate::arch::apic::ap_enable_timer(apic_id);

    // 7. Enable interrupts.
    x86_64::instructions::interrupts::enable();

    // 8. Enter scheduling loop.
    ap_sched_loop(apic_id)
}

/// Per-CPU scheduling loop.  Pause-spins when idle, steals tasks from
/// overloaded peers when empty.
fn ap_sched_loop(apic_id: u32) -> ! {
    for _ in 0..100_000 {
        core::hint::spin_loop();
    }
    let mut count = 0u64;
    loop {
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
        count += 1;
        if count % 100 == 0 {
            let cpu = percpu::apic_id_to_slot(apic_id);
            if percpu::task_count(cpu) <= 1 {
                crate::scheduler::steal_task(cpu);
            }
        }
    }
}

/// BSP-side: bring up all APs via LAPIC INIT-SIPI-SIPI.
///
/// For each AP:
///   0. Pre-allocate a PERCPU slot (serial on BSP — no race).
///   1. Initialise the per-CPU GDT/TSS (TSS descriptor, IST1 stack)
///      using the pre-allocated slot.
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

    // Clamp to available PERCPU slots (slot 0 is reserved for BSP).
    let count = count.min(MAX_CPUS - 1);

    // ---- Phase 0: pre-allocate PERCPU slots ----
    // This runs serially on the BSP, avoiding the race of multiple APs
    // calling find_slot() concurrently after SIPI broadcast.
    let mut ap_slots: [usize; MAX_CPUS] = [0; MAX_CPUS];
    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let slot = percpu::find_slot(apic_id).unwrap_or_else(|| {
            panic!(
                "SMP: no PERCPU slot for AP[{}] (apic_id={}, MAX_CPUS={})",
                i, apic_id, MAX_CPUS
            )
        });
        ap_slots[i] = slot;
        log::info!(
            "SMP: AP[{}] apic_id={} pre-allocated PERCPU slot {}",
            i, apic_id, slot
        );
    }

    // Pre-initialise every AP's per-CPU GDT/TSS using the same
    // pre-allocated slot that the AP will discover via SLOT_MAP_PHYS.
    for i in 0..count {
        let slot = ap_slots[i];
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
        let stack_phys: u64 = match crate::mm::phys::alloc_pages(consts::AP_STACK_PAGES as u64) {
            Some(p) => p,
            None => {
                log::error!("SMP: failed to allocate stack for AP[{}]", i);
                return;
            }
        };
        let stack_top = stack_phys + (consts::AP_STACK_PAGES * 4096) as u64;
        ap_stacks[i] = stack_top;

        // The trampoline loads RSP from the mailbox as a physical
        // address. The kernel page-table init already identity-maps the
        // first 4 GiB with 2 MiB pages, so low AP stacks need no extra
        // mapping. Remapping them here would split the existing huge page
        // during SMP bring-up, allocating page tables from the same low
        // memory area used for AP stacks and risking early AP page faults.
        let stack_size = (consts::AP_STACK_PAGES as u64) * 4096;
        if stack_phys.saturating_add(stack_size) > 0x1_0000_0000 {
            crate::mm::virt::map_region(
                crate::mm::virt::kernel_pml4(),
                stack_phys,
                stack_size,
                crate::mm::virt::WRITABLE | crate::mm::virt::PRESENT | crate::mm::virt::NO_EXECUTE,
            );
        }

        log::info!(
            "SMP: AP[{}] stack phys={:#x} top={:#x}",
            i, stack_phys, stack_top
        );
    }

    // Start the APs via SIPI trampoline (passes pre-allocated slots).
    crate::arch::smp::start_aps(boot_info, &ap_stacks[..count], &ap_slots[..count]);

    log::info!("SMP: all APs signalled ready");
}
