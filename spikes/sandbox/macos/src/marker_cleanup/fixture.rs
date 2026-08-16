// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native fixture cases that call the production marker cleanup implementation.

use super::marker::write_fixture_marker;
use super::process::{
    EPERM, abort_unreaped_query, classify_errno, current_pgid, query_identity, reap_child_bounded,
    reap_child_without_group, set_nonblocking, terminate_group_only,
};
use super::{MARKER_MODE, O_CLOEXEC_FLAG, O_NOFOLLOW_FLAG, cleanup_markers};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
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
    let mut launcher = Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sh -c '/bin/sleep 30 & wait' </dev/null >/dev/null 2>/dev/null & echo $!",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| "fixture-helper-spawn")?;
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
    let mut stdout = match launcher.stdout.take() {
        Some(stdout) => stdout,
        None => {
            reap_child_bounded(
                &mut launcher,
                process_group,
                Instant::now() + Duration::from_secs(1),
            )?;
            return finish_fixture_failure(process_group, "fixture-helper-pipe");
        }
    };
    if set_nonblocking(&stdout).is_err() {
        drop(stdout);
        reap_child_bounded(
            &mut launcher,
            process_group,
            Instant::now() + Duration::from_secs(1),
        )?;
        return finish_fixture_failure(process_group, "fixture-helper-pipe");
    }
    let output = match read_launcher_output(
        &mut stdout,
        &mut launcher,
        Instant::now() + Duration::from_secs(2),
    ) {
        Ok(output) => output,
        Err(error) => {
            drop(stdout);
            reap_child_bounded(
                &mut launcher,
                process_group,
                Instant::now() + Duration::from_secs(1),
            )?;
            return finish_fixture_failure(process_group, error);
        }
    };
    drop(stdout);
    reap_child_bounded(
        &mut launcher,
        process_group,
        Instant::now() + Duration::from_secs(1),
    )?;
    let pid = match output.trim().parse::<u32>() {
        Ok(pid) => pid,
        Err(_) => {
            return finish_fixture_failure(process_group, "fixture-helper-pid");
        }
    };
    let identity = match query_identity(pid) {
        Ok(identity) => identity,
        Err(_) => {
            return finish_fixture_failure(process_group, "fixture-helper-query");
        }
    };
    let nonce = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => {
            return finish_fixture_failure(process_group, "fixture-clock");
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
        return finish_fixture_failure(process_group, "fixture-marker-write");
    }
    let result = cleanup_markers(&root, &report, true);
    if result.is_err() {
        return finish_fixture_failure(process_group, "fixture-descendant-cleanup");
    }
    remove_fixture_root(root)
}

/// Re-run bounded group cleanup once after a residual observation; any final
/// failure aborts with a fixed category because this early fixture path has
/// no marker that an outer glob could safely use as a cleanup substitute.
fn finalize_fixture_group(process_group: i32) -> Result<(), &'static str> {
    let first = terminate_group_only(process_group, Instant::now() + Duration::from_secs(1));
    if first.is_ok() {
        return Ok(());
    }
    match terminate_group_only(process_group, Instant::now() + Duration::from_secs(1)) {
        Ok(()) => Ok(()),
        Err(category) => abort_fixture_group(category),
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

/// Route every early launcher failure through the same group finalizer before
/// returning its fixed fixture category; this keeps the error table honest.
fn finish_fixture_failure(process_group: i32, category: &'static str) -> Result<(), &'static str> {
    finish_fixture_failure_with(category, || finalize_fixture_group(process_group))
}

/// Keep the failure return separate from finalization so a table-driven test
/// can prove every early category invokes cleanup before becoming observable.
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
    use super::{finish_fixture_failure_with, fixture_group_failure_reason};

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
}
