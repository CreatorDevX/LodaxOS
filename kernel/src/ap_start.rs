//! AP (Application Processor) entry point.
//!
//! Called by the boot trampoline (`boot::mp::ap_trampoline`) once the
//! BSP writes `go=1` in the `ApArg` (detected via `in al, 0x80` / 
//! `clflush` / `mov` spin loop). Runs in long mode with kernel page
//! tables, kernel GDT, kernel IDT, and a per-CPU kernel stack.
//!
//! The entry sequence is:
//!   1. Read our LAPIC ID (from `ApArg` passed via a CPU-local register
//!      or from the LAPIC ID register).
//!   2. Initialise the FPU / SSE state.
//!   3. Mark this CPU online in `percpu::PERCPU`.
//!   4. Wait until the BSP sets `percpu::PERCPU[us].kernel_ready`.
//!   5. Enter the kernel's idle / scheduler loop.
//!
//! We never return from this function.

use core::arch::asm;


use lodaxos_system::{ApArg, BootInfo, MAX_CPUS};

use crate::percpu;

/// Flush a single cache line.  Required on WHPX (Windows Hypervisor
/// Platform) and some other virtualisation backends: writes from the
/// BSP to an `ApArg` may be cached in the BSP's L1 and not visible to
/// the AP's memory accesses, even after `mfence`.  The same trick is
/// used in the UEFI-side `boot::mp` trampoline (see `boot/src/mp.rs`
/// `clflush` calls).
#[inline(always)]
unsafe fn clflush(ptr: *const u8) {
    asm!(
        "clflush [{addr}]",
        addr = in(reg) ptr,
        options(nostack, preserves_flags),
    );
    // `clflush` is weakly ordered — a subsequent `mfence` ensures
    // the flush completes before any later instruction executes.
    asm!("mfence", options(nostack, preserves_flags));
}

/// Flush all cache lines covering `[addr, addr + len)` from the local
/// CPU's L1 cache, then issue `mfence`.  Required for WHPX cross-VP
/// coherence: the BSP's plain stores to GDT/TSS/IDT entries live in
/// the BSP's L1 cache and are invisible to APs until evicted.
#[inline(always)]
unsafe fn clflush_range(addr: *const u8, len: usize) {
    let start = addr as usize;
    let end = start + len;
    let mut line = start & !63;
    while line < end {
        clflush(line as *const u8);
        line += 64;
    }
}

/// AP entry point. The trampoline passes a single argument: a pointer
/// to its `ApArg` (in UEFI Loader-Data memory, kernel-mapped).
///
/// The trampoline has already:
///   - switched to the kernel's PML4
///   - loaded the kernel GDT and IDT
///   - switched RSP to the per-CPU kernel stack (from `ApArg.target_kernel_stack`)
///   - signalled `ApArg.ready = 1`
///   - waited for `ApArg.go = 1` (via `mov` / `clflush` / `in al, 0x80` spin loop)
///
/// All that remains is per-CPU init and entry into the kernel.
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry(arg: u64) -> ! {
    // VERY early debug: write directly to COM1 to prove we reached ap_entry.
    // Don't depend on ANY kernel state (no LAPIC, no logger, no globals).
    unsafe {
        for &byte in b"AP ENTRY REACHED\r\n" {
            core::arch::asm!(
                "2: in al, dx",
                "test al, 0x20",
                "jz 2b",
                in("dx") 0x3FDu16,
                out("al") _,
            );
            core::arch::asm!(
                "out dx, al",
                in("dx") 0x3F8u16,
                in("al") byte,
                options(nostack, nomem),
            );
        }
    }

    // Read our LAPIC ID from the ApArg struct passed in RDI (SysV ABI).
    // The trampoline jumps here with RCX = ApArg pointer (MS ABI) but
    // we need it in RDI (SysV ABI). Since the trampoline doesn't set RDI,
    // we read LAPIC ID from the LAPIC MMIO register instead.
    let apic_id: u32 = unsafe {
        let raw: u32;
        let lapic_id_addr = crate::arch::apic::LAPIC_BASE + crate::arch::apic::APIC_ID as u64;
        core::arch::asm!(
            "mov eax, dword ptr [{addr}]",
            addr = in(reg) lapic_id_addr as *const u32,
            out("eax") raw,
        );
        raw >> 24
    };
    let _ = arg; // suppress unused

    // 1. Init FPU / SSE BEFORE the log crate may use SSE instructions.
    //    On TCG, CR4 may be zero (SIPI reset state); OSXSAVE is only
    //    set if CPUID indicates XSAVE support (TCG may lack it).
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

    // 2. Mark online.
    percpu::mark_online(apic_id);
    // 2a. Install per-CPU TLS: GS base -> &PERCPU[our_slot], TSC_AUX
    //     -> our LAPIC ID.  This must happen *before* any code reads
    //     `current_apic_id()` via the fast `rdtscp` path, and before
    //     the LAPIC timer ISR fires (which uses `percpu::is_bsp()`).
    percpu::install_gs_base(apic_id as usize);

    // 3. Initialise LAPIC timer on this CPU. The BSP has already
    //    calibrated the timer ticks-per-ms; we use the same value.
    crate::arch::apic::ap_enable_timer(apic_id);

    // 4. Wait for the BSP to release us.
    percpu::wait_for_kernel_ready(apic_id);

    // 5. Register this CPU's idle task with the scheduler.
    crate::task::init_idle_task();

    // 5a. Load the per-CPU TSS into the TR.  The AP came up via
    //     INIT-SIPI-SIPI, which reset the CPU to its power-on state
    //     (TR = 0).  We MUST `ltr` the AP's own TSS selector (0x28)
    //     before enabling interrupts, otherwise the first #DF on the
    //     AP would use the BSP's IST1 stack (and the AP's first ring
    //     transition would use the BSP's rsp0).  Even on the first
    //     IRQ, the CPU consults the TR's TSS for the IST entry on
    //     vector-8 gates (which our #DF uses).  Without this `ltr`,
    //     the AP would #GP inside its own #DF handler and triple-fault.
    unsafe {
        core::arch::asm!("ltr ax", in("ax") 0x28u16, options(nostack, preserves_flags));
    }

    // 6. Enable interrupts. The trampoline left IF=0 (cli). Without sti,
    //    the LAPIC timer never fires and the AP can never receive IPIs.
    unsafe { core::arch::asm!("sti") };

    // 7. Enter the scheduling loop.  The AP never returns from here.
    ap_sched_loop(apic_id)
}

/// Per-CPU scheduling loop.  Pause-spins when idle, steals tasks from
/// overloaded peers when empty.  Uses `pause` instead of `hlt` because
/// WHPX does not handle `hlt` from AP vCPUs (reports "Unexpected VP
/// exit code 4").
fn ap_sched_loop(apic_id: u32) -> ! {
    // Wait a moment so the BSP can finish boot before we start stealing
    // tasks.
    for _ in 0..100_000 {
        unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
    }
    let mut count = 0u64;
    loop {
        // Pause a short while to let the timer ISR fire if any tasks are
        // queued for this CPU.  The timer ISR will context-switch us to
        // a real task (the ISR returns via `sti + ret` and never comes
        // back here until the task's time slice expires).
        for _ in 0..1000 {
            unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
        }
        count += 1;
        // Every ~1000 pause batches, try to steal work.
        if count % 100 == 0 {
            let cpu = apic_id as usize;
            if percpu::task_count(cpu) <= 1 {
                crate::task::steal_task(cpu);
            }
        }
    }
}

/// BSP-side: release APs that are already running in the bootloader
/// trampoline (started by UEFI MP Services before ExitBootServices).
///
/// For each AP, the trampoline has already signalled `ready = 1` and
/// is spinning on `go` via a `pause; mov; test; jnz; clflush; mfence; in;
/// jmp` loop.  This function writes the kernel's target fields into `ApArg`,
/// flushes the cache lines, publishes `go=1` via `lock xchg`, and forces
/// a VMEXIT (`in al, 0x80`) so WHPX commits the store to host physical
/// memory.  The AP detects `go=1` on its next `lock xchg` iteration
/// (which is a full memory barrier on every x86 implementation) and
/// proceeds to switch page tables / GDT / IDT / stack and jump to
/// `ap_entry`.
///
/// The BSP-side `in al, 0x80` forces a VMEXIT so WHPX commits the
/// `lock xchg` store to host physical memory — without it, the write
/// may sit in the BSP's write buffer and never become visible to AP
/// VPs even via the AP's own locked access.
pub fn release_aps(boot_info: &BootInfo) {
    let count = boot_info.ap_count as usize;
    if count == 0 {
        log::info!("SMP: no APs to release");
        return;
    }

    let pml4_phys = crate::mm::virt::pml4_address();
    let gdt_desc_addr = crate::arch::gdt::gdt_pointer_address();
    let idt_desc_addr = crate::arch::idt::idt_pointer_address();
    let ap_entry_addr = ap_entry as *const () as u64;

    log::info!(
        "SMP: releasing {} AP(s) via boot MP handoff - pml4={:#x} gdt_desc={:#x} idt_desc={:#x} entry={:#x}",
        count, pml4_phys, gdt_desc_addr, idt_desc_addr, ap_entry_addr
    );

    // Pre-initialise every AP's per-CPU GDT and TSS in BSP context.
    // The trampoline only does `lgdt [target_gdt_ptr]` and
    // `lidt [target_idt_ptr]` — it does NOT encode the TSS descriptor
    // because the encoding requires a 64-bit base address which the
    // trampoline has no way to compute (it's a static byte array,
    // not generated code).  So the BSP must fill in the GDT entry
    // *before* the AP runs `lgdt`, or the AP will load a GDT with a
    // zero TSS descriptor and #GP on the first `ltr` / interrupt.
    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let slot = (apic_id as usize) % MAX_CPUS;
        // Initialise the AP's TSS and the GDT's TSS descriptor.
        // We don't lgdt/ltr for the AP — that happens on the AP
        // itself.  We just make sure the static GDT and TSS are in a
        // valid state when the AP loads them.
        crate::arch::gdt::init_tss_descriptor_for_slot(slot);

        // WHPX cross-VP cache coherence: the BSP's plain stores above
        // live only in the BSP's L1 cache on WHPX.  The AP will read
        // these same physical addresses after its CR3 switch, and WHPX
        // does not maintain per-VP cache snooping — the AP sees stale
        // zeros unless we clflush now.
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

    for i in 0..count {
        let apic_id = boot_info.ap_apic_ids[i];
        let arg_phys = boot_info.ap_arg_phys[i];
        // Per-CPU GDT pointer: each AP loads its own GDT (with its
        // own TSS descriptor) so the AP's #DF uses the AP's own
        // IST1 stack — *not* the BSP's.  Sharing the BSP's GDT would
        // re-use the BSP's TSS and the AP would #GP on the first
        // instruction of its #DF handler.
        let slot = (apic_id as usize) % MAX_CPUS;
        let per_cpu_gdt = crate::arch::gdt::gdt_pointer_for_slot(slot);
        let per_cpu_idt = crate::arch::idt::idt_pointer_for_slot(slot);
        let ap = arg_phys as *mut ApArg;
        unsafe {
            (*ap).target_pml4_phys = pml4_phys;
            (*ap).target_gdt_ptr = per_cpu_gdt;
            (*ap).target_idt_ptr = per_cpu_idt;
            (*ap).target_entry = ap_entry_addr;
            // WHPX clflush trick: cached writes to ApArg must be flushed
            // before SIPI, or the AP's spin loop never sees `go=1`.
            for off in (0..core::mem::size_of::<ApArg>()).step_by(64) {
                clflush((ap as *const u8).add(off));
            }
            // Write `go = 1` with a plain store.  On QEMU TCG memory is
            // always coherent between vCPU threads.  On WHXP the AP's
            // trampoline uses `pause` + `mov` + tight loop (no clflush,
            // no mfence) — the BSP-side `clflush` + `in al,0x80` below
            // ensures the store leaves the BSP's write buffer and is
            // visible to other VPs.
            let ap_ptr = ap as *const u8;
            core::arch::asm!(
                "mov dword ptr [{}], 1",
                in(reg) ap_ptr.add(0x2C),
                options(nostack, preserves_flags),
            );
            clflush(&(*ap).go as *const _ as *const u8);
            asm!("in al, 0x80", options(nomem, nostack, preserves_flags));
        }
        log::info!(
            "SMP: AP[{}] lapic={} arg={:#x} gdt={:#x} idt={:#x} go=1",
            i, apic_id, arg_phys, per_cpu_gdt, per_cpu_idt
        );
    }

    log::info!("SMP: AP handoff go=1 written — APs spin with mov+test+jnz");

}
