//! Fault diagnostic dump -- orchestrates the full crash output.
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
// Null writer -- used to pre-compute instruction lengths without output
// ---------------------------------------------------------------------------
struct NullWrite;
impl Write for NullWrite {
    fn write_str(&mut self, _: &str) -> core::fmt::Result { Ok(()) }
}

// ---------------------------------------------------------------------------
// Safe memory probe  (moved from idt.rs)
// ---------------------------------------------------------------------------

/// Read a single u64 from a virtual address using volatile access.
/// Returns None if the pointer is null.
#[inline]
unsafe fn read_volatile_u64(ptr: *const u64) -> Option<u64> {
    if ptr.is_null() { return None; }
    Some(core::ptr::read_volatile(ptr))
}

pub fn probe_read_quad(pml4_phys: u64, addr: u64) -> Option<u64> {
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
            HIGHER_HALF + (pml4_phys & 0x000F_FFFF_FFFF_F000);

        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pml4_entry = read_volatile_u64((pml4_virt + (pml4_idx as u64) * 8) as *const u64)?;
        if pml4_entry & PRESENT == 0 {
            return None;
        }

        let pdp_phys = pml4_entry & 0x000F_FFFF_FFFF_F000;
        if pdp_phys >= MAX_PHYS { return None; }
        let pdp_virt = HIGHER_HALF + pdp_phys;
        let pdp_idx = ((addr >> 30) & 0x1FF) as usize;
        let pdp_entry = read_volatile_u64((pdp_virt + (pdp_idx as u64) * 8) as *const u64)?;
        if pdp_entry & PRESENT == 0 {
            return None;
        }
        if pdp_entry & (1 << 7) != 0 {
            // 1GB huge page
            let phys = (pdp_entry & 0x000F_FFC0_0000_0000) | (addr & 0x3FFF_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile(
                (HIGHER_HALF + phys) as *const u64,
            ));
        }

        let pd_phys = pdp_entry & 0x000F_FFFF_FFFF_F000;
        if pd_phys >= MAX_PHYS { return None; }
        let pd_virt = HIGHER_HALF + pd_phys;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;
        let pd_entry = read_volatile_u64((pd_virt + (pd_idx as u64) * 8) as *const u64)?;
        if pd_entry & PRESENT == 0 {
            return None;
        }
        if pd_entry & (1 << 7) != 0 {
            // 2MB huge page
            let phys = (pd_entry & 0x000F_FFFF_FE00_0000) | (addr & 0x1F_FFFF);
            if phys >= MAX_PHYS { return None; }
            return Some(core::ptr::read_volatile(
                (HIGHER_HALF + phys) as *const u64,
            ));
        }

        let pt_phys = pd_entry & 0x000F_FFFF_FFFF_F000;
        if pt_phys >= MAX_PHYS { return None; }
        let pt_virt = HIGHER_HALF + pt_phys;
        let pt_idx = ((addr >> 12) & 0x1FF) as usize;
        let pte = read_volatile_u64((pt_virt + (pt_idx as u64) * 8) as *const u64)?;
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
    let _ = writeln!(w, "--- Call Stack ---");
    let pml4 = if cpl == 3 { cr3 & 0x000F_FFFF_FFFF_F000 } else { kernel_pml4 };

    if frame.rbp == 0 {
        // Frame-pointer omission fallback: scan RSP for return addresses
        let _ = writeln!(w, "  (RBP=0; scanning stack for return addresses)");
        let stack_end = frame.rsp.wrapping_add(256);
        let mut addr = frame.rsp;
        let mut count = 0;
        while addr < stack_end && count < 8 {
            if let Some(val) = probe_read_quad(pml4, addr) {
                let is_kernel = (val as i64 >> 47) == -1 && val >= crate::mm::virt::HIGHER_HALF;
                let is_user = (val as i64) >= 0 && val < 0x0000800000000000;
                let plausible = is_kernel || (cpl == 3 && is_user);
                if plausible {
                    let _ = write!(w, "  #{:2}  {:#018x}", count, val);
                    write_symbol_info(&mut w, val);
                    let _ = writeln!(w);
                    count += 1;
                }
            }
            addr = addr.wrapping_add(8);
        }
        if count == 0 {
            let _ = writeln!(w, "  (no return addresses found on stack)");
        }
        return;
    }

    let mut rbp = frame.rbp;

    for depth in 0..16 {
        if rbp == 0 { break; }
        if cpl == 3 {
            if (rbp as i64) < 0 || rbp > 0x00007FFFFFFFFFFF { break; }
        } else {
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

/// Find the nearest kernel symbol to an address, with signed distance.
/// Returns (name, distance, file, line) where distance is the signed
/// offset from the symbol (positive = after symbol, negative = before).
/// Used as a fallback when resolve_kernel_symbol returns None or when
/// the user wants to see what an address is closest to.
pub fn find_nearest_kernel_symbol(virt_addr: u64) -> Option<(&'static str, i64, &'static str, u32)> {
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

    // Candidate before (lo-1) and candidate after (lo)
    let before = if lo > 0 { Some(&syms[lo - 1]) } else { None };
    let after = if lo < syms.len() { Some(&syms[lo]) } else { None };

    match (before, after) {
        (Some(b), Some(a)) => {
            // Pick whichever is closer
            let dist_before = needle as i64 - b.addr as i64;
            let dist_after = a.addr as i64 - needle as i64;
            if dist_before <= dist_after {
                Some((b.name, dist_before, b.file, b.line))
            } else {
                Some((a.name, -dist_after, a.file, a.line))
            }
        }
        (Some(b), None) => {
            let dist = needle as i64 - b.addr as i64;
            Some((b.name, dist, b.file, b.line))
        }
        (None, Some(a)) => {
            let dist = a.addr as i64 - needle as i64;
            Some((a.name, -dist, a.file, a.line))
        }
        (None, None) => None,
    }
}

fn write_symbol_info(w: &mut impl Write, rip: u64) {
    use crate::mm::virt::HIGHER_HALF;

    let mut name_buf = [0u8; 64];

    // User-space symbol lookup (drivers / ELF binaries loaded by the kernel)
    if rip < HIGHER_HALF {
        let vcpu_id = crate::scheduler::current_vcpu_id();
        let gang_id = crate::vcpu::get(vcpu_id).map(|v| v.gang_id).unwrap_or(0xFFFF);

        if gang_id != 0xFFFF {
            let (sym_phys, sym_size, str_phys) = {
                let maybe_table = crate::scheduler::GANG_TABLE.try_lock();
                match maybe_table {
                    Some(table) => match table.gangs.get(gang_id as usize).and_then(|g| g.as_ref()) {
                        Some(gang) if gang.symtab_phys != 0 => {
                            (gang.symtab_phys, gang.symtab_size, gang.strtab_phys)
                        }
                        _ => (0, 0, 0),
                    },
                    None => (0, 0, 0),
                }
            };

            if sym_phys != 0 && str_phys != 0 {
                let sym_base = (HIGHER_HALF + sym_phys) as *const u8;
                let str_base = (HIGHER_HALF + str_phys) as *const u8;
                let num_syms = sym_size as usize / 24;

                let mut best_addr = 0u64;
                let mut best_name_len = 0usize;

                for i in 0..num_syms {
                    unsafe {
                        let ent = sym_base.add(i * 24);
                        let st_name = *(ent as *const u32);
                        let st_value = *(ent.add(8) as *const u64);
                        let _st_size = *(ent.add(16) as *const u64);

                        if st_value <= rip && st_value > best_addr {
                            best_addr = st_value;

                            let mut name_len = 0usize;
                            loop {
                                let c = *str_base.add(st_name as usize + name_len);
                                if c == 0 || name_len >= name_buf.len() - 1 {
                                    break;
                                }
                                name_buf[name_len] = c;
                                name_len += 1;
                            }
                            best_name_len = name_len;
                        }
                    }
                }

                if best_addr != 0 {
                    let name = core::str::from_utf8(&name_buf[..best_name_len])
                        .unwrap_or("?");
                    let offset = rip - best_addr;
                    if offset == 0 {
                        let _ = write!(w, "  {}", name);
                    } else {
                        let _ = write!(w, "  {}+{:#x}", name, offset);
                    }
                    return;
                }
            }
        }
    }

    // Kernel symbol lookup -- also catches identity-mapped kernel code
    // in the lower half (CPL=0, RIP below HIGHER_HALF).
    // The wrapping subtraction in resolve_kernel_symbol produces a
    // gargantuan needle for non-kernel addresses; the binary-search will
    // always return the <last symbol, gargantuan offset> pair in that case.
    // Reject any offset larger than the kernel image (32 MB cap).
    if let Some((name, offset, file, line)) = resolve_kernel_symbol(rip) {
        if offset < 0x2000000 {
            if offset == 0 {
                let _ = write!(w, "  {} [{}:{}]", name, file, line);
            } else {
                let _ = write!(w, "  {}+{:#x} [{}:{}]", name, offset, file, line);
            }
            return;
        }
    }

    // No exact match -- show nearest symbol with signed distance
    if let Some((name, dist, file, line)) = find_nearest_kernel_symbol(rip) {
        if dist >= 0 {
            let _ = write!(w, "  {}+{:#x} (nearest, +{:#x} bytes after) [{}:{}]", name, dist, dist, file, line);
        } else {
            let _ = write!(w, "  {}-{:#x} (nearest, {:#x} bytes before) [{}:{}]", name, -dist, -dist, file, line);
        }
        return;
    }

    let _ = write!(w, "  (outside all mapped code)");
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
            let ext   = (code >> 0) & 1;
            let table = (code >> 1) & 3;
            let index = (code >> 3) & 0x1FFF;
            let table_name = ["GDT", "IDT", "LDT", "IDT"][table as usize];
            let _ = writeln!(w, "  External : {}", if ext != 0 { "Yes (event sourced externally)" } else { "No" });
            let _ = writeln!(w, "  Table    : {} ({})", table_name, match table { 0 => "GDT", 1 => "IDT", 2 => "LDT", _ => "IDT" });
            let _ = write!(w, "  Selector : {:#05x}", index << 3);
            if index == 0 {
                let _ = writeln!(w, " (null descriptor)");
            } else if table == 0 || table == 2 {
                let _ = writeln!(w, " (GDT/LDT index {})", index);
            } else {
                let _ = writeln!(w, " (IDT entry {})", index);
            }
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

    // Feature flags -- write directly (no heapless dependency)
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
    if entry & PRESENT         != 0 { let _ = write!(w, " P"); } else { let _ = write!(w, " ."); }
    if entry & WRITABLE        != 0 { let _ = write!(w, " W"); } else { let _ = write!(w, " ."); }
    if entry & USER            != 0 { let _ = write!(w, " U"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 5)        != 0 { let _ = write!(w, " A"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 6)        != 0 { let _ = write!(w, " D"); } else { let _ = write!(w, " ."); }
    if entry & (1 << 8)        != 0 { let _ = write!(w, " G"); }
    if entry & (1 << 7)        != 0 { let _ = write!(w, " PS"); }
    if entry & NO_EXECUTE      != 0 { let _ = write!(w, " NX"); } else { let _ = write!(w, " X"); }
    let phys = entry & 0x000F_FFFF_FFFF_F000;
    let _ = writeln!(w, "   phys={:#x}", phys);
}

fn dump_page_walk(w: &mut impl Write, pml4_phys: u64, addr: u64) {
    let _ = writeln!(w, "--- Page Table Walk for {:#018x} ---", addr);

    const MAX_PHYS: u64 = 4 * 1024 * 1024 * 1024;

    // Use probe reads throughout to avoid cascading faults if CR3 or any
    // intermediate page table pointer is corrupted.
    let pml4_phys = pml4_phys & 0x000F_FFFF_FFFF_F000;
    if pml4_phys >= MAX_PHYS { let _ = writeln!(w, "  (PML4 phys out of range)"); return; }
    let pml4_virt = HIGHER_HALF + pml4_phys;

    let idx4 = ((addr >> 39) & 0x1FF) as usize;
    let e4 = unsafe { read_volatile_u64((pml4_virt + (idx4 as u64) * 8) as *const u64) };
    let e4 = match e4 {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PML4 unreadable at {:#x})", pml4_virt); return; }
    };
    write_entry(w, "PML4", idx4, e4);
    if e4 & PRESENT == 0 { return; }

    let pdp_phys = e4 & 0x000F_FFFF_FFFF_F000;
    if pdp_phys >= MAX_PHYS { let _ = writeln!(w, "  (PDP phys out of range)"); return; }
    let pdp_virt = HIGHER_HALF + pdp_phys;
    let idx3 = ((addr >> 30) & 0x1FF) as usize;
    let e3 = unsafe { read_volatile_u64((pdp_virt + (idx3 as u64) * 8) as *const u64) };
    let e3 = match e3 {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PDP unreadable at {:#x})", pdp_virt); return; }
    };
    write_entry(w, "PDP", idx3, e3);
    if e3 & PRESENT == 0 { return; }
    if e3 & (1 << 7) != 0 {
        let phys = (e3 & 0x000F_FFC0_0000_0000) | (addr & 0x3FFF_FFFF);
        let _ = writeln!(w, "  -> 1 GiB huge page  phys={:#x}", phys);
        return;
    }

    let pd_phys = e3 & 0x000F_FFFF_FFFF_F000;
    if pd_phys >= MAX_PHYS { let _ = writeln!(w, "  (PD phys out of range)"); return; }
    let pd_virt = HIGHER_HALF + pd_phys;
    let idx2 = ((addr >> 21) & 0x1FF) as usize;
    let e2 = unsafe { read_volatile_u64((pd_virt + (idx2 as u64) * 8) as *const u64) };
    let e2 = match e2 {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PD unreadable at {:#x})", pd_virt); return; }
    };
    write_entry(w, " PD", idx2, e2);
    if e2 & PRESENT == 0 { return; }
    if e2 & (1 << 7) != 0 {
        let phys = (e2 & 0x000F_FFFF_FE00_0000) | (addr & 0x1F_FFFF);
        let _ = writeln!(w, "  -> 2 MiB huge page  phys={:#x}", phys);
        return;
    }

    let pt_phys = e2 & 0x000F_FFFF_FFFF_F000;
    if pt_phys >= MAX_PHYS { let _ = writeln!(w, "  (PT phys out of range)"); return; }
    let pt_virt = HIGHER_HALF + pt_phys;
    let idx1 = ((addr >> 12) & 0x1FF) as usize;
    let e1 = unsafe { read_volatile_u64((pt_virt + (idx1 as u64) * 8) as *const u64) };
    let e1 = match e1 {
        Some(v) => v,
        None => { let _ = writeln!(w, "  (PT unreadable at {:#x})", pt_virt); return; }
    };
    write_entry(w, " PT", idx1, e1);
    if e1 & PRESENT == 0 {
        let _ = writeln!(w, "  -> unmapped");
        return;
    }

    let phys = (e1 & 0x000F_FFFF_FFFF_F000) | (addr & 0xFFF);
    let _ = writeln!(w, "  -> phys={:#x}", phys);
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

    // ISR/IRR banks -- show pending interrupts
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
            crate::vcpu::VcpuState::Terminated => "Terminated",
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
// RFLAGS decoder
// ---------------------------------------------------------------------------
fn write_rflags(w: &mut impl Write, rflags: u64) {
    let flags = [
        ("CF",  0, "Carry"),
        ("PF",  2, "Parity"),
        ("AF",  4, "Adjust"),
        ("ZF",  6, "Zero"),
        ("SF",  7, "Sign"),
        ("TF",  8, "Trap (single-step)"),
        ("IF",  9, "Interrupt enable"),
        ("DF", 10, "Direction"),
        ("OF", 11, "Overflow"),
        ("IOPL", 12, "I/O privilege level"),
        ("NT", 14, "Nested task"),
        ("RF", 16, "Resume"),
        ("VM", 17, "Virtual-8086 mode"),
        ("AC", 18, "Alignment check"),
        ("VIF", 19, "Virtual interrupt"),
        ("VIP", 20, "Virtual interrupt pending"),
        ("ID", 21, "ID flag"),
    ];
    let iopl = (rflags >> 12) & 3;
    for &(name, bit, desc) in &flags {
        if bit == 12 { continue; } // IOPL handled separately
        let v = (rflags >> bit) & 1;
        let _ = writeln!(w, "      {:4} = {}  {}", name, v, if v != 0 { desc } else { "" });
    }
    let _ = writeln!(w, "      IOPL = {}  I/O privilege level {}", iopl, iopl);
}

// ---------------------------------------------------------------------------
// Interrupt state
// ---------------------------------------------------------------------------
fn dump_interrupt_state(w: &mut impl Write) {
    let _ = writeln!(w, "--- Interrupt State ---");
    // Read current RFLAGS to check IF
    let rflags: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) rflags); }
    let if_flag = (rflags >> 9) & 1;
    let _ = writeln!(w, "  Interrupts  : {}", if if_flag != 0 { "enabled (IF=1)" } else { "disabled (IF=0)" });
    // Tracked IRQ nesting depth from APIC TPR
    if crate::arch::apic::is_initialized() {
        let tpr = unsafe { crate::arch::apic::read32(crate::arch::apic::APIC_TPR) };
        let priority = tpr >> 4;
        let _ = writeln!(w, "  TPR priority: {} (nesting depth hint)", priority);
    }
}

// ---------------------------------------------------------------------------
// Process / task info
// ---------------------------------------------------------------------------
fn write_current_task_info(w: &mut impl Write) {
    let vcpu_id = crate::scheduler::current_vcpu_id();

    let maybe_vcpu = crate::vcpu::get(vcpu_id);
    let vcpu_type = maybe_vcpu.as_ref().map(|v| v.vcpu_type).unwrap_or(crate::vcpu::VcpuType::Idle);
    let gang_id = maybe_vcpu.as_ref().map(|v| v.gang_id).unwrap_or(0xFFFF);

    if vcpu_type == crate::vcpu::VcpuType::Idle {
        let _ = writeln!(w, "Task     : idle");
        let _ = writeln!(w, "Process  : kernel (PID=0)");
        return;
    }

    if gang_id != 0xFFFF && (gang_id as usize) < 32 {
        if let Some(table) = crate::scheduler::GANG_TABLE.try_lock() {
            if let Some(ref gang) = table.gangs[gang_id as usize] {
                let raw = core::str::from_utf8(&gang.name).unwrap_or("?");
                let end = raw.find('\0').unwrap_or(raw.len());
                let name = &raw[..end];
                let _ = writeln!(w, "Task     : {} (vCPU {})", name, vcpu_id);
                let _ = writeln!(w, "Process  : {} (PID={})", name, gang.id);
            } else {
                let _ = writeln!(w, "Task     : vCPU {} (gang {} missing)", vcpu_id, gang_id);
                let _ = writeln!(w, "Process  : kernel (PID=0)");
            }
        } else {
            let _ = writeln!(w, "Task     : vCPU {} (GANG_TABLE lock contended)", vcpu_id);
            let _ = writeln!(w, "Process  : (unknown)");
        }
    } else {
        let _ = writeln!(w, "Task     : vCPU {} (no gang)", vcpu_id);
        let _ = writeln!(w, "Process  : kernel (PID=0)");
    }
}

// ---------------------------------------------------------------------------
// Address info: canonical, user, mapped, executable
// ---------------------------------------------------------------------------
fn write_address_info(w: &mut impl Write, addr: u64, pml4_phys: u64) {
    const MAX_PHYS: u64 = 4 * 1024 * 1024 * 1024;

    let ext = (addr as i64) >> 47;
    let canonical = ext == 0 || ext == -1;
    let _ = writeln!(w, "  canonical: {}", if canonical { "yes" } else { "no" });

    if !canonical {
        let _ = writeln!(w, "  userspace: no");
        let _ = writeln!(w, "  mapped   : no");
        let _ = writeln!(w, "  executable: no");
        return;
    }

    let user_ok = addr < 0x0000800000000000;
    let _ = writeln!(w, "  userspace: {}", if user_ok { "yes" } else { "no" });

    let (mapped, executable) = if pml4_phys >= MAX_PHYS {
        (false, false)
    } else {
        unsafe {
            let pml4_virt = (HIGHER_HALF + (pml4_phys & 0x000F_FFFF_FFFF_F000))
                as *const [u64; 512];
            let idx4 = ((addr >> 39) & 0x1FF) as usize;
            let e4 = (*pml4_virt)[idx4];
            if e4 & PRESENT == 0 { (false, false) }
            else {
                let pdp_phys = e4 & 0x000F_FFFF_FFFF_F000;
                if pdp_phys >= MAX_PHYS { (false, false) }
                else {
                    let pdp = (HIGHER_HALF + pdp_phys) as *const [u64; 512];
                    let idx3 = ((addr >> 30) & 0x1FF) as usize;
                    let e3 = (*pdp)[idx3];
                    if e3 & PRESENT == 0 { (false, false) }
                    else if e3 & (1 << 7) != 0 {
                        (true, e3 & NO_EXECUTE == 0)
                    } else {
                        let pd_phys = e3 & 0x000F_FFFF_FFFF_F000;
                        if pd_phys >= MAX_PHYS { (false, false) }
                        else {
                            let pd = (HIGHER_HALF + pd_phys) as *const [u64; 512];
                            let idx2 = ((addr >> 21) & 0x1FF) as usize;
                            let e2 = (*pd)[idx2];
                            if e2 & PRESENT == 0 { (false, false) }
                            else if e2 & (1 << 7) != 0 {
                                (true, e2 & NO_EXECUTE == 0)
                            } else {
                                let pt_phys = e2 & 0x000F_FFFF_FFFF_F000;
                                if pt_phys >= MAX_PHYS { (false, false) }
                                else {
                                    let pt = (HIGHER_HALF + pt_phys) as *const [u64; 512];
                                    let idx1 = ((addr >> 12) & 0x1FF) as usize;
                                    let e1 = (*pt)[idx1];
                                    if e1 & PRESENT == 0 { (false, false) }
                                    else { (true, e1 & NO_EXECUTE == 0) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    let _ = writeln!(w, "  mapped   : {}", if mapped { "yes" } else { "no" });
    let _ = writeln!(w, "  executable: {}", if executable { "yes" } else { "no" });
}

// ---------------------------------------------------------------------------
// Main fault-dump orchestrator
// ---------------------------------------------------------------------------
pub(crate) fn dump_full_fault(frame: &TrapFrame, vector: u64) {
    // Re-entrancy guard: if a fault occurs inside the dump itself, halt
    // immediately instead of recursing and triple-faulting (Bug 26).
    if DUMP_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        log::error!("Fault inside fault dump -- halting.");
        halt_loop();
    }

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

    // -- Header ------------------------------------------------------
    match vector {
        14 => { let _ = writeln!(w, "==== PAGE FAULT (#14) {:=>31}", ""); }
        13 => { let _ = writeln!(w, "==== GENERAL PROTECTION (#13) {:=>21}", ""); }
        6  => { let _ = writeln!(w, "==== INVALID OPCODE (#6) {:=>27}", ""); }
        0  => { let _ = writeln!(w, "==== DIVIDE ERROR (#0) {:=>28}", ""); }
        8  => { let _ = writeln!(w, "==== DOUBLE FAULT (#8) {:=>28}", ""); }
        _  => { let _ = writeln!(w, "==== {} (#{}) {:=>31}", exception_name(vector), vector, ""); }
    }
    let _ = writeln!(w, "CPU      : {}", cpu);
    let _ = writeln!(w, "Uptime   : {} ms ({} s, {} ms)", uptime_now, secs, millis);
    write_current_task_info(&mut w);
    for _ in 0..48 { let _ = write!(w, "="); }
    let _ = writeln!(w);
    let _ = writeln!(w, "Task failed successfully.");
    let _ = writeln!(w, "Whoops.");
    let _ = writeln!(w);

    // -- Reason section for page faults -----------------------------
    if vector == 14 {
        let p   = (frame.error_code >> 0) & 1;
        let wr  = (frame.error_code >> 1) & 1;
        let us  = (frame.error_code >> 2) & 1;
        let rsv = (frame.error_code >> 3) & 1;
        let id  = (frame.error_code >> 4) & 1;
        let pk  = (frame.error_code >> 5) & 1;
        let ss  = (frame.error_code >> 6) & 1;

        let _ = writeln!(w, "Reason");
        let _ = writeln!(w, "------");
        if p   != 0 { let _ = writeln!(w, "Protection violation"); }
        else        { let _ = writeln!(w, "Non-present page"); }
        if wr  != 0 { let _ = writeln!(w, "Write access");       }
        else        { let _ = writeln!(w, "Read access");        }
        if us  != 0 { let _ = writeln!(w, "User mode");          }
        if id  != 0 { let _ = writeln!(w, "Instruction fetch");  }
        if rsv != 0 { let _ = writeln!(w, "Reserved bit violation"); }
        if pk  != 0 { let _ = writeln!(w, "Protection key violation"); }
        if ss  != 0 { let _ = writeln!(w, "Shadow stack access"); }
        let _ = writeln!(w);
    }

    // -- Fault Address section -------------------------------------
    let fault_addr = if vector == 14 { cr2 } else { frame.rip };
    let _ = writeln!(w, "Fault Address");
    let _ = writeln!(w, "-------------");
    if vector == 14 {
        let _ = writeln!(w, "CR2 : {:#018x}  (CPL={})", fault_addr, cpl);
    } else {
        let _ = writeln!(w, "RIP : {:#018x}  (CPL={})", fault_addr, cpl);
    }
    write_address_info(&mut w, fault_addr, cr3 & 0x000F_FFFF_FFFF_F000);
    let _ = writeln!(w);

    if vector != 14 {
        write_error_code(&mut w, vector, frame.error_code);
        let _ = writeln!(w);
    }

    write_cpuid_info(&mut w);
    let _ = writeln!(w);

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
    let _ = write!(w, "RIP  ={:#018x}", frame.rip);
    write_symbol_info(&mut w, frame.rip);
    let _ = writeln!(w);
    let _ = writeln!(w, "CS   ={:#018x}  (CPL={})", frame.cs, cpl);
    let _ = writeln!(w, "RFLAGS={:#018x}", frame.rflags);
    write_rflags(&mut w, frame.rflags);
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

    dump_interrupt_state(&mut w);
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
    // w is dropped here -> serial lock released
    DUMP_IN_PROGRESS.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// IPI remote register-dump protocol
// ---------------------------------------------------------------------------

/// CPU slot of the source CPU requesting a remote register dump via IPI.
/// Set to `u64::MAX` (default) when idle.
pub static DUMP_REQ_SLOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);

/// Set to `1` by the target CPU after it has finished dumping its state.
pub static DUMP_ACK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Set to `true` while dump_full_fault is executing.
/// Prevents re‑entrant fault‑in‑dump → triple‑fault escalation.
pub static DUMP_IN_PROGRESS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Set to `true` when a CPU enters the halt loop after a fatal fault.
/// The timer ISR checks this flag to skip the scheduler and only
/// service serial/katerm interrupts.
pub static HALT_MODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// True while a katerm operation is armed for fault recovery.
/// When set, the exception handler skips the faulting instruction
/// (for vector 13 GP or 14 PF) instead of calling dump_full_fault+halt_loop.
pub static KATERM_RECOVERY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Set to `true` when a katerm "freeze" command halts other CPUs.
/// The NMI handler checks this to enter a holding loop.
pub static FREEZE_ALL_CPUS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// ---- Rescue mode state saved on fault ----

use core::sync::atomic::AtomicU64;

pub static RESCUE_CR3: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_CR0: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_CR4: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_EFER: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_CPU: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_FAULT_VECTOR: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_FAULT_CR2: AtomicU64 = AtomicU64::new(0);
pub static RESCUE_FAULT_FRAME: SyncUnsafeCell<Option<TrapFrame>> = SyncUnsafeCell::new(None);

/// Save rescue state from the fault handler before entering the debugger.
pub fn save_rescue_state(frame: &TrapFrame, vector: u64) {
    let cr0: u64;
    let cr2: u64;
    let cr3_val: u64;
    let cr4: u64;
    unsafe {
        core::arch::asm!("mov {}, cr0", out(reg) cr0);
        core::arch::asm!("mov {}, cr2", out(reg) cr2);
        core::arch::asm!("mov {}, cr3", out(reg) cr3_val);
        core::arch::asm!("mov {}, cr4", out(reg) cr4);
    }
    let efer: u64 = unsafe { read_msr(0xC0000080) };
    let cpu = crate::percpu::current_apic_id() as u64;

    RESCUE_CR0.store(cr0, Ordering::Release);
    RESCUE_CR3.store(cr3_val, Ordering::Release);
    RESCUE_CR4.store(cr4, Ordering::Release);
    RESCUE_EFER.store(efer, Ordering::Release);
    RESCUE_CPU.store(cpu, Ordering::Release);
    RESCUE_FAULT_VECTOR.store(vector, Ordering::Release);
    RESCUE_FAULT_CR2.store(cr2, Ordering::Release);
    unsafe { *RESCUE_FAULT_FRAME.get() = Some(*frame); }

    log::info!("rescue: saved state (CPU={}, vector={}, CR2={:#x})", cpu, vector, cr2);
}

/// Infinite halt loop -- called after unrecoverable faults.
/// Enters the katerm rescue debugger instead of a simple halt loop.
pub(crate) fn halt_loop() -> ! {
    log::error!("System halted -- entering rescue debugger.");
    HALT_MODE.store(true, Ordering::Release);
    crate::katerm::enter_rescue_mode();
}
