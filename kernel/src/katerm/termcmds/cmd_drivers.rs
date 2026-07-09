use super::super::connector::Connector;
use super::super::vtparser;
use super::confirm_or_cancel;

static CONFIRM_DRV_INDEX: crate::sync::SyncUnsafeCell<usize> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_DRV_CMD: crate::sync::SyncUnsafeCell<u32> = crate::sync::SyncUnsafeCell::new(0);
static CONFIRM_DRV_ARGS: crate::sync::SyncUnsafeCell<(u64, u64, u64)> = crate::sync::SyncUnsafeCell::new((0, 0, 0));

pub(super) fn cmd_drivers(_args: &str, conn: &dyn Connector) {
    let mut found = false;
    let mut w = vtparser::ConnectorWriter { conn };
    for i in 0..16 {
        if let Some(name_arr) = crate::gdf::pkg_name(i) {
            found = true;
            let name = core::str::from_utf8(&name_arr[..]).unwrap_or("?").trim_end_matches('\0');
            let class = crate::gdf::pkg_class(i).unwrap_or("?");
            let _ = core::fmt::write(&mut w, format_args!("  [{}] {} (class={})\n", i, name, class));
        }
    }
    if !found {
        conn.write_str("No GDF drivers registered\n");
    }
}

pub(super) fn cmd_services(_args: &str, conn: &dyn Connector) {
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

pub(super) fn cmd_drv_call(args: &str, conn: &dyn Connector) {
    let mut parser = super::super::termexec::Args::new(args);
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

fn confirm_drv_call(yes: bool) {
    let conn = super::super::connector::get_active().unwrap();
    if yes {
        let idx = unsafe { *CONFIRM_DRV_INDEX.get() };
        let cmd = unsafe { *CONFIRM_DRV_CMD.get() };
        let (arg0, arg1, arg2) = unsafe { *CONFIRM_DRV_ARGS.get() };

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
