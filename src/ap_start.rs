//! AP (Application Processor) entry point.
//!
//! Called by the boot trampoline (`boot::mp::ap_trampoline`) once the
//! kernel has set `ApArg.go = 1`. Runs in long mode with kernel page
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
use core::sync::atomic::Ordering;

use lodaxos_system::{ApArg, BootInfo, MAX_CPUS};

use crate::percpu;

/// AP entry point. The trampoline passes a single argument: a pointer
/// to its `ApArg` (in UEFI Loader-Data memory, kernel-mapped).
///
/// The trampoline has already:
///   - switched to the kernel's PML4
///   - loaded the kernel GDT and IDT
///   - switched RSP to the per-CPU kernel stack (from `ApArg.target_kernel_stack`)
///   - signalled `ApArg.ready = 1`
///   - waited for `ApArg.go = 1`
///
/// All that remains is per-CPU init and entry into the kernel.
#[unsafe(no_mangle)]
pub extern "C" fn ap_entry(arg: u64) -> ! {
    // VERY early debug: write directly to COM1 to prove we reached ap_entry.
    // Don't depend on ANY kernel state (no LAPIC, no logger, no globals).
    unsafe {
        core::arch::asm!(
            // Wait for transmit buffer empty (LSR = COM1+5 = 0x3FD)
            "2: in al, dx",
            "test al, 0x20",
            "jz 2b",
            in("dx") 0x3FDu16,
            out("al") _,
        );
        core::arch::asm!(
            // Write 'A' to COM1 data register
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") b'A',
            options(nostack, nomem),
        );
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

    log::info!("AP[lapic={}] entered ap_entry, stack OK", apic_id);

    // 1. Init FPU / SSE. The x86 FPU state is initialised by the BSP at
    //    boot, but each AP needs its own init to clear any leftover state
    //    from firmware.
    unsafe {
        asm!("fninit", options(nostack, preserves_flags));
        // Enable SSE (CR4.OSFXSR + CR4.OSXMMEXCPT).
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, preserves_flags));
        cr4 |= 1 << 9 | 1 << 10;
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, preserves_flags));
    }

    // 2. Mark online.
    percpu::mark_online(apic_id);

    // 3. Initialise LAPIC timer on this CPU. The BSP has already
    //    calibrated the timer ticks-per-ms; we use the same value.
    crate::arch::apic::ap_enable_timer(apic_id);

    // 4. Wait for the BSP to release us.
    percpu::wait_for_kernel_ready(apic_id);

    // 5. Enable interrupts. The trampoline left IF=0 (cli). Without sti,
    //    the LAPIC timer never fires and the AP can never receive IPIs.
    unsafe { core::arch::asm!("sti") };

    // 6. Enter the kernel idle / scheduler loop.
    ap_idle_loop(apic_id)
}

fn ap_idle_loop(apic_id: u32) -> ! {
    log::info!("AP[lapic={}] entering idle loop", apic_id);
    let mut last_log = 0u64;
    loop {
        // WHPX/QEMU does not handle HLT from AP vCPUs (reports
        // "Unexpected VP exit code 4"). Use a pause + spin loop
        // instead so the VM survives.
        for _ in 0..10_000 {
            unsafe { core::arch::asm!("pause", options(nomem, preserves_flags)) };
        }
        let now = percpu::ticks();
        if now - last_log >= 5000 {
            log::info!("[ap{}] alive, tick={}", apic_id, now);
            last_log = now;
        }
    }
}

/// BSP-side: release the APs brought up by the boot MP Services.
///
/// The boot code has already:
///   - started each AP via `StartupThisAP` with the trampoline
///   - waited for each AP to signal `ready = 1`
///   - recorded the LAPIC ID and ApArg physical address in BootInfo
///
/// We just need to:
///   - read each ApArg via the identity map (Loader-Data < 4 GB is covered)
///   - write the kernel PML4 / GDT / IDT / AP entry into the ApArg
///   - set `go = 1`
///
/// After this function returns, APs enter `ap_entry` and the BSP can
/// continue to the idle loop. The APs spin on `kernel_ready` until
/// `percpu::release_all_aps()` is called (typically at the very end of
/// kernel init).
pub fn release_aps(boot_info: &BootInfo) {
    let count = boot_info.ap_count as usize;
    if count == 0 {
        log::info!("SMP: no APs to release");
        return;
    }
    let pml4_phys = crate::mm::virt::pml4_address();
    let gdt_ptr = crate::arch::gdt::gdt_pointer_address();
    let idt_ptr = crate::arch::idt::idt_pointer_address();
    let ap_entry_addr = ap_entry as *const () as u64;
    log::info!(
        "SMP: releasing {} AP(s) — pml4={:#x} gdt={:#x} idt={:#x} ap_entry={:#x}",
        count, pml4_phys, gdt_ptr, idt_ptr, ap_entry_addr
    );

    for i in 0..count {
        let phys = boot_info.ap_arg_phys[i];
        // Identity map covers the first 4 GB via 2 MB huge pages, so the
        // physical address of the ApArg is also a valid virtual address.
        let ap = phys as *mut ApArg;
        // SAFETY: the boot allocated this page in Loader-Data memory and
        // zero-initialised it. The identity map covers the address.
        unsafe {
            (*ap).target_pml4_phys = pml4_phys;
            (*ap).target_gdt_ptr = gdt_ptr;
            (*ap).target_idt_ptr = idt_ptr;
            (*ap).target_entry = ap_entry_addr;
            // Memory fence so the writes are visible before `go` is set.
            asm!("mfence", options(nostack, preserves_flags));
            (*ap).go.store(1, Ordering::Release);
        }
        log::info!(
            "SMP: AP[{}] (lapic {}) released at ApArg phys={:#x}",
            i, boot_info.ap_apic_ids[i], phys
        );
    }
    // The APs will now enter `ap_entry`. They will block on `kernel_ready`
    // until `percpu::release_all_aps()` is called by the BSP later.
    log::info!("SMP: APs in flight, waiting for `kernel_ready`");
    // Avoid "unused import" warnings if MAX_CPUS isn't referenced below.
    let _ = MAX_CPUS;
}
