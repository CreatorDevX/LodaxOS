use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

pub fn init() {
    unsafe {
        Port::<u8>::new(COM1 + 3).write(0x80u8);
        Port::<u8>::new(COM1).write(0x01u8);
        Port::<u8>::new(COM1 + 1).write(0x00u8);
        Port::<u8>::new(COM1 + 3).write(0x03u8);
        Port::<u8>::new(COM1 + 2).write(0xC7u8);
        // MCR: DTR=1, RTS=1, OUT2=1 (enable interrupt output via PIC).
        Port::<u8>::new(COM1 + 4).write(0x0Bu8);
    }
}

fn write_byte(byte: u8) {
    unsafe {
        let mut timeout: u32 = 0xFFFFFF;
        loop {
            let lsr: u8 = Port::<u8>::new(COM1 + 5).read();
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

pub fn write_str(s: &str) {
    for &b in s.as_bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}
