// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Exact-errno process and process-group supervision for marker cleanup.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

/// Minimal identity data used to prevent PID reuse before a group signal.
#[derive(Debug)]
pub(super) struct ProcessIdentity {
    pub(super) pgid: i32,
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

/// Read the current uid for descriptor-bound marker ownership checks.
pub(super) fn current_uid() -> u32 {
    unsafe { geteuid() }
}

/// Probe an exact PID or process group using kernel errno semantics.
pub(super) fn pid_state(pid: i32) -> ProcessState {
    classify_kill_result(unsafe { kill(pid, 0) })
}

/// Query process group/name/start identity through a supervised bounded `ps`.
pub(super) fn query_identity(pid: u32) -> Result<ProcessIdentity, &'static str> {
    let mut child = Command::new("/bin/ps")
        .args(["-o", "pgid=,comm=,lstart=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| "marker-owner-mismatch")?;
    // macOS PIDs are bounded by pid_t/i32; validate the conversion and reserve
    // PID/group values before any negative process-group operation is formed.
    let process_group = match i32::try_from(child.id()).ok().filter(|value| *value > 1) {
        Some(process_group) => process_group,
        None => {
            if !reap_child_without_group(&mut child) {
                abort_unreaped_query("invalid-process-group");
            }
            return Err("marker-owner-mismatch");
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return query_failure(&mut child, process_group),
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return query_failure(&mut child, process_group),
    };
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        drop(stdout);
        drop(stderr);
        return query_failure(&mut child, process_group);
    }
    let deadline = Instant::now() + QUERY_DEADLINE;
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
        None => return query_failure(&mut child, process_group),
    };
    if !status.success() || !pipes_ok {
        return query_failure(&mut child, process_group);
    }
    let identity = terminate_query(&mut child, process_group, Some(output))?;
    parse_identity(&identity)
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
    finalize_child(child, process_group, deadline, "fixture-unreaped")
}

/// Reap a failed query child before returning a fixed identity error; this
/// prevents an early pipe/status branch from dropping a live supervisor.
fn query_failure(child: &mut Child, process_group: i32) -> Result<ProcessIdentity, &'static str> {
    match terminate_query(child, process_group, None) {
        Ok(_) => Err("marker-owner-mismatch"),
        Err(category) => Err(category),
    }
}

/// Reap a query child without ever forming a dangerous negative process-group
/// target when the kernel returns an invalid/reserved child PID.
pub(super) fn reap_child_without_group(child: &mut Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + QUERY_DEADLINE;
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
    pid: u32,
    process_group: i32,
    deadline: Instant,
) -> Result<(), &'static str> {
    let group_signal = signal_group(process_group)?;
    if group_signal == ProcessState::Empty {
        // The group disappearing before the direct signal is a PID-reuse
        // boundary: never send a signal to a new process that inherited the
        // old numeric PID after the validated group vanished.
        return match pid_state(i32::try_from(pid).map_err(|_| "marker-owner-mismatch")?) {
            ProcessState::Empty => Ok(()),
            ProcessState::PermissionDenied => Err("marker-eperm"),
            ProcessState::Present => Err("marker-owner-mismatch"),
            ProcessState::Other(_) => Err("marker-process-probe-failed"),
        };
    }
    signal_pid(pid)?;
    loop {
        let direct = pid_state(i32::try_from(pid).map_err(|_| "marker-owner-mismatch")?);
        let group = group_state(process_group);
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

/// Finish cleanup when the direct launcher has already been reaped but a
/// descendant group remains; no direct PID signal is attempted in this path.
#[cfg(target_os = "macos")]
pub(super) fn terminate_group_only(
    process_group: i32,
    deadline: Instant,
) -> Result<(), &'static str> {
    let mut backend = RealGroupCleanupBackend { process_group };
    terminate_group_backend(&mut backend, deadline)
}

/// Keep group cleanup injectable so residual, permission and kernel-probe
/// failures are tested through the same state machine used by real fixtures.
trait GroupCleanupBackend {
    fn signal(&mut self) -> Result<ProcessState, &'static str>;
    fn state(&mut self) -> ProcessState;
}

/// Production adapter for exact process-group signal and liveness probes.
struct RealGroupCleanupBackend {
    process_group: i32,
}

impl GroupCleanupBackend for RealGroupCleanupBackend {
    fn signal(&mut self) -> Result<ProcessState, &'static str> {
        signal_group(self.process_group)
    }

    fn state(&mut self) -> ProcessState {
        group_state(self.process_group)
    }
}

/// Require a group to report empty within a bounded window; every non-empty
/// terminal state is a fixed failure and can never be mistaken for success.
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
    pid_state(-process_group)
}

/// Send SIGKILL only to a previously validated exact process group.
fn signal_group(process_group: i32) -> Result<ProcessState, &'static str> {
    match classify_kill_result(unsafe { kill(-process_group, SIGKILL) }) {
        ProcessState::Empty => Ok(ProcessState::Empty),
        ProcessState::Present => Ok(ProcessState::Present),
        ProcessState::PermissionDenied => Err("marker-eperm"),
        ProcessState::Other(_) => Err("marker-signal-failed"),
    }
}

/// Send SIGKILL only to the validated direct PID after the group signal.
fn signal_pid(pid: u32) -> Result<(), &'static str> {
    let pid = i32::try_from(pid).map_err(|_| "marker-owner-mismatch")?;
    match classify_kill_result(unsafe { kill(pid, SIGKILL) }) {
        ProcessState::Empty | ProcessState::Present => Ok(()),
        ProcessState::PermissionDenied => Err("marker-eperm"),
        ProcessState::Other(_) => Err("marker-signal-failed"),
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
    process_group: i32,
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
        let _ = unsafe { kill(-self.process_group, SIGKILL) };
        let _ = self.child.kill();
    }
}

/// Keep a Child-owned backend through every bounded poll; callers may only
/// return after `Reaped`, otherwise they must enter the explicit abort path.
fn bounded_reap_backend<B: QueryChildBackend>(
    backend: &mut B,
    deadline: Instant,
    retry_window: Duration,
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
    let retry_deadline = Instant::now() + retry_window;
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
    if process_group <= 1 {
        if !reap_child_without_group(child) {
            abort_unreaped_query(reason);
        }
        // A reaped child is not enough when its group identity is unsafe: a
        // caller must not continue into a path that could leave descendants
        // unowned, so fail closed after the direct child is known gone.
        abort_unreaped_query("group-unsafe");
    }
    let mut backend = RealQueryChild {
        child,
        process_group,
    };
    if !bounded_reap_backend(&mut backend, deadline, QUERY_DEADLINE)
        && !bounded_reap_backend(&mut backend, Instant::now(), QUERY_DEADLINE)
    {
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
) -> Result<Vec<u8>, &'static str> {
    finalize_child(
        child,
        process_group,
        Instant::now() + QUERY_DEADLINE,
        "query-unreaped",
    )?;
    let group_deadline = Instant::now() + QUERY_DEADLINE;
    while group_state(process_group) != ProcessState::Empty && Instant::now() < group_deadline {
        let _ = unsafe { kill(-process_group, SIGKILL) };
        thread::sleep(Duration::from_millis(5));
    }
    if group_state(process_group) != ProcessState::Empty {
        return Err("marker-query-group-residual");
    }
    output.ok_or("marker-owner-mismatch")
}

/// Parse only fixed `ps` columns; any unexpected shape is an owner mismatch.
fn parse_identity(output: &[u8]) -> Result<ProcessIdentity, &'static str> {
    let text = std::str::from_utf8(output).map_err(|_| "marker-owner-mismatch")?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 7 {
        return Err("marker-owner-mismatch");
    }
    let pgid = fields[0]
        .parse::<i32>()
        .ok()
        .filter(|value| *value > 1)
        .ok_or("marker-owner-mismatch")?;
    let comm = fields[1].to_owned();
    let start_identity = fields[2..].join(" ");
    Ok(ProcessIdentity {
        pgid,
        comm,
        start_identity,
    })
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
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn bounded query fixture");
        let process_group = i32::try_from(child.id())
            .ok()
            .filter(|value| *value > 1)
            .expect("fixture process group");
        let mut backend = RealQueryChild {
            child: &mut child,
            process_group,
        };
        assert!(bounded_reap_backend(
            &mut backend,
            Instant::now(),
            Duration::from_secs(1)
        ));
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
