// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Platform-neutral cleanup-state regression tests.

use super::*;

/// Prove the first cleanup failure retries and the second failure is
/// terminal while native code retains the child for its Drop fallback.
#[test]
fn cleanup_state_retries_once() {
    assert_eq!(
        cleanup_decision(CleanupPhase::FirstAttempt, CleanupPoll::Error),
        CleanupPhaseResult::Retry
    );
    assert_eq!(
        cleanup_decision(CleanupPhase::RetryAttempt, CleanupPoll::Deadline),
        CleanupPhaseResult::Failed
    );
    assert_eq!(
        cleanup_decision(CleanupPhase::RetryAttempt, CleanupPoll::Reaped),
        CleanupPhaseResult::Reaped
    );
}

/// Prove deadlines and poll errors retain child ownership, while only an
/// explicit OS reap permits the owner to release the process handle.
#[test]
fn cleanup_ownership_requires_reap() {
    assert_eq!(
        child_ownership(CleanupPoll::Deadline),
        CleanupOwnership::Owned
    );
    assert_eq!(child_ownership(CleanupPoll::Error), CleanupOwnership::Owned);
    assert_eq!(
        child_ownership(CleanupPoll::Reaped),
        CleanupOwnership::Reaped
    );
    assert_eq!(
        child_ownership(CleanupPoll::GroupMembers),
        CleanupOwnership::Owned
    );
}

/// Prove the marker cleanup contract rejects unsafe identities before any
/// workflow kill operation can be attempted.
#[test]
fn marker_identity_requires_non_reserved_group() {
    assert!(marker_identity_safe(1234, 42));
    assert!(!marker_identity_safe(1, 42));
    assert!(!marker_identity_safe(42, 42));
}

/// Prove cleanup state never reads a pipe before the nonblocking invariant
/// is established, even when the helper remains enabled.
#[test]
fn blocking_pipe_state_disables_pump() {
    assert!(!diagnostic_pump_allowed(true, false, false));
    assert!(!diagnostic_pump_allowed(true, true, true));
    assert!(diagnostic_pump_allowed(true, false, true));
}
