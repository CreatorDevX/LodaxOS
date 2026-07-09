use core::fmt::Write as _;
use super::super::connector::Connector;
use super::super::vtparser;

pub(super) fn cmd_cpuinfo(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_lapic(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_ioapic_dump(args: &str, conn: &dyn Connector) {
    if !crate::arch::ioapic::is_initialized() {
        conn.write_str("IOAPIC not initialized\n");
        return;
    }
    let mut parser = super::super::termexec::Args::new(args);
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

pub(super) fn cmd_irq(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_ticks(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_dumpcpu(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
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

pub(super) fn cmd_dumpremote(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
    let cpu_str = match parser.parse_str() {
        Some(s) => s,
        None => {
            conn.write_str("error: expected CPU name, e.g. dumpremote(CPU1)\n");
            return;
        }
    };

    let cpu_slot = if cpu_str.starts_with("CPU") || cpu_str.starts_with("cpu") {
        let num_part = &cpu_str[3..];
        match num_part.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                conn.write_str("error: invalid CPU, use e.g. CPU1\n");
                return;
            }
        }
    } else {
        conn.write_str("error: expected e.g. CPU1\n");
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

    let my_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
    if cpu_slot == my_slot {
        conn.write_str("error: use dumpall() for local CPU or target another CPU\n");
        return;
    }

    let apic_id = crate::percpu::PERCPU[cpu_slot].apic_id.load(core::sync::atomic::Ordering::Relaxed);

    conn.write_str("Requesting remote register dump (see COM1 serial)...\n");

    crate::arch::dump::DUMP_REQ_SLOT.store(my_slot as u64, core::sync::atomic::Ordering::Release);
    crate::arch::dump::DUMP_ACK.store(0, core::sync::atomic::Ordering::Release);
    crate::arch::apic::send_ipi(apic_id, crate::arch::idt::IPI_VECTOR);

    for _ in 0..10_000_000 {
        if crate::arch::dump::DUMP_ACK.load(core::sync::atomic::Ordering::Acquire) != 0 {
            conn.write_str("Remote dump complete\n");
            return;
        }
        core::hint::spin_loop();
    }

    conn.write_str("Timeout waiting for remote dump (CPU may have interrupts disabled)\n");
}

pub(super) fn cmd_dumpall(_args: &str, conn: &dyn Connector) {
    let my_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());

    conn.write_str("Dumping all online CPUs (see COM1 serial)...\n");

    for slot in 0..lodaxos_system::MAX_CPUS {
        if !crate::percpu::is_online(slot) { continue; }
        if slot == my_slot { continue; }

        let apic_id = crate::percpu::PERCPU[slot].apic_id.load(core::sync::atomic::Ordering::Relaxed);

        conn.write_str("  CPU");
        {
            let mut w = vtparser::ConnectorWriter { conn };
            let _ = core::fmt::write(&mut w, format_args!("{}...\n", slot));
        }

        crate::arch::dump::DUMP_REQ_SLOT.store(my_slot as u64, core::sync::atomic::Ordering::Release);
        crate::arch::dump::DUMP_ACK.store(0, core::sync::atomic::Ordering::Release);
        crate::arch::apic::send_ipi(apic_id, crate::arch::idt::IPI_VECTOR);

        let mut timed_out = true;
        for _ in 0..10_000_000 {
            if crate::arch::dump::DUMP_ACK.load(core::sync::atomic::Ordering::Acquire) != 0 {
                timed_out = false;
                break;
            }
            core::hint::spin_loop();
        }

        if timed_out {
            conn.write_str("  (timeout)\n");
        }
    }

    conn.write_str("All remote dumps complete\n");
}

pub(super) fn cmd_irqstat(_args: &str, conn: &dyn Connector) {
    conn.write_str(vtparser::sgr_bold());
    conn.write_str(vtparser::sgr_fg_cyan());
    conn.write_str("Interrupt/Exception Counts\n");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str("\n  Vector   Count      Description\n");
    conn.write_str("  ------   -----      -----------\n");

    let names = [
        "#DE", "#DB", "NMI", "#BP", "#OF", "#BR", "#UD", "#NM",
        "#DF", "reserved", "#TS", "#NP", "#SS", "#GP", "#PF", "#MF",
        "#AC", "#MC", "#XM", "#VE", "#CP",
    ];

    for vector in 0u8..22 {
        let count = crate::arch::idt::read_irq_count(vector);
        if count == 0 { continue; }
        let name = names[vector as usize];
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>6}   {:>10}   {}\n", vector, count, name));
    }

    let timer = crate::arch::idt::read_irq_count(32);
    if timer > 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>6}   {:>10}   LAPIC timer\n", 32, timer));
    }

    let serial = crate::arch::idt::read_irq_count(35);
    if serial > 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>6}   {:>10}   COM2 serial\n", 35, serial));
    }

    let ipi = crate::arch::idt::read_irq_count(0x81);
    if ipi > 0 {
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>6}   {:>10}   IPI (reschedule)\n", 0x81, ipi));
    }

    for vector in 36u8..255 {
        let count = crate::arch::idt::read_irq_count(vector);
        if count == 0 { continue; }
        let mut w = vtparser::ConnectorWriter { conn };
        let _ = core::fmt::write(&mut w, format_args!(
            "  {:>6}   {:>10}   (device IRQ)\n", vector, count));
    }
}
