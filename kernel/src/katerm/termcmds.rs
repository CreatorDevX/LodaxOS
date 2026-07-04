use core::arch::asm;
use core::fmt::{self, Write as _};
use super::connector::Connector;
use super::vtparser;

// ── FmtBuffer: stack-allocated string buffer for disasm output ─────
struct FmtBuffer {
    buf: [u8; 256],
    pos: usize,
}

impl FmtBuffer {
    fn new() -> Self { Self { buf: [0u8; 256], pos: 0 } }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }
    fn clear(&mut self) { self.pos = 0; }
}

impl fmt::Write for FmtBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let space = self.buf.len() - self.pos;
        let len = bytes.len().min(space);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

// ── Safe memory reader ──────────────────────────────────────────────
fn read_memory_bytes(virt: u64, buf: &mut [u8]) -> usize {
    use crate::mm::virt;
    let mut read = 0;
    for byte in buf.iter_mut() {
        let page = virt + read as u64;
        if virt::translate(page).is_none() {
            break;
        }
        unsafe { *byte = core::ptr::read_volatile(page as *const u8); }
        read += 1;
    }
    read
}

// ── Named address presets ───────────────────────────────────────────
struct Preset {
    alias: &'static str,
    name: &'static str,
    addr: u64,
}

static PRESETS: &[Preset] = &[
    Preset { alias: "code",       name: "__kernel_start", addr: 0xFFFF_8000_0010_0000 },
    Preset { alias: "kernel_code",name: "__kernel_start", addr: 0xFFFF_8000_0010_0000 },
    Preset { alias: "heap",       name: "kernel_heap",    addr: 0xFFFF_8080_0000_0000 },
    Preset { alias: "kernel_heap",name: "kernel_heap",    addr: 0xFFFF_8080_0000_0000 },
    Preset { alias: "apic",       name: "lapic",          addr: 0x0000_FEE0_0000_0000 },
    Preset { alias: "ioapic",     name: "io_apic",        addr: 0x0000_FEC0_0000_0000 },
    Preset { alias: "trampoline", name: "sipi_trampoline",addr: 0x0000_0000_0000_8000 },
];

/// Try to resolve a user-provided address token to a u64 virtual address.
/// Supports: raw hex (0x...), named presets (heap, apic), symbol names (kmain).
fn resolve_arg(arg: &str) -> Result<u64, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("empty address");
    }
    // 1. Raw hex
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_err(|_| "invalid hex address");
    }
    // Try plain decimal
    if let Ok(val) = u64::from_str_radix(trimmed, 10) {
        return Ok(val);
    }
    // 2. Named preset
    for p in PRESETS {
        if trimmed.eq_ignore_ascii_case(p.alias) || trimmed.eq_ignore_ascii_case(p.name) {
            return Ok(p.addr);
        }
    }
    // 3. Symbol name lookup
    let half = crate::mm::virt::HIGHER_HALF;
    for sym in crate::arch::symtab::SYMBOLS {
        if sym.name.eq_ignore_ascii_case(trimmed) || sym.name.contains(trimmed) {
            return Ok(sym.addr + half);
        }
    }
    Err("unknown address — try 0x..., a named preset (heap, apic), or a symbol name")
}

fn print_presets(conn: &dyn Connector) {
    conn.write_str("Named address presets:\n");
    for p in PRESETS {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("  {:14} -> 0x{:016X}  ({})\n", p.alias, p.addr, p.name));
    }
    conn.write_str("  Or type a symbol name (e.g. 'kmain')\n");
    conn.write_str("  Or raw hex: 0x...\n");
}

// ── Confirmation helpers ────────────────────────────────────────────
fn confirm_or_cancel(conn: &dyn Connector, msg: &str, callback: fn(bool)) {
    conn.write_str(msg);
    conn.write_str("\n");
    super::request_confirm("Proceed? [y/N]: ", callback);
}

pub struct CmdDef {
    pub name: &'static str,
    pub signature: &'static str,
    pub help: &'static str,
    pub exec: fn(args: &str, conn: &dyn Connector),
}

pub const COMMANDS: &[CmdDef] = &[
    CmdDef { name: "help",       signature: "(topic)",    help: "Show detailed help (try help memory, help tasks, help dump)", exec: cmd_help },
    CmdDef { name: "clear",      signature: "()",         help: "Clear terminal screen", exec: cmd_clear },
    CmdDef { name: "echo",       signature: "(msg)",      help: "Echo a message", exec: cmd_echo },
    // Symbols
    CmdDef { name: "symbols",    signature: "(filter)",   help: "List kernel symbols (all or matching filter)", exec: cmd_symbols },
    CmdDef { name: "lookup",     signature: "(addr)",     help: "Resolve address to symbol name + file:line", exec: cmd_lookup },
    CmdDef { name: "disasm",     signature: "(addr, n)",  help: "Disassemble x86-64 instructions at address", exec: cmd_disasm },
    // Memory
    CmdDef { name: "dump",       signature: "(addr, len)",help: "Hex dump memory (presets: heap, apic; or symbol/hex)", exec: cmd_dump },
    CmdDef { name: "peek",       signature: "(addr)",     help: "Read 64-bit from memory (presets or symbol/hex)", exec: cmd_peek },
    CmdDef { name: "poke",       signature: "(addr, val)",help: "Write 64-bit to physical memory (requires confirm)", exec: cmd_poke },
    CmdDef { name: "meminfo",    signature: "()",         help: "Physical memory stats (total/used/free)", exec: cmd_meminfo },
    CmdDef { name: "translate",  signature: "(virt_addr)",help: "Translate virtual address to physical", exec: cmd_translate },
    CmdDef { name: "pte",        signature: "(virt_addr)",help: "Show page table entry with decoded flags", exec: cmd_pte },
    CmdDef { name: "vmas",       signature: "()",         help: "List kernel virtual memory areas", exec: cmd_vmas },
    // Tasks
    CmdDef { name: "ps",         signature: "()",         help: "List all gangs (processes)", exec: cmd_ps },
    CmdDef { name: "trace",      signature: "(vcpu_id)",  help: "Full register dump of a vCPU", exec: cmd_trace },
    CmdDef { name: "vcpus",      signature: "()",         help: "List all allocated vCPUs", exec: cmd_vcpus },
    // Scheduler
    CmdDef { name: "loadavg",    signature: "()",         help: "Per-CPU task counts and load", exec: cmd_loadavg },
    CmdDef { name: "rq",         signature: "(cpu)",      help: "Peek at a CPU's ready queue", exec: cmd_rq },
    // Drivers
    CmdDef { name: "drivers",    signature: "()",         help: "List registered GDF drivers", exec: cmd_drivers },
    CmdDef { name: "services",   signature: "()",         help: "List running services", exec: cmd_services },
    CmdDef { name: "drv_call",   signature: "(name,cmd,args)", help: "Send command to a driver (requires confirm)", exec: cmd_drv_call },
    // Hardware
    CmdDef { name: "cpuinfo",    signature: "()",         help: "List online CPUs", exec: cmd_cpuinfo },
    CmdDef { name: "lapic",      signature: "()",         help: "Show LAPIC registers (TPR, SVR, timer, ISR)", exec: cmd_lapic },
    CmdDef { name: "ioapic_dump",signature: "(index)",    help: "Dump IOAPIC redirection entries", exec: cmd_ioapic_dump },
    CmdDef { name: "irq",        signature: "()",         help: "Show IOAPIC interrupt routing table", exec: cmd_irq },
    CmdDef { name: "ticks",      signature: "()",         help: "Show timer tick counts", exec: cmd_ticks },
    CmdDef { name: "dumpcpu",    signature: "(cpu)",      help: "Dump CPU state (e.g. CPU0)", exec: cmd_dumpcpu },
    // I/O
    CmdDef { name: "read",       signature: "(port)",     help: "Read byte from I/O port", exec: cmd_read_port },
    CmdDef { name: "write",      signature: "(port,val)", help: "Write byte to I/O port (requires confirm)", exec: cmd_write_port },
    // System
    CmdDef { name: "reboot",     signature: "()",         help: "Reboot the system (requires confirm)", exec: cmd_reboot },
];

// ── Command implementations ─────────────────────────────────────────

fn cmd_help(args: &str, conn: &dyn Connector) {
    let topic = args.trim();
    if topic.is_empty() {
        help_main(conn);
        return;
    }
    match topic {
        "memory" | "mem" => help_memory(conn),
        "tasks" | "proc" | "ps" => help_tasks(conn),
        "sched" | "scheduler" => help_sched(conn),
        "drivers" | "drv" => help_drivers(conn),
        "hardware" | "hw" => help_hardware(conn),
        "io" => help_io(conn),
        "dump" | "peek" => help_dump(conn),
        "symbols" | "lookup" | "disasm" => help_symbols(conn),
        _ => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("Unknown topic '");
            conn.write_str(topic);
            conn.write_str("'.\n");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str("Try: memory, tasks, sched, drivers, hardware, io, dump, symbols\n");
        }
    }
}

fn help_main(conn: &dyn Connector) {
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══════════════════════════════════════════════════════════\n");
    conn.write_str("  LodaxOS Kernel Access Terminal (katerm) v0.1\n");
    conn.write_str("═══════════════════════════════════════════════════════════\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str("  Type ");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("help(topic)");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" for details on a category:\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(memory)   ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Memory inspection (dump, peek, meminfo, ...)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(tasks)    ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Process/task inspection (ps, trace, vcpus)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(sched)    ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Scheduler info (loadavg, rq)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(drivers)  ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Driver/service management\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(hardware) ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Hardware info (lapic, ioapic, cpuinfo)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(io)       ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Low-level I/O (read, write, poke)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(dump)     ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("How addresses work (presets, symbols, hex)\n");
    conn.write_str("    ");
    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("help(symbols)  ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Symbol lookup and disassembly\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("  Quick Reference:\n");
    conn.write_str(vtparser::sgr_reset());
    for cmd in COMMANDS {
        conn.write_str("  ");
        conn.write_str(vtparser::sgr_fg_cyan());
        conn.write_str(cmd.name);
        conn.write_str(vtparser::sgr_reset());
        conn.write_str("  ");
        conn.write_str(vtparser::sgr_dim());
        conn.write_str(cmd.help);
        conn.write_str(vtparser::sgr_reset());
        conn.write_str("\n");
    }
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_dim());
    conn.write_str("  ── Address tip: type dump(heap), peek(apic), dump(kmain), or dump(0xFFFFFFFF81000000)\n");
    conn.write_str("  ── Safety: write, poke, drv_call, reboot ask for confirmation first\n");
    conn.write_str(vtparser::sgr_reset());
}

fn help_memory(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Memory Inspection ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  dump(addr, len)      ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Hex dump kernel memory.\n");
    conn.write_str("                       addr = heap, apic, ioapic, code, or symbol/hex\n");
    conn.write_str("                       len  = number of bytes (default 64, max 512)\n");
    conn.write_str("                       Example: dump(heap, 128)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  peek(addr)           ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Read 64-bit value from memory.\n");
    conn.write_str("                       Same address resolution as dump.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  poke(addr, val)      ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Write 64-bit to physical memory.\n");
    conn.write_str("                       Requires [y/N] confirmation.\n");
    conn.write_str("                       addr is physical address (not virtual).\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  meminfo()            ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show physical memory stats (total/used/free pages, MB).\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  translate(virt_addr) ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show physical address for a virtual address.\n");
    conn.write_str("                       Example: translate(0xFFFF800000100000)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  pte(virt_addr)       ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show page table entry with decoded flags (P, W, U, NX, etc).\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  vmas()               ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List all kernel virtual memory areas.\n");
}

fn help_tasks(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Task / Process Inspection ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  ps()                 ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List all gangs (processes):\n");
    conn.write_str("                       Shows PID, name, state, vruntime, vCPU count.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  trace(vcpu_id)       ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Full register dump of a vCPU:\n");
    conn.write_str("                       Shows RAX-R15, RIP, RFLAGS, CS, SS, RSP, ERR, VECTOR.\n");
    conn.write_str("                       Example: trace(0), trace(1)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  vcpus()              ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List all allocated vCPUs:\n");
    conn.write_str("                       Shows ID, type, state, gang, saved RIP, saved RSP.\n");
}

fn help_sched(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Scheduler ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  loadavg()            ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Per-CPU task counts and load information.\n");
    conn.write_str("                       Shows APIC ID, online status, task count, ticks.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  rq(cpu)              ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Peek at a CPU's ready queue.\n");
    conn.write_str("                       Shows the next vCPU ID in queue + total task count.\n");
    conn.write_str("                       Example: rq(0)\n");
}

fn help_drivers(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Drivers & Services ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  drivers()            ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List registered GDF driver packages.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  services()           ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List running services with state, vCPU, restart count.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  drv_call(name, cmd, args...)\n");
    conn.write_str("                       ");
    conn.write_str("Send a command to a driver (requires confirm).\n");
    conn.write_str("                       Example: drv_call(test_driver, 1, 0x1000, 0, 0)\n");
}

fn help_hardware(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Hardware ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  cpuinfo()            ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List all online CPUs with APIC ID and task count.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  lapic()              ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show LAPIC registers:\n");
    conn.write_str("                       Base address, ID, TPR, SVR, LVT timer, ISR/IRR.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  ioapic_dump(index)   ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show IOAPIC redirection entries:\n");
    conn.write_str("                       Each pin: vector, destination APIC, trigger mode.\n");
    conn.write_str("                       Example: ioapic_dump(0)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  irq()                ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show IOAPIC interrupt routing table (ISA→GSI→vector).\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  ticks()              ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Show LAPIC timer and PIT tick counts.\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  dumpcpu(cpu)         ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Dump state for a specific CPU.\n");
    conn.write_str("                       Example: dumpcpu(CPU0)\n");
}

fn help_io(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Low-Level I/O ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  read(port)           ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Read a byte from an I/O port.\n");
    conn.write_str("                       Example: read(0x60)  — PS/2 data port\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  write(port, val)     ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Write a byte to an I/O port (requires confirm).\n");
    conn.write_str("                       Example: write(0x64, 0xFE) — reboot\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  poke(addr, val)      ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Write 64-bit to physical memory (requires confirm).\n");
    conn.write_str("                       addr is physical (0x...), not virtual.\n");
}

fn help_dump(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Address Resolution ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str("  You can use any of these as the address argument:\n");
    conn.write_str("\n");
    conn.write_str("  1) ");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("Named presets");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" — easy names for important locations:\n");
    for p in PRESETS {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("       {:14} = 0x{:016X}\n", p.alias, p.addr));
    }
    conn.write_str("\n");
    conn.write_str("  2) ");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("Symbol names");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" — any function or variable in the kernel:\n");
    conn.write_str("       Example: dump(kmain), peek(idt::ticks)\n");
    conn.write_str("\n");
    conn.write_str("  3) ");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("Raw hex");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" — any valid address:\n");
    conn.write_str("       Example: dump(0xFFFFFFFF80100000, 128)\n");
    conn.write_str("\n");
    conn.write_str("  dump and peek also accept phys_addr (for peek/poke).\n");
}

fn help_symbols(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("═══ Symbols & Disassembly ═══\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  symbols(filter)      ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("List kernel symbols with addresses.\n");
    conn.write_str("                       No filter = all symbols.\n");
    conn.write_str("                       Filter = show only matching names.\n");
    conn.write_str("                       Example: symbols(kmain), symbols(apic)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  lookup(addr)         ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Resolve an address to its symbol + file:line.\n");
    conn.write_str("                       Example: lookup(0xFFFFFFFF80100000)\n");
    conn.write_str("\n");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str("  disasm(addr, count)  ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("Disassemble x86-64 instructions.\n");
    conn.write_str("                       addr = preset, symbol, or hex.\n");
    conn.write_str("                       count = number of instructions (default 10).\n");
    conn.write_str("                       Example: disasm(code, 5)\n");
    conn.write_str("                       Example: disasm(0xFFFFFFFF80100000)\n");
}

fn cmd_clear(_args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::clear_screen());
}

fn cmd_dump(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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

    let ptr = addr as *const u8;
    let hex_chars = b"0123456789abcdef";

    for offset in (0..len as usize).step_by(16) {
        let row_len = (len as usize - offset).min(16);

        // Address column
        conn.write_str(vtparser::sgr_fg_green());
        {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("{:#018x}: ", addr.wrapping_add(offset as u64)));
        }
        conn.write_str(vtparser::sgr_reset());

        // Hex bytes
        let mut ascii_buf = [0u8; 16];
        for i in 0..row_len {
            let byte = unsafe { ptr.add(offset + i).read_volatile() };
            ascii_buf[i] = byte;
            let hi = hex_chars[(byte >> 4) as usize];
            let lo = hex_chars[(byte & 0xF) as usize];
            let pair = [hi, lo, b' '];
            conn.write_str(unsafe { core::str::from_utf8_unchecked(&pair) });
            if i == 7 {
                conn.write_str(" ");
            }
        }

        // Padding
        for i in row_len..16 {
            conn.write_str("   ");
            if i == 7 {
                conn.write_str(" ");
            }
        }

        // ASCII column
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

fn cmd_dumpcpu(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let cpu_str = match parser.parse_str() {
        Some(s) => s,
        None => "CPU0",
    };

    let cpu_slot = if cpu_str.starts_with("CPU") || cpu_str.starts_with("cpu") {
        let num_part = &cpu_str[3..];
        match num_part.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                conn.write_str(vtparser::sgr_fg_red());
                conn.write_str("error:");
                conn.write_str(vtparser::sgr_reset());
                conn.write_str(" invalid CPU, use e.g. CPU0\n");
                return;
            }
        }
    } else {
        conn.write_str(vtparser::sgr_fg_red());
        conn.write_str("error:");
        conn.write_str(vtparser::sgr_reset());
        conn.write_str(" expected e.g. CPU0\n");
        return;
    };

    if cpu_slot >= lodaxos_system::MAX_CPUS {
        conn.write_str(vtparser::sgr_fg_red());
        conn.write_str("error:");
        conn.write_str(vtparser::sgr_reset());
        conn.write_str(" CPU slot out of range\n");
        return;
    }

    let pcpu = &crate::percpu::PERCPU[cpu_slot];
    let apic_id = pcpu.apic_id.load(core::sync::atomic::Ordering::Relaxed);
    let online = pcpu.online.load(core::sync::atomic::Ordering::Relaxed);

    conn.write_str(vtparser::sgr_bold());
    conn.write_str("CPU");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", cpu_slot));
    }
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    conn.write_str("  APIC ID:     ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", apic_id));
    }
    conn.write_str("\n");

    conn.write_str("  Online:      ");
    conn.write_str(if online { "yes" } else { "no" });
    conn.write_str("\n");

    conn.write_str("  Task count:  ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", crate::percpu::task_count(cpu_slot)));
    }
    conn.write_str("\n");
}

fn cmd_peek(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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

    // peek reads from physical memory via HIGHER_HALF
    let hh = crate::mm::virt::HIGHER_HALF;
    let ptr = (hh + addr) as *const u64;
    let val = unsafe { ptr.read_volatile() };

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

fn cmd_poke(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let phys = match parser.parse_u64() {
        Some(a) => a,
        None => {
            conn.write_str("error: expected (phys_addr, value)\n");
            return;
        }
    };
    let val = match parser.parse_u64() {
        Some(v) => v,
        None => {
            conn.write_str("error: expected (phys_addr, value)\n");
            return;
        }
    };

    unsafe {
        *CONFIRM_POKE_PHYS.get() = phys;
        *CONFIRM_POKE_VAL.get() = val;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:016X} to physical address 0x{:016X}\n", val, phys));
    confirm_or_cancel(conn, "Write to physical memory?", confirm_poke);
}

fn cmd_read_port(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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

    let byte: u8 = unsafe { x86_64::instructions::port::Port::new(port).read() };

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

fn cmd_write_port(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let port = match parser.parse_u64() {
        Some(p) if p <= 0xFFFF => p as u16,
        _ => {
            conn.write_str("error: expected (port, value)\n");
            return;
        }
    };
    let val = match parser.parse_u64() {
        Some(v) if v <= 0xFF => v as u8,
        _ => {
            conn.write_str("error: expected (port, value)\n");
            return;
        }
    };

    unsafe {
        *CONFIRM_PORT.get() = port;
        *CONFIRM_PORT_VAL.get() = val;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write 0x{:02X} to I/O port 0x{:04X}\n", val, port));
    confirm_or_cancel(conn, "Write to I/O port?", confirm_write_port);
}

fn cmd_ticks(_args: &str, conn: &dyn Connector) {
    let ticks = crate::arch::idt::ticks();
    let pit = crate::arch::idt::pit_ticks();

    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("LAPIC timer ticks:");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", ticks));
    }
    conn.write_str("\n");

    conn.write_str(vtparser::sgr_fg_yellow());
    conn.write_str("PIT ticks:        ");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", pit));
    }
    conn.write_str("\n");
}

fn cmd_info(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let topic = match parser.parse_str() {
        Some(s) => s,
        None => {
            conn.write_str("topics: apic, ioapic, irq, memory, heap\n");
            return;
        }
    };

    match topic {
        "apic" => cmd_info_apic(conn),
        "ioapic" => cmd_info_ioapic(conn),
        "irq" => cmd_irq("", conn),
        "memory" | "mem" => cmd_info_memory(conn),
        "heap" => cmd_info_heap(conn),
        _ => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("error:");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str(" unknown topic '");
            conn.write_str(topic);
            conn.write_str("' (try: apic, ioapic, irq, memory, heap)\n");
        }
    }
}

fn cmd_info_apic(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("LAPIC");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    conn.write_str("  BSP APIC ID: ");
    {
        let bsp_id = crate::percpu::PERCPU[0].apic_id.load(core::sync::atomic::Ordering::Relaxed);
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", bsp_id));
    }
    conn.write_str("\n");

    conn.write_str("  Initialized: ");
    conn.write_str(if crate::arch::apic::is_initialized() { "yes" } else { "no" });
    conn.write_str("\n");
}

fn cmd_info_ioapic(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("IOAPIC");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    for i in 0..crate::arch::ioapic::MAX_IOAPICS {
        if let Some(ioapic) = crate::arch::ioapic::get(i) {
            conn.write_str("  IOAPIC[");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", i));
            }
            conn.write_str("]:\n");
            conn.write_str("    ID:       ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", ioapic.id));
            }
            conn.write_str("\n");
            conn.write_str("    Version:  ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", ioapic.version));
            }
            conn.write_str("\n");
            conn.write_str("    Max pins: ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", ioapic.max_redir));
            }
            conn.write_str("\n");
            conn.write_str("    GSI base: ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", ioapic.gsi_base));
            }
            conn.write_str("\n");
        }
    }
}

fn cmd_info_memory(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("Memory");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    conn.write_str("  Higher half: ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{:#018x}", crate::mm::virt::HIGHER_HALF));
    }
    conn.write_str("\n");
    conn.write_str("  Free pages:  ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", crate::mm::phys::free_pages_count()));
    }
    conn.write_str("\n");
    conn.write_str("  Total pages: ");
    {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{}", crate::mm::phys::total_pages()));
    }
    conn.write_str("\n");
}

fn cmd_info_heap(conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("Kernel Heap");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
    conn.write_str("  Slab allocator active (32B..8KB caches)\n");
    conn.write_str("  (detailed per-cache stats not yet exported)\n");
}

fn cmd_cpuinfo(_args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("Online CPUs");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    for slot in 0..lodaxos_system::MAX_CPUS {
        let pcpu = &crate::percpu::PERCPU[slot];
        let online = pcpu.online.load(core::sync::atomic::Ordering::Relaxed);
        if online {
            let apic_id = pcpu.apic_id.load(core::sync::atomic::Ordering::Relaxed);
            let tasks = crate::percpu::task_count(slot);
            conn.write_str("  CPU");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", slot));
            }
            conn.write_str(": APIC ID=");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", apic_id));
            }
            conn.write_str(", tasks=");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", tasks));
            }
            conn.write_str("\n");
        }
    }
}

fn cmd_irq(_args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("IRQ Routing Table");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");

    for isa in 0u8..16 {
        if let Some(route) = crate::intr::lookup_isa(isa) {
            conn.write_str("  ISA IRQ ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", isa));
            }
            conn.write_str(": GSI=");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", route.gsi));
            }
            conn.write_str(" IOAPIC[");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", route.ioapic_index));
            }
            conn.write_str("] pin ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", route.ioapic_pin));
            }
            conn.write_str(" vector ");
            {
                let mut w = vtparser::ConnectorWriter { conn };
                let _ = core::fmt::write(&mut w, format_args!("{}", route.vector));
            }
            conn.write_str("\n");
        }
    }
}

// ── New commands: Symbols ──────────────────────────────────────────

fn cmd_symbols(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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
        conn.write_str("(symbol table is empty — ~1900 entries expected)\n");
    } else {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("{} symbols shown\n", found));
    }
}

fn cmd_lookup(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("No symbol found for 0x{:016X} (address may need HIGHER_HALF offset)\n", addr));
        }
    }
}

fn cmd_disasm(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let addr_token = match parser.parse_str() {
        Some(t) => t,
        None => {
            conn.write_str("Usage: disasm(address, count)\n");
            conn.write_str("Disassembles x86-64 instructions at the given address.\n");
            conn.write_str("Examples:\n");
            conn.write_str("  disasm(code, 5)       — 5 instructions from kernel code start\n");
            conn.write_str("  disasm(0xFFFFFFFF81000000, 10)\n");
            conn.write_str("  disasm(kmain)          — default 10 instructions\n");
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

    // Show symbol context
    if let Some((name, offset, _, _)) = crate::arch::dump::resolve_kernel_symbol(addr) {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Disassembly of {}+0x{:X}:\n", name, offset));
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

// ── New commands: Memory ──────────────────────────────────────────

fn cmd_meminfo(_args: &str, conn: &dyn Connector) {
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

fn cmd_translate(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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
            let _ = core::fmt::write(&mut w, format_args!("Virtual: 0x{:016X} — NOT MAPPED\n", virt_addr));
        }
    }
}

fn cmd_pte(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
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
            let _ = core::fmt::write(&mut w, format_args!("PTE for 0x{:016X} — NOT PRESENT\n", virt_addr));
        }
    }
}

fn cmd_vmas(_args: &str, conn: &dyn Connector) {
    use crate::mm::vma;
    conn.write_str("Kernel VMA Regions:\n");
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("{:18} {:18} {:6}\n", "START", "END", "PERM"));
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
}

// ── New commands: Tasks ──────────────────────────────────────────

fn cmd_ps(_args: &str, conn: &dyn Connector) {
    if !crate::scheduler::is_initialized() {
        conn.write_str("Scheduler not initialized\n");
        return;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "{:>3}  {:24} {:10} {:>12} {:>5}\n{:>3}  {:24} {:10} {:>12} {:>5}\n",
        "PID", "NAME", "STATE", "VRUNTIME", "VCPU#",
        "---", "----", "-----", "-------", "-----"
    ));
    let table = crate::scheduler::GANG_TABLE.lock();
    for i in 0..32 {
        if let Some(ref gang) = table.gangs[i] {
            let name = core::str::from_utf8(&gang.name[..]).unwrap_or("?").trim_end_matches('\0');
            let state = match gang.state {
                crate::scheduler::GangState::Active => "Active",
                crate::scheduler::GangState::Halted => "Halted",
            };
            let _ = core::fmt::write(&mut w, format_args!(
                "{:>3}  {:24} {:10} {:>12} {:>5}\n",
                gang.id, name, state, gang.vruntime, gang.vcpu_count
            ));
        }
    }
}

fn cmd_trace(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => {
            conn.write_str("Usage: trace(vcpu_id)\n");
            conn.write_str("Full register dump of a vCPU's saved TrapFrame.\n");
            conn.write_str("Example: trace(0)\n");
            return;
        }
    };
    let Some(vcpu) = crate::vcpu::get(id) else {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("vCPU {} not found\n", id));
        return;
    };
    let f = &vcpu.saved_frame;
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "vCPU {} Register Dump:\n", id
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  RAX: {:016X}  RBX: {:016X}\n", f.rax, f.rbx
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  RCX: {:016X}  RDX: {:016X}\n", f.rcx, f.rdx
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  RSI: {:016X}  RDI: {:016X}\n", f.rsi, f.rdi
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  RBP: {:016X}  RSP: {:016X}\n", f.rbp, f.rsp
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  R8:  {:016X}  R9:  {:016X}\n", f.r8, f.r9
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  R10: {:016X}  R11: {:016X}\n", f.r10, f.r11
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  R12: {:016X}  R13: {:016X}\n", f.r12, f.r13
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  R14: {:016X}  R15: {:016X}\n", f.r14, f.r15
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  RIP: {:016X}  RFLAGS: {:016X}\n", f.rip, f.rflags
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  CS:  {:016X}  SS:    {:016X}\n", f.cs, f.ss
    ));
    let _ = core::fmt::write(&mut w, format_args!(
        "  ERR: {:016X}  VECTOR:{:016X}\n", f.error_code, f.vector
    ));
    let type_name = match vcpu.vcpu_type {
        crate::vcpu::VcpuType::Normal => "Normal",
        crate::vcpu::VcpuType::HardwareDriver => "HwDriver",
        crate::vcpu::VcpuType::AbstractionDriver => "AbstDrv",
        crate::vcpu::VcpuType::Idle => "Idle",
    };
    let state_name = match vcpu.state {
        crate::vcpu::VcpuState::Ready => "Ready",
        crate::vcpu::VcpuState::Running => "Running",
        crate::vcpu::VcpuState::Halted => "Halted",
        crate::vcpu::VcpuState::Blocked => "Blocked",
        crate::vcpu::VcpuState::Idle => "Idle",
    };
    let _ = core::fmt::write(&mut w, format_args!(
        "  Gang: {}  Type: {}  State: {}\n", vcpu.gang_id, type_name, state_name
    ));
}

fn cmd_vcpus(_args: &str, conn: &dyn Connector) {
    let count = crate::vcpu::count();
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("vCPUs allocated: {}\n", count));
    let _ = core::fmt::write(&mut w, format_args!(
        "{:>3} {:10} {:10} {:>8} {:>18} {:>18}\n{:>3} {:10} {:10} {:>8} {:>18} {:>18}\n",
        "ID", "TYPE", "STATE", "GANG", "SAVED RIP", "SAVED RSP",
        "--", "----", "-----", "----", "----------", "----------"
    ));
    for id in 0..count as u32 {
        if let Some(vcpu) = crate::vcpu::get(id) {
            let type_name = match vcpu.vcpu_type {
                crate::vcpu::VcpuType::Normal => "Normal",
                crate::vcpu::VcpuType::HardwareDriver => "HwDrv",
                crate::vcpu::VcpuType::AbstractionDriver => "AbstDrv",
                crate::vcpu::VcpuType::Idle => "Idle",
            };
            let state_name = match vcpu.state {
                crate::vcpu::VcpuState::Ready => "Ready",
                crate::vcpu::VcpuState::Running => "Running",
                crate::vcpu::VcpuState::Halted => "Halted",
                crate::vcpu::VcpuState::Blocked => "Blocked",
                crate::vcpu::VcpuState::Idle => "Idle",
            };
            let _ = core::fmt::write(&mut w, format_args!(
                "{:>3} {:10} {:10} {:>8} 0x{:016X} 0x{:016X}\n",
                vcpu.id, type_name, state_name, vcpu.gang_id,
                vcpu.saved_frame.rip, vcpu.saved_frame.rsp
            ));
        }
    }
}

// ── New commands: Scheduler ──────────────────────────────────────

fn cmd_loadavg(_args: &str, conn: &dyn Connector) {
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "{:>3} {:>7} {:>6} {:>5}\n{:>3} {:>7} {:>6} {:>5}\n",
        "CPU", "APIC_ID", "ONLINE", "TASKS",
        "---", "-------", "------", "-----"
    ));
    for cpu in 0..lodaxos_system::MAX_CPUS {
        let p = &crate::percpu::PERCPU[cpu];
        let online = p.online.load(core::sync::atomic::Ordering::Relaxed);
        let tasks = p.task_count.load(core::sync::atomic::Ordering::Relaxed);
        let apic_id = p.apic_id.load(core::sync::atomic::Ordering::Relaxed);
        let _ = core::fmt::write(&mut w, format_args!(
            "{:>3} {:>7} {:>6} {:>5}\n",
            cpu, apic_id,
            if online { "yes" } else { "no" },
            tasks
        ));
    }
}

fn cmd_rq(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let cpu = match parser.parse_u64() {
        Some(c) if c < lodaxos_system::MAX_CPUS as u64 => c as usize,
        _ => {
            conn.write_str("Usage: rq(cpu_id)\n");
            conn.write_str("Peek at a CPU's ready queue.\n");
            conn.write_str("Example: rq(0)\n");
            return;
        }
    };
    let queue = crate::percpu::rq(cpu);
    let mut w = vtparser::ConnectorWriter { conn };
    match queue.peek() {
        Some(next_id) => {
            let _ = core::fmt::write(&mut w, format_args!("CPU {} ready queue: next vCPU = {}\n", cpu, next_id));
        }
        None => {
            let _ = core::fmt::write(&mut w, format_args!("CPU {} ready queue: empty\n", cpu));
        }
    }
    let tasks = crate::percpu::PERCPU[cpu].task_count.load(core::sync::atomic::Ordering::Relaxed);
    let _ = core::fmt::write(&mut w, format_args!("CPU {} total tasks: {}\n", cpu, tasks));
}

// ── New commands: Drivers ────────────────────────────────────────

fn cmd_drivers(_args: &str, conn: &dyn Connector) {
    let mut found = false;
    let mut w = vtparser::ConnectorWriter { conn };
    for i in 0..16 {
        if let Some(name_arr) = crate::gdf::pkg_name(i) {
            found = true;
            let name = core::str::from_utf8(&name_arr[..]).unwrap_or("?").trim_end_matches('\0');
            // pkg.class is private — access via gdf::pkg_class()
            let class = crate::gdf::pkg_class(i).unwrap_or("?");
            let _ = core::fmt::write(&mut w, format_args!("  [{}] {} (class={})\n", i, name, class));
        }
    }
    if !found {
        conn.write_str("No GDF drivers registered\n");
    }
}

fn cmd_services(_args: &str, conn: &dyn Connector) {
    let count = crate::service::count();
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("Services: {}\n", count));
    let _ = core::fmt::write(&mut w, format_args!(
        "{:>3} {:24} {:10} {:>6} {:>8}\n{:>3} {:24} {:10} {:>6} {:>8}\n",
        "ID", "NAME", "STATE", "VCPU", "RESTARTS",
        "---", "----", "-----", "----", "--------"
    ));
    for id in 0..count as u32 {
        if let Some(svc) = crate::service::get(id) {
            let name = core::str::from_utf8(&svc.name[..]).unwrap_or("?").trim_end_matches('\0');
            let state = match svc.state {
                crate::service::ServiceState::Loaded => "Loaded",
                crate::service::ServiceState::Running => "Running",
                crate::service::ServiceState::Crashed => "Crashed",
                crate::service::ServiceState::Restarting => "Restart",
                crate::service::ServiceState::Stopped => "Stopped",
            };
            let _ = core::fmt::write(&mut w, format_args!(
                "{:>3} {:24} {:10} {:>6} {:>8}\n",
                svc.id, name, state, svc.vcpu_id, svc.restart_count
            ));
        }
    }
}

fn cmd_drv_call(args: &str, conn: &dyn Connector) {
    let mut parser = super::termexec::Args::new(args);
    let name = match parser.parse_str() {
        Some(n) => n,
        None => {
            conn.write_str("Usage: drv_call(name, cmd, arg0, arg1, arg2)\n");
            conn.write_str("Sends a command to a GDF driver.\n");
            conn.write_str("Example: drv_call(test_driver, 1, 0x1000, 0, 0)\n");
            conn.write_str("Requires [y/N] confirmation.\n");
            return;
        }
    };
    // Validate driver exists
    let exists = crate::gdf::find_by_name(name.as_bytes()).is_some();
    if !exists {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Driver '{}' not found\n", name));
        return;
    }

    let cmd = parser.parse_u64().unwrap_or(0) as u32;
    let arg0 = parser.parse_u64().unwrap_or(0);
    let arg1 = parser.parse_u64().unwrap_or(0);
    let arg2 = parser.parse_u64().unwrap_or(0);

    let driver_index = crate::gdf::find_by_name(name.as_bytes()).unwrap_or(0);

    // Store args in statics for the callback
    unsafe {
        *CONFIRM_DRV_INDEX.get() = driver_index;
        *CONFIRM_DRV_CMD.get() = cmd;
        *CONFIRM_DRV_ARGS.get() = (arg0, arg1, arg2);
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: About to send cmd={} to driver '{}' (arg0=0x{:X}, arg1=0x{:X}, arg2=0x{:X})\n",
        cmd, name, arg0, arg1, arg2
    ));
    confirm_or_cancel(conn, "Send command to driver?", confirm_drv_call);
}

// Confirmation callback storage
static CONFIRM_DRV_INDEX: crate::sync::SyncUnsafeCell<usize> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_DRV_CMD: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_DRV_ARGS: crate::sync::SyncUnsafeCell<(u64, u64, u64)> = crate::sync::SyncUnsafeCell::new((0, 0, 0));
static CONFIRM_POKE_PHYS: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_POKE_VAL: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_PORT: crate::sync::SyncUnsafeCell<u16> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_PORT_VAL: crate::sync::SyncUnsafeCell<u8> = crate::sync::SyncUnsafeCell::new(0);

fn confirm_drv_call(yes: bool) {
    let conn = super::connector::get_active().unwrap();
    if yes {
        let idx = unsafe { *CONFIRM_DRV_INDEX.get() };
        let cmd = unsafe { *CONFIRM_DRV_CMD.get() };
        let (arg0, arg1, arg2) = unsafe { *CONFIRM_DRV_ARGS.get() };

        // Look up the driver name from PKG_META
        let name_arr = crate::gdf::pkg_name(idx).unwrap_or([0u8; 32]);
        let name = core::str::from_utf8(&name_arr[..]).unwrap_or("?").trim_end_matches('\0');
        let ok = crate::gdf::send_cmd(name.as_bytes(), cmd, arg0, arg1, arg2);

        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "Sent cmd={} to '{}': {}\n", cmd, name, if ok { "ok" } else { "FAILED" }
        ));
    } else {
        conn.write_str("Cancelled\n");
    }
}

fn confirm_poke(yes: bool) {
    let conn = super::connector::get_active().unwrap();
    if yes {
        let hh = crate::mm::virt::HIGHER_HALF;
        let phys = unsafe { *CONFIRM_POKE_PHYS.get() };
        let val = unsafe { *CONFIRM_POKE_VAL.get() };
        let ptr = (hh + phys) as *mut u64;
        unsafe { ptr.write_volatile(val); }
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Wrote 0x{:016X} to 0x{:016X}\n", val, phys));
    } else {
        conn.write_str("Cancelled\n");
    }
}

fn confirm_write_port(yes: bool) {
    let conn = super::connector::get_active().unwrap();
    if yes {
        let port = unsafe { *CONFIRM_PORT.get() };
        let val = unsafe { *CONFIRM_PORT_VAL.get() };
        unsafe { x86_64::instructions::port::Port::new(port).write(val); }
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("Wrote 0x{:02X} to port 0x{:04X}\n", val, port));
    } else {
        conn.write_str("Cancelled\n");
    }
}

fn confirm_reboot(_yes: bool) {
    let conn = super::connector::get_active().unwrap();
    conn.write_str("Rebooting...\n");
    // PS/2 keyboard controller CPU reset
    unsafe { x86_64::instructions::port::Port::<u8>::new(0x64).write(0xFE); }
    // CLI;HLT as fallback
    loop { unsafe { asm!("cli; hlt") }; }
}

// ── New commands: Hardware ───────────────────────────────────────

fn cmd_lapic(_args: &str, conn: &dyn Connector) {
    if !crate::arch::apic::is_initialized() {
        conn.write_str("LAPIC not initialized\n");
        return;
    }
    let id = crate::arch::apic::read_lapic_id();
    let base = crate::arch::apic::read_apic_base();
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "LAPIC Registers:\n  Base:     0x{:016X}\n  ID:       {}\n",
        base, id
    ));
    unsafe {
        let tpr = crate::arch::apic::read32(0x80);
        let lvt_timer = crate::arch::apic::read32(0x320);
        let cpr = crate::arch::apic::read32(0x390);
        let dfr = crate::arch::apic::read32(0x0E0);
        let svr = crate::arch::apic::read32(0x0F0);
        let _ = core::fmt::write(&mut w, format_args!( 
            "  TPR:      0x{:08X}\n  LVT Timer:0x{:08X}\n  CPR:      0x{:08X}\n  DFR:      0x{:08X}\n  SVR:      0x{:08X}\n",
            tpr, lvt_timer, cpr, dfr, svr
        ));

        // ISR/IRR banks
        let _ = write!(w, "  ISR:");
        let mut any = false;
        for bank in 0..8 {
            let reg = crate::arch::apic::read32(0x100 + bank * 0x10);
            if reg != 0 {
                for bit in 0..32 { if reg & (1 << bit) != 0 { let _ = write!(w, " {}", bank * 32 + bit); any = true; } }
            }
        }
        if !any { let _ = write!(w, " (none)"); }
        let _ = write!(w, "\n");

        let _ = write!(w, "  IRR:");
        let mut any = false;
        for bank in 0..8 {
            let reg = crate::arch::apic::read32(0x200 + bank * 0x10);
            if reg != 0 {
                for bit in 0..32 { if reg & (1 << bit) != 0 { let _ = write!(w, " {}", bank * 32 + bit); any = true; } }
            }
        }
        if !any { let _ = write!(w, " (none)"); }
        let _ = write!(w, "\n");
    }
}

fn cmd_ioapic_dump(args: &str, conn: &dyn Connector) {
    if !crate::arch::ioapic::is_initialized() {
        conn.write_str("IOAPIC not initialized\n");
        return;
    }
    let mut parser = super::termexec::Args::new(args);
    let index = parser.parse_u64().unwrap_or(0) as usize;
    match crate::arch::ioapic::get(index) {
        Some(ioapic) => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!(
                "IOAPIC[{}] id={} version={} max_redirect={} gsi_base={}\n",
                index, ioapic.id, ioapic.version, ioapic.max_redir, ioapic.gsi_base
            ));
            let _ = write!(w, "  Pin  Vector  DestAPIC  Mode       Status\n");
            for pin in 0..ioapic.max_redir {
                let (low, high) = ioapic.get_entry(pin);
                let vector = (low & 0xFF) as u8;
                let masked = (low & 0x0001_0000) != 0;
                let level = (low & 0x0000_8000) != 0;
                let active_low = (low & 0x0000_2000) != 0;
                let dest_apic = ((high >> 24) & 0xFF) as u8;
                let mode = if level {
                    if active_low { "level-low " } else { "level-high" }
                } else {
                    if active_low { "edge-low  " } else { "edge-high " }
                };
                let _ = core::fmt::write(&mut w, format_args!(
                    "  {:>3}  {:>6} {:>8}  {}  {}\n",
                    pin, vector, dest_apic, mode,
                    if masked { "MASKED" } else { "enabled" }
                ));
            }
        }
        None => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("IOAPIC index {} not found (max: {})\n", index, crate::arch::ioapic::count()));
        }
    }
}

fn cmd_reboot(_args: &str, conn: &dyn Connector) {
    conn.write_str("WARNING: This will reboot the entire system!\n");
    confirm_or_cancel(conn, "Reboot?", confirm_reboot);
}

// ── Echo ─────────────────────────────────────────────────────────

fn cmd_echo(args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str(args.trim());
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
}
