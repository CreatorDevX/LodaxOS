mod cmd_cpu;
mod cmd_debug;
mod cmd_drivers;
mod cmd_hardware;
mod cmd_memory;
mod cmd_symbols;
mod cmd_tasks;
mod help;

pub(crate) use cmd_debug::STEP_VCPU;

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use super::connector::Connector;
use super::vtparser;
use crate::sync::SyncUnsafeCell;

/// Set by the kernel during boot to the physical address of the
/// linear framebuffer (as reported by UEFI GOP).  Katerm presets
/// resolve "fb" / "framebuffer" from this value.
pub static KATERM_FB_PHYS: AtomicU64 = AtomicU64::new(0);

/// Software breakpoints (INT3 at address).
const MAX_BPS: usize = 8;
pub(crate) struct BreakpointEntry {
    pub(crate) addr: AtomicU64,
    pub(crate) original_byte: AtomicU8,
    pub(crate) enabled: AtomicBool,
}
const BP_NONE: BreakpointEntry = BreakpointEntry {
    addr: AtomicU64::new(0),
    original_byte: AtomicU8::new(0),
    enabled: AtomicBool::new(false),
};
pub(crate) static BREAKPOINTS: [BreakpointEntry; MAX_BPS] = [BP_NONE; MAX_BPS];

/// Communicate breakpoint/single-step hits from #BP/#DB handler to katerm.
pub static BP_HIT_VCPU: AtomicI32 = AtomicI32::new(-1);
pub static BP_HIT_RIP: AtomicU64 = AtomicU64::new(0);
pub static BP_HIT_VECTOR: AtomicU8 = AtomicU8::new(0);

pub(crate) fn find_bp(addr: u64) -> Option<usize> {
    for (i, bp) in BREAKPOINTS.iter().enumerate() {
        let a = bp.addr.load(Ordering::SeqCst);
        if a == addr || a == 0 {
            return Some(i);
        }
    }
    None
}

pub(crate) fn set_register(vcpu: &mut crate::vcpu::Vcpu, name: &str, val: u64) -> Result<u64, &'static str> {
    let old = match name.to_ascii_lowercase().as_str() {
        "rax" => { let o = vcpu.saved_frame.rax; vcpu.saved_frame.rax = val; o }
        "rbx" => { let o = vcpu.saved_frame.rbx; vcpu.saved_frame.rbx = val; o }
        "rcx" => { let o = vcpu.saved_frame.rcx; vcpu.saved_frame.rcx = val; o }
        "rdx" => { let o = vcpu.saved_frame.rdx; vcpu.saved_frame.rdx = val; o }
        "rsi" => { let o = vcpu.saved_frame.rsi; vcpu.saved_frame.rsi = val; o }
        "rdi" => { let o = vcpu.saved_frame.rdi; vcpu.saved_frame.rdi = val; o }
        "rbp" => { let o = vcpu.saved_frame.rbp; vcpu.saved_frame.rbp = val; o }
        "rsp" => { let o = vcpu.saved_frame.rsp; vcpu.saved_frame.rsp = val; o }
        "r8"  => { let o = vcpu.saved_frame.r8;  vcpu.saved_frame.r8  = val; o }
        "r9"  => { let o = vcpu.saved_frame.r9;  vcpu.saved_frame.r9  = val; o }
        "r10" => { let o = vcpu.saved_frame.r10; vcpu.saved_frame.r10 = val; o }
        "r11" => { let o = vcpu.saved_frame.r11; vcpu.saved_frame.r11 = val; o }
        "r12" => { let o = vcpu.saved_frame.r12; vcpu.saved_frame.r12 = val; o }
        "r13" => { let o = vcpu.saved_frame.r13; vcpu.saved_frame.r13 = val; o }
        "r14" => { let o = vcpu.saved_frame.r14; vcpu.saved_frame.r14 = val; o }
        "r15" => { let o = vcpu.saved_frame.r15; vcpu.saved_frame.r15 = val; o }
        "rip" => { let o = vcpu.saved_frame.rip; vcpu.saved_frame.rip = val; o }
        "cs"  => { let o = vcpu.saved_frame.cs;  vcpu.saved_frame.cs  = val; o }
        "rflags" | "flags" | "eflags" => { let o = vcpu.saved_frame.rflags; vcpu.saved_frame.rflags = val; o }
        "ss"  => { let o = vcpu.saved_frame.ss;  vcpu.saved_frame.ss  = val; o }
        _ => return Err("unknown register"),
    };
    Ok(old)
}

/// Virtual address of the framebuffer (HIGHER_HALF + fb_phys).
pub static KATERM_FB_VIRT: AtomicU64 = AtomicU64::new(0);

// -- Mode system -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Root,
    Mem,
    Tasks,
    Sched,
    Drv,
    Hw,
    Io,
    Sym,
    Dbg,
    Cpu,
    Diag,
}

static CURRENT_MODE: SyncUnsafeCell<Mode> = SyncUnsafeCell::new(Mode::Root);

pub fn current_mode() -> Mode {
    unsafe { *CURRENT_MODE.get() }
}

pub fn set_mode(mode: Mode) {
    unsafe { *CURRENT_MODE.get() = mode; }
}

pub fn prompt_for_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Root => "KERNEL$ ",
        Mode::Mem => "KERNEL/mem$ ",
        Mode::Tasks => "KERNEL/tasks$ ",
        Mode::Sched => "KERNEL/sched$ ",
        Mode::Drv => "KERNEL/drv$ ",
        Mode::Hw => "KERNEL/hw$ ",
        Mode::Io => "KERNEL/io$ ",
        Mode::Sym => "KERNEL/sym$ ",
        Mode::Dbg => "KERNEL/dbg$ ",
        Mode::Cpu => "KERNEL/cpu$ ",
        Mode::Diag => "KERNEL/diag$ ",
    }
}

pub struct ModeCommands {
    pub mode: Mode,
    pub name: &'static str,
    pub commands: &'static [&'static str],
}

pub static MODE_COMMANDS: &[ModeCommands] = &[
    ModeCommands { mode: Mode::Mem, name: "mem", commands: &[
        "dump", "peek", "poke", "meminfo", "translate", "pte", "vmas", "pagestat",
    ]},
    ModeCommands { mode: Mode::Tasks, name: "tasks", commands: &[
        "ps", "trace", "vcpus", "slabstat",
    ]},
    ModeCommands { mode: Mode::Sched, name: "sched", commands: &[
        "loadavg", "rq",
    ]},
    ModeCommands { mode: Mode::Drv, name: "drv", commands: &[
        "drivers", "services", "drv_call",
    ]},
    ModeCommands { mode: Mode::Hw, name: "hw", commands: &[
        "cpuinfo", "lapic", "ioapic_dump", "irq", "ticks", "dumpcpu", "dumpremote", "dumpall", "irqstat",
    ]},
    ModeCommands { mode: Mode::Io, name: "io", commands: &[
        "read", "write", "read16", "read32", "write16", "write32", "poke",
    ]},
    ModeCommands { mode: Mode::Sym, name: "sym", commands: &[
        "symbols", "lookup", "disasm",
    ]},
    ModeCommands { mode: Mode::Dbg, name: "dbg", commands: &[
        "set", "bt", "stack", "break", "del", "bpl", "cont", "step", "watch",
        "cli", "sti", "rdmsr", "wrmsr", "invlpg",
    ]},
    ModeCommands { mode: Mode::Cpu, name: "cpu", commands: &[
        "poke_code", "load_code", "exec_page", "jump", "force_next", "recover", "map", "hlt",
    ]},
    ModeCommands { mode: Mode::Diag, name: "diag", commands: &[
        "pagestat", "slabstat", "irqstat", "map", "hlt",
    ]},
];

const ALWAYS_AVAILABLE: &[&str] = &["cm", "help", "listmodes", "clear", "echo", "reboot"];

pub fn is_command_available(name: &str, mode: Mode) -> bool {
    if mode == Mode::Root {
        return true;
    }
    for &cmd in ALWAYS_AVAILABLE {
        if cmd == name {
            return true;
        }
    }
    for mc in MODE_COMMANDS {
        if mc.mode == mode {
            for &cmd in mc.commands {
                if cmd == name {
                    return true;
                }
            }
            return false;
        }
    }
    false
}

pub(crate) struct FmtBuffer {
    buf: [u8; 256],
    pos: usize,
}

impl FmtBuffer {
    pub(crate) fn new() -> Self { Self { buf: [0u8; 256], pos: 0 } }
    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.pos]).unwrap_or("")
    }
    pub(crate) fn clear(&mut self) { self.pos = 0; }
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

pub(crate) fn read_memory_bytes(virt: u64, buf: &mut [u8]) -> usize {
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

pub(crate) fn resolve_arg(arg: &str) -> Result<u64, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("empty address");
    }
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_err(|_| "invalid hex address");
    }
    if let Ok(val) = u64::from_str_radix(trimmed, 10) {
        return Ok(val);
    }
    for p in PRESETS {
        if trimmed.eq_ignore_ascii_case(p.alias) || trimmed.eq_ignore_ascii_case(p.name) {
            return Ok(p.addr);
        }
    }
    if trimmed.eq_ignore_ascii_case("fb_phys") || trimmed.eq_ignore_ascii_case("fb_physical") {
        let addr = KATERM_FB_PHYS.load(Ordering::Relaxed);
        if addr != 0 { return Ok(addr); }
        return Err("framebuffer not initialized");
    }
    if trimmed.eq_ignore_ascii_case("fb") || trimmed.eq_ignore_ascii_case("framebuffer") || trimmed.eq_ignore_ascii_case("fb_virt") {
        let addr = KATERM_FB_VIRT.load(Ordering::Relaxed);
        if addr != 0 { return Ok(addr); }
        return Err("framebuffer not initialized");
    }
    let half = crate::mm::virt::HIGHER_HALF;
    for sym in crate::arch::symtab::SYMBOLS {
        if sym.name.eq_ignore_ascii_case(trimmed) || sym.name.contains(trimmed) {
            return Ok(sym.addr + half);
        }
    }
    Err("unknown address -- try 0x..., a named preset (heap, fb, apic), or a symbol name")
}

pub(crate) fn print_presets(conn: &dyn Connector) {
    conn.write_str("Named address presets:\n");
    for p in PRESETS {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("  {:14} -> 0x{:016X}  ({})\n", p.alias, p.addr, p.name));
    }
    let fb_virt = KATERM_FB_VIRT.load(Ordering::Relaxed);
    if fb_virt != 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("  {:14} -> 0x{:016X}  (framebuffer virt)\n", "fb", fb_virt));
    }
    let fb_phys = KATERM_FB_PHYS.load(Ordering::Relaxed);
    if fb_phys != 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("  {:14} -> 0x{:016X}  (framebuffer phys)\n", "fb_phys", fb_phys));
    }
    conn.write_str("  Or type a symbol name (e.g. 'kmain')\n");
    conn.write_str("  Or raw hex: 0x...\n");
}

pub(super) fn confirm_or_cancel(conn: &dyn Connector, msg: &str, callback: fn(bool)) {
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
    CmdDef { name: "cm",         signature: "(mode)",     help: "Switch command mode (cm mem, cm dbg, cm ..)", exec: cmd_cm },
    CmdDef { name: "listmodes",  signature: "()",         help: "List all available modes and their commands", exec: cmd_listmodes },
    CmdDef { name: "tui",        signature: "()",         help: "Enter TUI interactive inspector mode", exec: cmd_tui },
    CmdDef { name: "cli",        signature: "()",         help: "Exit TUI and return to CLI mode", exec: cmd_cli },
    CmdDef { name: "help",       signature: "(topic)",    help: "Show detailed help (try help memory, help tasks, help dump)", exec: cmd_help },
    CmdDef { name: "clear",      signature: "()",         help: "Clear terminal screen", exec: cmd_clear },
    CmdDef { name: "echo",       signature: "(msg)",      help: "Echo a message", exec: cmd_echo },
    CmdDef { name: "symbols",    signature: "(filter)",   help: "List kernel symbols (all or matching filter)", exec: cmd_symbols::cmd_symbols },
    CmdDef { name: "lookup",     signature: "(addr)",     help: "Resolve address to symbol name + file:line", exec: cmd_symbols::cmd_lookup },
    CmdDef { name: "disasm",     signature: "(addr, n)",  help: "Disassemble x86-64 instructions at address", exec: cmd_symbols::cmd_disasm },
    CmdDef { name: "dump",       signature: "(addr, len)",help: "Hex dump memory (presets: heap, apic; or symbol/hex)", exec: cmd_memory::cmd_dump },
    CmdDef { name: "peek",       signature: "(addr)",     help: "Read 64-bit from memory (presets or symbol/hex)", exec: cmd_memory::cmd_peek },
    CmdDef { name: "poke",       signature: "(addr, val)",help: "Write 64-bit to physical memory (requires confirm)", exec: cmd_memory::cmd_poke },
    CmdDef { name: "meminfo",    signature: "()",         help: "Physical memory stats (total/used/free)", exec: cmd_memory::cmd_meminfo },
    CmdDef { name: "translate",  signature: "(virt_addr)",help: "Translate virtual address to physical", exec: cmd_memory::cmd_translate },
    CmdDef { name: "pte",        signature: "(virt_addr)",help: "Show page table entry with decoded flags", exec: cmd_memory::cmd_pte },
    CmdDef { name: "vmas",       signature: "()",         help: "List kernel virtual memory areas", exec: cmd_memory::cmd_vmas },
    CmdDef { name: "ps",         signature: "()",         help: "List all gangs (processes)", exec: cmd_tasks::cmd_ps },
    CmdDef { name: "trace",      signature: "(vcpu_id)",  help: "Full register dump of a vCPU", exec: cmd_tasks::cmd_trace },
    CmdDef { name: "vcpus",      signature: "()",         help: "List all allocated vCPUs", exec: cmd_tasks::cmd_vcpus },
    CmdDef { name: "loadavg",    signature: "()",         help: "Per-CPU task counts and load", exec: cmd_tasks::cmd_loadavg },
    CmdDef { name: "rq",         signature: "(cpu)",      help: "Peek at a CPU's ready queue", exec: cmd_tasks::cmd_rq },
    CmdDef { name: "drivers",    signature: "()",         help: "List registered GDF drivers", exec: cmd_drivers::cmd_drivers },
    CmdDef { name: "services",   signature: "()",         help: "List running services", exec: cmd_drivers::cmd_services },
    CmdDef { name: "drv_call",   signature: "(name,cmd,args)", help: "Send command to a driver (requires confirm)", exec: cmd_drivers::cmd_drv_call },
    CmdDef { name: "cpuinfo",    signature: "()",         help: "List online CPUs", exec: cmd_hardware::cmd_cpuinfo },
    CmdDef { name: "lapic",      signature: "()",         help: "Show LAPIC registers (TPR, SVR, timer, ISR)", exec: cmd_hardware::cmd_lapic },
    CmdDef { name: "ioapic_dump",signature: "(index)",    help: "Dump IOAPIC redirection entries", exec: cmd_hardware::cmd_ioapic_dump },
    CmdDef { name: "irq",        signature: "()",         help: "Show IOAPIC interrupt routing table", exec: cmd_hardware::cmd_irq },
    CmdDef { name: "ticks",      signature: "()",         help: "Show timer tick counts", exec: cmd_hardware::cmd_ticks },
    CmdDef { name: "dumpcpu",    signature: "(cpu)",      help: "Dump CPU state (e.g. CPU0)", exec: cmd_hardware::cmd_dumpcpu },
    CmdDef { name: "dumpremote",signature: "(cpu)",      help: "Force full register dump on remote CPU via IPI (e.g. CPU1)", exec: cmd_hardware::cmd_dumpremote },
    CmdDef { name: "dumpall",   signature: "()",         help: "Dump ALL online CPUs via IPI (one by one)", exec: cmd_hardware::cmd_dumpall },
    CmdDef { name: "read",       signature: "(port)",     help: "Read byte from I/O port", exec: cmd_memory::cmd_read_port },
    CmdDef { name: "write",      signature: "(port,val)", help: "Write byte to I/O port (requires confirm)", exec: cmd_memory::cmd_write_port },
    CmdDef { name: "reboot",     signature: "()",         help: "Reboot the system (requires confirm)", exec: cmd_reboot },
    CmdDef { name: "set",        signature: "(vcpu,reg,val)", help: "Modify a saved register in a vCPU", exec: cmd_debug::cmd_set },
    CmdDef { name: "bt",         signature: "(vcpu_id)",      help: "Backtrace from vCPU's saved frame", exec: cmd_debug::cmd_bt },
    CmdDef { name: "stack",      signature: "(vcpu_id,n)",    help: "Hex dump n quadwords from vCPU's stack", exec: cmd_debug::cmd_stack },
    CmdDef { name: "read16",     signature: "(port)",         help: "Read 16-bit from I/O port", exec: cmd_debug::cmd_read16 },
    CmdDef { name: "read32",     signature: "(port)",         help: "Read 32-bit from I/O port", exec: cmd_debug::cmd_read32 },
    CmdDef { name: "write16",    signature: "(port,val)",     help: "Write 16-bit to I/O port (requires confirm)", exec: cmd_debug::cmd_write16 },
    CmdDef { name: "write32",    signature: "(port,val)",     help: "Write 32-bit to I/O port (requires confirm)", exec: cmd_debug::cmd_write32 },
    CmdDef { name: "cli",        signature: "()",             help: "Disable interrupts (RFLAGS.IF=0)", exec: cmd_debug::cmd_cli },
    CmdDef { name: "sti",        signature: "()",             help: "Enable interrupts (RFLAGS.IF=1)", exec: cmd_debug::cmd_sti },
    CmdDef { name: "rdmsr",      signature: "(msr)",          help: "Read Model-Specific Register", exec: cmd_debug::cmd_rdmsr },
    CmdDef { name: "wrmsr",      signature: "(msr,val)",      help: "Write Model-Specific Register (requires confirm)", exec: cmd_debug::cmd_wrmsr },
    CmdDef { name: "invlpg",     signature: "(addr)",         help: "Flush TLB entry for virtual address", exec: cmd_debug::cmd_invlpg },
    CmdDef { name: "break",      signature: "(addr)",         help: "Set software breakpoint at address", exec: cmd_debug::cmd_break },
    CmdDef { name: "del",        signature: "(index)",        help: "Delete breakpoint by index", exec: cmd_debug::cmd_del },
    CmdDef { name: "bpl",        signature: "()",             help: "List all breakpoints", exec: cmd_debug::cmd_bpl },
    CmdDef { name: "cont",       signature: "()",             help: "Continue vCPU after breakpoint hit", exec: cmd_debug::cmd_cont },
    CmdDef { name: "step",       signature: "(vcpu_id)",      help: "Single-step vCPU one instruction", exec: cmd_debug::cmd_step },
    CmdDef { name: "watch",      signature: "(addr)",         help: "Set hardware execution breakpoint", exec: cmd_debug::cmd_watch },
    CmdDef { name: "poke_code",  signature: "(addr, b0, b1, ...)", help: "Write raw machine code bytes to virtual address (requires confirm)", exec: cmd_cpu::cmd_poke_code },
    CmdDef { name: "load_code",  signature: "(vcpu, dst, src, len)", help: "Copy code bytes between addresses in vCPU space (requires confirm)", exec: cmd_cpu::cmd_load_code },
    CmdDef { name: "exec_page",  signature: "(vcpu, phys)",  help: "Map physical code page into vCPU at 0x400000 and set RIP (requires confirm)", exec: cmd_cpu::cmd_exec_page },
    CmdDef { name: "jump",       signature: "(vcpu, addr)",  help: "Set vCPU RIP to addr and resume execution (requires confirm)", exec: cmd_cpu::cmd_jump },
    CmdDef { name: "force_next", signature: "(cpu)",         help: "Force CPU to reschedule to next driver (requires confirm)", exec: cmd_cpu::cmd_force_next },
    CmdDef { name: "recover",    signature: "(cpu)",         help: "Recover CPU from hard fault / halt mode (requires confirm)", exec: cmd_cpu::cmd_recover },
    CmdDef { name: "pagestat",   signature: "()",            help: "Physical page allocator stats per order", exec: cmd_memory::cmd_pagestat },
    CmdDef { name: "slabstat",   signature: "()",            help: "Kernel heap slab allocator stats", exec: cmd_tasks::cmd_slabstat },
    CmdDef { name: "irqstat",    signature: "()",            help: "Per-vector interrupt/exception counts", exec: cmd_hardware::cmd_irqstat },
    CmdDef { name: "map",        signature: "(vcpu, addr)",  help: "Full page table walk for a vCPU's virtual address", exec: cmd_cpu::cmd_map },
    CmdDef { name: "hlt",        signature: "(vcpu)",        help: "Immediately halt a running vCPU (requires confirm)", exec: cmd_cpu::cmd_hlt },
    CmdDef { name: "freeze",     signature: "()",            help: "Broadcast NMI to freeze all other CPUs", exec: cmd_freeze },
];

fn cmd_help(args: &str, conn: &dyn Connector) {
    let topic = args.trim();
    if topic.is_empty() {
        help::help_main(conn);
        return;
    }
    match topic {
        "memory" | "mem" => help::help_memory(conn),
        "tasks" | "proc" | "ps" => help::help_tasks(conn),
        "sched" | "scheduler" => help::help_sched(conn),
        "drivers" | "drv" => help::help_drivers(conn),
        "hardware" | "hw" => help::help_hardware(conn),
        "io" => help::help_io(conn),
        "dump" | "peek" => help::help_dump(conn),
        "symbols" | "lookup" | "disasm" => help::help_symbols(conn),
        "debug" | "dbg" | "break" | "step" => help::help_debug(conn),
        "cpu" | "control" => help::help_cpu(conn),
        "diag" | "diagnostics" => help::help_diag(conn),
        _ => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("Unknown topic '");
            conn.write_str(topic);
            conn.write_str("'.\n");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str("Try: memory, tasks, sched, drivers, hardware, io, dump, symbols, cpu\n");
        }
    }
}

fn cmd_clear(_args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::clear_screen());
}

fn cmd_echo(args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str(args.trim());
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n");
}

fn cmd_reboot(_args: &str, conn: &dyn Connector) {
    conn.write_str("WARNING: This will reboot the entire system!\n");
    confirm_or_cancel(conn, "Reboot?", confirm_reboot);
}

fn confirm_reboot(_yes: bool) {
    let conn = super::connector::get_active().unwrap();
    conn.write_str("Rebooting...\n");
    unsafe { x86_64::instructions::port::Port::<u8>::new(0x64).write(0xFE); }
    loop { unsafe { core::arch::asm!("cli; hlt") }; }
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
        "irq" => cmd_hardware::cmd_irq("", conn),
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

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Root => "root",
        Mode::Mem => "mem",
        Mode::Tasks => "tasks",
        Mode::Sched => "sched",
        Mode::Drv => "drv",
        Mode::Hw => "hw",
        Mode::Io => "io",
        Mode::Sym => "sym",
        Mode::Dbg => "dbg",
        Mode::Cpu => "cpu",
        Mode::Diag => "diag",
    }
}

fn cmd_cm(args: &str, conn: &dyn Connector) {
    let arg = args.trim();
    if arg.is_empty() {
        let mode = current_mode();
        conn.write_str("Current mode: ");
        conn.write_str(vtparser::sgr_fg_green());
        conn.write_str(mode_name(mode));
        conn.write_str(vtparser::sgr_reset());
        conn.write_str("\n\nAvailable modes:\n");
        for mc in MODE_COMMANDS {
            conn.write_str("  ");
            conn.write_str(vtparser::sgr_fg_yellow());
            conn.write_str(mc.name);
            conn.write_str(vtparser::sgr_reset());
            conn.write_str("\n");
        }
        conn.write_str("\n  cm(mode)  - switch to mode\n");
        conn.write_str("  cm(..)    - return to root\n");
        return;
    }

    let new_mode = match arg {
        "root" | ".." => Mode::Root,
        "mem" => Mode::Mem,
        "tasks" | "ps" => Mode::Tasks,
        "sched" => Mode::Sched,
        "drv" | "drivers" => Mode::Drv,
        "hw" | "hardware" => Mode::Hw,
        "io" => Mode::Io,
        "symbols" | "sym" => Mode::Sym,
        "debug" | "dbg" => Mode::Dbg,
        "cpu" | "control" => Mode::Cpu,
        "diag" | "diagnostics" => Mode::Diag,
        _ => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("error:");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str(" unknown mode '");
            conn.write_str(arg);
            conn.write_str("'. Type cm() to see available modes.\n");
            return;
        }
    };

    set_mode(new_mode);
    if new_mode == Mode::Root {
        conn.write_str("Returned to root mode.\n");
    } else {
        conn.write_str("Now in ");
        conn.write_str(vtparser::sgr_fg_green());
        conn.write_str(mode_name(new_mode));
        conn.write_str(vtparser::sgr_reset());
        conn.write_str(" mode. Type cm(..) to return.\n");
    }
}

fn cmd_listmodes(_args: &str, conn: &dyn Connector) {
    let current = current_mode();
    conn.write_str(vtparser::sgr_bold());
    conn.write_str("Available modes");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("  (current: ");
    conn.write_str(vtparser::sgr_fg_green());
    conn.write_str(mode_name(current));
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(")\n\n");

    for mc in MODE_COMMANDS {
        let is_current = mc.mode == current;
        conn.write_str("  ");
        if is_current {
            conn.write_str(vtparser::sgr_bold());
        }
        conn.write_str(vtparser::sgr_fg_yellow());
        conn.write_str(mc.name);
        conn.write_str(vtparser::sgr_reset());
        if is_current {
            conn.write_str(vtparser::sgr_bold());
            conn.write_str(" *");
            conn.write_str(vtparser::sgr_reset());
        }
        conn.write_str("  ");
        conn.write_str(vtparser::sgr_dim());
        for (i, &cmd) in mc.commands.iter().enumerate() {
            if i > 0 {
                conn.write_str(", ");
            }
            conn.write_str(cmd);
        }
        conn.write_str(vtparser::sgr_reset());
        conn.write_str("\n");
    }

    conn.write_str("\n  Use cm(mode) to switch. cm(..) returns to root.\n");
    conn.write_str("  Commands marked * are available in current mode.\n");
}

fn cmd_tui(_args: &str, conn: &dyn Connector) {
    conn.write_str("Entering TUI mode...\n");
    conn.write_str("  Tab     - switch inspector tabs\n");
    conn.write_str("  Esc     - toggle output/inspector\n");
    conn.write_str("  F1-F4   - switch inspector tabs\n");
    conn.write_str("  Esc Esc - exit TUI back to CLI\n");
    super::tui::enter_tui();
}

fn cmd_cli(_args: &str, _conn: &dyn Connector) {
    super::tui::exit_tui();
}

fn cmd_freeze(_args: &str, conn: &dyn Connector) {
    use core::sync::atomic::Ordering;

    // Set the flag so APs enter a holding loop when they receive the NMI.
    crate::arch::dump::FREEZE_ALL_CPUS.store(true, Ordering::Release);

    // Brief delay to let the flag propagate.
    for _ in 0..10000 {
        core::hint::spin_loop();
    }

    // Broadcast NMI to all other CPUs.
    crate::arch::apic::send_nmi_all_excluding_self();

    // Brief delay to let NMIs fire.
    for _ in 0..100000 {
        core::hint::spin_loop();
    }

    conn.write_str("freeze: NMI sent to all other CPUs\n");
    conn.write_str("freeze: other CPUs are now in holding loop\n");
}
