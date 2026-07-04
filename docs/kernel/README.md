# LodaxOS Kernel (`lodaxos-kernel`)

## Entry Point

The kernel entry point is `_start` at physical address `0x100000`, defined in
`kernel/src/main.rs:30`. The bootloader loads the ELF binary at this address
and jumps to `_start` with a `*const BootInfo` in RDI.

```c
// kernel/src/main.rs:30
extern "C" fn _start(boot_info: *const BootInfo) -> !
```

## Architecture

| Attribute       | Value                                  |
|-----------------|----------------------------------------|
| ISA             | x86-64                                 |
| Environment     | Freestanding (`#![no_std]`)            |
| Panic strategy | `panic=abort`                          |
| Code model      | `kernel`                               |
| Relocation      | `static`                               |
| Page size       | 4 KiB (`0x1000`)                       |
| Higher half     | `0xFFFF_8000_0000_0000`                |

## Init Sequence

The kernel initialises in strict order. Each phase is logged with its phase
label and must complete before the next starts.

```
_start(boot_info)
  │
  ├─ 1. serial::init()                    ── COM1 (UART 16550)
  ├─ 2. logger::init()                    ── log facade → serial
  ├─ 3. FPU/SSE/XSAVE enable             ── CR4.OSFXSR|OSXMMEXCPT|OSXSAVE
  ├─ 4. build_memory_layout(info)        ── carve kernel image from regions
  ├─ 5. phys::init_from_regions(…)       ── buddy allocator
  ├─ 6. discover_acpi(info)              ── RSDP → XSDT → MADT → IOAPICs
  ├─ 7. arch::smp::init()                ── load trampoline @ 0x8000
  ├─ 8. virt::init(regions, fb)          ── 4-level paging, higher-half + identity
  ├─ 9. mm::heap::init()                 ── slab allocator (32B..8KB)
  ├─10. mm::vma::init_kernel_vmas()      ── kernel VMA tree (demand paging)
  ├─11. vcpu::init()                     ── VCPU slab (max 128)
  ├─12. scheduler::init()                ── gang scheduler
  ├─13. arch::apic::init_mmio()          ── map LAPIC MMIO
  ├─14. arch::ioapic::init(…)            ── map IOAPIC MMIO, mask all pins
  ├─15. intr::init(madt)                 ── allocate vectors, build routes
  ├─16. percpu::set_bsp_apic_id(...)     ── record BSP LAPIC ID
  ├─17. gdt::init_for_slot(bsp_slot)     ── per-CPU GDT/TSS (lgdt, ltr)
  ├─18. idt::init()                      ── 256-entry IDT, exception stubs
  ├─19. percpu::install_gs_base()        ── IA32_GS_BASE → per-CPU slot
  ├─20. scheduler::init_idle_vcpu()      ── idle VCPU for BSP
  ├─21. gdf::init_from_package(binary)   ── load driver ELF(s)
  ├─22. intr::install_all_masked()        ── program IOAPIC redirections
  ├─23. arch::apic::enable()             ── SVR, TPR, mask LINT0/1
  ├─24. calibrate_pit()                  ── measure LAPIC ticks/ms
  ├─25. configure_timer(16, 32, periodic) ── LAPIC timer → vector 32
  ├─26. percpu::release_all_aps()        ── set kernel_ready for all slots
  ├─27. ap_start::smp_boot_aps(info)     ── INIT-SIPI-SIPI, poll APs
  ├─28. sti                               ── enable interrupts
  ├─29. Driver interaction (ext4, fb)    ── send/receive via GDF mailboxes
  └─30. bsp_idle_loop()                  ── hlt-wait, steal tasks, log
```

## Build

```bash
cargo +nightly build -p lodaxos-kernel --target kernel/target.json -Zbuild-std=core,alloc
```

The `target.json` selects:
- `llvm-target`: `x86_64-unknown-none`
- `linker-flavor`: `ld.lld`
- `code-model`: `kernel`
- `relocation-model`: `static`
- Pre-link arg: `-Tkernel/linker.ld --gc-sections`

## Linker Script (`kernel/linker.ld`)

```
ENTRY(_start)
. = 0x100000;
__kernel_start = .;
  .text   ALIGN(16)
  .rodata ALIGN(16)
  .data   ALIGN(16)
  .bss    ALIGN(16)
__kernel_end = .;
/DISCARD/ : *(.eh_frame) *(.comment) *(.note*)
```

## Kernel Constants (`kernel/src/consts.rs`)

| Constant              | Value                      | Description                    |
|-----------------------|----------------------------|--------------------------------|
| `TRAMPOLINE_PHYS`     | `0x8000`                   | SIPI trampoline base           |
| `LAPIC_PHYS`          | `0xFEE0_0000`              | LAPIC MMIO base                |
| `IOAPIC_PHYS`         | `0xFEC0_0000`              | IOAPIC MMIO base               |
| `APIC_MMIO_BASE`      | `0xFEC0_0000`              | MMIO region base               |
| `APIC_MMIO_SIZE`      | `0x40_0000`                | MMIO region size               |
| `PAGE_SHIFT`          | `12`                       | Page shift                     |
| `PAGE_SIZE`           | `0x1000`                   | Page size                      |
| `KERNEL_STACK_SIZE`   | `8192`                     | Kernel stack size              |
| `AP_STACK_PAGES`      | `4`                        | Per-AP stack pages             |
| `IDLE_TASK_ID`        | `0`                        | Idle VCPU id                   |
| `BUSY_LOOP_PER_MS`    | `1_000_000`                | Delay calibration              |
| `SERIAL_TIMEOUT`      | `0xFFFF`                   | Serial transmit timeout        |

## Panic Handler

Defined at `kernel/src/main.rs:454`. Writes location and message to COM1
without locking, then enters `cli; hlt` loop.

## Deploy

The bootloader expects a raw ELF binary loaded at `0x100000`. The kernel
communicates with drivers via a pre-loaded ELF package whose header
(`DriverPkgHeader`) and entries (`DriverPkgEntry`) are defined in the
`lodaxos_system` crate.
