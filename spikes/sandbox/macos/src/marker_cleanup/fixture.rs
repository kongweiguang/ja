// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native fixture cases that call the production marker cleanup implementation.

use super::marker::write_fixture_marker;
use super::process::{
    EPERM, ProcessIdentity, ProcessState, abort_unreaped_query, classify_errno, current_pgid,
    current_uid, group_state, pid_state, query_identity, reap_child_bounded,
    reap_child_group_bounded, reap_child_without_group, set_nonblocking, terminate_group,
};
use super::{MARKER_MODE, O_CLOEXEC_FLAG, O_NOFOLLOW_FLAG, cleanup_markers};
use crate::spawn_grouped;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
    let process_group = match i32::try_from(launcher.id()) {
        Ok(process_group) if process_group > 1 => process_group,
        _ => {
            let evidence_result = persist_fixture_failure_without_group(
                &root,
                launcher.id(),
                "fixture-invalid-process-group",
            );
            let reaped = reap_child_without_group(&mut launcher);
            if evidence_result.is_err() {
                abort_unreaped_query("fixture-failure-evidence");
            }
            if !reaped {
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
    let launcher_identity =
        match accept_launcher_identity(query_identity(launcher.id()), launcher.id(), process_group)
        {
            Ok(identity) => Some(identity),
            Err(category) => {
                return finish_launcher_failure(
                    &root,
                    &mut launcher,
                    process_group,
                    None,
                    category,
                );
            }
        };
    let mut stdout = match launcher.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return finish_launcher_failure(
                &root,
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
            &root,
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
                &root,
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
                &root,
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-pid",
            );
        }
        Ok(_) => {
            return finish_launcher_failure(
                &root,
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
                &root,
                &mut launcher,
                process_group,
                launcher_identity.as_ref(),
                "fixture-helper-query",
            );
        }
    };
    if identity.pgid != process_group {
        return finish_launcher_failure(
            &root,
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
                &root,
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
            &root,
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
        return finish_descendant_failure(
            &root,
            launcher.id(),
            &identity,
            launcher_result,
            category,
        );
    }
    if let Err(error) = launcher_result {
        return finish_descendant_failure(
            &root,
            launcher.id(),
            &identity,
            Err(error),
            "fixture-launcher-cleanup",
        );
    }
    remove_fixture_root(root)
}

/// Finish a pre-marker launcher failure while its Child remains owned; an
/// unresolved direct/group cleanup is fatal rather than a recoverable fixture
/// result because no trusted marker exists for an outer cleanup pass.
fn finish_launcher_failure(
    root: &Path,
    launcher: &mut std::process::Child,
    process_group: i32,
    identity: Option<&ProcessIdentity>,
    category: &'static str,
) -> Result<(), &'static str> {
    let evidence_result =
        persist_fixture_failure(root, launcher.id(), process_group, identity, category);
    let identity_result = identity
        .map(|identity| terminate_group(identity, Instant::now() + Duration::from_secs(2)))
        .unwrap_or(Ok(()));
    let reap_result = if identity.is_some() {
        reap_child_bounded(
            launcher,
            process_group,
            Instant::now() + Duration::from_secs(2),
        )
    } else {
        // The direct Child remains unreaped, so its PID anchors this newly
        // created group even when identity inspection failed.  Kill the whole
        // group before bounded direct-child reap rather than leaking a helper.
        reap_child_group_bounded(
            launcher,
            process_group,
            Instant::now() + Duration::from_secs(2),
        )
    };
    match (evidence_result, identity_result, reap_result) {
        (Ok(()), Ok(()), Ok(())) => Err(category),
        (_, Err(error), _) | (_, _, Err(error)) => abort_fixture_group(error),
        (Err(_), Ok(()), Ok(())) => abort_fixture_group("fixture-failure-evidence"),
    }
}

/// Complete a post-marker failure through the captured identity and direct
/// launcher ownership, tolerating only an already-empty target group.  This
/// keeps every exception path fail closed without reusing a stale numeric PID.
fn finish_descendant_failure(
    root: &Path,
    launcher_pid: u32,
    identity: &ProcessIdentity,
    launcher_result: Result<(), &'static str>,
    category: &'static str,
) -> Result<(), &'static str> {
    let evidence_result =
        persist_fixture_failure(root, launcher_pid, identity.pgid, Some(identity), category);
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
    match (evidence_result, launcher_result, identity_result) {
        (Ok(()), Ok(()), Ok(())) => Err(category),
        (_, Err(error), _) | (_, _, Err(error)) => abort_fixture_group(error),
        (Err(_), Ok(()), Ok(())) => abort_fixture_group("fixture-failure-evidence"),
    }
}

/// Validate the launcher query before any cleanup branch can discard its
/// identity.  A mismatched PID/PGID is classified separately from an OS query
/// error so both faults leave bounded, diagnosable evidence.
fn accept_launcher_identity(
    result: Result<ProcessIdentity, &'static str>,
    launcher_pid: u32,
    process_group: i32,
) -> Result<ProcessIdentity, &'static str> {
    match result {
        Ok(identity)
            if identity.pid == launcher_pid
                && identity.pid > 1
                && identity.pgid == process_group
                && identity.pgid > 1 =>
        {
            Ok(identity)
        }
        Ok(_) => Err("fixture-helper-identity"),
        Err(_) => Err("fixture-helper-query"),
    }
}

const FIXTURE_FAILURE_EVIDENCE: &str = "ja-sandbox-fixture-failure.evidence";
const FIXTURE_FAILURE_EVIDENCE_BYTES: usize = 4096;

/// Persist the usable group/identity facts before any abort or cleanup error
/// can erase the only explanation for an untrusted fixture launcher state.
/// Values originating in process inspection are reduced to bounded ASCII so
/// paths, locale text and injected lines cannot enter the evidence channel.
fn persist_fixture_failure(
    root: &Path,
    launcher_pid: u32,
    process_group: i32,
    identity: Option<&ProcessIdentity>,
    category: &'static str,
) -> Result<(), &'static str> {
    if launcher_pid <= 1
        || process_group <= 1
        || !is_fixture_failure_category(category)
        || identity.is_some_and(|identity| {
            identity.pid <= 1 || identity.pgid <= 1 || identity.pgid != process_group
        })
    {
        return Err("fixture-failure-evidence");
    }
    let (identity_state, identity_pid, identity_pgid, identity_comm, identity_start) =
        match identity {
            Some(identity) => (
                "known",
                identity.pid.to_string(),
                identity.pgid.to_string(),
                evidence_value(&identity.comm),
                evidence_value(&identity.start_identity),
            ),
            None => (
                "unavailable",
                "unknown".to_owned(),
                "unknown".to_owned(),
                "redacted".to_owned(),
                "redacted".to_owned(),
            ),
        };
    let contents = format!(
        "fixture-failure-version=1\ncategory={category}\nlauncher-pid={launcher_pid}\nprocess-group={process_group}\nidentity-state={identity_state}\nidentity-pid={identity_pid}\nidentity-pgid={identity_pgid}\nidentity-comm={identity_comm}\nidentity-start={identity_start}\n"
    );
    if contents.len() > FIXTURE_FAILURE_EVIDENCE_BYTES || !contents.is_ascii() {
        return Err("fixture-failure-evidence");
    }
    write_fixture_failure_evidence(root, &contents)
}

/// Keep the invalid-PID conversion branch diagnosable even though no safe
/// numeric PGID exists; the evidence records that limitation explicitly.
fn persist_fixture_failure_without_group(
    root: &Path,
    launcher_pid: u32,
    category: &'static str,
) -> Result<(), &'static str> {
    if launcher_pid <= 1 || !is_fixture_failure_category(category) {
        return Err("fixture-failure-evidence");
    }
    let contents = format!(
        "fixture-failure-version=1\ncategory={category}\nlauncher-pid={launcher_pid}\nprocess-group=unavailable\nidentity-state=unavailable\nidentity-pid=unknown\nidentity-pgid=unknown\nidentity-comm=redacted\nidentity-start=redacted\n"
    );
    write_fixture_failure_evidence(root, &contents)
}

/// Publish one owner-only failure record with no-follow/create-new semantics;
/// the parent directory sync makes the evidence durable before abort.
fn write_fixture_failure_evidence(root: &Path, contents: &str) -> Result<(), &'static str> {
    if contents.len() > FIXTURE_FAILURE_EVIDENCE_BYTES || !contents.is_ascii() {
        return Err("fixture-failure-evidence");
    }
    let path = root.join(FIXTURE_FAILURE_EVIDENCE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(&path)
        .map_err(|_| "fixture-failure-evidence")?;
    let metadata = file.metadata().map_err(|_| "fixture-failure-evidence")?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != MARKER_MODE
    {
        return Err("fixture-failure-evidence");
    }
    file.write_all(contents.as_bytes())
        .map_err(|_| "fixture-failure-evidence")?;
    file.sync_all().map_err(|_| "fixture-failure-evidence")?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "fixture-failure-evidence")
}

/// Keep process-derived evidence fields line-safe and bounded; an invalid
/// value is redacted instead of allowing raw command output into diagnostics.
fn evidence_value(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
        && !value.contains(['=', '\n', '\r', '/', '\\'])
    {
        value.to_owned()
    } else {
        "redacted".to_owned()
    }
}

/// Accept only fixed fixture failure categories; caller-controlled strings can
/// therefore never become evidence keys or abort diagnostics.
fn is_fixture_failure_category(category: &str) -> bool {
    matches!(
        category,
        "fixture-helper-pipe"
            | "fixture-helper-output"
            | "fixture-helper-pid"
            | "fixture-helper-query"
            | "fixture-helper-identity"
            | "fixture-invalid-process-group"
            | "fixture-clock"
            | "fixture-marker-write"
            | "fixture-descendant-cleanup"
            | "fixture-launcher-cleanup"
            | "fixture-failure-evidence"
            | "fixture-group-residual"
            | "fixture-group-eperm"
            | "fixture-group-probe-failed"
            | "fixture-group-signal-failed"
            | "fixture-group-failed"
            | "fixture-descendant-cleanup-residual"
            | "fixture-descendant-cleanup-eperm"
            | "fixture-descendant-cleanup-signal"
            | "fixture-descendant-cleanup-query"
            | "fixture-descendant-cleanup-identity"
            | "fixture-descendant-cleanup-remove"
            | "fixture-descendant-cleanup-scan"
            | "fixture-descendant-cleanup-unsafe"
            | "fixture-descendant-cleanup-unknown"
            | "fixture-descendant-cleanup-report"
    )
}

const CLEANUP_REPORT_MAX_BYTES: usize = 16 * 1024;
const CLEANUP_REPORT_MAX_LINES: usize = 64;
const CLEANUP_REPORT_MAX_COUNT: usize = 1_000_000;
const CLEANUP_REPORT_CATEGORIES: [&str; 15] = [
    "marker-residual",
    "marker-eperm",
    "marker-signal-failed",
    "marker-process-probe-failed",
    "marker-query-unreaped",
    "marker-query-group-residual",
    "marker-owner-mismatch",
    "marker-identity-lost",
    "marker-remove-failed",
    "marker-entry-invalid",
    "marker-root-invalid",
    "marker-stat-invalid",
    "marker-incomplete",
    "marker-group-unsafe",
    "marker-pending",
];

/// Convert a production cleanup report only after validating its complete,
/// bounded ASCII grammar.  Every category/count is unique and the count is
/// final, so a partial prefix can never hide an injected or conflicting line.
fn fixture_cleanup_failure_category(report: &Path) -> &'static str {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(report)
    {
        Ok(file) => file,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    let mut contents = Vec::new();
    let bytes = match file
        .take((CLEANUP_REPORT_MAX_BYTES + 1) as u64)
        .read_to_end(&mut contents)
    {
        Ok(bytes) => bytes,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    if bytes > CLEANUP_REPORT_MAX_BYTES
        || contents.is_empty()
        || !contents.is_ascii()
        || !contents.ends_with(b"\n")
    {
        return "fixture-descendant-cleanup-report";
    }
    let text = match std::str::from_utf8(&contents) {
        Ok(text) => text,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    let mut seen = [false; CLEANUP_REPORT_CATEGORIES.len()];
    let mut count_seen = false;
    let mut previous_category = None;
    let lines = text.split_terminator('\n').collect::<Vec<_>>();
    if lines.is_empty() || lines.len() > CLEANUP_REPORT_MAX_LINES {
        return "fixture-descendant-cleanup-report";
    }
    for line in lines {
        if line.is_empty() || line.contains('\r') || count_seen {
            return "fixture-descendant-cleanup-report";
        }
        if let Some(value) = line.strip_prefix("marker-count=") {
            if count_seen
                || value.is_empty()
                || value.len() > 20
                || (value.len() > 1 && value.starts_with('0'))
                || !value.bytes().all(|byte| byte.is_ascii_digit())
                || value
                    .parse::<usize>()
                    .ok()
                    .filter(|value| *value <= CLEANUP_REPORT_MAX_COUNT)
                    .is_none()
            {
                return "fixture-descendant-cleanup-report";
            }
            count_seen = true;
            continue;
        }
        let Some(category) = line.strip_suffix("=true") else {
            return "fixture-descendant-cleanup-report";
        };
        let Some(index) = CLEANUP_REPORT_CATEGORIES
            .iter()
            .position(|known| *known == category)
        else {
            return "fixture-descendant-cleanup-report";
        };
        if previous_category.is_some_and(|previous| previous >= category) {
            return "fixture-descendant-cleanup-report";
        }
        previous_category = Some(category);
        if seen[index] {
            return "fixture-descendant-cleanup-report";
        }
        seen[index] = true;
    }
    if !count_seen || !seen.iter().any(|value| *value) {
        return "fixture-descendant-cleanup-report";
    }
    if seen[0] {
        "fixture-descendant-cleanup-residual"
    } else if seen[1] {
        "fixture-descendant-cleanup-eperm"
    } else if seen[2] {
        "fixture-descendant-cleanup-signal"
    } else if seen[3] || seen[4] || seen[5] {
        "fixture-descendant-cleanup-query"
    } else if seen[6] || seen[7] {
        "fixture-descendant-cleanup-identity"
    } else if seen[8] {
        "fixture-descendant-cleanup-remove"
    } else if seen[9] || seen[10] || seen[11] || seen[12] || seen[14] {
        "fixture-descendant-cleanup-scan"
    } else if seen[13] {
        "fixture-descendant-cleanup-unsafe"
    } else {
        // All accepted category names are classified above; this terminal is
        // defensive if the fixed vocabulary grows without a mapping.
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
        ProcessIdentity, accept_launcher_identity, finish_fixture_failure_with,
        fixture_cleanup_failure_category, fixture_group_failure_reason, persist_fixture_failure,
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
        let valid_cases = [
            (
                "marker-residual=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-residual",
            ),
            (
                "marker-eperm=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-eperm",
            ),
            (
                "marker-signal-failed=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-signal",
            ),
            (
                "marker-process-probe-failed=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-query",
            ),
            (
                "marker-owner-mismatch=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-identity",
            ),
            (
                "marker-identity-lost=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-identity",
            ),
            (
                "marker-remove-failed=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-remove",
            ),
            (
                "marker-entry-invalid=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-scan",
            ),
            (
                "marker-group-unsafe=true\nmarker-count=1\n",
                "fixture-descendant-cleanup-unsafe",
            ),
            (
                "marker-pending=true\nmarker-count=0\n",
                "fixture-descendant-cleanup-scan",
            ),
            (
                "marker-eperm=true\nmarker-residual=true\nmarker-count=2\n",
                "fixture-descendant-cleanup-residual",
            ),
        ];
        let report = root.join("report.log");
        for (contents, expected) in valid_cases {
            fs::write(&report, contents).expect("category report");
            assert_eq!(fixture_cleanup_failure_category(&report), expected);
        }
        let invalid_cases = [
            "unrecognized=true\nmarker-count=1\n",
            "marker-residual=true\npath=/tmp/secret\nmarker-count=1\n",
            "marker-residual=true\nmarker-residual=true\nmarker-count=2\n",
            "marker-residual=true\nmarker-residual=false\nmarker-count=2\n",
            "marker-residual=true\nmarker-eperm=true\nmarker-count=2\n",
            "marker-residual=false\nmarker-count=1\n",
            "marker-residual=true\nmarker-count=1\ntrailing\n",
            "marker-residual=true\n\nmarker-count=1\n",
            "marker-residual=true\n",
            "marker-residual=true\nmarker-count=1\nmarker-count=1\n",
            "marker-residual=true\nscope-count=1\n",
            "marker-count=1\nmarker-residual=true\n",
            "marker-count=1\n",
            "marker-residual=true\nmarker-count=é\n",
            "marker-residual=true\nmarker-count=01\n",
        ];
        for contents in invalid_cases {
            fs::write(&report, contents).expect("invalid category report");
            assert_eq!(
                fixture_cleanup_failure_category(&report),
                "fixture-descendant-cleanup-report"
            );
        }
        let oversized = format!(
            "marker-residual=true\n{}marker-count=1\n",
            "x".repeat(super::CLEANUP_REPORT_MAX_BYTES)
        );
        fs::write(&report, oversized).expect("oversized category report");
        assert_eq!(
            fixture_cleanup_failure_category(&report),
            "fixture-descendant-cleanup-report"
        );
        fs::remove_file(&report).expect("remove report");
        assert_eq!(
            fixture_cleanup_failure_category(&report),
            "fixture-descendant-cleanup-report"
        );
        fs::remove_dir(root).expect("remove category root");
    }

    /// Keep query errors and identity mismatches distinct; neither may be
    /// converted into an absent identity that bypasses group finalization.
    #[test]
    fn launcher_identity_faults_are_not_discarded() {
        let valid = ProcessIdentity {
            pid: 42,
            pgid: 43,
            comm: "sh".to_owned(),
            start_identity: "Mon Jan 1 00:00:00 2026".to_owned(),
        };
        assert_eq!(
            accept_launcher_identity(Err("query-error"), 42, 43),
            Err("fixture-helper-query")
        );
        assert_eq!(
            accept_launcher_identity(
                Ok(ProcessIdentity {
                    pgid: 44,
                    ..valid.clone()
                }),
                42,
                43,
            ),
            Err("fixture-helper-identity")
        );
        assert_eq!(
            accept_launcher_identity(Ok(valid.clone()), 42, 43),
            Ok(valid)
        );
    }

    /// Exercise the real owner-only evidence writer so an identity-query
    /// fault retains the known process group even when identity is absent.
    #[test]
    fn launcher_query_failure_persists_group_evidence() {
        let root = std::env::temp_dir().join(format!(
            "ja-fixture-failure-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir(&root).expect("failure root");
        persist_fixture_failure(&root, 42, 43, None, "fixture-helper-query")
            .expect("failure evidence");
        let contents = fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
            .expect("failure evidence contents");
        assert!(contents.contains("category=fixture-helper-query\n"));
        assert!(contents.contains("launcher-pid=42\n"));
        assert!(contents.contains("process-group=43\n"));
        assert!(contents.contains("identity-state=unavailable\n"));
        fs::remove_dir_all(root).expect("remove failure root");
    }
}
