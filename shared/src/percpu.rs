// Stub `percpu` for lodaxos-core. The real implementation lives in
// `src/percpu.rs`, included by lodaxos-kernel via `#[path]` in its
// `main.rs`. The kernel's ISR calls `crate::percpu::tick()`, which
// also compiles inside lodaxos-core (where the same source file is
// pulled in via `#[path]` in `src/arch/idt.rs`'s parent) — the
// shared crate never actually executes the ISR, so the stub body
// is sufficient to satisfy the symbol resolver.

/// Increment the global LAPIC tick counter.
pub fn tick() -> u64 {
    crate::arch::idt::tick()
}

/// Read the global LAPIC tick counter.
pub fn ticks() -> u64 {
    crate::arch::idt::ticks()
}
