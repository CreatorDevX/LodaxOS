use core::fmt;
use x86_64::instructions::port::Port;

use crate::sync::IrqSaveSpinLock;

const COM1: u16 = 0x3F8;

static SERIAL_LOCK: IrqSaveSpinLock<()> = IrqSaveSpinLock::new(());

pub fn init() {
    // UART 16550 already initialized by bootloader.
    // Re-initializing would drop FIFO contents and add ~200 us delay.
}

fn write_byte_raw(byte: u8) {
    unsafe {
        let mut timeout: u32 = 0xFFFF;
        loop {
            let lsr = Port::<u8>::new(COM1 + 5).read();
            if lsr & 0x20 != 0 {
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                return;
            }
        }
        Port::<u8>::new(COM1).write(byte);
    }
}

fn write_str_inner(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            write_byte_raw(b'\r');
        }
        write_byte_raw(b);
    }
}

/// Write a string to COM1.  Acquires the global serial spinlock so
/// that concurrent writes from multiple CPUs are serialised.
pub fn write_str(s: &str) {
    let _guard = SERIAL_LOCK.lock();
    write_str_inner(s);
}

/// Write a string to COM1 **without** acquiring the serial spinlock.
/// Use only from the panic handler (which may already hold the lock)
/// and from very-early-boot code where no other CPUs are active yet.
pub fn write_str_unlocked(s: &str) {
    write_str_inner(s);
}

/// A RAII guard that holds the serial lock for the duration of a multi-line
/// dump.  Each line is written directly to COM1 *without* the usual
/// `[CPU] [LEVEL] module:` prefix -- the caller prints a single header line
/// with CPU info at the top and raw lines afterwards.
pub struct DumpWriter {
    _guard: crate::sync::IrqSaveGuard<'static, ()>,
}

impl DumpWriter {
    /// Acquire the serial lock and return a writer that holds it.
    pub fn lock() -> Self {
        DumpWriter {
            _guard: SERIAL_LOCK.lock(),
        }
    }
}

impl fmt::Write for DumpWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str_inner(s);
        Ok(())
    }
}
