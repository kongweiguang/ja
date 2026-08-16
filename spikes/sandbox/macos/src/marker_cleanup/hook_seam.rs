// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Portable group-hook/evidence state machine used by the macOS cleanup path.
//!
//! The operating-system code supplies identity queries, signals and durable
//! marker operations.  This module keeps the ordering invariant independent of
//! those APIs so a deterministic multi-group seam can run on the Windows host
//! as well as on native macOS.

/// The only successful evidence state is reached after the caller confirms
/// residual disappearance and then closes the corresponding evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GroupEvidenceDisposition {
    Closed,
    Retained,
}

/// Record the observable state of one group after the hook and residual phase.
/// The key is intentionally supplied by the caller so this module never
/// stores paths, PIDs as strings, or other unbounded diagnostic data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GroupCleanupObservation<K> {
    pub(crate) key: K,
    pub(crate) proof_bound: bool,
    pub(crate) residual_confirmed: bool,
    pub(crate) disposition: GroupEvidenceDisposition,
    pub(crate) failure: Option<&'static str>,
}

/// Drive the shared hook/residual/evidence ordering for a bounded group list.
///
/// A `Continue` release is deliberately not a failure: it means the caller
/// must use its normal identity-checked cleanup path.  Evidence is closed only
/// after that path reports residual disappearance; any hook, residual, or
/// close failure retains the group evidence for the next recovery pass.
pub(crate) fn drive_group_cleanup_sequence<I, K, R, Hook, Residual, Close, KeyOf, Bound>(
    groups: &[I],
    hook: &mut Hook,
    residual: &mut Residual,
    close: &mut Close,
    key_of: &mut KeyOf,
    is_bound: &mut Bound,
) -> Vec<GroupCleanupObservation<K>>
where
    Hook: FnMut(&I) -> Result<R, &'static str>,
    Residual: FnMut(&I) -> Result<(), &'static str>,
    Close: FnMut(&I) -> Result<(), &'static str>,
    KeyOf: FnMut(&I) -> K,
    Bound: FnMut(&R, &I) -> bool,
{
    let mut observations = Vec::with_capacity(groups.len());
    for identity in groups {
        let key = key_of(identity);
        let release = match hook(identity) {
            Ok(release) => release,
            Err(error) => {
                observations.push(GroupCleanupObservation {
                    key,
                    proof_bound: false,
                    residual_confirmed: false,
                    disposition: GroupEvidenceDisposition::Retained,
                    failure: Some(error),
                });
                continue;
            }
        };
        let proof_bound = is_bound(&release, identity);
        let residual_confirmed = match residual(identity) {
            Ok(()) => true,
            Err(error) => {
                observations.push(GroupCleanupObservation {
                    key,
                    proof_bound,
                    residual_confirmed: false,
                    disposition: GroupEvidenceDisposition::Retained,
                    failure: Some(error),
                });
                continue;
            }
        };
        match close(identity) {
            Ok(()) => observations.push(GroupCleanupObservation {
                key,
                proof_bound,
                residual_confirmed,
                disposition: GroupEvidenceDisposition::Closed,
                failure: None,
            }),
            Err(error) => observations.push(GroupCleanupObservation {
                key,
                proof_bound,
                residual_confirmed,
                disposition: GroupEvidenceDisposition::Retained,
                failure: Some(error),
            }),
        }
    }
    observations
}

#[cfg(test)]
mod tests {
    use super::{GroupEvidenceDisposition, drive_group_cleanup_sequence};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeIdentity {
        pid: u32,
        pgid: i32,
        uid: u32,
        comm: &'static str,
        start_identity: &'static str,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeKey {
        pid: u32,
        pgid: i32,
        uid: u32,
        comm: &'static str,
        start_identity: &'static str,
    }

    impl FakeKey {
        /// Capture every PID-reuse dimension so a stale first-group proof
        /// cannot authorize a later group with the same numeric PID.
        fn from_identity(identity: &FakeIdentity) -> Self {
            Self {
                pid: identity.pid,
                pgid: identity.pgid,
                uid: identity.uid,
                comm: identity.comm,
                start_identity: identity.start_identity,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum FakeRelease {
        Continue,
        Reaped(FakeKey),
    }

    /// Exercise the same hook/residual/evidence stage used by production
    /// cleanup with two groups, a repeated callback, and PID-reuse variants.
    /// The test is portable because only the process/evidence backend is fake;
    /// the ordering function is the production seam called by the macOS path.
    #[test]
    fn cleanup_hook_state_machine_multi_group_is_target_bound() {
        let first = FakeIdentity {
            pid: 41,
            pgid: 51,
            uid: 7,
            comm: "worker",
            start_identity: "first-start",
        };
        let second = FakeIdentity {
            pgid: 52,
            ..first.clone()
        };
        let changed_start = FakeIdentity {
            start_identity: "reused-start",
            ..first.clone()
        };
        let changed_comm = FakeIdentity {
            comm: "other-worker",
            ..first.clone()
        };
        let changed_uid = FakeIdentity {
            uid: 8,
            ..first.clone()
        };
        let changed_group = FakeIdentity {
            pgid: 53,
            ..first.clone()
        };
        let groups = vec![
            first.clone(),
            first.clone(),
            second.clone(),
            changed_start.clone(),
            changed_comm.clone(),
            changed_uid.clone(),
            changed_group.clone(),
        ];
        let first_key = FakeKey::from_identity(&first);
        let mut hook_calls = 0;
        let mut hook = |identity: &FakeIdentity| {
            let release = match hook_calls {
                0 => FakeRelease::Reaped(FakeKey::from_identity(identity)),
                1 | 2 => FakeRelease::Continue,
                // A stale proof is injected for the reuse cases to prove the
                // production bound check cannot mistake it for a new ACK.
                _ => FakeRelease::Reaped(first_key.clone()),
            };
            hook_calls += 1;
            Ok(release)
        };
        let mut residual_calls = Vec::new();
        let second_key = FakeKey::from_identity(&second);
        let mut residual = |identity: &FakeIdentity| {
            residual_calls.push(FakeKey::from_identity(identity));
            if FakeKey::from_identity(identity) == second_key {
                Err("marker-residual")
            } else {
                Ok(())
            }
        };
        let mut closed = Vec::new();
        let mut close = |identity: &FakeIdentity| {
            closed.push(FakeKey::from_identity(identity));
            Ok(())
        };
        let mut key_of = FakeKey::from_identity;
        let mut is_bound = |release: &FakeRelease, identity: &FakeIdentity| match release {
            FakeRelease::Continue => false,
            FakeRelease::Reaped(key) => key == &FakeKey::from_identity(identity),
        };
        let observations = drive_group_cleanup_sequence(
            &groups,
            &mut hook,
            &mut residual,
            &mut close,
            &mut key_of,
            &mut is_bound,
        );

        assert_eq!(observations.len(), groups.len());
        assert!(observations[0].proof_bound);
        assert!(observations[0].residual_confirmed);
        assert_eq!(
            observations[0].disposition,
            GroupEvidenceDisposition::Closed
        );
        assert!(!observations[1].proof_bound);
        assert!(!observations[2].proof_bound);
        assert_eq!(
            observations[2].disposition,
            GroupEvidenceDisposition::Retained
        );
        assert_eq!(observations[2].failure, Some("marker-residual"));
        assert!(observations[3..].iter().all(|observation| {
            !observation.proof_bound
                && observation.residual_confirmed
                && observation.disposition == GroupEvidenceDisposition::Closed
        }));
        assert!(residual_calls.contains(&second_key));
        assert!(!closed.contains(&second_key));
        assert!(closed.contains(&FakeKey::from_identity(&changed_start)));
        assert!(closed.contains(&FakeKey::from_identity(&changed_comm)));
        assert!(closed.contains(&FakeKey::from_identity(&changed_uid)));
        assert!(closed.contains(&FakeKey::from_identity(&changed_group)));
    }
}
