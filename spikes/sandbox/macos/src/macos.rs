// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Narrow Seatbelt/process-group adapter.  The probe deliberately keeps this
//! code outside the Tauri crate so a future production adapter must pass the
//! same native escape and lifecycle gates before it can be reused.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::path::{PreparedPaths, prepare_paths};

/// The cap is intentionally small enough to make a malicious worker unable
/// to turn the host into an unbounded log buffer while still accepting normal
/// tool diagnostics.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SIGKILL: i32 = 9;
const ESRCH: i32 = 3;

/// All inputs needed to form one immutable Seatbelt invocation.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub worker: PathBuf,
    pub workspace: PathBuf,
    pub profile_path: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub max_output_bytes: usize,
}

impl SandboxSpec {
    /// Use a bounded default so a caller has to make an explicit, reviewed
    /// decision before requesting a smaller or larger diagnostic budget.
    pub fn new(
        worker: impl Into<PathBuf>,
        workspace: impl Into<PathBuf>,
        profile_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            worker: worker.into(),
            workspace: workspace.into(),
            profile_path: profile_path.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            max_output_bytes: 64 * 1024,
        }
    }
}

/// Validated inputs reused by profile generation and process spawning.
/// Canonicalizing once makes the Seatbelt path contract match the command's
/// actual exec/current-directory paths instead of two independently resolved
/// aliases.
struct PreparedSandboxSpec {
    worker: PathBuf,
    workspace: PathBuf,
    profile_path: PathBuf,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    max_output_bytes: usize,
}

/// Stable public errors omit absolute paths and environment values so a
/// rejected tool invocation cannot accidentally expose a user secret.
#[derive(Debug)]
pub enum SandboxError {
    InvalidConfig(&'static str),
    Io(io::Error),
    Profile(&'static str),
    Unsupported,
    Timeout,
    Cancelled,
    OutputOverflow,
    ChildCleanup(&'static str),
}

impl Display for SandboxError {
    /// Keep diagnostics actionable while ensuring only categories, not
    /// command lines or fixture paths, reach the UI/log boundary.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(reason) => write!(formatter, "invalid sandbox config: {reason}"),
            Self::Io(error) => write!(formatter, "sandbox I/O failed: {error}"),
            Self::Profile(reason) => write!(formatter, "Seatbelt profile failed: {reason}"),
            Self::Unsupported => formatter.write_str("macOS Seatbelt sandbox is unsupported"),
            Self::Timeout => formatter.write_str("sandbox operation timed out"),
            Self::Cancelled => formatter.write_str("sandbox operation cancelled"),
            Self::OutputOverflow => formatter.write_str("sandbox output exceeded its bound"),
            Self::ChildCleanup(reason) => {
                write!(formatter, "sandbox child cleanup failed: {reason}")
            }
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<io::Error> for SandboxError {
    /// Preserve the original I/O kind for the local probe while callers still
    /// receive the sanitized Display form above.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Direct child status is retained separately from output so a normal parent
/// exit can still trigger process-group cleanup before pipe EOF is required.
#[derive(Debug, Clone, Copy)]
pub struct ChildOutcome {
    pub status: ExitStatus,
    pub timed_out: bool,
}

/// Bounded output returned only after both host-owned readers have stopped.
#[derive(Debug)]
pub struct RunOutput {
    pub outcome: ChildOutcome,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A nonblocking host-owned pipe collector keeps output bounded without
/// requiring reader threads whose join could outlive a detached descendant.
struct OutputCollector {
    bytes: Vec<u8>,
    max_bytes: usize,
    overflow: bool,
    eof: bool,
}

impl OutputCollector {
    /// Start an empty bounded collector; the cap is validated before spawn and
    /// cannot grow while the worker is running.
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            overflow: false,
            eof: false,
        }
    }

    /// Drain all currently available bytes and report EOF without blocking on
    /// a worker that may be waiting for another host operation.
    fn drain<R: Read>(&mut self, reader: &mut R) -> io::Result<()> {
        if self.eof || self.overflow {
            return Ok(());
        }
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(read) => {
                    if !self.overflow {
                        if self.bytes.len().saturating_add(read) > self.max_bytes {
                            let remaining = self.max_bytes.saturating_sub(self.bytes.len());
                            self.bytes.extend_from_slice(&buffer[..remaining]);
                            self.overflow = true;
                            return Ok(());
                        }
                        self.bytes.extend_from_slice(&buffer[..read]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

/// A live worker with a dedicated process group and profile cleanup owner.
pub struct SandboxChild {
    child: Child,
    process_group: i32,
    profile_path: PathBuf,
    terminated: bool,
    cancelled: bool,
    max_output_bytes: usize,
}

/// Start sandbox-exec with an explicit process group and an allowlisted
/// environment.  The wrapper is itself the group leader, so all normal
/// descendants inherit the kill boundary before the worker executes.
pub fn spawn(spec: SandboxSpec) -> Result<SandboxChild, SandboxError> {
    let spec = prepare_spec(spec)?;
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Err(SandboxError::Unsupported);
    }
    let profile = build_profile(&spec)?;
    let mut profile_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&spec.profile_path)
        .map_err(SandboxError::Io)?;
    if let Err(error) = profile_file.write_all(profile.as_bytes()) {
        let _ = fs::remove_file(&spec.profile_path);
        return Err(SandboxError::Io(error));
    }
    drop(profile_file);
    if let Err(error) = fs::set_permissions(&spec.profile_path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(&spec.profile_path);
        return Err(SandboxError::Io(error));
    }

    let mut command = Command::new(SANDBOX_EXEC);
    command
        .arg("-f")
        .arg(&spec.profile_path)
        .arg(&spec.worker)
        .args(&spec.args)
        .current_dir(&spec.workspace)
        .env_clear()
        .envs(spec.env.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // process_group(0) is applied before exec, closing the spawn/kill
        // race without relying on a shell or a post-spawn setpgid window.
        .process_group(0);

    let result = command.spawn();
    let child = match result {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&spec.profile_path);
            return Err(SandboxError::Io(error));
        }
    };
    let process_group = child.id() as i32;
    if child.stdout.is_none() || child.stderr.is_none() {
        let mut child = child;
        terminate_untracked_child(&mut child, process_group);
        let _ = fs::remove_file(&spec.profile_path);
        return Err(SandboxError::InvalidConfig(
            "sandbox output pipe unavailable",
        ));
    }
    if process_group <= 0 || process_group == std::process::id() as i32 {
        let mut child = child;
        terminate_untracked_child(&mut child, 0);
        let _ = fs::remove_file(&spec.profile_path);
        return Err(SandboxError::InvalidConfig("invalid worker process group"));
    }
    Ok(SandboxChild {
        child,
        process_group,
        profile_path: spec.profile_path,
        terminated: false,
        cancelled: false,
        max_output_bytes: spec.max_output_bytes,
    })
}

impl SandboxChild {
    /// Cancel the complete process group before waiting, preserving a
    /// separate cancelled result so UI cancellation cannot look successful.
    pub fn cancel(&mut self) -> Result<(), SandboxError> {
        self.cancelled = true;
        let result = self.terminate_group();
        if result.is_err() {
            // A failed group signal must not return while the direct child is
            // still unbounded; Drop will retry the group signal later.
            let _ = self.child.kill();
            let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        }
        result
    }

    /// Poll the direct child without blocking so a caller can distinguish a
    /// worker that never wrote its startup marker from a slow worker.
    pub fn poll_status(&mut self) -> Result<Option<ExitStatus>, SandboxError> {
        match self.child.try_wait() {
            Ok(status) => Ok(status),
            Err(error) => Err(self.cleanup_after_wait_error(SandboxError::Io(error))),
        }
    }

    /// Wait with a deadline, kill the full process group on every terminal
    /// path, then drain nonblocking pipes until EOF or a bounded cleanup
    /// deadline.  This avoids both lost final output and unbounded joins.
    pub fn wait_with_output(&mut self, timeout: Duration) -> Result<RunOutput, SandboxError> {
        let mut stdout = match self.child.stdout.take() {
            Some(pipe) => pipe,
            None => return self.fail_before_wait("sandbox stdout pipe unavailable"),
        };
        let mut stderr = match self.child.stderr.take() {
            Some(pipe) => pipe,
            None => return self.fail_before_wait("sandbox stderr pipe unavailable"),
        };
        if let Err(error) = set_nonblocking(&stdout).and_then(|_| set_nonblocking(&stderr)) {
            let cleanup_error = self.terminate_group().err();
            let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
            let _ = self.child.kill();
            let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
            if let Some(cleanup_error) = cleanup_error {
                return Err(cleanup_error);
            }
            return Err(SandboxError::Io(error));
        }
        let mut stdout_output = OutputCollector::new(self.max_output_bytes());
        let mut stderr_output = OutputCollector::new(self.max_output_bytes());
        let deadline = Instant::now() + timeout;
        let mut status = None;
        let mut terminal_error = None;

        loop {
            if let Err(error) = stdout_output
                .drain(&mut stdout)
                .and_then(|_| stderr_output.drain(&mut stderr))
            {
                terminal_error = Some(SandboxError::Io(error));
                break;
            }
            if stdout_output.overflow || stderr_output.overflow {
                terminal_error = Some(SandboxError::OutputOverflow);
                break;
            }
            match self.child.try_wait() {
                Ok(Some(value)) => {
                    status = Some(value);
                    // A direct parent can exit while an inherited pipe remains
                    // open in a grandchild, so group termination happens first.
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    terminal_error = Some(SandboxError::Io(error));
                    break;
                }
            }
            if self.cancelled {
                terminal_error = Some(SandboxError::Cancelled);
                break;
            }
            if Instant::now() >= deadline {
                terminal_error = Some(SandboxError::Timeout);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        if let Err(error) = self.terminate_group() {
            // Cleanup failure outranks timeout/exit status: returning a
            // successful-looking lifecycle result would hide live descendants.
            terminal_error = Some(error);
        }

        if !stdout_output.overflow && !stderr_output.overflow {
            // A normal group kill closes both pipes and lets us preserve all
            // already-buffered output.  A detached setsid descendant keeps a
            // pipe open, so this loop has a hard deadline and returns a
            // cleanup error.
            let drain_deadline = Instant::now() + CLEANUP_DEADLINE;
            while !(stdout_output.eof && stderr_output.eof) && Instant::now() < drain_deadline {
                if let Err(error) = stdout_output
                    .drain(&mut stdout)
                    .and_then(|_| stderr_output.drain(&mut stderr))
                {
                    if terminal_error.is_none() {
                        terminal_error = Some(SandboxError::Io(error));
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if !(stdout_output.eof && stderr_output.eof) && terminal_error.is_none() {
                terminal_error = Some(SandboxError::ChildCleanup("output pipes did not close"));
            }
        }

        if status.is_none() {
            match self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE) {
                Ok(value) => status = value,
                Err(error) => {
                    // A poll error has already followed group termination;
                    // retry direct kill so the caller never receives an
                    // error while the direct child is still intentionally
                    // left running.
                    let _ = self.child.kill();
                    if let Ok(value) = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE) {
                        status = value;
                    }
                    if terminal_error.is_none() {
                        terminal_error = Some(error);
                    }
                }
            }
        }
        let outcome = ChildOutcome {
            status: match status {
                Some(value) => value,
                None => return Err(SandboxError::ChildCleanup("direct child did not terminate")),
            },
            timed_out: matches!(&terminal_error, Some(SandboxError::Timeout)),
        };
        if let Some(error) = terminal_error {
            return Err(error);
        }
        Ok(RunOutput {
            outcome,
            stdout: stdout_output.bytes,
            stderr: stderr_output.bytes,
        })
    }

    /// The probe's output cap is fixed at construction time by the profile
    /// specification; this accessor keeps wait logic from accepting a larger
    /// limit after the process has started.
    fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// Kill and bounded-reap after a status poll error so diagnostics never
    /// trade a useful error category for an untracked live child.
    fn cleanup_after_wait_error(&mut self, error: SandboxError) -> SandboxError {
        let cleanup_error = self.terminate_group().err();
        let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        let _ = self.child.kill();
        let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        cleanup_error.unwrap_or(error)
    }

    /// Reuse the same cleanup ordering for malformed pipe state and setup
    /// errors, so an early return can never leave the worker alive.
    fn fail_before_wait(&mut self, reason: &'static str) -> Result<RunOutput, SandboxError> {
        let cleanup_error = self.terminate_group().err();
        let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        let _ = self.child.kill();
        let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        if let Some(cleanup_error) = cleanup_error {
            return Err(cleanup_error);
        }
        Err(SandboxError::InvalidConfig(reason))
    }

    /// Reap the direct child without an unbounded wait; failure is surfaced as
    /// cleanup error while the group kill has already been attempted.
    fn reap_direct_until(&mut self, deadline: Instant) -> Result<Option<ExitStatus>, SandboxError> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(Some(status)),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Ok(None) => return Ok(None),
                Err(error) => {
                    let _ = self.child.kill();
                    return Err(SandboxError::Io(error));
                }
            }
        }
    }

    /// Send SIGKILL to the process group exactly once; ESRCH is success after
    /// a normal child exit because the group may already have disappeared.
    fn terminate_group(&mut self) -> Result<(), SandboxError> {
        if self.terminated {
            return Ok(());
        }
        let result = unsafe { kill(-(self.process_group), SIGKILL) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ESRCH) {
                let _ = self.child.kill();
                return Err(SandboxError::Io(error));
            }
        }
        self.terminated = true;
        Ok(())
    }
}

impl Drop for SandboxChild {
    /// Drop is a last-resort fail-closed guard for panic/error paths; profile
    /// deletion is delayed until the process group has been signalled.
    fn drop(&mut self) {
        let _ = self.terminate_group();
        let _ = self.child.kill();
        let _ = self.reap_direct_until(Instant::now() + CLEANUP_DEADLINE);
        let _ = fs::remove_file(&self.profile_path);
    }
}

const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;

/// Fail closed for a child that could not be wrapped in `SandboxChild`; this
/// path still signals its group and uses bounded polling instead of `wait()`.
fn terminate_untracked_child(child: &mut Child, process_group: i32) {
    if process_group > 0 && process_group != std::process::id() as i32 {
        let _ = unsafe { kill(-process_group, SIGKILL) };
    }
    let _ = child.kill();
    let deadline = Instant::now() + CLEANUP_DEADLINE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Put host-owned pipes into nonblocking mode so cleanup can enforce a hard
/// deadline even if an escaped descendant retains a writer descriptor.
fn set_nonblocking<R: AsRawFd>(reader: &R) -> io::Result<()> {
    let flags = unsafe { fcntl(reader.as_raw_fd(), F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { fcntl(reader.as_raw_fd(), F_SETFL, flags | O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Validate every path and cap before any profile or child side effect is
/// created; this keeps malformed user data fail-closed and deterministic.
/// The returned canonical paths are the only paths used after preparation.
fn prepare_spec(spec: SandboxSpec) -> Result<PreparedSandboxSpec, SandboxError> {
    if spec.max_output_bytes == 0 || spec.max_output_bytes > MAX_OUTPUT_BYTES {
        return Err(SandboxError::InvalidConfig(
            "output bound is outside hard limit",
        ));
    }
    let worker_metadata = fs::symlink_metadata(&spec.worker).map_err(SandboxError::Io)?;
    if !worker_metadata.file_type().is_file() {
        return Err(SandboxError::InvalidConfig("worker is not a regular file"));
    }
    if !spec.workspace.is_dir() {
        return Err(SandboxError::InvalidConfig("workspace is not a directory"));
    }
    let PreparedPaths {
        worker,
        workspace,
        profile_path,
    } = prepare_paths(&spec.worker, &spec.workspace, &spec.profile_path)
        .map_err(SandboxError::Io)?;
    reject_workspace_links(&workspace)?;
    let profile_parent = profile_path
        .parent()
        .ok_or(SandboxError::InvalidConfig("profile has no parent"))?;
    if profile_parent.starts_with(&workspace) {
        return Err(SandboxError::InvalidConfig(
            "profile cannot be workspace-owned",
        ));
    }
    Ok(PreparedSandboxSpec {
        worker,
        workspace,
        profile_path,
        args: spec.args,
        env: spec.env,
        max_output_bytes: spec.max_output_bytes,
    })
}

/// Generate a default-deny Seatbelt profile with only fixed worker/resource,
/// workspace data and loader read access.  Network is intentionally absent.
fn build_profile(spec: &PreparedSandboxSpec) -> Result<String, SandboxError> {
    let worker = literal(&spec.worker)?;
    let workspace = literal(&spec.workspace)?;
    let metadata_rule = metadata_rule(&[spec.worker.clone(), spec.workspace.clone()])?;
    Ok(format!(
        "(version 1)\n\
(deny default)\n\
(allow process-fork)\n\
(allow process-exec (literal \"{worker}\"))\n\
(allow process-info* (target same-sandbox))\n\
(allow signal (target same-sandbox))\n\
(allow sysctl-read)\n\
(allow file-read* (subpath \"/usr/lib\") (subpath \"/System/Library\") (subpath \"/usr/share\"))\n\
(allow file-read* (literal \"/dev/null\") (literal \"/dev/random\") (literal \"/dev/urandom\"))\n\
(allow file-read* (literal \"{worker}\"))\n\
{metadata_rule}\
(allow file-read* (subpath \"{workspace}\"))\n\
(allow file-write* (subpath \"{workspace}\"))\n\
(deny network*)\n",
    ))
}

#[cfg(test)]
mod tests {
    use super::{PreparedSandboxSpec, build_profile};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// Lock the process-control policy to same-sandbox targets so a future
    /// profile edit cannot silently introduce unrestricted process access.
    #[test]
    fn profile_process_controls_are_same_sandbox_only() {
        let profile = build_profile(&PreparedSandboxSpec {
            worker: PathBuf::from("/private/var/tmp/worker"),
            workspace: PathBuf::from("/private/var/tmp/workspace"),
            profile_path: PathBuf::from("/private/var/tmp/profile.sb"),
            args: Vec::<OsString>::new(),
            env: BTreeMap::new(),
            max_output_bytes: 64 * 1024,
        })
        .expect("build profile");

        assert!(
            profile
                .lines()
                .any(|line| line == "(allow process-info* (target same-sandbox))")
        );
        assert!(
            profile
                .lines()
                .any(|line| line == "(allow signal (target same-sandbox))")
        );
        assert!(!profile.lines().any(|line| line == "(allow process-info*)"));
        assert!(!profile.contains("(allow default)"));
        assert!(!profile.contains("(allow process-info* (target all))"));
    }
}

/// Reject multiply-linked regular files before Seatbelt setup so a workspace
/// path cannot name protected content through a shared inode. Symlink targets
/// remain in the native smoke case and must be denied by Seatbelt itself.
fn reject_workspace_links(workspace: &Path) -> Result<(), SandboxError> {
    let metadata = fs::symlink_metadata(workspace).map_err(SandboxError::Io)?;
    if metadata.is_file() && metadata.nlink() > 1 {
        return Err(SandboxError::InvalidConfig(
            "workspace contains symlink or hardlink",
        ));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(workspace).map_err(SandboxError::Io)? {
            let entry = entry.map_err(SandboxError::Io)?;
            reject_workspace_links(&entry.path())?;
        }
    }
    Ok(())
}

/// Allow metadata only for path components required to traverse the worker
/// and workspace; protected siblings receive neither data nor metadata.
fn metadata_rule(paths: &[PathBuf]) -> Result<String, SandboxError> {
    let mut values = BTreeSet::new();
    for path in paths {
        for ancestor in path.ancestors() {
            values.insert(literal(ancestor)?);
        }
    }
    Ok(format!(
        "(allow file-read-metadata {})\n",
        values
            .into_iter()
            .map(|value| format!("(literal \"{value}\")"))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// Escape a path for Seatbelt's literal syntax and reject control characters
/// rather than allowing profile injection through a crafted workspace name.
fn literal(path: &Path) -> Result<String, SandboxError> {
    let value = path
        .to_str()
        .ok_or(SandboxError::InvalidConfig("path is not UTF-8"))?;
    if value
        .chars()
        .any(|character| character == '"' || character == '\\' || character.is_control())
    {
        return Err(SandboxError::InvalidConfig(
            "path cannot be represented safely",
        ));
    }
    Ok(value.to_string())
}

/// Query a PID without process-name matching; the probe uses this to prove
/// that a reported grandchild really left the system after group cleanup.
pub fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { kill(pid, 0) == 0 }
}

/// Kill only the PID reported by the escape fixture so a failed negative case
/// leaves no worker behind while still returning a production-blocking error.
pub fn kill_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { kill(pid, SIGKILL) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(ESRCH)
}

// Raw POSIX kill is used instead of a shell command so cancellation cannot
// accidentally parse a user-controlled argument or kill an unrelated name.
unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}
