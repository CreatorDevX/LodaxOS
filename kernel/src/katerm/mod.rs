use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::sync::SyncUnsafeCell;

pub mod connector;
pub mod buffer;
pub mod termexec;
pub mod termcmds;
pub mod vtparser;
pub mod tui;

#[cfg(debug_assertions)]
pub mod serial2int;

pub mod serial2polled;

static BOOTED: AtomicBool = AtomicBool::new(false);
static BOOT_POS: AtomicUsize = AtomicUsize::new(0);
const BOOT_CMD: &[u8] = b"bootkaterm";

/// Set to `true` once katerm is fully ready to process commands
/// (heap, scheduler, page tables all initialized).
static KATERM_READY: AtomicBool = AtomicBool::new(false);

/// Mark katerm as ready to process commands.
pub fn set_ready() {
    KATERM_READY.store(true, Ordering::Release);
}

/// Return true if katerm is ready to process commands.
pub fn is_ready() -> bool {
    KATERM_READY.load(Ordering::Acquire)
}

// -- Confirmation prompt system --------------------------------------
static CONFIRM_CALLBACK: SyncUnsafeCell<Option<fn(bool)>> = SyncUnsafeCell::new(None);

/// Request a y/n confirmation from the user.
/// The callback is invoked on the next input cycle with `true` for y/Y, `false` otherwise.
pub fn request_confirm(prompt: &str, callback: fn(bool)) {
    if let Some(conn) = connector::get_active() {
        conn.write_str(prompt);
    }
    unsafe { *CONFIRM_CALLBACK.get() = Some(callback); }
}

/// Called from idle loop -- handles pre-boot scanning and live command processing.
pub fn process_input() {
    if !BOOTED.load(Ordering::Relaxed) {
        process_pre_boot();
        return;
    }

    let conn = match connector::get_active() {
        Some(c) => c,
        None => return,
    };

    // If not ready, discard all pending input to avoid accumulating
    // garbage before the heap and scheduler are up.
    if !KATERM_READY.load(Ordering::Relaxed) {
        while let Some(_b) = conn.read_byte() {
            // discard
        }
        return;
    }

    // TUI mode takes over input processing
    if tui::is_active() {
        tui::process_input_tui();
        return;
    }

    // Handle pending confirmation first -- intercept all input
    let cb = unsafe { (*CONFIRM_CALLBACK.get()).take() };
    if let Some(callback) = cb {
        while let Some(b) = conn.read_byte() {
            let arr = [b];
            let s = unsafe { core::str::from_utf8_unchecked(&arr) };
            let confirmed = b == b'y' || b == b'Y';
            callback(confirmed);
            conn.write_str(s);
            conn.write_str("\n");
            conn.write_str(termcmds::prompt_for_mode(termcmds::current_mode()));
            return;
        }
        // No byte yet -- restore callback for next cycle
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
                conn.write_str("\r\n");
                conn.write_str(termcmds::prompt_for_mode(termcmds::current_mode()));
            }
        }
    }
}

/// Pre-boot: scan COM2 for the "bootkaterm" command.
/// Tries the interrupt-driven ring buffer first, then falls back to
/// polled UART reads so the handshake works even before interrupts
/// are enabled.
fn process_pre_boot() {
    #[cfg(debug_assertions)]
    {
        // Drain ring buffer (filled by COM2 IRQ handler)
        while let Some(b) = crate::serial2::read_byte_unlocked() {
            if handle_boot_byte(b) {
                boot_katerm();
                return;
            }
        }
        // Also poll the UART directly in case interrupts aren't up yet
        while let Some(b) = crate::serial2::poll_read_byte() {
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
    // Use polled connector if interrupt-driven one isn't available yet.
    if connector::get_active().is_none() {
        connector::set_active(&serial2polled::SERIAL2_POLLED);
    }

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

/// Early boot probe: scan COM2 for "bootkaterm" using polled UART reads.
/// Runs before page tables, heap, or scheduler. Only does port I/O.
#[cfg(debug_assertions)]
pub fn early_probe() {
    use crate::serial2::poll_read_byte;

    // Set the polled connector so katerm::write() works.
    connector::set_active(&serial2polled::SERIAL2_POLLED);

    // Scan for "bootkaterm" handshake.
    let mut pos = 0usize;
    loop {
        // Give up after a reasonable number of bytes to avoid hanging boot.
        if pos > 4096 {
            break;
        }
        match poll_read_byte() {
            Some(b) => {
                pos += 1;
                if handle_boot_byte(b) {
                    boot_katerm();
                    // Drain a few more bytes to give the user a chance.
                    for _ in 0..256 {
                        if poll_read_byte().is_none() {
                            break;
                        }
                    }
                    return;
                }
            }
            None => {
                // No data available, stop early — don't block boot.
                break;
            }
        }
    }
}

/// Write a string to the active connector.
pub fn write(s: &str) {
    if let Some(conn) = connector::get_active() {
        conn.write_str(s);
    }
}

/// Enter the rescue debugger. Called from `halt_loop()` after a fatal fault.
/// Switches to the rescue PML4 + katerm stack, enables interrupts, and
/// spins forever processing katerm input.
pub fn enter_rescue_mode() -> ! {
    // Switch to the rescue PML4 if available.
    let pml4 = crate::mm::virt::KATERM_PML4.load(core::sync::atomic::Ordering::Relaxed);
    if pml4 != 0 {
        crate::mm::virt::switch_pml4(pml4);
    }

    // Load katerm stack top (above the guard page).
    let stack_top = crate::mm::virt::KATERM_STACK_TOP.load(core::sync::atomic::Ordering::Relaxed);
    if stack_top != 0 {
        unsafe {
            core::arch::asm!("mov rsp, {}", in(reg) stack_top);
        }
    }

    // Ensure interrupts are enabled so the timer ISR fires
    // and COM2 input is processed.
    x86_64::instructions::interrupts::enable();

    katerm_rescue_loop()
}

fn katerm_rescue_loop() -> ! {
    loop {
        process_input();
        unsafe { core::arch::asm!("pause"); }
    }
}
