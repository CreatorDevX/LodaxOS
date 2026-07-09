//! Synchronization primitives for the LodaxOS kernel.
//!
//! ## Locking discipline
//!
//! - **`IrqSaveSpinLock<T>`** -- IRQ-disabling spinlock with interior mutability.
//!   The lock disables interrupts on the calling CPU for the duration of the
//!   critical section and restores them on drop. This is required for SMP:
//!   a plain `cli` does *not* prevent an IPI from delivering to the same CPU.
//!
//! - **Lock order** (lower levels acquired first):
//!   `phys -> heap -> vma -> virt -> task -> cap`
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
#[repr(C, align(8))]
pub struct IrqSaveSpinLock<T> {
    state: UnsafeCell<LockedState<T>>,
}

#[repr(C, align(8))]
struct LockedState<T> {
    locked: AtomicBool,
    value: T,
}

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

    /// Try to acquire the lock. Returns None if already held.
    pub fn try_lock(&self) -> Option<IrqSaveGuard<'_, T>> {
        let rflags = save_and_disable_interrupts();
        let state = unsafe { &*self.state.get() };
        if state
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqSaveGuard { lock: self, rflags })
        } else {
            restore_interrupts(rflags);
            None
        }
    }

    /// Unsafe direct access to the inner value without locking.
    /// Caller must guarantee exclusive access via external synchronization.
    ///
    /// # Safety
    /// - Caller must ensure no other code accesses the inner value concurrently.
    /// - Must not be called concurrently with `lock()` or other `unsafe_get()` calls.
    /// - Must not re-enter: calling `lock()` while a lock is already held deadlocks.
    pub unsafe fn unsafe_get(&self) -> &mut T {
        &mut (*self.state.get()).value
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
        unsafe { &(*self.lock.state.get()).value }
    }
}

impl<'a, T> DerefMut for IrqSaveGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut (*self.lock.state.get()).value }
    }
}

impl<'a, T> Drop for IrqSaveGuard<'a, T> {
    fn drop(&mut self) {
        unsafe {
            (*self.lock.state.get())
                .locked
                .store(false, Ordering::Release);
        }
        restore_interrupts(self.rflags);
    }
}

pub type SpinLockIrq = IrqSaveSpinLock<()>;

/// A `Sync`-safe wrapper around `UnsafeCell` for use in `static` items.
/// This is appropriate when the inner value is only accessed from a single
/// CPU at a time (e.g., per-CPU data) or when external synchronization
/// (e.g., an outer lock) prevents concurrent access.
#[repr(transparent)]
pub struct SyncUnsafeCell<T>(UnsafeCell<T>);

unsafe impl<T: Send> Sync for SyncUnsafeCell<T> {}
unsafe impl<T: Send> Send for SyncUnsafeCell<T> {}

impl<T> SyncUnsafeCell<T> {
    pub const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}

#[inline]
fn save_and_disable_interrupts() -> u64 {
    let rflags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {rflags}",
            "cli",
            rflags = out(reg) rflags,
            options(preserves_flags),
        );
    }
    rflags
}

#[inline]
fn restore_interrupts(rflags: u64) {
    use x86_64::instructions::interrupts;
    if rflags & 0x200 != 0 {
        interrupts::enable();
    } else {
        unsafe { x86_64::registers::rflags::write(x86_64::registers::rflags::RFlags::from_bits_truncate(rflags)); }
    }
}
