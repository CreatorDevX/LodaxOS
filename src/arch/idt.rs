#![allow(dead_code)]

use core::arch::{asm, naked_asm};
use core::mem;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

const KERNEL_CODE_SEL: u16 = super::gdt::KERNEL_CODE_SEL;

/// IDTR register value — 10 bytes: 2-byte limit + 8-byte base.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 64-bit IDT gate descriptor — 16 bytes.
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn empty() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    const fn interrupt_gate(handler: u64, ist: u8) -> Self {
        let offset = handler;
        Self {
            offset_low: (offset & 0xFFFF) as u16,
            selector: KERNEL_CODE_SEL,
            ist,
            type_attr: 0x8E,
            offset_mid: ((offset >> 16) & 0xFFFF) as u16,
            offset_high: ((offset >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }
}

/// Full register state saved by exception/IRQ stubs.
///
/// Stack layout after stub pushes (low address → high):
///   [rsp+0x00] r15          ← TrapFrame base (passed as &TrapFrame)
///   [rsp+0x08] r14
///   [rsp+0x10] r13
///   [rsp+0x18] r12
///   [rsp+0x20] r11
///   [rsp+0x28] r10
///   [rsp+0x30] r9
///   [rsp+0x38] r8
///   [rsp+0x40] rax
///   [rsp+0x48] rbx
///   [rsp+0x50] rcx
///   [rsp+0x58] rdx
///   [rsp+0x60] rbp
///   [rsp+0x68] rsi
///   [rsp+0x70] rdi
///   [rsp+0x78] vector       (stub pushes immediate)
///   [rsp+0x80] error_code   (stub pushes 0 for no-err, CPU pushes real err)
///   [rsp+0x88] rip          ← CPU-pushed interrupt frame
///   [rsp+0x90] cs
///   [rsp+0x98] rflags
///   [rsp+0xa0] rsp          (only on privilege-level change)
///   [rsp+0xa8] ss           (only on privilege-level change)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// ---- Assembly stubs ----

macro_rules! define_stub_noerr {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push 0",
                "push {vec}",
                "push rdi",
                "push rsi",
                "push rbp",
                "push rdx",
                "push rcx",
                "push rbx",
                "push rax",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "push r12",
                "push r13",
                "push r14",
                "push r15",
                // Set arg1 for BOTH calling conventions:
                //   System V (kernel):  arg1 in RDI
                //   Microsoft x64 (UEFI): arg1 in RCX
                "mov rdi, rsp",
                "mov rcx, rsp",
                "sub rsp, 32",
                "call {dispatcher}",
                "add rsp, 32",
                "pop r15",
                "pop r14",
                "pop r13",
                "pop r12",
                "pop r11",
                "pop r10",
                "pop r9",
                "pop r8",
                "pop rax",
                "pop rbx",
                "pop rcx",
                "pop rdx",
                "pop rbp",
                "pop rsi",
                "pop rdi",
                "add rsp, 16",
                "iretq",
                vec = const $vec,
                dispatcher = sym interrupt_dispatcher,
            );
        }
    };
}

macro_rules! define_stub_err {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push {vec}",
                "push rdi",
                "push rsi",
                "push rbp",
                "push rdx",
                "push rcx",
                "push rbx",
                "push rax",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "push r12",
                "push r13",
                "push r14",
                "push r15",
                // Set arg1 for BOTH calling conventions:
                "mov rdi, rsp",
                "mov rcx, rsp",
                "sub rsp, 32",
                "call {dispatcher}",
                "add rsp, 32",
                "pop r15",
                "pop r14",
                "pop r13",
                "pop r12",
                "pop r11",
                "pop r10",
                "pop r9",
                "pop r8",
                "pop rax",
                "pop rbx",
                "pop rcx",
                "pop rdx",
                "pop rbp",
                "pop rsi",
                "pop rdi",
                "add rsp, 16",
                "iretq",
                vec = const $vec,
                dispatcher = sym interrupt_dispatcher,
            );
        }
    };
}

// Define all stubs
define_stub_noerr!(stub_de, 0);
define_stub_noerr!(stub_db, 1);
define_stub_noerr!(stub_nmi, 2);
define_stub_noerr!(stub_bp, 3);
define_stub_noerr!(stub_of, 4);
define_stub_noerr!(stub_br, 5);
define_stub_noerr!(stub_ud, 6);
define_stub_noerr!(stub_nm, 7);
define_stub_err!(stub_df, 8);
define_stub_noerr!(stub_cop, 9);
define_stub_err!(stub_ts, 10);
define_stub_err!(stub_np, 11);
define_stub_err!(stub_ss, 12);
define_stub_err!(stub_gp, 13);
define_stub_err!(stub_pf, 14);
define_stub_noerr!(stub_mf, 16);
define_stub_err!(stub_ac, 17);
define_stub_noerr!(stub_mc, 18);
define_stub_noerr!(stub_xm, 19);
define_stub_noerr!(stub_ve, 20);
define_stub_err!(stub_cp, 21);
define_stub_noerr!(stub_exc_22, 22);
define_stub_noerr!(stub_exc_23, 23);
define_stub_noerr!(stub_exc_24, 24);
define_stub_noerr!(stub_exc_25, 25);
define_stub_noerr!(stub_exc_26, 26);
define_stub_noerr!(stub_exc_27, 27);
define_stub_noerr!(stub_exc_28, 28);
define_stub_noerr!(stub_exc_29, 29);
define_stub_noerr!(stub_exc_30, 30);
define_stub_noerr!(stub_exc_31, 31);

define_stub_noerr!(stub_irq0, 32);
define_stub_noerr!(stub_irq1, 33);
define_stub_noerr!(stub_irq2, 34);
define_stub_noerr!(stub_irq3, 35);
define_stub_noerr!(stub_irq4, 36);
define_stub_noerr!(stub_irq5, 37);
define_stub_noerr!(stub_irq6, 38);
define_stub_noerr!(stub_irq7, 39);
define_stub_noerr!(stub_irq8, 40);
define_stub_noerr!(stub_irq9, 41);
define_stub_noerr!(stub_irq10, 42);
define_stub_noerr!(stub_irq11, 43);
define_stub_noerr!(stub_irq12, 44);
define_stub_noerr!(stub_irq13, 45);
define_stub_noerr!(stub_irq14, 46);
define_stub_noerr!(stub_irq15, 47);
define_stub_noerr!(stub_irq16, 48);
define_stub_noerr!(stub_irq17, 49);
define_stub_noerr!(stub_irq18, 50);
define_stub_noerr!(stub_irq19, 51);
define_stub_noerr!(stub_irq20, 52);
define_stub_noerr!(stub_irq21, 53);
define_stub_noerr!(stub_irq22, 54);
define_stub_noerr!(stub_irq23, 55);
define_stub_noerr!(stub_irq24, 56);
define_stub_noerr!(stub_irq25, 57);
define_stub_noerr!(stub_irq26, 58);
define_stub_noerr!(stub_irq27, 59);
define_stub_noerr!(stub_irq28, 60);
define_stub_noerr!(stub_irq29, 61);
define_stub_noerr!(stub_irq30, 62);
define_stub_noerr!(stub_irq31, 63);

define_stub_noerr!(stub_spurious, 255);
define_stub_noerr!(stub_syscall, 0x80);

// ---- Static IDT ----

static mut IDT: [IdtEntry; 256] = {
    const EMPTY: IdtEntry = IdtEntry::empty();
    [EMPTY; 256]
};

static mut IDTR: Idtr = Idtr {
    limit: 0,
    base: 0,
};

// ---- IST1 stack for double faults ----

#[repr(C, align(16))]
pub struct AlignedIstStack(pub [u8; 16384]);

pub static mut IST1_STACK: AlignedIstStack = AlignedIstStack([0; 16384]);

// ---- Tick counter ----

static TICKS: AtomicU64 = AtomicU64::new(0);
static PIT_TICKS: AtomicU64 = AtomicU64::new(0);
static KEY_COUNT: AtomicU64 = AtomicU64::new(0);
static KEY_SCANCODE: AtomicU16 = AtomicU16::new(0);

/// Read the current LAPIC timer tick count (safe from any context).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Read the current PIT interrupt count (safe from any context).
pub fn pit_ticks() -> u64 {
    PIT_TICKS.load(Ordering::Relaxed)
}

/// Number of keyboard interrupt events received.
pub fn key_count() -> u64 {
    KEY_COUNT.load(Ordering::Relaxed)
}

/// Latest PS/2 scancode byte received.
pub fn key_scancode() -> u16 {
    KEY_SCANCODE.load(Ordering::Relaxed)
}

// ---- Interrupt enable/disable ----

pub fn enable_interrupts() {
    unsafe { asm!("sti") };
}

pub fn disable_interrupts() {
    unsafe { asm!("cli") };
}

/// Mask all 8259 PIC interrupts.
/// Writes 0xFF to the OCW1 (IMR) registers — no ICW init needed since we
/// use the IOAPIC exclusively and never want a PIC interrupt to fire.
pub fn mask_pic() {
    unsafe {
        asm!("out 0x21, al", in("al") 0xFFu8);
        asm!("out 0xA1, al", in("al") 0xFFu8);
    }
}

// ---- Public init ----

pub fn init() {
    unsafe {
        let idt_base = &raw const IDT as u64;
        IDTR.limit = (mem::size_of::<IdtEntry>() * 256 - 1) as u16;
        IDTR.base = idt_base;

        // Wire exception vectors 0–31
        set_gate(0, stub_de as *const () as u64, 0);
        set_gate(1, stub_db as *const () as u64, 0);
        set_gate(2, stub_nmi as *const () as u64, 0);
        set_gate(3, stub_bp as *const () as u64, 0);
        set_gate(4, stub_of as *const () as u64, 0);
        set_gate(5, stub_br as *const () as u64, 0);
        set_gate(6, stub_ud as *const () as u64, 0);
        set_gate(7, stub_nm as *const () as u64, 0);
        set_gate(8, stub_df as *const () as u64, 1); // IST1 for double fault
        set_gate(9, stub_cop as *const () as u64, 0);
        set_gate(10, stub_ts as *const () as u64, 0);
        set_gate(11, stub_np as *const () as u64, 0);
        set_gate(12, stub_ss as *const () as u64, 0);
        set_gate(13, stub_gp as *const () as u64, 0);
        set_gate(14, stub_pf as *const () as u64, 0);
        set_gate(16, stub_mf as *const () as u64, 0);
        set_gate(17, stub_ac as *const () as u64, 0);
        set_gate(18, stub_mc as *const () as u64, 0);
        set_gate(19, stub_xm as *const () as u64, 0);
        set_gate(20, stub_ve as *const () as u64, 0);
        set_gate(21, stub_cp as *const () as u64, 0);
        set_gate(22, stub_exc_22 as *const () as u64, 0);
        set_gate(23, stub_exc_23 as *const () as u64, 0);
        set_gate(24, stub_exc_24 as *const () as u64, 0);
        set_gate(25, stub_exc_25 as *const () as u64, 0);
        set_gate(26, stub_exc_26 as *const () as u64, 0);
        set_gate(27, stub_exc_27 as *const () as u64, 0);
        set_gate(28, stub_exc_28 as *const () as u64, 0);
        set_gate(29, stub_exc_29 as *const () as u64, 0);
        set_gate(30, stub_exc_30 as *const () as u64, 0);
        set_gate(31, stub_exc_31 as *const () as u64, 0);

        // Wire IRQ vectors 32–63
        set_gate(32, stub_irq0 as *const () as u64, 0);
        set_gate(33, stub_irq1 as *const () as u64, 0);
        set_gate(34, stub_irq2 as *const () as u64, 0);
        set_gate(35, stub_irq3 as *const () as u64, 0);
        set_gate(36, stub_irq4 as *const () as u64, 0);
        set_gate(37, stub_irq5 as *const () as u64, 0);
        set_gate(38, stub_irq6 as *const () as u64, 0);
        set_gate(39, stub_irq7 as *const () as u64, 0);
        set_gate(40, stub_irq8 as *const () as u64, 0);
        set_gate(41, stub_irq9 as *const () as u64, 0);
        set_gate(42, stub_irq10 as *const () as u64, 0);
        set_gate(43, stub_irq11 as *const () as u64, 0);
        set_gate(44, stub_irq12 as *const () as u64, 0);
        set_gate(45, stub_irq13 as *const () as u64, 0);
        set_gate(46, stub_irq14 as *const () as u64, 0);
        set_gate(47, stub_irq15 as *const () as u64, 0);
        set_gate(48, stub_irq16 as *const () as u64, 0);
        set_gate(49, stub_irq17 as *const () as u64, 0);
        set_gate(50, stub_irq18 as *const () as u64, 0);
        set_gate(51, stub_irq19 as *const () as u64, 0);
        set_gate(52, stub_irq20 as *const () as u64, 0);
        set_gate(53, stub_irq21 as *const () as u64, 0);
        set_gate(54, stub_irq22 as *const () as u64, 0);
        set_gate(55, stub_irq23 as *const () as u64, 0);
        set_gate(56, stub_irq24 as *const () as u64, 0);
        set_gate(57, stub_irq25 as *const () as u64, 0);
        set_gate(58, stub_irq26 as *const () as u64, 0);
        set_gate(59, stub_irq27 as *const () as u64, 0);
        set_gate(60, stub_irq28 as *const () as u64, 0);
        set_gate(61, stub_irq29 as *const () as u64, 0);
        set_gate(62, stub_irq30 as *const () as u64, 0);
        set_gate(63, stub_irq31 as *const () as u64, 0);

        // Spurious interrupt vector (0xFF) for LAPIC
        set_gate(255, stub_spurious as *const () as u64, 0);

        // Syscall interrupt gate (vector 0x80)
        set_gate(0x80, stub_syscall as *const () as u64, 0);

        // Set IST1 in TSS for double fault handler
        let ist1_addr = &raw const IST1_STACK.0 as u64 + 16384;
        super::gdt::set_ist1(ist1_addr);

        // Load IDT
        asm!("lidt [{idtr}]", idtr = in(reg) &raw const IDTR);
    }
}

unsafe fn set_gate(vector: usize, handler_addr: u64, ist: u8) {
    IDT[vector] = IdtEntry::interrupt_gate(handler_addr, ist);
}

// ---- Exception handler (called from interrupt_dispatcher) ----

extern "C" fn interrupt_dispatcher(frame: &mut TrapFrame) {
    #[cfg(debug_assertions)]
    {
        let frame_addr = frame as *const TrapFrame as usize;
        assert!(
            frame_addr % 16 == 0,
            "interrupt_dispatcher: TrapFrame misaligned ({:#x} & 0xF = {})",
            frame_addr,
            frame_addr & 0xF
        );
    }
    let vector = frame.vector;
    match vector {
        0..=31 => exception_handler(frame, vector),
        32..=63 => irq_handler(frame, vector),
        0x80 => syscall_handler(frame),
        0xFF => {},
        _ => exception_handler(frame, vector),
    }
}

fn exception_handler(frame: &mut TrapFrame, vector: u64) {
    let rip = frame.rip;
    let rsp = frame.rsp;
    let error = frame.error_code;

    let mut resolved = false;

    log::error!("EXCEPTION #{} at RIP={:#x}", vector, rip);

    match vector {
        0 => log::error!("#DE Divide Error"),
        1 => log::error!("#DB Debug Exception"),
        2 => log::error!("NMI"),
        3 => log::info!("#BP Breakpoint at RIP={:#x}", rip),
        4 => log::error!("#OF Overflow"),
        5 => log::error!("#BR Bound Range Exceeded"),
        6 => log::error!("#UD Invalid Opcode"),
        7 => log::error!("#NM Device Not Available"),
        8 => {
            log::error!("#DF DOUBLE FAULT at RIP={:#x} RSP={:#x} err={}", rip, rsp, error);
            halt_loop();
        }
        10 => log::error!("#TS Invalid TSS err={:#x}", error),
        11 => log::error!("#NP Segment Not Present err={:#x}", error),
        12 => log::error!("#SS Stack Segment Fault err={:#x}", error),
        13 => {
            log::error!("#GP General Protection Fault err={:#x}", error);
            if error != 0 {
                let index = (error >> 3) & 0xFFFF;
                let ti = (error >> 1) & 1;
                log::error!("  selector index={} TI={}", index, ti);
            }
        }
        14 => {
            let cr2: u64;
            unsafe { asm!("mov {cr2}, cr2", cr2 = out(reg) cr2) };
            log::error!("#PF Page Fault at CR2={:#x}", cr2);
            log::error!("  err={:#x} {} {} {}", error,
                if error & (1 << 1) != 0 { "Write" } else { "Read" },
                if error & (1 << 2) != 0 { "User" } else { "Kernel" },
                if error & 1 != 0 { "PRESENT" } else { "NOT-PRESENT" },
            );
            resolved = crate::mm::vma::handle_page_fault(cr2, error);
            if resolved {
                log::info!("  -> Resolved via demand paging");
            } else {
                log::error!("  -> Unresolved page fault");
            }
        }
        16 => log::error!("#MF x87 FPU Error"),
        17 => log::error!("#AC Alignment Check"),
        18 => log::error!("#MC Machine Check"),
        19 => log::error!("#XM SIMD Exception"),
        20 => log::error!("#VE Virtualization Exception"),
        21 => log::error!("#CP Control Protection"),
        v @ 22..=31 => log::error!("Reserved exception #{}", v),
        _ => log::error!("Unknown exception #{}", vector),
    }

    if !resolved {
        log::error!("  RAX={:#x} RBX={:#x} RCX={:#x} RDX={:#x}",
            frame.rax, frame.rbx, frame.rcx, frame.rdx);
        log::error!("  RSI={:#x} RDI={:#x} RBP={:#x} RSP={:#x}",
            frame.rsi, frame.rdi, frame.rbp, rsp);
        log::error!("  R8={:#x}  R9={:#x}  R10={:#x} R11={:#x}",
            frame.r8, frame.r9, frame.r10, frame.r11);
        log::error!("  R12={:#x} R13={:#x} R14={:#x} R15={:#x}",
            frame.r12, frame.r13, frame.r14, frame.r15);
        log::error!("  RIP={:#x} CS={:#x} RFLAGS={:#x}",
            rip, frame.cs, frame.rflags);

        if vector != 3 {
            halt_loop();
        }
    }
}

// ---- IRQ handler (called from interrupt_dispatcher) ----

fn irq_handler(frame: &mut TrapFrame, vector: u64) {
    if super::apic::is_initialized() {
        super::apic::send_eoi();
    }

    match vector {
        32 => {
            // LAPIC timer — scheduler heartbeat
            TICKS.fetch_add(1, Ordering::Relaxed);

            // Preemptive context switch
            if crate::task::is_initialized() {
                if crate::task::schedule(frame) {
                    let new_rsp = frame.rsp;
                    let rip = frame.rip;
                    let cs = frame.cs;
                    let rflags = frame.rflags;
                    log::info!("sched: switch RSP={:#x} RIP={:#x}", new_rsp, rip);
                    unsafe {
                        // Use popfq + retfq instead of iretq to avoid the strict
                        // CS-descriptor checks that iretq enforces (canonicality,
                        // DPL vs current CPL, etc.) — these checks sometimes reject
                        // a perfectly valid 0x08 selector when reached via synthetic
                        // frame on a different privilege-level path.
                        core::arch::asm!(
                            "mov rsp, {rsp}",
                            "push {cs}",
                            "push {rip}",
                            "push {rflags}",
                            "popfq",
                            "retfq",
                            rsp = in(reg) new_rsp,
                            rip = in(reg) rip,
                            cs = in(reg) cs,
                            rflags = in(reg) rflags,
                            options(noreturn)
                        );
                    }
                }
            }
        }
        _ => {
            // Device IRQs — look up by vector to find ISA source
            if let Some(isa_source) = crate::intr::lookup_vector_isa(vector as u8) {
                match isa_source {
                    0 => {
                        // PIT channel 0
                        PIT_TICKS.fetch_add(1, Ordering::Relaxed);
                    }
                    1 => {
                        // PS/2 keyboard
                        let sc: u8;
                        unsafe { core::arch::asm!("in al, dx", in("dx") 0x60u16, out("al") sc) };
                        KEY_SCANCODE.store(sc as u16, Ordering::Relaxed);
                        KEY_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---- Syscall handler (vector 0x80) ----
//
// Convention:
//   rax = syscall number
//   rdi, rsi, rdx = arguments
//   return value in rax
//
// Syscalls:
//   0 = yield (nop, preemptive timer handles scheduling)
//   1 = exit (block current task)
//   2 = get_task_id
//   3 = wake_task(task_id)
//   4 = get_ticks

fn syscall_handler(frame: &mut TrapFrame) {
    let nr = frame.rax;
    let arg0 = frame.rdi;

    match nr {
        0 => {
            // yield — with preemptive scheduling this is a no-op;
            // the task will be rescheduled on the next timer tick.
        }
        1 => {
            // exit — block this task and reschedule immediately
            if crate::task::is_initialized() {
                crate::task::block_current(frame);
            }
        }
        2 => {
            // get_task_id
            frame.rax = crate::task::current_task_id() as u64;
        }
        3 => {
            // wake_task(task_id)
            if crate::task::is_initialized() {
                crate::task::wake(arg0 as usize);
            }
        }
        4 => {
            // get_ticks
            frame.rax = TICKS.load(Ordering::Relaxed);
        }
        _ => {
            log::warn!("syscall: unknown nr={}", nr);
            frame.rax = u64::MAX; // error
        }
    }
}

fn halt_loop() -> ! {
    log::error!("System halted.");
    loop {
        unsafe { asm!("cli; hlt") };
    }
}
