use super::super::connector::Connector;
use super::super::vtparser;
use super::{FmtBuffer, resolve_arg, print_presets, read_memory_bytes};

pub(super) fn cmd_symbols(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let pattern = parser.parse_str().unwrap_or("").trim();
    let syms = crate::arch::symtab::SYMBOLS;
    let mut found = 0usize;
    for sym in syms {
        if pattern.is_empty() || sym.name.contains(pattern) || sym.name.eq_ignore_ascii_case(pattern) {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("  0x{:016X}  {}  {}:{}\n", sym.addr, sym.name, sym.file, sym.line));
            found += 1;
            if found >= 100 {
                conn.write_str("  ... (showing first 100; use symbols(pattern) to filter)\n");
                return;
            }
        }
    }
    if found == 0 && !pattern.is_empty() {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("No symbols matching '{}'\n", pattern));
    } else if found == 0 && pattern.is_empty() {
        conn.write_str("(symbol table is empty -- ~1900 entries expected)\n");
    } else {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{} symbols shown\n", found));
    }
}

pub(super) fn cmd_lookup(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => {
            conn.write_str("Usage: lookup(address)\n");
            conn.write_str("Resolves an address to its function name, file, and line.\n");
            conn.write_str("Example: lookup(0xFFFFFFFF80100000)\n");
            return;
        }
    };
    match crate::arch::dump::resolve_kernel_symbol(addr) {
        Some((name, offset, file, line)) => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("Address: 0x{:016X}\n", addr));
            let _ = core::fmt::write(&mut w, format_args!("Symbol:  {}+0x{:X}\n", name, offset));
            let _ = core::fmt::write(&mut w, format_args!("Source:  {}:{}\n", file, line));
        }
        None => {
            if let Some((name, dist, file, line)) = crate::arch::dump::find_nearest_kernel_symbol(addr) {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("Address: 0x{:016X}\n", addr));
                if dist >= 0 {
                    let _ = core::fmt::write(&mut w, format_args!("Nearest: {}+0x{:X} ({} bytes after symbol)\n", name, dist, dist));
                } else {
                    let _ = core::fmt::write(&mut w, format_args!("Nearest: {}-0x{:X} ({} bytes before symbol)\n", name, -dist, -dist));
                }
                let _ = core::fmt::write(&mut w, format_args!("Source:  {}:{}\n", file, line));
            } else {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("No symbol found for 0x{:016X} (address may need HIGHER_HALF offset)\n", addr));
            }
        }
    }
}

pub(super) fn cmd_disasm(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr_token = match parser.parse_str() {
        Some(t) => t,
        None => {
            conn.write_str("Usage: disasm(address, count)\n");
            conn.write_str("Disassembles x86-64 instructions at the given address.\n");
            conn.write_str("Examples:\n");
            conn.write_str("  disasm(code, 5)       -- 5 instructions from kernel code start\n");
            conn.write_str("  disasm(0xFFFFFFFF81000000, 10)\n");
            conn.write_str("  disasm(kmain)          -- default 10 instructions\n");
            return;
        }
    };
    let addr = match resolve_arg(addr_token) {
        Ok(a) => a,
        Err(e) => {
            conn.write_str(e);
            conn.write_str("\n");
            print_presets(conn);
            return;
        }
    };
    let count = parser.parse_u64().unwrap_or(10).min(50) as usize;

    let mut bytes = [0u8; 128];
    let bytes_read = read_memory_bytes(addr, &mut bytes);
    if bytes_read == 0 {
        conn.write_str("Cannot read memory at that address (unmapped or invalid)\n");
        return;
    }

    if let Some((name, offset, _, _)) = crate::arch::dump::resolve_kernel_symbol(addr) {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Disassembly of {}+0x{:X}:\n", name, offset));
    } else if let Some((name, dist, _, _)) = crate::arch::dump::find_nearest_kernel_symbol(addr) {
        let mut w = vtparser::ConnectorWriter { conn };
        if dist >= 0 {
            let _ = core::fmt::write(&mut w, format_args!("Disassembly of {}+0x{:X} (nearest, +{:#x}):\n", name, dist, dist));
        } else {
            let _ = core::fmt::write(&mut w, format_args!("Disassembly of {}-0x{:X} (nearest, -{:#x}):\n", name, -dist, -dist));
        }
    } else {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Disassembly of 0x{:016X}:\n", addr));
    }

    let mut fb = FmtBuffer::new();
    let mut pos = 0usize;
    let mut printed = 0usize;
    while printed < count && pos < bytes_read {
        fb.clear();
        let inst_addr = addr + pos as u64;
        if let Some(len) = crate::arch::disasm::disasm_one(inst_addr, &bytes[pos..], &mut fb) {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("  0x{:016X}  {}\n", inst_addr, fb.as_str()));
            pos += len;
            printed += 1;
        } else {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("  0x{:016X}  db 0x{:02X}\n", inst_addr, bytes[pos]));
            pos += 1;
        }
    }
}
