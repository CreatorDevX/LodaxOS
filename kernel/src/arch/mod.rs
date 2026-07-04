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
        // Optional CR3 switch (pass 0 to skip)
        "cmp {pml4}, 0",
        "je 2f",
        "mfence",
        "mov cr3, {pml4}",
        "2:",
        // Restore FPU state
        "fxrstor [{fpu}]",
        // Save {base} to stack before GPR restoration.
        // The compiler may allocate {base} to any GPR (r15-rdi). The
        // following mov instructions write to those registers, which
        // would clobber {base} and corrupt all subsequent [{base}] reads
        // (including the iretq frame setup below). Push/pop preserves
        // the original TrapFrame pointer across the GPR restore.
        "push {base}",
        // Restore GPRs from the TrapFrame
        "mov r15, [{base} + 0x00]",
        "mov r14, [{base} + 0x08]",
        "mov r13, [{base} + 0x10]",
        "mov r12, [{base} + 0x18]",
        "mov r11, [{base} + 0x20]",
        "mov r10, [{base} + 0x28]",
        "mov r9,  [{base} + 0x30]",
        "mov rax, [{base} + 0x40]",
        "mov rbx, [{base} + 0x48]",
        "mov rcx, [{base} + 0x50]",
        "mov rdx, [{base} + 0x58]",
        "mov rbp, [{base} + 0x60]",
        "mov rsi, [{base} + 0x68]",
        "mov rdi, [{base} + 0x70]",
        // Restore {base} — now safe to read iretq frame values
        "pop {base}",
        // Build iretq frame — check CS.RPL for ring-3 vs ring-0
        "test byte ptr [{base} + 0x90], 3",
        "jnz 3f",
        // Ring-0: load RSP, push RFLAGS, CS, RIP
        "mov rsp, [{base} + 0xa0]",
        "push qword ptr [{base} + 0x98]",
        "push qword ptr [{base} + 0x90]",
        "push qword ptr [{base} + 0x88]",
        "jmp 4f",
        // Ring-3: push SS, RSP, RFLAGS, CS, RIP (all 5)
        "3:",
        "push qword ptr [{base} + 0xa8]",
        "push qword ptr [{base} + 0xa0]",
        "push qword ptr [{base} + 0x98]",
        "push qword ptr [{base} + 0x90]",
        "push qword ptr [{base} + 0x88]",
        "4:",
        // Restore final register — r8 last (base was used to read it)
        "mov r8, [{base} + 0x38]",
        "iretq",
        base = in(reg) new_frame as *const TrapFrame,
        pml4 = in(reg) new_pml4,
        fpu = in(reg) new_fpu.0.as_ptr(),
        options(noreturn),
    )
}
