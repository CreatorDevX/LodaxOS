#![no_std]
#![no_main]
#![allow(unsafe_op_in_unsafe_fn)]

/// Secure Runtime stub — entry point.
///
/// The kernel loads this ELF into memory after boot. Execution does not
/// reach here yet; the kernel parses the ELF headers and records the
/// segments but does not transfer control.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("cli; hlt") } }
}
