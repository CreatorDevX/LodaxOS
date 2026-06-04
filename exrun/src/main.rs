#![no_std]
#![no_main]
#![allow(unsafe_op_in_unsafe_fn)]

//! Executive Runtime — policy layer.
//!
//! Architectural position (v1):
//!   - ExRun is a **separate ring-0 process** with its own PML4.
//!   - The kernel loads ExRun's ELF via `kernel::exec::load`, which
//!     forks a PML4, maps the ELF segments into the forked PML4
//!     only, allocates a shared mailbox page, and creates a `Task`
//!     with `RDI = MAILBOX_EXRUN_VIRT`.
//!   - The kernel does **not** look up any symbols in this ELF.
//!     The only contract is the entry-point signature below.
//!   - IPC is **deferred** (see `memory.md`). For v1, ExRun's
//!     `_start` is a HLT stub — it never reads the mailbox.
//!     The mailbox page is allocated and mapped in both PML4s as
//!     a placeholder for the v2 IPC implementation.
//!
//! When IPC is implemented, `_start` will:
//!   1. Map the mailbox page at `MAILBOX_EXRUN_VIRT` (already done
//!      by the kernel loader).
//!   2. Loop: spin on `mailbox.request_ready`, process the
//!      `CapRequest`, write a `CapResponse`, set `response_ready`.

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("cli; hlt") } }
}

/// ExRun entry point.
///
/// `mailbox_virt` is the kernel-mapped virtual address of the
/// 4 KiB shared mailbox page (same physical address is also mapped
/// in the kernel's higher half at `MAILBOX_KERNEL_VIRT`). The page
/// is `lodaxos_system::Mailbox`-shaped.
///
/// v1: we don't touch the mailbox. ExRun is a parked task in the
/// runqueue. The kernel's CFS scheduler time-slices it alongside
/// task 0. ExRun just halts.
#[unsafe(no_mangle)]
pub extern "C" fn _start(mailbox_virt: u64) -> ! {
    // The mailbox is allocated by the kernel and mapped in our
    // PML4. We don't read it in v1 — the variable is here to
    // document the contract for v2 IPC.
    let _ = mailbox_virt;
    log::info!("ExRun: entered (mailbox at {:#x}) — v1 stub, halting", mailbox_virt);
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
