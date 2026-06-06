//! Synchronization primitives for the LodaxOS kernel.
//!
//! ## Locking discipline
//!
//! - **`IrqSaveSpinLock<T>`** — IRQ-disabling spinlock with interior mutability.
//!   The lock disables interrupts on the calling CPU for the duration of the
//!   critical section and restores them on drop. This is required for SMP:
//!   a plain `cli` does *not* prevent an IPI from delivering to the same CPU.
//!
//! - **`without_interrupts(f)`** — RAII helper that disables interrupts for
//!   the body and restores on return. Use for short critical sections that
//!   don't need a mutex (e.g. read-modify-write of a single atomic).
//!
//! - **Lock order** (lower levels acquired first):
//!   `phys → heap → vma → virt → task → cap`
//!   Violating this order can deadlock under load. The order is enforced by
//!   the call graph: code holds a higher-level lock only while *not* holding
//!   a lower-level lock.
//!
//! - **Reentrance is not supported.** Callers must not call into a function
//!   that re-acquires the same lock.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spinlock that disables interrupts on the calling CPU while held.
///
/// On `lock()`: save rflags, `cli`, CAS-spin until acquired, return guard.
/// On `drop()`: store false, restore IF if it was set on entry.
pub struct IrqSaveSpinLock<T> {
    state: UnsafeCell<LockedState<T>>,
}

struct LockedState<T> {
    locked: AtomicBool,
    value: T,
}

// SAFETY: The lock serialises access; the only shared state is the AtomicBool
// used for mutual exclusion. T must itself be `Send` to be moved across
// threads; the lock doesn't introduce new aliases.
unsafe impl<T: Send> Send for IrqSaveSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSaveSpinLock<T> {}

impl<T> IrqSaveSpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: UnsafeCell::new(LockedState {
                locked: AtomicBool::new(false),
                value,
            }),
        }
    }

    /// Acquire the lock, disabling interrupts on this CPU for the duration.
    /// Returns a guard that releases the lock and restores rflags on drop.
    pub fn lock(&self) -> IrqSaveGuard<'_, T> {
        let rflags = save_and_disable_interrupts();
        let state = unsafe { &*self.state.get() };
        while state
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        IrqSaveGuard {
            lock: self,
            rflags,
        }
    }

    /// Try to acquire the lock once. Returns None if contended.
    pub fn try_lock(&self) -> Option<IrqSaveGuard<'_, T>> {
        let rflags = save_and_disable_interrupts();
        let state = unsafe { &*self.state.get() };
        if state
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqSaveGuard {
                lock: self,
                rflags,
            })
        } else {
            restore_interrupts(rflags);
            None
        }
    }
}

/// RAII guard. Holds the lock and the saved rflags; releases both on drop.
pub struct IrqSaveGuard<'a, T> {
    lock: &'a IrqSaveSpinLock<T>,
    rflags: u64,
}

impl<'a, T> Deref for IrqSaveGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the lock invariant guarantees exclusive access.
        unsafe { &(*self.lock.state.get()).value }
    }
}

impl<'a, T> DerefMut for IrqSaveGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the lock invariant guarantees exclusive access.
        unsafe { &mut (*self.lock.state.get()).value }
    }
}

impl<'a, T> Drop for IrqSaveGuard<'a, T> {
    fn drop(&mut self) {
        // Release the lock *before* restoring rflags, so other CPUs can take
        // the lock while we re-enable interrupts.
        unsafe {
            (*self.lock.state.get())
                .locked
                .store(false, Ordering::Release);
        }
        restore_interrupts(self.rflags);
    }
}

/// Backwards-compatible alias of the old simple lock. Most existing call
/// sites will be migrated to `IrqSaveSpinLock<T>`; the alias remains for
/// the rare case where a no-data lock is needed (e.g. a flag).
pub type SpinLockIrq = IrqSaveSpinLock<()>;

// ---- Inline helpers ----

#[inline]
fn save_and_disable_interrupts() -> u64 {
    let rflags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {rflags}",
            "cli",
            rflags = out(reg) rflags,
            options(nomem, preserves_flags),
        );
    }
    rflags
}

#[inline]
fn restore_interrupts(rflags: u64) {
    unsafe {
        if rflags & 0x200 != 0 {
            core::arch::asm!("sti", options(nomem, preserves_flags));
        } else {
            core::arch::asm!("push {rflags}; popfq", rflags = in(reg) rflags, options(nomem, preserves_flags));
        }
    }
}

/// Run `f` with interrupts disabled on this CPU. Restores the previous
/// state on return. Use for short critical sections that don't need a
/// mutex.
#[inline]
pub fn without_interrupts<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let rflags = save_and_disable_interrupts();
    let result = f();
    restore_interrupts(rflags);
    result
}
