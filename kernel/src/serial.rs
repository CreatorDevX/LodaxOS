use crate::sync::IrqSaveSpinLock;

const COM1: u16 = 0x3F8;

static SERIAL_LOCK: IrqSaveSpinLock<()> = IrqSaveSpinLock::new(());

pub fn init() {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") COM1 + 3, in("al") 0x80u8, options(nostack, nomem));

        core::arch::asm!("out dx, al", in("dx") COM1 + 0, in("al") 0x01u8, options(nostack, nomem));
        core::arch::asm!("out dx, al", in("dx") COM1 + 1, in("al") 0x00u8, options(nostack, nomem));

        core::arch::asm!("out dx, al", in("dx") COM1 + 3, in("al") 0x03u8, options(nostack, nomem));

        core::arch::asm!("out dx, al", in("dx") COM1 + 2, in("al") 0xC7u8, options(nostack, nomem));

        core::arch::asm!("out dx, al", in("dx") COM1 + 4, in("al") 0x0Bu8, options(nostack, nomem));
    }
}

fn write_byte_raw(byte: u8) {
    unsafe {
        let mut timeout: u32 = 0xFFFF;
        loop {
            let lsr: u8;
            core::arch::asm!("in al, dx", out("al") lsr, in("dx") COM1 + 5, options(nostack, nomem));
            if lsr & 0x20 != 0 {
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                return;
            }
        }
        core::arch::asm!("out dx, al", in("dx") COM1 + 0, in("al") byte, options(nostack, nomem));
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
