use core::fmt::Write;

use log::{Level, LevelFilter, Log, Metadata, Record, SetLoggerError};

use crate::serial;

struct BufWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Write for BufWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let n = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

struct SerialLogger;

impl Log for SerialLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let cpu = crate::percpu::current_apic_id();
        let level_padded = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };

        let mut buf = [0u8; 512];
        let mut w = BufWriter { buf: &mut buf, pos: 0 };

        let _ = core::fmt::write(
            &mut w,
            format_args!("[CPU{}] [{}] {}: ", cpu, level_padded, record.target()),
        );
        let _ = core::fmt::write(&mut w, *record.args());
        let _ = w.write_str("\n");

        let filled = w.pos;
        if filled > 0 {
            let s = unsafe { core::str::from_utf8_unchecked(&buf[..filled]) };
            serial::write_str(s);
        }
    }

    fn flush(&self) {}
}

static LOGGER: SerialLogger = SerialLogger;

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(LevelFilter::Trace);
    Ok(())
}
