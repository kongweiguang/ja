// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Platform-neutral lifecycle state used by the native sandbox probe.

/// Cleanup phases are kept separate from `Child` so their retry contract can
/// be tested without spawning a platform-specific process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupPhase {
    FirstAttempt,
    RetryAttempt,
}

/// A status observed while a kill/reap attempt owns the child handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupPoll {
    Reaped,
    GroupMembers,
    Deadline,
    Error,
}

/// Keep the direct child handle owned until the OS reports a reap; a timeout
/// or poll error is never equivalent to an exited child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupOwnership {
    Owned,
    Reaped,
}

/// Result of the pure cleanup state transition used by native cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupPhaseResult {
    Reaped,
    Retry,
    Failed,
}

/// Decide whether a failed bounded wait gets one final kill/reap attempt; the
/// second failure preserves ownership for `Drop` rather than dropping an
/// unconfirmed live child.
pub(super) fn cleanup_decision(phase: CleanupPhase, poll: CleanupPoll) -> CleanupPhaseResult {
    match (phase, child_ownership(poll)) {
        (_, CleanupOwnership::Reaped) => CleanupPhaseResult::Reaped,
        (CleanupPhase::FirstAttempt, CleanupOwnership::Owned) => CleanupPhaseResult::Retry,
        (CleanupPhase::RetryAttempt, CleanupOwnership::Owned) => CleanupPhaseResult::Failed,
    }
}

/// Translate a bounded poll into ownership state so every non-reaped path
/// remains responsible for the child and can retry or perform final reap.
pub(super) fn child_ownership(poll: CleanupPoll) -> CleanupOwnership {
    match poll {
        CleanupPoll::Reaped => CleanupOwnership::Reaped,
        CleanupPoll::GroupMembers | CleanupPoll::Deadline | CleanupPoll::Error => {
            CleanupOwnership::Owned
        }
    }
}

/// Accept marker identities only when workflow cleanup cannot target PID 1 or
/// the current probe process itself.
pub(super) fn marker_identity_safe(process_group: i32, own_pid: i32) -> bool {
    process_group > 1 && process_group != own_pid
}

/// Permit reads only after both pipes were proven nonblocking; cleanup paths
/// therefore cannot accidentally call Read on an unknown blocking descriptor.
pub(super) fn diagnostic_pump_allowed(
    enabled: bool,
    truncated: bool,
    pipes_nonblocking: bool,
) -> bool {
    enabled && !truncated && pipes_nonblocking
}
