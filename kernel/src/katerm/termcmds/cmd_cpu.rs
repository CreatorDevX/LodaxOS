use core::fmt::Write as _;
use core::sync::atomic::Ordering;
use super::super::connector::Connector;
use super::super::vtparser;
use super::confirm_or_cancel;

static CONFIRM_POKE_CODE_ADDR: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_POKE_CODE_LEN: crate::sync::SyncUnsafeCell<usize> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_POKE_CODE_BUF: crate::sync::SyncUnsafeCell<[u8; 15]> = crate::sync::SyncUnsafeCell::new([0u8; 15]);
static CONFIRM_LOAD_CODE_VCPU: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_LOAD_CODE_DST: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_LOAD_CODE_SRC: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_LOAD_CODE_LEN: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_EXEC_PAGE_VCPU: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_EXEC_PAGE_PHYS: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_JUMP_VCPU: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_JUMP_ADDR: crate::sync::SyncUnsafeCell<u64> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_FORCE_NEXT_CPU: crate::sync::SyncUnsafeCell<usize> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_RECOVER_CPU: crate::sync::SyncUnsafeCell<usize> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_HLT_VCPU: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);

pub(super) fn cmd_poke_code(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: poke_code(addr, byte0, byte1, ...)\n"); return; }
    };
    let mut buf = [0u8; 15];
    let mut len = 0usize;
    while len < 15 {
        match parser.parse_u64() {
            Some(b) if b <= 0xFF => { buf[len] = b as u8; len += 1; }
            Some(_) => { conn.write_str("error: bytes must be 0..255\n"); return; }
            None => break,
        }
    }
    if len == 0 {
        conn.write_str("Usage: poke_code(addr, byte0, byte1, ...)\n");
        return;
    }

    unsafe {
        *CONFIRM_POKE_CODE_ADDR.get() = addr;
        *CONFIRM_POKE_CODE_LEN.get() = len;
        (*CONFIRM_POKE_CODE_BUF.get()).copy_from_slice(&buf[..len]);
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("WARNING: About to write {} byte(s) of code to {:#018x}\n", len, addr));
    confirm_or_cancel(conn, "Write code to memory?", confirm_poke_code);
}

fn confirm_poke_code(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let addr = unsafe { *CONFIRM_POKE_CODE_ADDR.get() };
    let len = unsafe { *CONFIRM_POKE_CODE_LEN.get() };
    let buf = unsafe { *CONFIRM_POKE_CODE_BUF.get() };

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    let mut faulted_at = len;
    for i in 0..len {
        unsafe { core::ptr::write_volatile((addr + i as u64) as *mut u8, buf[i]); }
        if !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst) {
            faulted_at = i;
            break;
        }
    }
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

    let mut w = vtparser::ConnectorWriter { conn };
    if faulted_at < len {
        let _ = core::fmt::write(&mut w, format_args!("FAULT: write fault at byte {} ({:#018x})\n", faulted_at, addr + faulted_at as u64));
    } else {
        let _ = core::fmt::write(&mut w, format_args!("Wrote {} byte(s) of code to {:#018x}\n", len, addr));
    }
}

pub(super) fn cmd_load_code(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: load_code(vcpu_id, dst, src, len)\n"); return; }
    };
    let dst = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: load_code(vcpu_id, dst, src, len)\n"); return; }
    };
    let src = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: load_code(vcpu_id, dst, src, len)\n"); return; }
    };
    let len = match parser.parse_u64() {
        Some(l) if l > 0 && l <= 0x10000 => l,
        _ => { conn.write_str("Usage: load_code(vcpu_id, dst, src, len) [len: 1..65536]\n"); return; }
    };

    if dst >= crate::mm::virt::HIGHER_HALF || src >= crate::mm::virt::HIGHER_HALF {
        conn.write_str("error: addresses must be user-mode (< HIGHER_HALF)\n");
        return;
    }

    let exists = crate::vcpu::with(vcpu_id, |v| v.is_some());
    if !exists {
        conn.write_str("error: vCPU not found\n");
        return;
    }

    unsafe {
        *CONFIRM_LOAD_CODE_VCPU.get() = vcpu_id;
        *CONFIRM_LOAD_CODE_DST.get() = dst;
        *CONFIRM_LOAD_CODE_SRC.get() = src;
        *CONFIRM_LOAD_CODE_LEN.get() = len;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Copy {} byte(s) from {:#018x} to {:#018x} in vCPU {}\n", len, src, dst, vcpu_id));
    confirm_or_cancel(conn, "Copy code in vCPU space?", confirm_load_code);
}

fn confirm_load_code(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let _vcpu_id = unsafe { *CONFIRM_LOAD_CODE_VCPU.get() };
    let dst = unsafe { *CONFIRM_LOAD_CODE_DST.get() };
    let src = unsafe { *CONFIRM_LOAD_CODE_SRC.get() };
    let len = unsafe { *CONFIRM_LOAD_CODE_LEN.get() } as usize;

    crate::arch::dump::KATERM_RECOVERY.store(true, Ordering::SeqCst);
    let mut faulted = false;
    for i in 0..len {
        let b = unsafe { core::ptr::read_volatile((src + i as u64) as *const u8) };
        if !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst) {
            faulted = true;
            break;
        }
        unsafe { core::ptr::write_volatile((dst + i as u64) as *mut u8, b); }
        if !crate::arch::dump::KATERM_RECOVERY.load(Ordering::SeqCst) {
            faulted = true;
            break;
        }
    }
    crate::arch::dump::KATERM_RECOVERY.store(false, Ordering::SeqCst);

    let mut w = vtparser::ConnectorWriter { conn };
    if faulted {
        let _ = core::fmt::write(&mut w, format_args!("FAULT: memory access fault during copy (at byte {})\n", len));
    } else {
        let _ = core::fmt::write(&mut w, format_args!("Copied {} byte(s) from {:#018x} to {:#018x}\n", len, src, dst));
    }
}

pub(super) fn cmd_exec_page(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: exec_page(vcpu_id, phys_addr)\n"); return; }
    };
    let phys = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: exec_page(vcpu_id, phys_addr)\n"); return; }
    };

    if phys >= crate::mm::virt::HIGHER_HALF {
        conn.write_str("error: physical address too large\n");
        return;
    }

    let (exists, pml4) = crate::vcpu::with(vcpu_id, |v| {
        v.map(|vcpu| (true, vcpu.pml4)).unwrap_or((false, 0))
    });
    if !exists {
        conn.write_str("error: vCPU not found\n");
        return;
    }
    if pml4 == 0 {
        conn.write_str("error: vCPU has no address space\n");
        return;
    }

    unsafe {
        *CONFIRM_EXEC_PAGE_VCPU.get() = vcpu_id;
        *CONFIRM_EXEC_PAGE_PHYS.get() = phys;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Map phys {:#018x} as code page at 0x400000 in vCPU {} and set RIP=0x400000\n", phys, vcpu_id));
    confirm_or_cancel(conn, "Map code page and jump?", confirm_exec_page);
}

fn confirm_exec_page(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let vcpu_id = unsafe { *CONFIRM_EXEC_PAGE_VCPU.get() };
    let phys = unsafe { *CONFIRM_EXEC_PAGE_PHYS.get() };

    let (pml4, vtype, state) = match crate::vcpu::with(vcpu_id, |v| {
        v.map(|vcpu| (vcpu.pml4, vcpu.vcpu_type, vcpu.state))
    }) {
        Some(t) => t,
        None => { conn.write_str("error: vCPU not found\n"); return; }
    };
    if pml4 == 0 {
        conn.write_str("error: vCPU has no address space\n");
        return;
    }
    if state == crate::vcpu::VcpuState::Running {
        conn.write_str("error: vCPU is Running -- halt it first\n");
        return;
    }
    if vtype == crate::vcpu::VcpuType::Idle {
        conn.write_str("error: cannot exec_page on idle vCPU\n");
        return;
    }

    let virt_base = 0x400000u64;

    if let Some(pte) = crate::mm::virt::read_pte(pml4, virt_base) {
        if pte & crate::mm::virt::PRESENT != 0 {
            crate::mm::virt::unmap(virt_base);
        }
    }

    unsafe {
        crate::mm::virt::map_contiguous(pml4, virt_base, phys, 1, crate::mm::virt::CODE);
    }

    crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.saved_frame.rip = virt_base;
            vcpu.state = crate::vcpu::VcpuState::Ready;
        }
    });

    let best_cpu = crate::percpu::find_least_loaded();
    crate::percpu::rq(best_cpu).push(vcpu_id as usize);
    crate::percpu::set_task_count(best_cpu, crate::percpu::task_count(best_cpu) + 1);

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "Mapped {:#018x} at 0x400000 in vCPU {} -- RIP=0x400000, scheduled on CPU {}\n",
        phys, vcpu_id, best_cpu));
}

pub(super) fn cmd_jump(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: jump(vcpu_id, addr)\n"); return; }
    };
    let addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: jump(vcpu_id, addr)\n"); return; }
    };

    let exists = crate::vcpu::with(vcpu_id, |v| v.is_some());
    if !exists {
        conn.write_str("error: vCPU not found\n");
        return;
    }

    unsafe {
        *CONFIRM_JUMP_VCPU.get() = vcpu_id;
        *CONFIRM_JUMP_ADDR.get() = addr;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Set vCPU {} RIP={:#018x} and resume execution\n", vcpu_id, addr));
    confirm_or_cancel(conn, "Jump vCPU?", confirm_jump);
}

fn confirm_jump(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let vcpu_id = unsafe { *CONFIRM_JUMP_VCPU.get() };
    let addr = unsafe { *CONFIRM_JUMP_ADDR.get() };

    let state = crate::vcpu::with(vcpu_id, |v| v.map(|v| v.state));
    match state {
        Some(crate::vcpu::VcpuState::Running) => {
            conn.write_str("error: vCPU is currently Running on another CPU -- halt it first\n");
            return;
        }
        None => {
            conn.write_str("error: vCPU not found\n");
            return;
        }
        _ => {}
    }

    crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.saved_frame.rip = addr;
            vcpu.state = crate::vcpu::VcpuState::Ready;
            if (vcpu.saved_frame.cs & 3) == 0 {
                vcpu.saved_frame.cs = 0x1B;
                vcpu.saved_frame.ss = 0x23;
            }
            vcpu.saved_frame.rflags |= 0x202;
        }
    });

    let best_cpu = crate::percpu::find_least_loaded();
    crate::percpu::rq(best_cpu).push(vcpu_id as usize);
    crate::percpu::set_task_count(best_cpu, crate::percpu::task_count(best_cpu) + 1);

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "vCPU {} jumping to {:#018x} on CPU {}\n", vcpu_id, addr, best_cpu));
}

pub(super) fn cmd_force_next(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let cpu_str = match parser.parse_str() {
        Some(s) => s,
        None => { conn.write_str("Usage: force_next(CPU0)\n"); return; }
    };

    let cpu_slot = if cpu_str.starts_with("CPU") || cpu_str.starts_with("cpu") {
        let num_part = &cpu_str[3..];
        match num_part.parse::<usize>() {
            Ok(n) => n,
            Err(_) => { conn.write_str("error: invalid CPU, use e.g. CPU0\n"); return; }
        }
    } else {
        conn.write_str("error: expected e.g. force_next(CPU0)\n");
        return;
    };

    if cpu_slot >= lodaxos_system::MAX_CPUS {
        conn.write_str("error: CPU slot out of range\n");
        return;
    }
    if !crate::percpu::is_online(cpu_slot) {
        conn.write_str("error: CPU is not online\n");
        return;
    }

    let cur_vcpu = crate::percpu::current_vcpu(cpu_slot) as u32;
    let vtype = crate::vcpu::get_vcpu_type(cur_vcpu);
    let is_driver = vtype == crate::vcpu::VcpuType::HardwareDriver
        || vtype == crate::vcpu::VcpuType::AbstractionDriver;

    unsafe {
        *CONFIRM_FORCE_NEXT_CPU.get() = cpu_slot;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Force CPU {} to reschedule{}{}\n",
        cpu_slot,
        if is_driver { " (current: driver vCPU " } else { " (current: vCPU " },
        cur_vcpu));
    if is_driver {
        let _ = core::fmt::write(&mut w, format_args!(")"));
    }
    let _ = w.write_str("\n");
    confirm_or_cancel(conn, "Force reschedule?", confirm_force_next);
}

fn confirm_force_next(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let cpu_slot = unsafe { *CONFIRM_FORCE_NEXT_CPU.get() };

    if !crate::percpu::is_online(cpu_slot) {
        conn.write_str("error: CPU went offline\n");
        return;
    }

    let cur_vcpu = crate::percpu::current_vcpu(cpu_slot) as u32;
    crate::vcpu::with_mut(cur_vcpu, |v| {
        if let Some(vcpu) = v {
            vcpu.state = crate::vcpu::VcpuState::Halted;
        }
    });

    crate::percpu::PERCPU[cpu_slot].need_resched.store(true, Ordering::Release);

    let my_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    if cpu_slot != my_slot {
        let apic_id = crate::percpu::PERCPU[cpu_slot].apic_id.load(Ordering::Relaxed);
        crate::arch::apic::send_ipi(apic_id, crate::arch::idt::IPI_VECTOR);
    }

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "CPU {} forced to reschedule (vCPU {} halted)\n", cpu_slot, cur_vcpu));
}

pub(super) fn cmd_recover(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let cpu_str = match parser.parse_str() {
        Some(s) => s,
        None => { conn.write_str("Usage: recover(CPU0)\n"); return; }
    };

    let cpu_slot = if cpu_str.starts_with("CPU") || cpu_str.starts_with("cpu") {
        let num_part = &cpu_str[3..];
        match num_part.parse::<usize>() {
            Ok(n) => n,
            Err(_) => { conn.write_str("error: invalid CPU, use e.g. CPU0\n"); return; }
        }
    } else {
        conn.write_str("error: expected e.g. recover(CPU0)\n");
        return;
    };

    if cpu_slot >= lodaxos_system::MAX_CPUS {
        conn.write_str("error: CPU slot out of range\n");
        return;
    }

    let in_halt = crate::arch::dump::HALT_MODE.load(Ordering::Acquire);
    let cur_vcpu = crate::percpu::current_vcpu(cpu_slot) as u32;

    unsafe {
        *CONFIRM_RECOVER_CPU.get() = cpu_slot;
    }
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Recover CPU {} from{}halt mode\n",
        cpu_slot,
        if in_halt { " " } else { " potential " }));
    let _ = core::fmt::write(&mut w, format_args!(
        "  Current vCPU: {} -- will be terminated\n", cur_vcpu));
    confirm_or_cancel(conn, "Recover CPU?", confirm_recover);
}

fn confirm_recover(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let cpu_slot = unsafe { *CONFIRM_RECOVER_CPU.get() };

    if !crate::percpu::is_online(cpu_slot) {
        conn.write_str("error: CPU went offline\n");
        return;
    }

    crate::arch::dump::HALT_MODE.store(false, Ordering::Release);
    crate::arch::dump::DUMP_IN_PROGRESS.store(false, Ordering::Release);

    let cur_vcpu = crate::percpu::current_vcpu(cpu_slot) as u32;
    crate::vcpu::with_mut(cur_vcpu, |v| {
        if let Some(vcpu) = v {
            if vcpu.vcpu_type != crate::vcpu::VcpuType::Idle {
                vcpu.state = crate::vcpu::VcpuState::Terminated;
            }
        }
    });

    let idle_id = crate::percpu::idle_vcpu(cpu_slot);
    crate::percpu::set_current_vcpu(cpu_slot, idle_id as usize);

    crate::percpu::PERCPU[cpu_slot].need_resched.store(true, Ordering::Release);

    let my_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    if cpu_slot != my_slot {
        let apic_id = crate::percpu::PERCPU[cpu_slot].apic_id.load(Ordering::Relaxed);
        crate::arch::apic::send_ipi(apic_id, crate::arch::idt::IPI_VECTOR);
    }

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "CPU {} recovered: HALT_MODE cleared, vCPU {} terminated, idle vCPU {}\n",
        cpu_slot, cur_vcpu, idle_id));
}

pub(super) fn cmd_map(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: map(vcpu_id, virtual_addr)\n"); return; }
    };
    let virt_addr = match parser.parse_u64() {
        Some(a) => a,
        None => { conn.write_str("Usage: map(vcpu_id, virtual_addr)\n"); return; }
    };

    let pml4 = match crate::vcpu::with(vcpu_id, |v| v.map(|vcpu| vcpu.pml4)) {
        Some(p) => p,
        None => { conn.write_str("error: vCPU not found\n"); return; }
    };
    if pml4 == 0 {
        conn.write_str("error: vCPU has no address space\n");
        return;
    }

    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "Page table walk for vCPU {} at {:#018x}\n", vcpu_id, virt_addr));
    conn.write_str(vtparser::sgr_reset());

    let shifts = [39u64, 30, 21, 12];
    let masks = [0x1FFu64; 4];
    let names = ["PML4", "PDPT", "PD", "PT"];

    let mut current_phys = pml4;
    let mut present = true;

    for level in 0..4 {
        let idx = ((virt_addr >> shifts[level]) & masks[level]) as usize;
        let entry_addr = current_phys + idx as u64 * 8;
        let entry_virt = crate::mm::virt::HIGHER_HALF + entry_addr;
        let entry = if crate::mm::virt::translate(entry_virt).is_some() {
            unsafe { core::ptr::read_volatile(entry_virt as *const u64) }
        } else {
            present = false;
            0
        };

        let is_present = entry & 1 != 0;
        let is_writable = entry & 2 != 0;
        let is_user = entry & 4 != 0;
        let is_nx = entry & (1u64 << 63) != 0;
        let phys_addr = entry & 0x000F_FFFF_FFFF_F000;

        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {}[{:>3}] = {:#018x}  {}{}{}{}",
            names[level], idx, entry,
            if is_present { "P" } else { "-" },
            if is_writable { "W" } else { "-" },
            if is_user { "U" } else { "-" },
            if is_nx { "X" } else { "-" }));

        if is_present {
            if level < 3 {
                let _ = core::fmt::write(&mut w, format_args!("  -> phys {:#018x}", phys_addr));
            } else {
                let _ = core::fmt::write(&mut w, format_args!("  -> PAGE at phys {:#018x}", phys_addr));
            }
        } else if !present {
            let _ = w.write_str("  (not present)");
        }
        let _ = w.write_str("\n");

        if !is_present {
            break;
        }

        if level < 3 {
            current_phys = phys_addr;
        }
    }

    let phys = crate::mm::virt::translate(virt_addr);
    match phys {
        Some(p) => {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!(
                "\n  Final: {:#018x} -> physical {:#018x}\n", virt_addr, p));
        }
        None => {
            conn.write_str("\n  Final: NOT MAPPED\n");
        }
    }
}

pub(super) fn cmd_hlt(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let vcpu_id = match parser.parse_u64() {
        Some(i) => i as u32,
        None => { conn.write_str("Usage: hlt(vcpu_id)\n"); return; }
    };

    let (exists, state) = match crate::vcpu::with(vcpu_id, |v| {
        v.map(|vcpu| (true, vcpu.state))
    }) {
        Some(t) => t,
        None => { conn.write_str("error: vCPU not found\n"); return; }
    };
    if !exists { return; }

    if state == crate::vcpu::VcpuState::Running {
        conn.write_str("error: vCPU is Running on another CPU -- use force_next first\n");
        return;
    }
    if state == crate::vcpu::VcpuState::Halted || state == crate::vcpu::VcpuState::Terminated {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!("vCPU {} already halted\n", vcpu_id));
        return;
    }

    unsafe {
        *CONFIRM_HLT_VCPU.get() = vcpu_id;
    }

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "WARNING: Halt vCPU {} (state: {:?})\n", vcpu_id, state));
    confirm_or_cancel(conn, "Halt vCPU?", confirm_hlt);
}

fn confirm_hlt(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if !yes {
        conn.write_str("Cancelled\n");
        return;
    }
    let vcpu_id = unsafe { *CONFIRM_HLT_VCPU.get() };

    crate::vcpu::with_mut(vcpu_id, |v| {
        if let Some(vcpu) = v {
            vcpu.state = crate::vcpu::VcpuState::Halted;
        }
    });

    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!("vCPU {} halted\n", vcpu_id));
}
