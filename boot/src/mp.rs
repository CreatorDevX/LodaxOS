//! UEFI Multi-Processor Services bring-up.
//!
//! Uses the `MpServices` protocol (PI spec, volume 2) to dispatch a
//! trampoline to each AP. The trampoline switches the AP to the kernel's
//! page tables, GDT, IDT, and stack; sets `ready = 1`; and spins on `go`.
//! The BSP writes `go = 1` (and the target entry point) after the kernel
//! has finished its own init, releasing the AP into the kernel.
//!
//! Layout of `ApArg` (must match the kernel's `ap_start` expectations):
//!
//! | Offset | Field                  | Notes                              |
//! |--------|------------------------|------------------------------------|
//! | 0x00   | `target_pml4_phys`     | Physical address of kernel PML4    |
//! | 0x08   | `target_gdt_ptr`       | Pointer to GDT descriptor (16 B)   |
//! | 0x10   | `target_idt_ptr`       | Pointer to IDT descriptor (16 B)   |
//! | 0x18   | `target_kernel_stack`  | Top of per-CPU kernel stack        |
//! | 0x20   | `target_entry`         | AP kernel entry (long mode)        |
//! | 0x28   | `ready`                | AtomicU32 — AP sets to 1           |
//! | 0x30   | `go`                   | AtomicU32 — BSP sets to 1          |
//! | 0x34   | `lapic_id`             | AP's LAPIC ID                      |
//!
//! The struct is allocated in UEFI Loader-Data memory so it survives
//! `ExitBootServices`. Its physical address is recorded in `BootInfo`.

use core::ffi::c_void;
use core::sync::atomic::Ordering;
use core::time::Duration;

use lodaxos_system::{ApArg, BootInfo, MAX_CPUS};
use uefi::boot::AllocateType;
use uefi::mem::memory_map::MemoryType;
use uefi::proto::pi::mp::MpServices;
use uefi::Status;

const AP_STACK_PAGES: usize = 4; // 4 × 4 KiB = 16 KiB per AP

/// Bring up APs via UEFI MP Services.
///
/// For each AP, allocates an `ApArg` and a 16 KiB kernel stack, then calls
/// `StartupThisAP` with the trampoline. Waits for each `ready == 1` (with
/// a 100 ms timeout) and records the LAPIC ID and `ApArg` physical address
/// in `boot_info` so the kernel can release them after init.
pub fn bring_up_aps(boot_info: &mut BootInfo) -> uefi::Result<()> {
    let mp_handle = uefi::boot::get_handle_for_protocol::<MpServices>()?;
    let mp = uefi::boot::open_protocol_exclusive::<MpServices>(mp_handle)?;

    let count = mp.get_number_of_processors()?;
    log::info!(
        "MP Services: total={} enabled={}",
        count.total, count.enabled
    );

    if count.enabled > MAX_CPUS {
        log::warn!(
            "MP Services: {} enabled CPUs exceeds MAX_CPUS={}, only first {} will be brought up",
            count.enabled, MAX_CPUS, MAX_CPUS
        );
    }
    let to_bring_up = count.enabled.min(MAX_CPUS);

    let mut ap_index = 0u32;
    for proc_num in 0..count.total {
        if to_bring_up == 0 {
            break;
        }
        let info = mp.get_processor_info(proc_num)?;
        if !info.is_enabled() || !info.is_healthy() {
            log::debug!("MP Services: proc {} disabled/unhealthy, skipping", proc_num);
            continue;
        }
        if info.is_bsp() {
            log::debug!("MP Services: proc {} is BSP, skipping (we are BSP)", proc_num);
            continue;
        }
        if ap_index as usize >= MAX_CPUS - 1 {
            log::warn!("MP Services: ran out of AP slots");
            break;
        }

        // Allocate the ApArg in UEFI Loader-Data memory (survives ExitBootServices).
        let ap_arg_ptr = uefi::boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            1, // 1 page = 4 KiB
        )?;
        let ap_arg_phys = ap_arg_ptr.as_ptr() as u64;
        // SAFETY: we just allocated this page and it's zero-initialised.
        let ap_arg: &mut ApArg = unsafe { &mut *(ap_arg_ptr.as_ptr() as *mut ApArg) };
        ap_arg.ready.store(0, Ordering::Release);
        ap_arg.go.store(0, Ordering::Release);
        ap_arg.lapic_id = info.processor_id as u32;

        // Allocate the AP's 16 KiB kernel stack (also in Loader-Data).
        let stack_ptr = uefi::boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            AP_STACK_PAGES,
        )?;
        let stack_phys = stack_ptr.as_ptr() as u64;
        let stack_top = stack_phys + (AP_STACK_PAGES * 4096) as u64;
        ap_arg.target_kernel_stack = stack_top;

        log::info!(
            "MP Services: starting AP proc={} apic_id={} aparg_phys={:#x} stack_top={:#x}",
            proc_num, info.processor_id, ap_arg_phys, stack_top
        );

        // Trampoline function (long mode, runs on the AP with UEFI stack).
        let arg: *mut c_void = ap_arg as *mut ApArg as *mut c_void;

        // Start the AP. The trampoline never returns (it spins on `go`
        // after setting `ready = 1`), so `StartupThisAP` will always time
        // out — that is expected and harmless.
        let result = mp.startup_this_ap(
            proc_num,
            ap_trampoline,
            arg,
            None,
            Some(Duration::from_millis(100)),
        );
        match result {
            Ok(()) => {
                log::warn!(
                    "MP Services: AP proc={} trampoline unexpectedly returned",
                    proc_num,
                );
            }
            Err(e) if e.status() == Status::TIMEOUT => {
                // Expected — the AP is alive, running the trampoline.
            }
            Err(e) => return Err(e),
        }

        // The AP should have set `ready = 1` within microseconds of
        // executing the trampoline.  If not, wait up to 5 more seconds.
        if ap_arg.ready.load(Ordering::Acquire) == 0 {
            log::info!(
                "MP Services: AP proc={} started but not yet ready, waiting up to 5 s",
                proc_num,
            );
            for _ in 0..500 {
                uefi::boot::stall(Duration::from_millis(10));
                if ap_arg.ready.load(Ordering::Acquire) != 0 {
                    break;
                }
            }
        }

        if ap_arg.ready.load(Ordering::Acquire) == 0 {
            log::error!(
                "MP Services: AP proc={} did not signal ready",
                proc_num,
            );
        }

        // Record for kernel.
        boot_info.ap_apic_ids[ap_index as usize] = info.processor_id as u32;
        boot_info.ap_arg_phys[ap_index as usize] = ap_arg_phys;
        ap_index += 1;
    }

    boot_info.ap_count = ap_index;
    boot_info.bsp_apic_id = 0; // BSP is always processor 0 in MP Services
    log::info!("MP Services: {} APs prepared for kernel release", ap_index);
    Ok(())
}

/// Trampoline function dispatched to each AP via UEFI MP Services.
///
/// Runs in long mode with the UEFI-provided stack. This is a **naked**
/// function — the Rust compiler emits zero prologue/epilogue, and the
/// first argument (`arg: *mut c_void`) is taken directly from RCX
/// (Microsoft x64 ABI) by the inline asm.  Any Rust-generated prologue
/// (stack-frame setup, register saves, TLS references) would fault on
/// the AP because UEFI's AP startup provides only a minimal stack and
/// no runtime TLS context.
///
/// ## Two-phase design
///
/// **Phase 1** (safe — UEFI environment intact):
///   - Signal `ready = 1` so the BSP knows we're alive.
///   - Spin on `go` (offset 0x2C) until the kernel writes the
///     `target_*` fields and sets `go = 1`.
///
/// **Phase 2** (full environment switch — after kernel has written fields):
///   - Switch CR3 to the kernel's page tables.
///   - Load the kernel's GDT.
///   - Reload CS to kernel code selector (0x08) via `retfq`.
///   - Reload data segment registers.
///   - Load the kernel's IDT.
///   - Switch RSP to the per-CPU kernel stack.
///   - Jump to the kernel's AP entry point (`[{arg} + 0x20]`).
///     Never returns.
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub extern "efiapi" fn ap_trampoline(_arg: *mut core::ffi::c_void) {
    // Naked asm — no prologue. The parameter is declared only to match
    // the function-pointer type expected by UEFI's StartupThisAP
    // (`extern "efiapi" fn(*mut c_void)`).  The inline asm reads the
    // first argument from RCX (Microsoft x64 ABI) directly; the Rust
    // name `_arg` is never referenced in code.
    core::arch::naked_asm!(
        "cli",
        "mov dword ptr [rcx + 0x28], 1",
        "mfence",
        "2:",
        "pause",
        "mov eax, dword ptr [rcx + 0x2C]",
        "test eax, eax",
        "jz 2b",
        "mov rax, [rcx + 0x00]",
        // Switch to the kernel stack *before* switching CR3: the UEFI
        // stack may live above 4 GB, which the kernel identity map does
        // not cover, and we need a mapped stack for `push`/`retfq` etc.
        "mov rsp, [rcx + 0x18]",
        "mov cr3, rax",
        "mov rbx, [rcx + 0x08]",
        "lgdt [rbx]",
        "push 0x08",
        "lea rax, [4f]",
        "push rax",
        "retfq",
        "4:",
        "mov ax, 0x10",
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        "mov ss, ax",
        "mov rbx, [rcx + 0x10]",
        "lidt [rbx]",
        "mov rdi, rcx",
        "mov rax, [rcx + 0x20]",
        "jmp rax",
    );
}
