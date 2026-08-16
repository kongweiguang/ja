// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native fixture cases that call the production marker cleanup implementation.

use super::fd;
use super::marker::write_fixture_marker;
use super::process::{
    EPERM, GroupSignalRelease, ProcessIdentity, ProcessIdentityKey, abort_unreaped_query,
    classify_errno, current_pgid, current_uid, probe_pid_group_until, query_identity_until,
    reap_child_bounded, reap_child_group_bounded, reap_child_without_group_until,
    require_group_empty, set_nonblocking, terminate_group_with_identity_fallback,
};
use super::{
    MARKER_MODE, O_CLOEXEC_FLAG, O_NOFOLLOW_FLAG, cleanup_diagnostic_code,
    cleanup_diagnostic_stage, cleanup_markers_until,
    cleanup_markers_until_with_group_signal_hook_and_diagnostics, emit_cleanup_diagnostic_line,
    marker_remove_diagnostic_code,
};
use crate::spawn_grouped;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FIXTURE_CLEANUP_DEADLINE: Duration = Duration::from_secs(8);
const FIXTURE_ACK_BYTES: usize = 256;
const FIXTURE_ACK_WRITE_DEADLINE: Duration = Duration::from_secs(1);
const POST_SIGNAL_PROOF_BYTES: usize = 1024;
const POST_SIGNAL_PROOF_FIELD_BYTES: usize = 256;

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

/// Run the child-side fixture supervisor.  The supervisor waits for the
/// explicit control byte before reaping its target so production cleanup can
/// observe one stable target identity; polling `try_wait` here would race the
/// parent and turn a transient zombie into an architecture-dependent identity
/// failure.
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
    let target_snapshot = match FixtureTargetSnapshot::capture(&child) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            emit_cleanup_diagnostic_line("supervisor-status", error);
            terminate_fixture_child(&mut child, "fixture-helper-snapshot");
            abort_unreaped_query(error);
        }
    };
    // Publishing the PID is the readiness edge.  The snapshot must already be
    // complete, so a parent signal proof can be matched without querying the
    // target after it becomes a Darwin zombie.
    println!("{}", target_snapshot.identity.pid);
    let _ = std::io::stdout().flush();
    let mut control = std::io::stdin();
    if control_setup_state(set_nonblocking(&control).map_err(|error| error.kind()))
        == FixtureControlEvent::SetupFailed
    {
        terminate_fixture_child(&mut child, "fixture-helper-control");
        abort_unreaped_query("fixture-helper-control");
    }
    loop {
        let mut command_byte = [0_u8; 1];
        let event = match control.read(&mut command_byte) {
            Ok(read) => classify_fixture_control(Ok(read), command_byte[0]),
            Err(error) => classify_fixture_control(Err(error.kind()), command_byte[0]),
        };
        match event {
            FixtureControlEvent::Command => {
                let proof_deadline = Instant::now() + Duration::from_secs(2);
                emit_cleanup_diagnostic_line("post-signal-proof", "begin");
                match read_fixture_post_signal_proof(&mut control, proof_deadline) {
                    Ok(mut proof) => {
                        emit_cleanup_diagnostic_line("post-signal-proof", "ok");
                        emit_cleanup_diagnostic_line("post-signal-reap", "begin");
                        let reap_result = reap_fixture_child_after_group_signal(
                            &mut child,
                            &mut proof,
                            &target_snapshot,
                            proof_deadline,
                        );
                        match reap_result {
                            Ok(()) => {
                                emit_cleanup_diagnostic_line("post-signal-reap", "ok");
                                emit_cleanup_diagnostic_line("ack-write", "begin");
                                match write_fixture_reap_ack() {
                                    Ok(()) => emit_cleanup_diagnostic_line("ack-write", "ok"),
                                    Err(_) => {
                                        emit_cleanup_diagnostic_line(
                                            "ack-write",
                                            "fixture-helper-ack-write",
                                        );
                                        // The target is already reaped, but
                                        // without an explicit acknowledgement
                                        // the parent cannot distinguish
                                        // completion from a crashed helper.
                                        abort_unreaped_query("fixture-helper-ack");
                                    }
                                }
                                break;
                            }
                            Err(error) => emit_cleanup_diagnostic_line("post-signal-reap", error),
                        }
                        // A q without a valid production proof is not the
                        // post-signal path.  Re-enter the original strict
                        // group cleanup, preserving fail-closed behavior for
                        // forged/PID-reused/malformed control payloads.
                        terminate_fixture_child(&mut child, "fixture-helper-control");
                        abort_unreaped_query("fixture-helper-proof");
                    }
                    Err(error) => {
                        emit_cleanup_diagnostic_line("post-signal-proof", error);
                        // Missing or malformed proof is handled exactly like
                        // every other non-production control fault.
                        terminate_fixture_child(&mut child, "fixture-helper-control");
                        abort_unreaped_query("fixture-helper-proof");
                    }
                }
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

/// Freeze the complete target identity before readiness is published.  The
/// supervisor keeps this snapshot next to the owned Child handle so the q
/// proof can be checked after SIGKILL without querying an unstable zombie row.
struct FixtureTargetSnapshot {
    identity: ProcessIdentity,
}

impl FixtureTargetSnapshot {
    /// Capture and bind the target identity before the parent can signal it;
    /// any query, ownership, PGID, or Child mismatch prevents readiness.
    fn capture(child: &std::process::Child) -> Result<Self, &'static str> {
        let pid = child.id();
        let process_group = i32::try_from(pid).map_err(|_| "fixture-helper-snapshot")?;
        if pid <= 1 || process_group <= 1 || process_group == current_pgid() {
            return Err("fixture-helper-snapshot");
        }
        let identity = query_identity_until(pid, Instant::now() + Duration::from_secs(2))?;
        if identity.pid != pid
            || identity.pgid != process_group
            || identity.uid != current_uid()
            || identity.comm.is_empty()
            || identity.start_identity.is_empty()
        {
            return Err("fixture-helper-snapshot");
        }
        Ok(Self { identity })
    }

    /// Require the frozen identity to remain attached to the same Child
    /// handle; no live full-identity query is permitted after group signal.
    fn matches_child(&self, child: &std::process::Child) -> bool {
        child.id() == self.identity.pid
            && self.identity.pid > 1
            && self.identity.pgid > 1
            && u32::try_from(self.identity.pgid).ok() == Some(self.identity.pid)
            && self.identity.uid == current_uid()
            && self.identity.pgid != current_pgid()
    }
}

/// Carry the production hook's completed group-signal proof across the
/// supervisor pipe.  The child-side helper may use the direct-reap shortcut
/// only after this exact identity has been checked and this value consumed;
/// EOF, malformed input, and ordinary control paths never receive such a
/// capability.
struct PostSignalProof {
    identity: ProcessIdentity,
    consumed: bool,
}

impl PostSignalProof {
    /// Issue a proof only from the callback that production invokes after its
    /// captured group signal phase.  Keeping construction private prevents a
    /// generic fixture-control branch from manufacturing the shortcut.
    fn issue_from_production_hook(captured: &ProcessIdentity) -> Self {
        Self {
            identity: captured.clone(),
            consumed: false,
        }
    }

    /// Encode all PID-reuse dimensions without putting command/path text into
    /// the control protocol or diagnostics; the child validates the decoded
    /// identity against its own exact Child before consuming the proof.
    fn encode(&self) -> Result<Vec<u8>, &'static str> {
        if self.consumed {
            return Err("fixture-helper-proof");
        }
        let comm = encode_proof_field(&self.identity.comm)?;
        let start = encode_proof_field(&self.identity.start_identity)?;
        let wire = format!(
            "post-signal-proof-v1;pid={};pgid={};uid={};comm={comm};start={start}\n",
            self.identity.pid, self.identity.pgid, self.identity.uid
        );
        if wire.len() > POST_SIGNAL_PROOF_BYTES {
            return Err("fixture-helper-proof");
        }
        Ok(wire.into_bytes())
    }

    /// Consume exactly once after every identity field matches the observed
    /// target.  Reuse, cross-target proof, and PID-replacement attempts are
    /// all rejected before any direct Child signal is attempted.
    fn consume_for(&mut self, observed: &ProcessIdentity) -> Result<(), &'static str> {
        if self.consumed || self.identity != *observed {
            return Err("fixture-helper-identity");
        }
        self.consumed = true;
        Ok(())
    }
}

/// Convert a bounded identity string to an ASCII-only wire field.  Hex keeps
/// spaces and platform command names out of the grammar while preserving the
/// complete start/comm identity for the child-side equality check.
fn encode_proof_field(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > POST_SIGNAL_PROOF_FIELD_BYTES {
        return Err("fixture-helper-proof");
    }
    Ok(value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Decode one strict hexadecimal identity field; unknown, empty, oversized,
/// and non-UTF-8 values never become a usable post-signal capability.
fn decode_proof_field(value: &str) -> Result<String, &'static str> {
    if value.is_empty()
        || value.len() > POST_SIGNAL_PROOF_FIELD_BYTES * 2
        || !value.len().is_multiple_of(2)
    {
        return Err("fixture-helper-proof");
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = proof_hex_nibble(pair[0]).ok_or("fixture-helper-proof")?;
        let low = proof_hex_nibble(pair[1]).ok_or("fixture-helper-proof")?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).map_err(|_| "fixture-helper-proof")
}

/// Parse one ASCII hex nibble without accepting locale or Unicode variants.
fn proof_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Parse the exact proof record emitted by the production hook.  Field order,
/// count, bounds and identity ranges are fixed so a malformed q payload can
/// only enter the strict cleanup path, never the post-signal shortcut.
fn decode_post_signal_proof(line: &[u8]) -> Result<PostSignalProof, &'static str> {
    if line.len() > POST_SIGNAL_PROOF_BYTES || !line.ends_with(b"\n") {
        return Err("fixture-helper-proof");
    }
    let line = std::str::from_utf8(line).map_err(|_| "fixture-helper-proof")?;
    let fields: Vec<&str> = line[..line.len() - 1].split(';').collect();
    if fields.len() != 6 || fields[0] != "post-signal-proof-v1" {
        return Err("fixture-helper-proof");
    }
    let pid = parse_proof_field(fields[1], "pid=")
        .and_then(|value| value.parse::<u32>().map_err(|_| "fixture-helper-proof"))?;
    let pgid = parse_proof_field(fields[2], "pgid=")
        .and_then(|value| value.parse::<i32>().map_err(|_| "fixture-helper-proof"))?;
    let uid = parse_proof_field(fields[3], "uid=")
        .and_then(|value| value.parse::<u32>().map_err(|_| "fixture-helper-proof"))?;
    let comm = decode_proof_field(parse_proof_field(fields[4], "comm=")?)?;
    let start_identity = decode_proof_field(parse_proof_field(fields[5], "start=")?)?;
    // The fixture target is spawned with process_group(0), so its production
    // proof is valid only when the target PID is also its PGID.  Rejecting the
    // mismatch in the wire parser keeps malformed q payloads out of the
    // direct-reap helper even before a Child is available for comparison.
    if pid <= 1 || pgid <= 1 || u32::try_from(pgid).ok() != Some(pid) {
        return Err("fixture-helper-proof");
    }
    Ok(PostSignalProof {
        identity: ProcessIdentity {
            pid,
            pgid,
            uid,
            comm,
            start_identity,
        },
        consumed: false,
    })
}

/// Extract one ordered proof field while preserving the input slice lifetime;
/// the parser never accepts a duplicate, unknown, empty, or reordered key.
fn parse_proof_field<'a>(field: &'a str, name: &str) -> Result<&'a str, &'static str> {
    field
        .strip_prefix(name)
        .filter(|value| !value.is_empty())
        .ok_or("fixture-helper-proof")
}

/// Read one complete proof record after the command byte.  The reader is
/// nonblocking and bounded so a q without a production proof cannot hang the
/// supervisor or accidentally enter the direct-reap shortcut.
fn read_fixture_post_signal_proof(
    reader: &mut impl Read,
    deadline: Instant,
) -> Result<PostSignalProof, &'static str> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 128];
    while Instant::now() < deadline {
        match reader.read(&mut buffer) {
            Ok(0) => return Err("fixture-helper-proof"),
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > POST_SIGNAL_PROOF_BYTES || output.contains(&b'\n') {
                    // A newline is accepted only when it terminates the
                    // complete bounded record; trailing bytes are rejected by
                    // the exact parser below rather than silently discarded.
                    return decode_post_signal_proof(&output);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err("fixture-helper-proof"),
        }
    }
    Err("fixture-helper-proof")
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

/// Reap a control-path target through the original strict group cleanup.  This
/// remains the only path for EOF, invalid bytes, setup faults, and q payloads
/// that do not carry a proof issued by the production signal hook.
fn terminate_fixture_child(child: &mut std::process::Child, reason: &'static str) {
    let process_group = match i32::try_from(child.id()).ok().filter(|value| *value > 1) {
        Some(value) if value != current_pgid() => value,
        Some(_) | None => {
            if !reap_child_without_group_until(child, Instant::now() + Duration::from_secs(2)) {
                abort_unreaped_query(reason);
            }
            abort_unreaped_query("fixture-group-unsafe");
        }
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

/// Finish a target after a real production group signal.  The typed proof is
/// the only authority that allows the already-signalled direct-reap path; it
/// must match the supervisor's pre-signal snapshot and owned Child before
/// bounded reap and complete target PGID disappearance are attempted.
fn reap_fixture_child_after_group_signal(
    child: &mut std::process::Child,
    proof: &mut PostSignalProof,
    snapshot: &FixtureTargetSnapshot,
    deadline: Instant,
) -> Result<(), &'static str> {
    let process_group = snapshot.identity.pgid;
    emit_cleanup_diagnostic_line("post-signal-snapshot", "begin");
    if !snapshot.matches_child(child) {
        emit_cleanup_diagnostic_line("post-signal-snapshot", "fixture-helper-identity");
        return Err("fixture-helper-identity");
    }
    if consume_post_signal_proof_with_diagnostic(
        proof,
        &snapshot.identity,
        "post-signal-snapshot",
        |stage, code| {
            emit_cleanup_diagnostic_line(stage, code);
        },
    )
    .is_err()
    {
        return Err("fixture-helper-identity");
    }
    emit_cleanup_diagnostic_line("post-signal-snapshot", "ok");

    // The frozen pre-signal snapshot is intentionally reused for the direct
    // reap.  A Darwin target can change its `ps` row while becoming a zombie,
    // but the owned Child handle and pre-signal proof already bind this exact
    // process; only reap and PGID disappearance remain observable now.
    if !reap_fixture_direct_after_proof(child, &snapshot.identity, deadline) {
        emit_cleanup_diagnostic_line("post-signal-reap", "fixture-helper-reap");
        return Err("fixture-helper-reap");
    }
    emit_cleanup_diagnostic_line("post-signal-pgid", "begin");
    match require_group_empty(process_group, deadline) {
        Ok(()) => {
            emit_cleanup_diagnostic_line("post-signal-pgid", "ok");
            Ok(())
        }
        Err("marker-eperm") => {
            emit_cleanup_diagnostic_line("post-signal-pgid", "marker-eperm");
            Err("marker-eperm")
        }
        Err(_) => {
            emit_cleanup_diagnostic_line("post-signal-pgid", "fixture-helper-pgid");
            Err("fixture-helper-pgid")
        }
    }
}

/// Classify proof/observation differences without exposing identity values;
/// the field-level code distinguishes a signal-transition artifact from a
/// genuine PID/PGID/UID or start-identity replacement while staying fail-closed.
fn proof_identity_mismatch_code(
    expected: &ProcessIdentity,
    observed: &ProcessIdentity,
) -> &'static str {
    let mismatches = [
        expected.pid != observed.pid,
        expected.pgid != observed.pgid,
        expected.uid != observed.uid,
        expected.comm != observed.comm,
        expected.start_identity != observed.start_identity,
    ];
    match mismatches.iter().filter(|mismatch| **mismatch).count() {
        0 => "fixture-helper-identity",
        1 if mismatches[0] => "fixture-helper-pid",
        1 if mismatches[1] => "fixture-helper-pgid",
        1 if mismatches[2] => "fixture-helper-uid",
        1 if mismatches[3] => "fixture-helper-comm",
        1 if mismatches[4] => "fixture-helper-start",
        _ => "fixture-helper-identity-multiple",
    }
}

/// Consume the proof before classifying any mismatch; a prior consumed state
/// is itself an error, and only that error path may expose a field category.
fn consume_post_signal_proof_with_diagnostic<F>(
    proof: &mut PostSignalProof,
    observed: &ProcessIdentity,
    stage: &'static str,
    mut emit: F,
) -> Result<(), &'static str>
where
    F: FnMut(&'static str, &'static str),
{
    match proof.consume_for(observed) {
        Ok(()) => Ok(()),
        Err(error) => {
            let code = if proof.identity != *observed {
                proof_identity_mismatch_code(&proof.identity, observed)
            } else {
                error
            };
            emit(stage, code);
            Err(error)
        }
    }
}

/// Reap only the Child bound by the frozen, consumed production proof.  The
/// prior identity query is the sole PID-reuse check before direct Child kill;
/// re-querying a signal-transitioning zombie would make a safe target look
/// different, while the owned Child handle and final PGID probe remain strict.
fn reap_fixture_direct_after_proof(
    child: &mut std::process::Child,
    identity: &ProcessIdentity,
    deadline: Instant,
) -> bool {
    if child.id() != identity.pid || identity.pid <= 1 || identity.pgid <= 1 {
        return false;
    }
    match child.try_wait() {
        Ok(Some(_)) => return true,
        Ok(None) => {}
        Err(_) => return false,
    }
    // The parent already signalled the proof-bound group.  Reissuing kill via
    // this still-owned Child is handle-bound and does not reopen a stale PID
    // query window while the target transitions to a reapable zombie.
    let _ = child.kill();
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
    false
}

/// Publish the bounded target-reap acknowledgement only after the supervisor
/// has proved direct reap and target PGID emptiness; the parent uses this line
/// as the happens-before edge before it probes the marker identity again.
#[cfg(target_os = "macos")]
fn write_fixture_reap_ack() -> Result<(), &'static str> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    set_nonblocking(&stdout).map_err(|_| "fixture-helper-ack")?;
    write_fixture_reap_ack_until(&mut stdout, Instant::now() + FIXTURE_ACK_WRITE_DEADLINE)
}

/// Complete the small ACK write without turning a transient nonblocking pipe
/// state into a false supervisor crash.  The hard deadline preserves the
/// parent-side fail-closed handshake when the reader is unavailable.
fn write_fixture_reap_ack_until(
    writer: &mut impl Write,
    deadline: Instant,
) -> Result<(), &'static str> {
    let ack = b"target-reaped=true\n";
    let mut written = 0;
    while written < ack.len() && Instant::now() < deadline {
        match writer.write(&ack[written..]) {
            Ok(0) => return Err("fixture-helper-ack"),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return Err("fixture-helper-ack"),
        }
    }
    (written == ack.len())
        .then_some(())
        .ok_or("fixture-helper-ack")
}

/// Keep the control-write and acknowledgement read in one bounded phase so a
/// production group cleanup cannot begin direct PID probing between them.
fn complete_fixture_reap_handshake<F>(deadline: Instant, handshake: F) -> Result<(), &'static str>
where
    F: FnOnce(Instant) -> Result<(), &'static str>,
{
    if Instant::now() >= deadline {
        return Err("marker-query-unreaped");
    }
    handshake(deadline)?;
    if Instant::now() >= deadline {
        return Err("marker-query-unreaped");
    }
    Ok(())
}

/// Hold one supervisor ACK proof for exactly one captured target. A later
/// marker group or callback invocation deliberately falls back to normal
/// identity validation instead of reusing the first target's proof.
#[derive(Default)]
struct FixtureAckProof {
    key: Option<ProcessIdentityKey>,
}

impl FixtureAckProof {
    /// Issue the proof once and bind it to all PID-reuse dimensions of the
    /// current capture; repeated or cross-group calls cannot select Reaped.
    fn issue_once(&mut self, captured: &ProcessIdentity) -> GroupSignalRelease {
        if self.key.is_some() {
            return GroupSignalRelease::Continue;
        }
        let key = ProcessIdentityKey::from_identity(captured);
        self.key = Some(key.clone());
        GroupSignalRelease::Reaped(key)
    }

    /// Route every production hook invocation through the one-shot proof
    /// state machine, so a later marker group can never inherit the first
    /// group's acknowledgement.
    fn release_for(&mut self, captured: &ProcessIdentity) -> GroupSignalRelease {
        self.issue_once(captured)
    }

    /// Report whether this one-shot protocol has already consumed its proof.
    fn issued(&self) -> bool {
        self.key.is_some()
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
        .stderr(Stdio::piped());
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
    emit_cleanup_diagnostic_line("supervisor-identity", "begin");
    let launcher_query = query_identity_until(launcher.id(), fixture_deadline);
    match &launcher_query {
        Ok(_) => emit_cleanup_diagnostic_line("supervisor-identity", "ok"),
        Err(category) => emit_cleanup_diagnostic_line("supervisor-identity", category),
    }
    let launcher_identity =
        match accept_launcher_identity(launcher_query, launcher.id(), launcher_group) {
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
    let stdout = match launcher.stdout.take() {
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
    let stderr = match launcher.stderr.take() {
        Some(stderr) => stderr,
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
    if set_nonblocking(&stderr).is_err() {
        drop(stderr);
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
    let mut launcher_output = BufReader::new(stdout);
    // Keep the diagnostic stream unbuffered so the explicit 4 KiB retained
    // prefix is also the process-side memory bound, not merely an output cap
    // hidden behind a larger BufReader allocation.
    let mut launcher_diagnostics = stderr;
    emit_cleanup_diagnostic_line("supervisor-status", "begin");
    let output_result = read_launcher_output(&mut launcher_output, &mut launcher, fixture_deadline);
    match &output_result {
        Ok(_) => emit_cleanup_diagnostic_line("supervisor-status", "ok"),
        Err(category) => emit_cleanup_diagnostic_line("supervisor-status", category),
    }
    let output = match output_result {
        Ok(output) => output,
        Err(error) => {
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
    emit_cleanup_diagnostic_line("direct-pid", "begin");
    let target_query = query_identity_until(pid, fixture_deadline);
    match &target_query {
        Ok(_) => emit_cleanup_diagnostic_line("direct-pid", "ok"),
        Err(category) => emit_cleanup_diagnostic_line("direct-pid", category),
    }
    let identity = match target_query {
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
    emit_cleanup_diagnostic_line("direct-pgid", "begin");
    let process_group = match i32::try_from(pid) {
        Ok(process_group)
            if process_group > 1
                && process_group != current_pgid()
                && identity.pgid == process_group =>
        {
            emit_cleanup_diagnostic_line("direct-pgid", "ok");
            process_group
        }
        _ => {
            emit_cleanup_diagnostic_line("direct-pgid", "fixture-helper-identity");
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
        emit_cleanup_diagnostic_line("direct-pid", "fixture-helper-identity");
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
    // This flag only selects the supervisor's final bounded reap path after
    // marker cleanup; it is deliberately not an authorization proof for any
    // target group. The proof itself is carried by FixtureAckProof below.
    let mut ack_handshake_succeeded = false;
    let result = {
        let mut handshake_error = None;
        let mut ack_proof = FixtureAckProof::default();
        let mut signal_hook = |captured: &ProcessIdentity| {
            if ack_proof.issued() {
                // The one-shot proof cannot authorize another marker group or
                // a repeated callback; Continue forces the normal identity
                // revalidation path instead of reusing the first ACK.
                return Ok(GroupSignalRelease::Continue);
            }
            if let Some(error) = handshake_error {
                return Err(error);
            }
            let result = complete_fixture_reap_handshake(fixture_deadline, |deadline| {
                emit_cleanup_diagnostic_line("supervisor-status", "begin");
                let proof = PostSignalProof::issue_from_production_hook(captured);
                if let Err(error) =
                    request_fixture_launcher_shutdown_with_proof(&mut launcher, &proof, deadline)
                {
                    emit_cleanup_diagnostic_line("supervisor-status", error);
                    return Err(error);
                }
                emit_cleanup_diagnostic_line("supervisor-status", "ok");
                emit_cleanup_diagnostic_line("ack-read", "begin");
                let ack = wait_fixture_reap_ack_with_diagnostics(
                    &mut launcher_output,
                    &mut launcher,
                    Some(&mut launcher_diagnostics),
                    deadline,
                );
                match &ack {
                    Ok(()) => emit_cleanup_diagnostic_line("ack-read", "ok"),
                    Err(error) => emit_cleanup_diagnostic_line("ack-read", error),
                }
                ack
            })
            .map_err(|_| "marker-query-unreaped");
            match result {
                Ok(()) => {
                    ack_handshake_succeeded = true;
                    Ok(ack_proof.release_for(captured))
                }
                Err(error) => {
                    handshake_error = Some(error);
                    Err(error)
                }
            }
        };
        let mut query_diagnostic = |stage: &'static str, code: &'static str| {
            emit_cleanup_diagnostic_line(stage, code);
        };
        cleanup_markers_until_with_group_signal_hook_and_diagnostics(
            &root,
            &report,
            true,
            fixture_deadline,
            &mut signal_hook,
            &mut query_diagnostic,
        )
    };
    // Let the supervisor perform its own bounded target reap after production
    // cleanup has finished.  A successful production pass already consumed the
    // acknowledgement, so this phase only waits/reaps the supervisor; it never
    // sends a second control byte that could hide a lost acknowledgement.
    let launcher_result = shutdown_launcher_after_marker(
        &mut launcher,
        launcher_group,
        &identity,
        fixture_deadline,
        ack_handshake_succeeded,
    );
    if result.is_err() {
        // Preserve only the production report's fixed category so a native
        // runner can distinguish residual, identity, query and signal faults
        // without exposing a PID, path, locale text or marker contents.
        emit_cleanup_diagnostic_line("cleanup-report", "begin");
        let category = fixture_cleanup_failure_category_until(&report, fixture_deadline);
        emit_cleanup_diagnostic_line("cleanup-report", category);
        return finish_descendant_failure(
            &root,
            launcher_identity.as_ref(),
            &identity,
            launcher_result,
            fixture_deadline,
            category,
        );
    }
    if !ack_handshake_succeeded {
        return finish_descendant_failure(
            &root,
            launcher_identity.as_ref(),
            &identity,
            Err("fixture-helper-ack"),
            fixture_deadline,
            "fixture-group-control",
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
    // The supervisor's success proves it consumed the control request, but
    // only an independent target PID/PGID probe proves that its descendant is
    // actually gone before the fixture removes its evidence root.
    if let Err(error) = verify_reaped_group(&identity, fixture_deadline) {
        abort_fixture_group(error);
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

/// Complete the supervisor/target teardown after the production marker pass.
/// The control protocol is preferred because it lets the original supervisor
/// reap its target; only a failed control or non-success exit enters the
/// identity-checked fallback before the supervisor Child is reaped.
fn shutdown_launcher_after_marker(
    launcher: &mut std::process::Child,
    launcher_group: i32,
    target_identity: &ProcessIdentity,
    deadline: Instant,
    reap_acknowledged: bool,
) -> Result<(), &'static str> {
    let (control_sent, status_success) = if reap_acknowledged {
        // The ACK reader already observed a successful terminal status while
        // draining EOF; calling try_wait again could turn a valid reap into an
        // ECHILD/query failure on platforms that do not cache that status.
        (true, true)
    } else {
        let control_sent = request_fixture_launcher_shutdown(launcher, deadline).is_ok();
        let status_success = control_sent
            && wait_child_exit_until(launcher, deadline).is_some_and(|status| status.success());
        (control_sent, status_success)
    };
    let status_failed = control_sent && !status_success;
    let target_result = if status_success {
        // The supervisor's successful exit is evidence that it ran its own
        // target finalizer; the caller still performs the independent exact
        // PID/PGID proof before accepting the fixture result.
        Ok(())
    } else {
        match verify_reaped_group(target_identity, deadline) {
            Ok(()) => Ok(()),
            Err(_) => terminate_group_with_identity_fallback(
                target_identity,
                std::slice::from_ref(target_identity),
                deadline,
            ),
        }
    };
    let supervisor_result = if status_success {
        require_group_empty(launcher_group, deadline)
    } else {
        reap_child_bounded(launcher, launcher_group, deadline)
    };
    match (status_failed, target_result, supervisor_result) {
        (false, Ok(()), Ok(())) => Ok(()),
        (true, _, _) => Err("fixture-helper-control"),
        (false, Err(error), _) | (false, _, Err(error)) => Err(error),
    }
}

/// Ask the fixture supervisor to kill and reap its exact sleep child before the
/// parent touches the supervisor Child; a direct kill of the supervisor alone
/// would orphan the target's private process group.
fn request_fixture_launcher_shutdown(
    launcher: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), &'static str> {
    write_fixture_control(launcher, b"q", deadline)
}

/// Send q together with the proof issued by the real production hook.  The
/// supervisor cannot manufacture this payload itself, so only this callback
/// can unlock its already-signalled direct-reap path.
fn request_fixture_launcher_shutdown_with_proof(
    launcher: &mut std::process::Child,
    proof: &PostSignalProof,
    deadline: Instant,
) -> Result<(), &'static str> {
    let proof_wire = proof.encode()?;
    let mut payload = Vec::with_capacity(1 + proof_wire.len());
    payload.push(b'q');
    payload.extend_from_slice(&proof_wire);
    write_fixture_control(launcher, &payload, deadline)
}

/// Write one bounded control payload without using write_all, because a full
/// pipe must not turn a supervisor cleanup phase into an unbounded wait.
fn write_fixture_control(
    launcher: &mut std::process::Child,
    payload: &[u8],
    deadline: Instant,
) -> Result<(), &'static str> {
    let stdin = launcher.stdin.as_mut().ok_or("fixture-helper-control")?;
    set_nonblocking(stdin).map_err(|_| "fixture-helper-control")?;
    let mut remaining = payload;
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

/// Wait for the supervisor's exact acknowledgement line under the same
/// deadline as the control write; a missing, malformed or premature ack keeps
/// marker evidence alive and never permits production PID cleanup to proceed.
#[cfg(test)]
fn wait_fixture_reap_ack(
    reader: &mut impl Read,
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), &'static str> {
    wait_fixture_reap_ack_with_diagnostics(reader, child, None, deadline)
}

/// Wait for ACK while draining the supervisor's separate fixed diagnostic
/// stream.  Keeping diagnostics off stdout preserves the exact ACK grammar;
/// forwarding only the allowlisted stage/code pairs makes the failing child
/// phase observable without exposing identity values or raw stderr.
fn wait_fixture_reap_ack_with_diagnostics(
    reader: &mut impl Read,
    child: &mut std::process::Child,
    mut diagnostics: Option<&mut dyn Read>,
    deadline: Instant,
) -> Result<(), &'static str> {
    let mut output = Vec::new();
    let mut diagnostic_output = FixtureDiagnosticBuffer::default();
    let mut buffer = [0_u8; 128];
    let mut terminal_status = None;
    let mut eof = false;
    while Instant::now() < deadline {
        let mut progressed = false;
        if let Some(diagnostics) = diagnostics.as_deref_mut() {
            progressed |= drain_fixture_diagnostics(diagnostics, &mut diagnostic_output);
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                eof = true;
                progressed = true;
            }
            Ok(read) => {
                output.extend_from_slice(&buffer[..read]);
                if output.len() > FIXTURE_ACK_BYTES {
                    forward_fixture_diagnostics(diagnostic_output.bytes());
                    return Err("fixture-helper-ack");
                }
                progressed = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                forward_fixture_diagnostics(diagnostic_output.bytes());
                return Err("fixture-helper-ack");
            }
        }
        if terminal_status.is_none() {
            terminal_status = match child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    forward_fixture_diagnostics(diagnostic_output.bytes());
                    return Err("fixture-helper-ack");
                }
            };
        }
        if eof {
            if let Some(status) = terminal_status {
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    drain_fixture_diagnostics(diagnostics, &mut diagnostic_output);
                }
                forward_fixture_diagnostics(diagnostic_output.bytes());
                let ack = std::str::from_utf8(&output)
                    .ok()
                    .is_some_and(is_fixture_reap_ack);
                return if status.success() && ack {
                    Ok(())
                } else {
                    Err("fixture-helper-ack")
                };
            }
        }
        if !progressed || (eof && terminal_status.is_none()) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    forward_fixture_diagnostics(diagnostic_output.bytes());
    Err("marker-query-unreaped")
}

const FIXTURE_DIAGNOSTIC_BYTES: usize = 4096;
const FIXTURE_DIAGNOSTIC_READ_BYTES: usize = 256;
const FIXTURE_DIAGNOSTIC_DRAIN_BUDGET: usize = 1024;

/// Retain only a bounded diagnostic prefix while allowing the parent to keep
/// consuming a noisy child pipe.  The drop mode is deliberately independent
/// from ACK validation: stderr is observability only, so malformed or failed
/// diagnostics must never turn a valid stdout ACK into a cleanup failure.
#[derive(Default)]
struct FixtureDiagnosticBuffer {
    retained: Vec<u8>,
    drop_mode: bool,
    closed: bool,
}

impl FixtureDiagnosticBuffer {
    /// Read at most one small budget per call so a continuous stderr writer
    /// cannot prevent the outer absolute-deadline loop from polling stdout or
    /// the child status.
    fn drain(&mut self, reader: &mut dyn Read) -> bool {
        if self.closed {
            return false;
        }
        let mut buffer = [0_u8; FIXTURE_DIAGNOSTIC_READ_BYTES];
        let mut budget = FIXTURE_DIAGNOSTIC_DRAIN_BUDGET;
        let mut progressed = false;
        while budget > 0 {
            let read_limit = budget.min(buffer.len());
            match reader.read(&mut buffer[..read_limit]) {
                Ok(0) => {
                    self.closed = true;
                    return progressed;
                }
                Ok(read) => {
                    progressed = true;
                    budget = budget.saturating_sub(read);
                    if !self.drop_mode {
                        let remaining = FIXTURE_DIAGNOSTIC_BYTES - self.retained.len();
                        let keep = remaining.min(read);
                        self.retained.extend_from_slice(&buffer[..keep]);
                        if keep < read {
                            self.drop_mode = true;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return progressed;
                }
                Err(_) => {
                    // A failed read cannot authorize any operation, but the
                    // next bounded poll still attempts to drain a pipe whose
                    // producer may recover; all subsequent bytes are dropped.
                    self.drop_mode = true;
                    return progressed;
                }
            }
        }
        progressed
    }

    /// Expose only the retained prefix; the backing allocation never exceeds
    /// the diagnostic byte cap and dropped bytes are not recoverable.
    fn bytes(&self) -> &[u8] {
        &self.retained
    }
}

/// Drain only the bounded nonblocking helper diagnostic stream; a full or
/// unreadable stream switches to discard mode but never affects ACK or cleanup
/// ownership.
fn drain_fixture_diagnostics(reader: &mut dyn Read, output: &mut FixtureDiagnosticBuffer) -> bool {
    output.drain(reader)
}

/// Forward only proof/reap diagnostic fields through the production allowlist;
/// control/status noise and malformed input are discarded without influencing
/// the result of the stdout ACK state machine.
fn forward_fixture_diagnostics(output: &[u8]) {
    forward_fixture_diagnostics_with(output, |stage, code| {
        emit_cleanup_diagnostic_line(stage, code);
    });
}

/// Parse complete newline-terminated diagnostic records with a sink seam so
/// tests can prove redaction without capturing process-global stderr output.
fn forward_fixture_diagnostics_with<F>(output: &[u8], mut sink: F)
where
    F: FnMut(&str, &str),
{
    let Ok(text) = std::str::from_utf8(output) else {
        return;
    };
    for record in text.split_inclusive('\n') {
        let Some(line) = record.strip_suffix('\n') else {
            continue;
        };
        let Some(rest) = line.strip_prefix("SANDBOX-MARKER-QUERY: stage=") else {
            continue;
        };
        let Some((stage, code)) = rest.split_once(" code=") else {
            continue;
        };
        if !matches!(
            stage,
            "post-signal-proof"
                | "post-signal-snapshot"
                | "post-signal-identity"
                | "post-signal-reap"
                | "post-signal-pgid"
                | "marker-remove"
                | "ack-write"
        ) {
            continue;
        }
        if stage == "marker-remove" {
            let Some(code) = marker_remove_diagnostic_code(code) else {
                continue;
            };
            sink("marker-remove", code);
        } else {
            sink(
                cleanup_diagnostic_stage(stage),
                cleanup_diagnostic_code(code),
            );
        }
    }
}

/// Accept only the one-line acknowledgement emitted after target direct reap
/// and PGID emptiness; extra fields or missing newlines cannot authorize a
/// parent-side identity probe.
fn is_fixture_reap_ack(line: &str) -> bool {
    line == "target-reaped=true\n"
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
            emit_cleanup_diagnostic_line("supervisor-status", "begin");
            if let Ok(output) = read_launcher_output(&mut stdout, launcher, deadline) {
                emit_cleanup_diagnostic_line("supervisor-status", "ok");
                if let Ok(pid) = output.trim().parse::<u32>() {
                    if let Ok(process_group) = i32::try_from(pid)
                        && pid > 1
                        && process_group > 1
                        && process_group != current_pgid()
                    {
                        target_reference = Some((pid, process_group));
                        emit_cleanup_diagnostic_line("direct-pid", "begin");
                        let target_query = query_identity_until(pid, deadline);
                        match &target_query {
                            Ok(_) => emit_cleanup_diagnostic_line("direct-pid", "ok"),
                            Err(category) => emit_cleanup_diagnostic_line("direct-pid", category),
                        }
                        target_identity = target_query.ok().filter(|identity| {
                            identity.pid == pid
                                && identity.pgid == process_group
                                && identity.uid == current_uid()
                        });
                    }
                }
            } else {
                emit_cleanup_diagnostic_line("supervisor-status", "fixture-helper-output");
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
                    return Err("fixture-helper-output");
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
    use super::super::format_cleanup_diagnostic_line;
    use super::{
        FIXTURE_DIAGNOSTIC_BYTES, FIXTURE_DIAGNOSTIC_DRAIN_BUDGET, FixtureAckProof,
        FixtureControlEvent, FixtureDiagnosticBuffer, FixtureEvidenceFault, FixtureFailureContext,
        FixtureTargetSnapshot, GroupSignalRelease, PostSignalProof, ProcessIdentity,
        accept_launcher_identity, classify_fixture_control, complete_fixture_reap_handshake,
        consume_post_signal_proof_with_diagnostic, control_setup_state, decode_post_signal_proof,
        drain_fixture_diagnostics, finish_fixture_failure_with, fixture_cleanup_failure_category,
        fixture_group_failure_reason, fixture_paths, forward_fixture_diagnostics_with,
        is_fixture_reap_ack, persist_fixture_failure_pair_until, proof_identity_mismatch_code,
        read_fixture_post_signal_proof, reap_fixture_child_after_group_signal,
        wait_child_exit_until, wait_fixture_reap_ack_with_diagnostics,
        write_fixture_failure_evidence_until, write_fixture_failure_evidence_with_fault,
        write_fixture_reap_ack_until,
    };
    use crate::marker_cleanup::process::{
        ProcessState, current_uid, group_state, reap_child_group_bounded, set_nonblocking,
    };
    use crate::spawn_grouped;
    use std::fs;
    use std::io::{self, BufReader, Cursor, Read, Write};
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

    /// Keep the supervisor acknowledgement grammar closed: only the exact
    /// target-reaped line can establish the happens-before edge for cleanup.
    #[test]
    fn fixture_reap_ack_requires_exact_line() {
        for line in [
            "target-reaped=true\n",
            "target-reaped=true",
            "target-reaped=false\n",
            "target-reaped=true\nextra\n",
            "target-reaped=true\r\n",
        ] {
            assert_eq!(is_fixture_reap_ack(line), line == "target-reaped=true\n");
        }
    }

    /// Prove the supervisor ACK writer tolerates bounded partial/WouldBlock
    /// states while still rejecting a closed or expired control pipe.
    #[test]
    fn fixture_reap_ack_writer_is_bounded_and_complete() {
        struct FlakyWriter {
            calls: usize,
            output: Vec<u8>,
        }

        impl Write for FlakyWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "retry"));
                }
                let count = bytes.len().min(3);
                self.output.extend_from_slice(&bytes[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = FlakyWriter {
            calls: 0,
            output: Vec::new(),
        };
        write_fixture_reap_ack_until(&mut writer, Instant::now() + Duration::from_secs(1))
            .expect("bounded ACK write");
        assert_eq!(writer.output, b"target-reaped=true\n");

        let mut closed = io::sink();
        assert!(write_fixture_reap_ack_until(&mut closed, Instant::now()).is_err());
    }

    /// Prove stderr is observability-only: a valid stdout ACK remains a success
    /// when the child emits oversized, malformed, or unreadable diagnostics.
    #[test]
    fn fixture_reap_ack_ignores_diagnostic_faults() {
        struct ErrorReader {
            failed: bool,
        }

        impl Read for ErrorReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "diagnostic fault",
                    ))
                } else {
                    Ok(0)
                }
            }
        }

        let cases = [
            "printf 'target-reaped=true\\n'; printf '%05000d\\n' 0 >&2",
            "printf 'target-reaped=true\\n'; printf 'secret=/private/not-forwarded\\n' >&2",
        ];
        for script in cases {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = spawn_grouped(&mut command).expect("spawn diagnostic ACK child");
            let process_group = i32::try_from(child.id())
                .ok()
                .filter(|value| *value > 1)
                .expect("diagnostic ACK process group");
            let stdout = child.stdout.take().expect("diagnostic ACK stdout");
            let stderr = child.stderr.take().expect("diagnostic ACK stderr");
            set_nonblocking(&stdout).expect("diagnostic ACK stdout nonblocking");
            set_nonblocking(&stderr).expect("diagnostic ACK stderr nonblocking");
            let mut output = BufReader::new(stdout);
            let mut diagnostics = stderr;
            let result = wait_fixture_reap_ack_with_diagnostics(
                &mut output,
                &mut child,
                Some(&mut diagnostics),
                Instant::now() + Duration::from_secs(2),
            );
            assert_eq!(result, Ok(()));
            assert_eq!(group_state(process_group), ProcessState::Empty);
        }

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'target-reaped=true\\n'"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn_grouped(&mut command).expect("spawn read-error ACK child");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("read-error ACK process group");
        let stdout = child.stdout.take().expect("read-error ACK stdout");
        set_nonblocking(&stdout).expect("read-error ACK stdout nonblocking");
        let mut output = BufReader::new(stdout);
        let mut diagnostics = ErrorReader { failed: false };
        assert_eq!(
            wait_fixture_reap_ack_with_diagnostics(
                &mut output,
                &mut child,
                Some(&mut diagnostics),
                Instant::now() + Duration::from_secs(2),
            ),
            Ok(())
        );
        assert_eq!(group_state(process_group), ProcessState::Empty);
    }

    /// Prove a normal diagnostic stream cannot make an invalid ACK succeed;
    /// stdout grammar and child exit status remain the only authorization.
    #[test]
    fn fixture_reap_ack_rejects_invalid_stdout_with_valid_diagnostics() {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf 'target-reaped=false\\n'; printf 'SANDBOX-MARKER-QUERY: stage=post-signal-reap code=ok\\n' >&2",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn_grouped(&mut command).expect("spawn invalid ACK child");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("invalid ACK process group");
        let stdout = child.stdout.take().expect("invalid ACK stdout");
        let stderr = child.stderr.take().expect("invalid ACK stderr");
        set_nonblocking(&stdout).expect("invalid ACK stdout nonblocking");
        set_nonblocking(&stderr).expect("invalid ACK stderr nonblocking");
        let mut output = BufReader::new(stdout);
        let mut diagnostics = stderr;
        assert_eq!(
            wait_fixture_reap_ack_with_diagnostics(
                &mut output,
                &mut child,
                Some(&mut diagnostics),
                Instant::now() + Duration::from_secs(2),
            ),
            Err("fixture-helper-ack")
        );
        reap_child_group_bounded(
            &mut child,
            process_group,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("invalid ACK cleanup");
    }

    /// Bound continuous diagnostic output per poll and retain no more than the
    /// fixed prefix; subsequent bytes are consumed and discarded.
    #[test]
    fn fixture_diagnostic_drain_is_bounded_and_discarding() {
        struct ContinuousReader {
            bytes_read: usize,
        }

        impl Read for ContinuousReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.bytes_read += buffer.len();
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }

        let mut reader = ContinuousReader { bytes_read: 0 };
        let mut diagnostics = FixtureDiagnosticBuffer::default();
        assert!(drain_fixture_diagnostics(&mut reader, &mut diagnostics));
        assert_eq!(reader.bytes_read, FIXTURE_DIAGNOSTIC_DRAIN_BUDGET);
        assert_eq!(diagnostics.bytes().len(), FIXTURE_DIAGNOSTIC_DRAIN_BUDGET);
        assert!(!diagnostics.drop_mode);
        let mut rounds = 1;
        while !diagnostics.drop_mode {
            assert!(drain_fixture_diagnostics(&mut reader, &mut diagnostics));
            rounds += 1;
            assert!(rounds <= 8);
        }
        assert_eq!(diagnostics.bytes().len(), FIXTURE_DIAGNOSTIC_BYTES);
        assert_eq!(reader.bytes_read, FIXTURE_DIAGNOSTIC_DRAIN_BUDGET * rounds);
        assert!(drain_fixture_diagnostics(&mut reader, &mut diagnostics));
        assert_eq!(diagnostics.bytes().len(), FIXTURE_DIAGNOSTIC_BYTES);
        assert_eq!(
            reader.bytes_read,
            FIXTURE_DIAGNOSTIC_DRAIN_BUDGET * (rounds + 1)
        );
    }

    /// Prove read errors switch to discard mode and a later recovered read
    /// cannot grow the retained buffer or change ACK-relevant state.
    #[test]
    fn fixture_diagnostic_read_error_is_best_effort() {
        struct ErrorThenData {
            failed: bool,
        }

        impl Read for ErrorThenData {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("diagnostic fault"));
                }
                buffer[..4].copy_from_slice(b"tail");
                Ok(4)
            }
        }

        let mut reader = ErrorThenData { failed: false };
        let mut diagnostics = FixtureDiagnosticBuffer::default();
        assert!(!drain_fixture_diagnostics(&mut reader, &mut diagnostics));
        assert!(diagnostics.drop_mode);
        assert!(drain_fixture_diagnostics(&mut reader, &mut diagnostics));
        assert!(diagnostics.bytes().is_empty());
    }

    /// Only complete proof/reap records reach the sink; unknown stages are
    /// dropped and unknown codes are normalized without exposing raw values.
    #[test]
    fn fixture_diagnostic_forwarding_is_redacted_and_subsetted() {
        let input = b"SANDBOX-MARKER-QUERY: stage=post-signal-proof code=begin\n\
SANDBOX-MARKER-QUERY: stage=supervisor-status code=ok\n\
SANDBOX-MARKER-QUERY: stage=post-signal-snapshot code=fixture-helper-comm\n\
SANDBOX-MARKER-QUERY: stage=post-signal-pgid code=secret=/private/path\n\
SANDBOX-MARKER-QUERY: stage=post-signal-reap code=fixture-helper-reap\n\
SANDBOX-MARKER-QUERY: stage=marker-remove code=unlink\n\
SANDBOX-MARKER-QUERY: stage=marker-remove code=begin\n\
SANDBOX-MARKER-QUERY: stage=marker-remove code=/private/path\n\
SANDBOX-MARKER-QUERY: stage=post-signal-proof code=partial";
        let mut seen = Vec::new();
        forward_fixture_diagnostics_with(input, |stage, code| {
            seen.push((stage.to_owned(), code.to_owned()));
        });
        assert_eq!(
            seen,
            [
                ("post-signal-proof".to_owned(), "begin".to_owned()),
                (
                    "post-signal-snapshot".to_owned(),
                    "fixture-helper-comm".to_owned(),
                ),
                ("post-signal-pgid".to_owned(), "unknown".to_owned()),
                (
                    "post-signal-reap".to_owned(),
                    "fixture-helper-reap".to_owned()
                ),
                ("marker-remove".to_owned(), "unlink".to_owned()),
            ]
        );

        let mut invalid_utf8 = b"SANDBOX-MARKER-QUERY: stage=post-signal-proof code=ok".to_vec();
        invalid_utf8.push(0xff);
        invalid_utf8.push(b'\n');
        let mut invalid_seen = Vec::new();
        forward_fixture_diagnostics_with(&invalid_utf8, |stage, code| {
            invalid_seen.push((stage.to_owned(), code.to_owned()));
        });
        assert!(invalid_seen.is_empty());
    }

    /// Drive the producer formatter through the same bounded diagnostic
    /// buffer and forwarder used by the fixture, proving valid marker removal
    /// stages survive while malformed values produce no stderr record.
    #[test]
    fn marker_remove_producer_to_forwarder_preserves_closed_code() {
        let line = format_cleanup_diagnostic_line("marker-remove", "unlink")
            .expect("valid marker removal diagnostic");
        let mut producer_output = line.into_bytes();
        producer_output.push(b'\n');
        let mut producer_reader = Cursor::new(producer_output);
        let mut bounded_output = FixtureDiagnosticBuffer::default();
        assert!(drain_fixture_diagnostics(
            &mut producer_reader,
            &mut bounded_output
        ));

        let mut seen = Vec::new();
        forward_fixture_diagnostics_with(bounded_output.bytes(), |stage, code| {
            seen.push((stage.to_owned(), code.to_owned()));
        });
        assert_eq!(seen, [("marker-remove".to_owned(), "unlink".to_owned())]);
        assert!(format_cleanup_diagnostic_line("marker-remove", "/private/path").is_none());
        assert!(format_cleanup_diagnostic_line("marker-remove", "identity=42").is_none());
    }

    /// Prove the production proof slot is single-use and rejects a second
    /// marker group, including PID reuse with a changed start/comm/PGID.
    #[test]
    fn fixture_ack_proof_is_single_use_and_target_bound() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 43,
            uid: current_uid(),
            comm: "target".to_owned(),
            start_identity: "start".to_owned(),
        };
        let mut proof = FixtureAckProof::default();
        match proof.release_for(&target) {
            GroupSignalRelease::Reaped(key) => assert!(key.matches(&target)),
            GroupSignalRelease::Continue => panic!("first proof was not issued"),
        }
        assert_eq!(proof.release_for(&target), GroupSignalRelease::Continue);
        let reused = ProcessIdentity {
            pgid: 44,
            comm: "other".to_owned(),
            start_identity: "reused".to_owned(),
            ..target
        };
        assert_eq!(proof.release_for(&reused), GroupSignalRelease::Continue);
    }

    /// Exercise the actual production-hook proof wire and its one-shot,
    /// full-identity binding.  A changed PID/PGID/UID/comm/start field or any
    /// grammar mutation must remain outside the post-signal shortcut.
    #[test]
    fn post_signal_proof_is_strict_and_single_use() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 42,
            uid: current_uid(),
            comm: "target worker".to_owned(),
            start_identity: "Tue Jan 2 00:00:00 2026".to_owned(),
        };
        let proof = PostSignalProof::issue_from_production_hook(&target);
        let wire = proof.encode().expect("proof wire");
        let mut decoded = decode_post_signal_proof(&wire).expect("decode proof wire");
        assert_eq!(decoded.identity, target);
        decoded.consume_for(&target).expect("first proof consume");
        assert_eq!(decoded.consume_for(&target), Err("fixture-helper-identity"));

        for malformed in [
            String::from_utf8(wire.clone())
                .expect("wire utf8")
                .replace("pgid=42", "pgid=43"),
            String::from_utf8(wire.clone())
                .expect("wire utf8")
                .replace(";start=", ";unknown=x;start="),
            String::from_utf8(wire.clone())
                .expect("wire utf8")
                .replace("\n", ";pid=42\n"),
            String::from_utf8(wire.clone())
                .expect("wire utf8")
                .trim_end_matches('\n')
                .to_owned(),
        ] {
            assert!(decode_post_signal_proof(malformed.as_bytes()).is_err());
        }

        let mut reused = PostSignalProof::issue_from_production_hook(&target);
        let changed = ProcessIdentity {
            start_identity: "reused-start".to_owned(),
            ..target.clone()
        };
        assert_eq!(reused.consume_for(&changed), Err("fixture-helper-identity"));
        reused
            .consume_for(&target)
            .expect("wrong proof must not consume capability");
    }

    /// Keep proof mismatch diagnostics field-specific but path-free so native
    /// runs reveal whether a signal transition changed a row field without
    /// turning any mismatch into a successful cleanup authorization.
    #[test]
    fn post_signal_proof_mismatch_categories_are_closed() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 42,
            uid: current_uid(),
            comm: "target".to_owned(),
            start_identity: "start".to_owned(),
        };
        let cases = [
            (
                ProcessIdentity {
                    pid: 43,
                    ..target.clone()
                },
                "fixture-helper-pid",
            ),
            (
                ProcessIdentity {
                    pgid: 43,
                    ..target.clone()
                },
                "fixture-helper-pgid",
            ),
            (
                ProcessIdentity {
                    uid: target.uid.saturating_add(1),
                    ..target.clone()
                },
                "fixture-helper-uid",
            ),
            (
                ProcessIdentity {
                    comm: "zombie".to_owned(),
                    ..target.clone()
                },
                "fixture-helper-comm",
            ),
            (
                ProcessIdentity {
                    start_identity: "reused".to_owned(),
                    ..target.clone()
                },
                "fixture-helper-start",
            ),
            (
                ProcessIdentity {
                    comm: "zombie".to_owned(),
                    start_identity: "reused".to_owned(),
                    ..target.clone()
                },
                "fixture-helper-identity-multiple",
            ),
        ];
        for (observed, expected) in cases {
            assert_eq!(proof_identity_mismatch_code(&target, &observed), expected);
        }
        assert_eq!(
            proof_identity_mismatch_code(&target, &target),
            "fixture-helper-identity"
        );
    }

    /// Bind the production proof to the supervisor's frozen pre-signal
    /// snapshot.  A changed comm/start value is rejected before any direct
    /// reap, even though those fields may later be unstable on a zombie.
    #[test]
    fn pre_signal_snapshot_rejects_exec_and_comm_drift() {
        let snapshot = FixtureTargetSnapshot {
            identity: ProcessIdentity {
                pid: 42,
                pgid: 42,
                uid: current_uid(),
                comm: "target".to_owned(),
                start_identity: "start".to_owned(),
            },
        };
        let changed_cases = [
            (
                ProcessIdentity {
                    comm: "replacement".to_owned(),
                    ..snapshot.identity.clone()
                },
                "fixture-helper-comm",
            ),
            (
                ProcessIdentity {
                    start_identity: "reused-start".to_owned(),
                    ..snapshot.identity.clone()
                },
                "fixture-helper-start",
            ),
        ];
        for (changed, expected) in changed_cases {
            let mut proof = PostSignalProof::issue_from_production_hook(&changed);
            let mut diagnostics = Vec::new();
            assert_eq!(
                consume_post_signal_proof_with_diagnostic(
                    &mut proof,
                    &snapshot.identity,
                    "post-signal-snapshot",
                    |stage, code| diagnostics.push((stage, code)),
                ),
                Err("fixture-helper-identity")
            );
            assert_eq!(diagnostics, [("post-signal-snapshot", expected)]);
        }
        let mut valid_proof = PostSignalProof::issue_from_production_hook(&snapshot.identity);
        consume_post_signal_proof_with_diagnostic(
            &mut valid_proof,
            &snapshot.identity,
            "post-signal-snapshot",
            |_, _| panic!("matching pre-signal snapshot emitted a mismatch"),
        )
        .expect("matching pre-signal snapshot proof");
    }

    /// Exercise the production consume/error seam: an already-consumed proof
    /// with the same identity is generic, while a changed identity is only
    /// field-classified after `consume_for` has rejected it.
    #[test]
    fn post_signal_proof_consume_errors_are_ordered() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 42,
            uid: current_uid(),
            comm: "target".to_owned(),
            start_identity: "start".to_owned(),
        };
        let changed = ProcessIdentity {
            start_identity: "reused".to_owned(),
            ..target.clone()
        };

        let mut same_proof = PostSignalProof::issue_from_production_hook(&target);
        same_proof.consume_for(&target).expect("consume proof once");
        let mut same_diagnostics = Vec::new();
        assert_eq!(
            consume_post_signal_proof_with_diagnostic(
                &mut same_proof,
                &target,
                "post-signal-proof",
                |stage, code| same_diagnostics.push((stage, code)),
            ),
            Err("fixture-helper-identity")
        );
        assert_eq!(
            same_diagnostics,
            [("post-signal-proof", "fixture-helper-identity")]
        );

        let mut changed_proof = PostSignalProof::issue_from_production_hook(&target);
        changed_proof
            .consume_for(&target)
            .expect("consume changed proof once");
        let mut changed_diagnostics = Vec::new();
        assert_eq!(
            consume_post_signal_proof_with_diagnostic(
                &mut changed_proof,
                &changed,
                "post-signal-proof",
                |stage, code| changed_diagnostics.push((stage, code)),
            ),
            Err("fixture-helper-identity")
        );
        assert_eq!(
            changed_diagnostics,
            [("post-signal-proof", "fixture-helper-start")]
        );
    }

    /// Drive the bounded control reader itself so a missing proof, trailing
    /// record, or malformed identity cannot be mistaken for production q.
    #[test]
    fn post_signal_proof_reader_rejects_missing_and_trailing_data() {
        let target = ProcessIdentity {
            pid: 42,
            pgid: 42,
            uid: current_uid(),
            comm: "target".to_owned(),
            start_identity: "start".to_owned(),
        };
        let wire = PostSignalProof::issue_from_production_hook(&target)
            .encode()
            .expect("proof wire");
        let mut valid_reader = Cursor::new(wire.clone());
        assert!(
            read_fixture_post_signal_proof(
                &mut valid_reader,
                Instant::now() + Duration::from_secs(1)
            )
            .is_ok()
        );
        let mut missing_reader = Cursor::new(Vec::<u8>::new());
        assert!(
            read_fixture_post_signal_proof(
                &mut missing_reader,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        let mut trailing_reader = Cursor::new([wire.as_slice(), b"extra\n"].concat());
        assert!(
            read_fixture_post_signal_proof(
                &mut trailing_reader,
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }

    /// Exercise the acknowledgement reader against real nonblocking pipes so
    /// chunking, delayed tail bytes, missing/malformed/oversized output,
    /// timeout and nonzero supervisor status cannot become a false success.
    #[cfg(target_os = "macos")]
    #[test]
    fn fixture_ack_pipe_cases_are_bounded_and_strict() {
        let cases = [
            (
                "printf 'target-reaped=true\\n'",
                true,
                Duration::from_secs(2),
            ),
            (
                "printf 'target-reaped='; /bin/sleep 0.05; printf 'true\\n'",
                true,
                Duration::from_secs(2),
            ),
            (
                "printf 'target-reaped=true\\n'; /bin/sleep 0.05; printf 'extra\\n'",
                false,
                Duration::from_secs(2),
            ),
            ("printf 'target-reaped=true'", false, Duration::from_secs(2)),
            (
                "printf 'target-reaped=false\\n'",
                false,
                Duration::from_secs(2),
            ),
            ("printf '%300s' x", false, Duration::from_secs(2)),
            ("/bin/sleep 2", false, Duration::from_millis(100)),
            (
                "printf 'target-reaped=true\\n'; exit 7",
                false,
                Duration::from_secs(2),
            ),
        ];
        for (script, expected_success, budget) in cases {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut child = spawn_grouped(&mut command).expect("spawn ack pipe fixture");
            let process_group = i32::try_from(child.id())
                .ok()
                .filter(|value| *value > 1)
                .expect("ack pipe process group");
            let stdout = child.stdout.take().expect("ack pipe stdout");
            set_nonblocking(&stdout).expect("ack pipe nonblocking");
            let mut reader = BufReader::new(stdout);
            let result =
                super::wait_fixture_reap_ack(&mut reader, &mut child, Instant::now() + budget);
            if result.is_ok() != expected_success {
                let _ = reap_child_group_bounded(
                    &mut child,
                    process_group,
                    Instant::now() + Duration::from_secs(2),
                );
                panic!("unexpected acknowledgement result for script: {script}");
            }
            if result.is_err() {
                reap_child_group_bounded(
                    &mut child,
                    process_group,
                    Instant::now() + Duration::from_secs(2),
                )
                .expect("failed ack fixture cleanup");
            } else {
                assert_eq!(group_state(process_group), ProcessState::Empty);
            }
        }
    }

    /// Exercise the production supervisor branch after the target PGID was
    /// already signalled by marker cleanup.  A second group signal may report
    /// ESRCH even though the direct Child still needs reaping; the shared
    /// helper must accept that ordering only after PGID emptiness is proven.
    #[cfg(target_os = "macos")]
    #[test]
    fn fixture_child_reap_accepts_pre_signalled_group() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_grouped(&mut command).expect("spawn pre-signalled child");
        let snapshot =
            FixtureTargetSnapshot::capture(&child).expect("capture pre-signalled target snapshot");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("pre-signalled process group");
        let identity = snapshot.identity.clone();
        let mut proof = PostSignalProof::issue_from_production_hook(&identity);
        crate::safe_signal_group(process_group, 9).expect("signal target group");
        reap_fixture_child_after_group_signal(
            &mut child,
            &mut proof,
            &snapshot,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("reap pre-signalled target");
        assert_eq!(group_state(process_group), ProcessState::Empty);
    }

    /// A PID-reused or cross-group proof must be rejected before the helper
    /// sends a direct signal; the strict group finalizer then owns cleanup.
    #[cfg(target_os = "macos")]
    #[test]
    fn post_signal_helper_rejects_wrong_proof_before_reap() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_grouped(&mut command).expect("spawn proof rejection target");
        let snapshot = FixtureTargetSnapshot::capture(&child)
            .expect("capture proof rejection target snapshot");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("proof rejection process group");
        let identity = snapshot.identity.clone();
        let wrong_identity = ProcessIdentity {
            start_identity: "reused-start".to_owned(),
            ..identity.clone()
        };
        let mut proof = PostSignalProof::issue_from_production_hook(&wrong_identity);
        assert_eq!(
            reap_fixture_child_after_group_signal(
                &mut child,
                &mut proof,
                &snapshot,
                Instant::now() + Duration::from_secs(1),
            ),
            Err("fixture-helper-identity")
        );
        reap_child_group_bounded(
            &mut child,
            process_group,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("strict cleanup after rejected proof");
    }

    /// Exercise the complete q-plus-ack phase, including control loss, ack
    /// loss and a deadline that expires before any child-side action.
    #[test]
    fn fixture_reap_handshake_faults_are_fail_closed() {
        let mut phases = Vec::new();
        assert_eq!(
            complete_fixture_reap_handshake(Instant::now() + Duration::from_secs(1), |_| {
                phases.push("q");
                phases.push("ack");
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(phases, ["q", "ack"]);
        assert_eq!(
            complete_fixture_reap_handshake(Instant::now() + Duration::from_secs(1), |_| {
                Err("fixture-helper-control")
            }),
            Err("fixture-helper-control")
        );
        assert_eq!(
            complete_fixture_reap_handshake(Instant::now() + Duration::from_secs(1), |_| {
                Err("fixture-helper-ack")
            }),
            Err("fixture-helper-ack")
        );
        assert_eq!(
            complete_fixture_reap_handshake(Instant::now(), |_| Ok(())),
            Err("marker-query-unreaped")
        );
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
