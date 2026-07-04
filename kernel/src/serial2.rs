use core::sync::atomic::{AtomicUsize, Ordering};
use x86_64::instructions::port::Port;

use crate::sync::{IrqSaveSpinLock, SyncUnsafeCell};

const COM2: u16 = 0x2F8;

// ---- Ring buffer for interrupt-driven receive ----

const RING_SIZE: usize = 1024;
const RING_MASK: usize = RING_SIZE - 1;
static RING_BUF: SyncUnsafeCell<[u8; RING_SIZE]> = SyncUnsafeCell::new([0u8; RING_SIZE]);
static RING_HEAD: AtomicUsize = AtomicUsize::new(0);
static RING_TAIL: AtomicUsize = AtomicUsize::new(0);

// ---- Synchronisation for write path ----

static SERIAL2_LOCK: IrqSaveSpinLock<()> = IrqSaveSpinLock::new(());

fn push_ring(byte: u8) {
    let head = RING_HEAD.load(Ordering::Relaxed);
    let tail = RING_TAIL.load(Ordering::Acquire);
    unsafe { (*RING_BUF.get())[head] = byte; }
    let next = (head + 1) & RING_MASK;
    if next == tail {
        RING_TAIL.store((tail + 1) & RING_MASK, Ordering::Release);
    }
    RING_HEAD.store(next, Ordering::Release);
}

fn pop_ring() -> Option<u8> {
    let tail = RING_TAIL.load(Ordering::Relaxed);
    let head = RING_HEAD.load(Ordering::Acquire);
    if tail == head {
        return None;
    }
    let byte = unsafe { (*RING_BUF.get())[tail] };
    RING_TAIL.store((tail + 1) & RING_MASK, Ordering::Release);
    Some(byte)
}

// ---- Initialisation ----

pub fn init() {
    unsafe {
        Port::<u8>::new(COM2 + 3).write(0x80u8);
        Port::<u8>::new(COM2).write(0x01u8);
        Port::<u8>::new(COM2 + 1).write(0x00u8);
        Port::<u8>::new(COM2 + 3).write(0x03u8);
        Port::<u8>::new(COM2 + 2).write(0xC7u8);
        Port::<u8>::new(COM2 + 4).write(0x0Bu8);
        Port::<u8>::new(COM2 + 1).write(0x05u8);
    }

    if let Some(route) = crate::intr::lookup_isa(3) {
        log::info!(
            "COM2: ISA IRQ 3 → GSI {} → IOAPIC[{}] pin {} vector {}",
            route.gsi, route.ioapic_index, route.ioapic_pin, route.vector,
        );
        crate::intr::enable_route(route);
    } else {
        log::warn!("COM2: no IOAPIC route for ISA IRQ 3");
    }
}

// ---- Write path (polled) ----

fn write_byte_raw(byte: u8) {
    unsafe {
        let mut timeout: u32 = crate::consts::SERIAL_TIMEOUT;
        loop {
            let lsr = Port::<u8>::new(COM2 + 5).read();
            if lsr & 0x20 != 0 {
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                return;
            }
        }
        Port::<u8>::new(COM2).write(byte);
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

pub fn write_str(s: &str) {
    let _guard = SERIAL2_LOCK.lock();
    write_str_inner(s);
}

pub fn write_str_unlocked(s: &str) {
    write_str_inner(s);
}

// ---- Read path (interrupt-driven, ring buffer) ----

pub fn read_byte() -> Option<u8> {
    let _guard = SERIAL2_LOCK.lock();
    pop_ring()
}

pub fn read_byte_unlocked() -> Option<u8> {
    pop_ring()
}

pub fn data_available() -> bool {
    let tail = RING_TAIL.load(Ordering::Relaxed);
    let head = RING_HEAD.load(Ordering::Acquire);
    tail != head
}

// ---- Interrupt handler ----

pub fn irq_handler() {
    let lsr: u8 = unsafe { Port::<u8>::new(COM2 + 5).read() };
    if lsr & 1 != 0 {
        loop {
            let byte: u8 = unsafe { Port::<u8>::new(COM2).read() };
            push_ring(byte);
            let more: u8 = unsafe { Port::<u8>::new(COM2 + 5).read() };
            if more & 1 == 0 {
                break;
            }
        }
    }
}
