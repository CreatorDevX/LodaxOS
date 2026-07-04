use crate::sync::SyncUnsafeCell;

pub trait Connector: Send + Sync {
    fn read_byte(&self) -> Option<u8>;
    fn write_str(&self, s: &str);
    fn write_byte(&self, b: u8);
    fn name(&self) -> &'static str;
}

static ACTIVE: SyncUnsafeCell<Option<&'static dyn Connector>> = SyncUnsafeCell::new(None);

pub fn set_active(conn: &'static dyn Connector) {
    unsafe { *ACTIVE.get() = Some(conn); }
}

pub fn get_active() -> Option<&'static dyn Connector> {
    unsafe { *ACTIVE.get() }
}
