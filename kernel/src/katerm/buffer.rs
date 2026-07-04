use crate::sync::SyncUnsafeCell;

const MAX_LINE: usize = 256;

static BUF: SyncUnsafeCell<[u8; MAX_LINE]> = SyncUnsafeCell::new([0u8; MAX_LINE]);
static POS: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);
static LINE_LEN: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);

/// Push a byte into the line buffer.
/// Returns true if a complete line (terminated by \n) is ready.
pub fn push_byte(b: u8) -> bool {
    unsafe {
        let pos = &mut *POS.get();
        let buf = &mut *BUF.get();

        match b {
            b'\r' => {}
            b'\n' => {
                buf[*pos] = 0;
                *LINE_LEN.get() = *pos;
                *pos = 0;
                return true;
            }
            0x7F | 0x08 => {
                if *pos > 0 {
                    *pos -= 1;
                    if let Some(conn) = super::connector::get_active() {
                        conn.write_str("\x08 \x08");
                    }
                }
            }
            _ if b >= 0x20 && b <= 0x7E => {
                if *pos < MAX_LINE - 1 {
                    buf[*pos] = b;
                    *pos += 1;
                    if let Some(conn) = super::connector::get_active() {
                        let arr = [b];
                        let s = core::str::from_utf8_unchecked(&arr);
                        conn.write_str(s);
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Return the completed line as a string.
pub fn as_str() -> &'static str {
    unsafe {
        let buf = &*BUF.get();
        let len = *LINE_LEN.get();
        core::str::from_utf8_unchecked(&buf[..len])
    }
}

/// Reset the buffer for the next line.
pub fn reset() {
    unsafe { *POS.get() = 0; }
}
