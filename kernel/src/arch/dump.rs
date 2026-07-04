//! Fault diagnostic dump — orchestrates the full crash output.
//!
//! Extracted from `idt.rs` to keep the interrupt machinery lean.
//! Uses the tiny disassembler in `super::disasm`.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use crate::arch::idt::TrapFrame;
use crate::serial::DumpWriter;
use crate::sync::SyncUnsafeCell;
use crate::mm::virt::{HIGHER_HALF, PRESENT, WRITABLE, USER, NO_EXECUTE};

// ---------------------------------------------------------------------------
// Exception history ring buffer  (per-CPU, traced in dump_full_fault)
// ---------------------------------------------------------------------------
const EX_HISTORY_DEPTH: usize = 4;

#[derive(Clone, Copy)]
struct ExRecord {
    vector: u64,
    rip: u64,
    uptime: u64,
}

struct ExHistory {
    entries: [ExRecord; EX_HISTORY_DEPTH],
    head: usize,
}

static EX_HISTORY: SyncUnsafeCell<ExHistory> = SyncUnsafeCell::new(ExHistory {
    entries: [ExRecord { vector: 0, rip: 0, uptime: 0 }; EX_HISTORY_DEPTH],
    head: 0,
});

fn push_exception(vector: u64, rip: u64, uptime: u64) {
    let h = unsafe { &mut *EX_HISTORY.get() };
    h.entries[h.head] = ExRecord { vector, rip, uptime };
    h.head = (h.head + 1) % EX_HISTORY_DEPTH;
}

fn dump_exception_history(mut w: &mut impl Write, current_vector: u64, current_rip: u64) {
    let h = unsafe { &*EX_HISTORY.get() };
    let _ = writeln!(w, "--- Exception History ---");

    // Show current exception first
    let uptime_now = crate::arch::idt::ticks();
    let secs = uptime_now / 1000;
    let millis = uptime_now % 1000;
    let vec_name = exception_name(current_vector);
    let _ = write!(w, "  #0  #{:2} {}  RIP={:#018x}  uptime={}.{:03}s (current)",
        current_vector, vec_name, current_rip, secs, millis);
    write_symbol_info(&mut w, current_rip);
    let _ = writeln!(w);

    // Walk ring buffer (oldest first, skipping zeros)
    let mut idx = 0;
    for i in 0..EX_HISTORY_DEPTH {
        let entry_idx = (h.head + i) % EX_HISTORY_DEPTH;
        let e = &h.entries[entry_idx];
        if e.vector == 0 && e.rip == 0 {
            continue;
        }
        idx += 1;
        let up_secs = e.uptime / 1000;
        let up_millis = e.uptime % 1000;
        let name = exception_name(e.vector);
        let _ = write!(w, "  #{}  #{:2} {}  RIP={:#018x}  uptime={}.{:03}s",
            idx, e.vector, name, e.rip, up_secs, up_millis);
        write_symbol_info(&mut w, e.rip);
        let _ = writeln!(w);
    }
}

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE",  1 => "#DB",  2 => "#NMI", 3 => "#BP",
        4 => "#OF",  5 => "#BR",  6 => "#UD",  7 => "#NM",
        8 => "#DF",  9 => "#COP",10 => "#TS", 11 => "#NP",
        12 => "#SS", 13 => "#GP", 14 => "#PF", 16 => "#MF",
        17 => "#AC", 18 => "#MC", 19 => "#XM", 20 => "#VE",
        _ => "??",
    }
}

// ---------------------------------------------------------------------------
// Null writer — used to pre-compute instruction lengths without output
// ---------------------------------------------------------------------------
struct NullWrite;
impl Write for NullWrite {
    fn write_str(&mut self, _: &str) -> core::fmt::Result { Ok(()) }
}

// ---------------------------------------------------------------------------
// Safe memory probe  (moved from idt.rs)
// ---------------------------------------------------------------------------
fn probe_read_quad(pml4_phys: u64, addr: u64) -> Option<u64> {
    let ext = (addr as i64) >> 47;
    if ext != 0 && ext != -1 {
        return None;
    }

    // Maximum physical address we'll trust.  The kernel identity-maps
    // 0..4 GiB; anything beyond that is not safely dereferenceable
    // through HIGHER_HALF without a dedicated mapping.
    const MAX_PHYS: u64 = 4 * 1024 * 1024 * 1024;

    if pml4_phys >= MAX_PHYS { return None; }

    unsafe {
        let pml4_virt =
            (crate::mm::virt::HIGHER_HALF + (pml4_phys & 0x000F_FFFF_FFFF_F000)) as *const [u64; 512];

        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdp_entry = (*pml4_virt)[pml4_idx];
        if pdp_entry & PRESENT == 0 {
            return None;
        }
        if pdp_entry & (1 << 7) != 0 {
            let phys = (pdp_entry & 0x000F_FFC0_0000_0000) | (addr & 0x3FFF_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile(
                (HIGHER_HALF + phys) as *const u64,
            ));
        }

        let pdp_phys = pdp_entry & 0x000F_FFFF_FFFF_F000;
        if pdp_phys >= MAX_PHYS { return None; }
        let pdp_virt = (HIGHER_HALF + pdp_phys) as *const [u64; 512];
        let pdp_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_entry = (*pdp_virt)[pdp_idx];
        if pd_entry & PRESENT == 0 {
            return None;
        }
        if pd_entry & (1 << 7) != 0 {
            let phys = (pd_entry & 0x000F_FFFF_FE00_0000) | (addr & 0x1F_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile(
                (HIGHER_HALF + phys) as *const u64,
            ));
        }

        let pd_phys = pd_entry & 0x000F_FFFF_FFFF_F000;
        if pd_phys >= MAX_PHYS { return None; }
        let pd_virt = (HIGHER_HALF + pd_phys) as *const [u64; 512];
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        let pt_entry = (*pd_virt)[pd_idx];
        if pt_entry & PRESENT == 0 {
            return None;
        }

        let pt_phys = pt_entry & 0x000F_FFFF_FFFF_F000;
        if pt_phys >= MAX_PHYS { return None; }
        let pt_virt = (HIGHER_HALF + pt_phys) as *const [u64; 512];
        let pt_idx = ((addr >> 12) & 0x1FF) as usize;
        let pte = (*pt_virt)[pt_idx];
        if pte & PRESENT == 0 {
            return None;
        }

        let phys = (pte & 0x000F_FFFF_FFFF_F000) | (addr & 0xFFF);
        if phys >= MAX_PHYS { return None; }
        Some(core::ptr::read_volatile(
            (HIGHER_HALF + phys) as *const u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// Stack dump  (moved from idt.rs)
// ---------------------------------------------------------------------------
fn dump_fault_stack(w: &mut impl Write, frame: &TrapFrame, kernel_pml4: u64, cpl: u64, cr3: u64) {
    let _ = writeln!(w, "--- Stack Dump (up to 32 quadwords) ---");

    let (stack_addr, use_pml4, label) = if cpl == 3 {
        (frame.rsp, cr3 & 0x000F_FFFF_FFFF_F000, "user RSP (saved by CPU)")
    } else if frame.rbp != 0 && (frame.rbp as i64 >> 47) == -1 {
        (frame.rbp.wrapping_add(16), kernel_pml4, "RBP+16 (ring-0, estimated)")
    } else if frame.rsp != 0 && (frame.rsp as i64 >> 47) == -1 {
        (frame.rsp, kernel_pml4, "frame.rsp (ring-0, may include IST save)")
    } else {
        let _ = writeln!(w, "  (original RSP unavailable)");
        return;
    };

    let _ = writeln!(w, "  (probing from {}={:#018x})", label, stack_addr);

    for row in 0..8 {
        let base = stack_addr.wrapping_add(row as u64 * 32);
        let mut all_valid = false;
        let _ = write!(w, "  {:#018x}:", base);
        for col in 0..4 {
            let addr = base.wrapping_add(col as u64 * 8);
            match probe_read_quad(use_pml4, addr) {
                Some(val) => {
                    let _ = write!(w, "  {:#018x}", val);
                    all_valid = true;
                }
                None => { let _ = write!(w, "  ______UNMAPPED______"); }
            }
        }
        let _ = writeln!(w);
        if !all_valid {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// RBP chain walk  (moved from idt.rs)
// ---------------------------------------------------------------------------
fn dump_rbp_chain(mut w: &mut impl Write, frame: &TrapFrame, kernel_pml4: u64, cr3: u64, cpl: u64) {
    let _ = writeln!(w, "--- Call Stack (RBP chain) ---");
    if frame.rbp == 0 {
        let _ = writeln!(w, "  (RBP is 0; stack chain unavailable)");
        return;
    }
    let mut rbp = frame.rbp;

    // Choose the right page tables: kernel PML4 for kernel-space RBP,
    // user PML4 (cr3) for user-space RBP.
    let pml4 = if cpl == 3 { cr3 & 0x000F_FFFF_FFFF_F000 } else { kernel_pml4 };

    for depth in 0..16 {
        if rbp == 0 { break; }
        // Validate canonical address for the current CPL
        if cpl == 3 {
            // User space: must be in lower half [0, 0x00007FFFFFFFFFFF]
            if (rbp as i64) < 0 || rbp > 0x00007FFFFFFFFFFF { break; }
        } else {
            // Kernel space: must be in upper half [0xFFFF800000000000, 0xFFFFFFFFFFFFFFFF]
            if (rbp as i64 >> 47) != -1 { break; }
        }
        let ret_addr = match probe_read_quad(pml4, rbp.wrapping_add(8)) {
            Some(v) => v,
            None => break,
        };
        let _ = write!(w, "  #{:2}  {:#018x}", depth, ret_addr);
        write_symbol_info(&mut w, ret_addr);
        let _ = writeln!(w);
        rbp = match probe_read_quad(pml4, rbp) {
            Some(v) => v,
            None => break,
        };
    }
}

// ---------------------------------------------------------------------------
// Code dump with tiny disassembler
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Kernel symbol lookup via build-time generated symtab
// ---------------------------------------------------------------------------

/// Resolve a kernel virtual address to its symbol name, offset, file, and line.
pub fn resolve_kernel_symbol(virt_addr: u64) -> Option<(&'static str, u64, &'static str, u32)> {
    use crate::arch::symtab;
    use crate::mm::virt::HIGHER_HALF;

    let needle = virt_addr.wrapping_sub(HIGHER_HALF);
    let syms = symtab::SYMBOLS;
    if syms.is_empty() {
        return None;
    }

    // Binary search for the largest symbol address <= needle
    let mut lo = 0usize;
    let mut hi = syms.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if syms[mid].addr <= needle {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    if lo == 0 {
        return None;
    }
    let idx = lo - 1;
    let sym = &syms[idx];
    Some((sym.name, needle - sym.addr, sym.file, sym.line))
}

fn write_symbol_info(w: &mut impl Write, rip: u64) {
    use crate::mm::virt::HIGHER_HALF;

    // Is this a user-space RIP?
    if rip < HIGHER_HALF {
        // Look up using current vCPU's gang symbol tables
        let cpu = crate::scheduler::current_cpu_slot();
        let vcpu_id = crate::percpu::current_vcpu(cpu) as crate::vcpu::VcpuId;
        
        let sym_info = crate::vcpu::with_mut(vcpu_id, |maybe_vcpu| {
            if let Some(vcpu) = maybe_vcpu {
                let gt = crate::scheduler::GANG_TABLE.lock();
                if let Some(Some(gang)) = gt.gangs.get(vcpu.gang_id as usize) {
                    if gang.symtab_phys != 0 {
                        return Some((gang.symtab_phys, gang.symtab_size));
                    }
                }
            }
            None
        });

        if let Some((sym_phys, sym_size)) = sym_info {
            // Read symbol table via HIGHER_HALF
            let symtab_ptr = (HIGHER_HALF + sym_phys) as *const u64;
            let num_syms = sym_size as usize / 16; // 8 byte addr + 4 byte len + 4 byte padding

            for i in 0..num_syms {
                unsafe {
                    let addr = *symtab_ptr.add(i * 2);
                    let len = *(symtab_ptr.add(i * 2 + 1) as *const u32) as usize;
                    let name_ptr = (symtab_ptr.add(i * 2 + 1) as *const u8).add(4);
                    
                    if rip >= addr && rip < addr + 0x1000 { // Crude function size check
                        let name = core::str::from_utf8(core::slice::from_raw_parts(name_ptr, len)).unwrap_or("?");
                        let _ = write!(w, "  ({})", name);
                        return;
                    }
                }
            }
        }
    }

    // Use the shared resolve_kernel_symbol function
    if let Some((name, offset, _file, _line)) = resolve_kernel_symbol(rip) {
        if offset == 0 {
            let _ = write!(w, "  {}", name);
        } else {
            let _ = write!(w, "  {}+{:#x}", name, offset);
        }
    }
}

fn dump_code_bytes(mut w: &mut impl Write, frame: &TrapFrame, kernel_pml4: u64, cr3: u64, _cpl: u64) {
    let _ = writeln!(w, "--- Code (instructions around RIP) ---");

    let pml4 = if frame.rip >= HIGHER_HALF {
        kernel_pml4
    } else {
        cr3 & 0x000F_FFFF_FFFF_F000
    };

    // Read 64 bytes centered around RIP
    let start_addr = frame.rip.saturating_sub(32) & !7;
    let mut buf = [0u8; 64];
    let mut valid = 0usize;
    for i in 0..8 {
        let addr = start_addr.wrapping_add(i as u64 * 8);
        if let Some(val) = probe_read_quad(pml4, addr) {
            buf[valid..][..8].copy_from_slice(&val.to_le_bytes());
            valid += 8;
        } else {
            valid += 8;
        }
    }

    if valid == 0 {
        let _ = writeln!(w, "  (no code readable)");
        return;
    }

    // Pre-scan: Identify instruction boundaries
    let mut insn_offsets = [0usize; 64];
    let mut num_insns = 0;
    let mut rip_insn_idx = None;

    let mut offset = 0;
    while offset < valid && num_insns < 64 {
        let addr = start_addr.wrapping_add(offset as u64);
        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len }; // Treat failed disasm as 1 byte

        insn_offsets[num_insns] = offset;
        if frame.rip >= addr && frame.rip < addr.wrapping_add(len as u64) {
            rip_insn_idx = Some(num_insns);
        }
        
        offset += len;
        num_insns += 1;
    }

    let rip_idx = rip_insn_idx.unwrap_or(0);
    let start_idx = rip_idx.saturating_sub(4);
    let end_idx = (rip_idx + 5).min(num_insns);

    // Print window
    for i in start_idx..end_idx {
        let offset = insn_offsets[i];
        let addr = start_addr.wrapping_add(offset as u64);

        let _ = write!(w, "  {:#018x}:", addr);
        
        // Disassemble to determine length
        let len = super::disasm::disasm_one(addr, &buf[offset..], &mut NullWrite).unwrap_or(0);
        let len = if len == 0 { 1 } else { len };

        for j in 0..len {
            let _ = write!(w, " {:02x}", buf[offset + j]);
        }
        let pad_len = (25usize).saturating_sub(len * 3);
        for _ in 0..pad_len { let _ = write!(w, " "); }

        super::disasm::disasm_one(addr, &buf[offset..], w);

        if i == rip_idx {
            let _ = write!(w, "  <-- RIP");
            write_symbol_info(&mut w, frame.rip);
        }
        let _ = writeln!(w);
    }
}

// ---------------------------------------------------------------------------
// Error-code decoder
// ---------------------------------------------------------------------------
fn write_error_code(w: &mut impl Write, vector: u64, code: u64) {
    match vector {
        14 => {
            let p   = (code >> 0) & 1;
            let wr  = (code >> 1) & 1;
            let us  = (code >> 2) & 1;
            let rsv = (code >> 3) & 1;
            let id  = (code >> 4) & 1;
            let pk  = (code >> 5) & 1;
            let ss  = (code >> 6) & 1;
            let sgx = (code >> 15) & 1;

            let _ = writeln!(w, "--- Page Fault Error Code ({:#x}) ---", code);
            let _ = writeln!(w, "  P    = {}  {}", p,    if p   != 0 { "Protection violation"     } else { "Not present"            });
            let _ = writeln!(w, "  W/R  = {}  {}", wr,   if wr  != 0 { "Write access"              } else { "Read access"            });
            let _ = writeln!(w, "  U/S  = {}  {}", us,   if us  != 0 { "User mode"                 } else { "Supervisor mode"        });
            let _ = writeln!(w, "  RSVD = {}", rsv);
            let _ = writeln!(w, "  I/D  = {}  {}", id,   if id  != 0 { "Instruction fetch"         } else { "Data access"            });
            let _ = writeln!(w, "  PK   = {}", pk);
            let _ = writeln!(w, "  SS   = {}", ss);
            let _ = writeln!(w, "  SGX  = {}", sgx);
        }
        10 | 11 | 12 | 13 => {
            let _ = writeln!(w, "Error code: {:#x}", code);
            let ext   = code & 1;
            let table = (code >> 1) & 3;
            let index = (code >> 3) & 0xFFFF;
            let table_name = ["GDT", "IDT", "LDT", "IDT"][table as usize];
            if ext != 0 { let _ = write!(w, "  external "); }
            let _ = writeln!(w, "  {} selector index {:#x}", table_name, index);
        }
        _ => {
            let _ = writeln!(w, "Error code: {:#x}", code);
        }
    }
}

// ---------------------------------------------------------------------------
// CPUID identification
// ---------------------------------------------------------------------------
fn write_cpuid_info(w: &mut impl Write) {
    let mut vendor = [0u8; 12];
    let mut eax_1: u32 = 0;
    let mut ecx_1: u32 = 0;
    let mut edx_1: u32 = 0;
    let mut edx_8: u32 = 0;
    let mut _ecx_7: u32 = 0;
    let mut ebx_7: u32 = 0;

    unsafe {
        // Leaf 0: vendor string
        asm!("push rbx", "mov eax, 0", "cpuid", "mov [{v}], ebx", "mov [{v}+4], edx", "mov [{v}+8], ecx", "pop rbx",
             v = in(reg) vendor.as_mut_ptr(),
             out("eax") _, out("ecx") _, out("edx") _);

        // Leaf 1: stepping, model, family, feature flags
        asm!("push rbx", "mov eax, 1", "cpuid", "mov {0:e}, eax", "mov {1:e}, ecx", "mov {2:e}, edx", "pop rbx",
             out(reg) eax_1, out(reg) ecx_1, out(reg) edx_1);

        // Leaf 7 (ECX=0): extended features
        asm!("push rbx", "mov eax, 7", "xor ecx, ecx", "cpuid", "mov {0:e}, ebx", "mov {1:e}, ecx", "pop rbx",
             out(reg) ebx_7, out(reg) _ecx_7);

        // Leaf 0x80000001: extended features (NX, SYSCALL, RDTSCP)
        asm!("push rbx", "mov eax, 0x80000001", "cpuid", "mov {0:e}, edx", "pop rbx",
             out(reg) edx_8,
             out("eax") _, out("ecx") _);
    }

    let stepping = eax_1 & 0xF;
    let model   = ((eax_1 >> 4) & 0xF) | ((eax_1 >> 12) & 0xF0);
    let family  = ((eax_1 >> 8) & 0xF) + if (eax_1 >> 8) & 0xF == 0xF { (eax_1 >> 20) & 0xFF } else { 0 };
    let v = core::str::from_utf8(&vendor).unwrap_or("unknown");

    let _ = writeln!(w, "CPUID: {}  Family {}  Model {}  Stepping {}", v, family, model, stepping);

    // Feature flags — write directly (no heapless dependency)
    let _ = write!(w, "Features:");
    macro_rules! feat { ($cond:expr, $name:expr) => { if $cond { let _ = write!(w, " {}", $name); } }; }
    feat!((edx_1 >> 25) & 1 != 0, "sse");
    feat!((edx_1 >> 26) & 1 != 0, "sse2");
    feat!((ecx_1 >> 0)  & 1 != 0, "sse3");
    feat!((ecx_1 >> 9)  & 1 != 0, "ssse3");
    feat!((ecx_1 >> 19) & 1 != 0, "sse4.1");
    feat!((ecx_1 >> 20) & 1 != 0, "sse4.2");
    feat!((ecx_1 >> 28) & 1 != 0, "avx");
    feat!((ecx_1 >> 26) & 1 != 0, "xsave");
    feat!((edx_8 >> 11) & 1 != 0, "syscall");
    feat!((edx_8 >> 20) & 1 != 0, "nx");
    feat!((edx_8 >> 27) & 1 != 0, "rdtscp");
    feat!((ebx_7 >> 7)  & 1 != 0, "smep");
    feat!((ebx_7 >> 20) & 1 != 0, "smap");
    feat!((ebx_7 >> 0)  & 1 != 0, "fsgsbase");
    let _ = writeln!(w);
}

// ---------------------------------------------------------------------------
// Important MSRs
// ---------------------------------------------------------------------------
unsafe fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi);
    ((hi as u64) << 32) | (lo as u64)
}

fn write_msrs(w: &mut impl Write) {
    let _ = writeln!(w, "--- Important MSRs ---");
    unsafe {
        let efer = read_msr(0xC0000080);
        let star = read_msr(0xC0000081);
        let lstar = read_msr(0xC0000082);
        let cstar = read_msr(0xC0000083);
        let fmask = read_msr(0xC0000084);
        let fs_base = read_msr(0xC0000100);
        let gs_base = read_msr(0xC0000101);
        let kernel_gs_base = read_msr(0xC0000102);

        let _ = writeln!(w, "EFER        = {:#018x}", efer);
        let _ = writeln!(w, "  SCE ={}   System Call Extensions", (efer >> 0) & 1);
        let _ = writeln!(w, "  LME ={}   Long Mode Enable", (efer >> 8) & 1);
        let _ = writeln!(w, "  LMA ={}   Long Mode Active", (efer >> 10) & 1);
        let _ = writeln!(w, "  NXE ={}   No-Execute Enable", (efer >> 11) & 1);
        let _ = writeln!(w, "  SVME={}   SVM Enable", (efer >> 12) & 1);
        let _ = writeln!(w, "  LMSLE={}  Long Mode Segment Limit", (efer >> 13) & 1);
        let _ = writeln!(w, "  FFXSR={}  Fast FXSAVE/FXRSTOR", (efer >> 14) & 1);
        let _ = writeln!(w, "  TCE ={}   Translation Cache Extension", (efer >> 15) & 1);
        let _ = writeln!(w, "STAR        = {:#018x}", star);
        let _ = writeln!(w, "LSTAR       = {:#018x}", lstar);
        let _ = writeln!(w, "CSTAR       = {:#018x}", cstar);
        let _ = writeln!(w, "FMASK       = {:#018x}  (IF={})", fmask, if fmask & 0x200 != 0 { "masked" } else { "unmasked" });
        let _ = writeln!(w, "FS_BASE     = {:#018x}", fs_base);
        let _ = writeln!(w, "GS_BASE     = {:#018x}", gs_base);
        let _ = writeln!(w, "KERNEL_GS_BASE = {:#018x}", kernel_gs_base);
    }
}

// ---------------------------------------------------------------------------
// Page-table walk
// ---------------------------------------------------------------------------
fn write_entry(w: &mut impl Write, label: &str, idx: usize, entry: u64) {
    let _ = write!(w, "  {}[{}] = {:#018x}", label, idx, entry);
    if entry & PRESENT  != 0 { let _ = write!(w, " P"); }
    if entry & WRITABLE != 0 { let _ = write!(w, " W"); }
    if entry & USER     != 0 { let _ = write!(w, " U"); }
    if entry & (1 << 4) != 0 { let _ = write!(w, " CD"); }
    if entry & (1 << 5) != 0 { let _ = write!(w, " A"); }
    if entry & (1 << 6) != 0 { let _ = write!(w, " D"); }
    if entry & (1 << 7) != 0 { let _ = write!(w, " PS"); }
    if entry & NO_EXECUTE != 0 { let _ = write!(w, " NX"); }
    let phys = entry & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "   phys={:#x}", phys);
}

fn dump_page_walk(w: &mut impl Write, pml4_phys: u64, addr: u64) {
    let _ = writeln!(w, "--- Page Table Walk for {:#018x} ---", addr);

    const MAX_PHYS: u64 = 4 * 1024 * 1024 * 1024;

    unsafe {
        // PML4
        if pml4_phys >= MAX_PHYS { let _ = writeln!(w, "  (PML4 phys out of range)"); return; }
        let pml4 = (HIGHER_HALF + (pml4_phys & 0x000F_FFFF_FFFF_F000)) as *const [u64; 512];
        let idx4 = ((addr >> 39) & 0x1FF) as usize;
        let e4 = (*pml4)[idx4];
        write_entry(w, "PML4", idx4, e4);
        if e4 & PRESENT == 0 { return; }

        let pdp_phys = e4 & 0x000F_FFFF_FFFF_F000;
        if pdp_phys >= MAX_PHYS { let _ = writeln!(w, "  (PDP phys out of range)"); return; }
        let pdp = (HIGHER_HALF + pdp_phys) as *const [u64; 512];
        let idx3 = ((addr >> 30) & 0x1FF) as usize;
        let e3 = (*pdp)[idx3];
        write_entry(w, "PDP", idx3, e3);
        if e3 & PRESENT == 0 { return; }
        if e3 & (1 << 7) != 0 {
            let phys = (e3 & 0x000F_FFC0_0000_0000) | (addr & 0x3FFF_FFFF);
            let _ = writeln!(w, "  → 1 GiB huge page  phys={:#x}", phys);
            return;
        }

        let pd_phys = e3 & 0x000F_FFFF_FFFF_F000;
        if pd_phys >= MAX_PHYS { let _ = writeln!(w, "  (PD phys out of range)"); return; }
        let pd = (HIGHER_HALF + pd_phys) as *const [u64; 512];
        let idx2 = ((addr >> 21) & 0x1FF) as usize;
        let e2 = (*pd)[idx2];
        write_entry(w, " PD", idx2, e2);
        if e2 & PRESENT == 0 { return; }
        if e2 & (1 << 7) != 0 {
            let phys = (e2 & 0x000F_FFFF_FE00_0000) | (addr & 0x1F_FFFF);
            let _ = writeln!(w, "  → 2 MiB huge page  phys={:#x}", phys);
            return;
        }

        let pt_phys = e2 & 0x000F_FFFF_FFFF_F000;
        if pt_phys >= MAX_PHYS { let _ = writeln!(w, "  (PT phys out of range)"); return; }
        let pt = (HIGHER_HALF + pt_phys) as *const [u64; 512];
        let idx1 = ((addr >> 12) & 0x1FF) as usize;
        let e1 = (*pt)[idx1];
        write_entry(w, " PT", idx1, e1);
        if e1 & PRESENT == 0 {
            let _ = writeln!(w, "  → unmapped");
            return;
        }

        let phys = (e1 & 0x000F_FFFF_FFFF_F000) | (addr & 0xFFF);
        let _ = writeln!(w, "  → phys={:#x}", phys);
    }
}

// ---------------------------------------------------------------------------
// APIC / Interrupt state
// ---------------------------------------------------------------------------
fn dump_apic_state(w: &mut impl Write) {
    if !crate::arch::apic::is_initialized() {
        return;
    }
    let _ = writeln!(w, "--- Local APIC State ---");

    // Task Priority Register
    let tpr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_TPR) };
    let _ = writeln!(w, "  TPR:              {:#x} (priority {})", tpr, tpr >> 4);

    // Spurious Vector Register
    let svr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_SVR) };
    let _ = writeln!(w, "  SVR:              {:#x}{}", svr, if svr & (1 << 8) != 0 { " (enabled)" } else { " (disabled)" });

    // Timer registers
    let ticr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_TICR) };
    let ccr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_CCR) };
    let tdcr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_TDCR) };
    let divider = match tdcr & 0xB {
        0x0 => 2, 0x1 => 4, 0x2 => 8, 0x3 => 16,
        0x8 => 32, 0x9 => 64, 0xA => 128, 0xB => 256,
        _ => 1,
    };
    let _ = writeln!(w, "  Timer init:       {} ticks", ticr);
    let _ = writeln!(w, "  Timer countdown:  {} ticks", ccr);
    let _ = writeln!(w, "  Timer divider:    {}", divider);

    // LVT entries
    let lvt_timer = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_LVT_TIMER) };
    let timer_mode = if lvt_timer & (1 << 17) != 0 { "periodic" } else { "one-shot" };
    let timer_mask = if lvt_timer & (1 << 16) != 0 { " (masked)" } else { "" };
    let _ = writeln!(w, "  LVT timer:        {:#x} [{}{}]", lvt_timer, timer_mode, timer_mask);

    // ISR/IRR banks — show pending interrupts
    let _ = write!(w, "  ISR:");
    let mut any_isr = false;
    for bank in 0..8 {
        let reg = unsafe { crate::arch::apic::read32(0x100 + bank * 0x10) };
        if reg != 0 {
            for bit in 0..32 {
                if reg & (1 << bit) != 0 {
                    let vec = bank * 32 + bit;
                    let _ = write!(w, " {}", vec);
                    any_isr = true;
                }
            }
        }
    }
    if !any_isr { let _ = write!(w, " (none)"); }
    let _ = writeln!(w);

    let _ = write!(w, "  IRR:");
    let mut any_irr = false;
    for bank in 0..8 {
        let reg = unsafe { crate::arch::apic::read32(0x200 + bank * 0x10) };
        if reg != 0 {
            for bit in 0..32 {
                if reg & (1 << bit) != 0 {
                    let vec = bank * 32 + bit;
                    let _ = write!(w, " {}", vec);
                    any_irr = true;
                }
            }
        }
    }
    if !any_irr { let _ = write!(w, " (none)"); }
    let _ = writeln!(w);
}

// ---------------------------------------------------------------------------
// Memory / allocator state
// ---------------------------------------------------------------------------
fn dump_memory_state(w: &mut impl Write) {
    let _ = writeln!(w, "--- Memory ---");
    let free_pages = crate::mm::phys::free_pages_count();
    let total_pages = crate::mm::phys::total_pages();
    let used_pages = total_pages.saturating_sub(free_pages);
    let free_mb = (free_pages as u64 * 4096) / (1024 * 1024);
    let total_mb = (total_pages as u64 * 4096) / (1024 * 1024);
    let _ = writeln!(w, "  Physical pages:   {} free / {} total ({} / {} MiB)",
        free_pages, total_pages, free_mb, total_mb);
    let _ = writeln!(w, "  Used:             {} pages ({} MiB)", used_pages,
        (used_pages as u64 * 4096) / (1024 * 1024));
}

// ---------------------------------------------------------------------------
// Timer / uptime
// ---------------------------------------------------------------------------
fn dump_timer_state(w: &mut impl Write) {
    let uptime = crate::arch::idt::ticks();
    let secs = uptime / 1000;
    let millis = uptime % 1000;
    let _ = writeln!(w, "--- Timer ---");
    let _ = writeln!(w, "  Uptime:           {} ms ({} s, {} ms)", uptime, secs, millis);
}

// ---------------------------------------------------------------------------
// Scheduler state
// ---------------------------------------------------------------------------
fn dump_scheduler_state(w: &mut impl Write) {
    let _ = writeln!(w, "--- Scheduler State ---");
    if !crate::scheduler::is_initialized() {
        let _ = writeln!(w, "  (scheduler not initialized)");
        return;
    }

    let cpu = crate::percpu::current_apic_id();
    let vcpu_id = crate::scheduler::current_vcpu_id();
    let _ = writeln!(w, "  CPU:              {}", cpu);
    let _ = writeln!(w, "  vCPU:             {}", vcpu_id);

    // vCPU details
    if let Some(vcpu) = crate::vcpu::get(vcpu_id) {
        let type_name = match vcpu.vcpu_type {
            crate::vcpu::VcpuType::Normal => "Normal",
            crate::vcpu::VcpuType::HardwareDriver => "HardwareDriver",
            crate::vcpu::VcpuType::AbstractionDriver => "AbstractionDriver",
            crate::vcpu::VcpuType::Idle => "Idle",
        };
        let state_name = match vcpu.state {
            crate::vcpu::VcpuState::Ready => "Ready",
            crate::vcpu::VcpuState::Running => "Running",
            crate::vcpu::VcpuState::Halted => "Halted",
            crate::vcpu::VcpuState::Blocked => "Blocked",
            crate::vcpu::VcpuState::Idle => "Idle",
        };
        let _ = writeln!(w, "  Type:             {}", type_name);
        let _ = writeln!(w, "  State:            {}", state_name);
        let _ = writeln!(w, "  Gang ID:          {}", vcpu.gang_id);
        let _ = writeln!(w, "  vruntime:         {}", vcpu.vruntime);
    }

    // Service/driver name lookup
    if let Some(svc_id) = crate::service::find_by_vcpu(vcpu_id) {
        if let Some(svc) = crate::service::get(svc_id) {
            let name = core::str::from_utf8(&svc.name).unwrap_or("?");
            let svc_state = match svc.state {
                crate::service::ServiceState::Loaded => "Loaded",
                crate::service::ServiceState::Running => "Running",
                crate::service::ServiceState::Crashed => "Crashed",
                crate::service::ServiceState::Restarting => "Restarting",
                crate::service::ServiceState::Stopped => "Stopped",
            };
            let _ = writeln!(w, "  Service:          {} (id={}, {})", name, svc.id, svc_state);
            if svc.restart_count > 0 {
                let _ = writeln!(w, "  Restart count:    {}", svc.restart_count);
            }
        }
    }

    // Per-CPU run queue info
    let slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    if slot < lodaxos_system::MAX_CPUS {
        let percpu = &crate::percpu::PERCPU[slot];
        let task_cnt = percpu.task_count.load(Ordering::Relaxed);
        let timer_fires = percpu.timer_fires.load(Ordering::Relaxed);
        let idle_vcpu = percpu.idle_vcpu_id.load(Ordering::Relaxed);
        let _ = writeln!(w, "  Tasks on CPU:     {}", task_cnt);
        let _ = writeln!(w, "  Idle vCPU:        {}", idle_vcpu);
        let _ = writeln!(w, "  Timer fires:      {}", timer_fires);
    }

    let total_vcpus = crate::vcpu::count();
    let total_services = crate::service::count();
    let _ = writeln!(w, "  Total vCPUs:      {}", total_vcpus);
    let _ = writeln!(w, "  Total services:   {}", total_services);
}

// ---------------------------------------------------------------------------
// CR0 / CR4 flag decoders
// ---------------------------------------------------------------------------
fn write_cr0_flags(w: &mut impl Write, cr0: u64) {
    let flags = [
        ("PE    ",  0, "Protected mode"),
        ("MP    ",  1, "Monitor co-processor"),
        ("EM    ",  2, "Emulation"),
        ("TS    ",  3, "Task switched"),
        ("NE    ",  5, "Numeric error"),
        ("WP    ", 16, "Write protect"),
        ("AM    ", 18, "Alignment mask"),
        ("NW    ", 29, "Not write-through"),
        ("CD    ", 30, "Cache disable"),
        ("PG    ", 31, "Paging"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr0 >> bit) & 1;
        let _ = writeln!(w, "      {} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
}

fn write_cr4_flags(w: &mut impl Write, cr4: u64) {
    let flags = [
        ("VME        ",  0, "VM Extensions"),
        ("PVI        ",  1, "Protected-mode VM"),
        ("TSD        ",  2, "Time-stamp disable"),
        ("DE         ",  3, "Debugging extensions"),
        ("PSE        ",  4, "Page size extensions"),
        ("PAE        ",  5, "Physical address extension"),
        ("MCE        ",  6, "Machine check enable"),
        ("PGE        ",  7, "Page global enable"),
        ("PCE        ",  8, "Performance counter enable"),
        ("OSFXSR     ",  9, "FXSAVE/FXRSTOR"),
        ("OSXMMEXCPT ", 10, "SSE unmasked exceptions"),
        ("UMIP       ", 11, "UMIP"),
        ("LA57       ", 12, "57-bit VA"),
        ("VMXE       ", 13, "VMX enable"),
        ("SMXE       ", 14, "SMX enable"),
        ("FSGSBASE   ", 16, "FS/GS base access"),
        ("PCIDE      ", 17, "PCID enable"),
        ("OSXSAVE    ", 18, "XSAVE"),
        ("SMEP       ", 20, "SMEP"),
        ("SMAP       ", 21, "SMAP"),
        ("PKE        ", 22, "Protection key"),
        ("CET        ", 23, "CET"),
        ("PKS        ", 24, "Protection key supervisor"),
    ];
    for &(name, bit, desc) in &flags {
        let v = (cr4 >> bit) & 1;
        let _ = writeln!(w, "      {} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
}

// ---------------------------------------------------------------------------
// Main fault-dump orchestrator
// ---------------------------------------------------------------------------
pub(crate) fn dump_full_fault(frame: &TrapFrame, vector: u64) {
    let cpu = crate::percpu::current_apic_id();
    let uptime_now = crate::arch::idt::ticks();

    // Record this exception in history before printing
    push_exception(vector, frame.rip, uptime_now);

    let mut cr0: u64 = 0;
    let mut cr2: u64 = 0;
    let mut cr4: u64 = 0;
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0);
        asm!("mov {}, cr2", out(reg) cr2);
        asm!("mov {}, cr4", out(reg) cr4);
    }
    // Read the CR3 saved at interrupt entry (before any scheduler switch).
    let cpu_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    let cr3 = crate::percpu::PERCPU[cpu_slot].saved_cr3.load(Ordering::Relaxed);

    let kernel_pml4 = crate::mm::virt::kernel_pml4();
    let cpl = frame.cs & 3;

    let mut w = DumpWriter::lock();

    let secs = uptime_now / 1000;
    let millis = uptime_now % 1000;

    let _ = writeln!(w, "================================================");
    let _ = writeln!(w, "         FAULT DIAGNOSTIC  (vector #{})  [CPU{}]", vector, cpu);
    let _ = writeln!(w, "         uptime={}.{:03} s", secs, millis);
    let _ = writeln!(w, "================================================");

    write_cpuid_info(&mut w);
    let _ = writeln!(w);

    write_error_code(&mut w, vector, frame.error_code);

    let _ = writeln!(w, "--- General Purpose Registers ---");
    let _ = writeln!(w, "RAX={:#018x}  RBX={:#018x}", frame.rax, frame.rbx);
    let _ = writeln!(w, "RCX={:#018x}  RDX={:#018x}", frame.rcx, frame.rdx);
    let _ = writeln!(w, "RSI={:#018x}  RDI={:#018x}", frame.rsi, frame.rdi);
    let _ = writeln!(w, "RBP={:#018x}  RSP={:#018x}", frame.rbp, frame.rsp);
    let _ = writeln!(w, "R8 ={:#018x}  R9 ={:#018x}", frame.r8, frame.r9);
    let _ = writeln!(w, "R10={:#018x}  R11={:#018x}", frame.r10, frame.r11);
    let _ = writeln!(w, "R12={:#018x}  R13={:#018x}", frame.r12, frame.r13);
    let _ = writeln!(w, "R14={:#018x}  R15={:#018x}", frame.r14, frame.r15);
    let _ = writeln!(w);

    let _ = writeln!(w, "--- Interrupt Frame ---");
    if cpl == 3 {
        let _ = writeln!(w, "RIP  ={:#018x}", frame.rip);
    } else {
        let _ = write!(w, "RIP  ={:#018x}", frame.rip);
        write_symbol_info(&mut w, frame.rip);
        let _ = writeln!(w);
    }
    let _ = writeln!(w, "CS   ={:#018x}  (CPL={})", frame.cs, cpl);
    let _ = writeln!(w, "RFLAGS={:#018x}", frame.rflags);
    if cpl == 3 {
        let _ = writeln!(w, "SS   ={:#018x}  (saved by CPU on CPL change)", frame.ss);
        let _ = writeln!(w, "RSP  ={:#018x}  (original user RSP)", frame.rsp);
    } else {
        let _ = writeln!(w, "  (original RSP saved only if IST or CPL-changing interrupt)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "--- Control Registers ---");
    let _ = writeln!(w, "CR0 = {:#018x}", cr0);
    write_cr0_flags(&mut w, cr0);
    let _ = writeln!(w, "CR2 = {:#018x}", cr2);
    // CR3: bit 63 = NX (not part of phys addr); bits 11:0 = ASID [PCIDE]/flags
    let cr3_asid = cr3 & 0xFFF;
    let cr3_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "CR3 = {:#018x}", cr3);
    if cr3_asid != 0 {
        let _ = writeln!(w, "      phys={:#x}  ASID/PCID={:#x}", cr3_phys, cr3_asid);
    } else {
        let _ = writeln!(w, "      phys={:#x}", cr3_phys);
    }
    let _ = writeln!(w, "CR4 = {:#018x}", cr4);
    write_cr4_flags(&mut w, cr4);
    let _ = writeln!(w);

    write_msrs(&mut w);
    let _ = writeln!(w);

    dump_timer_state(&mut w);
    let _ = writeln!(w);

    dump_memory_state(&mut w);
    let _ = writeln!(w);

    dump_apic_state(&mut w);
    let _ = writeln!(w);

    dump_exception_history(&mut w, vector, frame.rip);
    let _ = writeln!(w);

    if vector == 14 {
        dump_page_walk(&mut w, cr3 & 0x000F_FFFF_FFFF_F000, cr2);
        let _ = writeln!(w);
    }

    dump_scheduler_state(&mut w);
    let _ = writeln!(w);

    dump_fault_stack(&mut w, frame, kernel_pml4, cpl, cr3);
    let _ = writeln!(w);

    dump_rbp_chain(&mut w, frame, kernel_pml4, cr3, cpl);
    let _ = writeln!(w);

    dump_code_bytes(&mut w, frame, kernel_pml4, cr3, cpl);
    let _ = writeln!(w);

    let _ = writeln!(w, "================================================");
    // w is dropped here → serial lock released
}

/// Infinite halt loop — called after unrecoverable faults.
pub(crate) fn halt_loop() -> ! {
    log::error!("System halted.");
    loop {
        unsafe { asm!("cli; hlt") };
    }
}
