use super::super::connector::Connector;
use super::super::vtparser;

pub(super) fn cmd_ps(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_trace(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
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
        crate::vcpu::VcpuState::Terminated => "Terminated",
        crate::vcpu::VcpuState::Idle => "Idle",
    };
    let _ = core::fmt::write(&mut w, format_args!(
        "  Gang: {}  Type: {}  State: {}\n", vcpu.gang_id, type_name, state_name
    ));
}

pub(super) fn cmd_vcpus(_args: &str, conn: &dyn Connector) {
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
                crate::vcpu::VcpuState::Terminated => "Terminated",
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

pub(super) fn cmd_loadavg(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_rq(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
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
    let drops = queue.dropped_count();
    if drops > 0 {
        let _ = core::fmt::write(&mut w, format_args!("CPU {} queue overflows: {}\n", cpu, drops));
    }
}

pub(super) fn cmd_slabstat(_args: &str, conn: &dyn Connector) {
    let stats = crate::mm::heap::slab_stats();

    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("Kernel Heap Slab Allocator\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n  ObjSize   Slabs(P/F/E)   Total Objs   Free Objs   Utilization\n");
    conn.write_str("  -------   ------------   ----------   ---------   -----------\n");

    let mut total_alloc = 0usize;
    let mut total_free = 0usize;

    for stat in &stats {
        let slabs = stat.slab_count[0] + stat.slab_count[1] + stat.slab_count[2];
        if slabs == 0 {
            continue;
        }
        let used = stat.total_objs - stat.free_objs;
        let util = if stat.total_objs > 0 {
            used * 100 / stat.total_objs
        } else {
            0
        };
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>7}   {:>2}/{:>2}/{:>2}       {:>8}     {:>8}   {:>3}%\n",
            stat.obj_size,
            stat.slab_count[1], // full
            stat.slab_count[0], // partial
            stat.slab_count[2], // free (empty)
            stat.total_objs,
            stat.free_objs,
            util));
        total_alloc += stat.total_objs;
        total_free += stat.free_objs;
    }

    let total_used = total_alloc - total_free;
    let util = if total_alloc > 0 { total_used * 100 / total_alloc } else { 0 };
    let mut w = vtparser::ConnectorWriter { conn };
    let _ = core::fmt::write(&mut w, format_args!(
        "  Total: {} allocated, {} free, {} used ({}% utilization)\n",
        total_alloc, total_free, total_used, util));
}
