#![allow(dead_code)]

use core::arch::asm;

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
///
/// TSS descriptor low 8 bytes (same format as code/data for the access/limit fields):
///   Bits  15:0   = Limit[15:0]
///   Bits  31:16  = Base[15:0]
///   Bits  39:32  = Base[23:16]
///   Bits  47:40  = Access (P=1, DPL=0, Type=0x9 = 64-bit available TSS)
///   Bits  51:48  = Limit[19:16]
///   Bits  55:52  = Flags (G=0, D/B=0, L=0, AVL=0)
///   Bits  63:56  = Base[31:24]
///
/// TSS descriptor high 8 bytes:
///   Bits  31:0   = Base[63:32]
///   Bits  63:32  = Reserved (must be zero)
const fn make_tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let base_lo16 = base & 0xFFFF;
    let base_mid8 = (base >> 16) & 0xFF;
    let base_hi8 = (base >> 24) & 0xFF;
    let base_hi32 = (base >> 32) & 0xFFFF_FFFF;
    let limit_lo16 = (limit & 0xFFFF) as u64;
    // Access: Present=1, DPL=00, S=0, Type=1001b (64-bit available TSS)
    let access: u64 = 0x89;
    // Flags: G=0, D/B=0, L=0, AVL=0, Limit[19:16]=0
    let flags: u64 = 0x0;

    let low = limit_lo16
        | (base_lo16 << 16)
        | (base_mid8 << 32)
        | (access << 40)
        | (flags << 48)
        | (base_hi8 << 56);

    let high = base_hi32;

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

/// Dummy stack for initial RSP0 (kernel stack on ring-3 -> ring-0 transition).
#[repr(C, align(16))]
struct AlignedStack([u8; 4096]);

static mut DUMMY_STACK: AlignedStack = AlignedStack([0; 4096]);

/// Static TSS instance.
static mut TSS: Tss = Tss {
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
};

/// Static GDT instance.
static mut GDT: Gdt = Gdt {
    null: 0,
    kernel_code: make_descriptor(0, 0xFFFFF, access::KERNEL_CODE, granularity::LONG_MODE),
    kernel_data: make_descriptor(0, 0xFFFFF, access::KERNEL_DATA, granularity::LONG_MODE),
    user_code: make_descriptor(0, 0xFFFFF, access::USER_CODE, granularity::LONG_MODE),
    user_data: make_descriptor(0, 0xFFFFF, access::USER_DATA, granularity::LONG_MODE),
    tss_low: 0,
    tss_high: 0,
};

/// Static GDT pointer for lgdt.
static mut GDT_PTR: GdtPtr = GdtPtr {
    limit: 0,
    base: 0,
};

/// Write a single byte to COM1 for debug tracing.
/// Returns false if the transmit buffer never becomes ready (timeout).
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

/// Load the GDT and TSS. Must be called after page tables are initialized
/// (higher-half mappings must be active so our statics are accessible).
pub fn load() {
    unsafe {
        com1_trace(b'A');

        let stack_top = &raw const DUMMY_STACK.0 as u64 + 4096;
        TSS.rsp0 = stack_top;
        com1_trace(b'B');

        let tss_addr = &raw const TSS as u64;

        // Verify TSS address is canonical (bits 48–63 all same as bit 47).
        // Non-canonical addresses cause #GP on ltr.
        let canonical = (tss_addr >> 47) & 1 == 0
            || (tss_addr >> 47) & 0x1_FFFF == 0x1_FFFF;
        assert!(canonical, "TSS address {:#x} is non-canonical", tss_addr);

        let tss_limit = (core::mem::size_of::<Tss>() - 1) as u32;
        let (tss_lo, tss_hi) = make_tss_descriptor(tss_addr, tss_limit);
        GDT.tss_low = tss_lo;
        GDT.tss_high = tss_hi;
        com1_trace(b'C');

        GDT_PTR.limit = (core::mem::size_of::<Gdt>() - 1) as u16;
        GDT_PTR.base = &raw const GDT as u64;
        com1_trace(b'D');

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
            gdt_ptr = in(reg) &raw const GDT_PTR,
        );
    }
}

pub fn set_ist1(addr: u64) {
    unsafe {
        TSS.ist1 = addr;
    }
}
