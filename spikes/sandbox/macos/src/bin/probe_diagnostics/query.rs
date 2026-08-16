// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Bounded process-start identity queries and their private supervisors.

use super::SandboxDenialDiagnostics;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const START_QUERY_DEADLINE: Duration = Duration::from_secs(1);
const START_QUERY_STDOUT_LIMIT: usize = 1024;
const START_QUERY_STDERR_LIMIT: usize = 1024;

/// Query identity from tests without attaching an owner; production callers
/// use the owner-aware variant so every post-spawn failure cleans the helper.
#[cfg(test)]
pub(super) fn process_start_identity(pid: u32) -> Option<String> {
    process_start_identity_inner(None, pid)
}

/// Query identity after the host helper is owned by diagnostics; any query
/// process lifecycle failure first enters that owner's bounded cleanup path.
pub(super) fn process_start_identity_owned(
    owner: &mut SandboxDenialDiagnostics,
    pid: u32,
) -> Option<String> {
    process_start_identity_inner(Some(owner), pid)
}

/// Run the bounded identity query with an optional owner for fail-safe cleanup.
fn process_start_identity_inner(
    owner: Option<&mut SandboxDenialDiagnostics>,
    pid: u32,
) -> Option<String> {
    let pid = pid.to_string();
    let mut child = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .ok()?;
    let process_group = match i32::try_from(child.id()).ok().filter(|value| *value > 1) {
        Some(process_group) => process_group,
        None => {
            let _ = reap_query_child_without_group(&mut child);
            abort_identity_query(owner, "pid-range");
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let pipes_ready = stdout.as_ref().is_some_and(super::set_nonblocking)
        && stderr.as_ref().is_some_and(super::set_nonblocking);
    if !pipes_ready {
        drop(stdout.take());
        drop(stderr.take());
        return finish_start_identity_query(&mut child, process_group, None, owner);
    }
    let deadline = Instant::now() + START_QUERY_DEADLINE;
    let mut output = Vec::new();
    let mut stdout_bytes = 0;
    let mut stderr_bytes = 0;
    let mut pipes_ok = true;
    let mut status = None;
    loop {
        if let Some(reader) = stdout.as_mut() {
            pipes_ok &= drain_query_pipe(
                reader,
                Some(&mut output),
                &mut stdout_bytes,
                START_QUERY_STDOUT_LIMIT,
                deadline,
            );
        }
        if let Some(reader) = stderr.as_mut() {
            pipes_ok &= drain_query_pipe(
                reader,
                None,
                &mut stderr_bytes,
                START_QUERY_STDERR_LIMIT,
                deadline,
            );
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => break,
        }
    }
    let status = match status {
        Some(status) => status,
        None => {
            return finish_start_identity_query(&mut child, process_group, None, owner);
        }
    };
    if let Some(reader) = stdout.as_mut() {
        pipes_ok &= drain_query_pipe(
            reader,
            Some(&mut output),
            &mut stdout_bytes,
            START_QUERY_STDOUT_LIMIT,
            deadline,
        );
    }
    if let Some(reader) = stderr.as_mut() {
        pipes_ok &= drain_query_pipe(
            reader,
            None,
            &mut stderr_bytes,
            START_QUERY_STDERR_LIMIT,
            deadline,
        );
    }
    drop(stdout);
    drop(stderr);
    if !status.success() || !pipes_ok {
        return finish_start_identity_query(&mut child, process_group, None, owner);
    }
    finish_start_identity_query(&mut child, process_group, Some(output), owner)
}

/// Drain only bounded bytes from a nonblocking identity-query pipe; stderr is
/// deliberately discarded because its content is not part of cleanup proof.
pub(super) fn drain_query_pipe<R: Read>(
    reader: &mut R,
    mut output: Option<&mut Vec<u8>>,
    consumed: &mut usize,
    cap: usize,
    deadline: Instant,
) -> bool {
    let mut buffer = [0_u8; 256];
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        match reader.read(&mut buffer) {
            Ok(0) => return true,
            Ok(read) => {
                *consumed = consumed.saturating_add(read);
                if *consumed > cap {
                    if let Some(output) = &mut output {
                        output.clear();
                    }
                    return false;
                }
                if let Some(output) = &mut output {
                    output.extend_from_slice(&buffer[..read]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

/// Finish an identity query only after direct-child reap and group absence;
/// an unreapable query process aborts the host rather than being dropped live.
fn finish_start_identity_query(
    child: &mut Child,
    process_group: i32,
    output: Option<Vec<u8>>,
    owner: Option<&mut SandboxDenialDiagnostics>,
) -> Option<String> {
    if child.try_wait().ok().flatten().is_none() {
        let _ = unsafe { kill(-process_group, super::SIGKILL) };
        let _ = child.kill();
    }
    let deadline = Instant::now() + START_QUERY_DEADLINE;
    let mut reaped = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                reaped = true;
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    if !reaped {
        abort_identity_query(owner, "unreaped");
    }
    let group_deadline = Instant::now() + START_QUERY_DEADLINE;
    while !query_group_empty(process_group) && Instant::now() < group_deadline {
        let _ = unsafe { kill(-process_group, super::SIGKILL) };
        thread::sleep(Duration::from_millis(5));
    }
    if !query_group_empty(process_group) {
        abort_identity_query(owner, "group-residual");
    }
    let output = output?;
    let identity = String::from_utf8(output)
        .ok()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    valid_start_identity(&identity).then_some(identity)
}

/// Flush evidence and clean the already-spawned host helper before aborting if
/// the independent identity query cannot prove its own bounded lifecycle.
fn abort_identity_query(owner: Option<&mut SandboxDenialDiagnostics>, reason: &str) -> ! {
    if let Some(owner) = owner {
        owner.unavailable = Some("start-identity");
        let _ = owner.cleanup_child(false);
        owner.flush_failure_evidence();
    }
    eprintln!("SANDBOX-DIAGNOSTICS: start-identity-query={reason}");
    let _ = io::stderr().flush();
    std::process::abort();
}

/// Reap a query child without forming a negative process-group target when
/// process creation returned a reserved or otherwise invalid PID.
fn reap_query_child_without_group(child: &mut Child) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + START_QUERY_DEADLINE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(_) => return false,
        }
    }
    false
}

/// Verify the query group vanished with ESRCH, not merely with a generic
/// nonzero status that could mean permission failure.
fn query_group_empty(process_group: i32) -> bool {
    unsafe { kill(-process_group, 0) == -1 && *__error() == super::ESRCH }
}

/// Restrict start identities to a short printable value that cannot add
/// marker fields, newlines, paths or secrets to the cleanup evidence.
pub(super) fn valid_start_identity(identity: &str) -> bool {
    !identity.is_empty()
        && identity.len() <= 128
        && identity
            .chars()
            .all(|character| character == ' ' || character.is_ascii_graphic())
        && !identity.contains('=')
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn __error() -> *mut i32;
}
