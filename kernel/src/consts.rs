// ---- Physical addresses ----

/// Physical address of the SIPI trampoline page (SIPI vector = 0x08).
pub const TRAMPOLINE_PHYS: u64 = 0x8000;

/// LAPIC MMIO physical base address (architecturally fixed).
pub const LAPIC_PHYS: u64 = 0xFEE0_0000;

/// IOAPIC MMIO physical base address (architecturally fixed).
pub const IOAPIC_PHYS: u64 = 0xFEC0_0000;

/// MMIO region covering LAPIC + IOAPIC (2 MB aligned).
pub const APIC_MMIO_BASE: u64 = 0xFEC0_0000;
pub const APIC_MMIO_SIZE: u64 = 0x40_0000;

// ---- Memory layout constants ----

pub const PAGE_SHIFT: u64 = 12;
pub const PAGE_SIZE: u64 = 0x1000;

/// Kernel task stack size (8 KB).
pub const KERNEL_STACK_SIZE: u64 = 8192;

/// AP kernel stack pages (4 × 4 KB = 16 KB per AP).
pub const AP_STACK_PAGES: usize = 4;

/// Number of tasks reserved for the idle task on each CPU.
pub const IDLE_TASK_ID: usize = 0;

// ---- Timeline / timeout constants ----

/// Approximate loop iterations per millisecond for busy-wait delays.
pub const BUSY_LOOP_PER_MS: u32 = 1_000_000;

/// Serial transmit timeout (loop iterations).
pub const SERIAL_TIMEOUT: u32 = 0xFFFF;
