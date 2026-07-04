# Interrupts

LodaxOS uses the xAPIC interrupt architecture: the LAPIC for local interrupts
(timer, IPI) and the IOAPIC for device interrupt routing. The legacy 8259 PIC
is fully masked.

---

## 1. IDT (`kernel/src/arch/idt.rs`)

### IDT Entry

```c
// 16 bytes
struct IdtEntry {
    offset_low: u16,    // bits 15:0 of handler address
    selector: u16,      // code segment selector (0x08)
    ist: u8,            // Interrupt Stack Table index (0, 1, or 2)
    type_attr: u8,      // 0x8E = 32-bit interrupt gate
    offset_mid: u16,    // bits 31:16
    offset_high: u32,   // bits 63:32
    reserved: u32,
}
```

### TrapFrame

Full register save area pushed by naked assembly stubs:

```
Offset  Size  Register
------  ----  --------
0x00    8     r15
0x08    8     r14
0x10    8     r13
0x18    8     r12
0x20    8     r11
0x28    8     r10
0x30    8     r9
0x38    8     r8
0x40    8     rax
0x48    8     rbx
0x50    8     rcx
0x58    8     rdx
0x60    8     rbp
0x68    8     rsi
0x70    8     rdi
0x78    8     vector (pushed by stub)
0x80    8     error_code (pushed by stub or CPU)
0x88    8     rip      ← CPU-pushed interrupt frame
0x90    8     cs
0x98    8     rflags
0xA0    8     rsp      (only on privilege-level change)
0xA8    8     ss       (only on privilege-level change)
```

Total: 0xB0 bytes (176 bytes) without SS/RSP, 0xC0 bytes (192 bytes) with.

### Stub Generation

Two macros generate naked assembly stubs:

- `define_stub_noerr!(name, vec)` — pushes `0` as dummy error code, then vector
- `define_stub_err!(name, vec)` — CPU pushes real error code, stub pushes vector

Both push all 15 GPRs, set `rdi = rsp` (TrapFrame pointer), call
`interrupt_dispatcher`, restore GPRs, `add rsp, 16`, `iretq`.

### Vector Allocation

| Range     | Usage                    |
|-----------|--------------------------|
| 0..31     | CPU exceptions           |
| 32        | LAPIC timer              |
| 33..63    | Device IRQs (IOAPIC)     |
| 0x81 (129)| IPI (cross-CPU wake/TLB) |
| 0xFF (255)| Spurious interrupt       |

### Interrupt Stack Table

- **IST1** (index 1): Double fault (`#DF`, vector 8). Each CPU has a 16 KiB
  per-CPU IST1 stack.
- **IST2** (index 2): All IRQ vectors (32..63), IPI, spurious. Each CPU has a
  16 KiB per-CPU IST2 stack.

### Public API

| Function | Description |
|----------|-------------|
| `idt_init()` | Wire all 256 entries, set IST1 in TSS, `lidt` for BSP |
| `idt_load()` | `lidt` for the current CPU |
| `mask_pic()` | Write `0xFF` to PIC OCW1 registers (ports `0x21`, `0xA1`) |
| `ticks() -> u64` | Read global LAPIC tick counter |
| `tick() -> u64` | Increment and return tick counter |
| `pit_ticks() -> u64` | Read PIRQ0 tick counter |
| `enable_interrupts()` | `sti` |
| `disable_interrupts()` | `cli` |
| `idt_ptr_limit_base(slot) -> (u16, u64)` | Return IDTR contents for a per-CPU slot |

### Dispatcher (`interrupt_dispatcher`)

```c
extern "C" fn interrupt_dispatcher(frame: &mut TrapFrame)
```

1. Process any `pending_tlb_flush` for this CPU.
2. Dispatch by vector:
   - `0..31` → `exception_handler`
   - `32..63` → `irq_handler`
   - `0x81` → `ipi_handler`
   - `0xFF` → ignored (spurious)

### Exception Handling

Exceptions 0..31 (excluding 3=BP and 14=PF) print register dump and halt.
For vector 14 (page fault), calls `vma::handle_page_fault`. If unresolved,
checks if the faulting VCPU is a GDF service (HardwareDriver/AbstractionDriver)
and attempts restart via `gdf::handle_crash`.

### IPI Handler (`ipi_handler`)

```
vector 0x81:
  1. Set IPI_PENDING flag
  2. If TLB_FLUSH_ADDR non-zero, execute invlpg and ACK via TLB_ACK[cpu]
  3. Send EOI
```

---

## 2. LAPIC (`kernel/src/arch/apic.rs`)

### MMIO Registers

| Offset | Name          | Description                    |
|--------|---------------|--------------------------------|
| 0x020  | APIC_ID       | Local APIC ID (high byte)      |
| 0x030  | APIC_LVR      | Version Register               |
| 0x080  | APIC_TPR      | Task Priority Register         |
| 0x0B0  | APIC_EOI      | End-Of-Interrupt               |
| 0x0F0  | APIC_SVR      | Spurious Vector Register       |
| 0x300  | APIC_ICR_LOW  | Interrupt Command Register low |
| 0x310  | APIC_ICR_HIGH | Interrupt Command Register high|
| 0x320  | APIC_LVT_TIMER| LVT Timer entry                |
| 0x350  | APIC_LVT_LINT0| LVT LINT0 entry                |
| 0x360  | APIC_LVT_LINT1| LVT LINT1 entry                |
| 0x370  | APIC_LVT_ERROR| LVT Error entry                |
| 0x3E0  | APIC_TDCR     | Timer Divide Configuration     |
| 0x380  | APIC_TICR     | Timer Initial Count            |
| 0x390  | APIC_CCR      | Current Count Register         |

Physical base: `0xFEE0_0000` (architecturally fixed, confirmed via
`IA32_APIC_BASE` MSR). Mapped in the higher-half at `HIGHER_HALF + phys`.

### LAPIC Timer Calibration

1. Configure LAPIC timer: one-shot, divide-by-16, max count (`0xFFFF_FFFF`).
2. Reprogram PIT channel 0 to mode 0 with a 20 ms target count.
3. Poll PIT until it reaches ~0.
4. Read LAPIC CCR, compute `TICKS_PER_MS = elapsed / 20`.

### ICR Delivery Modes

| Mode        | ICR bits         |
|-------------|------------------|
| Fixed       | `0`              |
| INIT        | `5 << 8`         |
| Startup     | `6 << 8`         |

### ICR Destination Shorthands

| Shorthand               | Bits       |
|-------------------------|------------|
| Physical (specific)     | `0`        |
| Self                    | `1 << 18`  |
| All (incl. self)        | `2 << 18`  |
| All (excl. self)        | `3 << 18`  |

### Public API

| Function | Description |
|----------|-------------|
| `init_mmio()` | Map LAPIC MMIO in higher-half |
| `enable()` | Mask LINT0/1, set SVR+TPR |
| `configure_timer(divisor, vector, periodic)` | Program LVT timer entry |
| `calibrate_pit()` | Measure TICKS_PER_MS (20 ms window) |
| `set_timer_count(ms)` | Write TICR for desired interval |
| `ap_enable_timer(apic_id)` | Per-AP timer setup (physical MMIO) |
| `pit_enable_periodic(freq_hz)` | PIT channel 0 rate generator |
| `send_init_ipi_all()` | Broadcast INIT to all APs |
| `send_sipi_all(vector)` | Broadcast SIPI to all APs |
| `send_init_ipi(dest)` | Send INIT to specific APIC ID |
| `send_sipi(dest, vector)` | Send SIPI to specific APIC ID |
| `send_ipi(dest, vector)` | Send fixed IPI to specific APIC ID |
| `send_ipi_others(vector)` | Send fixed IPI to all other CPUs |
| `send_eoi()` | Write `0` to EOI register |
| `read_lapic_id() -> u32` | Read APIC ID register |
| `read_apic_base() -> u64` | Read IA32_APIC_BASE MSR |
| `read32(offset) -> u32` | Read LAPIC MMIO register |
| `write32(offset, val)` | Write LAPIC MMIO register |
| `is_initialized() -> bool` | True after `init_mmio` |
| `is_bsp() -> bool` | True if current CPU is BSP |
| `set_bsp_lapic_id(id)` | Record BSP LAPIC ID |

### IRQ Handler (vector 32 timer)

```c
fn irq_handler(frame: &mut TrapFrame, vector: u64) {
    send_eoi();
    match vector {
        32 => {
            percpu::tick();
            if scheduler::is_initialized() {
                let (switched, next_pml4, next_fpu) = scheduler::schedule(frame);
                if switched {
                    // Inline asm: CR3 switch, RSP restore, fxrstor, GPR restore, sti, ret
                }
            }
        }
        _ => {
            // Device IRQ → lookup_vector_isa
        }
    }
}
```

---

## 3. IOAPIC (`kernel/src/arch/ioapic.rs`)

### MMIO Registers

| Offset | Register     |
|--------|--------------|
| 0x00   | IOREGSEL     |
| 0x10   | IOWIN        |

Redirection entries are accessed by writing the index register (IOREGSEL)
then reading/writing the data register (IOWIN):

```
entry_low  at IOAPIC_REDIR_BASE + (pin * 2)
entry_high at IOAPIC_REDIR_BASE + (pin * 2) + 1
```

### Redirection Entry Low DWORD

```
Bit    Field
───    ─────
 7:0   Vector
10:8   Delivery Mode (000 = fixed)
11     Destination Mode (0 = physical)
13     Polarity (0 = high, 1 = low)
15     Trigger Mode (0 = edge, 1 = level)
16     Mask (1 = masked)
63:17  Reserved / other
```

### Redirection Entry High DWORD

```
Bit    Field
───    ─────
63:56  Destination APIC ID
31:0   Reserved
```

### Public API

| Function | Description |
|----------|-------------|
| `init(ioapic_infos)` | Map MMIO, read ID/version, mask all redirections |
| `get(index) -> Option<&IoApic>` | Get IOAPIC reference by index |
| `count() -> usize` | Number of IOAPICs discovered |
| `lookup_gsi(gsi) -> Option<(usize, u8)>` | Find IOAPIC+pin for a GSI |
| `mask_entry(pin)` | Set masked bit on a pin |
| `unmask_entry(pin)` | Clear masked bit on a pin |
| `set_entry(pin, low, high)` | Program redirection entry |
| `get_entry(pin) -> (u32, u32)` | Read redirection entry |
| `make_redir_low(vector, flags, masked) -> u32` | Build redir low DWORD |
| `make_redir_high(apic_id) -> u32` | Build redir high DWORD |
| `is_initialized() -> bool` | True after `init` |

---

## 4. Vector Allocator & IRQ Routing (`kernel/src/intr/mod.rs`)

### Vector Allocation

Simple incrementing allocator. Vectors 33..63 (31 slots) are available for
device IRQs. Vector 32 is reserved for the LAPIC timer.

| Function | Description |
|----------|-------------|
| `alloc_vector() -> Option<u8>` | Allocate next free vector |
| `vectors_exhausted() -> bool` | True if all vectors consumed |

### IrqRoute

```c
struct IrqRoute {
    isa_source: u8,       // ISA IRQ number (0..15)
    gsi: u32,             // Global System Interrupt
    ioapic_index: usize,  // Index into IOAPIC array
    ioapic_pin: u8,       // Pin on that IOAPIC
    vector: u8,           // Allocated IDT vector
    flags: u16,           // MADT ISO flags (polarity, trigger)
}
```

### MADT ISO Parsing

For each ISA IRQ (0..15):
1. If an Interrupt Source Override exists (bus=0), use the ISO's GSI and flags.
2. Otherwise, identity-map: `GSI = IRQ`, `flags = 0`.

Each valid mapping allocates a vector via `alloc_vector()` and calls
`build_route()` which uses `madt::lookup_ioapic()` to find the IOAPIC+pin.

### Public API

| Function | Description |
|----------|-------------|
| `intr_init(madt)` | Build routing table from MADT |
| `intr_alloc_vector() -> Option<u8>` | Allocate device vector |
| `intr_route_irq(gsi, vector)` | Route a GSI (low-level) |
| `lookup_isa(isa_irq) -> Option<&IrqRoute>` | Find route by ISA source |
| `lookup_gsi(gsi) -> Option<&IrqRoute>` | Find route by GSI |
| `lookup_vector_isa(vector) -> Option<u8>` | Find ISA source by vector |
| `install_route(route)` | Program IOAPIC entry (masked) |
| `enable_route(route)` | Unmask IOAPIC entry |
| `install_all_routes()` | Install all routes (masked) |
| `install_all_masked() -> usize` | Install all routes, count |

### IRQ CPU Distribution

A round-robin counter (`IRQ_CPU_NEXT`) distributes device IRQs across
available CPUs when programming IOAPIC entries via `install_route`.
