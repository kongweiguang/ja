// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Exact-errno process and process-group supervision for marker cleanup.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::{safe_signal_group, safe_signal_pid, spawn_grouped};

const ESRCH: i32 = 3;
pub(super) const EPERM: i32 = 1;
const SIGKILL: i32 = 9;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;
const QUERY_DEADLINE: Duration = Duration::from_secs(1);
const QUERY_BYTES: usize = 4096;

/// `kill(..., 0)` is classified from errno, never from locale-dependent text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProcessState {
    Empty,
    Present,
    PermissionDenied,
    Other(i32),
}

/// Capture both kernel liveness probes under one caller-owned deadline so a
/// cleaned marker is garbage-collected only after PID and PGID both report
/// the exact ESRCH state.
pub(super) fn probe_pid_group_until(
    pid: u32,
    process_group: i32,
    deadline: Instant,
) -> Result<(ProcessState, ProcessState), &'static str> {
    let pid = i32::try_from(pid).map_err(|_| "marker-owner-mismatch")?;
    if pid <= 1
        || process_group <= 1
        || u32::try_from(pid).ok() == Some(current_pid())
        || process_group == current_pgid()
    {
        return Err("marker-owner-mismatch");
    }
    let direct = pid_state_until(pid, deadline)?;
    let group = group_state_until(process_group, deadline)?;
    Ok((direct, group))
}

/// Minimal identity data used to prevent PID reuse before a group signal.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct ProcessIdentity {
    pub(super) pid: u32,
    pub(super) pgid: i32,
    pub(super) uid: u32,
    pub(super) comm: String,
    pub(super) start_identity: String,
}

/// Expose exact errno classification to the in-crate fixture and regression
/// tests, including the EPERM case that Bash `kill -0` cannot distinguish.
pub(super) fn classify_errno(value: i32) -> ProcessState {
    match value {
        ESRCH => ProcessState::Empty,
        EPERM => ProcessState::PermissionDenied,
        other => ProcessState::Other(other),
    }
}

/// Read current process-group identity without spawning a shell or parsing
/// command output; invalid values fail closed before any marker signal.
pub(super) fn current_pgid() -> i32 {
    unsafe { getpgrp() }
}

/// Return the host process identity so cleanup can never signal its own PID or
/// process group even when a forged marker supplies otherwise valid numbers.
pub(super) fn current_pid() -> u32 {
    std::process::id()
}

/// Read the current uid for descriptor-bound marker ownership checks.
pub(super) fn current_uid() -> u32 {
    unsafe { geteuid() }
}

/// Probe an exact PID or process group using kernel errno semantics.
pub(super) fn pid_state(pid: i32) -> ProcessState {
    if pid <= 1 || u32::try_from(pid).ok() == Some(current_pid()) {
        return ProcessState::Other(22);
    }
    classify_kill_result(unsafe { kill(pid, 0) })
}

/// Query process group, owner UID, name and start identity through one
/// supervised bounded `ps` invocation; the shared deadline prevents a marker
/// set from multiplying the per-query timeout.
pub(super) fn query_identity(pid: u32) -> Result<ProcessIdentity, &'static str> {
    query_identity_until(pid, Instant::now() + QUERY_DEADLINE)
}

/// Query one identity without extending a caller-owned cleanup deadline.
pub(super) fn query_identity_until(
    pid: u32,
    deadline: Instant,
) -> Result<ProcessIdentity, &'static str> {
    if pid <= 1 || pid == current_pid() {
        return Err("marker-owner-mismatch");
    }
    if Instant::now() >= deadline {
        return Err("marker-query-unreaped");
    }
    let mut command = Command::new("/bin/ps");
    let pid_argument = pid.to_string();
    command
        .args(["-o", "uid=,pgid=,comm=,lstart=", "-p", &pid_argument])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_grouped(&mut command).map_err(|_| "marker-owner-mismatch")?;
    // macOS PIDs are bounded by pid_t/i32; validate the conversion and reserve
    // PID/group values before any negative process-group operation is formed.
    let process_group = match i32::try_from(child.id()).ok().filter(|value| *value > 1) {
        Some(process_group) if process_group != current_pgid() => process_group,
        Some(_) | None => {
            if !reap_child_without_group_until(&mut child, deadline) {
                abort_unreaped_query("invalid-process-group");
            }
            return Err("marker-owner-mismatch");
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return query_failure(&mut child, process_group, deadline),
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return query_failure(&mut child, process_group, deadline),
    };
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        drop(stdout);
        drop(stderr);
        return query_failure(&mut child, process_group, deadline);
    }
    let mut output = Vec::new();
    let mut stdout_bytes = 0;
    let mut stderr_bytes = 0;
    let mut pipes_ok = true;
    let mut status = None;
    while Instant::now() < deadline {
        pipes_ok &= drain(&mut stdout, Some(&mut output), &mut stdout_bytes, deadline);
        pipes_ok &= drain(&mut stderr, None, &mut stderr_bytes, deadline);
        match child.try_wait() {
            Ok(Some(value)) => {
                status = Some(value);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    pipes_ok &= drain(&mut stdout, Some(&mut output), &mut stdout_bytes, deadline);
    pipes_ok &= drain(&mut stderr, None, &mut stderr_bytes, deadline);
    drop(stdout);
    drop(stderr);
    let status = match status {
        Some(status) => status,
        None => return query_failure(&mut child, process_group, deadline),
    };
    if !status.success() || !pipes_ok {
        return query_failure(&mut child, process_group, deadline);
    }
    let identity = terminate_query(&mut child, process_group, Some(output), deadline)?;
    let mut identity = parse_identity(&identity)?;
    identity.pid = pid_from_argument(&pid_argument)?;
    Ok(identity)
}

/// Parse the already validated decimal PID used by the identity helper so the
/// capture includes the numeric owner as well as start/comm/PGID fields.
fn pid_from_argument(value: &str) -> Result<u32, &'static str> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value > 1)
        .ok_or("marker-owner-mismatch")
}

/// Reap a fixture launcher with an absolute deadline; a failed read must not
/// leave a live direct child behind merely because the normal marker path was
/// never reached.  The shared finalizer aborts if ownership cannot be proven
/// safe, so every caller may propagate only after the Child is reaped.
#[cfg(target_os = "macos")]
pub(super) fn reap_child_bounded(
    child: &mut Child,
    process_group: i32,
    deadline: Instant,
) -> Result<(), &'static str> {
    finalize_child(child, process_group, deadline, "fixture-unreaped")?;
    require_group_empty(process_group, deadline)
}

/// Kill the freshly spawned child's complete process group when identity
/// inspection failed before a trusted `ProcessIdentity` existed.  The live
/// `Child` handle pins its PID against reuse, while strict PGID validation and
/// bounded direct reap prevent an early query fault from orphaning descendants.
#[cfg(target_os = "macos")]
pub(super) fn reap_child_group_bounded(
    child: &mut Child,
    process_group: i32,
    deadline: Instant,
) -> Result<(), &'static str> {
    if process_group <= 1 || process_group == current_pgid() {
        return Err("marker-owner-mismatch");
    }
    if Instant::now() >= deadline {
        return Err("marker-residual");
    }
    let signal_result = signal_group(process_group, deadline);
    let reap_result = finalize_child(child, process_group, deadline, "fixture-unreaped");
    let group_result = require_group_empty(process_group, deadline);
    match (signal_result, reap_result, group_result) {
        (Ok(_), Ok(()), Ok(())) => Ok(()),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => Err(error),
    }
}

/// Reap a failed query child before returning a fixed identity error; this
/// prevents an early pipe/status branch from dropping a live supervisor.
fn query_failure(
    child: &mut Child,
    process_group: i32,
    deadline: Instant,
) -> Result<ProcessIdentity, &'static str> {
    match terminate_query(child, process_group, None, deadline) {
        Ok(_) => Err("marker-owner-mismatch"),
        Err(category) => Err(category),
    }
}

/// Reap a query child without ever forming a dangerous negative process-group
/// target when the kernel returns an invalid/reserved child PID.
pub(super) fn reap_child_without_group(child: &mut Child) -> bool {
    reap_child_without_group_until(child, Instant::now() + QUERY_DEADLINE)
}

/// Reap a child without creating a fresh timeout window for a caller-owned
/// cleanup operation whose global deadline has already been established.
pub(super) fn reap_child_without_group_until(child: &mut Child, deadline: Instant) -> bool {
    let _ = child.kill();
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
    false
}

/// Kill the exact group and direct PID, then require both to report ESRCH
/// before allowing sibling marker deletion.
pub(super) fn terminate_group(
    captured: &ProcessIdentity,
    deadline: Instant,
) -> Result<(), &'static str> {
    validate_captured_identity(captured, deadline)?;
    let group_signal = signal_group_captured(captured, deadline)?;
    if group_signal == ProcessState::Empty {
        // The group disappearing before the direct signal is a PID-reuse
        // boundary: never send a signal to a new process that inherited the
        // old numeric PID after the validated group vanished.
        return match pid_state_until(
            i32::try_from(captured.pid).map_err(|_| "marker-owner-mismatch")?,
            deadline,
        )? {
            ProcessState::Empty => Ok(()),
            ProcessState::PermissionDenied => Err("marker-eperm"),
            ProcessState::Present => Err("marker-identity-lost"),
            ProcessState::Other(_) => Err("marker-process-probe-failed"),
        };
    }
    let direct = pid_state_until(
        i32::try_from(captured.pid).map_err(|_| "marker-owner-mismatch")?,
        deadline,
    )?;
    let group = group_state_until(captured.pgid, deadline)?;
    match (direct, group) {
        (ProcessState::Empty, ProcessState::Empty) => return Ok(()),
        (ProcessState::PermissionDenied, _) | (_, ProcessState::PermissionDenied) => {
            return Err("marker-eperm");
        }
        (ProcessState::Other(_), _) | (_, ProcessState::Other(_)) => {
            return Err("marker-process-probe-failed");
        }
        (ProcessState::Empty, ProcessState::Present) => {
            // The validated group signal already covers the descendant tree;
            // a direct target can become a zombie before its parent reaps it.
            // Wait for the group to disappear without ever signalling an
            // unqueryable or potentially reused direct PID.
            return wait_after_group_signal(captured, deadline);
        }
        (ProcessState::Present, ProcessState::Empty) => return Err("marker-identity-lost"),
        (ProcessState::Present, ProcessState::Present) => {}
    }
    // The group signal may have raced with exit/reuse.  A fresh full identity
    // query is mandatory before direct PID signalling can be attempted.
    if validate_captured_identity(captured, deadline).is_err() {
        return wait_after_group_signal(captured, deadline);
    }
    signal_pid(captured.pid, deadline)?;
    loop {
        let direct = pid_state_until(
            i32::try_from(captured.pid).map_err(|_| "marker-owner-mismatch")?,
            deadline,
        )?;
        let group = group_state_until(captured.pgid, deadline)?;
        match (direct, group) {
            (ProcessState::Empty, ProcessState::Empty) => return Ok(()),
            (ProcessState::PermissionDenied, _) | (_, ProcessState::PermissionDenied) => {
                return Err("marker-eperm");
            }
            (ProcessState::Other(_), _) | (_, ProcessState::Other(_)) => {
                return Err("marker-process-probe-failed");
            }
            _ if Instant::now() >= deadline => return Err("marker-residual"),
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Recover a group-signal `EPERM` only with a complete, caller-supplied set of
/// identity-checked members.  This is deliberately fail-closed for an
/// incomplete set: every known PID must disappear and the kernel must also
/// report the PGID empty, so an unknown same-group process can never be
/// mistaken for successful cleanup.
pub(super) fn terminate_group_with_identity_fallback(
    captured: &ProcessIdentity,
    peers: &[ProcessIdentity],
    deadline: Instant,
) -> Result<(), &'static str> {
    match terminate_group(captured, deadline) {
        Ok(()) => Ok(()),
        Err("marker-eperm") => {
            let mut targets: Vec<&ProcessIdentity> = Vec::with_capacity(peers.len());
            for peer in peers {
                if peer.pid <= 1
                    || peer.pgid <= 1
                    || peer.pid == current_pid()
                    || peer.pgid == current_pgid()
                    || peer.pgid != captured.pgid
                    || peer.uid != current_uid()
                {
                    return Err("marker-owner-mismatch");
                }
                if let Some(existing) = targets.iter().find(|target| target.pid == peer.pid) {
                    if *existing != peer {
                        return Err("marker-identity-lost");
                    }
                    continue;
                }
                targets.push(peer);
            }
            if !targets.iter().any(|target| target.pid == captured.pid) {
                return Err("marker-owner-mismatch");
            }
            // An EPERM group probe may already have killed a member.  Query
            // each remaining member before its direct signal, and never use a
            // stale numeric PID merely because it appeared in a marker.
            for target in &targets {
                let pid = i32::try_from(target.pid).map_err(|_| "marker-owner-mismatch")?;
                match pid_state_until(pid, deadline)? {
                    ProcessState::Empty => {}
                    ProcessState::PermissionDenied => return Err("marker-eperm"),
                    ProcessState::Other(_) => return Err("marker-process-probe-failed"),
                    ProcessState::Present => {
                        validate_captured_identity(target, deadline)?;
                        signal_pid(target.pid, deadline)?;
                    }
                }
            }
            wait_after_identity_group(&targets, captured.pgid, deadline)
        }
        Err(error) => Err(error),
    }
}

/// Wait for every trusted member and the complete group to disappear after a
/// direct fallback.  The group check remains mandatory because the marker set
/// may not describe an untrusted extra descendant.
fn wait_after_identity_group(
    targets: &[&ProcessIdentity],
    process_group: i32,
    deadline: Instant,
) -> Result<(), &'static str> {
    loop {
        let mut all_direct_empty = true;
        for target in targets {
            match pid_state_until(
                i32::try_from(target.pid).map_err(|_| "marker-owner-mismatch")?,
                deadline,
            )? {
                ProcessState::Empty => {}
                ProcessState::PermissionDenied => return Err("marker-eperm"),
                ProcessState::Other(_) => return Err("marker-process-probe-failed"),
                ProcessState::Present => all_direct_empty = false,
            }
        }
        match group_state_until(process_group, deadline)? {
            ProcessState::Empty if all_direct_empty => return Ok(()),
            ProcessState::PermissionDenied => return Err("marker-eperm"),
            ProcessState::Other(_) => return Err("marker-process-probe-failed"),
            ProcessState::Empty => return Err("marker-identity-lost"),
            ProcessState::Present if Instant::now() >= deadline => return Err("marker-residual"),
            ProcessState::Present => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// After a validated group SIGKILL, wait only for kernel disappearance.  This
/// avoids a stale direct-PID signal when macOS briefly exposes a zombie or the
/// identity helper cannot observe a just-killed process.
fn wait_after_group_signal(
    captured: &ProcessIdentity,
    deadline: Instant,
) -> Result<(), &'static str> {
    loop {
        let direct = pid_state_until(
            i32::try_from(captured.pid).map_err(|_| "marker-owner-mismatch")?,
            deadline,
        )?;
        let group = group_state_until(captured.pgid, deadline)?;
        match (direct, group) {
            (ProcessState::Empty, ProcessState::Empty) => return Ok(()),
            (ProcessState::PermissionDenied, _) | (_, ProcessState::PermissionDenied) => {
                return Err("marker-eperm");
            }
            (ProcessState::Other(_), _) | (_, ProcessState::Other(_)) => {
                return Err("marker-process-probe-failed");
            }
            (ProcessState::Present, ProcessState::Empty) => return Err("marker-identity-lost"),
            _ if Instant::now() >= deadline => return Err("marker-residual"),
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Re-query every immutable field captured before cleanup; a PID or PGID
/// mismatch is an identity-loss failure and never authorizes a signal.
fn validate_captured_identity(
    captured: &ProcessIdentity,
    deadline: Instant,
) -> Result<(), &'static str> {
    if captured.pid <= 1
        || captured.pgid <= 1
        || captured.pid == current_pid()
        || captured.pgid == current_pgid()
        || captured.uid != current_uid()
    {
        return Err("marker-owner-mismatch");
    }
    let current =
        query_identity_until(captured.pid, deadline).map_err(|_| "marker-identity-lost")?;
    if !same_captured_identity(captured, &current) {
        return Err("marker-identity-lost");
    }
    Ok(())
}

/// Signal a captured identity's group only after the complete identity has
/// been refreshed immediately before the kernel operation.
fn signal_group_captured(
    captured: &ProcessIdentity,
    deadline: Instant,
) -> Result<ProcessState, &'static str> {
    validate_captured_identity(captured, deadline)?;
    signal_group(captured.pgid, deadline)
}

/// Keep the direct Child alive until it is reaped, then require its captured
/// group to disappear; this helper deliberately sends no stale group signal.
fn require_group_empty(process_group: i32, deadline: Instant) -> Result<(), &'static str> {
    if process_group <= 1 || process_group == current_pgid() {
        return Err("marker-owner-mismatch");
    }
    loop {
        match group_state_until(process_group, deadline)? {
            ProcessState::Empty => return Ok(()),
            ProcessState::PermissionDenied => return Err("marker-eperm"),
            ProcessState::Other(_) => return Err("marker-process-probe-failed"),
            ProcessState::Present if Instant::now() >= deadline => return Err("marker-residual"),
            ProcessState::Present => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Keep a pure group state machine injectable for regression tests; production
/// group signals go only through `terminate_group`'s captured identity path.
#[cfg(test)]
trait GroupCleanupBackend {
    fn signal(&mut self) -> Result<ProcessState, &'static str>;
    fn state(&mut self) -> ProcessState;
}

/// Require a group to report empty within a bounded window; every non-empty
/// terminal state is a fixed failure and can never be mistaken for success.
#[cfg(test)]
fn terminate_group_backend<B: GroupCleanupBackend>(
    backend: &mut B,
    deadline: Instant,
) -> Result<(), &'static str> {
    backend.signal()?;
    loop {
        match backend.state() {
            ProcessState::Empty => return Ok(()),
            ProcessState::PermissionDenied => return Err("marker-eperm"),
            ProcessState::Other(_) => return Err("marker-process-probe-failed"),
            ProcessState::Present if Instant::now() >= deadline => return Err("marker-residual"),
            ProcessState::Present => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Probe a process group exactly; `PermissionDenied` is never treated as gone.
pub(super) fn group_state(process_group: i32) -> ProcessState {
    if process_group <= 1 || process_group == current_pgid() {
        return ProcessState::Other(22);
    }
    classify_kill_result(unsafe { kill(-process_group, 0) })
}

/// Poll one PID while refusing to report a terminal state after the shared
/// deadline; a late `ESRCH` must not turn an over-budget cleanup into success.
fn pid_state_until(pid: i32, deadline: Instant) -> Result<ProcessState, &'static str> {
    if Instant::now() >= deadline {
        return Err("marker-residual");
    }
    let state = pid_state(pid);
    if Instant::now() >= deadline {
        Err("marker-residual")
    } else {
        Ok(state)
    }
}

/// Poll one PGID under the same before/after deadline gate used by direct PID
/// checks, so group disappearance is not accepted after the budget expires.
fn group_state_until(process_group: i32, deadline: Instant) -> Result<ProcessState, &'static str> {
    if Instant::now() >= deadline {
        return Err("marker-residual");
    }
    let state = group_state(process_group);
    if Instant::now() >= deadline {
        Err("marker-residual")
    } else {
        Ok(state)
    }
}

/// Send SIGKILL only to a previously validated exact process group.
fn signal_group(process_group: i32, deadline: Instant) -> Result<ProcessState, &'static str> {
    if process_group <= 1 || process_group == current_pgid() {
        return Err("marker-owner-mismatch");
    }
    if Instant::now() >= deadline {
        return Err("marker-residual");
    }
    let result = match safe_signal_group(process_group, SIGKILL) {
        Ok(()) => Ok(ProcessState::Present),
        Err(error) if error.raw_os_error() == Some(ESRCH) => Ok(ProcessState::Empty),
        Err(error) if error.raw_os_error() == Some(EPERM) => Err("marker-eperm"),
        Err(_) => Err("marker-signal-failed"),
    };
    if Instant::now() >= deadline {
        Err("marker-residual")
    } else {
        result
    }
}

/// Send SIGKILL only to the validated direct PID after the group signal.
fn signal_pid(pid: u32, deadline: Instant) -> Result<(), &'static str> {
    let pid = i32::try_from(pid).map_err(|_| "marker-owner-mismatch")?;
    if pid <= 1 || u32::try_from(pid).ok() == Some(current_pid()) {
        return Err("marker-owner-mismatch");
    }
    if Instant::now() >= deadline {
        return Err("marker-residual");
    }
    let result = match safe_signal_pid(pid, SIGKILL) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(ESRCH) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EPERM) => Err("marker-eperm"),
        Err(_) => Err("marker-signal-failed"),
    };
    if Instant::now() >= deadline {
        Err("marker-residual")
    } else {
        result
    }
}

/// Classify the raw kill result using Darwin errno rather than shell output.
fn classify_kill_result(result: i32) -> ProcessState {
    if result == 0 {
        ProcessState::Present
    } else {
        classify_errno(unsafe { *__error() })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryPoll {
    Reaped,
    Running,
    Error,
}

/// Isolate the OS polling boundary so try_wait errors and unreaped states can
/// be regression-tested without ever transferring Child ownership to Drop.
trait QueryChildBackend {
    fn poll(&mut self) -> QueryPoll;
    fn force_kill(&mut self);
}

/// Borrowed production adapter keeps the real Child in its caller's scope so
/// an unresolved fault cannot be silently consumed by a temporary wrapper.
struct RealQueryChild<'a> {
    child: &'a mut Child,
}

impl QueryChildBackend for RealQueryChild<'_> {
    fn poll(&mut self) -> QueryPoll {
        match self.child.try_wait() {
            Ok(Some(_)) => QueryPoll::Reaped,
            Ok(None) => QueryPoll::Running,
            Err(_) => QueryPoll::Error,
        }
    }

    fn force_kill(&mut self) {
        // The helper's captured group is only observed after direct reap; a
        // stale group signal here could target a PID-reused process, so the
        // production recovery path kills the owned Child directly and then
        // fails closed if its group does not disappear.
        let _ = self.child.kill();
    }
}

/// Keep a Child-owned backend through every bounded poll; callers may only
/// return after `Reaped`, otherwise they must enter the explicit abort path.
#[cfg(test)]
fn bounded_reap_backend<B: QueryChildBackend>(
    backend: &mut B,
    deadline: Instant,
    retry_window: Duration,
) -> bool {
    bounded_reap_backend_until(backend, deadline, Instant::now() + retry_window)
}

/// Reap using an absolute retry deadline so a caller-owned global budget is
/// never extended by a second cleanup phase after a poll error or timeout.
fn bounded_reap_backend_until<B: QueryChildBackend>(
    backend: &mut B,
    deadline: Instant,
    retry_deadline: Instant,
) -> bool {
    loop {
        match backend.poll() {
            QueryPoll::Reaped => return true,
            QueryPoll::Running if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            QueryPoll::Running | QueryPoll::Error => break,
        }
    }
    backend.force_kill();
    loop {
        match backend.poll() {
            QueryPoll::Reaped => return true,
            QueryPoll::Running if Instant::now() < retry_deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            QueryPoll::Running | QueryPoll::Error => return false,
        }
    }
}

/// Keep the direct Child borrowed across both bounded cleanup attempts; the
/// shared path is used by query and fixture launchers so no error branch can
/// release live ownership to `Drop`.
fn finalize_child(
    child: &mut Child,
    process_group: i32,
    deadline: Instant,
    reason: &'static str,
) -> Result<(), &'static str> {
    if process_group <= 1 || process_group == current_pgid() {
        if !reap_child_without_group_until(child, deadline) {
            abort_unreaped_query(reason);
        }
        // A reaped child is not enough when its group identity is unsafe: a
        // caller must not continue into a path that could leave descendants
        // unowned, so fail closed after the direct child is known gone.
        abort_unreaped_query("group-unsafe");
    }
    let mut backend = RealQueryChild { child };
    if !bounded_reap_backend_until(&mut backend, deadline, deadline) {
        abort_unreaped_query(reason);
    }
    Ok(())
}

/// Abort only after repeated bounded cleanup cannot prove direct-child reap;
/// returning would drop a possibly live Child and hide the ownership failure.
pub(super) fn abort_unreaped_query(reason: &'static str) -> ! {
    eprintln!("SANDBOX-MARKER-CLEANUP: query={reason}");
    let _ = std::io::stderr().flush();
    std::process::abort();
}

/// Reap the bounded identity-query child and require its private group gone.
fn terminate_query(
    child: &mut Child,
    process_group: i32,
    output: Option<Vec<u8>>,
    deadline: Instant,
) -> Result<Vec<u8>, &'static str> {
    finalize_child(child, process_group, deadline, "query-unreaped")?;
    require_group_empty(process_group, deadline).map_err(|_| "marker-query-group-residual")?;
    output.ok_or("marker-owner-mismatch")
}

/// Parse only fixed `ps` columns; any unexpected shape is an owner mismatch.
fn parse_identity(output: &[u8]) -> Result<ProcessIdentity, &'static str> {
    let text = std::str::from_utf8(output).map_err(|_| "marker-owner-mismatch")?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 8 {
        return Err("marker-owner-mismatch");
    }
    let uid = fields[0]
        .parse::<u32>()
        .ok()
        .filter(|value| *value == current_uid())
        .ok_or("marker-owner-mismatch")?;
    let pgid = fields[1]
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 1)
        .ok_or("marker-owner-mismatch")?;
    let comm = fields[2].to_owned();
    let start_identity = fields[3..].join(" ");
    Ok(ProcessIdentity {
        // The query caller fills the already validated target PID after the
        // helper output is parsed, so an untrusted `ps` row cannot choose it.
        pid: 0,
        pgid,
        uid,
        comm,
        start_identity,
    })
}

/// Compare every captured identity field before a signal; this pure boundary
/// makes PID-reuse and deterministic fault cases testable without sending a
/// signal to a real process.
fn same_captured_identity(expected: &ProcessIdentity, current: &ProcessIdentity) -> bool {
    expected.pid > 1
        && expected.pgid > 1
        && expected.pid != current_pid()
        && expected.pgid != current_pgid()
        && expected.uid == current_uid()
        && current.pid == expected.pid
        && current.pgid == expected.pgid
        && current.uid == expected.uid
        && current.comm == expected.comm
        && current.start_identity == expected.start_identity
}

/// Read only currently available bytes from a nonblocking identity pipe and
/// discard stderr after the same hard cap to prevent a noisy query deadlock.
fn drain<R: Read>(
    reader: &mut R,
    mut output: Option<&mut Vec<u8>>,
    consumed: &mut usize,
    deadline: Instant,
) -> bool {
    let mut buffer = [0_u8; 256];
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        match reader.read(&mut buffer) {
            Ok(0) => return true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
            Ok(read) => {
                *consumed = consumed.saturating_add(read);
                if *consumed > QUERY_BYTES {
                    if let Some(output) = &mut output {
                        output.clear();
                    }
                    return false;
                }
                if let Some(output) = &mut output {
                    output.extend_from_slice(&buffer[..read]);
                }
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[cfg(target_os = "macos")]
    use std::process::{Command, Stdio};

    struct FakeQueryBackend {
        polls: VecDeque<QueryPoll>,
        killed: bool,
    }

    impl QueryChildBackend for FakeQueryBackend {
        fn poll(&mut self) -> QueryPoll {
            self.polls.pop_front().unwrap_or(QueryPoll::Running)
        }

        fn force_kill(&mut self) {
            self.killed = true;
        }
    }

    /// Prove a try_wait-like error first forces cleanup while retaining the
    /// backend, then permits return only after a later reap is observed.
    #[test]
    fn query_backend_error_keeps_ownership_until_reaped() {
        let mut backend = FakeQueryBackend {
            polls: VecDeque::from([QueryPoll::Error, QueryPoll::Reaped]),
            killed: false,
        };
        assert!(bounded_reap_backend(
            &mut backend,
            Instant::now(),
            Duration::ZERO
        ));
        assert!(backend.killed);
    }

    /// Prove an unreaped backend remains a failed state after bounded retry;
    /// production then enters `abort_unreaped_query` instead of dropping a
    /// live Child, while the test avoids invoking the process abort itself.
    #[test]
    fn query_backend_unreaped_is_not_treated_as_success() {
        let mut backend = FakeQueryBackend {
            polls: VecDeque::from([QueryPoll::Running, QueryPoll::Running]),
            killed: false,
        };
        assert!(!bounded_reap_backend(
            &mut backend,
            Instant::now(),
            Duration::ZERO
        ));
        assert!(backend.killed);
    }

    /// Prove a try_wait-style error after the first forced kill remains a
    /// fail-closed result instead of being mistaken for a successful reap.
    #[test]
    fn query_backend_error_after_kill_is_not_released() {
        let mut backend = FakeQueryBackend {
            polls: VecDeque::from([QueryPoll::Running, QueryPoll::Error]),
            killed: false,
        };
        assert!(!bounded_reap_backend(
            &mut backend,
            Instant::now(),
            Duration::ZERO
        ));
        assert!(backend.killed);
    }

    /// Reserved direct and group identifiers are classified without crossing
    /// the signal boundary, protecting the runner from process-wide targets.
    #[test]
    fn reserved_probe_ids_are_not_clean() {
        for value in [-1, 0, 1] {
            assert_eq!(pid_state(value), ProcessState::Other(22));
            assert_eq!(group_state(value), ProcessState::Other(22));
        }
    }

    /// A deterministic PID-reuse table proves that every changed identity
    /// field blocks the signal boundary before any OS call is attempted.
    #[test]
    fn captured_identity_reuse_is_rejected() {
        let captured = ProcessIdentity {
            pid: 42,
            pgid: 43,
            uid: current_uid(),
            comm: "ja-sandbox-worker".to_owned(),
            start_identity: "Mon Jan 1 00:00:00 2026".to_owned(),
        };
        let cases = [
            ProcessIdentity {
                pid: 41,
                ..captured.clone()
            },
            ProcessIdentity {
                pgid: 44,
                ..captured.clone()
            },
            ProcessIdentity {
                comm: "unrelated".to_owned(),
                ..captured.clone()
            },
            ProcessIdentity {
                start_identity: "Tue Jan 2 00:00:00 2026".to_owned(),
                ..captured.clone()
            },
            ProcessIdentity {
                uid: current_uid().saturating_add(1),
                ..captured.clone()
            },
        ];
        for reused in cases {
            assert!(!same_captured_identity(&captured, &reused));
        }
        assert!(same_captured_identity(&captured, &captured));
        assert!(!same_captured_identity(
            &ProcessIdentity {
                pid: current_pid(),
                ..captured.clone()
            },
            &captured
        ));
        assert!(!same_captured_identity(
            &ProcessIdentity {
                pgid: current_pgid(),
                ..captured.clone()
            },
            &captured
        ));
    }

    struct FakeGroupBackend {
        signal_result: Result<ProcessState, &'static str>,
        states: VecDeque<ProcessState>,
        signalled: bool,
    }

    impl GroupCleanupBackend for FakeGroupBackend {
        fn signal(&mut self) -> Result<ProcessState, &'static str> {
            self.signalled = true;
            self.signal_result
        }

        fn state(&mut self) -> ProcessState {
            self.states.pop_front().unwrap_or(ProcessState::Other(5))
        }
    }

    /// Prove all group terminal faults use fixed categories while the genuine
    /// empty state remains successful; this table drives fixture early exits.
    #[test]
    fn group_backend_faults_are_fail_closed() {
        let cases = [
            (
                "residual",
                Ok(ProcessState::Present),
                vec![ProcessState::Present],
                Err("marker-residual"),
            ),
            (
                "eperm-signal",
                Err("marker-eperm"),
                Vec::new(),
                Err("marker-eperm"),
            ),
            (
                "eperm-state",
                Ok(ProcessState::Present),
                vec![ProcessState::PermissionDenied],
                Err("marker-eperm"),
            ),
            (
                "other-signal",
                Err("marker-process-probe-failed"),
                Vec::new(),
                Err("marker-process-probe-failed"),
            ),
            (
                "other-state",
                Ok(ProcessState::Present),
                vec![ProcessState::Other(5)],
                Err("marker-process-probe-failed"),
            ),
            (
                "empty",
                Ok(ProcessState::Present),
                vec![ProcessState::Empty],
                Ok(()),
            ),
        ];
        for (_name, signal_result, states, expected) in cases {
            let mut backend = FakeGroupBackend {
                signal_result,
                states: VecDeque::from(states),
                signalled: false,
            };
            assert_eq!(
                terminate_group_backend(&mut backend, Instant::now()),
                expected
            );
            assert!(backend.signalled);
        }
    }

    /// Prove the production adapter keeps a real child owned until the
    /// bounded kill/reap path observes `try_wait` as terminal; this catches a
    /// regression where an unresolved Child could be released to `Drop`.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_query_child_is_reaped_after_forced_kill() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_grouped(&mut command).expect("spawn bounded query fixture");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("fixture process group");
        let mut backend = RealQueryChild { child: &mut child };
        assert!(bounded_reap_backend(
            &mut backend,
            Instant::now(),
            Duration::from_secs(1)
        ));
        assert_eq!(group_state(process_group), ProcessState::Empty);
    }

    /// Exercise the identity-query fallback with a real descendant: the
    /// anchored Child permits a group kill even before a trusted ps identity
    /// exists, and success still requires direct reap plus group emptiness.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_group_reap_cleans_descendant_without_identity() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "/bin/sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = spawn_grouped(&mut command).expect("spawn descendant fixture");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("fixture process group");
        assert!(
            reap_child_group_bounded(
                &mut child,
                process_group,
                Instant::now() + Duration::from_secs(1)
            )
            .is_ok()
        );
        assert_eq!(group_state(process_group), ProcessState::Empty);
    }
}

/// Set a pipe nonblocking before any bounded reader loop.
pub(super) fn set_nonblocking<R: AsRawFd>(reader: &R) -> io::Result<()> {
    let flags = unsafe { fcntl(reader.as_raw_fd(), F_GETFL) };
    if flags == -1 || unsafe { fcntl(reader.as_raw_fd(), F_SETFL, flags | O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn __error() -> *mut i32;
    fn getpgrp() -> i32;
    fn geteuid() -> u32;
}
