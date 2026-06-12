#![allow(dead_code)]

use core::arch::asm;
use lodaxos_system::MAX_CPUS;

/// TSS (Task State Segment) — 104 bytes on x86-64.
#[repr(C)]
struct Tss {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist1: u64,
    ist2: u64,
    ist3: u64,
    ist4: u64,
    ist5: u64,
    ist6: u64,
    ist7: u64,
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

impl Tss {
    const fn empty() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist1: 0,
            ist2: 0,
            ist3: 0,
            ist4: 0,
            ist5: 0,
            ist6: 0,
            ist7: 0,
            reserved2: 0,
            reserved3: 0,
            iomap_base: core::mem::size_of::<Tss>() as u16,
        }
    }
}

/// GDT pointer structure for `lgdt` — 10 bytes: 2-byte limit + 8-byte base.
#[repr(C, packed)]
struct GdtPtr {
    limit: u16,
    base: u64,
}

/// 64-bit segment descriptor access flags.
mod access {
    /// Kernel code: Present, ring 0, code, readable.
    pub const KERNEL_CODE: u8 = 0x9A;
    /// Kernel data: Present, ring 0, data, writable.
    pub const KERNEL_DATA: u8 = 0x92;
    /// User code (ring 3): Present, ring 3, code, readable.
    pub const USER_CODE: u8 = 0xFA;
    /// User data (ring 3): Present, ring 3, data, writable.
    pub const USER_DATA: u8 = 0xF2;
}

/// Granularity flags for 64-bit mode.
mod granularity {
    /// Kernel code/data: G=1 (4KB), L=1 (64-bit long mode). Nibble value.
    pub const LONG_MODE: u8 = 0xA;
}

/// Encode a 64-bit code/data segment descriptor (base is ignored in long mode
/// for flat-model code/data segments, but we encode it correctly anyway).
const fn make_descriptor(base: u32, limit: u32, access: u8, granularity: u8) -> u64 {
    let limit_low = (limit & 0xFFFF) as u64;
    let base_low = (base & 0xFFFF) as u64;
    let base_mid = ((base >> 16) & 0xFF) as u64;
    let access_byte = access as u64;
    let flags_limit_high = (((granularity as u64) << 4) | (((limit >> 16) & 0x0F) as u64)) as u64;
    let base_high = ((base >> 24) & 0xFF) as u64;

    limit_low
        | (base_low << 16)
        | (base_mid << 32)
        | (access_byte << 40)
        | (flags_limit_high << 48)
        | (base_high << 56)
}

/// Encode a 64-bit TSS descriptor (system descriptor, occupies two GDT entries).
const fn make_tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let base_lo16 = base & 0xFFFF;
    let base_mid8 = (base >> 16) & 0xFF;
    let base_hi8 = (base >> 24) & 0xFF;
    let base_hi32 = (base >> 32) as u32;
    let limit_lo16 = (limit & 0xFFFF) as u64;
    let access: u64 = 0x89;
    let flags: u64 = 0x0;

    let low = limit_lo16
        | (base_lo16 << 16)
        | (base_mid8 << 32)
        | (access << 40)
        | (flags << 48)
        | (base_hi8 << 56);

    let high = base_hi32 as u64;

    (low, high)
}

/// GDT with 7 entries: null, kernel code, kernel data, user code, user data, TSS low, TSS high.
#[repr(C)]
struct Gdt {
    null: u64,
    kernel_code: u64,
    kernel_data: u64,
    user_code: u64,
    user_data: u64,
    tss_low: u64,
    tss_high: u64,
}

/// Selector indices (byte offset = index * 8).
pub const KERNEL_CODE_SEL: u16 = 0x08;
const KERNEL_DATA_SEL: u16 = 0x10;
const TSS_SEL: u16 = 0x28;

/// Per-CPU IST1 stack (one per LAPIC ID slot). 16 KiB is enough to
/// capture a double-fault dump and let the kernel print diagnostics
/// before halting the CPU.
///
/// IMPORTANT: the kernel's GDT/TSS system previously used a *single*
/// global IST1 stack shared by all CPUs. This was safe in the SMP-CPU
/// bring-up log because only the BSP took a #DF, but on a real AP the
/// IST1 stack would belong to a different physical CPU — a #DF on the
/// AP would try to switch to a stack whose "top" pointer is unrelated
/// to the AP's current stack, producing a #GP on the very first
/// instruction of the #DF handler.  Per-CPU IST1 stacks fix this.
#[repr(C, align(16))]
pub struct AlignedIstStack(pub [u8; 16384]);

static mut IST1_STACKS: [AlignedIstStack; MAX_CPUS] = [const { AlignedIstStack([0; 16384]) }; MAX_CPUS];

/// Per-CPU dummy kernel stack used as the initial TSS.rsp0.  The
/// scheduler overwrites TSS.rsp0 on every context switch.
#[repr(C, align(16))]
struct AlignedStack([u8; 4096]);

static mut DUMMY_STACKS: [AlignedStack; MAX_CPUS] = [const { AlignedStack([0; 4096]) }; MAX_CPUS];

/// Per-CPU TSS instance. Indexed by LAPIC ID slot.
static mut TSS_TABLE: [Tss; MAX_CPUS] = [const { Tss::empty() }; MAX_CPUS];

/// Per-CPU GDT instance.
static mut GDT_TABLE: [Gdt; MAX_CPUS] = [const {
    Gdt {
        null: 0,
        kernel_code: make_descriptor(0, 0xFFFFF, access::KERNEL_CODE, granularity::LONG_MODE),
        kernel_data: make_descriptor(0, 0xFFFFF, access::KERNEL_DATA, granularity::LONG_MODE),
        user_code: make_descriptor(0, 0xFFFFF, access::USER_CODE, granularity::LONG_MODE),
        user_data: make_descriptor(0, 0xFFFFF, access::USER_DATA, granularity::LONG_MODE),
        tss_low: 0,
        tss_high: 0,
    }
}; MAX_CPUS];

/// Per-CPU GDT pointer (loaded by `lgdt`).
static mut GDT_PTR_TABLE: [GdtPtr; MAX_CPUS] = [const { GdtPtr { limit: 0, base: 0 } }; MAX_CPUS];

/// Return the higher-half virtual address of the GDT pointer for `slot`.
/// Used by the SMP bring-up code to copy the GDT pointer into the
/// SIPI trampoline mailbox.
///
/// Note: `smp.rs` now calls `gdt_ptr_limit_base(slot)` directly instead
/// of copying from this address. This function is kept for compatibility.
pub fn gdt_pointer_address() -> u64 {
    unsafe { &raw const GDT_PTR_TABLE[0] as u64 }
}

/// Return the higher-half virtual address of the GDT pointer for the
/// given per-CPU slot.  Each AP needs to load its own GDT (pointing to
/// its own TSS) — sharing the BSP's GDT would re-use the BSP's TSS
/// (and the BSP's IST1 stack) on every AP.
pub fn gdt_pointer_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const GDT_PTR_TABLE[slot] as u64 }
}

/// Return the higher-half virtual address of the GDT table for the
/// given per-CPU slot.  Used by the SMP bring-up code to flush
/// (clflush) the GDT entries so the AP sees valid data under WHPX.
pub fn gdt_table_address_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const GDT_TABLE[slot] as u64 }
}

/// Return the higher-half virtual address of the TSS for the given
/// per-CPU slot.  Used by `idt::init` to wire the IST1 stack for
/// the BSP's slot, and by `ap_start` to verify the per-CPU TSS was
/// initialised.
pub fn tss_address_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const TSS_TABLE[slot] as u64 }
}

/// IST1 stack top for `slot`.  The CPU reads `TSS.ist1` and sets RSP
/// to this value when a #DF fires on that CPU.
pub fn ist1_top_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const IST1_STACKS[slot].0 as u64 + 16384 }
}

/// Initial dummy RSP0 for `slot`.  The scheduler replaces this on
/// the first context switch.
pub fn dummy_rsp0_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const DUMMY_STACKS[slot].0 as u64 + 4096 }
}

/// Write a single byte to COM1 for debug tracing.
#[inline(always)]
unsafe fn com1_trace(ch: u8) -> bool {
    let mut retries = 100_000u32;
    loop {
        let lsr: u8;
        core::arch::asm!(
            "in al, dx",
            out("al") lsr,
            in("dx") 0x3FDu16,
        );
        if lsr & 0x20 != 0 { break; }
        retries = retries.saturating_sub(1);
        if retries == 0 { return false; }
    }
    core::arch::asm!(
        "out dx, al",
        in("dx") 0x3F8u16,
        in("al") ch,
    );
    true
}

#[inline(always)]
unsafe fn com1_trace_str(s: &[u8]) {
    for &b in s {
        com1_trace(b);
    }
}

/// Initialise the per-CPU GDT and TSS for the BSP (slot 0 typically,
/// but the caller passes its slot in).  Loads the GDTR and the TR on
/// the calling CPU.
///
/// Must be called *before* the LAPIC timer is enabled, because the
/// IST1 entry for the BSP's TSS must point to a valid per-CPU stack
/// before any #DF could fire.
///
/// # Safety
/// The caller must guarantee `slot < MAX_CPUS`.  The caller must be
/// running on the CPU identified by `slot`.
pub unsafe fn init_for_slot(slot: usize) {
    let slot = slot % MAX_CPUS;

    com1_trace_str(b"GDT START\r\n");

    // Initialise TSS.
    TSS_TABLE[slot].rsp0 = dummy_rsp0_for_slot(slot);
    TSS_TABLE[slot].ist1 = ist1_top_for_slot(slot);
    com1_trace_str(b"GDT STACK\r\n");

    let tss_addr = &raw const TSS_TABLE[slot] as u64;

    // Verify TSS address is canonical (bits 48–63 all same as bit 47).
    // Non-canonical addresses cause #GP on ltr.
    let canonical = (tss_addr >> 47) & 1 == 0
        || (tss_addr >> 47) & 0x1_FFFF == 0x1_FFFF;
    assert!(canonical, "TSS address {:#x} is non-canonical", tss_addr);

    let tss_limit = (core::mem::size_of::<Tss>() - 1) as u32;
    let (tss_lo, tss_hi) = make_tss_descriptor(tss_addr, tss_limit);
    GDT_TABLE[slot].tss_low = tss_lo;
    GDT_TABLE[slot].tss_high = tss_hi;
    com1_trace_str(b"GDT TSS\r\n");

    GDT_PTR_TABLE[slot].limit = (core::mem::size_of::<Gdt>() - 1) as u16;
    GDT_PTR_TABLE[slot].base = &raw const GDT_TABLE[slot] as u64;
    com1_trace_str(b"GDT PTR\r\n");

    asm!(
        "cli",
        "lgdt [{gdt_ptr}]",
        // -- far return: reload CS --
        "mov rax, 0x08",
        "push rax",
        "lea rax, [3f]",
        "push rax",
        "retfq",
        "3:",
        // -- reload data segments --
        "mov ax, 0x10",
        "mov ds, ax",
        "mov es, ax",
        "mov fs, ax",
        "mov gs, ax",
        "mov ss, ax",
        // -- load TSS --
        "mov ax, 0x28",
        "ltr ax",
        gdt_ptr = in(reg) &raw const GDT_PTR_TABLE[slot],
    );
}

/// Backwards-compat wrapper for the BSP. Calls `init_for_slot(0)`.
/// TODO: remove once all call sites use `init_for_slot` directly.
pub fn load() {
    unsafe { init_for_slot(0); }
}

/// Initialise the GDT's TSS descriptor for `slot` to point at the
/// per-CPU TSS for that slot, and set the per-CPU IST1 stack.  This
/// is called by the BSP *before* `smp_boot_aps` so that the AP
/// trampoline's `lgdt` finds a fully-formed TSS descriptor and the AP's
/// subsequent `ltr` works on first try.
///
/// Does NOT `lgdt` or `ltr` on the calling CPU — that's the
/// responsibility of the AP itself (or `init_for_slot` for the BSP).
pub fn init_tss_descriptor_for_slot(slot: usize) {
    let slot = slot % MAX_CPUS;
    unsafe {
        TSS_TABLE[slot].rsp0 = dummy_rsp0_for_slot(slot);
        TSS_TABLE[slot].ist1 = ist1_top_for_slot(slot);

        let tss_addr = &raw const TSS_TABLE[slot] as u64;
        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u32;
        let (tss_lo, tss_hi) = make_tss_descriptor(tss_addr, tss_limit);
        GDT_TABLE[slot].tss_low = tss_lo;
        GDT_TABLE[slot].tss_high = tss_hi;

        GDT_PTR_TABLE[slot].limit = (core::mem::size_of::<Gdt>() - 1) as u16;
        GDT_PTR_TABLE[slot].base = &raw const GDT_TABLE[slot] as u64;
    }
}

/// Set the IST1 stack address for `slot`.  Normally initialised in
/// `init_for_slot`; this is exposed for `idt::init` which needs to
/// write the IST1 stack top before the BSP's first `lgdt` (to match
/// the IST1 gate descriptor).
pub fn set_ist1_for_slot(slot: usize, addr: u64) {
    let slot = slot % MAX_CPUS;
    unsafe {
        TSS_TABLE[slot].ist1 = addr;
    }
}

/// Backwards-compat: set the BSP's (slot 0) IST1 stack address.
pub fn set_ist1(addr: u64) {
    set_ist1_for_slot(0, addr);
}

/// Update TSS.rsp0 for `slot` to the top of the current task's kernel
/// stack.  Called by the scheduler on every context switch.
pub unsafe fn tss_set_rsp0_for_slot(slot: usize, rsp0: u64) {
    let slot = slot % MAX_CPUS;
    TSS_TABLE[slot].rsp0 = rsp0;
}

/// Backwards-compat: update the BSP's RSP0.
/// TODO: remove once all call sites use `tss_set_rsp0_for_slot` directly.
pub unsafe fn tss_set_rsp0(rsp0: u64) {
    tss_set_rsp0_for_slot(0, rsp0);
}

/// Return the limit and base of the GDT pointer for `slot`.
/// Used by the SMP init code to copy the GDT pointer into the
/// SIPI mailbox.
pub fn gdt_ptr_limit_base(slot: usize) -> (u16, u64) {
    let slot = slot % MAX_CPUS;
    unsafe { (GDT_PTR_TABLE[slot].limit, GDT_PTR_TABLE[slot].base) }
}
