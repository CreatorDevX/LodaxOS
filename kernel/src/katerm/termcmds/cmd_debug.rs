use core::arch::asm;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicI32, Ordering};
use super::super::connector::Connector;
use super::super::vtparser;
use super::{BREAKPOINTS, BP_HIT_VCPU, BP_HIT_RIP, BP_HIT_VECTOR, MAX_BPS, find_bp, read_memory_bytes, confirm_or_cancel, set_register};

static CONFIRM_W16_PORT: crate::sync::SyncUnsafeCell<u16> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_W16_VAL: crate::sync::SyncUnsafeCell<u16> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_W32_PORT: crate::sync::SyncUnsafeCell<u16> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_W32_VAL: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_WRMSR_MSR: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_WRMSR_VAL: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);

pub(crate) static STEP_VCPU: AtomicI32 = AtomicI32::new(-1);

pub(super) fn cmd_set(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: set(vcpu_id, reg, val)\n"); return; }
    };
    let reg_name = match parser.parse_str() {
        Some(s) => s,
        None => { conn.write_str("Usage: set(vcpu_id, reg, val)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) => v,
        None => { conn.write_str("Usage: set(vcpu_id, reg, val)\n"); return; }
    };

    let changed = crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            match set_register(vcpu, reg_name, val) {
                Ok(old) => Some(old),
                Err(e) => { conn.write_str(e); conn.write_str("\n"); None }
            }
        } else {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("vCPU {} not found\n", vcpu_id));
            None
        }
    });

    if let Some(old) = changed {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("vCPU {}: {} changed {:#018x} -> {:#018x}\n",
            vcpu_id, reg_name, old, val));
    }
}

struct BtBuf {
    buf: [u8; 2048],
    pos: usize,
}
impl BtBuf {
    fn new() -> Self { Self { buf: [0u8; 2048], pos: 0 } }
    fn as_str(&self) -> &str { core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("") }
}
impl fmt::Write for BtBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let space = self.buf.len() - self.pos;
        let len = bytes.len().min(space);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

pub(super) fn cmd_bt(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: bt(vcpu_id)\n"); return; }
    };

    let frame = crate::vcpu::with(vcpu_id, |v| v.map(|vcpu| vcpu.saved_frame));
    let frame = match frame {
        Some(f) => f,
        None => { let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("vCPU {} not found\n", vcpu_id)); return; }
    };

    let pml4 = crate::mm::virt::kernel_pml4();
    let mut lines = BtBuf::new();
    let cpl = frame.cs & 3;

    if frame.rbp == 0 {
        let _ = lines.write_str("  (RBP=0; scanning stack for return addresses)\n");
        let stack_end = frame.rsp.wrapping_add(256);
        let mut addr = frame.rsp;
        let mut count = 0;
        while addr < stack_end && count < 8 {
            if let Some(val) = crate::arch::dump::probe_read_quad(pml4, addr) {
                let is_kernel = (val as i64 >> 47) == -1 && val >= crate::mm::virt::HIGHER_HALF;
                let is_user = (val as i64) >= 0 && val < 0x0000800000000000;
                let plausible = is_kernel || (cpl == 3 && is_user);
                if plausible {
                    let _ = core::fmt::write(&mut lines, format_args!("  #{:2}  {:#018x}", count, val));
                    if let Some((sym, offset, file, line)) = crate::arch::dump::resolve_kernel_symbol(val) {
                        let _ = core::fmt::write(&mut lines, format_args!("  {} + {:#x} ({}:{})", sym, offset, file, line));
                    } else if let Some((sym, dist, file, line)) = crate::arch::dump::find_nearest_kernel_symbol(val) {
                        if dist >= 0 {
                            let _ = core::fmt::write(&mut lines, format_args!("  {}+{:#x} (nearest, +{:#x}) ({}:{})", sym, dist, dist, file, line));
                        } else {
                            let _ = core::fmt::write(&mut lines, format_args!("  {}-{:#x} (nearest, -{:#x}) ({}:{})", sym, -dist, -dist, file, line));
                        }
                    }
                    let _ = lines.write_str("\n");
                    count += 1;
                }
            }
            addr = addr.wrapping_add(8);
        }
        if count == 0 {
            let _ = lines.write_str("  (no return addresses found on stack)\n");
        }
    } else {
        let mut rbp = frame.rbp;
        for depth in 0..16 {
            if rbp == 0 { break; }
            if cpl == 3 {
                if (rbp as i64) < 0 || rbp > 0x00007FFFFFFFFFFF { break; }
            } else {
                if (rbp as i64 >> 47) != -1 { break; }
            }
            let ret_addr = match crate::arch::dump::probe_read_quad(pml4, rbp.wrapping_add(8)) {
                Some(v) => v,
                None => break,
            };
            let _ = core::fmt::write(&mut lines, format_args!("  #{:2}  {:#018x}", depth, ret_addr));
            if let Some((sym, offset, file, line)) = crate::arch::dump::resolve_kernel_symbol(ret_addr) {
                let _ = core::fmt::write(&mut lines, format_args!("  {} + {:#x} ({}:{})", sym, offset, file, line));
            } else if let Some((sym, dist, file, line)) = crate::arch::dump::find_nearest_kernel_symbol(ret_addr) {
                if dist >= 0 {
                    let _ = core::fmt::write(&mut lines, format_args!("  {}+{:#x} (nearest, +{:#x}) ({}:{})", sym, dist, dist, file, line));
                } else {
                    let _ = core::fmt::write(&mut lines, format_args!("  {}-{:#x} (nearest, -{:#x}) ({}:{})", sym, -dist, -dist, file, line));
                }
            }
            let _ = lines.write_str("\n");
            rbp = match crate::arch::dump::probe_read_quad(pml4, rbp) {
                Some(v) => v,
                None => break,
            };
        }
    }
    conn.write_str(lines.as_str());
}

pub(super) fn cmd_stack(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: stack(vcpu_id, n_quadwords)\n"); return; }
    };
    let n = match parser.parse_u64() {
        Some(v) if v > 0 && v <= 128 => v as usize,
        _ => 16,
    };

    let rsp = crate::vcpu::with(vcpu_id, |v| v.map(|vcpu| vcpu.saved_frame.rsp));
    let rsp = match rsp {
        Some(s) => s,
        None => { let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("vCPU {} not found\n", vcpu_id)); return; }
    };

    let mut buf = [0u8; 1024];
    let read = if n * 8 <= 1024 { n * 8 } else { 1024 };
    let actual = read_memory_bytes(rsp, &mut buf[..read]);

    conn.write_str(vtparser::sgr_fg_green());
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Stack dump from RSP={:#018x} ({} qwords)\n", rsp, n));
    }
    conn.write_str(vtparser::sgr_reset());

    let hex_chars = b"0123456789abcdef";
    for offset in (0..actual).step_by(16) {
        let row_len = (actual - offset).min(16);
        {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("{:#018x}: ", rsp.wrapping_add(offset as u64)));
        }
        for i in 0..row_len {
            let b = buf[offset + i];
            let hi = hex_chars[(b >> 4) as usize];
            let lo = hex_chars[(b & 0xF) as usize];
            let pair = [hi, lo, b' '];
            conn.write_str(unsafe { core::str::from_utf8_unchecked(&pair) });
        }
        conn.write_str("\n");
    }
    if actual < read {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("  (read only {} bytes, rest unmapped)\n", actual));
    }
}

pub(super) fn cmd_read16(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => { conn.write_str("Usage: read16(port)\n"); return; }
    };
    let val: u16;
    unsafe { asm!("in ax, dx", out("ax") val, in("dx") port); }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("port {:#06x}: {:#06x} ({})\n", port, val, val));
}

pub(super) fn cmd_read32(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => { conn.write_str("Usage: read32(port)\n"); return; }
    };
    let val: u32;
    unsafe { asm!("in eax, dx", out("eax") val, in("dx") port); }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("port {:#06x}: {:#010x} ({})\n", port, val, val));
}

pub(super) fn cmd_write16(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => { conn.write_str("Usage: write16(port, val)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) if v <= 0xFFFF => v as u16,
        _ => { conn.write_str("Usage: write16(port, val)\n"); return; }
    };
    unsafe { *CONFIRM_W16_PORT.get() = port; *CONFIRM_W16_VAL.get() = val; }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:04X} to I/O port 0x{:04X}\n", val, port));
    confirm_or_cancel(conn, "Write to I/O port?", confirm_write16);
}

pub(super) fn cmd_write32(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => { conn.write_str("Usage: write32(port, val)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) if v <= 0xFFFFFFFF => v as u32,
        _ => { conn.write_str("Usage: write32(port, val)\n"); return; }
    };
    unsafe { *CONFIRM_W32_PORT.get() = port; *CONFIRM_W32_VAL.get() = val; }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:08X} to I/O port 0x{:04X}\n", val, port));
    confirm_or_cancel(conn, "Write to I/O port?", confirm_write32);
}

pub(super) fn cmd_cli(_args: &str, conn: &dyn Connector) {
    unsafe { asm!("cli"); }
    conn.write_str("RFLAGS.IF = 0 (interrupts disabled)\n");
}

pub(super) fn cmd_sti(_args: &str, conn: &dyn Connector) {
    unsafe { asm!("sti"); }
    conn.write_str("RFLAGS.IF = 1 (interrupts enabled)\n");
}

pub(super) fn cmd_rdmsr(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let msr = match parser.parse_u64() {
        Some(m) if m <= 0xFFFFFFFF => m as u32,
        _ => { conn.write_str("Usage: rdmsr(msr_number)\n"); return; }
    };
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi); }
    let val = (lo as u64) | ((hi as u64) << 32);
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("MSR {:#010x}: {:#018x}\n", msr, val));
}

pub(super) fn cmd_wrmsr(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let msr = match parser.parse_u64() {
        Some(m) if m <= 0xFFFFFFFF => m as u32,
        _ => { conn.write_str("Usage: wrmsr(msr, val)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) => v,
        _ => { conn.write_str("Usage: wrmsr(msr, val)\n"); return; }
    };
    unsafe { *CONFIRM_WRMSR_MSR.get() = msr; *CONFIRM_WRMSR_VAL.get() = val; }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:016X} to MSR {:#010x}\n", val, msr));
    confirm_or_cancel(conn, "Write MSR?", confirm_wrmsr);
}

pub(super) fn cmd_invlpg(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: invlpg(virtual_address)\n"); return; }
    };
    unsafe { asm!("invlpg [{addr}]", addr = in(reg) addr); }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("TLB flushed for {:#018x}\n", addr));
}

pub(super) fn cmd_break(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: break(address)\n"); return; }
    };

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    let original = unsafe { core::ptr::read_volatile(addr as *const u8) };
    let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);
    if faulted {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Cannot read address {:#018x}\n", addr));
        return;
    }

    if original == 0xCC {
        conn.write_str("Breakpoint already exists at this address\n");
        return;
    }

    let slot = match find_bp(0) {
        Some(s) => s,
        None => { conn.write_str("Breakpoint table full (max 8)\n"); return; }
    };

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    unsafe { core::ptr::write_volatile(addr as *mut u8, 0xCC); }
    let wfaulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);
    if wfaulted {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Cannot write to {:#018x}\n", addr));
        return;
    }

    BREAKPOINTS[slot].addr.store(addr, Ordering::SeqCst);
    BREAKPOINTS[slot].original_byte.store(original, Ordering::SeqCst);
    BREAKPOINTS[slot].enabled.store(true, Ordering::SeqCst);
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("Breakpoint {} set at {:#018x} (original: {:#04x})\n", slot, addr, original));
}

pub(super) fn cmd_del(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let index = match parser.parse_u64() {
        Some(i) if (i as usize) < MAX_BPS => i as usize,
        _ => { conn.write_str("Usage: del(index)  -- use bpl() to list indices\n"); return; }
    };

    let addr = BREAKPOINTS[index].addr.load(Ordering::SeqCst);
    let original = BREAKPOINTS[index].original_byte.load(Ordering::SeqCst);
    if addr == 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Breakpoint {} is empty\n", index));
        return;
    }

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    unsafe { core::ptr::write_volatile(addr as *mut u8, original); }
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

    BREAKPOINTS[index].addr.store(0, Ordering::SeqCst);
    BREAKPOINTS[index].original_byte.store(0, Ordering::SeqCst);
    BREAKPOINTS[index].enabled.store(false, Ordering::SeqCst);
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("Breakpoint {} removed from {:#018x}\n", index, addr));
}

pub(super) fn cmd_bpl(_args: &str, conn: &dyn Connector) {
    conn.write_str("Breakpoints:\n");
    let mut found = false;
    for (i, bp) in BREAKPOINTS.iter().enumerate() {
        let addr = bp.addr.load(Ordering::SeqCst);
        if addr != 0 {
            let enabled = bp.enabled.load(Ordering::SeqCst);
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("  {}: {:#018x}  {}\n", i, addr,
                if enabled { "enabled" } else { "disabled" }));
            found = true;
        }
    }
    if !found {
        conn.write_str("  (none)\n");
    }
}

pub(super) fn cmd_step(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: step(vcpu_id)\n"); return; }
    };

    crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.saved_frame.rflags |= 0x100; // Set TF
            vcpu.state = crate::vcpu::VcpuState::Ready;
            crate::percpu::rq(crate::scheduler::current_cpu_slot()).push(vcpu_id as usize);
            crate::percpu::set_task_count(crate::scheduler::current_cpu_slot(),
                crate::percpu::task_count(crate::scheduler::current_cpu_slot()) + 1);
            STEP_VCPU.store(vcpu_id as i32, Ordering::SeqCst);
        } else {
            conn.write_str("vCPU not found\n");
        }
    });
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("Step mode enabled for vCPU {}\n", vcpu_id));
}

pub(super) fn cmd_cont(_args: &str, conn: &dyn Connector) {
    let vcpu_id = BP_HIT_VCPU.load(Ordering::SeqCst);
    if vcpu_id < 0 {
        conn.write_str("No vCPU halted by breakpoint\n");
        return;
    }

    let vcpu_id = vcpu_id as u32;
    let hit_rip = BP_HIT_RIP.load(Ordering::SeqCst);

    for bp in BREAKPOINTS.iter() {
        let addr = bp.addr.load(Ordering::SeqCst);
        let enabled = bp.enabled.load(Ordering::SeqCst);
        if addr == hit_rip && enabled {
            crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
            unsafe { core::ptr::write_volatile(addr as *mut u8, 0xCC); }
            crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);
            break;
        }
    }

    crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.state = crate::vcpu::VcpuState::Ready;
            crate::percpu::rq(crate::scheduler::current_cpu_slot()).push(vcpu_id as usize);
            crate::percpu::set_task_count(crate::scheduler::current_cpu_slot(),
                crate::percpu::task_count(crate::scheduler::current_cpu_slot()) + 1);
        }
    });

    BP_HIT_VCPU.store(-1, Ordering::SeqCst);
    BP_HIT_RIP.store(0, Ordering::SeqCst);
    BP_HIT_VECTOR.store(0, Ordering::SeqCst);
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("vCPU {} resumed\n", vcpu_id));
}

pub(super) fn cmd_watch(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: watch(address)\n"); return; }
    };
    unsafe {
        asm!(
            "mov dr0, {addr}",
            "mov {dr7}, dr7",
            "or {dr7}, 1",
            "mov dr7, {dr7}",
            addr = in(reg) addr,
            dr7 = out(reg) _,
            options(nostack, preserves_flags),
        );
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("Hardware execution breakpoint set at {:#018x} (DR0)\n", addr));
}

fn confirm_write16(yes: bool) {
    let port = unsafe { *CONFIRM_W16_PORT.get() };
    let val = unsafe { *CONFIRM_W16_VAL.get() };
    if yes {
        unsafe { asm!("out dx, ax", in("dx") port, in("ax") val); }
    }
}

fn confirm_write32(yes: bool) {
    let port = unsafe { *CONFIRM_W32_PORT.get() };
    let val = unsafe { *CONFIRM_W32_VAL.get() };
    if yes {
        unsafe { asm!("out dx, eax", in("dx") port, in("eax") val); }
    }
}

fn confirm_wrmsr(yes: bool) {
    let msr = unsafe { *CONFIRM_WRMSR_MSR.get() };
    let val = unsafe { *CONFIRM_WRMSR_VAL.get() };
    if yes {
        let lo = val as u32;
        let hi = (val >> 32) as u32;
        unsafe { asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi); }
    }
}