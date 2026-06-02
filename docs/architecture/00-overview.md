# 00 — System Overview

## Philosophy

LodaxOS is built on three architectural axioms:

1. **The kernel is absolute.** It owns scheduling, memory, IPC primitives, capability enforcement, and interrupt infrastructure. It contains zero policy logic, zero driver logic, and zero business logic.

2. **Everything outside the kernel is replaceable.** Filesystems, drivers, device management, user interfaces, and runtime environments are all implemented as processes managed by Secure Runtime. If any of them fail, the kernel remains intact and recovery is possible without reboot.

3. **Recovery is layered from bottom to top.** A failure in an application restarts only the application. A failure in PyI restarts the user runtime. A failure in Secure Runtime triggers kernel-assisted recovery. A kernel panic is the last resort.

## Current Implementation State

LodaxOS currently implements only the kernel layer and the boot chain. The Secure Runtime, PyI, Agent framework, and driver services are future work. This document describes both what exists today and what is planned.

### What Exists Today

- 5-crate Rust workspace producing UEFI-compatible binaries
- Two-stage UEFI boot chain (chainloader → bootloader → kernel)
- Bare-metal x86-64 kernel with full interrupt handling
- 4-level page tables with higher-half mapping
- Buddy-based physical page allocator (orders 0-10)
- SLUB-style slab heap allocator with demand-paged VMA support
- LAPIC/IOAPIC interrupt controller drivers
- ACPI RSDP/MADT/XSDT discovery and parsing
- Preemptive round-robin task scheduler with syscall interface
- Self-contained ext4 filesystem reader (bootloader only)
- UEFI GOP framebuffer with bitmap font rendering

### What Is Planned (see 08-future-architecture.md)

- Secure Runtime: userspace service manager, policy engine, capability broker
- PyI: JIT-compiled Python/WASM runtime for userspace applications
- Agent model: first-class system domains with isolated userspace environments
- Driver services: device-specific logic outside the kernel
- PCI enumeration, MSI/MSI-X, SMP support
- Layered recovery from application through kernel

## System Structure

```
┌──────────────────────────────────────────────────────────────────┐
│                     User Applications                            │
│  (Editor, Browser, Terminal — each a PyI subprocess)             │
├──────────────────────────────────────────────────────────────────┤
│                     PyI Runtime                                  │
│  (JIT WASM-backed Python, REPL, UI layer)                        │
├──────────────────────────────────────────────────────────────────┤
│                   Secure Runtime                                 │
│  (Service manager, policy engine, capability broker, Agent mgmt)  │
├──────────────────────────────────────────────────────────────────┤
│                   Kernel (Ring 0)                                │
│  (Scheduler, memory, IPC, capability enforcement, HAL)            │
├──────────────────────────────────────────────────────────────────┤
│             UEFI Boot Chain (x86-64)                             │
│  (OVMF → chainloader → bootloader → kernel)                      │
├──────────────────────────────────────────────────────────────────┤
│                    Hardware                                      │
│  (CPU, LAPIC, IOAPIC, HPET/PIT, UART, PCI bus)                  │
└──────────────────────────────────────────────────────────────────┘
```

## Design Tenets

1. **Static linking for the kernel.** No runtime module loading. The kernel is a single ELF binary loaded at 0x100000. All subsystems are compiled in.

2. **No heap in interrupt context.** Interrupt handlers must not allocate. The `TrapFrame` lives on the interrupt stack. The scheduler modifies it in-place.

3. **Identity mapping until page tables are ready.** The bootloader and early kernel run on UEFI's identity mapping. The kernel builds its own page tables before switching CR3.

4. **Higher-half kernel.** All kernel code and data are mapped in the upper virtual address range starting at 0xFFFF_8000_0000_0000. The lower half is reserved for userspace.

5. **Spinlocks only.** The kernel uses spinlocks (atomic CAS) for all synchronization. No blocking locks, no IRQ-safe variants — interrupts are disabled during critical sections by convention.

6. **No floating point in the kernel.** x87, SSE, AVX registers are not saved or restored during context switches. FPU use requires explicit opt-in with save/restore (future work).

7. **Framebuffer is not double-buffered.** The kernel writes directly to the hardware framebuffer. No compositor, no window manager — text and splash output only.

## Namespace Convention

| Prefix | Meaning |
|---|---|
| `HF_` | Hard Fault — kernel-level halt required |
| `SF_` | Soft Fault — SR-level recovery possible |
| `KERNEL_` | Kernel internal interface |
| `SR_` | Secure Runtime interface |
| `CAP_` | Capability identifier |
| `AGENT_` | Agent domain constants |

Fault codes follow a flat hex numbering: `0x0x` = hard fault, `0x1x` = soft fault.

## File Layout

All kernel implementation lives in `src/` at the workspace root. The `shared/` crate re-exports `src/` code via `#[path]` and `include!` directives. Individual crate entry points are in `chain/src/`, `boot/src/`, and `kernel/src/`. This single-source layout avoids duplication while allowing independent compilation targets (UEFI PE32+ vs. bare-metal ELF).
