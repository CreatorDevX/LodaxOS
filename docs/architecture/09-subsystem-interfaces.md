# 09 — Subsystem Interfaces

## Overview

This document defines the API surfaces between kernel subsystems. The interfaces are designed to be minimal and flat — each subsystem exposes a small number of public functions, and subsystems interact through these narrow interfaces rather than through shared global state.

## Serial Subsystem (`src/serial.rs`)

### Public API

```rust
pub fn init();                           // Initialize COM1 at 115200 8N1
pub fn write_byte(byte: u8);             // Write single byte (poll THR)
pub fn write_str(s: &str);               // Write string (\n → \r\n)
```

### Internals

- I/O port `0x3F8` (COM1) with divisor `0x01` (115200 baud from 1.8432 MHz clock)
- Line control register (LCR): 8N1 = `0x03`
- FIFO control register (FCR): enable, clear, 14-byte threshold = `0xC7`
- Modem control register (MCR): DTR + RTS + IRQ enable = `0x0B`
- Write polling: check LSR bit 5 (THR empty) before each byte
- No buffering, no interrupts — synchronous writes only

### Dependents

- Logger: calls `write_str` for log output
- Panic handler: calls `write_str` for error messages
- GDT loader: uses `com1_trace` for early debug output (single-byte writes with 100K retry timeout)

## Logger Subsystem (`src/logger.rs`)

### Public API

```rust
pub fn init() -> Result<(), SetLoggerError>;
```

### Registration

Implements `log::Log` trait:
```rust
fn enabled(&self, metadata: &LogMetadata) -> bool { true }
fn log(&self, record: &LogRecord);
fn flush(&self);
```

Log format: `[LEVEL] target: message\n`

Max log level: `LevelFilter::Trace` (all levels enabled)

Uses `core::fmt::write` to render arguments without heap allocation.

### Dependents

- All kernel code via `log::info!()`, `log::warn!()`, `log::error!()`, `log::debug!()`, `log::trace!()`
- Panic handler uses `write` directly for panic message formatting

## Font Subsystem (`src/font.rs`)

### Public API

```rust
pub const GLYPH_WIDTH: usize = 8;
pub const GLYPH_HEIGHT: usize = 16;
pub fn get_glyph(ch: char) -> &'static [u8; 16];
```

### Data

Bitmap font for ASCII 32–126 (95 glyphs). Each glyph is 16 bytes (16 rows × 8 columns). MSB = leftmost pixel.

### Dependents

- Framebuffer (`kernel::Framebuffer`): calls `get_glyph` for text rendering in `put_char`, `write_str`, `write_str_centered`

## Physical Memory Allocator (`src/mm/phys.rs`)

### Public API

```rust
pub unsafe fn init_from_regions(regions: &[(u64, u64)], boot_info_phys: u64);
pub fn alloc_page() -> Option<u64>;        // returns physical address
pub fn alloc_pages(count: u64) -> Option<u64>;
pub fn free_page(addr: u64);
pub fn free_pages(addr: u64, count: u64);
```

### Interface Contract

- `init_from_regions` must be called once before any alloc/free
- Regions must be the free memory descriptors from BootInfo
- Allocators may be called from any kernel context (interrupts must be enabled or the caller must hold no locks that the dispatcher would contend on)
- Returns physical addresses (4 KB aligned)
- `alloc_pages(0)` = `None`
- Double-free detection: `free_page` on an already-free page logs a warning

### Dependents

- Page table builder (`virt.rs`): allocates pages for PML4, PDP, PD, PT tables
- Heap allocator (`heap.rs`): allocates pages for heap arena
- Task manager (`task.rs`): allocates pages for task kernel stacks
- IOAPIC/LAPIC MMIO mapping utilities

## Virtual Memory Manager (`src/mm/virt.rs`)

### Public API

```rust
pub const PRESENT: u64;
pub const WRITABLE: u64;
pub const USER: u64;
pub const CACHE_DISABLE: u64;
pub const NO_EXECUTE: u64;
pub const DATA: u64;                // PRESENT | WRITABLE | NO_EXECUTE
pub const HIGHER_HALF: u64;         // 0xFFFF_8000_0000_0000

pub unsafe fn init(regions: &[(u64, u64)], fb_phys: Option<(u64, u64)>);
pub fn translate(virt: u64) -> Option<u64>;
pub fn unmap(virt: u64);
pub fn pml4_address() -> u64;       // current PML4 physical address
pub unsafe fn map_contiguous(pml4, virt_start, phys_start, num_pages, flags);
pub fn map_region_higher_half(pml4, phys, size, flags);
```

### Interface Contract

- `init` must be called once after physical allocator init
- After `init`, CR3 points to kernel's own PML4
- All memory operations after init use higher-half virtual addresses
- MMIO regions must use `map_region_higher_half` to avoid 2 MB identity page conflict
- `pml4_address()` reads CR3 — valid only after `init`

### Dependents

- Heap: allocates + maps pages at heap virtual base
- IOAPIC init: maps MMIO regions
- LAPIC init: maps LAPIC MMIO region
- Task init: maps kernel stack pages
- (Future) Userspace: manages per-process page tables

## Heap Allocator (`src/mm/heap.rs`)

### Public API

```rust
pub fn init();
pub fn heap_size() -> usize;

// Via GlobalAlloc impl:
#[global_allocator]
static ALLOCATOR: GlobalAllocator;
```

### Interface Contract

- `init` must be called after page table init
- Uses `linked_list_allocator::Heap` internally
- Thread-safe via spinlock
- First-fit allocation strategy
- Max size: 64 MB at `0xFFFF_8080_0000_0000`

### Dependents

- All code that uses `alloc::vec::Vec`, `alloc::boxed::Box`, `alloc::string::String`, `alloc::format!`, etc.
- Bootloader uses UEFI allocator (`uefi::allocator::Allocator`), not this kernel heap

## ACPI Subsystem (`src/acpi/mod.rs`)

### Public API

```rust
pub fn init() -> AcpiContext;
pub fn find_sdt(xsdt_addr: u64, signature: &[u8; 4]) -> Option<u64>;
pub fn validate_table(addr: u64) -> bool;

pub struct AcpiContext {
    pub revision: u8,
    pub rsdp_addr: u64,
    pub xsdt_addr: u64,
    pub madt_addr: Option<u64>,
}
```

### Interface Contract

- Kernel ACPI init must happen before page table switch (identity map needed) OR after with physical addresses
- The bootloader's RSDP capture uses a different path (UEFI config table) than the kernel's fallback path (EBDA/BIOS ROM scan)
- Currently only MADT is parsed; FADT, MCFG, HPET are identified but not used

### Dependents

- Kernel main: calls `acpi::init()` → parses MADT → configures IOAPICs and interrupt routing
- MADT parser: called by ACPI subsystem with physical address

## Interrupt Routing (`src/intr/mod.rs`)

### Public API

```rust
pub fn init(madt: &MadtInfo);
pub fn alloc_vector() -> Option<u8>;
pub fn lookup_isa(isa_irq: u8) -> Option<&'static IrqRoute>;
pub fn lookup_gsi(gsi: u32) -> Option<&'static IrqRoute>;
pub fn lookup_vector_isa(vector: u8) -> Option<u8>;
pub fn install_route(route: &IrqRoute);
pub fn enable_route(route: &IrqRoute);
pub fn install_all_routes();     // install all, masked
pub fn install_and_enable_all() -> usize;
```

### Data Flows

```
Input: MADT info (from acpi::madt::parse)
  → walks ISO entries
  → for each: ISA IRQ → GSI → IOAPIC lookup → vector allocation → IrqRoute
  → identity maps remaining ISA IRQs
  → stores in routing table

Output: IrqRoute instances used by:
  - IOAPIC driver for redirection entry programming
  - IDT handler for device IRQ dispatch
  - Kernel main for enabling device routes (PIT, keyboard)
```

### Dependents

- Kernel main: routes IOAPIC entries, enables PIT/keyboard
- IDT irq_handler: maps vector back to ISA source for PIT/keyboard handling

## IOAPIC Driver (`src/arch/ioapic.rs`)

### Public API

```rust
pub fn init(ioapic_infos: &[IoApicInfo]);
pub fn is_initialized() -> bool;
pub fn get(index: usize) -> Option<&'static IoApic>;
pub fn count() -> usize;
pub fn lookup_gsi(gsi: u32) -> Option<(usize, u8)>;

// IoApic methods:
pub fn set_entry(&self, pin: u8, low: u32, high: u32);
pub fn get_entry(&self, pin: u8) -> (u32, u32);
pub fn mask_entry(&self, pin: u8);
pub fn unmask_entry(&self, pin: u8);
pub fn make_redir_low(vector: u8, flags: u16, masked: bool) -> u32;
pub fn make_redir_high(apic_id: u8) -> u32;
```

### Dependents

- Interrupt routing: calls `set_entry`, `mask_entry`, `unmask_entry`
- Kernel main: calls `init` with IOAPIC info from MADT

## LAPIC Driver (`src/arch/apic.rs`)

### Public API

```rust
pub fn init_mmio();
pub fn enable();
pub fn is_initialized() -> bool;
pub fn configure_timer(divisor: u32, vector: u8, periodic: bool);
pub fn calibrate_pit();
pub fn set_timer_count(ms: u32);
pub fn pit_enable_periodic(freq_hz: u32);
pub fn send_eoi();
```

### Dependents

- Kernel main: calls `init_mmio` → `enable` → `calibrate_pit` → `configure_timer` → `set_timer_count`
- IDT irq_handler: calls `send_eoi` if LAPIC is initialized
- Kernel idle loop: relies on timer for scheduling

## GDT Subsystem (`src/arch/gdt.rs`)

### Public API

```rust
pub fn load();
pub fn set_ist1(addr: u64);

// Exported selectors:
pub const KERNEL_CODE_SEL: u16 = 0x08;
```

### Dependents

- Kernel main: calls `load`, then `set_ist1` after IDT init
- IDT init: calls `set_ist1` to set up IST1 pointer for double fault handler
- Task creation: uses `KERNEL_CODE_SEL` (0x08) for task CS

## IDT Subsystem (`src/arch/idt.rs`)

### Public API

```rust
pub fn init();
pub fn mask_pic();
pub fn enable_interrupts();
pub fn disable_interrupts();
pub fn ticks() -> u64;
pub fn pit_ticks() -> u64;
pub fn key_count() -> u64;
pub fn key_scancode() -> u16;
```

### Internals

- `TrapFrame` struct shared with task manager (defines register save layout)
- `interrupt_dispatcher` called by all stubs, dispatches by vector
- `irq_handler` sends EOI, handles timer/PIT/keyboard, calls scheduler
- `exception_handler` logs details, halts on unrecoverable
- `syscall_handler` dispatches syscalls by number

### Dependents

- Kernel main: calls `init`, `mask_pic`
- Task scheduler: modifies TrapFrame in timer handler for context switch
- Syscall handlers: process syscall requests
- Idle loop: reads `ticks()`, `pit_ticks()`, `key_count()`

## Task Manager (`src/task.rs`)

### Public API

```rust
pub fn init();
pub fn init_main_task();
pub fn task0_stack_top() -> u64;
pub fn is_initialized() -> bool;
pub fn current_task_id() -> usize;
pub fn task_count() -> usize;
pub fn create_task(entry: u64) -> Option<usize>;
pub fn schedule(frame: &mut TrapFrame) -> bool;
pub fn block_current(frame: &mut TrapFrame);
pub fn wake(task_id: usize);
pub fn yield_now();
```

### Data Flow

```
Timer IRQ (vector 32)
  → irq_handler()
    → task::schedule(&mut TrapFrame)
      → saves current task state in Task.saved_frame
      → finds next ready task (round-robin)
      → overwrites TrapFrame with next task's state
      → returns true
    → context switch via mov rsp + popfq/retfq

Syscall (int 0x80)
  → syscall_handler()
    → task::block_current(frame)  // syscall 1
    → task::wake(id)             // syscall 3
    → task::current_task_id()    // syscall 2
    → task::yield_now()          // via int 0x80 nr=0
```

### Dependents

- IDT: calls `schedule` from timer IRQ handler
- Kernel main: calls `init`, `init_main_task`, `create_task` for test tasks
- Syscall handlers: call block_current, wake, current_task_id

## Framebuffer (`kernel/src/main.rs`)

### Public API (per-crate implementation in kernel)

```rust
pub struct Framebuffer { ptr, width, height, stride, bytes_per_pixel, is_bgr }

impl Framebuffer {
    pub fn from_info(info: &FramebufferInfo) -> Self;
    pub fn set_pixel(&self, x, y, r, g, b);
    pub fn clear(&mut self, r, g, b);
    pub fn put_char(&mut self, ch, x, y, r, g, b);
    pub fn write_str(&mut self, s, x, y, r, g, b);
    pub fn write_str_centered(&mut self, s, y, r, g, b);
}
```

(Framebuffer is not a separate module — it is defined inline in `kernel/src/main.rs` and `src/main.rs`. A bootloader Framebuffer is similar but uses `Framebuffer::new(gop)` instead of `from_info`.)

### Interface Contract

- Pixel writes are volatile (prevent compiler optimization of redundant writes)
- BGR/RGB handling: `is_bgr` flag read from GOP pixel format
- Bounds-checked: writes outside the visible area are silently dropped
- After CR3 switch: `ptr` must be updated to higher-half virtual address
- No double-buffering, no vsync, no compositing

### Dependents

- Kernel main: renders splash screen and status text
- (Future) GUI system: will render windows, widgets, and composited output

## Subsystem Initialization Order

The kernel initialization sequence in `_start` has strict ordering constraints:

```
Phase 0: Serial → Logger                   (no dependencies)
Phase 1A: Memory regions from BootInfo      (no dependencies)
Phase 1B: Framebuffer init                  (no dependencies)
Phase 1C: Physical allocator init           (regions from 1A)
Phase 1D: ACPI init                        (regions from 1A, phys alloc)
Phase 1E: Page tables init                 (regions, phys alloc, optionally fb)
Phase 1F: Heap init                        (page tables, phys alloc)
Phase 2A: cli + mask PIC                    (no dependencies)
Phase 2B: LAPIC MMIO init                  (page tables)
Phase 2C: IOAPIC init + INTR routing       (page tables, ACPI/MADT)
Phase 3A: GDT load                         (page tables)
Phase 3B: IDT init                         (GDT)
Phase 3C: Task init                        (IDT, page tables, phys alloc)
Phase 3D: Create test tasks                (task init)
Phase 3E: Install IOAPIC routes            (IOAPIC + INTR init)
Phase 3F: Enable LAPIC                     (LAPIC MMIO, IOAPIC)
Phase 3G: Calibrate LAPIC timer            (LAPIC)
Phase 3H: Configure LAPIC timer            (LAPIC calibrated)
Phase 3I: Enable PIT periodic              (IOAPIC routes)
Phase 3J: sti + int 32 test               (everything above)
Phase 3K: Unmask device routes             (IOAPIC routes)
Phase 4: Idle loop                         (all of the above)
```

This order ensures that each subsystem's dependencies are initialized before it runs. For example, heap depends on page tables (to map heap pages) which depends on the physical allocator (to allocate page table pages).
