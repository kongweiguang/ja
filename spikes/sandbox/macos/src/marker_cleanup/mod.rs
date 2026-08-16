// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native marker cleanup shared by the workflow and its fixture tests.

mod fd;
mod hook_seam;
mod marker;
mod process;
mod process_scan;

#[cfg(target_os = "macos")]
mod fixture;

#[cfg(test)]
use marker::scan_root;
use marker::{
    MarkerRecord, PendingMarker, parse_cleaned_record_from_file, scan_root_from_directory,
};
use process::{
    GroupSignalRelease, ProcessIdentity, ProcessIdentityKey, ProcessState, current_pgid,
    probe_pid_group_until, query_identity_until, terminate_group_with_identity_fallback,
    terminate_group_with_identity_fallback_hook,
};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Stable identity data that a residual process-table scan may compare
/// without exposing platform-specific process output to callers.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ControlledProcessIdentity {
    pub pid: u32,
    pub pgid: i32,
    pub uid: u32,
    pub comm: String,
    pub start_identity: String,
}

/// Query one already-controlled PID through the bounded production identity
/// path so scope evidence never relies on an unbounded `ps` capture.
pub fn query_controlled_identity(pid: u32) -> Result<ControlledProcessIdentity, &'static str> {
    let identity = process::query_identity(pid)?;
    Ok(ControlledProcessIdentity {
        pid: identity.pid,
        pgid: identity.pgid,
        uid: identity.uid,
        comm: identity.comm,
        start_identity: identity.start_identity,
    })
}

/// Reuse the production identity-checked group/direct cleanup for native
/// fixtures, so a setsid descendant cannot be killed solely by a stale PID.
pub fn terminate_controlled_identity(
    identity: &ControlledProcessIdentity,
    deadline: Duration,
) -> Result<(), &'static str> {
    let captured = ProcessIdentity {
        pid: identity.pid,
        pgid: identity.pgid,
        uid: identity.uid,
        comm: identity.comm.clone(),
        start_identity: identity.start_identity.clone(),
    };
    terminate_group_with_identity_fallback(
        &captured,
        std::slice::from_ref(&captured),
        Instant::now() + deadline,
    )
}

const CLEANUP_DEADLINE: Duration = Duration::from_secs(20);
const MAX_MARKER_RECORDS: usize = 64;
const MAX_MARKER_GROUP_MEMBERS: usize = 32;
const MAX_PENDING_MARKERS: usize = 64;
const MARKER_MODE: u32 = 0o600;
const CLEANUP_SYSCALL_RESERVE: Duration = Duration::from_millis(250);
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;

type MarkerCleanupKey = (u32, u128, u32, i32, u64, u64, u64, u32, u32, u8);
type GroupSignalHook<'a> =
    dyn FnMut(&ProcessIdentity) -> Result<GroupSignalRelease, &'static str> + 'a;

/// Build a complete marker identity for deduplication.  Owner/nonce alone is
/// insufficient because suffixes, PGIDs, or inode replacements can represent
/// distinct cleanup obligations in the same scan.
fn marker_cleanup_key(record: &MarkerRecord) -> MarkerCleanupKey {
    let suffix = match record.suffix.as_str() {
        "marker" => 1,
        "fallback" => 2,
        "emergency" => 3,
        _ => 0,
    };
    (
        record.owner_pid,
        record.nonce,
        record.pid,
        record.pgid,
        record.file_identity.dev,
        record.file_identity.ino,
        record.file_identity.nlink,
        record.file_identity.mode,
        record.file_identity.uid,
        suffix,
    )
}

/// Run production cleanup against the runner marker root; all observable
/// report values are fixed categories so paths and marker contents never leak.
pub fn cleanup_markers(
    root: &Path,
    report: &Path,
    allow_fixture: bool,
) -> Result<(), &'static str> {
    cleanup_markers_until(
        root,
        report,
        allow_fixture,
        Instant::now() + CLEANUP_DEADLINE,
    )
}

/// Keep the one absolute cleanup budget across root verification, marker
/// discovery, process queries, signals, waits, unlinks, syncs and reporting;
/// callers such as the fixture pass their own shared deadline so failure
/// finalization cannot silently restart a fresh timeout for each phase.
pub(super) fn cleanup_markers_until(
    root: &Path,
    report: &Path,
    allow_fixture: bool,
    cleanup_deadline: Instant,
) -> Result<(), &'static str> {
    cleanup_markers_until_impl(root, report, allow_fixture, cleanup_deadline, None)
}

/// Run cleanup with a bounded callback that is invoked immediately after a
/// target group signal succeeds.  The descendant fixture uses this narrow
/// handshake to let its supervisor reap a killed child before the production
/// identity wait checks the direct PID; normal workflow callers pass no hook.
pub(super) fn cleanup_markers_until_with_group_signal_hook(
    root: &Path,
    report: &Path,
    allow_fixture: bool,
    cleanup_deadline: Instant,
    on_group_signal: &mut GroupSignalHook<'_>,
) -> Result<(), &'static str> {
    cleanup_markers_until_impl(
        root,
        report,
        allow_fixture,
        cleanup_deadline,
        Some(on_group_signal),
    )
}

/// Keep the standard and fixture cleanup paths on one implementation so the
/// callback cannot bypass root, identity, deadline or evidence invariants.
fn cleanup_markers_until_impl(
    root: &Path,
    report: &Path,
    allow_fixture: bool,
    cleanup_deadline: Instant,
    mut on_group_signal: Option<&mut GroupSignalHook<'_>>,
) -> Result<(), &'static str> {
    let own_pgid = current_pgid();
    if own_pgid <= 1 {
        return write_failure_report(report, "marker-group-unsafe");
    }
    let mut report_categories = BTreeSet::new();
    // Open and verify the private root before enumeration.  The scan receives
    // this same descriptor so authorization and subsequent unlinkat remain in
    // one directory namespace instead of crossing a pathname replacement.
    let (root_directory, root_identity) =
        match open_verified_root_directory_until(root, cleanup_deadline) {
            Ok(value) => value,
            Err(()) => {
                report_categories.insert("marker-root-invalid");
                let _ = write_report_until(report, &report_categories, 0, cleanup_deadline);
                return Err("marker cleanup failed");
            }
        };
    let scan = scan_root_from_directory(root, &root_directory, allow_fixture, cleanup_deadline);
    report_categories.extend(scan.categories.iter().copied());
    let mut cleaned_markers = BTreeSet::new();
    if scan.records.len() > MAX_MARKER_RECORDS || scan.pending.len() > MAX_PENDING_MARKERS {
        // Refuse the complete operation rather than processing a prefix: an
        // attacker must not hide unprocessed identities behind a count cap.
        report_categories.insert("marker-entry-invalid");
        write_report_until(
            report,
            &report_categories,
            scan.records.len(),
            cleanup_deadline,
        )?;
        return Err("marker cleanup failed");
    }

    for record in &scan.records {
        let marker_identity = marker_cleanup_key(record);
        if !cleaned_markers.insert(marker_identity) {
            continue;
        }
        if Instant::now() >= cleanup_deadline {
            report_categories.insert("marker-query-unreaped");
            break;
        }
        if record.pgid == own_pgid || record.pgid <= 1 {
            report_categories.insert("marker-group-unsafe");
            continue;
        }
        let group_records = scan
            .records
            .iter()
            .filter(|candidate| candidate.pgid == record.pgid)
            .collect::<Vec<_>>();
        if group_records.len() > MAX_MARKER_GROUP_MEMBERS {
            report_categories.insert("marker-entry-invalid");
            continue;
        }
        for candidate in &group_records {
            cleaned_markers.insert(marker_cleanup_key(candidate));
        }
        let mut identities = Vec::with_capacity(group_records.len());
        let mut group_valid = true;
        for candidate in &group_records {
            if Instant::now() >= cleanup_deadline {
                report_categories.insert("marker-query-unreaped");
                group_valid = false;
                break;
            }
            let actual = match query_identity_until(candidate.pid, cleanup_deadline) {
                Ok(identity) => identity,
                Err(category) => {
                    report_categories.insert(category);
                    group_valid = false;
                    continue;
                }
            };
            if !identity_matches(candidate, &actual, allow_fixture) {
                report_categories.insert("marker-owner-mismatch");
                group_valid = false;
                continue;
            }
            identities.push(actual);
        }
        if !group_valid || identities.is_empty() {
            continue;
        }
        match on_group_signal.as_deref_mut() {
            Some(hook) => {
                let mut observed_release = GroupSignalRelease::Continue;
                let termination = {
                    let mut capturing_hook = |captured: &ProcessIdentity| {
                        let release = hook(captured)?;
                        observed_release = release.clone();
                        Ok(release)
                    };
                    terminate_group_with_identity_fallback_hook(
                        &identities[0],
                        &identities,
                        cleanup_deadline,
                        &mut capturing_hook,
                    )
                };
                let hook_result = {
                    let mut context = HookedGroupContext {
                        root,
                        root_directory: &root_directory,
                        root_identity,
                        records: &group_records,
                        categories: &mut report_categories,
                        deadline: cleanup_deadline,
                    };
                    finish_hooked_group(&mut context, &identities[0], observed_release, termination)
                };
                if let Err(category) = hook_result {
                    report_categories.insert(category);
                }
            }
            None => match terminate_group_with_identity_fallback(
                &identities[0],
                &identities,
                cleanup_deadline,
            ) {
                Ok(()) => {
                    if remove_identity_markers(
                        root,
                        &root_directory,
                        root_identity,
                        &group_records,
                        &mut report_categories,
                        cleanup_deadline,
                    )
                    .is_err()
                    {
                        report_categories.insert("marker-remove-failed");
                    }
                }
                Err(category) => {
                    report_categories.insert(category);
                }
            },
        }
    }

    // Pending activation files never contain a trusted process identity and
    // are removed without signals; cleaned/recovery records carry the original
    // identity and must complete the bounded liveness/cleanup path below.
    let mut completed_cleaned_entries = BTreeSet::new();
    let mut completed_pending_entries = BTreeSet::new();
    let mut blocked_cleaned_entries = BTreeSet::new();

    // A malformed cleaned/recovery image is not independently deletable.  A
    // valid sibling must first be reopened, reparsed and durably synced so a
    // short-write candidate can never become the last surviving evidence.
    for index in 0..scan.pending.len() {
        let pending = &scan.pending[index];
        if !pending.cleaned || pending.damaged {
            continue;
        }
        if Instant::now() >= cleanup_deadline {
            report_categories.insert("marker-query-unreaped");
            break;
        }
        let Some(pending_name) = marker_name(&pending.path).ok().map(ToOwned::to_owned) else {
            report_categories.insert("marker-entry-invalid");
            continue;
        };
        if completed_cleaned_entries.contains(&pending_name) {
            continue;
        }
        let counterpart = recovery_backup_names(&pending_name)
            .ok()
            .map(|(counterpart, _)| counterpart);
        let damaged_index = counterpart.as_ref().and_then(|counterpart| {
            scan.pending.iter().position(|candidate| {
                candidate.cleaned
                    && candidate.damaged
                    && candidate.cleaned_record.is_none()
                    && marker_name(&candidate.path)
                        .ok()
                        .is_some_and(|name| name == counterpart.as_slice())
            })
        });
        if let Some(damaged_index) = damaged_index {
            let damaged = &scan.pending[damaged_index];
            if let Err(category) = isolate_damaged_sibling(
                root,
                &root_directory,
                root_identity,
                pending,
                damaged,
                allow_fixture,
                cleanup_deadline,
            ) {
                report_categories.insert(category);
                blocked_cleaned_entries.insert(pending_name);
                continue;
            }
            if let Some(counterpart) = counterpart {
                completed_cleaned_entries.insert(counterpart);
            }
        }
        let result = remove_pending_marker(
            root,
            &root_directory,
            root_identity,
            pending,
            allow_fixture,
            cleanup_deadline,
        );
        if result.is_ok() {
            completed_cleaned_entries.insert(pending_name.clone());
            if let Ok((counterpart, _)) = recovery_backup_names(&pending_name) {
                completed_cleaned_entries.insert(counterpart);
            }
        }
        if let Err(category) = result {
            report_categories.insert(category);
        }
    }

    // Pending activation files and unpaired damaged images are handled only
    // after valid cleaned entries have had a chance to prove and isolate their
    // sibling.  An unpaired malformed image remains a visible failure.
    for pending in &scan.pending {
        let pending_name = marker_name(&pending.path).ok().map(ToOwned::to_owned);
        if pending.cleaned {
            if pending_name
                .as_ref()
                .is_some_and(|name| completed_cleaned_entries.contains(name))
                || pending_name
                    .as_ref()
                    .is_some_and(|name| blocked_cleaned_entries.contains(name))
            {
                continue;
            }
            if pending.damaged {
                report_categories.insert("marker-stat-invalid");
                continue;
            }
            // A valid entry without a malformed sibling was already handled
            // in the first pass; a second attempt would only duplicate a
            // durable transaction and can race its pre-scan twin.
            continue;
        }
        if pending_name
            .as_ref()
            .is_some_and(|name| completed_pending_entries.contains(name))
        {
            continue;
        }
        if Instant::now() >= cleanup_deadline {
            report_categories.insert("marker-pending");
            report_categories.insert("marker-query-unreaped");
            break;
        }
        let result = remove_pending_marker(
            root,
            &root_directory,
            root_identity,
            pending,
            allow_fixture,
            cleanup_deadline,
        );
        if let Ok(outcome) = &result {
            completed_pending_entries.extend(outcome.completed_names.iter().cloned());
        }
        if let Err(category) = result {
            report_categories.insert("marker-pending");
            report_categories.insert(category);
        }
    }
    // The marker directory entry is part of the durable cleanup evidence; a
    // successful unlink without a parent-directory sync cannot be reported as
    // complete after a runner crash/restart boundary.
    if !has_cleanup_budget(cleanup_deadline, CLEANUP_SYSCALL_RESERVE)
        || fd::sync_directory(&root_directory).is_err()
        || Instant::now() >= cleanup_deadline
    {
        report_categories.insert("marker-remove-failed");
    }
    write_report_until(
        report,
        &report_categories,
        scan.records.len(),
        cleanup_deadline,
    )?;
    if report_categories.is_empty() {
        Ok(())
    } else {
        Err("marker cleanup failed")
    }
}

/// Execute the native same-implementation fixture; the workflow invokes this
/// path rather than maintaining a second shell implementation of cleanup.
#[cfg(target_os = "macos")]
pub fn run_fixture() -> Result<(), &'static str> {
    fixture::run()
}

/// Enter the macOS-only child supervisor used by the real descendant fixture;
/// exposing this narrow mode keeps the launcher in the same audited binary and
/// lets the parent reap the descendant after its private group is killed.
#[cfg(target_os = "macos")]
pub fn run_fixture_launcher() -> ! {
    fixture::run_fixture_launcher()
}

/// Match the marker's immutable identity against one fresh process query;
/// process-name and start-time checks prevent PID reuse from receiving a kill.
fn identity_matches(record: &MarkerRecord, actual: &ProcessIdentity, allow_fixture: bool) -> bool {
    actual.pid == record.pid
        && actual.pgid == record.pgid
        && actual.uid == process::current_uid()
        && actual.uid == record.file_identity.uid
        && actual.start_identity == record.start_identity
        && ((record.executable_kind == "log"
            && matches!(actual.comm.as_str(), "log" | "/usr/bin/log"))
            || (allow_fixture
                && record.executable_kind == "fixture"
                && matches!(
                    actual.comm.as_str(),
                    "sh" | "/bin/sh" | "sleep" | "/bin/sleep"
                )))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootDirectoryIdentity {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
}

/// Open and retain the private marker root so every deletion is anchored to
/// one descriptor; a later pathname replacement cannot redirect `unlinkat`.
#[cfg(test)]
fn open_verified_root_directory(root: &Path) -> Result<(File, RootDirectoryIdentity), ()> {
    open_verified_root_directory_until(root, Instant::now() + CLEANUP_DEADLINE)
}

/// Open and validate the marker root while honoring the caller's absolute
/// budget; every metadata operation is bounded so a hostile filesystem cannot
/// consume the time reserved for process cleanup or durable evidence.
fn open_verified_root_directory_until(
    root: &Path,
    deadline: Instant,
) -> Result<(File, RootDirectoryIdentity), ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(root)
        .map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let metadata = file.metadata().map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let path_metadata = fs::symlink_metadata(root).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let identity = RootDirectoryIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    let path_identity = RootDirectoryIdentity {
        dev: path_metadata.dev(),
        ino: path_metadata.ino(),
        nlink: path_metadata.nlink(),
        mode: path_metadata.permissions().mode() & 0o777,
        uid: path_metadata.uid(),
    };
    if !metadata.file_type().is_dir()
        || path_metadata.file_type().is_symlink()
        || identity.uid != process::current_uid()
        || identity.mode != 0o700
        || identity != path_identity
    {
        return Err(());
    }
    Ok((file, identity))
}

/// Compare the held descriptor identity with the current pathname without
/// spending time beyond the shared cleanup deadline.
fn verify_root_path_until(
    root: &Path,
    expected: RootDirectoryIdentity,
    deadline: Instant,
) -> Result<(), ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    let metadata = fs::symlink_metadata(root).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let actual = RootDirectoryIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    (!metadata.file_type().is_symlink() && actual == expected)
        .then_some(())
        .ok_or(())
}

/// Remove only the exact sibling marker names derived from a validated owner
/// and nonce, after the direct PID and complete group both report ESRCH.
fn remove_identity_markers(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    records: &[&MarkerRecord],
    categories: &mut BTreeSet<&'static str>,
    deadline: Instant,
) -> Result<(), ()> {
    let mut result = Ok(());
    for record in records {
        if Instant::now() >= deadline {
            categories.insert("marker-query-unreaped");
            return Err(());
        }
        let name = match marker_name(&record.path) {
            Ok(name) => name,
            Err(()) => {
                categories.insert("marker-remove-failed");
                result = Err(());
                continue;
            }
        };
        let file = match open_verified_marker_at(
            root_directory.as_raw_fd(),
            name,
            record.file_identity,
            deadline,
        ) {
            Ok(file) => file,
            Err(()) => {
                categories.insert("marker-remove-failed");
                result = Err(());
                continue;
            }
        };
        if unlink_verified_entry(
            root,
            root_directory,
            root_identity,
            &record.path,
            record.file_identity,
            deadline,
        )
        .is_err()
        {
            categories.insert("marker-remove-failed");
            result = Err(());
        }
        // Keep the descriptor alive through unlink so the identity used for
        // the final check remains owned until the path operation completes.
        drop(file);
    }
    result
}

/// Finish one production hook group through the portable residual/evidence
/// state machine.  The real termination already ran before this function;
/// retaining its release in the seam makes the same ordering testable with a
/// controlled backend without duplicating the marker cleanup policy.
struct HookedGroupContext<'a> {
    root: &'a Path,
    root_directory: &'a File,
    root_identity: RootDirectoryIdentity,
    records: &'a [&'a MarkerRecord],
    categories: &'a mut BTreeSet<&'static str>,
    deadline: Instant,
}

fn finish_hooked_group(
    context: &mut HookedGroupContext<'_>,
    identity: &ProcessIdentity,
    observed_release: GroupSignalRelease,
    termination: Result<(), &'static str>,
) -> Result<(), &'static str> {
    let HookedGroupContext {
        root,
        root_directory,
        root_identity,
        records,
        categories,
        deadline,
    } = context;
    let groups = [identity.clone()];
    let mut hook = |_identity: &ProcessIdentity| Ok(observed_release.clone());
    let mut residual = |_identity: &ProcessIdentity| termination;
    let mut close = |_identity: &ProcessIdentity| {
        remove_identity_markers(
            root,
            root_directory,
            *root_identity,
            records,
            categories,
            *deadline,
        )
        .map_err(|_| "marker-remove-failed")
    };
    let mut key_of = ProcessIdentityKey::from_identity;
    let mut is_bound =
        |release: &GroupSignalRelease, current: &ProcessIdentity| release.is_reaped_for(current);
    let observation = hook_seam::drive_group_cleanup_sequence(
        &groups,
        &mut hook,
        &mut residual,
        &mut close,
        &mut key_of,
        &mut is_bound,
    )
    .into_iter()
    .next()
    .ok_or("marker-process-probe-failed")?;
    match observation.disposition {
        hook_seam::GroupEvidenceDisposition::Closed => Ok(()),
        hook_seam::GroupEvidenceDisposition::Retained => {
            Err(observation.failure.unwrap_or("marker-remove-failed"))
        }
    }
}

/// Re-open each marker through the held root fd with no-follow and fstatat
/// checks; the descriptor remains owned through the finite-state transition.
fn open_verified_marker_at(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
    expected: marker::MarkerFileIdentity,
    deadline: Instant,
) -> Result<File, ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    fd::fstatat_no_follow(root_fd, name).map_err(|_| ())?;
    let file = fd::open_at_file(root_fd, name).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let metadata = file.metadata().map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let actual = marker::MarkerFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    fd::fstatat_no_follow(root_fd, name).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    (metadata.file_type().is_file() && actual == expected)
        .then_some(file)
        .ok_or(())
}

/// Apply the same identity recheck to incomplete activation files; a pending
/// marker is never trusted as a process identity, but it is still evidence.
#[derive(Debug, Default)]
struct PendingCleanupOutcome {
    completed_names: Vec<Vec<u8>>,
}

/// Keep pending cleanup fault injection on the same production fd-relative
/// path; the default implementation introduces no test-only behavior.
trait PendingFaultInjector {
    fn before_alias_write(&mut self) -> Result<(), ()>;
    fn before_file_sync(&mut self) -> Result<(), ()>;
    /// Number each destructive transition so a fault can target the final
    /// unlink rather than accidentally exercising only the first edge.
    fn before_directory_sync(&mut self, ordinal: usize) -> Result<(), ()>;
    fn before_unlink(&mut self, ordinal: usize) -> Result<(), ()>;
    fn after_unlink(&mut self, ordinal: usize) -> Result<(), ()>;
    /// Inject a failure between the unlink postcheck and the root-path
    /// revalidation; the caller must then restore durable pending evidence.
    fn before_root_revalidate(&mut self, ordinal: usize) -> Result<(), ()>;
}

struct NoPendingFault;

impl PendingFaultInjector for NoPendingFault {
    fn before_alias_write(&mut self) -> Result<(), ()> {
        Ok(())
    }

    fn before_file_sync(&mut self) -> Result<(), ()> {
        Ok(())
    }

    fn before_directory_sync(&mut self, _ordinal: usize) -> Result<(), ()> {
        Ok(())
    }

    fn before_unlink(&mut self, _ordinal: usize) -> Result<(), ()> {
        Ok(())
    }

    fn after_unlink(&mut self, _ordinal: usize) -> Result<(), ()> {
        Ok(())
    }

    fn before_root_revalidate(&mut self, _ordinal: usize) -> Result<(), ()> {
        Ok(())
    }
}

/// Apply the pending finite-state transition or the cleaned-marker process
/// identity path, keeping pending completion names explicit for stale scans.
fn remove_pending_marker(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    pending: &PendingMarker,
    allow_fixture: bool,
    deadline: Instant,
) -> Result<PendingCleanupOutcome, &'static str> {
    if !pending.cleaned {
        let mut faults = NoPendingFault;
        return remove_pending_evidence_with_fault(
            root,
            root_directory,
            root_identity,
            pending,
            deadline,
            &mut faults,
        )
        .map_err(|_| "marker-remove-failed");
    }
    let record = pending
        .cleaned_record
        .as_ref()
        .ok_or("marker-stat-invalid")?;
    let disposition =
        classify_cleaned_identity(probe_pid_group_until(record.pid, record.pgid, deadline)?)?;
    if disposition == CleanedIdentityDisposition::Active {
        let actual = query_identity_until(record.pid, deadline)?;
        if !identity_matches(record, &actual, allow_fixture) {
            return Err("marker-identity-lost");
        }
        terminate_group_with_identity_fallback(&actual, std::slice::from_ref(&actual), deadline)?;
        if classify_cleaned_identity(probe_pid_group_until(record.pid, record.pgid, deadline)?)?
            != CleanedIdentityDisposition::Gone
        {
            return Err("marker-residual");
        }
    }
    unlink_cleaned_evidence(
        root,
        root_directory,
        root_identity,
        &pending.path,
        pending.file_identity,
        deadline,
    )
    .map_err(|_| "marker-remove-failed")?;
    Ok(PendingCleanupOutcome::default())
}

/// Derive the finite pending state names from either the original pending
/// basename or one of its two recognized aliases.  Active marker parsing is
/// deliberately not consulted for this nested `.pending` state.
struct PendingStateNames {
    base: Vec<u8>,
    recovery: Vec<u8>,
    cleaned: Vec<u8>,
}

/// Derive all names in the bounded pending state machine.
fn pending_state_names(name: &[u8]) -> Result<PendingStateNames, ()> {
    let text = std::str::from_utf8(name).map_err(|_| ())?;
    let base = if marker::parse_pending_name_for_cleanup(text).is_some() {
        name.to_vec()
    } else {
        marker::parse_pending_alias_for_cleanup(text)
            .map(str::as_bytes)
            .map(ToOwned::to_owned)
            .ok_or(())?
    };
    let mut recovery = b".ja-sandbox-recovery.".to_vec();
    recovery.extend_from_slice(&base);
    let mut cleaned = b".ja-sandbox-cleaned.".to_vec();
    cleaned.extend_from_slice(&base);
    if recovery.len() > 255 || cleaned.len() > 255 {
        return Err(());
    }
    Ok(PendingStateNames {
        base,
        recovery,
        cleaned,
    })
}

/// Open a pending or pending-alias entry relative to the verified root and
/// return its owner-only inode identity; no basename is accepted unchecked.
fn open_pending_candidate_at(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
    deadline: Instant,
) -> Result<(File, marker::MarkerFileIdentity), ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    fd::fstatat_no_follow(root_fd, name).map_err(|_| ())?;
    let file = fd::open_at_file(root_fd, name).map_err(|_| ())?;
    let identity = validate_recovery_image(&file)?;
    fd::fstatat_no_follow(root_fd, name).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    Ok((file, identity))
}

/// Read one bounded pending image while preserving its descriptor identity;
/// matching raw bytes is the only safe way to pair incomplete activation
/// evidence that does not yet contain a parseable active marker.
fn read_pending_candidate(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
    deadline: Instant,
) -> Result<Option<(marker::MarkerFileIdentity, Vec<u8>)>, ()> {
    if fd::fstatat_no_follow(root_fd, name).is_err() {
        return if fd::last_errno() == fd::ENOENT {
            Ok(None)
        } else {
            Err(())
        };
    }
    let (mut file, identity) = open_pending_candidate_at(root_fd, name, deadline)?;
    let image = read_marker_image(&mut file, deadline)?;
    file.sync_all().map_err(|_| ())?;
    Ok(Some((identity, image)))
}

/// Publish one pending alias with O_EXCL and file/parent sync before the
/// original entry can be unlinked.  Existing matching aliases are resumed;
/// conflicting bytes fail closed instead of being overwritten.
fn ensure_pending_alias(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    alias: &[u8],
    contents: &[u8],
    deadline: Instant,
    faults: &mut impl PendingFaultInjector,
) -> Result<marker::MarkerFileIdentity, ()> {
    let root_fd = root_directory.as_raw_fd();
    verify_root_path_until(root, root_identity, deadline)?;
    if let Some((identity, existing)) = read_pending_candidate(root_fd, alias, deadline)? {
        if existing != contents {
            return Err(());
        }
        faults.before_directory_sync(0)?;
        fd::sync_directory(root_directory).map_err(|_| ())?;
        if Instant::now() >= deadline {
            return Err(());
        }
        return Ok(identity);
    }
    faults.before_alias_write()?;
    let mut file = fd::create_at_file(root_fd, alias, MARKER_MODE).map_err(|_| ())?;
    let identity = validate_recovery_image(&file)?;
    file.write_all(contents).map_err(|_| ())?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    faults.before_file_sync()?;
    file.sync_all().map_err(|_| ())?;
    drop(file);
    verify_root_path_until(root, root_identity, deadline)?;
    faults.before_directory_sync(0)?;
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let (mut verified, actual) = open_pending_candidate_at(root_fd, alias, deadline)?;
    let image = read_marker_image(&mut verified, deadline)?;
    if actual != identity || image != contents {
        return Err(());
    }
    Ok(actual)
}

/// Keep the pending unlink inputs together so the destructive operation cannot
/// accidentally pair an identity from one root/name with another transition.
struct PendingUnlinkSpec<'a> {
    root: &'a Path,
    root_directory: &'a File,
    root_identity: RootDirectoryIdentity,
    name: &'a [u8],
    expected: marker::MarkerFileIdentity,
    deadline: Instant,
    ordinal: usize,
}

/// Remove one pending state entry directly through the held root fd, with the
/// paired alias retained by the caller until this unlink is durable.
fn unlink_pending_entry(
    spec: PendingUnlinkSpec<'_>,
    faults: &mut impl PendingFaultInjector,
) -> Result<(), ()> {
    let PendingUnlinkSpec {
        root,
        root_directory,
        root_identity,
        name,
        expected,
        deadline,
        ordinal,
    } = spec;
    let file = open_verified_marker_at(root_directory.as_raw_fd(), name, expected, deadline)?;
    drop(file);
    verify_root_path_until(root, root_identity, deadline)?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    faults.before_unlink(ordinal)?;
    fd::unlink_at(root_directory.as_raw_fd(), name).map_err(|_| ())?;
    faults.after_unlink(ordinal)?;
    if fd::fstatat_no_follow(root_directory.as_raw_fd(), name).is_ok()
        || fd::last_errno() != fd::ENOENT
        || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
    {
        return Err(());
    }
    faults.before_root_revalidate(ordinal)?;
    verify_root_path_until(root, root_identity, deadline)?;
    faults.before_directory_sync(ordinal + 1)?;
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    Ok(())
}

/// Remove the last pending evidence only with an immediate durable rollback
/// path.  A post-unlink, root-revalidation, or directory-sync error can occur
/// after the pathname has disappeared; recreating the exact image through the
/// same O_EXCL fd-relative publisher prevents an error path from returning
/// with no restartable evidence.
fn unlink_pending_entry_with_restore(
    spec: PendingUnlinkSpec<'_>,
    contents: &[u8],
    faults: &mut impl PendingFaultInjector,
) -> Result<(), ()> {
    let PendingUnlinkSpec {
        root,
        root_directory,
        root_identity,
        name,
        expected,
        deadline,
        ordinal,
    } = spec;
    if unlink_pending_entry(
        PendingUnlinkSpec {
            root,
            root_directory,
            root_identity,
            name,
            expected,
            deadline,
            ordinal,
        },
        faults,
    )
    .is_ok()
    {
        return Ok(());
    }

    // Recovery must not consume the injected fault a second time: the fault
    // represents the failed destructive transition, while this publisher is
    // the production last-copy invariant that makes the failure recoverable.
    let existing_identity = match read_pending_candidate(root_directory.as_raw_fd(), name, deadline)
    {
        Ok(Some((identity, existing))) if identity == expected && existing == contents => {
            Some(identity)
        }
        Ok(None) => None,
        Ok(Some(_)) | Err(()) => {
            // A replacement inode or conflicting bytes mean the failed
            // transition cannot be safely repaired under the captured identity.
            std::process::abort();
        }
    };
    let mut recovery_faults = NoPendingFault;
    let restored_identity = if let Ok(identity) = ensure_pending_alias(
        root,
        root_directory,
        root_identity,
        name,
        contents,
        deadline,
        &mut recovery_faults,
    ) {
        identity
    } else {
        // Returning with neither the deleted entry nor a durable replacement
        // would make the next bounded cleanup pass unable to reason about the
        // state, so fail closed instead of silently losing its only evidence.
        std::process::abort();
    };
    if existing_identity.is_some() && restored_identity != expected {
        // The path existed before recovery; accepting a different inode would
        // turn a post-unlink fault into an unverified pathname replacement.
        std::process::abort();
    }
    Err(())
}

/// Complete the pending finite-state transition.  Normal pending entries
/// publish a recovery alias first; alias-only states publish the opposite
/// alias first.  Thus the first unlink has a durable sibling, while the final
/// unlink retains a complete image for immediate durable rollback; a restart
/// can recognize any partially completed state.
fn remove_pending_evidence_with_fault<F: PendingFaultInjector>(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    pending: &PendingMarker,
    deadline: Instant,
    faults: &mut F,
) -> Result<PendingCleanupOutcome, ()> {
    let current = marker_name(&pending.path)?;
    let PendingStateNames {
        base,
        recovery,
        cleaned,
    } = pending_state_names(current)?;
    let is_base = current == base.as_slice();
    if pending.pending_recovery == is_base {
        return Err(());
    }
    let (mut current_file, current_identity) = open_verified_marker_at(
        root_directory.as_raw_fd(),
        current,
        pending.file_identity,
        deadline,
    )
    .map(|file| {
        let identity = pending.file_identity;
        (file, identity)
    })?;
    let current_image = read_marker_image(&mut current_file, deadline)?;
    drop(current_file);
    if current_identity != pending.file_identity {
        return Err(());
    }
    verify_root_path_until(root, root_identity, deadline)?;

    if is_base {
        let recovery_entry =
            read_pending_candidate(root_directory.as_raw_fd(), &recovery, deadline)?;
        let cleaned_entry = read_pending_candidate(root_directory.as_raw_fd(), &cleaned, deadline)?;
        if recovery_entry.is_some() && cleaned_entry.is_some() {
            return Err(());
        }
        let (backup_name, backup_identity) = if let Some((identity, image)) = recovery_entry {
            if image != current_image {
                return Err(());
            }
            (recovery, identity)
        } else if let Some((identity, image)) = cleaned_entry {
            if image != current_image {
                return Err(());
            }
            (cleaned, identity)
        } else {
            let identity = ensure_pending_alias(
                root,
                root_directory,
                root_identity,
                &recovery,
                &current_image,
                deadline,
                faults,
            )?;
            (recovery, identity)
        };
        unlink_pending_entry(
            PendingUnlinkSpec {
                root,
                root_directory,
                root_identity,
                name: &base,
                expected: pending.file_identity,
                deadline,
                ordinal: 0,
            },
            faults,
        )?;
        unlink_pending_entry_with_restore(
            PendingUnlinkSpec {
                root,
                root_directory,
                root_identity,
                name: &backup_name,
                expected: backup_identity,
                deadline,
                ordinal: 1,
            },
            &current_image,
            faults,
        )?;
        return Ok(PendingCleanupOutcome {
            completed_names: vec![base, backup_name],
        });
    }

    let base_entry = read_pending_candidate(root_directory.as_raw_fd(), &base, deadline)?;
    if let Some((_base_identity, base_image)) = base_entry {
        if base_image != current_image {
            return Err(());
        }
        let other = if current == recovery.as_slice() {
            &cleaned
        } else {
            &recovery
        };
        if fd::fstatat_no_follow(root_directory.as_raw_fd(), other).is_ok()
            || fd::last_errno() != fd::ENOENT
        {
            return Err(());
        }
        unlink_pending_entry(
            PendingUnlinkSpec {
                root,
                root_directory,
                root_identity,
                name: current,
                expected: pending.file_identity,
                deadline,
                ordinal: 0,
            },
            faults,
        )?;
        return Ok(PendingCleanupOutcome {
            completed_names: vec![current.to_vec()],
        });
    }

    let other = if current == recovery.as_slice() {
        &cleaned
    } else {
        &recovery
    };
    let (other_identity, other_image) = if let Some((identity, image)) =
        read_pending_candidate(root_directory.as_raw_fd(), other, deadline)?
    {
        if image != current_image {
            return Err(());
        }
        (identity, image)
    } else {
        let identity = ensure_pending_alias(
            root,
            root_directory,
            root_identity,
            other,
            &current_image,
            deadline,
            faults,
        )?;
        (identity, current_image.clone())
    };
    if other_image != current_image {
        return Err(());
    }
    unlink_pending_entry(
        PendingUnlinkSpec {
            root,
            root_directory,
            root_identity,
            name: current,
            expected: pending.file_identity,
            deadline,
            ordinal: 0,
        },
        faults,
    )?;
    unlink_pending_entry_with_restore(
        PendingUnlinkSpec {
            root,
            root_directory,
            root_identity,
            name: other,
            expected: other_identity,
            deadline,
            ordinal: 1,
        },
        &current_image,
        faults,
    )?;
    Ok(PendingCleanupOutcome {
        completed_names: vec![current.to_vec(), other.to_vec()],
    })
}

/// Isolate a malformed cleaned/recovery sibling only after the valid member
/// has been reparsed from its expected inode and its parent directory has
/// been synced.  This narrowly repairs a failed restore pair without turning
/// arbitrary stat-invalid files into cleanup targets.
fn isolate_damaged_sibling(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    valid: &PendingMarker,
    damaged: &PendingMarker,
    allow_fixture: bool,
    deadline: Instant,
) -> Result<(), &'static str> {
    if !valid.cleaned
        || valid.damaged
        || valid.cleaned_record.is_none()
        || !damaged.cleaned
        || !damaged.damaged
        || damaged.cleaned_record.is_some()
    {
        return Err("marker-owner-mismatch");
    }
    let valid_name = marker_name(&valid.path).map_err(|_| "marker-entry-invalid")?;
    let damaged_name = marker_name(&damaged.path).map_err(|_| "marker-entry-invalid")?;
    let (expected_damaged_name, _) =
        recovery_backup_names(valid_name).map_err(|_| "marker-owner-mismatch")?;
    if damaged_name != expected_damaged_name.as_slice() {
        return Err("marker-owner-mismatch");
    }

    let valid_file = open_verified_marker_at(
        root_directory.as_raw_fd(),
        valid_name,
        valid.file_identity,
        deadline,
    )
    .map_err(|_| "marker-remove-failed")?;
    // Sync the already verified inode before parsing so the sibling proof is
    // durable even when the subsequent malformed-entry unlink is retried.
    valid_file.sync_all().map_err(|_| "marker-remove-failed")?;
    let actual = parse_cleaned_record_from_file(valid_file, &valid.path, allow_fixture)
        .map_err(|_| "marker-owner-mismatch")?;
    let expected = valid
        .cleaned_record
        .as_ref()
        .ok_or("marker-owner-mismatch")?;
    if !marker_records_match(&actual, expected) {
        return Err("marker-owner-mismatch");
    }
    verify_root_path_until(root, root_identity, deadline).map_err(|_| "marker-remove-failed")?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err("marker-remove-failed");
    }
    fd::sync_directory(root_directory).map_err(|_| "marker-remove-failed")?;
    if Instant::now() >= deadline {
        return Err("marker-remove-failed");
    }

    // The malformed inode is reopened and checked immediately before the
    // fd-relative unlink; a path with the right basename but a different
    // inode cannot be consumed by this recovery transaction.
    let damaged_file = open_verified_marker_at(
        root_directory.as_raw_fd(),
        damaged_name,
        damaged.file_identity,
        deadline,
    )
    .map_err(|_| "marker-remove-failed")?;
    drop(damaged_file);
    verify_root_path_until(root, root_identity, deadline).map_err(|_| "marker-remove-failed")?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err("marker-remove-failed");
    }
    fd::unlink_at(root_directory.as_raw_fd(), damaged_name).map_err(|_| "marker-remove-failed")?;
    if fd::fstatat_no_follow(root_directory.as_raw_fd(), damaged_name).is_ok()
        || fd::last_errno() != fd::ENOENT
        || verify_root_path_until(root, root_identity, deadline).is_err()
        || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
    {
        return Err("marker-remove-failed");
    }
    fd::sync_directory(root_directory).map_err(|_| "marker-remove-failed")?;
    if Instant::now() >= deadline {
        return Err("marker-remove-failed");
    }
    Ok(())
}

/// Compare every parsed marker field, not only the process key, so a durable
/// sibling cannot authorize removal after a same-inode content replacement.
fn marker_records_match(actual: &MarkerRecord, expected: &MarkerRecord) -> bool {
    marker_cleanup_key(actual) == marker_cleanup_key(expected)
        && actual.start_identity == expected.start_identity
        && actual.executable_kind == expected.executable_kind
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanedIdentityDisposition {
    Gone,
    Active,
}

/// Map exact kernel probe states to the only two safe cleanup branches; an
/// unknown, denied, or partially-live group is never treated as gone.
fn classify_cleaned_identity(
    states: (ProcessState, ProcessState),
) -> Result<CleanedIdentityDisposition, &'static str> {
    match states {
        (ProcessState::Empty, ProcessState::Empty) => Ok(CleanedIdentityDisposition::Gone),
        (ProcessState::Present, ProcessState::Present) => Ok(CleanedIdentityDisposition::Active),
        (ProcessState::Empty, ProcessState::Present)
        | (ProcessState::Present, ProcessState::Empty) => Err("marker-residual"),
        (ProcessState::PermissionDenied, _) | (_, ProcessState::PermissionDenied) => {
            Err("marker-eperm")
        }
        (ProcessState::Other(_), _) | (_, ProcessState::Other(_)) => {
            Err("marker-process-probe-failed")
        }
    }
}

/// Garbage-collect one cleaned/recovery entry only after the caller has
/// proved the original PID and PGID are both gone; the held root descriptor
/// prevents a pathname replacement from redirecting the final unlink.
fn unlink_cleaned_evidence(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    path: &Path,
    expected: marker::MarkerFileIdentity,
    deadline: Instant,
) -> Result<(), ()> {
    if Instant::now() >= deadline || path.parent() != Some(root) {
        return Err(());
    }
    let name = marker_name(path)?;
    let mut file = open_verified_marker_at(root_directory.as_raw_fd(), name, expected, deadline)?;
    let contents = read_marker_image(&mut file, deadline)?;
    drop(file);
    verify_root_path_until(root, root_identity, deadline)?;
    // Publish a second complete image before removing the target pathname.
    // The backup intentionally remains until the target unlink and its
    // directory sync are durable; otherwise a restore write fault could make
    // both O_EXCL candidates partial and leave no parseable evidence.
    let (backup, pending) = recovery_backup_names(name)?;
    let backup_identity = prepare_recovery_backup(
        root,
        root_directory,
        root_identity,
        &backup,
        &pending,
        &contents,
        deadline,
    )?;
    // Revalidate and hold the target before the final unlink.  Keeping this
    // descriptor alive does not authorize a pathname replacement; the held
    // root fd and the immediate identity checks below provide that boundary.
    let _target_file =
        open_verified_marker_at(root_directory.as_raw_fd(), name, expected, deadline)?;

    // Both the target and the backup are complete at this point.  Only now is
    // it safe to attempt the target unlink; the backup remains available if
    // any post-unlink check or restore write fails.
    verify_root_path_until(root, root_identity, deadline)?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    if fd::unlink_at(root_directory.as_raw_fd(), name).is_err() {
        let target_is_gone = fd::fstatat_no_follow(root_directory.as_raw_fd(), name).is_err()
            && fd::last_errno() == fd::ENOENT;
        if target_is_gone {
            let outcome = restore_recovery_evidence(
                root,
                root_directory,
                root_identity,
                name,
                &backup,
                &contents,
                deadline,
            )
            .unwrap_or_else(|_| std::process::abort());
            if !outcome.durable_survivor {
                std::process::abort();
            }
        }
        return Err(());
    }

    let entry_gone = fd::fstatat_no_follow(root_directory.as_raw_fd(), name).is_err()
        && fd::last_errno() == fd::ENOENT;
    let final_state_is_durable = recovery_postconditions_are_durable(
        Instant::now() < deadline,
        entry_gone,
        verify_root_path_until(root, root_identity, deadline).is_ok(),
        has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE),
        fd::sync_directory(root_directory).is_ok(),
    );
    if final_state_is_durable {
        // The target is now durably gone, so remove the retained backup last.
        // A failed removal leaves that complete sibling for the next pass;
        // it never becomes a successful zero-evidence result accidentally.
        if remove_recovery_backup(
            root,
            root_directory,
            root_identity,
            &backup,
            backup_identity,
            &contents,
            deadline,
        )
        .is_ok()
        {
            return Ok(());
        }
        if fd::fstatat_no_follow(root_directory.as_raw_fd(), &backup).is_ok() {
            return Err(());
        }
    }

    // If the retained backup disappeared unexpectedly, restore the complete
    // image through O_EXCL.  An inability to restore either image is
    // unrecoverable and aborts rather than returning control with no durable
    // evidence.  Normally the backup remains and this branch is skipped,
    // allowing the next bounded scan to consume it without a second write.
    match restore_recovery_evidence(
        root,
        root_directory,
        root_identity,
        name,
        &backup,
        &contents,
        deadline,
    ) {
        Ok(outcome) if outcome.durable_survivor => return Err(()),
        Ok(_) | Err(()) => {}
    }
    std::process::abort()
}

/// Require every post-unlink durability edge; a single false result keeps the
/// recovery image in the restore path instead of treating a partial delete as
/// success.  The pure boundary is also used to exhaustively test deadline and
/// sync fault combinations without depending on runner timing.
fn recovery_postconditions_are_durable(
    deadline_ok: bool,
    entry_gone: bool,
    root_valid: bool,
    budget_reserved: bool,
    directory_synced: bool,
) -> bool {
    deadline_ok && entry_gone && root_valid && budget_reserved && directory_synced
}

/// Read one bounded marker image while its descriptor remains owned; the
/// image is needed to restore evidence after an unlink/directory-sync fault.
fn read_marker_image(file: &mut File, deadline: Instant) -> Result<Vec<u8>, ()> {
    let mut contents = Vec::new();
    Read::by_ref(file)
        .take(4097)
        .read_to_end(&mut contents)
        .map_err(|_| ())?;
    if contents.len() > 4096 || Instant::now() >= deadline {
        return Err(());
    }
    Ok(contents)
}

/// Derive recognized backup and staging names from a validated recovery name.
/// The staging suffix is itself a valid pending marker so an interrupted
/// publication remains discoverable by the next bounded cleanup pass.
fn recovery_backup_names(name: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let (original, backup_prefix) =
        if let Some(original) = name.strip_prefix(b".ja-sandbox-recovery.") {
            (original, b".ja-sandbox-cleaned.".as_slice())
        } else if let Some(original) = name.strip_prefix(b".ja-sandbox-cleaned.") {
            (original, b".ja-sandbox-recovery.".as_slice())
        } else {
            return Err(());
        };
    let original_text = std::str::from_utf8(original).map_err(|_| ())?;
    if marker::parse_marker_name_for_cleanup(original_text).is_none() {
        return Err(());
    }
    let mut backup = backup_prefix.to_vec();
    backup.extend_from_slice(original);
    let mut pending = b".".to_vec();
    pending.extend_from_slice(original);
    pending.extend_from_slice(b".pending");
    if backup.len() > 255 || pending.len() > 255 {
        return Err(());
    }
    Ok((backup, pending))
}

/// Publish a complete backup image before the final recovery unlink.  Existing
/// matching evidence may be resumed, but a conflicting inode/content fails
/// closed instead of being overwritten.
fn prepare_recovery_backup(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    backup: &[u8],
    pending: &[u8],
    contents: &[u8],
    deadline: Instant,
) -> Result<marker::MarkerFileIdentity, ()> {
    let root_fd = root_directory.as_raw_fd();
    if Instant::now() >= deadline || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    verify_root_path_until(root, root_identity, deadline)?;
    if fd::fstatat_no_follow(root_fd, backup).is_ok() {
        if fd::fstatat_no_follow(root_fd, pending).is_ok() {
            // A second staging entry means the previous publication did not
            // reach a single settled state; keep both images for the next
            // bounded pass instead of claiming this transaction complete.
            return Err(());
        }
        if fd::last_errno() != fd::ENOENT {
            return Err(());
        }
        let mut file = fd::open_at_file(root_fd, backup).map_err(|_| ())?;
        let identity = validate_recovery_image(&file)?;
        let existing = read_marker_image(&mut file, deadline)?;
        if existing != contents || identity.uid != process::current_uid() {
            return Err(());
        }
        return Ok(identity);
    }
    if fd::last_errno() != fd::ENOENT {
        return Err(());
    }

    let mut pending_file = match fd::open_at_file(root_fd, pending) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            fd::create_at_file(root_fd, pending, MARKER_MODE).map_err(|_| ())?
        }
        Err(_) => return Err(()),
    };
    let pending_identity = validate_recovery_image(&pending_file)?;
    let existing = read_marker_image(&mut pending_file, deadline)?;
    if !existing.is_empty() && existing != contents {
        return Err(());
    }
    if existing.is_empty() {
        pending_file.write_all(contents).map_err(|_| ())?;
    }
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    pending_file.sync_all().map_err(|_| ())?;
    drop(pending_file);
    verify_root_path_until(root, root_identity, deadline)?;
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    fd::rename_at(root_fd, pending, backup).map_err(|_| ())?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let mut backup_file = fd::open_at_file(root_fd, backup).map_err(|_| ())?;
    let actual = validate_recovery_image(&backup_file)?;
    let image = read_marker_image(&mut backup_file, deadline)?;
    if actual != pending_identity || image != contents {
        return Err(());
    }
    Ok(actual)
}

/// Verify the owner-only regular-file invariants shared by recovery aliases.
fn validate_recovery_image(file: &File) -> Result<marker::MarkerFileIdentity, ()> {
    let metadata = file.metadata().map_err(|_| ())?;
    let identity = marker::MarkerFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    if !metadata.file_type().is_file()
        || identity.mode != MARKER_MODE
        || identity.nlink != 1
        || identity.uid != process::current_uid()
    {
        return Err(());
    }
    Ok(identity)
}

/// Remove a retained backup only after the target image has reached its
/// durable final-unlink state; every identity and directory-sync edge is
/// checked so a failed cleanup leaves the backup for the next pass.
fn remove_recovery_backup(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    backup: &[u8],
    expected: marker::MarkerFileIdentity,
    contents: &[u8],
    deadline: Instant,
) -> Result<(), ()> {
    let root_fd = root_directory.as_raw_fd();
    let mut file = open_verified_marker_at(root_fd, backup, expected, deadline)?;
    let image = read_marker_image(&mut file, deadline)?;
    if image != contents {
        return Err(());
    }
    verify_root_path_until(root, root_identity, deadline)?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    fd::unlink_at(root_fd, backup).map_err(|_| ())?;
    if Instant::now() >= deadline
        || fd::fstatat_no_follow(root_fd, backup).is_ok()
        || fd::last_errno() != fd::ENOENT
        || verify_root_path_until(root, root_identity, deadline).is_err()
        || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
    {
        return Err(());
    }
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    Ok(())
}

/// Restore the original or a recognized cleaned alias after a post-unlink
/// failure.  O_EXCL ensures a replacement is never overwritten silently.
fn restore_recovery_evidence(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    original: &[u8],
    alternate: &[u8],
    contents: &[u8],
    deadline: Instant,
) -> Result<RestoreOutcome, ()> {
    let mut faults = NoRestoreFault;
    restore_recovery_evidence_with_fault(
        RecoveryRestoreRequest {
            root,
            root_directory,
            root_identity,
            original,
            alternate,
            contents,
            deadline,
        },
        &mut faults,
    )
}

/// Keep the restore transaction's namespace, identity and byte image together
/// so fault-injection calls cannot accidentally mix evidence from two roots.
struct RecoveryRestoreRequest<'a> {
    root: &'a Path,
    root_directory: &'a File,
    root_identity: RootDirectoryIdentity,
    original: &'a [u8],
    alternate: &'a [u8],
    contents: &'a [u8],
    deadline: Instant,
}

/// Keep production restoration on the same code path as deterministic tests;
/// the seam can fail before each write/file-sync/directory-sync edge without
/// weakening the real implementation's ownership or deadline checks.
trait RestoreFaultInjector {
    fn before_write(&mut self, candidate: usize) -> Result<(), ()>;
    fn before_file_sync(&mut self, candidate: usize) -> Result<(), ()>;
    fn before_directory_sync(&mut self, candidate: usize) -> Result<(), ()>;
    /// Inject a failure immediately before a failed candidate is unlinked;
    /// production uses the no-op implementation so this remains the same
    /// transaction used by native fault tests.
    fn before_candidate_unlink(&mut self, candidate: usize) -> Result<(), ()>;
    /// Inject a post-unlink verification failure without removing the valid
    /// sibling, proving the next scan can safely close the remaining pair.
    fn after_candidate_unlink(&mut self, candidate: usize) -> Result<(), ()>;
}

struct NoRestoreFault;

impl RestoreFaultInjector for NoRestoreFault {
    fn before_write(&mut self, _candidate: usize) -> Result<(), ()> {
        Ok(())
    }

    fn before_file_sync(&mut self, _candidate: usize) -> Result<(), ()> {
        Ok(())
    }

    fn before_directory_sync(&mut self, _candidate: usize) -> Result<(), ()> {
        Ok(())
    }

    fn before_candidate_unlink(&mut self, _candidate: usize) -> Result<(), ()> {
        Ok(())
    }

    fn after_candidate_unlink(&mut self, _candidate: usize) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RestoreOutcome {
    /// At least one complete candidate has been synced and remains available.
    durable_survivor: bool,
    /// A failed sibling could not be removed in this pass and must be retried
    /// only after the valid survivor is revalidated by the next scan.
    sibling_cleanup_failed: bool,
}

/// Execute recovery with an injectable fault plan while preserving the exact
/// production candidate lifecycle and last-copy invariant.
fn restore_recovery_evidence_with_fault<F: RestoreFaultInjector>(
    request: RecoveryRestoreRequest<'_>,
    faults: &mut F,
) -> Result<RestoreOutcome, ()> {
    let RecoveryRestoreRequest {
        root,
        root_directory,
        root_identity,
        original,
        alternate,
        contents,
        deadline,
    } = request;
    let root_fd = root_directory.as_raw_fd();
    let mut failed_candidates = Vec::new();
    for (index, candidate) in [original, alternate].into_iter().enumerate() {
        if Instant::now() >= deadline || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
            return Err(());
        }
        verify_root_path_until(root, root_identity, deadline)?;
        let mut file = match fd::create_at_file(root_fd, candidate, MARKER_MODE) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                // A retained backup is already a complete recovery witness;
                // never overwrite it with a faulted empty O_EXCL candidate.
                let mut existing = match fd::open_at_file(root_fd, candidate) {
                    Ok(file) => file,
                    Err(_) => continue,
                };
                let existing_identity = match validate_recovery_image(&existing) {
                    Ok(identity) => identity,
                    Err(()) => continue,
                };
                let existing_image = match read_marker_image(&mut existing, deadline) {
                    Ok(image) => image,
                    Err(()) => {
                        failed_candidates.push((index, candidate.to_vec(), existing_identity));
                        continue;
                    }
                };
                if existing_image != contents || existing.sync_all().is_err() {
                    failed_candidates.push((index, candidate.to_vec(), existing_identity));
                    continue;
                }
                drop(existing);
                return finalize_durable_restore_candidate(
                    RestoreFinalizeRequest {
                        root,
                        root_directory,
                        root_identity,
                        candidate_index: index,
                        failed_candidates,
                        deadline,
                    },
                    faults,
                );
            }
            Err(_) => continue,
        };
        let candidate_identity = match validate_recovery_image(&file) {
            Ok(identity) => identity,
            Err(()) => {
                drop(file);
                // The inode is not trustworthy enough to unlink; preserve it
                // and let the next bounded scan report the malformed evidence.
                continue;
            }
        };
        if faults.before_write(index).is_err()
            || file.write_all(contents).is_err()
            || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
            || faults.before_file_sync(index).is_err()
            || file.sync_all().is_err()
        {
            drop(file);
            // This candidate may be partial, but it is the only durable
            // namespace evidence until another candidate completes.  Never
            // delete it here; a later successful candidate may make removal
            // safe, otherwise abort preserves this best available evidence.
            failed_candidates.push((index, candidate.to_vec(), candidate_identity));
            continue;
        }
        drop(file);
        if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
            || faults.before_directory_sync(index).is_err()
            || fd::sync_directory(root_directory).is_err()
            || Instant::now() >= deadline
            || verify_root_path_until(root, root_identity, deadline).is_err()
        {
            failed_candidates.push((index, candidate.to_vec(), candidate_identity));
            continue;
        }
        return finalize_durable_restore_candidate(
            RestoreFinalizeRequest {
                root,
                root_directory,
                root_identity,
                candidate_index: index,
                failed_candidates,
                deadline,
            },
            faults,
        );
    }
    Err(())
}

/// Finish a restore only after the selected candidate and its parent
/// directory are durable; failed siblings are then retried with identity
/// checks while the complete candidate remains available.
struct RestoreFinalizeRequest<'a> {
    root: &'a Path,
    root_directory: &'a File,
    root_identity: RootDirectoryIdentity,
    candidate_index: usize,
    failed_candidates: Vec<(usize, Vec<u8>, marker::MarkerFileIdentity)>,
    deadline: Instant,
}

/// Finish a restore transaction through one bounded finalization path.
fn finalize_durable_restore_candidate<F: RestoreFaultInjector>(
    request: RestoreFinalizeRequest<'_>,
    faults: &mut F,
) -> Result<RestoreOutcome, ()> {
    let RestoreFinalizeRequest {
        root,
        root_directory,
        root_identity,
        candidate_index,
        failed_candidates,
        deadline,
    } = request;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
        || faults.before_directory_sync(candidate_index).is_err()
        || fd::sync_directory(root_directory).is_err()
        || Instant::now() >= deadline
        || verify_root_path_until(root, root_identity, deadline).is_err()
    {
        return Err(());
    }
    let mut sibling_cleanup_failed = false;
    for (failed_index, failed_name, failed_identity) in failed_candidates {
        if remove_restore_candidate(
            RestoreCandidateRequest {
                root,
                root_directory,
                root_identity,
                candidate: &failed_name,
                expected: failed_identity,
                deadline,
                candidate_index: failed_index,
            },
            faults,
        )
        .is_err()
        {
            // The valid sibling remains durable, so returning an outcome is
            // safe; this bit forces the next scan to retry the unresolved
            // basename instead of silently declaring the pair settled.
            sibling_cleanup_failed = true;
        }
    }
    Ok(RestoreOutcome {
        durable_survivor: true,
        sibling_cleanup_failed,
    })
}

/// Remove a failed restore candidate only after rechecking its inode; this
/// prevents a short-write fault from leaving an invalid basename that blocks
/// the next cleanup scan forever.  The injector is a no-op in production and
/// lets native restart tests exercise the real unlink/postcheck transaction.
struct RestoreCandidateRequest<'a> {
    root: &'a Path,
    root_directory: &'a File,
    root_identity: RootDirectoryIdentity,
    candidate: &'a [u8],
    expected: marker::MarkerFileIdentity,
    deadline: Instant,
    candidate_index: usize,
}

/// Remove one candidate through a bounded, descriptor-relative transaction.
fn remove_restore_candidate<F: RestoreFaultInjector>(
    request: RestoreCandidateRequest<'_>,
    faults: &mut F,
) -> Result<(), ()> {
    let RestoreCandidateRequest {
        root,
        root_directory,
        root_identity,
        candidate,
        expected,
        deadline,
        candidate_index,
    } = request;
    let file = open_verified_marker_at(root_directory.as_raw_fd(), candidate, expected, deadline)?;
    drop(file);
    verify_root_path_until(root, root_identity, deadline)?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    faults.before_candidate_unlink(candidate_index)?;
    fd::unlink_at(root_directory.as_raw_fd(), candidate).map_err(|_| ())?;
    faults.after_candidate_unlink(candidate_index)?;
    if fd::fstatat_no_follow(root_directory.as_raw_fd(), candidate).is_ok()
        || fd::last_errno() != fd::ENOENT
        || verify_root_path_until(root, root_identity, deadline).is_err()
        || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
    {
        return Err(());
    }
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    Ok(())
}

/// Extract one safe basename for all descriptor-relative operations; no
/// caller-controlled separator or parent component can reach an `*at` call.
fn marker_name(path: &Path) -> Result<&[u8], ()> {
    let name = path.file_name().ok_or(())?;
    if name.is_empty() || name == "." || name == ".." {
        return Err(());
    }
    Ok(name.as_bytes())
}

/// Delete one verified sibling through the held root descriptor and sync the
/// directory before reporting success.  A same-UID attacker racing between
/// the final lstat and unlink remains outside the stated threat model; the
/// private root plus openat-style unlink closes pathname root replacement.
fn unlink_verified_entry(
    root: &Path,
    root_directory: &File,
    root_identity: RootDirectoryIdentity,
    path: &Path,
    expected: marker::MarkerFileIdentity,
    deadline: Instant,
) -> Result<(), ()> {
    if Instant::now() >= deadline {
        return Err(());
    }
    if path.parent() != Some(root) {
        return Err(());
    }
    let name = marker_name(path)?;
    let mut file = open_verified_marker_at(root_directory.as_raw_fd(), name, expected, deadline)?;
    verify_root_path_until(root, root_identity, deadline)?;
    // Do not begin a destructive unlink when the shared budget is already at
    // its edge; preserving the marker is safer than deleting the only
    // recoverable evidence and then timing out before the directory sync.
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    let mut tombstone = b".ja-sandbox-cleaned.".to_vec();
    tombstone.extend_from_slice(name);
    if tombstone.len() > 255 {
        return Err(());
    }
    if fd::rename_at(root_directory.as_raw_fd(), name, &tombstone).is_err() {
        return Err(());
    }
    if Instant::now() >= deadline {
        return Err(());
    }
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
        || fd::sync_directory(root_directory).is_err()
        || Instant::now() >= deadline
    {
        return Err(());
    }
    // Keep a second owner-only original image before unlink.  If the
    // post-unlink directory sync fails, this recovery file is what the next
    // bounded cleanup pass can discover instead of relying on an unlinked fd.
    let mut recovery = b".ja-sandbox-recovery.".to_vec();
    recovery.extend_from_slice(name);
    if recovery.len() > 255 || Instant::now() >= deadline {
        return Err(());
    }
    let mut recovery_file =
        fd::create_at_file(root_directory.as_raw_fd(), &recovery, MARKER_MODE).map_err(|_| ())?;
    let recovery_metadata = recovery_file.metadata().map_err(|_| ())?;
    let recovery_identity = marker::MarkerFileIdentity {
        dev: recovery_metadata.dev(),
        ino: recovery_metadata.ino(),
        nlink: recovery_metadata.nlink(),
        mode: recovery_metadata.permissions().mode() & 0o777,
        uid: recovery_metadata.uid(),
    };
    if !recovery_metadata.file_type().is_file()
        || recovery_identity.mode != MARKER_MODE
        || recovery_identity.nlink != 1
        || recovery_identity.uid != process::current_uid()
    {
        return Err(());
    }
    let mut contents = Vec::new();
    Read::by_ref(&mut file)
        .take(4097)
        .read_to_end(&mut contents)
        .map_err(|_| ())?;
    if contents.len() > 4096 || !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    recovery_file.write_all(&contents).map_err(|_| ())?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    recovery_file.sync_all().map_err(|_| ())?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE)
        || fd::sync_directory(root_directory).is_err()
        || Instant::now() >= deadline
    {
        return Err(());
    }
    let _tombstone_file =
        open_verified_marker_at(root_directory.as_raw_fd(), &tombstone, expected, deadline)?;
    verify_root_path_until(root, root_identity, deadline)?;
    if Instant::now() >= deadline {
        return Err(());
    }
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    if fd::unlink_at(root_directory.as_raw_fd(), &tombstone).is_err() {
        return Err(());
    }
    if Instant::now() >= deadline
        || fd::fstatat_no_follow(root_directory.as_raw_fd(), &tombstone)
            .err()
            .is_none()
        || fd::last_errno() != fd::ENOENT
    {
        return Err(());
    }
    verify_root_path_until(root, root_identity, deadline)?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err(());
    }
    fd::sync_directory(root_directory).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    drop(recovery_file);
    let recovery_name = std::str::from_utf8(&recovery).map_err(|_| ())?;
    let recovery_path = root.join(recovery_name);
    // The recovery image is part of this transaction, not a deferred item:
    // finalizing it here keeps a successful active-marker cleanup from
    // leaving an entry that the already-completed scan cannot revisit.
    unlink_cleaned_evidence(
        root,
        root_directory,
        root_identity,
        &recovery_path,
        recovery_identity,
        deadline,
    )?;
    drop(file);
    Ok(())
}

/// Reserve time for a destructive filesystem syscall and its postcondition;
/// a late deadline leaves the evidence in place for a bounded recovery pass.
fn has_cleanup_budget(deadline: Instant, reserve: Duration) -> bool {
    Instant::now()
        .checked_add(reserve)
        .is_some_and(|reserved| reserved < deadline)
}

/// Write a bounded, fixed-category report with owner-only permissions so the
/// outer workflow can decide success without parsing locale-dependent errors.
fn write_report(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    marker_count: usize,
) -> Result<(), &'static str> {
    write_report_with_count_label(report, categories, marker_count, "marker-count")
}

/// Write a cleanup report only while the caller's single deadline still has
/// budget; if it expires, marker evidence remains on disk for the next
/// bounded recovery pass instead of being deleted without a durable summary.
fn write_report_until(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    marker_count: usize,
    deadline: Instant,
) -> Result<(), &'static str> {
    write_report_with_count_label_until(report, categories, marker_count, "marker-count", deadline)
}

/// Write process-table evidence with a truthful scope count while sharing the
/// same owner-only report file policy as marker cleanup.
pub(super) fn write_scope_report(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    scope_count: usize,
) -> Result<(), &'static str> {
    write_report_with_count_label(report, categories, scope_count, "scope-count")
}

/// Keep report creation and fsync behavior identical for marker and scope
/// evidence; only the fixed count field name differs by producer.
fn write_report_with_count_label(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    count: usize,
    count_label: &str,
) -> Result<(), &'static str> {
    write_report_with_count_label_until(
        report,
        categories,
        count,
        count_label,
        Instant::now() + CLEANUP_DEADLINE,
    )
}

/// Keep report creation, byte writes and both file/directory syncs inside a
/// caller-owned monotonic budget.  The checks are intentionally placed around
/// every potentially blocking filesystem operation so the final evidence path
/// cannot consume time reserved for process-tree recovery.
fn write_report_with_count_label_until(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    count: usize,
    count_label: &str,
    deadline: Instant,
) -> Result<(), &'static str> {
    if Instant::now() >= deadline {
        return Err("report-deadline");
    }
    let Some(parent) = report.parent() else {
        return Err("report-path-invalid");
    };
    fs::create_dir_all(parent).map_err(|_| "report-open-failed")?;
    if Instant::now() >= deadline {
        return Err("report-deadline");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(report)
        .map_err(|_| "report-open-failed")?;
    for category in categories {
        if Instant::now() >= deadline {
            return Err("report-deadline");
        }
        writeln!(file, "{category}=true").map_err(|_| "report-write-failed")?;
    }
    if Instant::now() >= deadline {
        return Err("report-deadline");
    }
    writeln!(file, "{count_label}={count}").map_err(|_| "report-write-failed")?;
    if Instant::now() >= deadline {
        return Err("report-deadline");
    }
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err("report-deadline");
    }
    file.sync_all().map_err(|_| "report-write-failed")?;
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err("report-deadline");
    }
    if !has_cleanup_budget(deadline, CLEANUP_SYSCALL_RESERVE) {
        return Err("report-deadline");
    }
    fd::sync_directory(&File::open(parent).map_err(|_| "report-write-failed")?)
        .map_err(|_| "report-write-failed")?;
    if Instant::now() >= deadline {
        return Err("report-deadline");
    }
    Ok(())
}

/// Preserve a fixed failure category even when the runner report itself is
/// unavailable; callers still receive a nonzero result and no signal occurs.
fn write_failure_report(report: &Path, category: &'static str) -> Result<(), &'static str> {
    let mut categories = BTreeSet::new();
    categories.insert(category);
    write_report(report, &categories, 0).map_err(|_| category)?;
    Err(category)
}

/// Entrypoint used by the tiny binary wrapper so normal cleanup and fixture
/// execution share exactly the same Rust implementation.
pub fn run_cli(arguments: &[String]) -> Result<(), &'static str> {
    let fixture = arguments.iter().any(|argument| argument == "--fixture");
    if fixture {
        #[cfg(target_os = "macos")]
        {
            return run_fixture();
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err("macOS-only fixture");
        }
    }
    if arguments
        .iter()
        .any(|argument| argument == "--residual-scan")
    {
        let root = argument_path(arguments, "--root")?;
        let report = argument_path(arguments, "--report")?;
        return process_scan::run(&root, &report);
    }
    let root = argument_path(arguments, "--root")?;
    let report = argument_path(arguments, "--report")?;
    cleanup_markers(&root, &report, false)
}

/// Parse a path argument without echoing user-controlled values into errors.
fn argument_path(arguments: &[String], flag: &str) -> Result<PathBuf, &'static str> {
    let index = arguments
        .iter()
        .position(|argument| argument == flag)
        .ok_or("argument-missing")?;
    arguments
        .get(index + 1)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or("argument-missing")
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink};
    #[cfg(target_os = "macos")]
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Keep the identity comparison strict so a forged fixture cannot reach a
    /// signal call merely by matching a PID or group number.
    #[test]
    fn identity_requires_pgid_start_and_known_comm() {
        let record = MarkerRecord {
            path: PathBuf::new(),
            suffix: "marker".into(),
            file_identity: marker::MarkerFileIdentity {
                dev: 0,
                ino: 0,
                nlink: 1,
                mode: MARKER_MODE,
                uid: process::current_uid(),
            },
            owner_pid: 7,
            nonce: 8,
            pid: 9,
            pgid: 10,
            start_identity: "start".into(),
            executable_kind: "log".into(),
        };
        let actual = ProcessIdentity {
            pid: 9,
            pgid: 10,
            uid: process::current_uid(),
            comm: "log".into(),
            start_identity: "start".into(),
        };
        assert!(identity_matches(&record, &actual, false));
        assert!(!identity_matches(
            &record,
            &ProcessIdentity {
                start_identity: "other".into(),
                ..actual.clone()
            },
            false
        ));
        assert!(!identity_matches(
            &record,
            &ProcessIdentity {
                uid: process::current_uid().saturating_add(1),
                ..actual
            },
            false
        ));
    }

    /// Prove marker deduplication retains distinct suffix, group and inode
    /// obligations instead of collapsing them to owner PID and nonce alone.
    #[test]
    fn marker_cleanup_key_keeps_distinct_identities() {
        let base = MarkerRecord {
            path: PathBuf::new(),
            suffix: "marker".into(),
            file_identity: marker::MarkerFileIdentity {
                dev: 1,
                ino: 2,
                nlink: 1,
                mode: MARKER_MODE,
                uid: process::current_uid(),
            },
            owner_pid: 7,
            nonce: 8,
            pid: 9,
            pgid: 10,
            start_identity: "start".into(),
            executable_kind: "log".into(),
        };
        assert_ne!(
            marker_cleanup_key(&base),
            marker_cleanup_key(&MarkerRecord {
                suffix: "fallback".into(),
                ..base.clone()
            })
        );
        assert_ne!(
            marker_cleanup_key(&base),
            marker_cleanup_key(&MarkerRecord {
                pgid: 11,
                ..base.clone()
            })
        );
        assert_ne!(
            marker_cleanup_key(&base),
            marker_cleanup_key(&MarkerRecord {
                file_identity: marker::MarkerFileIdentity {
                    ino: 3,
                    ..base.file_identity
                },
                ..base
            })
        );
    }

    /// Keep permission failures distinct from ESRCH so workflow cleanup never
    /// turns a denied probe into a false residual-free result.
    #[test]
    fn errno_classification_is_exact() {
        assert_eq!(process::classify_errno(3), process::ProcessState::Empty);
        assert_eq!(
            process::classify_errno(process::EPERM),
            process::ProcessState::PermissionDenied
        );
        assert_eq!(
            process::classify_errno(13),
            process::ProcessState::Other(13)
        );
    }

    /// Keep cleaned evidence fail-closed for every partial liveness state;
    /// only the exact double-ESRCH state is eligible for bounded garbage
    /// collection.
    #[test]
    fn cleaned_evidence_requires_pid_and_group_gone() {
        assert_eq!(
            classify_cleaned_identity((ProcessState::Empty, ProcessState::Empty)),
            Ok(CleanedIdentityDisposition::Gone)
        );
        assert_eq!(
            classify_cleaned_identity((ProcessState::Present, ProcessState::Present)),
            Ok(CleanedIdentityDisposition::Active)
        );
        for states in [
            (ProcessState::Empty, ProcessState::Present),
            (ProcessState::Present, ProcessState::Empty),
            (ProcessState::PermissionDenied, ProcessState::Empty),
            (ProcessState::Empty, ProcessState::Other(5)),
        ] {
            assert!(classify_cleaned_identity(states).is_err());
        }
    }

    /// Every post-unlink edge is required before the state machine can report
    /// durable deletion; one injected false edge must enter restoration.
    #[test]
    fn recovery_postconditions_have_no_success_bypass() {
        let all_true = [true, true, true, true, true];
        assert!(recovery_postconditions_are_durable(
            all_true[0],
            all_true[1],
            all_true[2],
            all_true[3],
            all_true[4]
        ));
        for index in 0..all_true.len() {
            let mut edges = all_true;
            edges[index] = false;
            assert!(!recovery_postconditions_are_durable(
                edges[0], edges[1], edges[2], edges[3], edges[4]
            ));
        }
    }

    /// A cleaned entry must use the recovery prefix as its backup; reusing
    /// the target name would delete the only image before the final unlink.
    #[test]
    fn recovery_backup_names_are_distinct_for_both_states() {
        let recovery = b".ja-sandbox-recovery.ja-sandbox-log-helper-42-12.marker";
        let cleaned = b".ja-sandbox-cleaned.ja-sandbox-log-helper-42-12.marker";
        let (recovery_backup, _) = recovery_backup_names(recovery).expect("recovery names");
        let (cleaned_backup, _) = recovery_backup_names(cleaned).expect("cleaned names");
        assert_eq!(recovery_backup, cleaned);
        assert_eq!(cleaned_backup, recovery);
    }

    /// A successful active-marker transaction consumes its newly-created
    /// recovery image before returning, because the scan snapshot cannot see
    /// entries created after enumeration.
    #[cfg(target_os = "macos")]
    #[test]
    fn active_marker_transaction_leaves_no_unresolved_evidence() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-transaction-{nonce}"));
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        let marker_path = root.join("ja-sandbox-log-helper-42-12.marker");
        let mut marker_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(MARKER_MODE)
            .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
            .open(&marker_path)
            .expect("marker");
        marker_file.write_all(b"marker-image\n").expect("image");
        marker_file.sync_all().expect("marker sync");
        let metadata = marker_file.metadata().expect("marker metadata");
        let expected = marker::MarkerFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            mode: metadata.permissions().mode() & 0o777,
            uid: metadata.uid(),
        };
        drop(marker_file);
        let (root_directory, root_identity) = open_verified_root_directory(&root).expect("open");
        unlink_verified_entry(
            &root,
            &root_directory,
            root_identity,
            &marker_path,
            expected,
            Instant::now() + CLEANUP_DEADLINE,
        )
        .expect("same-round cleanup");
        let leftovers = fs::read_dir(&root)
            .expect("read root")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".ja-sandbox-cleaned.")
                    || name.starts_with(".ja-sandbox-recovery.")
                    || name.ends_with(".pending")
            })
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "unresolved evidence: {leftovers:?}");
        fs::remove_dir(&root).expect("root cleanup");
    }

    /// Exercise the restore path and then run the production finalizer again;
    /// a post-unlink fault therefore leaves evidence that the next pass can
    /// safely consume instead of silently losing the only image.
    #[cfg(target_os = "macos")]
    #[test]
    fn restored_recovery_evidence_is_closed_by_next_pass() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-recovery-{nonce}"));
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        let recovery_path = root.join(".ja-sandbox-recovery.ja-sandbox-log-helper-42-12.marker");
        let mut recovery_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(MARKER_MODE)
            .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
            .open(&recovery_path)
            .expect("recovery");
        recovery_file.write_all(b"recovery-image\n").expect("image");
        recovery_file.sync_all().expect("recovery sync");
        let metadata = recovery_file.metadata().expect("recovery metadata");
        let expected = marker::MarkerFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            mode: metadata.permissions().mode() & 0o777,
            uid: metadata.uid(),
        };
        drop(recovery_file);
        let (root_directory, root_identity) = open_verified_root_directory(&root).expect("open");
        let root_fd = root_directory.as_raw_fd();
        let name = marker_name(&recovery_path).expect("recovery name");
        let mut image_file =
            open_verified_marker_at(root_fd, name, expected, Instant::now() + CLEANUP_DEADLINE)
                .expect("open image");
        let image = read_marker_image(&mut image_file, Instant::now() + CLEANUP_DEADLINE)
            .expect("read image");
        drop(image_file);
        fd::unlink_at(root_fd, name).expect("simulate post-unlink");
        restore_recovery_evidence(
            &root,
            &root_directory,
            root_identity,
            name,
            b".ja-sandbox-cleaned.ja-sandbox-log-helper-42-12.marker",
            &image,
            Instant::now() + CLEANUP_DEADLINE,
        )
        .expect("restore evidence");
        let restored_metadata = fs::metadata(&recovery_path).expect("restored metadata");
        let restored = marker::MarkerFileIdentity {
            dev: restored_metadata.dev(),
            ino: restored_metadata.ino(),
            nlink: restored_metadata.nlink(),
            mode: restored_metadata.permissions().mode() & 0o777,
            uid: restored_metadata.uid(),
        };
        unlink_cleaned_evidence(
            &root,
            &root_directory,
            root_identity,
            &recovery_path,
            restored,
            Instant::now() + CLEANUP_DEADLINE,
        )
        .expect("next-pass cleanup");
        assert!(!recovery_path.exists());
        assert!(fs::read_dir(&root).expect("read root").next().is_none());
        fs::remove_dir(&root).expect("root cleanup");
    }

    #[cfg(target_os = "macos")]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PendingFaultPoint {
        AliasWrite,
        FileSync,
        DirectorySync(usize),
        Unlink(usize),
        Postcheck(usize),
        RootRevalidate(usize),
    }

    #[cfg(target_os = "macos")]
    struct PendingFaultPlan {
        point: PendingFaultPoint,
    }

    #[cfg(target_os = "macos")]
    impl PendingFaultPlan {
        fn fails(&self, point: PendingFaultPoint) -> bool {
            self.point == point
        }
    }

    #[cfg(target_os = "macos")]
    impl PendingFaultInjector for PendingFaultPlan {
        fn before_alias_write(&mut self) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::AliasWrite))
                .then_some(())
                .ok_or(())
        }

        fn before_file_sync(&mut self) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::FileSync))
                .then_some(())
                .ok_or(())
        }

        fn before_directory_sync(&mut self, ordinal: usize) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::DirectorySync(ordinal)))
                .then_some(())
                .ok_or(())
        }

        fn before_unlink(&mut self, ordinal: usize) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::Unlink(ordinal)))
                .then_some(())
                .ok_or(())
        }

        fn after_unlink(&mut self, ordinal: usize) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::Postcheck(ordinal)))
                .then_some(())
                .ok_or(())
        }

        fn before_root_revalidate(&mut self, ordinal: usize) -> Result<(), ()> {
            (!self.fails(PendingFaultPoint::RootRevalidate(ordinal)))
                .then_some(())
                .ok_or(())
        }
    }

    /// Exercise every pending FSM fault edge, then use the production scan
    /// and cleanup entry as a restart boundary to reach zero unresolved state.
    #[cfg(target_os = "macos")]
    #[test]
    fn pending_fsm_faults_leave_restartable_state() {
        for alias_only in [false, true] {
            for fault in [
                None,
                Some(PendingFaultPoint::AliasWrite),
                Some(PendingFaultPoint::FileSync),
                Some(PendingFaultPoint::DirectorySync(0)),
                Some(PendingFaultPoint::Unlink(0)),
                Some(PendingFaultPoint::Postcheck(0)),
                Some(PendingFaultPoint::RootRevalidate(0)),
                // These faults target the second unlink, after the first copy
                // has already been durably removed; the production restore
                // path must leave a recognized alias for the next scan.
                Some(PendingFaultPoint::Unlink(1)),
                Some(PendingFaultPoint::Postcheck(1)),
                Some(PendingFaultPoint::RootRevalidate(1)),
                Some(PendingFaultPoint::DirectorySync(2)),
            ] {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos();
                let root = std::env::temp_dir().join(format!("ja-pending-fsm-{nonce}"));
                fs::DirBuilder::new()
                    .recursive(false)
                    .mode(0o700)
                    .create(&root)
                    .expect("root");
                let base_name = b".ja-sandbox-log-helper-42-999991.marker.pending";
                let names = pending_state_names(base_name).expect("pending states");
                let name = if alias_only {
                    names.recovery.as_slice()
                } else {
                    base_name.as_slice()
                };
                let path = root.join(std::str::from_utf8(name).expect("pending name"));
                let (root_directory, root_identity) =
                    open_verified_root_directory(&root).expect("open root");
                let mut file = fd::create_at_file(root_directory.as_raw_fd(), name, MARKER_MODE)
                    .expect("pending create");
                file.write_all(b"partial pending image\n")
                    .expect("pending image");
                file.sync_all().expect("pending sync");
                let identity = validate_recovery_image(&file).expect("pending identity");
                drop(file);
                let pending = PendingMarker {
                    path,
                    file_identity: identity,
                    cleaned: false,
                    cleaned_record: None,
                    damaged: false,
                    pending_recovery: alias_only,
                };
                if let Some(point) = fault {
                    let mut plan = PendingFaultPlan { point };
                    assert!(
                        remove_pending_evidence_with_fault(
                            &root,
                            &root_directory,
                            root_identity,
                            &pending,
                            Instant::now() + CLEANUP_DEADLINE,
                            &mut plan,
                        )
                        .is_err()
                    );
                } else {
                    let mut plan = NoPendingFault;
                    assert!(
                        remove_pending_evidence_with_fault(
                            &root,
                            &root_directory,
                            root_identity,
                            &pending,
                            Instant::now() + CLEANUP_DEADLINE,
                            &mut plan,
                        )
                        .is_ok()
                    );
                }
                if fault.is_some() {
                    // A failed destructive transition must leave at least one
                    // grammar-recognized entry before the restart pass; this
                    // catches the historical second-unlink evidence loss.
                    let recoverable = fs::read_dir(&root)
                        .expect("read pending state")
                        .map(|entry| entry.expect("entry").file_name())
                        .filter(|entry| {
                            let name = entry.to_string_lossy();
                            name.starts_with(".ja-sandbox-log-helper-")
                                || name.starts_with(".ja-sandbox-recovery.")
                                || name.starts_with(".ja-sandbox-cleaned.")
                        })
                        .collect::<Vec<_>>();
                    assert!(
                        !recoverable.is_empty(),
                        "fault dropped all pending evidence: {fault:?}"
                    );
                }
                let report = root.join("cleanup-report");
                cleanup_markers_until(&root, &report, false, Instant::now() + CLEANUP_DEADLINE)
                    .expect("pending restart cleanup");
                let unresolved = fs::read_dir(&root)
                    .expect("read root")
                    .map(|entry| entry.expect("entry").file_name())
                    .filter(|entry| {
                        let name = entry.to_string_lossy();
                        name.starts_with(".ja-sandbox-log-helper-")
                            || name.starts_with(".ja-sandbox-recovery.")
                            || name.starts_with(".ja-sandbox-cleaned.")
                    })
                    .collect::<Vec<_>>();
                assert!(unresolved.is_empty(), "pending unresolved: {unresolved:?}");
                drop(root_directory);
                fs::remove_file(report).expect("report cleanup");
                fs::remove_dir(&root).expect("root cleanup");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RestoreFaultPoint {
        Write(usize),
        FileSync(usize),
        DirectorySync(usize),
        CandidateUnlink(usize),
        CandidatePostcheck(usize),
    }

    #[cfg(target_os = "macos")]
    struct RestoreFaultPlan {
        points: Vec<RestoreFaultPoint>,
    }

    #[cfg(target_os = "macos")]
    impl RestoreFaultPlan {
        fn new(points: impl IntoIterator<Item = RestoreFaultPoint>) -> Self {
            Self {
                points: points.into_iter().collect(),
            }
        }

        fn fails(&self, point: RestoreFaultPoint) -> bool {
            self.points.contains(&point)
        }
    }

    #[cfg(target_os = "macos")]
    impl RestoreFaultInjector for RestoreFaultPlan {
        fn before_write(&mut self, candidate: usize) -> Result<(), ()> {
            (!self.fails(RestoreFaultPoint::Write(candidate)))
                .then_some(())
                .ok_or(())
        }

        fn before_file_sync(&mut self, candidate: usize) -> Result<(), ()> {
            (!self.fails(RestoreFaultPoint::FileSync(candidate)))
                .then_some(())
                .ok_or(())
        }

        fn before_directory_sync(&mut self, candidate: usize) -> Result<(), ()> {
            (!self.fails(RestoreFaultPoint::DirectorySync(candidate)))
                .then_some(())
                .ok_or(())
        }

        fn before_candidate_unlink(&mut self, candidate: usize) -> Result<(), ()> {
            (!self.fails(RestoreFaultPoint::CandidateUnlink(candidate)))
                .then_some(())
                .ok_or(())
        }

        fn after_candidate_unlink(&mut self, candidate: usize) -> Result<(), ()> {
            (!self.fails(RestoreFaultPoint::CandidatePostcheck(candidate)))
                .then_some(())
                .ok_or(())
        }
    }

    /// Run one production restore transaction with deterministic IO faults.
    /// The successful cases must leave one complete survivor that the normal
    /// finalizer can close; the double-fault cases must retain both candidates.
    #[cfg(target_os = "macos")]
    fn run_restore_fault_case(
        points: impl IntoIterator<Item = RestoreFaultPoint>,
        expect_success: bool,
    ) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-restore-fault-{nonce}"));
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        let original = b".ja-sandbox-recovery.ja-sandbox-log-helper-42-12.marker";
        let alternate = b".ja-sandbox-cleaned.ja-sandbox-log-helper-42-12.marker";
        let contents = b"complete-recovery-image\n";
        let (root_directory, root_identity) = open_verified_root_directory(&root).expect("open");
        let mut plan = RestoreFaultPlan::new(points);
        let result = restore_recovery_evidence_with_fault(
            RecoveryRestoreRequest {
                root: &root,
                root_directory: &root_directory,
                root_identity,
                original,
                alternate,
                contents,
                deadline: Instant::now() + CLEANUP_DEADLINE,
            },
            &mut plan,
        );
        let original_path = root.join(std::str::from_utf8(original).expect("original name"));
        let alternate_path = root.join(std::str::from_utf8(alternate).expect("alternate name"));
        if expect_success {
            assert!(result.is_ok(), "restore unexpectedly failed");
            let survivor = [original_path.clone(), alternate_path.clone()]
                .into_iter()
                .find(|path| path.exists())
                .expect("durable survivor");
            let metadata = fs::metadata(&survivor).expect("survivor metadata");
            let expected = marker::MarkerFileIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                mode: metadata.permissions().mode() & 0o777,
                uid: metadata.uid(),
            };
            unlink_cleaned_evidence(
                &root,
                &root_directory,
                root_identity,
                &survivor,
                expected,
                Instant::now() + CLEANUP_DEADLINE,
            )
            .expect("next-pass finalizer");
            assert!(fs::read_dir(&root).expect("read root").next().is_none());
        } else {
            assert!(result.is_err(), "double fault unexpectedly succeeded");
            assert!(original_path.exists(), "original candidate was deleted");
            assert!(alternate_path.exists(), "alternate candidate was deleted");
            for path in [original_path, alternate_path] {
                let metadata = fs::metadata(&path).expect("candidate metadata");
                assert!(metadata.file_type().is_file());
                assert_eq!(metadata.permissions().mode() & 0o777, MARKER_MODE);
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(metadata.uid(), process::current_uid());
            }
        }
        drop(root_directory);
        fs::remove_dir(&root).expect("root cleanup");
    }

    /// Cover first-candidate write, file-sync and directory-sync faults with
    /// a real production restore/finalizer path and deterministic injection.
    #[cfg(target_os = "macos")]
    #[test]
    fn restore_faults_keep_a_durable_survivor_and_close_next_pass() {
        run_restore_fault_case([RestoreFaultPoint::Write(0)], true);
        run_restore_fault_case([RestoreFaultPoint::FileSync(0)], true);
        run_restore_fault_case([RestoreFaultPoint::DirectorySync(0)], true);
    }

    /// Every two-candidate fault pair must retain both namespace entries; the
    /// double-write case is only safe in production because the caller now
    /// retains a separate durable backup, covered by the next test.
    #[cfg(target_os = "macos")]
    #[test]
    fn restore_double_faults_retain_both_candidates() {
        for points in [
            [RestoreFaultPoint::Write(0), RestoreFaultPoint::Write(1)],
            [RestoreFaultPoint::Write(0), RestoreFaultPoint::FileSync(1)],
            [
                RestoreFaultPoint::Write(0),
                RestoreFaultPoint::DirectorySync(1),
            ],
            [RestoreFaultPoint::FileSync(0), RestoreFaultPoint::Write(1)],
            [
                RestoreFaultPoint::FileSync(0),
                RestoreFaultPoint::FileSync(1),
            ],
            [
                RestoreFaultPoint::FileSync(0),
                RestoreFaultPoint::DirectorySync(1),
            ],
            [
                RestoreFaultPoint::DirectorySync(0),
                RestoreFaultPoint::Write(1),
            ],
            [
                RestoreFaultPoint::DirectorySync(0),
                RestoreFaultPoint::FileSync(1),
            ],
            [
                RestoreFaultPoint::DirectorySync(0),
                RestoreFaultPoint::DirectorySync(1),
            ],
        ] {
            run_restore_fault_case(points, false);
        }
    }

    /// When both restore writes fail, the retained pre-unlink backup is the
    /// durable survivor.  The real scan must consume it on restart rather
    /// than treating two empty O_EXCL candidates as recoverable evidence.
    #[cfg(target_os = "macos")]
    #[test]
    fn double_write_fault_uses_retained_backup_and_closes_on_restart() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-double-write-{nonce}"));
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        let original = b".ja-sandbox-recovery.ja-sandbox-log-helper-42-999991.marker";
        let alternate = b".ja-sandbox-cleaned.ja-sandbox-log-helper-42-999991.marker";
        let pending = b".ja-sandbox-log-helper-42-999991.marker.pending";
        let contents = b"owner_pid=42\nnonce=999991\npid=999991\npgid=999992\nstart_identity=fixture\nexecutable_kind=log\nstate=active\n";
        let (root_directory, root_identity) =
            open_verified_root_directory(&root).expect("open root");
        let root_fd = root_directory.as_raw_fd();
        let mut target = fd::create_at_file(root_fd, original, MARKER_MODE).expect("target");
        target.write_all(contents).expect("target image");
        target.sync_all().expect("target sync");
        drop(target);
        prepare_recovery_backup(
            &root,
            &root_directory,
            root_identity,
            alternate,
            pending,
            contents,
            Instant::now() + CLEANUP_DEADLINE,
        )
        .expect("retained backup");
        fd::unlink_at(root_fd, original).expect("simulate target unlink");
        fd::sync_directory(&root_directory).expect("target unlink sync");

        let mut plan =
            RestoreFaultPlan::new([RestoreFaultPoint::Write(0), RestoreFaultPoint::Write(1)]);
        let outcome = restore_recovery_evidence_with_fault(
            RecoveryRestoreRequest {
                root: &root,
                root_directory: &root_directory,
                root_identity,
                original,
                alternate,
                contents,
                deadline: Instant::now() + CLEANUP_DEADLINE,
            },
            &mut plan,
        )
        .expect("retained backup should satisfy both write faults");
        assert!(outcome.durable_survivor);
        assert!(!outcome.sibling_cleanup_failed);
        assert!(
            !root
                .join(std::str::from_utf8(original).expect("name"))
                .exists()
        );
        assert_eq!(
            fs::read(root.join(std::str::from_utf8(alternate).expect("name")))
                .expect("backup image"),
            contents
        );

        let report = root.join("cleanup-report");
        cleanup_markers_until(&root, &report, false, Instant::now() + CLEANUP_DEADLINE)
            .expect("restart cleanup");
        let unresolved = fs::read_dir(&root)
            .expect("read root")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".ja-sandbox-cleaned.")
                    || name.starts_with(".ja-sandbox-recovery.")
                    || name.ends_with(".pending")
            })
            .collect::<Vec<_>>();
        assert!(unresolved.is_empty(), "unresolved evidence: {unresolved:?}");
        drop(root_directory);
        fs::remove_file(report).expect("report cleanup");
        fs::remove_dir(&root).expect("root cleanup");
    }

    /// A failed candidate unlink must remain recoverable on restart.  The
    /// production scan may remove it only after reopening and durably proving
    /// its valid cleaned sibling; a post-unlink fault leaves no stale pair.
    #[cfg(target_os = "macos")]
    #[test]
    fn restore_candidate_cleanup_faults_close_on_restart() {
        for fault in [
            RestoreFaultPoint::CandidateUnlink(0),
            RestoreFaultPoint::CandidatePostcheck(0),
        ] {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("ja-marker-restart-{nonce}"));
            fs::DirBuilder::new()
                .recursive(false)
                .mode(0o700)
                .create(&root)
                .expect("root");
            let original = b".ja-sandbox-recovery.ja-sandbox-log-helper-42-999991.marker";
            let alternate = b".ja-sandbox-cleaned.ja-sandbox-log-helper-42-999991.marker";
            let contents = b"owner_pid=42\nnonce=999991\npid=999991\npgid=999992\nstart_identity=fixture\nexecutable_kind=log\nstate=active\n";
            let (root_directory, root_identity) =
                open_verified_root_directory(&root).expect("open root");
            let mut plan = RestoreFaultPlan::new([RestoreFaultPoint::Write(0), fault]);
            let outcome = restore_recovery_evidence_with_fault(
                RecoveryRestoreRequest {
                    root: &root,
                    root_directory: &root_directory,
                    root_identity,
                    original,
                    alternate,
                    contents,
                    deadline: Instant::now() + CLEANUP_DEADLINE,
                },
                &mut plan,
            )
            .expect("valid sibling must survive candidate cleanup fault");
            assert!(outcome.durable_survivor);
            assert!(outcome.sibling_cleanup_failed);

            let report = root.join("cleanup-report");
            cleanup_markers_until(&root, &report, false, Instant::now() + CLEANUP_DEADLINE)
                .expect("restart cleanup");
            let leftovers = fs::read_dir(&root)
                .expect("read root")
                .map(|entry| entry.expect("entry").file_name())
                .filter(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".ja-sandbox-cleaned.")
                        || name.starts_with(".ja-sandbox-recovery.")
                        || name.ends_with(".pending")
                })
                .collect::<Vec<_>>();
            assert!(
                leftovers.is_empty(),
                "unresolved recovery pair: {leftovers:?}"
            );
            drop(root_directory);
            fs::remove_file(report).expect("report cleanup");
            fs::remove_dir(&root).expect("root cleanup");
        }
    }

    /// Prove a directory-entry backend error clears previously collected
    /// targets and reports a stable category instead of silently skipping it.
    #[test]
    fn marker_scan_entry_error_is_fail_closed() {
        let mut result = marker::ScanResult {
            records: vec![MarkerRecord {
                path: PathBuf::new(),
                suffix: "marker".into(),
                file_identity: marker::MarkerFileIdentity {
                    dev: 0,
                    ino: 0,
                    nlink: 1,
                    mode: MARKER_MODE,
                    uid: process::current_uid(),
                },
                owner_pid: 7,
                nonce: 8,
                pid: 9,
                pgid: 10,
                start_identity: "start".into(),
                executable_kind: "log".into(),
            }],
            pending: Vec::new(),
            categories: Vec::new(),
        };
        let entries = std::iter::once::<std::io::Result<std::fs::DirEntry>>(Err(
            std::io::Error::other("fixture entry error"),
        ));
        marker::scan_entries(entries, false, &mut result);
        assert!(result.records.is_empty());
        assert!(result.pending.is_empty());
        assert_eq!(result.categories, vec!["marker-entry-invalid"]);
    }

    /// Reject a replaced/symlinked marker root before directory enumeration so
    /// cleanup cannot be redirected to a different same-user tree.
    #[cfg(target_os = "macos")]
    #[test]
    fn marker_scan_rejects_symlink_root() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-root-{nonce}"));
        let link = std::env::temp_dir().join(format!("ja-marker-root-link-{nonce}"));
        std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        symlink(&root, &link).expect("symlink");
        let scan = scan_root(&link, false);
        assert!(scan.categories.contains(&"marker-root-invalid"));
        std::fs::remove_file(link).expect("link cleanup");
        std::fs::remove_dir(root).expect("root cleanup");
    }

    /// Exercise the descriptor-anchored unlink path and prove a root pathname
    /// replacement is rejected before the old inode can be removed through a
    /// redirected name.
    #[cfg(target_os = "macos")]
    #[test]
    fn marker_unlink_rechecks_root_identity() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ja-marker-unlink-{nonce}"));
        let old_root = std::env::temp_dir().join(format!("ja-marker-unlink-old-{nonce}"));
        fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&root)
            .expect("root");
        let marker_path = root.join("marker");
        let mut marker_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(MARKER_MODE)
            .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
            .open(&marker_path)
            .expect("marker");
        marker_file.write_all(b"marker").expect("marker contents");
        marker_file.sync_all().expect("marker sync");
        let file_metadata = marker_file.metadata().expect("marker metadata");
        let expected = marker::MarkerFileIdentity {
            dev: file_metadata.dev(),
            ino: file_metadata.ino(),
            nlink: file_metadata.nlink(),
            mode: file_metadata.permissions().mode() & 0o777,
            uid: file_metadata.uid(),
        };
        drop(marker_file);
        let (root_directory, root_identity) = open_verified_root_directory(&root).expect("open");
        std::fs::rename(&root, &old_root).expect("move root");
        symlink(&old_root, &root).expect("replace root");
        assert!(
            unlink_verified_entry(
                &root,
                &root_directory,
                root_identity,
                &root.join("marker"),
                expected,
                Instant::now() + CLEANUP_DEADLINE,
            )
            .is_err()
        );
        assert!(old_root.join("marker").exists());
        std::fs::remove_file(&root).expect("link cleanup");
        std::fs::rename(&old_root, &root).expect("restore root");
        std::fs::remove_file(root.join("marker")).expect("marker cleanup");
        std::fs::remove_dir(root).expect("root cleanup");
    }
}
