use core::fmt::Write;

use log::{Level, LevelFilter, Log, Metadata, Record, SetLoggerError};

use crate::serial;

struct SerialLogger;

impl Log for SerialLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let level = record.level();
        let level_padded = match level {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };

        serial::write_str("[");
        serial::write_str(level_padded);
        serial::write_str("] ");
        serial::write_str(record.target());
        serial::write_str(": ");

        struct SerialWriter;
        impl Write for SerialWriter {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                serial::write_str(s);
                Ok(())
            }
        }

        let _ = core::fmt::write(&mut SerialWriter, *record.args());
        serial::write_str("\n");
    }

    fn flush(&self) {}
}

static LOGGER: SerialLogger = SerialLogger;

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(LevelFilter::Trace);
    Ok(())
}
