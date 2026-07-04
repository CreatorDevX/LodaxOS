use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::sync::SyncUnsafeCell;

pub mod connector;
pub mod buffer;
pub mod termexec;
pub mod termcmds;
pub mod vtparser;

#[cfg(debug_assertions)]
pub mod serial2int;

static BOOTED: AtomicBool = AtomicBool::new(false);
static BOOT_POS: AtomicUsize = AtomicUsize::new(0);
const BOOT_CMD: &[u8] = b"bootkaterm";

// ── Confirmation prompt system ──────────────────────────────────────
static CONFIRM_CALLBACK: SyncUnsafeCell<Option<fn(bool)>> = SyncUnsafeCell::new(None);

/// Request a y/n confirmation from the user.
/// The callback is invoked on the next input cycle with `true` for y/Y, `false` otherwise.
pub fn request_confirm(prompt: &str, callback: fn(bool)) {
    if let Some(conn) = connector::get_active() {
        conn.write_str(prompt);
    }
    unsafe { *CONFIRM_CALLBACK.get() = Some(callback); }
}

/// Called from idle loop — handles pre-boot scanning and live command processing.
pub fn process_input() {
    if !BOOTED.load(Ordering::Relaxed) {
        process_pre_boot();
        return;
    }

    let conn = match connector::get_active() {
        Some(c) => c,
        None => return,
    };

    // Handle pending confirmation first — intercept all input
    let cb = unsafe { (*CONFIRM_CALLBACK.get()).take() };
    if let Some(callback) = cb {
        while let Some(b) = conn.read_byte() {
            let arr = [b];
            let s = unsafe { core::str::from_utf8_unchecked(&arr) };
            let confirmed = b == b'y' || b == b'Y';
            callback(confirmed);
            conn.write_str(s);
            conn.write_str("\n");
            conn.write_str("KERNEL$ ");
            return;
        }
        // No byte yet — restore callback for next cycle
        unsafe { *CONFIRM_CALLBACK.get() = Some(callback); }
        return;
    }

    while let Some(b) = conn.read_byte() {
        if buffer::push_byte(b) {
            let line = buffer::as_str();
            termexec::execute(line);
            buffer::reset();
            // Only print prompt if not in confirmation mode
            if unsafe { (*CONFIRM_CALLBACK.get()).is_none() } {
                conn.write_str("\r\nKERNEL$ ");
            }
        }
    }
}

/// Pre-boot: scan COM2 ring buffer for the "bootkaterm" command.
/// Only compiled when the COM2 serial hardware module is available.
fn process_pre_boot() {
    #[cfg(debug_assertions)]
    {
        while let Some(b) = crate::serial2::read_byte() {
            if handle_boot_byte(b) {
                boot_katerm();
                return;
            }
        }
    }
}

/// Match an incoming byte against the expected boot sequence.
/// Resets on mismatch, returns true only when the full command is matched.
#[cfg(debug_assertions)]
fn handle_boot_byte(b: u8) -> bool {
    let pos = BOOT_POS.load(Ordering::Relaxed);
    if pos < BOOT_CMD.len() && b == BOOT_CMD[pos] {
        BOOT_POS.store(pos + 1, Ordering::Relaxed);
        if pos + 1 == BOOT_CMD.len() {
            return true;
        }
    } else {
        BOOT_POS.store(0, Ordering::Relaxed);
        if b == BOOT_CMD[0] {
            BOOT_POS.store(1, Ordering::Relaxed);
        }
    }
    false
}

/// Activate katerm: set COM2 connector, show boot banner.
#[cfg(debug_assertions)]
fn boot_katerm() {
    BOOTED.store(true, Ordering::Relaxed);
    connector::set_active(&serial2int::SERIAL2_INT);

    write("\n");
    write("Starting...\n");
    write("=================================================\n");
    write("LodaxOS - Bedrock Stack - Kernel Access Terminal\n");
    write("Unsafe katerm v0.1.\n");
    write("Welcome [COM2]\n");
    write("=================================================\n");
    write("\n");
    write("KERNEL$ ");
}

/// Write a string to the active connector.
pub fn write(s: &str) {
    if let Some(conn) = connector::get_active() {
        conn.write_str(s);
    }
}
