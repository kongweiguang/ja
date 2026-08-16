// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native fixture cases that call the production marker cleanup implementation.

use super::marker::write_fixture_marker;
use super::process::{
    EPERM, ProcessIdentity, ProcessState, abort_unreaped_query, classify_errno, current_pgid,
    group_state, pid_state, query_identity, reap_child_bounded, reap_child_without_group,
    set_nonblocking, terminate_group,
};
use super::{MARKER_MODE, O_CLOEXEC_FLAG, O_NOFOLLOW_FLAG, cleanup_markers};
use crate::spawn_grouped;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Run forged, pending, residual-descendant and exact-EPERM cases using the
/// same scan/signal implementation that production workflow cleanup invokes.
pub(super) fn run() -> Result<(), &'static str> {
    if classify_errno(EPERM) != super::process::ProcessState::PermissionDenied {
        return Err("fixture-eperm-classification");
    }
    forged_case()?;
    pending_case()?;
    residual_group_case()?;
    descendant_case()?;
    println!("marker-cleanup-fixtures=pass");
    Ok(())
}

/// Prove a forged owner field is rejected before the production code signals.
fn forged_case() -> Result<(), &'static str> {
    let (root, report) = fixture_paths("forged");
    let path = root.join(format!(
        "ja-sandbox-log-helper-{}-11.marker",
        std::process::id()
    ));
    write_fixture_marker(
        &path,
        "owner_pid=999999\nnonce=11\npid=999999\npgid=999999\nstart_identity=forged\nexecutable_kind=log\nstate=active\n",
    )?;
    if cleanup_markers(&root, &report, false).is_ok()
        || !report_contains(&report, "marker-owner-mismatch=true")?
    {
        return Err("fixture-forged-marker");
    }
    remove_fixture_root(root)
}

/// Prove pending activation is reported and removed without any signal path.
fn pending_case() -> Result<(), &'static str> {
    let (root, report) = fixture_paths("pending");
    let path = root.join(format!(
        ".ja-sandbox-log-helper-{}-12.marker.pending",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(&path)
        .map_err(|_| "fixture-pending-create")?;
    file.write_all(b"pending")
        .map_err(|_| "fixture-pending-write")?;
    file.sync_all().map_err(|_| "fixture-pending-write")?;
    if cleanup_markers(&root, &report, false).is_ok()
        || !report_contains(&report, "marker-pending=true")?
    {
        return Err("fixture-pending-marker");
    }
    remove_fixture_root(root)
}

/// Prove a marker naming the cleanup process group is retained and reported
/// as residual/unsafe instead of ever signalling the workflow itself.
fn residual_group_case() -> Result<(), &'static str> {
    let (root, report) = fixture_paths("residual");
    let pid = std::process::id();
    let process_group = current_pgid();
    let path = root.join(format!(
        "ja-sandbox-log-helper-{}-13.marker",
        std::process::id()
    ));
    write_fixture_marker(
        &path,
        &format!(
            "owner_pid={pid}\nnonce=13\npid={pid}\npgid={process_group}\nstart_identity=fixture\nexecutable_kind=fixture\nstate=active\n"
        ),
    )?;
    if cleanup_markers(&root, &report, true).is_ok()
        || !report_contains(&report, "marker-group-unsafe=true")?
        || !path.exists()
    {
        let _ = fs::remove_file(&path);
        let _ = remove_fixture_root(root);
        return Err("fixture-residual-group");
    }
    fs::remove_file(path).map_err(|_| "fixture-residual-cleanup")?;
    remove_fixture_root(root)
}

/// Spawn a real descendant group, write a fixture marker, then let production
/// cleanup signal and verify both direct PID and group reach exact ESRCH.
fn descendant_case() -> Result<(), &'static str> {
    let (root, report) = fixture_paths("descendant");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "/bin/sh -c '/bin/sleep 30 & wait' </dev/null >/dev/null 2>/dev/null & echo $!",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut launcher = spawn_grouped(&mut command).map_err(|_| "fixture-helper-spawn")?;
    let process_group = match i32::try_from(launcher.id()).ok().filter(|value| *value > 1) {
        Some(process_group) => process_group,
        None => {
            if !reap_child_without_group(&mut launcher) {
                abort_unreaped_query("fixture-invalid-process-group");
            }
            // A converted PID that cannot form a safe process-group target
            // leaves descendants unaddressable; after direct reap, abort
            // instead of returning and relying on marker discovery.
            abort_unreaped_query("fixture-group-unsafe");
        }
    };
    // Capture the launcher identity while it is still alive so an early pipe
    // or parsing fault can still use the same identity-checked group cleanup.
    let launcher_identity = query_identity(launcher.id())
        .ok()
        .filter(|identity| identity.pgid == process_group);
    let mut stdout = match launcher.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-pipe",
            );
        }
    };
    if set_nonblocking(&stdout).is_err() {
        drop(stdout);
        return finish_launcher_failure(
            &mut launcher,
            process_group,
            launcher_identity.as_ref(),
            "fixture-helper-pipe",
        );
    }
    let output = match read_launcher_output(
        &mut stdout,
        &mut launcher,
        Instant::now() + Duration::from_secs(2),
    ) {
        Ok(output) => output,
        Err(error) => {
            drop(stdout);
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                error,
            );
        }
    };
    drop(stdout);
    let pid = match output.trim().parse::<u32>() {
        Ok(pid) if pid > 1 => pid,
        Err(_) => {
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-pid",
            );
        }
        Ok(_) => {
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-pid",
            );
        }
    };
    let identity = match query_identity(pid) {
        Ok(identity) => identity,
        Err(_) => {
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-query",
            );
        }
    };
    if identity.pgid != process_group {
        return finish_launcher_failure(
            &mut launcher,
            process_group,
            launcher_identity.as_ref(),
            "fixture-helper-identity",
        );
    }
    let nonce = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => {
            return finish_launcher_failure(
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-clock",
            );
        }
    };
    let path = root.join(format!(
        "ja-sandbox-log-helper-{}-{nonce}.marker",
        std::process::id()
    ));
    let contents = format!(
        "owner_pid={}\nnonce={nonce}\npid={pid}\npgid={process_group}\nstart_identity={}\nexecutable_kind=fixture\nstate=active\n",
        std::process::id(),
        identity.start_identity
    );
    if write_fixture_marker(&path, &contents).is_err() {
        return finish_launcher_failure(
            &mut launcher,
            process_group,
            Some(&identity),
            "fixture-marker-write",
        );
    }
    let result = cleanup_markers(&root, &report, true);
    // Keep the launcher Child owned until production cleanup has observed and
    // terminated its real descendant; reaping before this point would make the
    // fixture prove only a helper exit, not the marker cleanup contract.
    let launcher_result = reap_child_bounded(
        &mut launcher,
        process_group,
        Instant::now() + Duration::from_secs(2),
    );
    if result.is_err() {
        // Preserve only the production report's fixed category so a native
        // runner can distinguish residual, identity, query and signal faults
        // without exposing a PID, path, locale text or marker contents.
        let category = fixture_cleanup_failure_category(&report);
        return finish_descendant_failure(&identity, launcher_result, category);
    }
    if let Err(error) = launcher_result {
        return finish_descendant_failure(&identity, Err(error), "fixture-launcher-cleanup");
    }
    remove_fixture_root(root)
}

/// Finish a pre-marker launcher failure while its Child remains owned; an
/// unresolved direct/group cleanup is fatal rather than a recoverable fixture
/// result because no trusted marker exists for an outer cleanup pass.
fn finish_launcher_failure(
    launcher: &mut std::process::Child,
    process_group: i32,
    identity: Option<&ProcessIdentity>,
    category: &'static str,
) -> Result<(), &'static str> {
    let identity_result = identity
        .map(|identity| terminate_group(identity, Instant::now() + Duration::from_secs(2)))
        .unwrap_or(Ok(()));
    let reap_result = reap_child_bounded(
        launcher,
        process_group,
        Instant::now() + Duration::from_secs(2),
    );
    match (identity_result, reap_result) {
        (Ok(()), Ok(())) => Err(category),
        (Err(error), _) | (_, Err(error)) => abort_fixture_group(error),
    }
}

/// Complete a post-marker failure through the captured identity and direct
/// launcher ownership, tolerating only an already-empty target group.  This
/// keeps every exception path fail closed without reusing a stale numeric PID.
fn finish_descendant_failure(
    identity: &ProcessIdentity,
    launcher_result: Result<(), &'static str>,
    category: &'static str,
) -> Result<(), &'static str> {
    let identity_result = match terminate_group(identity, Instant::now() + Duration::from_secs(2)) {
        Ok(()) => Ok(()),
        Err(_error)
            if pid_state(i32::try_from(identity.pid).unwrap_or(-1)) == ProcessState::Empty
                && group_state(identity.pgid) == ProcessState::Empty =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    };
    match (launcher_result, identity_result) {
        (Ok(()), Ok(())) => Err(category),
        (Err(error), _) | (_, Err(error)) => abort_fixture_group(error),
    }
}

/// Convert a production cleanup report into a closed fixture vocabulary; an
/// unreadable or unfamiliar report is itself a hard diagnostic failure rather
/// than an opportunity to silently classify a live descendant as cleaned.
fn fixture_cleanup_failure_category(report: &Path) -> &'static str {
    let contents = match fs::read_to_string(report) {
        Ok(contents) => contents,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    let categories = contents.lines().collect::<std::collections::BTreeSet<_>>();
    if categories.contains("marker-residual") {
        "fixture-descendant-cleanup-residual"
    } else if categories.contains("marker-eperm") {
        "fixture-descendant-cleanup-eperm"
    } else if categories.contains("marker-signal-failed") {
        "fixture-descendant-cleanup-signal"
    } else if categories.contains("marker-process-probe-failed")
        || categories.contains("marker-query-unreaped")
        || categories.contains("marker-query-group-residual")
    {
        "fixture-descendant-cleanup-query"
    } else if categories.contains("marker-owner-mismatch")
        || categories.contains("marker-identity-lost")
    {
        "fixture-descendant-cleanup-identity"
    } else if categories.contains("marker-remove-failed") {
        "fixture-descendant-cleanup-remove"
    } else if categories.contains("marker-entry-invalid")
        || categories.contains("marker-root-invalid")
        || categories.contains("marker-stat-invalid")
        || categories.contains("marker-incomplete")
    {
        "fixture-descendant-cleanup-scan"
    } else if categories.contains("marker-group-unsafe") {
        "fixture-descendant-cleanup-unsafe"
    } else {
        "fixture-descendant-cleanup-unknown"
    }
}

/// Convert every group cleanup failure to a stable, path-free fail-closed
/// category; unknown backend states are never treated as successful cleanup.
fn abort_fixture_group(category: &'static str) -> ! {
    abort_unreaped_query(fixture_group_failure_reason(category))
}

/// Map backend failures to a closed vocabulary before the process aborts; no
/// OS text or caller-controlled value can reach the diagnostic stream.
fn fixture_group_failure_reason(category: &'static str) -> &'static str {
    match category {
        "marker-residual" => "fixture-group-residual",
        "marker-eperm" => "fixture-group-eperm",
        "marker-process-probe-failed" => "fixture-group-probe-failed",
        "marker-signal-failed" => "fixture-group-signal-failed",
        _ => "fixture-group-failed",
    }
}

/// Keep the failure return separate from finalization so a table-driven test
/// can prove every early category invokes cleanup before becoming observable.
#[cfg(test)]
fn finish_fixture_failure_with<F>(category: &'static str, finalize: F) -> Result<(), &'static str>
where
    F: FnOnce() -> Result<(), &'static str>,
{
    finalize()?;
    Err(category)
}

/// Read only the launcher PID line with a hard byte/deadline budget; the
/// fixture must not use an unbounded `read_to_string` on a descendant pipe.
fn read_launcher_output(
    reader: &mut impl Read,
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<String, &'static str> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 128];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > 256 {
                    return Err("fixture-helper-output");
                }
                if output.contains(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child
                    .try_wait()
                    .map_err(|_| "fixture-helper-output")?
                    .is_some()
                {
                    continue;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err("fixture-helper-output"),
        }
    }
    if output.is_empty() || !output.contains(&b'\n') {
        return Err("fixture-helper-output");
    }
    String::from_utf8(output).map_err(|_| "fixture-helper-output")
}

/// Allocate a private temporary fixture root and fixed report path.
fn fixture_paths(label: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!(
        "ja-marker-cleanup-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(&root)
        .expect("fixture root");
    let report = root.join("report.log");
    (root, report)
}

/// Read only fixed report categories; fixture assertions never inspect paths
/// or platform error text.
fn report_contains(report: &Path, category: &str) -> Result<bool, &'static str> {
    Ok(fs::read_to_string(report)
        .map_err(|_| "fixture-report-read")?
        .lines()
        .any(|line| line == category))
}

/// Remove only the exact private fixture root created by `fixture_paths`.
fn remove_fixture_root(root: PathBuf) -> Result<(), &'static str> {
    fs::remove_dir_all(root).map_err(|_| "fixture-root-cleanup")
}

#[cfg(test)]
mod tests {
    use super::{
        finish_fixture_failure_with, fixture_cleanup_failure_category, fixture_group_failure_reason,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Exercise every early launcher category through the shared finalizer
    /// adapter, ensuring a failure can never bypass group cleanup ordering.
    #[test]
    fn early_failure_categories_finalize_before_return() {
        let categories = [
            "fixture-helper-pipe",
            "fixture-helper-output",
            "fixture-helper-pid",
            "fixture-helper-query",
            "fixture-clock",
            "fixture-marker-write",
            "fixture-descendant-cleanup",
        ];
        for category in categories {
            let mut finalizer_calls = 0;
            let result = finish_fixture_failure_with(category, || {
                finalizer_calls += 1;
                Ok(())
            });
            assert_eq!(result, Err(category));
            assert_eq!(finalizer_calls, 1);
        }
    }

    /// Prove a finalizer error propagates instead of allowing an early
    /// category to report success; production maps this state to abort.
    #[test]
    fn early_failure_finalizer_error_is_not_hidden() {
        let result =
            finish_fixture_failure_with("fixture-helper-output", || Err("marker-residual"));
        assert_eq!(result, Err("marker-residual"));
    }

    /// Keep residual, EPERM, probe and unknown failures on fixed diagnostic
    /// categories before the production abort path emits them.
    #[test]
    fn group_failure_categories_are_redacted() {
        let cases = [
            ("marker-residual", "fixture-group-residual"),
            ("marker-eperm", "fixture-group-eperm"),
            ("marker-process-probe-failed", "fixture-group-probe-failed"),
            ("marker-signal-failed", "fixture-group-signal-failed"),
            ("unexpected", "fixture-group-failed"),
        ];
        for (category, expected) in cases {
            assert_eq!(fixture_group_failure_reason(category), expected);
        }
    }

    /// Keep each report family on a stable, path-free category so arm64
    /// launcher failures expose their real cleanup phase instead of a timeout.
    #[test]
    fn descendant_cleanup_report_categories_are_specific() {
        let root = std::env::temp_dir().join(format!(
            "ja-fixture-category-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir(&root).expect("category root");
        let cases = [
            (
                "marker-residual=true\n",
                "fixture-descendant-cleanup-residual",
            ),
            ("marker-eperm=true\n", "fixture-descendant-cleanup-eperm"),
            (
                "marker-signal-failed=true\n",
                "fixture-descendant-cleanup-signal",
            ),
            (
                "marker-process-probe-failed=true\n",
                "fixture-descendant-cleanup-query",
            ),
            (
                "marker-owner-mismatch=true\n",
                "fixture-descendant-cleanup-identity",
            ),
            (
                "marker-identity-lost=true\n",
                "fixture-descendant-cleanup-identity",
            ),
            (
                "marker-remove-failed=true\n",
                "fixture-descendant-cleanup-remove",
            ),
            (
                "marker-entry-invalid=true\n",
                "fixture-descendant-cleanup-scan",
            ),
            (
                "marker-group-unsafe=true\n",
                "fixture-descendant-cleanup-unsafe",
            ),
            ("unrecognized=true\n", "fixture-descendant-cleanup-unknown"),
        ];
        let report = root.join("report.log");
        for (contents, expected) in cases {
            fs::write(&report, contents).expect("category report");
            assert_eq!(fixture_cleanup_failure_category(&report), expected);
        }
        fs::remove_file(&report).expect("remove report");
        assert_eq!(
            fixture_cleanup_failure_category(&report),
            "fixture-descendant-cleanup-report"
        );
        fs::remove_dir(root).expect("remove category root");
    }
}
