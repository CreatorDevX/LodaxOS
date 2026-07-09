use core::sync::atomic::Ordering;
use super::super::connector::Connector;
use super::super::vtparser;
use super::{resolve_arg, print_presets, confirm_or_cancel};

pub(super) fn cmd_dump(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr_token = match parser.parse_str() {
        Some(t) => t,
        None => {
            conn.write_str("error: expected (address, length)\n");
            conn.write_str("Tip: address can be a preset (heap, apic), symbol (kmain), or hex (0x...)\n");
            return;
        }
    };
    let addr = match resolve_arg(addr_token) {
        Ok(a) => a,
        Err(e) => {
            conn.write_str("error: ");
            conn.write_str(e);
            conn.write_str("\n");
            print_presets(conn);
            return;
        }
    };
    let len = match parser.parse_u64() {
        Some(l) => {
            if l > 512 {
                conn.write_str("error: max dump length is 512 bytes\n");
                return;
            }
            l
        }
        None => 64,
    };

    let hex_chars = b"0123456789abcdef";

    for offset in (0..len as usize).step_by(16) {
        let row_len = (len as usize - offset).min(16);

        conn.write_str(vtparser::sgr_fg_green());
        {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("{:#018x}: ", addr.wrapping_add(offset as u64)));
        }
        conn.write_str(vtparser::sgr_reset());

        let mut ascii_buf = [0u8; 16];
        for i in 0..row_len {
            let byte_addr = addr.wrapping_add((offset + i) as u64);
            let byte = if crate::mm::virt::translate(byte_addr).is_some() {
                crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
                let b = unsafe { (byte_addr as *const u8).read_volatile() };
                let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
                crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);
                if faulted { b'?' } else { b }
            } else {
                b'?'
            };
            ascii_buf[i] = byte;
            let hi = hex_chars[(byte >> 4) as usize];
            let lo = hex_chars[(byte & 0xF) as usize];
            let pair = [hi, lo, b' '];
            conn.write_str(unsafe { core::str::from_utf8_unchecked(&pair) });
            if i == 7 {
                conn.write_str(" ");
            }
        }

        for i in row_len..16 {
            conn.write_str("   ");
            if i == 7 {
                conn.write_str(" ");
            }
        }

        conn.write_str(" ");
        for i in 0..row_len {
            let c = ascii_buf[i];
            if c >= 0x20 && c <= 0x7E {
                let arr = [c];
                let s = unsafe { core::str::from_utf8_unchecked(&arr) };
                conn.write_str(s);
            } else {
                conn.write_str(".");
            }
        }
        conn.write_str("\n");
    }
}

pub(super) fn cmd_peek(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr_token = match parser.parse_str() {
        Some(t) => t,
        None => {
            conn.write_str("error: expected (address)\n");
            conn.write_str("Tip: address can be a preset (heap, apic), symbol (kmain), or hex (0x...)\n");
            return;
        }
    };
    let addr = match resolve_arg(addr_token) {
        Ok(a) => a,
        Err(e) => {
            conn.write_str("error: ");
            conn.write_str(e);
            conn.write_str("\n");
            print_presets(conn);
            return;
        }
    };

    let hh = crate::mm::virt::HIGHER_HALF;
    let virt_addr = hh + addr;

    let pml4 = crate::mm::virt::kernel_pml4();
    match crate::mm::virt::read_pte(pml4, virt_addr) {
        Some(pte) if pte & crate::mm::virt::PRESENT == 0 => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w,
                format_args!("{:#018x}: NOT MAPPED\n", addr));
            return;
        }
        None => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w,
                format_args!("{:#018x}: INVALID ADDRESS\n", addr));
            return;
        }
        _ => {}
    }

    let ptr = virt_addr as *const u64;
    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    let val = unsafe { ptr.read_volatile() };
    let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

    if faulted {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w,
            format_args!("{:#018x}: FAULT (read failed)\n", addr));
        return;
    }

    conn.write_str(vtparser::sgr_fg_green());
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{:#018x}", addr));
    }
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(": ");
    conn.write_str(vtparser::sgr_fg_yellow());
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{:#018x}", val));
    }
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
}

static CONFIRM_POKE_PHYS: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_POKE_VAL: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);

pub(super) fn cmd_poke(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let phys = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("error: expected (phys_addr, value)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) => v,
        None => { conn.write_str("error: expected (phys_addr, value)\n"); return; }
    };

    unsafe {
        *CONFIRM_POKE_PHYS.get() = phys;
        *CONFIRM_POKE_VAL.get() = val;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:016X} to physical address 0x{:016X}\n", val, phys));
    confirm_or_cancel(conn, "Write to physical memory?", confirm_poke);
}

fn confirm_poke(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if yes {
        let hh = crate::mm::virt::HIGHER_HALF;
        let phys = unsafe { *CONFIRM_POKE_PHYS.get() };
        let val = unsafe { *CONFIRM_POKE_VAL.get() };
        let virt_addr = hh + phys;

        let pml4 = crate::mm::virt::kernel_pml4();
        match crate::mm::virt::read_pte(pml4, virt_addr) {
            Some(pte) if pte & crate::mm::virt::PRESENT == 0 => {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w,
                    format_args!("FAULT: 0x{:016X} is NOT MAPPED\n", phys));
                return;
            }
            Some(pte) if pte & crate::mm::virt::WRITABLE == 0 => {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w,
                    format_args!("FAULT: 0x{:016X} is READ-ONLY\n", phys));
                return;
            }
            None => {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w,
                    format_args!("FAULT: 0x{:016X} INVALID ADDRESS\n", phys));
                return;
            }
            _ => {}
        }

        let ptr = virt_addr as *mut u64;
        crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
        unsafe { ptr.write_volatile(val); }
        let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
        crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

        let mut w = vtparser::ConnectorWriter { conn };
        if faulted {
            let _ = core::fmt::write(&mut w,
                format_args!("FAULT: write to 0x{:016X} failed\n", phys));
        } else {
            let _ = core::fmt::write(&mut w,
                format_args!("Wrote 0x{:016X} to 0x{:016X}\n", val, phys));
        }
    } else {
        conn.write_str("Cancelled\n");
    }
}

pub(super) fn cmd_read_port(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("error:");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str(" expected (port)\n");
            return;
        }
    };

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    let byte: u8 = unsafe { x86_64::instructions::port::Port::new(port).read() };
    let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

    if faulted {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w,
            format_args!("port {:#06x}: FAULT (GP on read)\n", port));
        return;
    }

    conn.write_str(vtparser::sgr_fg_cyan());
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("port {:#06x}", port));
    }
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(": ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{:#04x} ({})", byte, byte));
    }
    conn.write_str("\n");
}

static CONFIRM_PORT: crate::sync::SyncUnsafeCell<u16> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_PORT_VAL: crate::sync::SyncUnsafeCell<u8> = crate::sync::SyncUnsafeCell::new(0);

pub(super) fn cmd_write_port(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => { conn.write_str("error: expected (port, value)\n"); return; }
    };
    let val = match parser.parse_u64() {
        Some(v) if v <= 0xFF => v as u8,
        _ => { conn.write_str("error: expected (port, value)\n"); return; }
    };

    unsafe {
        *CONFIRM_PORT.get() = port;
        *CONFIRM_PORT_VAL.get() = val;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:02X} to I/O port 0x{:04X}\n", val, port));
    confirm_or_cancel(conn, "Write to I/O port?", confirm_write_port);
}

fn confirm_write_port(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if yes {
        let port = unsafe { *CONFIRM_PORT.get() };
        let val = unsafe { *CONFIRM_PORT_VAL.get() };

        crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
        unsafe { x86_64::instructions::port::Port::new(port).write(val); }
        let faulted = !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst);
        crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

        let mut w = vtparser::ConnectorWriter { conn };
        if faulted {
            let _ = core::fmt::write(&mut w,
                format_args!("port {:#04x}: FAULT (GP on write)\n", port));
        } else {
            let _ = core::fmt::write(&mut w,
                format_args!("Wrote 0x{:02X} to port 0x{:04X}\n", val, port));
        }
    } else {
        conn.write_str("Cancelled\n");
    }
}

pub(super) fn cmd_meminfo(_args: &str, conn: &dyn Connector) {
    let total = crate::mm::phys::total_pages();
    let free = crate::mm::phys::free_pages_count();
    let used = total - free;
    let total_mb = (total as u64 * 4096) / (1024 * 1024);
    let used_mb = (used as u64 * 4096) / (1024 * 1024);
    let free_mb = (free as u64 * 4096) / (1024 * 1024);
    let pct = if total > 0 { (used * 100) / total } else { 0 };
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "Physical Memory:\n  Total: {} MB ({} pages)\n  Used:  {} MB ({} pages) ({}%)\n  Free:  {} MB ({} pages)\n",
        total_mb, total, used_mb, used, pct, free_mb, free
    ));
}

pub(super) fn cmd_translate(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let virt_addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: translate(virtual_addr)\n"); return; }
    };
    match crate::mm::virt::translate(virt_addr) {
        Some(phys) => {
            let offset = virt_addr & 0xFFF;
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!(
                "Virtual:  0x{:016X}\nPhysical: 0x{:016X}\nOffset:   0x{:X}\n", virt_addr, phys, offset
            ));
        }
        None => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("Virtual: 0x{:016X} -- NOT MAPPED\n", virt_addr));
        }
    }
}

pub(super) fn cmd_pte(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let virt_addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: pte(virtual_addr)\n"); return; }
    };
    let pml4 = crate::mm::virt::current_pml4();
    match crate::mm::virt::read_pte(pml4, virt_addr) {
        Some(pte) => {
            let present   = (pte & 1) != 0;
            let writable  = (pte & 2) != 0;
            let user      = (pte & 4) != 0;
            let wthru     = (pte & 8) != 0;
            let cache_dis = (pte & 0x10) != 0;
            let accessed  = (pte & 0x20) != 0;
            let dirty     = (pte & 0x40) != 0;
            let ps        = (pte & 0x80) != 0;
            let global    = (pte & 0x100) != 0;
            let nx        = (pte & 0x8000_0000_0000_0000) != 0;
            let phys      = pte & 0x000F_FFFF_FFFF_F000;
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!(
                "PTE for 0x{:016X}: 0x{:016X}\n  Flags: {}{}{}{}{}{}{}{}{}{}\n  Physical page: 0x{:016X}\n",
                virt_addr, pte,
                if present  {"P"} else {"-"},
                if writable {"W"} else {"R"},
                if user     {"U"} else {"K"},
                if wthru    {"T"} else {"-"},
                if cache_dis{"C"} else {"-"},
                if accessed {"A"} else {"-"},
                if dirty    {"D"} else {"-"},
                if ps       {"PS"} else {"-"},
                if global   {"G"} else {"-"},
                if nx       {"NX"} else {"-"},
                phys
            ));
        }
        None => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("PTE for 0x{:016X} -- NOT PRESENT\n", virt_addr));
        }
    }
}

pub(super) fn cmd_vmas(_args: &str, conn: &dyn Connector) {
    use crate::mm::vma;
    use crate::arch::dump::KATERM_RECOVERY;
    conn.write_str("Kernel VMA Regions:\n");
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("{:18} {:18} {:6}\n", "START", "END", "PERM"));
    KATERM_RECOVERY.store(true, Ordering::SeqCst);
    crate::mm::vma::with_kernel_vma_tree(|tree| {
        tree.visit_all(|vma| {
            let perm = match vma.perm {
                vma::VmaPerm::None => "none",
                vma::VmaPerm::Read => "R----",
                vma::VmaPerm::Write => "-W---",
                vma::VmaPerm::ReadWrite => "-RW--",
                vma::VmaPerm::Execute => "--X--",
                vma::VmaPerm::ReadExecute => "R--X-",
                vma::VmaPerm::WriteExecute => "-W-X-",
                vma::VmaPerm::ReadWriteExecute => "RWX--",
            };
            let _ = core::fmt::write(&mut w, format_args!(
                "  0x{:016X} 0x{:016X} {:6}\n", vma.start, vma.end, perm
            ));
            Some(())
        });
    });
    let faulted = !KATERM_RECOVERY.load(Ordering::SeqCst);
    KATERM_RECOVERY.store(false, Ordering::SeqCst);
    if faulted {
        conn.write_str("(fault accessing VMA tree - data may be incomplete)\n");
    }
}

pub(super) fn cmd_pagestat(_args: &str, conn: &dyn Connector) {
    let total = crate::mm::phys::total_pages();
    let free = crate::mm::phys::free_pages_count();
    let used = total.saturating_sub(free);
    let counts = crate::mm::phys::free_counts_per_order();

    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("Physical Page Allocator\n");
    conn.write_str(vtparser::sgr_reset());
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "  Total: {} pages ({} KB)  Free: {} pages ({} KB)  Used: {} pages ({} KB)\n",
        total, total * 4, free, free * 4, used, used * 4));

    conn.write_str("\n  Order    Block Size    Free Blocks    Wasted Pages\n");
    conn.write_str("  -----    ----------    -----------    -----------\n");
    for order in 0..=10 {
        let block_pages = 1usize << order;
        let block_kb = block_pages * 4;
        let free_blocks = counts[order];
        let wasted = free_blocks * (block_pages - 1);
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:5}    {:>7} KB    {:>10}    {:>10}\n",
            order, block_kb, free_blocks, wasted));
    }
    let total_wasted: usize = counts.iter().enumerate()
        .map(|(o, &c)| c * ((1usize << o) - 1))
        .sum();
    let _ = core::fmt::write(&mut w, format_args!(
        "  Total wasted in free blocks: {} pages ({} KB)\n", total_wasted, total_wasted * 4));
}
