pub mod apic;
pub mod disasm;
pub mod dump;
pub mod gdt;
pub mod symtab;
pub mod idt;
pub mod ioapic;
pub mod smp;

use self::idt::TrapFrame;

/// 512-byte FPU save area, 16-byte aligned as required by `fxsave`/`fxrstor`.
#[derive(Debug, Clone)]
#[repr(align(16))]
pub struct FpuState(pub [u8; 512]);

/// Save x87 + SSE state using fxsave.
/// `buf` must be 512 bytes and 16-byte aligned (enforced by `FpuState`).
#[inline]
pub unsafe fn fxsave(buf: &mut FpuState) {
    core::arch::asm!("fxsave [{}]", in(reg) buf.0.as_mut_ptr(), options(nostack, preserves_flags));
}

/// Restore x87 + SSE state using fxrstor.
/// `buf` must be 512 bytes and 16-byte aligned (enforced by `FpuState`).
#[inline]
pub unsafe fn fxrstor(buf: &FpuState) {
    core::arch::asm!("fxrstor [{}]", in(reg) buf.0.as_ptr(), options(nostack, preserves_flags));
}

/// Switch execution to a new vCPU.
///
/// Restores all GPRs from `new_frame`, optionally switches CR3 to
/// `new_pml4` (pass 0 to keep current), and restores FPU state from
/// `new_fpu`. Never returns.
///
/// # Safety
/// - `new_frame` must contain valid, consistent register state for the
///   target vCPU.
/// - `new_pml4`, if non-zero, must be a valid page table that maps
///   everything the target vCPU needs (kernel code, its stack, and the
///   higher-half mapping).
/// - `new_fpu` must point to a valid, 16-byte-aligned 512-byte FPU save
///   area (enforced by `FpuState`'s repr alignment).
#[inline]
pub(crate) unsafe fn context_switch(
    new_frame: &TrapFrame,
    new_pml4: u64,
    new_fpu: &FpuState,
) -> ! {
    core::arch::asm!(
        // Optional CR3 switch (pass 0 to skip) — r9 used before GPR restore
        "cmp r9, 0",
        "je 2f",
        "mfence",
        "mov cr3, r9",
        "2:",
        // Restore FPU state — r10 used before GPR restore (r10 is the
        // LSB of the address, which must be 16-byte aligned for fxsave)
        "fxrstor [r10]",
        // r8 = TrapFrame pointer, pinned here as the only register NOT
        // in the GPR restore loop below — saved/restored via push/pop.
        "push r8",
        // Restore GPRs from the TrapFrame
        "mov r15, [r8 + 0x00]",
        "mov r14, [r8 + 0x08]",
        "mov r13, [r8 + 0x10]",
        "mov r12, [r8 + 0x18]",
        "mov r11, [r8 + 0x20]",
        "mov r10, [r8 + 0x28]",
        "mov r9,  [r8 + 0x30]",
        "mov rax, [r8 + 0x40]",
        "mov rbx, [r8 + 0x48]",
        "mov rcx, [r8 + 0x50]",
        "mov rdx, [r8 + 0x58]",
        "mov rbp, [r8 + 0x60]",
        "mov rsi, [r8 + 0x68]",
        "mov rdi, [r8 + 0x70]",
        // Restore r8 — now safe to read iretq frame values
        "pop r8",
        // Build iretq frame — check CS.RPL for ring-3 vs ring-0
        "test byte ptr [r8 + 0x90], 3",
        "jnz 3f",
        // Ring-0: load RSP, push RFLAGS, CS, RIP.
        // Defense-in-depth: if TrapFrame.rsp is zero (uninitialised vCPU),
        // skip the load and keep the current kernel RSP so we at least
        // get a readable crash dump instead of a double-fault cascade.
        "cmp qword ptr [r8 + 0xa0], 0",
        "je 5f",
        "mov rsp, [r8 + 0xa0]",
        "5:",
        "push qword ptr [r8 + 0x98]",
        "push qword ptr [r8 + 0x90]",
        "push qword ptr [r8 + 0x88]",
        "jmp 4f",
        // Ring-3: push SS, RSP, RFLAGS, CS, RIP (all 5)
        "3:",
        "push qword ptr [r8 + 0xa8]",
        "push qword ptr [r8 + 0xa0]",
        "push qword ptr [r8 + 0x98]",
        "push qword ptr [r8 + 0x90]",
        "push qword ptr [r8 + 0x88]",
        "4:",
        // Restore final register — r8 last (was used to read iretq frame)
        "mov r8, [r8 + 0x38]",
        "iretq",
        in("r8") new_frame as *const TrapFrame,
        in("r9") new_pml4,
        in("r10") new_fpu.0.as_ptr(),
        options(noreturn),
    )
}
