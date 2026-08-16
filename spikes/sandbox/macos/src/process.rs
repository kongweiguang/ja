// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Shared bounded process primitives for every host-side macOS helper.
//!
//! The security invariant is deliberately centralized here: callers may
//! configure a command, but only this module may create its process group.
//! Keeping the group creation and the invalid-PID cleanup together prevents a
//! future capability probe or diagnostic helper from quietly becoming an
//! unbounded child process.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SIGKILL: i32 = 9;
const ESRCH: i32 = 3;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;
const CHILD_REAP_BUDGET: Duration = Duration::from_secs(2);

/// The small result used by capability and metadata probes after both output
/// streams have been bounded and the complete process group has disappeared.
#[derive(Debug)]
pub struct BoundedCommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Spawn one process-group leader and reject reserved identities immediately.
/// The process group is assigned before exec so cleanup never depends on a
/// post-spawn race; an impossible reserved PID is reaped before returning.
pub fn spawn_grouped(command: &mut Command) -> io::Result<Child> {
    spawn_internal(command, true)
}

/// Spawn an ordinary descendant without changing its inherited process group.
/// Host cleanup must be able to terminate normal worker descendants with the
/// wrapper group; only the explicit setsid fixture is allowed to create an
/// escape group, and it is handled as a separate negative test.
pub fn spawn_inherited(command: &mut Command) -> io::Result<Child> {
    spawn_internal(command, false)
}

/// Share reserved-PID handling between the two explicitly reviewed spawn
/// modes so no caller can receive an untracked child after a fork boundary.
fn spawn_internal(command: &mut Command, new_process_group: bool) -> io::Result<Child> {
    if new_process_group {
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id()).unwrap_or(-1);
    if pid > 1 {
        return Ok(child);
    }
    let _ = child.kill();
    if !reap_direct(&mut child, Instant::now() + CHILD_REAP_BUDGET) {
        // Returning while an identity we could not register is live would
        // make later cleanup unable to target it precisely.
        std::process::abort();
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reserved child identity",
    ))
}

/// Signal only a validated process group; values 0, 1 and -1 are never
/// passed to the kernel because they have process-wide or session-wide scope.
pub fn safe_signal_group(process_group: i32, signal: i32) -> io::Result<()> {
    if process_group <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved process group",
        ));
    }
    let result = unsafe { kill(-process_group, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Signal only a validated direct PID; the reserved boundary is checked at
/// every call site because PID reuse must never widen a cleanup operation.
pub fn safe_signal_pid(pid: i32, signal: i32) -> io::Result<()> {
    if pid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved process id",
        ));
    }
    let result = unsafe { kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Run a short host helper with nonblocking pipes, independent stream caps,
/// one absolute deadline, and group cleanup.  This is used for xattr/ACL/hash
/// snapshots and capability checks so inspection commands share worker-grade
/// lifecycle guarantees instead of relying on `Command::output`.
pub fn run_bounded_command(
    mut command: Command,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> io::Result<BoundedCommandOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_grouped(&mut command)?;
    let process_group = match i32::try_from(child.id()) {
        Ok(value) => value,
        Err(_) => {
            // `spawn_grouped` already rejects this state on normal Darwin
            // PIDs, but keep the cleanup explicit so a future platform with
            // a wider child identifier cannot return a live Child to Drop.
            return cleanup_invalid_child(&mut child, 0);
        }
    };
    if process_group <= 1 {
        return cleanup_invalid_child(&mut child, process_group);
    }
    let mut stdout = match child.stdout.take() {
        Some(pipe) => pipe,
        None => return cleanup_failed_child(&mut child, process_group, "stdout pipe unavailable"),
    };
    let mut stderr = match child.stderr.take() {
        Some(pipe) => pipe,
        None => {
            drop(stdout);
            return cleanup_failed_child(&mut child, process_group, "stderr pipe unavailable");
        }
    };
    if let Err(error) = set_nonblocking(&stdout).and_then(|_| set_nonblocking(&stderr)) {
        drop(stdout);
        drop(stderr);
        return cleanup_with_error(&mut child, process_group, error);
    }

    let deadline = Instant::now() + timeout;
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut status = None;
    let mut failure = None;
    while Instant::now() < deadline {
        if let Err(error) = drain(&mut stdout, &mut stdout_buf, max_stdout, &mut stdout_eof) {
            failure = Some(error);
            break;
        }
        if let Err(error) = drain(&mut stderr, &mut stderr_buf, max_stderr, &mut stderr_eof) {
            failure = Some(error);
            break;
        }
        match child.try_wait() {
            Ok(Some(value)) => {
                status = Some(value);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    if status.is_none() && failure.is_none() {
        failure = Some(io::Error::new(io::ErrorKind::TimedOut, "helper timed out"));
    }

    // A normal helper can exit before its inherited pipe writers close.  Kill
    // the group before the final drain so this bounded function never waits
    // for an escaped writer indefinitely.
    let cleanup_error = terminate_and_reap(&mut child, process_group);
    let drain_deadline = Instant::now() + CHILD_REAP_BUDGET;
    let mut drain_error = None;
    while !(stdout_eof && stderr_eof) && Instant::now() < drain_deadline {
        let before = (stdout_eof, stderr_eof, stdout_buf.len(), stderr_buf.len());
        if !stdout_eof {
            if let Err(error) = drain(&mut stdout, &mut stdout_buf, max_stdout, &mut stdout_eof) {
                drain_error = Some(error);
                break;
            }
        }
        if !stderr_eof {
            if let Err(error) = drain(&mut stderr, &mut stderr_buf, max_stderr, &mut stderr_eof) {
                drain_error = Some(error);
                break;
            }
        }
        if before == (stdout_eof, stderr_eof, stdout_buf.len(), stderr_buf.len()) {
            thread::sleep(Duration::from_millis(2));
        }
    }
    if let Some(error) = cleanup_error {
        drop(stdout);
        drop(stderr);
        eprintln!("SANDBOX-PROCESS: helper-cleanup-unconfirmed");
        let _ = error;
        std::process::abort();
    }
    let status = status.ok_or_else(|| io::Error::other("helper unreaped"))?;
    drop(stdout);
    drop(stderr);
    if !stdout_eof || !stderr_eof {
        // The streams were bounded while live and are closed after group
        // cleanup; a missing EOF is retained as a hard lifecycle failure.
        return Err(io::Error::other("helper pipe not closed"));
    }
    if let Some(error) = drain_error {
        return Err(error);
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(BoundedCommandOutput {
        status,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Drain only bytes currently available on a nonblocking pipe.  Once a cap
/// is exceeded the error path kills the entire group instead of discarding an
/// unbounded stream and pretending the helper completed normally.
fn drain<R: Read>(
    reader: &mut R,
    target: &mut Vec<u8>,
    cap: usize,
    eof: &mut bool,
) -> io::Result<()> {
    if *eof {
        return Ok(());
    }
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(read) => {
                if target.len().saturating_add(read) > cap {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "helper output overflow",
                    ));
                }
                target.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

/// Stop a helper group and retain direct-child ownership until both the
/// child is reaped and the original group reports ESRCH.
fn terminate_and_reap(child: &mut Child, process_group: i32) -> Option<io::Error> {
    let mut first_error = None;
    if let Err(error) = safe_signal_group(process_group, SIGKILL)
        && error.raw_os_error() != Some(ESRCH)
    {
        first_error = Some(error);
    }
    let deadline = Instant::now() + CHILD_REAP_BUDGET;
    if !reap_direct(child, deadline) {
        let _ = child.kill();
        if !reap_direct(child, Instant::now() + CHILD_REAP_BUDGET) {
            return Some(io::Error::other("helper child unreaped"));
        }
    }
    let group_deadline = Instant::now() + CHILD_REAP_BUDGET;
    while Instant::now() < group_deadline {
        match group_is_gone(process_group) {
            Ok(true) => return first_error,
            Ok(false) => {
                let _ = safe_signal_group(process_group, SIGKILL);
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Some(error),
        }
    }
    Some(io::Error::other("helper process group remained"))
}

/// Poll a direct child until it is reaped; no unbounded `wait` is permitted in
/// a helper used by a security gate.
fn reap_direct(child: &mut Child, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_) => return false,
        }
    }
    false
}

/// A setup failure still owns the direct child; cleanup is attempted before a
/// stable error is returned, and an unprovable reap aborts rather than leaks.
fn cleanup_failed_child(
    child: &mut Child,
    process_group: i32,
    message: &'static str,
) -> io::Result<BoundedCommandOutput> {
    cleanup_with_error(
        child,
        process_group,
        io::Error::new(io::ErrorKind::InvalidData, message),
    )
}

/// Handle the impossible reserved-group branch without ever signalling a
/// process-wide target.
fn cleanup_invalid_child(
    child: &mut Child,
    process_group: i32,
) -> io::Result<BoundedCommandOutput> {
    if process_group > 1 {
        if terminate_and_reap(child, process_group).is_some() {
            std::process::abort();
        }
    } else {
        let _ = child.kill();
        if !reap_direct(child, Instant::now() + CHILD_REAP_BUDGET) {
            std::process::abort();
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "reserved process group",
    ))
}

/// Preserve the first setup error while still proving the helper group died.
fn cleanup_with_error(
    child: &mut Child,
    process_group: i32,
    error: io::Error,
) -> io::Result<BoundedCommandOutput> {
    if let Some(cleanup_error) = terminate_and_reap(child, process_group) {
        eprintln!("SANDBOX-PROCESS: helper-cleanup-unconfirmed");
        let _ = cleanup_error;
        std::process::abort();
    }
    Err(error)
}

/// Check group existence using errno, never by parsing localized output.
fn group_is_gone(process_group: i32) -> io::Result<bool> {
    if process_group <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved process group",
        ));
    }
    let result = unsafe { kill(-process_group, 0) };
    if result == 0 {
        Ok(false)
    } else if io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
        Ok(true)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Set a pipe nonblocking before any read so inherited writers cannot create
/// an unbounded wait during a helper timeout or output overflow.
fn set_nonblocking<T: AsRawFd>(file: &T) -> io::Result<()> {
    let flags = unsafe { fcntl(file.as_raw_fd(), F_GETFL) };
    if flags == -1 || unsafe { fcntl(file.as_raw_fd(), F_SETFL, flags | O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use super::{safe_signal_group, safe_signal_pid};

    /// Reserved process values must be rejected before the FFI boundary; the
    /// test proves the guard cannot accidentally signal the test process.
    #[test]
    fn reserved_ids_are_rejected() {
        for value in [-1, 0, 1] {
            assert!(safe_signal_pid(value, 0).is_err());
            assert!(safe_signal_group(value, 0).is_err());
        }
    }
}
