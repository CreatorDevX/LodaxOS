const COM1: u16 = 0x3F8;

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

fn write_byte(byte: u8) {
    unsafe {
        loop {
            let lsr: u8;
            core::arch::asm!("in al, dx", out("al") lsr, in("dx") COM1 + 5, options(nostack, nomem));
            if lsr & 0x20 != 0 {
                break;
            }
        }
        core::arch::asm!("out dx, al", in("dx") COM1 + 0, in("al") byte, options(nostack, nomem));
    }
}

pub fn write_str(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}
