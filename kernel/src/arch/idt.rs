#![allow(dead_code)]

use core::arch::{asm, naked_asm};
use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::SyncUnsafeCell;

const KERNEL_CODE_SEL: u16 = super::gdt::KERNEL_CODE_SEL;

/// IDTR register value — 10 bytes: 2-byte limit + 8-byte base.
/// Must remain packed to match the CPU's `lidt` operand format.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 64-bit IDT gate descriptor — 16 bytes.
/// Must remain packed to match the CPU's interrupt gate descriptor format.
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
define_stub_noerr!(stub_ipi, 0x81);

// ---- Static IDT ----

static IDT: SyncUnsafeCell<[IdtEntry; 256]> = SyncUnsafeCell::new({
    const EMPTY: IdtEntry = IdtEntry::empty();
    [EMPTY; 256]
});

use lodaxos_system::MAX_CPUS;

/// Per-CPU IDTR. Although the IDT contents are shared (handlers are
/// CPU-agnostic), the IDTR register itself must be loaded by each CPU.
/// On x86, the IDTR is a CPU-local register — every CPU can have a
/// different IDT base.  We share the IDT contents but give each CPU
/// its own IDTR slot so the SMP trampoline can `lidt` to a per-CPU
/// pointer (the trampoline mailbox stores the IDT pointer directly,
/// and the AP loads it without further translation).
static IDTR_TABLE: SyncUnsafeCell<[Idtr; MAX_CPUS]> = SyncUnsafeCell::new([const { Idtr { limit: 0, base: 0 } }; MAX_CPUS]);

/// Return the higher-half virtual address of the IDT pointer for `slot`.
pub fn idt_pointer_for_slot(slot: usize) -> u64 {
    let slot = slot % MAX_CPUS;
    unsafe { &raw const (*IDTR_TABLE.get())[slot] as u64 }
}

/// Return the limit and base of the IDT pointer for `slot`.
/// Used by the SMP init code to copy the IDT pointer into the
/// SIPI mailbox.
pub fn idt_ptr_limit_base(slot: usize) -> (u16, u64) {
    let slot = slot % MAX_CPUS;
    unsafe { ((*IDTR_TABLE.get())[slot].limit, (*IDTR_TABLE.get())[slot].base) }
}

/// Backwards-compat alias (returns the BSP's IDTR pointer address).
/// TODO: remove once all call sites use `idt_pointer_for_slot` directly.
pub fn idt_pointer_address() -> u64 {
    idt_pointer_for_slot(0)
}

// ---- Tick counter ----

static TICKS: AtomicU64 = AtomicU64::new(0);
static PIT_TICKS: AtomicU64 = AtomicU64::new(0);

/// Debug counter for verifying the timer ISR fires after context switches.
static TIMER_FIRED_ON_BSP: AtomicU64 = AtomicU64::new(0);

/// Read the current LAPIC timer tick count (safe from any context).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Increment the LAPIC timer tick counter. Returns the new value.
/// Used by the per-CPU wrapper in `kernel/src/percpu.rs` to funnel all
/// increments through this single source of truth.
pub fn tick() -> u64 {
    TICKS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Read the current PIT interrupt count (safe from any context).
pub fn pit_ticks() -> u64 {
    PIT_TICKS.load(Ordering::Relaxed)
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
    use x86_64::instructions::port::Port;
    unsafe {
        Port::new(0x21).write(0xFFu8);
        Port::new(0xA1).write(0xFFu8);
    }
}

// ---- Public init ----

pub fn init() {
    unsafe {
        let idt_base = IDT.get() as *const IdtEntry as u64;
        for slot in 0..MAX_CPUS {
            (*IDTR_TABLE.get())[slot].limit = (mem::size_of::<IdtEntry>() * 256 - 1) as u16;
            (*IDTR_TABLE.get())[slot].base = idt_base;
        }

        // Wire exception vectors 0–31
        set_gate(0, stub_de as *const () as u64, 0);
        set_gate(1, stub_db as *const () as u64, 0);
        set_gate(2, stub_nmi as *const () as u64, 3); // IST3 for NMI (Bug 33)
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
        set_gate(32, stub_irq0 as *const () as u64, 2); // IST2
        set_gate(33, stub_irq1 as *const () as u64, 2);
        set_gate(34, stub_irq2 as *const () as u64, 2);
        set_gate(35, stub_irq3 as *const () as u64, 2);
        set_gate(36, stub_irq4 as *const () as u64, 2);
        set_gate(37, stub_irq5 as *const () as u64, 2);
        set_gate(38, stub_irq6 as *const () as u64, 2);
        set_gate(39, stub_irq7 as *const () as u64, 2);
        set_gate(40, stub_irq8 as *const () as u64, 2);
        set_gate(41, stub_irq9 as *const () as u64, 2);
        set_gate(42, stub_irq10 as *const () as u64, 2);
        set_gate(43, stub_irq11 as *const () as u64, 2);
        set_gate(44, stub_irq12 as *const () as u64, 2);
        set_gate(45, stub_irq13 as *const () as u64, 2);
        set_gate(46, stub_irq14 as *const () as u64, 2);
        set_gate(47, stub_irq15 as *const () as u64, 2);
        set_gate(48, stub_irq16 as *const () as u64, 2);
        set_gate(49, stub_irq17 as *const () as u64, 2);
        set_gate(50, stub_irq18 as *const () as u64, 2);
        set_gate(51, stub_irq19 as *const () as u64, 2);
        set_gate(52, stub_irq20 as *const () as u64, 2);
        set_gate(53, stub_irq21 as *const () as u64, 2);
        set_gate(54, stub_irq22 as *const () as u64, 2);
        set_gate(55, stub_irq23 as *const () as u64, 2);
        set_gate(56, stub_irq24 as *const () as u64, 2);
        set_gate(57, stub_irq25 as *const () as u64, 2);
        set_gate(58, stub_irq26 as *const () as u64, 2);
        set_gate(59, stub_irq27 as *const () as u64, 2);
        set_gate(60, stub_irq28 as *const () as u64, 2);
        set_gate(61, stub_irq29 as *const () as u64, 2);
        set_gate(62, stub_irq30 as *const () as u64, 2);
        set_gate(63, stub_irq31 as *const () as u64, 2);

        // Spurious interrupt vector
        set_gate(255, stub_spurious as *const () as u64, 2); // IST2

        // IPI vector (cross-CPU wake / reschedule)
        set_gate(0x81, stub_ipi as *const () as u64, 2); // IST2

        // IST1/IST2 are set per-CPU via gdt::init_for_slot / gdt::init_tss_descriptor_for_slot,
        // which use per-CPU stacks (gdt::IST1_STACKS / gdt::IST2_STACKS), NOT this shared one.

        // Load IDT (BSP slot).
        asm!("lidt [{idtr}]", idtr = in(reg) IDTR_TABLE.get() as *const Idtr);
    }
}

unsafe fn set_gate(vector: usize, handler_addr: u64, ist: u8) {
    (*IDT.get())[vector] = IdtEntry::interrupt_gate(handler_addr, ist);
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

    // Process any pending TLB flush from a timed-out shootdown.
    let cpu_slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());

    // Save CR3 BEFORE any scheduler activity.  The fault diagnostic dump
    // needs the faulting context's page tables, not the (possibly switched)
    // current CR3.
    {
        let cr3: u64;
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
        crate::percpu::PERCPU[cpu_slot].saved_cr3.store(cr3, Ordering::Relaxed);
    }

    let pending = crate::percpu::PERCPU[cpu_slot].pending_tlb_flush.swap(0, Ordering::Acquire);
    if pending == u64::MAX {
        // Full TLB flush was requested (multiple addresses were pending
        // and at least one was overwritten — Bug 12 fix).
        unsafe { core::arch::asm!("mov rax, cr3; mov cr3, rax") };
    } else if pending != 0 {
        unsafe { core::arch::asm!("invlpg [{}]", in(reg) pending) };
    }

    let vector = frame.vector;

    // NMI (vector 2) must be handled with minimal, lock‑free code
    // because it can fire during any critical section (Bug 33).
    if vector == 2 {
        nmi_counter();
        return;
    }

    match vector {
        0..=31 => exception_handler(frame, vector),
        32..=63 => irq_handler(frame, vector),
        0x81 => ipi_handler(frame),
        0xFF => {},
        _ => exception_handler(frame, vector),
    }
}

fn exception_handler(frame: &mut TrapFrame, vector: u64) {
    let rip = frame.rip;
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

    if !resolved && vector != 3 {
        // Check if the faulting vCPU belongs to a GDF service
        let vcpu_id = crate::scheduler::current_vcpu_id();
        let vcpu_type = crate::vcpu::get_vcpu_type(vcpu_id);
        if vcpu_type == VcpuType::HardwareDriver || vcpu_type == VcpuType::AbstractionDriver {
            log::warn!("GDF service crashed — attempting restart");
            crate::gdf::handle_crash(vcpu_id);
            crate::gdf::switch_frame_to_idle(frame);
            // frame is now the idle vCPU's frame — iretq goes to idle loop
            return;
        }
        super::dump::dump_full_fault(frame, vector);
        super::dump::halt_loop();
    }
}

// ---- IRQ handler (called from interrupt_dispatcher) ----

fn irq_handler(frame: &mut TrapFrame, vector: u64) {
    // Send EOI immediately for non-timer IRQs.
    // For the LAPIC timer (vector 32), we defer EOI until after the
    // scheduler runs to prevent a nested timer interrupt on IST2
    // from overwriting the scheduler's stack frame (Bug 26).
    if vector != 32 && super::apic::is_initialized() {
        super::apic::send_eoi();
    }

    match vector {
            32 => {
            // LAPIC timer — scheduler heartbeat
            let t = crate::percpu::tick();
            let cpu = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
            // Per-CPU rate-limited timer log.  Each CPU logs every
            // ~200 ticks so the output doesn't saturate the serial port.
            let fires = crate::percpu::PERCPU[cpu].timer_fires.fetch_add(1, Ordering::Relaxed) + 1;
            if fires % 200 == 1 {
                log::info!(
                    "[timer] cpu={} tick={} percpu_tick={}",
                    cpu, fires, t
                );
            }

            // Preemptive context switch — every CPU runs the scheduler
            // on each timer tick.
            if crate::scheduler::is_initialized() {
                // FPU buffer owned by this scope — fixes Bug: dangling pointer
                // when schedule() returned a pointer to its own local.
                let mut fpu_buf = crate::arch::FpuState([0u8; 512]);
                // Initialise with a clean FPU state so idle gets sane defaults.
                unsafe { core::arch::asm!("fninit", "fxsave [{}]", in(reg) fpu_buf.0.as_mut_ptr()); }
                let (switched, next_pml4) = crate::scheduler::schedule(frame, &mut fpu_buf);
                // EOI after scheduler runs so periodic timer doesn't
                // re-arm before the scheduler finishes (Bug 26).
                if super::apic::is_initialized() {
                    super::apic::send_eoi();
                }
                if switched {
                    log::info!("sched: switch RSP={:#x} RIP={:#x} CS={:#x}", frame.rsp, frame.rip, frame.cs);
                    unsafe { crate::arch::context_switch(frame, next_pml4, &fpu_buf); }
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
                    #[cfg(debug_assertions)]
                    3 => {
                        // COM2 serial
                        crate::serial2::irq_handler();
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---- Syscall dispatcher ───────────────────────────────────────────
//
// Entry via `syscall` instruction.  IA32_LSTAR points here.
// Syscall numbers and access by VcpuType:
//
//  Nr │ Name             │ N  HW  AB  │ Description
// ─────┼──────────────────┼────────────┼─────────────────────────
//   0  │ yield            │ ✓  ✓  ✓   │ Yield vCPU
//   1  │ exit             │ ✓  ✓  ✓   │ Halt vCPU
//   2  │ get_vcpu_id      │ ✓  ✓  ✓   │ Return VcpuId
//   3  │ wake             │ ✓  ✓  ✓   │ Wake another vCPU
//   4  │ get_ticks        │ ✓  ✓  ✓   │ Uptime ticks
//   5  │ mmap             │ ✓  ✓  ✓   │ Allocate + map pages
//   6  │ munmap           │ ✓  ✓  ✓   │ Unmap + free pages
//   7  │ create_gang      │ ✓  ✓  ✓   │ Spawn new vCPU gang
// ─────┼──────────────────┼────────────┼─────────────────────────
//  10  │ mmap_phys        │    ✓      │ Map MMIO, always UC
//  11  │ register_intr    │    ✓      │ Register IRQ handler
//  12  │ intr_ack         │    ✓      │ EOI
//  13  │ dma_alloc        │    ✓      │ Allocate DMA buffer
//  14  │ dma_free         │    ✓      │ Free DMA buffer
//  15  │ pci_config       │    ✓      │ PCI config space R/W
// ─────┼──────────────────┼────────────┼─────────────────────────
//  20  │ driver_recv      │       ✓  ✓│ Read kernel→driver mailbox
//  21  │ driver_send      │       ✓  ✓│ Write driver→kernel response
//  30  │ gdf_register     │       ✓  ✓│ Register driver name

use crate::vcpu::VcpuType;

fn current_vcpu_type() -> VcpuType {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    crate::vcpu::get_vcpu_type(vcpu_id)
}

/// Copy bytes from userspace (lower half) into a kernel buffer.
/// Validates the source range is below HIGHER_HALF.
fn copy_from_user(src: u64, dst: &mut [u8]) -> Result<(), ()> {
    let len = dst.len();
    if len == 0 {
        return Ok(());
    }
    let end = src.checked_add(len as u64).ok_or(())?;
    if src >= crate::mm::virt::HIGHER_HALF || end > crate::mm::virt::HIGHER_HALF {
        return Err(());
    }
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), len);
    }
    Ok(())
}

/// Write to user memory from kernel memory, validating the user buffer is
/// mapped writable before touching it (Bug 31).
fn copy_to_user(src: &[u64], dst: u64) -> Result<(), ()> {
    let len_bytes = (src.len() * 8) as u64;
    if len_bytes == 0 {
        return Ok(());
    }
    let end = dst.checked_add(len_bytes).ok_or(())?;
    if dst >= crate::mm::virt::HIGHER_HALF || end > crate::mm::virt::HIGHER_HALF {
        return Err(());
    }
    // Probe each 4 KB page for writability by checking the PTE.
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
    let mut addr = dst;
    while addr < end {
        if let Some(pte) = crate::mm::virt::read_pte(cr3 & !0xFFF, addr) {
            if pte & 1 == 0 || pte & 2 == 0 {
                return Err(());  // not present or not writable
            }
        } else {
            return Err(());
        }
        addr = (addr & !0xFFF) + 0x1000;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u64, src.len());
    }
    Ok(())
}

#[unsafe(naked)]
pub extern "C" fn syscall_entry() {
    naked_asm!(
        // RCX = return RIP, R11 = saved RFLAGS (set by SYSCALL instruction)
        //
        // IMPORTANT: SYSCALL does NOT change RSP.  If we came from user
        // mode (CPL3), RSP is the user stack — we must switch to the
        // per-CPU kernel stack.  If we came from kernel code (CPL0,
        // e.g. `yield_now` from the syscall handler itself), RSP is
        // already the kernel stack.
        //
        // Detect user mode: user RSP is in the lower half (bit 47 = 0),
        // kernel RSP is in the higher half (bit 47 = 1).
        "mov r10, rsp",
        "mov rax, r10",
        "shr rax, 47",
        "test rax, rax",
        "jnz 1f",
        // User-mode: switch to per-CPU kernel stack (GS:[kernel_stack_top])
        "mov rsp, gs:[8]",
        "test rsp, rsp",
        "cmovz rsp, r10",
        "1:",
        "and rsp, -16",
        // Push TrapFrame in reverse field order (ss..r15) so that RSP
        // ends up pointing to TrapFrame.r15 (struct base, offset 0).
        // This matches the #[repr(C)] struct layout expected by the handler.
        // Default SS=RPL3 for user-mode return; the handler fixes up to
        // kernel CS/SS if the syscall originated from kernel code (yield).
        "push 0x23",
        "push r10",
        "push r11",
        "push 0x1B",
        "push rcx",
        "push 0x80",
        "push 0",
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
        // RSP == &TrapFrame.r15 now
        "mov rdi, rsp",
        "mov r15, rsp",
        "sub rsp, 32",
        "call {handler}",
        "add rsp, 32",
        "mov rsp, r15",
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
        handler = sym syscall_handler,
    );
}

#[inline(never)]
fn syscall_handler(frame: &mut TrapFrame) {
    // Sanitize RFLAGS: clear NT (bit 14), RF (bit 16), and reserved
    // bits so iretq cannot attempt a task switch (#GP) or cause other
    // undefined behaviour (Bug 32).
    frame.rflags &= 0x3000_00D7u64;

    // Fix up CS/SS based on original caller privilege.
    // The syscall_entry always pushes CPL3 values (0x1B/0x23) as default.
    // If the saved RSP is in the kernel higher half, the caller was CPL0
    // (e.g., yield from kernel code), so we must return to CPL0.
    if frame.rsp >= crate::mm::virt::HIGHER_HALF {
        frame.cs = 0x08;
        frame.ss = 0x10;
    } else {
        frame.cs = 0x1B;
        frame.ss = 0x23;
    }

    let nr = frame.rax;
    let vcpu_type = current_vcpu_type();

    // ── Access control by (VcpuType, syscall_nr) ──
    let allowed = match (vcpu_type, nr) {
        // Idle vCPU should never syscall
        (VcpuType::Idle, _) => false,

        // Universal (0-7): ALL non-idle types
        (_, 0..=7) => true,

        // Hardware driver only (10-15)
        (VcpuType::HardwareDriver, 10..=15) => true,

        // Driver IPC (20-22, 30-31): HardwareDriver + AbstractionDriver
        (VcpuType::HardwareDriver | VcpuType::AbstractionDriver, 20..=22) => true,
        (VcpuType::HardwareDriver | VcpuType::AbstractionDriver, 30..=31) => true,

        _ => false,
    };

    if !allowed {
        if vcpu_type != VcpuType::Idle {
            log::warn!(
                "syscall denied: vcpu_type={:?} nr={}",
                vcpu_type, nr
            );
        }
        frame.rax = u64::MAX;
        return;
    }

    let arg0 = frame.rdi;
    let arg1 = frame.rsi;
    let arg2 = frame.rdx;

    match nr {
        0 => sys_yield(frame),
        1 => sys_exit(frame),
        2 => sys_get_vcpu_id(frame),
        3 => sys_wake_vcpu(arg0),
        4 => sys_get_ticks(frame),
        5 => sys_mmap(frame, arg0, arg1),
        6 => sys_munmap(frame, arg0, arg1),
        7 => sys_create_gang(frame, arg0, arg1, arg2),

        10 => sys_mmap_phys(frame, arg0, arg1),
        11 => sys_register_intr(frame, arg0, arg1),
        12 => sys_intr_ack(frame, arg0),
        13 => sys_dma_alloc(frame, arg0),
        14 => sys_dma_free(frame, arg0, arg1),
        15 => sys_pci_config(frame, arg0, arg1, arg2, frame.r10, frame.r8),

        20 => sys_driver_recv(frame, arg0),
        21 => sys_driver_send(frame, arg0),
        22 => sys_driver_recv_block(frame, arg0),

        30 => sys_gdf_register(frame, arg0, arg1),
        31 => sys_driver_call(frame, arg0, arg1, arg2, frame.r10, frame.r8, frame.r9),

        _ => unreachable!(),
    }
}

// ── Universal syscalls (0-7) ──────────────────────────────────────

fn sys_yield(frame: &mut TrapFrame) {
    if crate::scheduler::is_initialized() {
        crate::scheduler::block_current(frame);
    }
}

#[inline(never)]
fn sys_exit(frame: &mut TrapFrame) {
    if crate::scheduler::is_initialized() {
        crate::scheduler::block_current(frame);
    }
}

fn sys_get_vcpu_id(frame: &mut TrapFrame) {
    frame.rax = crate::scheduler::current_vcpu_id() as u64;
}

fn sys_wake_vcpu(vcpu_id: u64) {
    if crate::scheduler::is_initialized() {
        crate::scheduler::wake(vcpu_id);
    }
}

fn sys_get_ticks(frame: &mut TrapFrame) {
    frame.rax = TICKS.load(Ordering::Relaxed);
}

fn sys_mmap(frame: &mut TrapFrame, hint: u64, size: u64) {
    if !crate::scheduler::is_initialized() {
        frame.rax = u64::MAX;
        return;
    }
    let size = size.max(0x1000);
    let pages = (size + 0xFFF) / 0x1000;
    let phys = match crate::mm::phys::alloc_pages(pages) {
        Some(p) => p,
        None => { frame.rax = u64::MAX; return; }
    };
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3) };
    let virt = if hint != 0 {
        if hint >= crate::mm::virt::HIGHER_HALF {
            crate::mm::phys::free_pages(phys, pages);
            frame.rax = u64::MAX;
            return;
        }
        hint
    } else {
        crate::mm::virt::HIGHER_HALF + phys
    };
    let map_flags = crate::mm::virt::DATA | crate::mm::virt::USER;
    unsafe { crate::mm::virt::map_contiguous(cr3 & !0xFFF, virt, phys, pages, map_flags) };
    frame.rax = virt;
}

fn sys_munmap(frame: &mut TrapFrame, addr: u64, size: u64) {
    if !crate::scheduler::is_initialized() {
        frame.rax = u64::MAX;
        return;
    }
    if addr >= crate::mm::virt::HIGHER_HALF {
        frame.rax = u64::MAX;
        return;
    }
    let size = size.max(0x1000);
    let pages = (size + 0xFFF) / 0x1000;
    for p in 0..pages {
        let virt = addr + p * 0x1000;
        if let Some(phys) = crate::mm::virt::translate(virt) {
            let phys_page = phys & !0xFFF;
            crate::mm::virt::unmap(virt);
            crate::mm::phys::free_page(phys_page);
        } else {
            crate::mm::virt::unmap(virt);
        }
    }
    frame.rax = 0;
}

fn sys_create_gang(frame: &mut TrapFrame, entry: u64, n_vcpus: u64, name_ptr: u64) {
    if !crate::scheduler::is_initialized() {
        frame.rax = u64::MAX;
        return;
    }
    
    // Copy name from user
    let mut name = [0u8; 32];
    let _len = 32; // Assuming name is passed as 32-byte struct or similar
    if copy_from_user(name_ptr, &mut name).is_err() {
        frame.rax = u64::MAX;
        return;
    }

    frame.rax = crate::scheduler::create_gang(n_vcpus.max(1) as u32, entry, 0, &name, None)
        .map(|id| id as u64)
        .unwrap_or(u64::MAX);
}

// ── Hardware driver syscalls (10-15) ──────────────────────────────

#[inline(never)]
fn sys_mmap_phys(frame: &mut TrapFrame, phys: u64, size: u64) {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    let pml4 = match crate::vcpu::get(vcpu_id) {
        Some(vcpu) => vcpu.pml4,
        None => { frame.rax = u64::MAX; return; }
    };
    let pages = size.max(0x1000).saturating_add(0xFFF) / 0x1000;
    let flags = crate::mm::virt::PRESENT
        | crate::mm::virt::WRITABLE
        | crate::mm::virt::CACHE_DISABLE
        | crate::mm::virt::NO_EXECUTE
        | crate::mm::virt::USER;
    unsafe {
        crate::mm::virt::map_contiguous(pml4, phys, phys, pages, flags);
    }
    crate::gdf::track_service_mmio(vcpu_id, phys, phys, pages);
    frame.rax = phys;
}

fn sys_register_intr(frame: &mut TrapFrame, vector: u64, _handler_vaddr: u64) {
    // Stub — interrupt routing will be implemented with the IOAPIC driver.
    // For now, reserve the vector and acknowledge.
    log::warn!("sys_register_intr: not yet implemented (vector={})", vector);
    frame.rax = u64::MAX;
}

fn sys_intr_ack(frame: &mut TrapFrame, _vector: u64) {
    crate::arch::apic::send_eoi();
    frame.rax = 0;
}

fn sys_dma_alloc(frame: &mut TrapFrame, size: u64) {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    let pages = size.max(0x1000).saturating_add(0xFFF) / 0x1000;
    match crate::mm::phys::alloc_pages(pages) {
        Some(phys) => {
            unsafe {
                core::ptr::write_bytes(
                    (crate::mm::virt::HIGHER_HALF + phys) as *mut u8,
                    0,
                    (pages * 0x1000) as usize,
                );
            }
            crate::gdf::track_service_dma(vcpu_id, phys, pages);
            frame.rax = phys;
        }
        None => {
            frame.rax = u64::MAX;
        }
    }
}

fn sys_dma_free(frame: &mut TrapFrame, phys: u64, size: u64) {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    let pages = size.max(0x1000).saturating_add(0xFFF) / 0x1000;
    crate::gdf::untrack_service_dma(vcpu_id, phys);
    crate::mm::phys::free_pages(phys, pages);
    frame.rax = 0;
}

fn sys_pci_config(
    frame: &mut TrapFrame,
    bdf: u64,
    offset: u64,
    width: u64,
    value: u64,
    is_write: u64,
) {
    // Validate inputs (Bug 30)
    if bdf > 0xFFFF || offset > 0xFF || (width != 1 && width != 2 && width != 4) {
        frame.rax = u64::MAX;
        return;
    }
    let addr = 0x8000_0000u32 | ((bdf as u32) << 8) | (offset as u32 & 0xFC);
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") addr);

        if is_write != 0 {
            match width {
                1 => {
                    let v = value as u8;
                    core::arch::asm!("out dx, al", in("dx") 0xCFCu16, in("al") v);
                }
                2 => {
                    let v = value as u16;
                    core::arch::asm!("out dx, ax", in("dx") 0xCFCu16, in("ax") v);
                }
                4 => {
                    let v = value as u32;
                    core::arch::asm!("out dx, eax", in("dx") 0xCFCu16, in("eax") v);
                }
                _ => { frame.rax = u64::MAX; return; }
            }
            frame.rax = 0;
        } else {
            let result: u64;
            match width {
                1 => {
                    let r: u8;
                    core::arch::asm!("in al, dx", in("dx") 0xCFCu16, out("al") r);
                    result = r as u64;
                }
                2 => {
                    let r: u16;
                    core::arch::asm!("in ax, dx", in("dx") 0xCFCu16, out("ax") r);
                    result = r as u64;
                }
                4 => {
                    let r: u32;
                    core::arch::asm!("in eax, dx", in("dx") 0xCFCu16, out("eax") r);
                    result = r as u64;
                }
                _ => { frame.rax = u64::MAX; return; }
            }
            frame.rax = result;
        }
    }
}

// ── Driver IPC syscalls (20-21, 30) ───────────────────────────────

fn sys_driver_recv(frame: &mut TrapFrame, buf_ptr: u64) {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    match crate::gdf::recv(vcpu_id) {
        Some((cmd, arg0, arg1, arg2)) => {
            let out = [cmd as u64, arg0, arg1, arg2];
            if copy_to_user(&out, buf_ptr).is_ok() {
                frame.rax = 0;
            } else {
                frame.rax = u64::MAX;
            }
        }
        None => {
            frame.rax = u64::MAX;
        }
    }
}

fn sys_driver_send(frame: &mut TrapFrame, result: u64) {
    let vcpu_id = crate::scheduler::current_vcpu_id();
    if crate::gdf::send_response(vcpu_id, result) {
        frame.rax = 0;
    } else {
        frame.rax = u64::MAX;
    }
}

#[inline(never)]
fn sys_driver_recv_block(frame: &mut TrapFrame, buf_ptr: u64) {
    if buf_ptr >= crate::mm::virt::HIGHER_HALF {
        frame.rax = u64::MAX;
        return;
    }
    let vcpu_id = crate::scheduler::current_vcpu_id();

    // Fast path: message already pending
    if let Some((cmd, arg0, arg1, arg2)) = crate::gdf::recv(vcpu_id) {
        let out = [cmd as u64, arg0, arg1, arg2];
        if copy_to_user(&out, buf_ptr).is_ok() {
            frame.rax = 0;
        } else {
            frame.rax = u64::MAX;
        }
        return;
    }

    // Slow path: block until a message arrives.
    // We do NOT use blocked_buf_ptr to avoid a lost-wakeup race
    // (Bug 27): send_cmd could write to the buffer and call wake()
    // before block_current sets state to Halted.
    frame.rax = 0;
    crate::scheduler::block_current(frame);

    // On wake — read the pending message from the mailbox.
    if let Some((cmd, arg0, arg1, arg2)) = crate::gdf::recv(vcpu_id) {
        let out = [cmd as u64, arg0, arg1, arg2];
        if copy_to_user(&out, buf_ptr).is_ok() {
            frame.rax = 0;
        } else {
            frame.rax = u64::MAX;
        }
    } else {
        frame.rax = u64::MAX;
    }
}

fn sys_driver_call(frame: &mut TrapFrame, name_ptr: u64, name_len: u64, cmd: u64, target_arg0: u64, target_arg1: u64, target_arg2: u64) {
    if name_len > 31 || name_len == 0 {
        frame.rax = u64::MAX;
        return;
    }
    let mut name = [0u8; 32];
    if copy_from_user(name_ptr, &mut name[..name_len as usize]).is_err() {
        frame.rax = u64::MAX;
        return;
    }
    if crate::gdf::driver_call(&name[..name_len as usize], cmd as u32, target_arg0, target_arg1, target_arg2, frame) {
        // rax was set by driver_call / send_response via saved_frame
    } else {
        frame.rax = u64::MAX;
    }
}

#[inline(never)]
fn sys_gdf_register(frame: &mut TrapFrame, name_ptr: u64, name_len: u64) {
    if name_len > 31 || name_len == 0 {
        frame.rax = u64::MAX;
        return;
    }
    let mut name = [0u8; 32];
    if copy_from_user(name_ptr, &mut name[..name_len as usize]).is_err() {
        frame.rax = u64::MAX;
        return;
    }
    let vcpu_id = crate::scheduler::current_vcpu_id();
    let vcpu_type = crate::vcpu::get_vcpu_type(vcpu_id);
    if crate::gdf::register_driver(&name[..name_len as usize], vcpu_id, vcpu_type) {
        frame.rax = 0;
    } else {
        frame.rax = u64::MAX;
    }
}

/// IPI vector number used for cross-CPU wake / reschedule requests.
pub const IPI_VECTOR: u8 = 0x81;

/// Flag set by the IPI handler; the timer ISR checks it to decide
/// whether to reschedule after an IPI wake.
pub(crate) static IPI_PENDING: AtomicU64 = AtomicU64::new(0);

fn ipi_handler(_frame: &mut TrapFrame) {
    // Mark that an IPI was received so the next timer tick knows to
    // re-evaluate the runqueue.  We don't reschedule *here* because
    // the IPI may have interrupted a critical section.
    IPI_PENDING.store(1, Ordering::Release);

    // TLB shootdown: if TLB_FLUSH_ADDR is non-zero, execute invlpg.
    let flush_addr = crate::mm::virt::TLB_FLUSH_ADDR.load(Ordering::Acquire);
    if flush_addr != 0 {
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) flush_addr);
        }
        // Acknowledge via our per-CPU TLB_ACK slot.
        let slot = crate::percpu::apic_id_to_slot(crate::percpu::current_apic_id());
        crate::mm::virt::TLB_ACK[slot].store(1, Ordering::Release);
    }

    // Send EOI — this handler is dispatched directly (not via irq_handler)
    // so we must ack the LAPIC ourselves.
    super::apic::send_eoi();
    log::trace!("IPI received");
}

/// Lock‑free NMI handler — just acknowledges the interrupt and returns
/// (Bug 33).  No logging, no locks.
fn nmi_counter() {
    unsafe { core::arch::asm!("xchg eax, eax"); } // NOP / debugger hint
    super::apic::send_eoi();
}

// ---- End of interrupt handling; dump code moved to super::dump ----
