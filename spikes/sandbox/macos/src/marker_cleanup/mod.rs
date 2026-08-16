// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native marker cleanup shared by the workflow and its fixture tests.

mod marker;
mod process;
mod process_scan;

#[cfg(target_os = "macos")]
mod fixture;

use marker::{MarkerRecord, PendingMarker, scan_root};
use process::{ProcessIdentity, current_pgid, query_identity, terminate_group};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Stable identity data that a residual process-table scan may compare
/// without exposing platform-specific process output to callers.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ControlledProcessIdentity {
    pub pid: u32,
    pub pgid: i32,
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
        comm: identity.comm.clone(),
        start_identity: identity.start_identity.clone(),
    };
    terminate_group(&captured, Instant::now() + deadline)
}

const CLEANUP_DEADLINE: Duration = Duration::from_secs(20);
const MARKER_MODE: u32 = 0o600;
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;

/// Run production cleanup against the runner marker root; all observable
/// report values are fixed categories so paths and marker contents never leak.
pub fn cleanup_markers(
    root: &Path,
    report: &Path,
    allow_fixture: bool,
) -> Result<(), &'static str> {
    let own_pgid = current_pgid();
    if own_pgid <= 1 {
        return write_failure_report(report, "marker-group-unsafe");
    }
    let scan = scan_root(root, allow_fixture);
    let mut report_categories = BTreeSet::new();
    report_categories.extend(scan.categories.iter().copied());
    let mut cleaned = BTreeSet::new();

    for record in &scan.records {
        let identity = (record.owner_pid, record.nonce);
        if !cleaned.insert(identity) {
            continue;
        }
        if record.pgid == own_pgid || record.pgid <= 1 {
            report_categories.insert("marker-group-unsafe");
            continue;
        }
        let actual = match query_identity(record.pid) {
            Ok(identity) => identity,
            Err(category) => {
                report_categories.insert(category);
                continue;
            }
        };
        if !identity_matches(record, &actual, allow_fixture) {
            report_categories.insert("marker-owner-mismatch");
            continue;
        }
        match terminate_group(&actual, Instant::now() + CLEANUP_DEADLINE) {
            Ok(()) => {
                let group = scan
                    .records
                    .iter()
                    .filter(|candidate| {
                        candidate.owner_pid == record.owner_pid && candidate.nonce == record.nonce
                    })
                    .collect::<Vec<_>>();
                if remove_identity_markers(root, &group, &mut report_categories).is_err() {
                    report_categories.insert("marker-remove-failed");
                }
            }
            Err(category) => {
                report_categories.insert(category);
            }
        }
    }

    // Pending activation files never contain a trusted process identity and
    // therefore are removed without sending any signal.
    for pending in scan.pending {
        report_categories.insert("marker-pending");
        if remove_pending_marker(&pending).is_err() {
            report_categories.insert("marker-remove-failed");
        }
    }
    // The marker directory entry is part of the durable cleanup evidence; a
    // successful unlink without a parent-directory sync cannot be reported as
    // complete after a runner crash/restart boundary.
    if File::open(root)
        .and_then(|directory| directory.sync_all())
        .is_err()
    {
        report_categories.insert("marker-remove-failed");
    }
    write_report(report, &report_categories, scan.records.len())?;
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

/// Match the marker's immutable identity against one fresh process query;
/// process-name and start-time checks prevent PID reuse from receiving a kill.
fn identity_matches(record: &MarkerRecord, actual: &ProcessIdentity, allow_fixture: bool) -> bool {
    actual.pid == record.pid
        && actual.pgid == record.pgid
        && actual.start_identity == record.start_identity
        && ((record.executable_kind == "log"
            && matches!(actual.comm.as_str(), "log" | "/usr/bin/log"))
            || (allow_fixture
                && record.executable_kind == "fixture"
                && matches!(actual.comm.as_str(), "sh" | "/bin/sh")))
}

/// Remove only the exact sibling marker names derived from a validated owner
/// and nonce, after the direct PID and complete group both report ESRCH.
fn remove_identity_markers(
    _root: &Path,
    records: &[&MarkerRecord],
    categories: &mut BTreeSet<&'static str>,
) -> Result<(), ()> {
    let mut result = Ok(());
    for record in records {
        let file = match open_verified_marker(&record.path, record.file_identity) {
            Ok(file) => file,
            Err(()) => {
                categories.insert("marker-remove-failed");
                result = Err(());
                continue;
            }
        };
        if fs::remove_file(&record.path).is_err() {
            categories.insert("marker-remove-failed");
            result = Err(());
        }
        // Keep the descriptor alive through unlink so the identity used for
        // the final check remains owned until the path operation completes.
        drop(file);
    }
    result
}

/// Re-open each marker with no-follow flags and retain the descriptor through
/// unlink; a path/inode swap after the initial scan therefore fails closed
/// before any marker deletion is attempted.
fn open_verified_marker(path: &Path, expected: marker::MarkerFileIdentity) -> Result<File, ()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    let actual = marker::MarkerFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    let path_metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    let path_identity = marker::MarkerFileIdentity {
        dev: path_metadata.dev(),
        ino: path_metadata.ino(),
        nlink: path_metadata.nlink(),
        mode: path_metadata.permissions().mode() & 0o777,
        uid: path_metadata.uid(),
    };
    (metadata.file_type().is_file()
        && !path_metadata.file_type().is_symlink()
        && actual == expected
        && path_identity == actual)
        .then_some(file)
        .ok_or(())
}

/// Apply the same identity recheck to incomplete activation files; a pending
/// marker is never trusted as a process identity, but it is still evidence.
fn remove_pending_marker(pending: &PendingMarker) -> Result<(), ()> {
    let file = open_verified_marker(&pending.path, pending.file_identity)?;
    // Keep the no-follow descriptor alive until the pending evidence unlink
    // returns, so the identity check is not separated from deletion.
    let result = fs::remove_file(&pending.path).map_err(|_| ());
    drop(file);
    result
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
    let Some(parent) = report.parent() else {
        return Err("report-path-invalid");
    };
    fs::create_dir_all(parent).map_err(|_| "report-open-failed")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(report)
        .map_err(|_| "report-open-failed")?;
    for category in categories {
        writeln!(file, "{category}=true").map_err(|_| "report-write-failed")?;
    }
    writeln!(file, "{count_label}={count}").map_err(|_| "report-write-failed")?;
    file.sync_all().map_err(|_| "report-write-failed")?;
    File::open(parent)
        .map_err(|_| "report-write-failed")?
        .sync_all()
        .map_err(|_| "report-write-failed")?;
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
    use std::os::unix::fs::{DirBuilderExt, symlink};
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
            comm: "log".into(),
            start_identity: "start".into(),
        };
        assert!(identity_matches(&record, &actual, false));
        assert!(!identity_matches(
            &record,
            &ProcessIdentity {
                start_identity: "other".into(),
                ..actual
            },
            false
        ));
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
}
