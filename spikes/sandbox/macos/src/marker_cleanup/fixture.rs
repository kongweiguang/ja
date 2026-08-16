// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native fixture cases that call the production marker cleanup implementation.

use super::fd;
use super::marker::write_fixture_marker;
use super::process::{
    EPERM, ProcessIdentity, abort_unreaped_query, classify_errno, current_pgid, current_uid,
    probe_pid_group_until, query_identity_until, reap_child_bounded, reap_child_group_bounded,
    reap_child_without_group_until, set_nonblocking, terminate_group_with_identity_fallback,
};
use super::{MARKER_MODE, O_CLOEXEC_FLAG, O_NOFOLLOW_FLAG, cleanup_markers_until};
use crate::spawn_grouped;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FIXTURE_CLEANUP_DEADLINE: Duration = Duration::from_secs(8);

/// Run forged, pending, residual-descendant and exact-EPERM cases using the
/// same scan/signal implementation that production workflow cleanup invokes.
pub(super) fn run() -> Result<(), &'static str> {
    // One deadline covers every fixture phase so a slow or wedged earlier case
    // cannot silently grant a fresh cleanup window to a later case.
    let deadline = Instant::now() + FIXTURE_CLEANUP_DEADLINE;
    if classify_errno(EPERM) != super::process::ProcessState::PermissionDenied {
        return Err("fixture-eperm-classification");
    }
    forged_case(deadline)?;
    pending_case(deadline)?;
    residual_group_case(deadline)?;
    descendant_case(deadline)?;
    println!("marker-cleanup-fixtures=pass");
    Ok(())
}

/// Prove a forged owner field is rejected before the production code signals.
fn forged_case(deadline: Instant) -> Result<(), &'static str> {
    let (root, report) = fixture_paths("forged");
    let path = root.join(format!(
        "ja-sandbox-log-helper-{}-11.marker",
        std::process::id()
    ));
    write_fixture_marker(
        &path,
        "owner_pid=999999\nnonce=11\npid=999999\npgid=999999\nstart_identity=forged\nexecutable_kind=log\nstate=active\n",
    )?;
    if cleanup_markers_until(&root, &report, false, deadline).is_ok()
        || !report_contains(&report, "marker-owner-mismatch=true")?
    {
        return Err("fixture-forged-marker");
    }
    remove_fixture_root(root)
}

/// Prove pending activation is reported and removed without any signal path.
fn pending_case(deadline: Instant) -> Result<(), &'static str> {
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
    if cleanup_markers_until(&root, &report, false, deadline).is_ok()
        || !report_contains(&report, "marker-pending=true")?
    {
        return Err("fixture-pending-marker");
    }
    remove_fixture_root(root)
}

/// Prove a marker naming the cleanup process group is retained and reported
/// as residual/unsafe instead of ever signalling the workflow itself.
fn residual_group_case(deadline: Instant) -> Result<(), &'static str> {
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
    if cleanup_markers_until(&root, &report, true, deadline).is_ok()
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

/// Run the child-side fixture supervisor.  The supervisor owns the sleep
/// process and waits for it, so a group kill cannot leave a zombie descendant
/// for the parent fixture to mistake for PID reuse.
#[cfg(target_os = "macos")]
pub(super) fn run_fixture_launcher() -> ! {
    let mut command = Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match spawn_grouped(&mut command) {
        Ok(child) => child,
        Err(_) => std::process::exit(71),
    };
    println!("{}", child.id());
    let _ = std::io::stdout().flush();
    let mut control = std::io::stdin();
    if control_setup_state(set_nonblocking(&control).map_err(|error| error.kind()))
        == FixtureControlEvent::SetupFailed
    {
        terminate_fixture_child(&mut child, "fixture-helper-control");
        abort_unreaped_query("fixture-helper-control");
    }
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // A natural target exit still needs the same group-empty
                // proof as an explicit control request; otherwise an
                // unobserved descendant could survive this helper.
                terminate_fixture_child(&mut child, "fixture-helper-reap");
                break;
            }
            Ok(None) => {}
            Err(_) => {
                terminate_fixture_child(&mut child, "fixture-helper-reap");
                abort_unreaped_query("fixture-helper-reap");
            }
        }
        let mut command_byte = [0_u8; 1];
        let event = match control.read(&mut command_byte) {
            Ok(read) => classify_fixture_control(Ok(read), command_byte[0]),
            Err(error) => classify_fixture_control(Err(error.kind()), command_byte[0]),
        };
        match event {
            FixtureControlEvent::Command => {
                terminate_fixture_child(&mut child, "fixture-helper-control");
                break;
            }
            FixtureControlEvent::Eof => {
                terminate_fixture_child(&mut child, "fixture-helper-control");
                abort_unreaped_query("fixture-helper-control");
            }
            FixtureControlEvent::Invalid | FixtureControlEvent::Error => {
                terminate_fixture_child(&mut child, "fixture-helper-control");
                abort_unreaped_query("fixture-helper-control");
            }
            FixtureControlEvent::WouldBlock => {}
            FixtureControlEvent::SetupFailed => {
                terminate_fixture_child(&mut child, "fixture-helper-control");
                abort_unreaped_query("fixture-helper-control");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::process::exit(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureControlEvent {
    Command,
    Eof,
    WouldBlock,
    Invalid,
    Error,
    SetupFailed,
}

/// Convert the control pipe boundary into a closed vocabulary before the
/// launcher decides whether it may keep waiting; unknown reads fail closed.
fn classify_fixture_control(
    result: Result<usize, std::io::ErrorKind>,
    byte: u8,
) -> FixtureControlEvent {
    match result {
        Ok(1) if byte == b'q' => FixtureControlEvent::Command,
        Ok(0) => FixtureControlEvent::Eof,
        Ok(_) => FixtureControlEvent::Invalid,
        Err(std::io::ErrorKind::WouldBlock) => FixtureControlEvent::WouldBlock,
        Err(_) => FixtureControlEvent::Error,
    }
}

/// Keep nonblocking setup failures in the same testable state machine as
/// control reads; setup failure never silently enters the wait loop.
fn control_setup_state(result: Result<(), std::io::ErrorKind>) -> FixtureControlEvent {
    match result {
        Ok(()) => FixtureControlEvent::WouldBlock,
        Err(_) => FixtureControlEvent::SetupFailed,
    }
}

/// Terminate the helper's own private target group through the same bounded
/// primitive as production cleanup, so the control path cannot strand a child
/// merely because the parent fixture encountered an early parsing fault.
fn terminate_fixture_child(child: &mut std::process::Child, reason: &'static str) {
    let Some(process_group) = i32::try_from(child.id()).ok().filter(|value| *value > 1) else {
        if !reap_child_without_group_until(child, Instant::now() + Duration::from_secs(2)) {
            abort_unreaped_query(reason);
        }
        abort_unreaped_query("fixture-group-unsafe");
    };
    if reap_child_group_bounded(
        child,
        process_group,
        Instant::now() + Duration::from_secs(2),
    )
    .is_err()
    {
        abort_unreaped_query(reason);
    }
}

/// Spawn a real descendant group, write a fixture marker, then let production
/// cleanup signal and verify both direct PID and group reach exact ESRCH.
fn descendant_case(deadline: Instant) -> Result<(), &'static str> {
    let (root, report) = fixture_paths("descendant");
    let helper = std::env::current_exe().map_err(|_| "fixture-helper-spawn")?;
    let mut command = Command::new(helper);
    command
        .arg("--fixture-launcher")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut launcher = spawn_grouped(&mut command).map_err(|_| "fixture-helper-spawn")?;
    // The caller's deadline is shared with the preceding fixture cases; every
    // query, signal, reap and report must consume that same absolute budget.
    let fixture_deadline = deadline;
    let launcher_group = match i32::try_from(launcher.id()) {
        Ok(launcher_group) if launcher_group > 1 => launcher_group,
        _ => {
            let evidence_result = persist_fixture_failure_without_group_until(
                &root,
                launcher.id(),
                "fixture-invalid-process-group",
                fixture_deadline,
            );
            let reaped = reap_child_without_group_until(&mut launcher, fixture_deadline);
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
    let launcher_identity = match accept_launcher_identity(
        query_identity_until(launcher.id(), fixture_deadline),
        launcher.id(),
        launcher_group,
    ) {
        Ok(identity) => Some(identity),
        Err(category) => {
            return finish_supervisor_identity_failure(
                &root,
                &mut launcher,
                launcher_group,
                fixture_deadline,
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
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: None,
                },
                fixture_deadline,
                "fixture-helper-pipe",
            );
        }
    };
    if set_nonblocking(&stdout).is_err() {
        drop(stdout);
        return finish_launcher_failure(
            &root,
            &mut launcher,
            FixtureFailureContext {
                supervisor_group: launcher_group,
                supervisor_identity: launcher_identity.as_ref(),
                target_identity: None,
                target_reference: None,
            },
            fixture_deadline,
            "fixture-helper-pipe",
        );
    }
    let output = match read_launcher_output(&mut stdout, &mut launcher, fixture_deadline) {
        Ok(output) => output,
        Err(error) => {
            drop(stdout);
            return finish_launcher_failure(
                &root,
                &mut launcher,
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: None,
                },
                fixture_deadline,
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
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: None,
                },
                fixture_deadline,
                "fixture-helper-pid",
            );
        }
        Ok(_) => {
            return finish_launcher_failure(
                &root,
                &mut launcher,
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: None,
                },
                fixture_deadline,
                "fixture-helper-pid",
            );
        }
    };
    let identity = match query_identity_until(pid, fixture_deadline) {
        Ok(identity) => identity,
        Err(_) => {
            return finish_launcher_failure(
                &root,
                &mut launcher,
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: provisional_target_reference(pid),
                },
                fixture_deadline,
                "fixture-helper-query",
            );
        }
    };
    let process_group = match i32::try_from(pid) {
        Ok(process_group)
            if process_group > 1
                && process_group != current_pgid()
                && identity.pgid == process_group =>
        {
            process_group
        }
        _ => {
            return finish_launcher_failure(
                &root,
                &mut launcher,
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: provisional_target_reference(pid),
                },
                fixture_deadline,
                "fixture-helper-identity",
            );
        }
    };
    if identity.pid != pid {
        return finish_launcher_failure(
            &root,
            &mut launcher,
            FixtureFailureContext {
                supervisor_group: launcher_group,
                supervisor_identity: launcher_identity.as_ref(),
                target_identity: None,
                target_reference: provisional_target_reference(pid),
            },
            fixture_deadline,
            "fixture-helper-identity",
        );
    }
    let nonce = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => {
            return finish_launcher_failure(
                &root,
                &mut launcher,
                FixtureFailureContext {
                    supervisor_group: launcher_group,
                    supervisor_identity: launcher_identity.as_ref(),
                    target_identity: None,
                    target_reference: None,
                },
                fixture_deadline,
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
            FixtureFailureContext {
                supervisor_group: launcher_group,
                supervisor_identity: launcher_identity.as_ref(),
                target_identity: Some(&identity),
                target_reference: Some((identity.pid, identity.pgid)),
            },
            fixture_deadline,
            "fixture-marker-write",
        );
    }
    let result = cleanup_markers_until(&root, &report, true, fixture_deadline);
    // Keep the launcher Child owned until production cleanup has observed and
    // terminated its real descendant; reaping before this point would make the
    // fixture prove only a helper exit, not the marker cleanup contract.
    // The supervisor owns its own group; the target group was already handled
    // by production marker cleanup.  Reaping with the target PGID would leave
    // the live supervisor outside the identity proof and can strand it when
    // the target exits normally.
    let launcher_result = reap_child_bounded(&mut launcher, launcher_group, fixture_deadline);
    if result.is_err() {
        // Preserve only the production report's fixed category so a native
        // runner can distinguish residual, identity, query and signal faults
        // without exposing a PID, path, locale text or marker contents.
        let category = fixture_cleanup_failure_category_until(&report, fixture_deadline);
        return finish_descendant_failure(
            &root,
            launcher_identity.as_ref(),
            &identity,
            launcher_result,
            fixture_deadline,
            category,
        );
    }
    if let Err(error) = launcher_result {
        return finish_descendant_failure(
            &root,
            launcher_identity.as_ref(),
            &identity,
            Err(error),
            fixture_deadline,
            "fixture-launcher-cleanup",
        );
    }
    // The target marker cleanup and direct supervisor reap are independent
    // facts.  Verify the supervisor's own captured PID/PGID before declaring
    // the fixture successful so a target-only proof cannot hide a live outer
    // helper or an identity transition.
    if let Some(supervisor_identity) = launcher_identity.as_ref() {
        if let Err(error) = verify_reaped_group(supervisor_identity, fixture_deadline) {
            abort_fixture_group(error);
        }
    } else {
        abort_fixture_group("fixture-helper-query");
    }
    remove_fixture_root(root)
}

/// Ask the fixture supervisor to kill and reap its exact sleep child before the
/// parent touches the supervisor Child; a direct kill of the supervisor alone
/// would orphan the target's private process group.
fn request_fixture_launcher_shutdown(
    launcher: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), &'static str> {
    let stdin = launcher.stdin.as_mut().ok_or("fixture-helper-control")?;
    set_nonblocking(stdin).map_err(|_| "fixture-helper-control")?;
    let mut remaining = b"q".as_slice();
    while !remaining.is_empty() && Instant::now() < deadline {
        match stdin.write(remaining) {
            Ok(0) => return Err("fixture-helper-control"),
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err("fixture-helper-control"),
        }
    }
    if remaining.is_empty() {
        Ok(())
    } else {
        Err("fixture-helper-control")
    }
}

/// Wait for an owned supervisor to report its terminal status without sending
/// a signal.  This is the required first phase when target identity is not
/// available: the supervisor's successful exit is the only safe confirmation
/// that its target finalizer completed.
fn wait_child_exit_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
    None
}

/// Recover the only pre-marker branch where the supervisor identity query is
/// unavailable.  The target PID is captured before sending control, then the
/// supervisor is allowed to report its own bounded target reap; killing this
/// outer Child first would orphan a target whose identity is not yet trusted.
fn finish_supervisor_identity_failure(
    root: &Path,
    launcher: &mut std::process::Child,
    supervisor_group: i32,
    deadline: Instant,
    category: &'static str,
) -> Result<(), &'static str> {
    let mut target_reference = None;
    let mut target_identity = None;
    if let Some(mut stdout) = launcher.stdout.take() {
        if set_nonblocking(&stdout).is_ok() {
            if let Ok(output) = read_launcher_output(&mut stdout, launcher, deadline) {
                if let Ok(pid) = output.trim().parse::<u32>() {
                    if let Ok(process_group) = i32::try_from(pid)
                        && pid > 1
                        && process_group > 1
                        && process_group != current_pgid()
                    {
                        target_reference = Some((pid, process_group));
                        target_identity =
                            query_identity_until(pid, deadline).ok().filter(|identity| {
                                identity.pid == pid
                                    && identity.pgid == process_group
                                    && identity.uid == current_uid()
                            });
                    }
                }
            }
        }
    }
    finish_launcher_failure(
        root,
        launcher,
        FixtureFailureContext {
            supervisor_group,
            supervisor_identity: None,
            target_identity: target_identity.as_ref(),
            target_reference,
        },
        deadline,
        category,
    )
}

/// Finish a pre-marker launcher failure while its Child remains owned; an
/// unresolved direct/group cleanup is fatal rather than a recoverable fixture
/// result because no trusted marker exists for an outer cleanup pass.
#[derive(Clone, Copy)]
struct FixtureFailureContext<'a> {
    supervisor_group: i32,
    supervisor_identity: Option<&'a ProcessIdentity>,
    target_identity: Option<&'a ProcessIdentity>,
    target_reference: Option<(u32, i32)>,
}

fn finish_launcher_failure(
    root: &Path,
    launcher: &mut std::process::Child,
    context: FixtureFailureContext<'_>,
    deadline: Instant,
    category: &'static str,
) -> Result<(), &'static str> {
    let evidence_result =
        persist_fixture_failure_pair_until(root, launcher.id(), context, category, deadline);
    let control_result = request_fixture_launcher_shutdown(launcher, deadline);
    let mut supervisor_waited = false;
    let target_result = match context.target_identity {
        Some(identity) if control_result.is_ok() => {
            if context.supervisor_identity.is_none() {
                let status = wait_child_exit_until(launcher, deadline);
                supervisor_waited = status.is_some();
                if status.is_some_and(|status| !status.success()) {
                    terminate_group_with_identity_fallback(identity, &[identity.clone()], deadline)
                } else if status.is_some() {
                    verify_reaped_group(identity, deadline)
                } else {
                    Err("fixture-helper-control")
                }
            } else {
                verify_reaped_group(identity, deadline)
            }
        }
        Some(identity) => {
            terminate_group_with_identity_fallback(identity, &[identity.clone()], deadline)
        }
        None if control_result.is_ok() => {
            let status = wait_child_exit_until(launcher, deadline);
            supervisor_waited = status.is_some();
            if context.target_reference.is_some() && status.is_some_and(|status| status.success()) {
                Ok(())
            } else {
                Err("fixture-helper-control")
            }
        }
        None => Err("fixture-helper-control"),
    };
    let supervisor_result = if target_result.is_ok() {
        match context.supervisor_identity {
            Some(identity) if supervisor_waited => verify_reaped_group(identity, deadline),
            Some(identity) => reap_child_bounded(launcher, context.supervisor_group, deadline)
                .and_then(|()| verify_reaped_group(identity, deadline)),
            None if supervisor_waited => Ok(()),
            None => {
                // A direct kill is permitted only after target cleanup has
                // been independently confirmed; before that point the helper
                // remains owned so it can finish its own target finalizer.
                if reap_child_without_group_until(launcher, deadline) {
                    Ok(())
                } else {
                    Err("fixture-helper-reap")
                }
            }
        }
    } else {
        // No trusted target result means the supervisor must not be killed;
        // preserving the Child and evidence is safer than orphaning target.
        if !supervisor_waited {
            let _ = wait_child_exit_until(launcher, deadline);
        }
        Err("fixture-helper-control")
    };
    match (evidence_result, supervisor_result, target_result) {
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
    supervisor_identity: Option<&ProcessIdentity>,
    target_identity: &ProcessIdentity,
    launcher_result: Result<(), &'static str>,
    deadline: Instant,
    category: &'static str,
) -> Result<(), &'static str> {
    let Some(supervisor_identity) = supervisor_identity else {
        abort_fixture_group("fixture-helper-query");
    };
    if supervisor_identity.pgid == target_identity.pgid
        || supervisor_identity.pid == target_identity.pid
    {
        abort_fixture_group("marker-owner-mismatch");
    }
    let evidence_result = persist_fixture_failure_pair_until(
        root,
        supervisor_identity.pid,
        FixtureFailureContext {
            supervisor_group: supervisor_identity.pgid,
            supervisor_identity: Some(supervisor_identity),
            target_identity: Some(target_identity),
            target_reference: Some((target_identity.pid, target_identity.pgid)),
        },
        category,
        deadline,
    );
    let supervisor_result = verify_reaped_group(supervisor_identity, deadline);
    // `reap_child_bounded` has already proved the direct supervisor is reaped;
    // its target-group empty proof is checked separately below.  Re-signalling
    // the captured descendant after that proof would turn a safe gone state
    // into a PID-reuse/identity error and obscure the original production
    // cleanup category.  Only fall back to identity-checked signals when the
    // target-group reap itself was uncertain.
    let identity_result = if launcher_result.is_ok() {
        verify_reaped_group(target_identity, deadline)
    } else {
        terminate_group_with_identity_fallback(
            target_identity,
            &[target_identity.clone()],
            deadline,
        )
    };
    match (
        evidence_result,
        launcher_result,
        supervisor_result,
        identity_result,
    ) {
        (Ok(()), Ok(()), Ok(()), Ok(())) => Err(category),
        (_, Err(error), _, _) | (_, _, Err(error), _) | (_, _, _, Err(error)) => {
            abort_fixture_group(error)
        }
        (Err(_), Ok(()), Ok(()), Ok(())) => abort_fixture_group("fixture-failure-evidence"),
    }
}

/// Confirm the already-reaped descendant's PID and complete PGID are both
/// kernel-empty without issuing a signal to a possibly reused numeric PID.
fn verify_reaped_group(identity: &ProcessIdentity, deadline: Instant) -> Result<(), &'static str> {
    match probe_pid_group_until(identity.pid, identity.pgid, deadline)? {
        (super::process::ProcessState::Empty, super::process::ProcessState::Empty) => Ok(()),
        (super::process::ProcessState::PermissionDenied, _)
        | (_, super::process::ProcessState::PermissionDenied) => Err("marker-eperm"),
        (super::process::ProcessState::Other(_), _)
        | (_, super::process::ProcessState::Other(_)) => Err("marker-process-probe-failed"),
        _ => Err("marker-residual"),
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
                && identity.pgid > 1
                && identity.pid != std::process::id()
                && identity.uid == current_uid() =>
        {
            Ok(identity)
        }
        Ok(_) => Err("fixture-helper-identity"),
        Err(_) => Err("fixture-helper-query"),
    }
}

/// Retain only a safe provisional target reference after identity query
/// failure.  It is evidence for the supervisor completion protocol, never a
/// permission to signal without the full start/comm/UID identity.
fn provisional_target_reference(pid: u32) -> Option<(u32, i32)> {
    let process_group = i32::try_from(pid).ok()?;
    (pid > 1 && process_group > 1 && process_group != current_pgid())
        .then_some((pid, process_group))
}

const FIXTURE_FAILURE_EVIDENCE: &str = "ja-sandbox-fixture-failure.evidence";
const FIXTURE_FAILURE_PENDING: &str = ".ja-sandbox-fixture-failure.evidence.pending";
const FIXTURE_FAILURE_RECOVERY: &str = ".ja-sandbox-fixture-failure.evidence.recovery";
const FIXTURE_FAILURE_DAMAGED: [&str; 2] = [
    ".ja-sandbox-fixture-failure.evidence.damaged",
    ".ja-sandbox-fixture-failure.evidence.damaged-2",
];
const FIXTURE_FAILURE_EVIDENCE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureEvidenceFault {
    Write,
    FileSync,
    Rename,
    DirectorySync,
}

/// Keep fault injection attached to the real fd-relative transaction.  The
/// production path always uses `None`, while tests fail one concrete syscall
/// phase without replacing the filesystem implementation with a model.
#[derive(Clone, Copy, Debug, Default)]
struct FixtureEvidenceFaultPlan {
    phase: Option<FixtureEvidenceFault>,
    fired: bool,
}

impl FixtureEvidenceFaultPlan {
    fn production() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn once(phase: FixtureEvidenceFault) -> Self {
        Self {
            phase: Some(phase),
            fired: false,
        }
    }

    fn fail_once(&mut self, phase: FixtureEvidenceFault) -> bool {
        if self.fired || self.phase != Some(phase) {
            return false;
        }
        self.fired = true;
        true
    }
}

/// Persist supervisor and target identities as separate records.  The helper
/// is deliberately explicit because a supervisor PID and a target PGID are
/// different cleanup objects; combining them would make recovery vulnerable
/// to signalling the wrong process after an early control failure.
fn persist_fixture_failure_pair_until(
    root: &Path,
    supervisor_pid: u32,
    context: FixtureFailureContext<'_>,
    category: &'static str,
    deadline: Instant,
) -> Result<(), &'static str> {
    if Instant::now() >= deadline
        || supervisor_pid <= 1
        || context.supervisor_group <= 1
        || !is_fixture_failure_category(category)
    {
        return Err("fixture-failure-evidence");
    }
    let supervisor = fixture_identity_evidence(
        "supervisor",
        Some(supervisor_pid),
        Some(context.supervisor_group),
        context.supervisor_identity,
    )?;
    let target = fixture_identity_evidence(
        "target",
        context.target_reference.map(|reference| reference.0),
        context.target_reference.map(|reference| reference.1),
        context.target_identity,
    )?;
    if let Some((target_pid, target_group)) = context.target_reference {
        if supervisor_pid == target_pid
            || context.supervisor_group == target_group
            || i32::try_from(supervisor_pid).ok() == Some(target_group)
            || u32::try_from(context.supervisor_group).ok() == Some(target_pid)
            || context.supervisor_identity.is_some_and(|supervisor| {
                supervisor.uid
                    != context
                        .target_identity
                        .map_or(current_uid(), |target| target.uid)
            })
        {
            return Err("fixture-failure-evidence");
        }
    }
    if let (Some(supervisor), Some(target)) = (context.supervisor_identity, context.target_identity)
        && (supervisor.pid == target.pid
            || supervisor.pgid == target.pgid
            || i32::try_from(supervisor.pid).ok() == Some(target.pgid)
            || u32::try_from(supervisor.pgid).ok() == Some(target.pid)
            || supervisor.uid != target.uid)
    {
        return Err("fixture-failure-evidence");
    }
    let contents = format!("fixture-failure-version=2\ncategory={category}\n{supervisor}{target}");
    if contents.len() > FIXTURE_FAILURE_EVIDENCE_BYTES || !contents.is_ascii() {
        return Err("fixture-failure-evidence");
    }
    write_fixture_failure_evidence_until(root, &contents, deadline)
}

/// Render one identity with a fixed prefix and retain provisional PID/PGID
/// values when the OS query failed; unknown start/comm never becomes a signal
/// authorization, but remains auditable for the next bounded cleanup pass.
fn fixture_identity_evidence(
    prefix: &str,
    expected_pid: Option<u32>,
    expected_group: Option<i32>,
    identity: Option<&ProcessIdentity>,
) -> Result<String, &'static str> {
    let (state, pid, group, uid, comm, start) = match identity {
        Some(identity)
            if identity.pid > 1
                && identity.pgid > 1
                && identity.pid == expected_pid.unwrap_or(identity.pid)
                && identity.pgid == expected_group.unwrap_or(identity.pgid)
                && identity.pid != std::process::id()
                && identity.pgid != current_pgid()
                && identity.uid == current_uid() =>
        {
            (
                "known",
                identity.pid.to_string(),
                identity.pgid.to_string(),
                identity.uid.to_string(),
                evidence_value(&identity.comm),
                evidence_value(&identity.start_identity),
            )
        }
        Some(_) => return Err("fixture-failure-evidence"),
        None if expected_pid.is_some_and(|pid| pid <= 1 || pid == std::process::id())
            || expected_group.is_some_and(|group| group <= 1 || group == current_pgid()) =>
        {
            return Err("fixture-failure-evidence");
        }
        None => (
            if expected_pid.is_some() {
                "provisional"
            } else {
                "unavailable"
            },
            expected_pid
                .filter(|value| *value > 1)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            expected_group
                .filter(|value| *value > 1)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            "unknown".to_owned(),
            "redacted".to_owned(),
            "redacted".to_owned(),
        ),
    };
    Ok(format!(
        "{prefix}-state={state}\n{prefix}-pid={pid}\n{prefix}-pgid={group}\n{prefix}-uid={uid}\n{prefix}-comm={comm}\n{prefix}-start={start}\n"
    ))
}

/// Preserve the no-group branch's bounded evidence semantics without
/// inventing an unsafe process-group identifier for the recovery path.
fn persist_fixture_failure_without_group_until(
    root: &Path,
    launcher_pid: u32,
    category: &'static str,
    deadline: Instant,
) -> Result<(), &'static str> {
    if Instant::now() >= deadline {
        return Err("fixture-failure-evidence");
    }
    if launcher_pid <= 1 || !is_fixture_failure_category(category) {
        return Err("fixture-failure-evidence");
    }
    let contents = format!(
        "fixture-failure-version=2\ncategory={category}\nsupervisor-state=unavailable\nsupervisor-pid={launcher_pid}\nsupervisor-pgid=unknown\nsupervisor-uid=unknown\nsupervisor-comm=redacted\nsupervisor-start=redacted\ntarget-state=unavailable\ntarget-pid=unknown\ntarget-pgid=unknown\ntarget-uid=unknown\ntarget-comm=redacted\ntarget-start=redacted\n"
    );
    write_fixture_failure_evidence_until(root, &contents, deadline)
}

/// Write owner-only evidence through a descriptor-relative pending transaction;
/// no deadline or fault path removes a prior good final/recovery copy.
fn write_fixture_failure_evidence_until(
    root: &Path,
    contents: &str,
    deadline: Instant,
) -> Result<(), &'static str> {
    let mut faults = FixtureEvidenceFaultPlan::production();
    write_fixture_failure_evidence_transaction(root, contents, deadline, &mut faults)
}

/// Exercise the production transaction with one real filesystem phase fault;
/// tests inspect the resulting pending/recovery files rather than a model.
#[cfg(test)]
fn write_fixture_failure_evidence_with_fault(
    root: &Path,
    contents: &str,
    fault: FixtureEvidenceFault,
    deadline: Instant,
) -> Result<(), &'static str> {
    let mut faults = FixtureEvidenceFaultPlan::once(fault);
    write_fixture_failure_evidence_transaction(root, contents, deadline, &mut faults)
}

/// Publish fixture evidence atomically inside the verified root namespace.
/// At least one complete copy remains on every recoverable failure path; the
/// final name is never opened with truncation or replaced by this transaction.
fn write_fixture_failure_evidence_transaction(
    root: &Path,
    contents: &str,
    deadline: Instant,
    faults: &mut FixtureEvidenceFaultPlan,
) -> Result<(), &'static str> {
    if Instant::now() >= deadline {
        return Err("fixture-failure-evidence");
    }
    if contents.len() > FIXTURE_FAILURE_EVIDENCE_BYTES
        || !contents.is_ascii()
        || validate_fixture_failure_evidence(contents.as_bytes()).is_err()
    {
        return Err("fixture-failure-evidence");
    }
    let root_directory = open_fixture_evidence_root(root)?;
    let root_fd = root_directory.as_raw_fd();
    if recover_existing_fixture_evidence(&root_directory, root_fd, deadline, faults)? {
        return Ok(());
    }
    let recovery_result = write_fixture_evidence_copy(
        root_fd,
        FIXTURE_FAILURE_RECOVERY.as_bytes(),
        contents,
        deadline,
        faults,
    );
    let pending_result = write_fixture_evidence_copy(
        root_fd,
        FIXTURE_FAILURE_PENDING.as_bytes(),
        contents,
        deadline,
        faults,
    );
    if recovery_result.is_err() || pending_result.is_err() {
        return Err("fixture-failure-evidence");
    }
    if Instant::now() >= deadline {
        return Err("fixture-failure-evidence");
    }
    fd::sync_directory(&root_directory).map_err(|_| "fixture-failure-evidence")?;
    if Instant::now() >= deadline || faults.fail_once(FixtureEvidenceFault::Rename) {
        return Err("fixture-failure-evidence");
    }
    match fixture_evidence_entry_exists(root_fd, FIXTURE_FAILURE_EVIDENCE.as_bytes()) {
        Ok(false) => {}
        Ok(true) | Err(_) => return Err("fixture-failure-evidence"),
    }
    fd::rename_at_no_replace(
        root_fd,
        FIXTURE_FAILURE_PENDING.as_bytes(),
        FIXTURE_FAILURE_EVIDENCE.as_bytes(),
    )
    .map_err(|_| "fixture-failure-evidence")?;
    if Instant::now() >= deadline || faults.fail_once(FixtureEvidenceFault::DirectorySync) {
        return Err("fixture-failure-evidence");
    }
    fd::sync_directory(&root_directory).map_err(|_| "fixture-failure-evidence")?;
    match read_fixture_evidence_copy(
        root_fd,
        FIXTURE_FAILURE_EVIDENCE.as_bytes(),
        Some(contents),
        deadline,
    )? {
        Some(true) => Ok(()),
        Some(false) | None => Err("fixture-failure-evidence"),
    }
}

/// Open the fixture root once with no-follow and verify its owner-private mode
/// before any pending/final evidence operation uses the descriptor.
fn open_fixture_evidence_root(root: &Path) -> Result<File, &'static str> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(root)
        .map_err(|_| "fixture-failure-evidence")?;
    let metadata = directory
        .metadata()
        .map_err(|_| "fixture-failure-evidence")?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_uid()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err("fixture-failure-evidence");
    }
    Ok(directory)
}

/// Return whether a relative entry exists, retaining non-ENOENT failures as
/// hard evidence errors rather than treating them as a clean restart state.
fn fixture_evidence_entry_exists(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
) -> Result<bool, &'static str> {
    match fd::fstatat_no_follow(root_fd, name) {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(fd::ENOENT) => Ok(false),
        Err(_) => Err("fixture-failure-evidence"),
    }
}

const FIXTURE_EVIDENCE_FIELD_NAMES: [&str; 14] = [
    "fixture-failure-version",
    "category",
    "supervisor-state",
    "supervisor-pid",
    "supervisor-pgid",
    "supervisor-uid",
    "supervisor-comm",
    "supervisor-start",
    "target-state",
    "target-pid",
    "target-pgid",
    "target-uid",
    "target-comm",
    "target-start",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureEvidenceState {
    Known,
    Provisional,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ParsedFixtureEvidenceIdentity {
    pid: Option<u32>,
    pgid: Option<i32>,
}

/// Parse the complete version-2 evidence grammar before a candidate can be
/// promoted.  Fixed ordering and exact field count reject duplicate, unknown,
/// truncated, or trailing records instead of treating a readable prefix as a
/// durable identity witness.
fn validate_fixture_failure_evidence(contents: &[u8]) -> Result<(), &'static str> {
    if contents.is_empty()
        || contents.len() > FIXTURE_FAILURE_EVIDENCE_BYTES
        || contents.last() != Some(&b'\n')
        || !contents.is_ascii()
    {
        return Err("fixture-failure-evidence");
    }
    let text = std::str::from_utf8(contents).map_err(|_| "fixture-failure-evidence")?;
    let lines = text.split_terminator('\n').collect::<Vec<_>>();
    if lines.len() != FIXTURE_EVIDENCE_FIELD_NAMES.len() {
        return Err("fixture-failure-evidence");
    }
    let mut values = [""; FIXTURE_EVIDENCE_FIELD_NAMES.len()];
    for (index, (line, field)) in lines.iter().zip(FIXTURE_EVIDENCE_FIELD_NAMES).enumerate() {
        let prefix = format!("{field}=");
        let value = line
            .strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .ok_or("fixture-failure-evidence")?;
        values[index] = value;
    }
    if values[0] != "2" || !is_fixture_failure_category(values[1]) {
        return Err("fixture-failure-evidence");
    }
    let supervisor = parse_fixture_evidence_identity(&values[2..8])?;
    let target = parse_fixture_evidence_identity(&values[8..14])?;
    if supervisor.pid.is_some_and(|pid| Some(pid) == target.pid)
        || supervisor
            .pgid
            .is_some_and(|pgid| Some(pgid) == target.pgid)
        || supervisor.pid.is_some_and(|pid| {
            i32::try_from(pid)
                .ok()
                .is_some_and(|pid| Some(pid) == target.pgid)
        })
        || supervisor.pgid.is_some_and(|pgid| {
            u32::try_from(pgid)
                .ok()
                .is_some_and(|pgid| Some(pgid) == target.pid)
        })
    {
        return Err("fixture-failure-evidence");
    }
    Ok(())
}

/// Validate one supervisor/target identity record, including state-specific
/// fields. A known record keeps the trusted numeric identity and current UID;
/// its command/start values may be redacted because path-bearing `ps` output
/// must not enter evidence, while provisional/unavailable records must redact
/// every field that was not independently captured before the fault.
fn parse_fixture_evidence_identity(
    fields: &[&str],
) -> Result<ParsedFixtureEvidenceIdentity, &'static str> {
    if fields.len() != 6 {
        return Err("fixture-failure-evidence");
    }
    let state = match fields[0] {
        "known" => FixtureEvidenceState::Known,
        "provisional" => FixtureEvidenceState::Provisional,
        "unavailable" => FixtureEvidenceState::Unavailable,
        _ => return Err("fixture-failure-evidence"),
    };
    let pid = parse_fixture_evidence_pid(fields[1])?;
    let pgid = parse_fixture_evidence_pgid(fields[2])?;
    let uid = parse_fixture_evidence_uid(fields[3])?;
    let comm = fields[4];
    let start = fields[5];
    if !is_fixture_evidence_atom(comm) || !is_fixture_evidence_atom(start) {
        return Err("fixture-failure-evidence");
    }
    match state {
        FixtureEvidenceState::Known => {
            if pid.is_none()
                || pgid.is_none()
                || uid != Some(current_uid())
                || pid.is_some_and(|pid| pid <= 1 || pid == std::process::id())
                || pgid.is_some_and(|pgid| pgid <= 1 || pgid == current_pgid())
            {
                return Err("fixture-failure-evidence");
            }
        }
        FixtureEvidenceState::Provisional => {
            if pid.is_none()
                || pgid.is_none()
                || uid.is_some()
                || comm != "redacted"
                || start != "redacted"
                || pid.is_some_and(|pid| pid <= 1 || pid == std::process::id())
                || pgid.is_some_and(|pgid| pgid <= 1 || pgid == current_pgid())
            {
                return Err("fixture-failure-evidence");
            }
        }
        FixtureEvidenceState::Unavailable => {
            if pgid.is_some()
                || uid.is_some()
                || comm != "redacted"
                || start != "redacted"
                || pid.is_some_and(|pid| pid <= 1 || pid == std::process::id())
            {
                return Err("fixture-failure-evidence");
            }
        }
    }
    Ok(ParsedFixtureEvidenceIdentity { pid, pgid })
}

/// Parse an optional unsigned PID without accepting signs, leading zeros,
/// overflow, or the reserved values that could later become signal targets.
fn parse_fixture_evidence_pid(value: &str) -> Result<Option<u32>, &'static str> {
    parse_fixture_evidence_unsigned(value)?
        .map(|value| {
            u32::try_from(value)
                .map_err(|_| "fixture-failure-evidence")
                .and_then(|value| {
                    (value <= i32::MAX as u32)
                        .then_some(value)
                        .ok_or("fixture-failure-evidence")
                })
        })
        .transpose()
}

/// Parse an optional positive process group ID within Darwin's signed range.
fn parse_fixture_evidence_pgid(value: &str) -> Result<Option<i32>, &'static str> {
    let value = parse_fixture_evidence_unsigned(value)?;
    value
        .map(|value| {
            i32::try_from(value)
                .map_err(|_| "fixture-failure-evidence")
                .and_then(|value| {
                    (value > 0)
                        .then_some(value)
                        .ok_or("fixture-failure-evidence")
                })
        })
        .transpose()
}

/// Parse an optional UID while retaining `unknown` as the only nonnumeric
/// state marker; UID zero is valid for a root-owned fixture but must match the
/// current process when the identity is known.
fn parse_fixture_evidence_uid(value: &str) -> Result<Option<u32>, &'static str> {
    parse_fixture_evidence_unsigned(value)?
        .map(|value| u32::try_from(value).map_err(|_| "fixture-failure-evidence"))
        .transpose()
}

/// Parse a bounded decimal field or the exact `unknown` sentinel.
fn parse_fixture_evidence_unsigned(value: &str) -> Result<Option<u64>, &'static str> {
    if value == "unknown" {
        return Ok(None);
    }
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("fixture-failure-evidence");
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "fixture-failure-evidence")
}

/// Restrict command/start fields to a short, line-safe, path-free atom so
/// evidence parsing cannot smuggle arbitrary logs or control characters.
fn is_fixture_evidence_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
        && !value.contains(['=', '\n', '\r', '/', '\\'])
}

/// Read and validate one bounded evidence copy through `openat`; `expected`
/// is used for the just-published final, while restart recovery accepts only a
/// complete, state-consistent version-2 record without rewriting it.
fn read_fixture_evidence_copy(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
    expected: Option<&str>,
    deadline: Instant,
) -> Result<Option<bool>, &'static str> {
    if Instant::now() >= deadline {
        return Err("fixture-failure-evidence");
    }
    if !fixture_evidence_entry_exists(root_fd, name)? {
        return Ok(None);
    }
    fd::fstatat_no_follow(root_fd, name).map_err(|_| "fixture-failure-evidence")?;
    let file = fd::open_at_file(root_fd, name).map_err(|_| "fixture-failure-evidence")?;
    let metadata = file.metadata().map_err(|_| "fixture-failure-evidence")?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != MARKER_MODE
    {
        return Err("fixture-failure-evidence");
    }
    let mut bytes = Vec::new();
    file.take((FIXTURE_FAILURE_EVIDENCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "fixture-failure-evidence")?;
    if bytes.is_empty() || bytes.len() > FIXTURE_FAILURE_EVIDENCE_BYTES || !bytes.is_ascii() {
        return Err("fixture-failure-evidence");
    }
    let valid = validate_fixture_failure_evidence(&bytes).is_ok();
    if let Some(expected) = expected {
        if !valid || bytes != expected.as_bytes() {
            return Err("fixture-failure-evidence");
        }
    }
    fd::fstatat_no_follow(root_fd, name).map_err(|_| "fixture-failure-evidence")?;
    Ok(Some(valid))
}

/// Recover an already durable final/pending/recovery copy after a crash;
/// existing valid evidence wins over a new write and invalid evidence is
/// retained for fail-closed inspection rather than overwritten.
fn recover_existing_fixture_evidence(
    root_directory: &File,
    root_fd: std::os::fd::RawFd,
    deadline: Instant,
    faults: &mut FixtureEvidenceFaultPlan,
) -> Result<bool, &'static str> {
    let mut invalid_final = false;
    match read_fixture_evidence_copy(root_fd, FIXTURE_FAILURE_EVIDENCE.as_bytes(), None, deadline)?
    {
        Some(true) => return Ok(true),
        Some(false) => {
            quarantine_invalid_fixture_evidence(root_directory, root_fd, deadline, faults)?;
            invalid_final = true;
        }
        None => {}
    }
    for candidate in [
        FIXTURE_FAILURE_PENDING.as_bytes(),
        FIXTURE_FAILURE_RECOVERY.as_bytes(),
    ] {
        if read_fixture_evidence_copy(root_fd, candidate, None, deadline)? == Some(true) {
            if Instant::now() >= deadline || faults.fail_once(FixtureEvidenceFault::Rename) {
                return Err("fixture-failure-evidence");
            }
            if fixture_evidence_entry_exists(root_fd, FIXTURE_FAILURE_EVIDENCE.as_bytes())? {
                return Err("fixture-failure-evidence");
            }
            fd::rename_at_no_replace(root_fd, candidate, FIXTURE_FAILURE_EVIDENCE.as_bytes())
                .map_err(|_| "fixture-failure-evidence")?;
            if Instant::now() >= deadline || faults.fail_once(FixtureEvidenceFault::DirectorySync) {
                return Err("fixture-failure-evidence");
            }
            fd::sync_directory(root_directory).map_err(|_| "fixture-failure-evidence")?;
            return Ok(true);
        }
    }
    if invalid_final {
        Err("fixture-failure-evidence")
    } else {
        Ok(false)
    }
}

/// Move malformed final evidence into one of two fixed damaged names without
/// replacing either name.  The source is rechecked through the root fd before
/// rename, and no candidate is deleted if both bounded destinations are busy.
fn quarantine_invalid_fixture_evidence(
    root_directory: &File,
    root_fd: std::os::fd::RawFd,
    deadline: Instant,
    faults: &mut FixtureEvidenceFaultPlan,
) -> Result<(), &'static str> {
    validate_fixture_evidence_entry(root_fd, FIXTURE_FAILURE_EVIDENCE.as_bytes())?;
    for damaged in FIXTURE_FAILURE_DAMAGED {
        if Instant::now() >= deadline || faults.fail_once(FixtureEvidenceFault::Rename) {
            return Err("fixture-failure-evidence");
        }
        match fd::rename_at_no_replace(
            root_fd,
            FIXTURE_FAILURE_EVIDENCE.as_bytes(),
            damaged.as_bytes(),
        ) {
            Ok(()) => {
                if Instant::now() >= deadline
                    || faults.fail_once(FixtureEvidenceFault::DirectorySync)
                {
                    return Err("fixture-failure-evidence");
                }
                fd::sync_directory(root_directory).map_err(|_| "fixture-failure-evidence")?;
                return Ok(());
            }
            Err(error) if error.raw_os_error() == Some(fd::EEXIST) => continue,
            Err(_) => return Err("fixture-failure-evidence"),
        }
    }
    Err("fixture-failure-evidence")
}

/// Recheck the malformed source using the same owner-only regular-file rules
/// as normal evidence reads; a symlink, hardlink, other UID, or mode change is
/// never authorized for the damaged quarantine rename.
fn validate_fixture_evidence_entry(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
) -> Result<(), &'static str> {
    fd::fstatat_no_follow(root_fd, name).map_err(|_| "fixture-failure-evidence")?;
    let file = fd::open_at_file(root_fd, name).map_err(|_| "fixture-failure-evidence")?;
    let metadata = file.metadata().map_err(|_| "fixture-failure-evidence")?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != MARKER_MODE
    {
        return Err("fixture-failure-evidence");
    }
    fd::fstatat_no_follow(root_fd, name).map_err(|_| "fixture-failure-evidence")
}

/// Create and durably validate one complete sibling copy.  A simulated write
/// fault leaves the first copy partial but still permits the second copy to
/// become the recoverable full image.
fn write_fixture_evidence_copy(
    root_fd: std::os::fd::RawFd,
    name: &[u8],
    contents: &str,
    deadline: Instant,
    faults: &mut FixtureEvidenceFaultPlan,
) -> Result<(), &'static str> {
    if Instant::now() >= deadline {
        return Err("fixture-failure-evidence");
    }
    let mut file =
        fd::create_at_file(root_fd, name, MARKER_MODE).map_err(|_| "fixture-failure-evidence")?;
    let metadata = file.metadata().map_err(|_| "fixture-failure-evidence")?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o777 != MARKER_MODE
    {
        return Err("fixture-failure-evidence");
    }
    if faults.fail_once(FixtureEvidenceFault::Write) {
        let partial = contents.len().max(1) / 2;
        let _ = file.write_all(&contents.as_bytes()[..partial]);
        return Err("fixture-failure-evidence");
    }
    file.write_all(contents.as_bytes())
        .map_err(|_| "fixture-failure-evidence")?;
    if faults.fail_once(FixtureEvidenceFault::FileSync) {
        return Err("fixture-failure-evidence");
    }
    file.sync_all().map_err(|_| "fixture-failure-evidence")?;
    if read_fixture_evidence_copy(root_fd, name, Some(contents), deadline)? != Some(true) {
        return Err("fixture-failure-evidence");
    }
    Ok(())
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
            | "fixture-helper-control"
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
            | "fixture-group-identity"
            | "fixture-group-query"
            | "fixture-group-reap"
            | "fixture-group-remove"
            | "fixture-group-evidence"
            | "fixture-group-control"
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
fn fixture_cleanup_failure_category_until(report: &Path, deadline: Instant) -> &'static str {
    if Instant::now() >= deadline {
        return "fixture-descendant-cleanup-report";
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(report)
    {
        Ok(file) => file,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    if Instant::now() >= deadline {
        return "fixture-descendant-cleanup-report";
    }
    let mut contents = Vec::new();
    let bytes = match file
        .take((CLEANUP_REPORT_MAX_BYTES + 1) as u64)
        .read_to_end(&mut contents)
    {
        Ok(bytes) => bytes,
        Err(_) => return "fixture-descendant-cleanup-report",
    };
    if Instant::now() >= deadline {
        return "fixture-descendant-cleanup-report";
    }
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

/// Keep parser unit tests concise while production passes its existing shared
/// fixture deadline through report classification.
#[cfg(test)]
fn fixture_cleanup_failure_category(report: &Path) -> &'static str {
    fixture_cleanup_failure_category_until(report, Instant::now() + Duration::from_secs(2))
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
        "marker-identity-lost" | "marker-owner-mismatch" => "fixture-group-identity",
        "marker-query-unreaped" | "marker-query-group-residual" => "fixture-group-query",
        "fixture-unreaped" => "fixture-group-reap",
        "marker-remove-failed" => "fixture-group-remove",
        "fixture-failure-evidence" => "fixture-group-evidence",
        "fixture-helper-control" => "fixture-group-control",
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
        FixtureControlEvent, FixtureEvidenceFault, FixtureFailureContext, ProcessIdentity,
        accept_launcher_identity, classify_fixture_control, control_setup_state,
        finish_fixture_failure_with, fixture_cleanup_failure_category,
        fixture_group_failure_reason, fixture_paths, persist_fixture_failure_pair_until,
        wait_child_exit_until, write_fixture_failure_evidence_until,
        write_fixture_failure_evidence_with_fault,
    };
    use crate::marker_cleanup::process::{ProcessState, group_state};
    use crate::spawn_grouped;
    use std::fs;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    /// Exercise the real control-pipe seam for EOF, non-command bytes, read
    /// errors and nonblocking setup failure; none may become a success event.
    #[test]
    fn fixture_control_faults_are_fail_closed() {
        let cases = [
            (Ok(1), b'q', FixtureControlEvent::Command),
            (Ok(1), b'x', FixtureControlEvent::Invalid),
            (Ok(0), b'q', FixtureControlEvent::Eof),
            (Ok(2), b'q', FixtureControlEvent::Invalid),
            (
                Err(std::io::ErrorKind::WouldBlock),
                b'q',
                FixtureControlEvent::WouldBlock,
            ),
            (
                Err(std::io::ErrorKind::BrokenPipe),
                b'q',
                FixtureControlEvent::Error,
            ),
        ];
        for (result, byte, expected) in cases {
            assert_eq!(classify_fixture_control(result, byte), expected);
        }
        assert_eq!(
            control_setup_state(Err(std::io::ErrorKind::Other)),
            FixtureControlEvent::SetupFailed
        );
        assert_eq!(control_setup_state(Ok(())), FixtureControlEvent::WouldBlock);
    }

    /// Exercise the bounded supervisor-status seam with both normal and crash
    /// exits; the parent observes the result before any optional kill path.
    #[test]
    fn supervisor_status_faults_are_observed_bounded() {
        for command_line in ["exit 0", "exit 71"] {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", command_line])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = spawn_grouped(&mut command).expect("spawn supervisor seam");
            let process_group = i32::try_from(child.id())
                .ok()
                .filter(|value| *value > 1)
                .expect("supervisor group");
            let status = wait_child_exit_until(&mut child, Instant::now() + Duration::from_secs(2))
                .expect("bounded supervisor status");
            assert_eq!(status.success(), command_line == "exit 0");
            assert_eq!(group_state(process_group), ProcessState::Empty);
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
            ("marker-identity-lost", "fixture-group-identity"),
            ("marker-query-unreaped", "fixture-group-query"),
            ("fixture-unreaped", "fixture-group-reap"),
            ("marker-remove-failed", "fixture-group-remove"),
            ("fixture-failure-evidence", "fixture-group-evidence"),
            ("fixture-helper-control", "fixture-group-control"),
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
            uid: super::current_uid(),
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
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("failure root");
        persist_fixture_failure_pair_until(
            &root,
            42,
            FixtureFailureContext {
                supervisor_group: 43,
                supervisor_identity: None,
                target_identity: None,
                target_reference: Some((44, 45)),
            },
            "fixture-helper-query",
            Instant::now() + Duration::from_secs(2),
        )
        .expect("failure evidence");
        let contents = fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
            .expect("failure evidence contents");
        assert!(contents.contains("category=fixture-helper-query\n"));
        assert!(contents.contains("supervisor-pid=42\n"));
        assert!(contents.contains("supervisor-pgid=43\n"));
        assert!(contents.contains("supervisor-state=unavailable\n"));
        assert!(contents.contains("target-state=provisional\n"));
        assert!(contents.contains("target-pid=44\n"));
        assert!(contents.contains("target-pgid=45\n"));
        fs::remove_dir_all(root).expect("remove failure root");
    }

    /// Write one malformed candidate through the same root-fd helper used by
    /// production recovery; tests must not bypass no-follow and mode checks.
    fn write_evidence_candidate(root: &Path, name: &str, contents: &str) {
        let directory = super::open_fixture_evidence_root(root).expect("evidence root");
        let mut file =
            super::fd::create_at_file(directory.as_raw_fd(), name.as_bytes(), super::MARKER_MODE)
                .expect("evidence candidate");
        file.write_all(contents.as_bytes())
            .expect("candidate write");
        file.sync_all().expect("candidate sync");
        super::fd::sync_directory(&directory).expect("candidate directory sync");
    }

    /// Require every version-2 field and identity state before recovery can
    /// promote a candidate; an invalid sibling remains visible for diagnosis.
    #[test]
    fn fixture_evidence_recovery_requires_complete_identity_grammar() {
        let good = "fixture-failure-version=2\ncategory=fixture-helper-query\nsupervisor-state=provisional\nsupervisor-pid=42\nsupervisor-pgid=43\nsupervisor-uid=unknown\nsupervisor-comm=redacted\nsupervisor-start=redacted\ntarget-state=provisional\ntarget-pid=44\ntarget-pgid=45\ntarget-uid=unknown\ntarget-comm=redacted\ntarget-start=redacted\n";
        let invalid_cases = [
            "fixture-failure-version=2\ncategory=fixture-helper-query\n".to_owned(),
            good.replace(
                "category=fixture-helper-query\n",
                "category=fixture-helper-query\ncategory=fixture-helper-query\n",
            ),
            good.replace("target-start=redacted\n", ""),
            good.trim_end_matches('\n').to_owned(),
            good.replace("target-state=provisional\n", "target-unknown=x\n"),
            good.replace("target-pid=44\n", "target-pid=1\n"),
            good.replace("target-pid=44\n", "target-pid=unknown\n"),
            good.replace("target-pgid=45\n", "target-pgid=0\n"),
            good.replace("target-pgid=45\n", "target-pgid=42\n"),
            good.replace("target-pid=44\n", "target-pid=43\n"),
        ];
        for (index, invalid) in invalid_cases.iter().enumerate() {
            let (root, _) = fixture_paths(&format!("evidence-invalid-{index}"));
            write_evidence_candidate(&root, super::FIXTURE_FAILURE_RECOVERY, invalid);
            let result = write_fixture_failure_evidence_until(
                &root,
                good,
                Instant::now() + Duration::from_secs(2),
            );
            assert_eq!(result, Err("fixture-failure-evidence"));
            assert!(!root.join(super::FIXTURE_FAILURE_EVIDENCE).exists());
            assert_eq!(
                fs::read_to_string(root.join(super::FIXTURE_FAILURE_RECOVERY))
                    .expect("invalid evidence retained"),
                *invalid
            );
            fs::remove_dir_all(root).expect("invalid evidence root cleanup");
        }

        let (root, _) = fixture_paths("evidence-invalid-final");
        write_evidence_candidate(
            &root,
            super::FIXTURE_FAILURE_EVIDENCE,
            invalid_cases[0].as_str(),
        );
        let result = write_fixture_failure_evidence_until(
            &root,
            good,
            Instant::now() + Duration::from_secs(2),
        );
        assert_eq!(result, Err("fixture-failure-evidence"));
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
                .expect("invalid final retained"),
            invalid_cases[0]
        );
        fs::remove_dir_all(root).expect("invalid final root cleanup");
    }

    /// Select a complete recovery sibling when the pending copy is short, and
    /// promote a complete recovery by itself; the invalid pending stays for a
    /// later diagnostic pass instead of being mistaken for valid evidence.
    #[test]
    fn fixture_evidence_recovery_prefers_complete_sibling() {
        let short = "fixture-failure-version=2\ncategory=fixture-helper-query\n";
        let (root, _) = fixture_paths("evidence-short-pending");
        write_evidence_candidate(&root, super::FIXTURE_FAILURE_PENDING, short);
        write_evidence_candidate(
            &root,
            super::FIXTURE_FAILURE_RECOVERY,
            GOOD_FIXTURE_EVIDENCE,
        );
        write_fixture_failure_evidence_until(
            &root,
            GOOD_FIXTURE_EVIDENCE,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("complete recovery sibling");
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE)).expect("promoted final"),
            GOOD_FIXTURE_EVIDENCE
        );
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_PENDING))
                .expect("short pending retained"),
            short
        );
        fs::remove_dir_all(root).expect("sibling root cleanup");

        let (root, _) = fixture_paths("evidence-complete-recovery");
        write_evidence_candidate(
            &root,
            super::FIXTURE_FAILURE_RECOVERY,
            GOOD_FIXTURE_EVIDENCE,
        );
        write_fixture_failure_evidence_until(
            &root,
            GOOD_FIXTURE_EVIDENCE,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("complete recovery promotion");
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE)).expect("promoted final"),
            GOOD_FIXTURE_EVIDENCE
        );
        fs::remove_dir_all(root).expect("complete recovery root cleanup");
    }

    /// Quarantine an invalid ordinary final before choosing a complete sibling;
    /// both fixed damaged names are preserved if the first destination exists.
    #[test]
    fn invalid_final_is_quarantined_before_valid_sibling_promotion() {
        let invalid = "fixture-failure-version=2\ncategory=fixture-helper-query\n";
        for sibling in [
            super::FIXTURE_FAILURE_PENDING,
            super::FIXTURE_FAILURE_RECOVERY,
        ] {
            let (root, _) = fixture_paths("evidence-invalid-final-sibling");
            write_evidence_candidate(&root, super::FIXTURE_FAILURE_EVIDENCE, invalid);
            write_evidence_candidate(&root, sibling, GOOD_FIXTURE_EVIDENCE);
            write_fixture_failure_evidence_until(
                &root,
                GOOD_FIXTURE_EVIDENCE,
                Instant::now() + Duration::from_secs(2),
            )
            .expect("valid sibling promotion");
            assert_eq!(
                fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
                    .expect("promoted final"),
                GOOD_FIXTURE_EVIDENCE
            );
            assert_eq!(
                fs::read_to_string(root.join(super::FIXTURE_FAILURE_DAMAGED[0]))
                    .expect("damaged final"),
                invalid
            );
            fs::remove_dir_all(root).expect("sibling promotion cleanup");
        }

        let (root, _) = fixture_paths("evidence-invalid-final-damaged");
        write_evidence_candidate(&root, super::FIXTURE_FAILURE_EVIDENCE, invalid);
        write_evidence_candidate(&root, super::FIXTURE_FAILURE_DAMAGED[0], "old damaged\n");
        write_evidence_candidate(
            &root,
            super::FIXTURE_FAILURE_RECOVERY,
            GOOD_FIXTURE_EVIDENCE,
        );
        write_fixture_failure_evidence_until(
            &root,
            GOOD_FIXTURE_EVIDENCE,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("bounded second damaged destination");
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_DAMAGED[0]))
                .expect("first damaged"),
            "old damaged\n"
        );
        assert_eq!(
            fs::read_to_string(root.join(super::FIXTURE_FAILURE_DAMAGED[1]))
                .expect("second damaged"),
            invalid
        );
        fs::remove_dir_all(root).expect("damaged destination cleanup");

        let (root, _) = fixture_paths("evidence-invalid-final-no-sibling");
        write_evidence_candidate(&root, super::FIXTURE_FAILURE_EVIDENCE, invalid);
        assert_eq!(
            write_fixture_failure_evidence_until(
                &root,
                GOOD_FIXTURE_EVIDENCE,
                Instant::now() + Duration::from_secs(2),
            ),
            Err("fixture-failure-evidence")
        );
        assert!(!root.join(super::FIXTURE_FAILURE_EVIDENCE).exists());
        assert!(root.join(super::FIXTURE_FAILURE_DAMAGED[0]).exists());
        fs::remove_dir_all(root).expect("no sibling cleanup");

        for fault in [
            FixtureEvidenceFault::Rename,
            FixtureEvidenceFault::DirectorySync,
        ] {
            let (root, _) = fixture_paths("evidence-invalid-final-fault");
            write_evidence_candidate(&root, super::FIXTURE_FAILURE_EVIDENCE, invalid);
            write_evidence_candidate(
                &root,
                super::FIXTURE_FAILURE_RECOVERY,
                GOOD_FIXTURE_EVIDENCE,
            );
            assert_eq!(
                write_fixture_failure_evidence_with_fault(
                    &root,
                    GOOD_FIXTURE_EVIDENCE,
                    fault,
                    Instant::now() + Duration::from_secs(2),
                ),
                Err("fixture-failure-evidence")
            );
            assert!(
                root.join(super::FIXTURE_FAILURE_EVIDENCE).exists()
                    || root.join(super::FIXTURE_FAILURE_DAMAGED[0]).exists()
            );
            assert!(root.join(super::FIXTURE_FAILURE_RECOVERY).exists());
            fs::remove_dir_all(root).expect("fault cleanup");
        }
    }

    const GOOD_FIXTURE_EVIDENCE: &str = "fixture-failure-version=2\ncategory=fixture-helper-query\nsupervisor-state=provisional\nsupervisor-pid=42\nsupervisor-pgid=43\nsupervisor-uid=unknown\nsupervisor-comm=redacted\nsupervisor-start=redacted\ntarget-state=provisional\ntarget-pid=44\ntarget-pgid=45\ntarget-uid=unknown\ntarget-comm=redacted\ntarget-start=redacted\n";

    /// Drive each real transaction fault at the filesystem seam and then run
    /// the restart path; a complete pending/recovery/final copy must remain
    /// discoverable instead of silently losing provisional target evidence.
    #[test]
    fn fixture_evidence_faults_recover_after_restart() {
        let contents = "fixture-failure-version=2\ncategory=fixture-helper-query\nsupervisor-state=unavailable\nsupervisor-pid=42\nsupervisor-pgid=43\nsupervisor-uid=unknown\nsupervisor-comm=redacted\nsupervisor-start=redacted\ntarget-state=provisional\ntarget-pid=44\ntarget-pgid=45\ntarget-uid=unknown\ntarget-comm=redacted\ntarget-start=redacted\n";
        for fault in [
            FixtureEvidenceFault::Write,
            FixtureEvidenceFault::FileSync,
            FixtureEvidenceFault::Rename,
            FixtureEvidenceFault::DirectorySync,
        ] {
            let (root, _) = fixture_paths("evidence-fault");
            let result = write_fixture_failure_evidence_with_fault(
                &root,
                contents,
                fault,
                Instant::now() + Duration::from_secs(2),
            );
            assert_eq!(result, Err("fixture-failure-evidence"));
            let recoverable = [
                super::FIXTURE_FAILURE_EVIDENCE,
                super::FIXTURE_FAILURE_PENDING,
                super::FIXTURE_FAILURE_RECOVERY,
            ]
            .iter()
            .filter_map(|name| fs::read(root.join(name)).ok())
            .any(|bytes| bytes == contents.as_bytes());
            assert!(recoverable, "fault {fault:?} lost complete evidence");
            write_fixture_failure_evidence_until(
                &root,
                contents,
                Instant::now() + Duration::from_secs(2),
            )
            .expect("restart recovery");
            assert_eq!(
                fs::read(root.join(super::FIXTURE_FAILURE_EVIDENCE)).expect("final evidence"),
                contents.as_bytes()
            );
            fs::remove_dir_all(root).expect("remove evidence fault root");
        }
    }

    /// Prove supervisor and target records retain separate PID/PGID facts;
    /// recovery must never infer a target identity from the outer launcher.
    #[test]
    fn launcher_and_target_evidence_are_not_mixed() {
        let root = std::env::temp_dir().join(format!(
            "ja-fixture-pair-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("pair root");
        let supervisor = ProcessIdentity {
            pid: 42,
            pgid: 43,
            uid: super::current_uid(),
            comm: "supervisor".to_owned(),
            start_identity: "supervisor-start".to_owned(),
        };
        let target = ProcessIdentity {
            pid: 44,
            pgid: 45,
            uid: supervisor.uid,
            comm: "target".to_owned(),
            start_identity: "target-start".to_owned(),
        };
        persist_fixture_failure_pair_until(
            &root,
            supervisor.pid,
            FixtureFailureContext {
                supervisor_group: supervisor.pgid,
                supervisor_identity: Some(&supervisor),
                target_identity: Some(&target),
                target_reference: Some((target.pid, target.pgid)),
            },
            "fixture-helper-control",
            Instant::now() + Duration::from_secs(2),
        )
        .expect("pair evidence");
        let contents = fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
            .expect("pair evidence contents");
        assert!(contents.contains("supervisor-pid=42\n"));
        assert!(contents.contains("supervisor-pgid=43\n"));
        assert!(contents.contains("target-pid=44\n"));
        assert!(contents.contains("target-pgid=45\n"));
        assert!(!contents.contains("supervisor-pgid=45\n"));
        fs::remove_dir_all(root).expect("remove pair root");
    }

    /// Accept a known identity whose path-bearing command/start fields were
    /// redacted.  The numeric identity and UID still authorize recovery; the
    /// redaction is required when Darwin's `ps` value contains a path.
    #[test]
    fn known_identity_allows_path_redaction() {
        let (root, _) = fixture_paths("known-redacted");
        let uid = super::current_uid();
        let supervisor = ProcessIdentity {
            pid: 42,
            pgid: 43,
            uid,
            comm: "/bin/sleep".to_owned(),
            start_identity: "/private/var/start".to_owned(),
        };
        let target = ProcessIdentity {
            pid: 44,
            pgid: 45,
            uid,
            comm: "/usr/bin/sleep".to_owned(),
            start_identity: "/private/var/target".to_owned(),
        };
        persist_fixture_failure_pair_until(
            &root,
            supervisor.pid,
            FixtureFailureContext {
                supervisor_group: supervisor.pgid,
                supervisor_identity: Some(&supervisor),
                target_identity: Some(&target),
                target_reference: Some((target.pid, target.pgid)),
            },
            "fixture-helper-control",
            Instant::now() + Duration::from_secs(2),
        )
        .expect("redacted known identity evidence");
        let contents = fs::read_to_string(root.join(super::FIXTURE_FAILURE_EVIDENCE))
            .expect("redacted evidence contents");
        assert!(contents.contains("supervisor-state=known\n"));
        assert!(contents.contains("supervisor-comm=redacted\n"));
        assert!(contents.contains("target-start=redacted\n"));
        fs::remove_dir_all(root).expect("remove redacted evidence root");
    }

    /// Reject cross-domain collisions as well as same-domain collisions: a
    /// supervisor PID must not equal the target PGID, and vice versa, because
    /// either value could later be reused as a signal destination.
    #[test]
    fn launcher_target_cross_identity_collisions_fail_closed() {
        for (target_pid, target_group) in [(44_u32, 42_i32), (43_u32, 45_i32)] {
            let (root, _) = fixture_paths("cross-identity");
            let supervisor = ProcessIdentity {
                pid: 42,
                pgid: 43,
                uid: super::current_uid(),
                comm: "supervisor".to_owned(),
                start_identity: "supervisor-start".to_owned(),
            };
            let target = ProcessIdentity {
                pid: target_pid,
                pgid: target_group,
                uid: supervisor.uid,
                comm: "target".to_owned(),
                start_identity: "target-start".to_owned(),
            };
            assert_eq!(
                persist_fixture_failure_pair_until(
                    &root,
                    supervisor.pid,
                    FixtureFailureContext {
                        supervisor_group: supervisor.pgid,
                        supervisor_identity: Some(&supervisor),
                        target_identity: Some(&target),
                        target_reference: Some((target.pid, target.pgid)),
                    },
                    "fixture-helper-control",
                    Instant::now() + Duration::from_secs(2),
                ),
                Err("fixture-failure-evidence")
            );
            assert!(!root.join(super::FIXTURE_FAILURE_EVIDENCE).exists());
            fs::remove_dir_all(root).expect("cross identity cleanup");
        }
    }
}
