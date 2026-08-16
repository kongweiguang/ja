// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native marker cleanup shared by the workflow and its fixture tests.

mod marker;
mod process;

#[cfg(target_os = "macos")]
mod fixture;

use marker::{MarkerRecord, scan_root};
use process::{ProcessIdentity, current_pgid, query_identity, terminate_group};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        match terminate_group(record.pid, record.pgid, Instant::now() + CLEANUP_DEADLINE) {
            Ok(()) => {
                if remove_identity_markers(root, record, &mut report_categories).is_err() {
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
        if fs::remove_file(pending).is_err() {
            report_categories.insert("marker-remove-failed");
        }
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
    actual.pgid == record.pgid
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
    root: &Path,
    record: &MarkerRecord,
    categories: &mut BTreeSet<&'static str>,
) -> Result<(), ()> {
    let stem = format!(
        "ja-sandbox-log-helper-{}-{}",
        record.owner_pid, record.nonce
    );
    let mut result = Ok(());
    for suffix in ["marker", "fallback", "emergency"] {
        let path = root.join(format!("{stem}.{suffix}"));
        if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            categories.insert("marker-remove-failed");
            result = Err(());
        }
    }
    result
}

/// Write a bounded, fixed-category report with owner-only permissions so the
/// outer workflow can decide success without parsing locale-dependent errors.
fn write_report(
    report: &Path,
    categories: &BTreeSet<&'static str>,
    marker_count: usize,
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
    writeln!(file, "marker-count={marker_count}").map_err(|_| "report-write-failed")?;
    file.sync_all().map_err(|_| "report-write-failed")?;
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
            owner_pid: 7,
            nonce: 8,
            pid: 9,
            pgid: 10,
            start_identity: "start".into(),
            executable_kind: "log".into(),
        };
        let actual = ProcessIdentity {
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
