/// Capability system — the kernel's policy boundary.
///
/// Mechanism lives here (who can do what check). Policy is
/// **static-only** in v1 — each task's cap set is granted at
/// creation time and checked against requested operations.
///
/// When IPC and a separate policy process are added later, the
/// static check will be augmented with a dynamic policy hook
/// via the shared mailbox.
///
/// The static check is always run:
///   - early-init bypass: if `!task::is_initialized()`, allow
///   - BSP bypass: if subject 0 isn't registered, allow (during
///     `task::init_idle_task` the stack is allocated BEFORE the
///     Task struct is written)
///   - normal: check the cap bits in the subject's `Caps` field
///
/// Subject identity on ring 0 is a `TaskId` (== index into the
/// kernel task table). When ring 3 is added, the same struct will
/// cover process identities (a process inherits its creator's caps).

use lodaxos_system::{CapError, CapOp, Caps};
use crate::task;

/// Return the current subject (task id). On ring 0, this is the index
/// of the running task in the kernel task table.
#[inline]
pub fn current_subject() -> u32 {
    task::current_task_id() as u32
}

/// Read the cap set of `subject`. `None` if subject doesn't exist.
pub fn caps_of(subject: u32) -> Option<Caps> {
    task::task_caps(subject as usize)
}

/// Static-only authorization check.
///
/// Returns `Ok(())` if the subject holds the required cap bits. v1
/// is static-only. The function name is kept as `check_and_authorize`
/// for forward compatibility — when IPC is implemented, the dynamic
/// check will be added here.
pub fn check_and_authorize(
    subject: u32,
    required: Caps,
    op: CapOp,
) -> Result<(), CapError> {
    let _ = op; // unused in v1; logged when IPC is implemented
    check_static(subject, required)
}

/// Static-only check (no policy hook). Useful for tests and for very
/// hot paths where a function-pointer call would be too expensive.
#[inline]
pub fn check_static(subject: u32, required: Caps) -> Result<(), CapError> {
    if !task::is_initialized() {
        return Ok(());
    }
    let have = if subject == 0 && caps_of(subject).is_none() {
        Caps::all()
    } else {
        caps_of(subject).ok_or(CapError::UnknownSubject(subject))?
    };
    if have.contains(required) {
        Ok(())
    } else {
        Err(CapError::Denied {
            subject,
            required,
            missing: required.difference(have),
        })
    }
}

/// Grant `add` to `target`. The caller (current subject) must hold
/// `CAP_POLICY_WRITE`. (v1 is static-only; a future IPC-based
/// policy hook will be consulted here.)
pub fn grant_caps(target: u32, add: Caps) -> Result<(), CapError> {
    let caller = current_subject();
    check_and_authorize(
        caller,
        Caps::CAP_POLICY_WRITE,
        lodaxos_system::CapOp::CapGrant { target, cap: 0 },
    )?;
    task::grant_task_caps(target as usize, add)
        .then_some(())
        .ok_or(CapError::UnknownSubject(target))
}

/// Revoke `remove` from `target`. The caller (current subject) must
/// hold `CAP_POLICY_WRITE`.
pub fn revoke_caps(target: u32, remove: Caps) -> Result<(), CapError> {
    let caller = current_subject();
    check_and_authorize(
        caller,
        Caps::CAP_POLICY_WRITE,
        lodaxos_system::CapOp::CapRevoke { target, cap: 0 },
    )?;
    task::revoke_task_caps(target as usize, remove)
        .then_some(())
        .ok_or(CapError::UnknownSubject(target))
}

/// Inspect a task's cap set. The caller (current subject) must hold
/// `CAP_POLICY_READ`. Returns the cap set.
pub fn inspect_caps(target: u32) -> Result<Caps, CapError> {
    let caller = current_subject();
    check_static(caller, Caps::CAP_POLICY_READ)
        .map_err(|_| CapError::NotAuthorised)?;
    caps_of(target).ok_or(CapError::UnknownSubject(target))
}

/// Apply the default cap set for a newly created task.
///
/// v1 default: `Caps::empty()` unless the parent is task 0 (BSP), in
/// which case the child gets `Caps::all()`. A future IPC-based
/// policy hook will be consulted here.
pub fn apply_default_caps(_child: u32, parent: Option<u32>) -> Caps {
    if parent == Some(0) {
        Caps::all()
    } else {
        Caps::empty()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_set_clear_round_trip() {
        let c = Caps::CAP_LOG | Caps::CAP_DEBUG;
        assert!(c.contains(Caps::CAP_LOG));
        assert!(c.contains(Caps::CAP_DEBUG));
        assert!(!c.contains(Caps::CAP_REBOOT));
        let c2 = c & !Caps::CAP_LOG;
        assert!(!c2.contains(Caps::CAP_LOG));
        assert!(c2.contains(Caps::CAP_DEBUG));
    }

    #[test]
    fn static_check_denies_missing_cap() {
        let s = 0u32;
        let _ = task::set_task_caps(s as usize, Caps::CAP_LOG);
        let result = check_static(s, Caps::CAP_REBOOT);
        assert!(matches!(result, Err(CapError::Denied { .. })));
        let _ = task::set_task_caps(s as usize, Caps::all());
    }

    #[test]
    fn static_check_allows_held_cap() {
        let s = 0u32;
        let _ = task::set_task_caps(s as usize, Caps::CAP_REBOOT | Caps::CAP_HALT);
        let result = check_static(s, Caps::CAP_REBOOT);
        assert!(result.is_ok());
        let _ = task::set_task_caps(s as usize, Caps::all());
    }

    #[test]
    fn on_create_returns_initial_caps() {
        assert_eq!(apply_default_caps(1, Some(0)), Caps::all());
        assert_eq!(apply_default_caps(2, Some(1)), Caps::empty());
    }

    #[test]
    fn grant_requires_policy_write() {
        let s = 0u32;
        let _ = task::set_task_caps(s as usize, Caps::CAP_REBOOT);
        let result = grant_caps(0, Caps::CAP_LOG);
        assert!(matches!(result, Err(CapError::NotAuthorised)));
        let _ = task::set_task_caps(s as usize, Caps::all());
    }
}
