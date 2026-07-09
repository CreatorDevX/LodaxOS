use super::connector::Connector;

pub struct Serial2PolledConnector;

impl Connector for Serial2PolledConnector {
    fn read_byte(&self) -> Option<u8> {
        crate::serial2::poll_read_byte()
    }

    fn write_str(&self, s: &str) {
        crate::serial2::write_str_unlocked(s)
    }

    fn write_byte(&self, b: u8) {
        let buf = [b];
        if let Ok(s) = core::str::from_utf8(&buf) {
            self.write_str(s);
        }
    }

    fn name(&self) -> &'static str {
        "com2-polled"
    }
}

pub static SERIAL2_POLLED: Serial2PolledConnector = Serial2PolledConnector;
