use super::connector::Connector;

pub struct Serial2IntConnector;

impl Connector for Serial2IntConnector {
    fn read_byte(&self) -> Option<u8> {
        crate::serial2::read_byte()
    }

    fn write_str(&self, s: &str) {
        crate::serial2::write_str(s)
    }

    fn write_byte(&self, b: u8) {
        let buf = [b];
        if let Ok(s) = core::str::from_utf8(&buf) {
            self.write_str(s);
        }
    }

    fn name(&self) -> &'static str {
        "com2"
    }
}

pub static SERIAL2_INT: Serial2IntConnector = Serial2IntConnector;
