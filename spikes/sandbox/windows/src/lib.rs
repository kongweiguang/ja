// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Windows-only security-boundary probe used before the JA worker adapter is
//! promoted into the Tauri host.  The public model is platform-neutral so
//! unsupported platforms fail explicitly instead of silently downgrading to a
//! path check.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

#[cfg(windows)]
mod windows;

/// The first version deliberately has no network capability.  A future
/// explicit network mode must create a separate profile and document its
/// coarse AppContainer capability boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkCapability {
    Denied,
    InternetClient,
}

/// Workspace access is separate from executable-resource access so a worker
/// cannot rewrite its own native image merely because it can edit a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

/// Resource limits are applied by a Job Object so a worker cannot turn a
/// harmless tool invocation into an unbounded process or memory fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    pub max_processes: u32,
    pub process_memory_bytes: usize,
}

impl Default for ResourceBudget {
    /// Small defaults keep the probe representative of a desktop tool worker
    /// while still allowing a worker and a few helper processes.
    fn default() -> Self {
        Self {
            max_processes: 8,
            process_memory_bytes: 256 * 1024 * 1024,
        }
    }
}

/// All executable and filesystem inputs are explicit.  There is no shell
/// command string and no inherited environment in this contract.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub worker: PathBuf,
    pub workspace: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub network: NetworkCapability,
    pub workspace_access: WorkspaceAccess,
    pub budget: ResourceBudget,
}

impl SandboxSpec {
    /// Create a denied-network spec so callers must opt into every capability
    /// that changes the OS security boundary.
    pub fn denied_network(worker: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            worker: worker.into(),
            workspace: workspace.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            network: NetworkCapability::Denied,
            workspace_access: WorkspaceAccess::ReadWrite,
            budget: ResourceBudget::default(),
        }
    }
}

/// Stable error categories let the host show why a sandbox was not started
/// without leaking full paths or environment values into the UI/log stream.
#[derive(Debug)]
pub enum SandboxError {
    Unsupported,
    InvalidConfig(String),
    Io(std::io::Error),
    Os { operation: &'static str, code: u32 },
    Timeout,
}

impl Display for SandboxError {
    /// Keep diagnostics actionable while intentionally omitting secret-bearing
    /// command lines and absolute paths from the public error text.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("windows AppContainer sandbox is unsupported"),
            Self::InvalidConfig(message) => write!(formatter, "invalid sandbox config: {message}"),
            Self::Io(error) => write!(formatter, "sandbox I/O failed: {error}"),
            Self::Os { operation, code } => {
                write!(formatter, "Windows {operation} failed ({code})")
            }
            Self::Timeout => formatter.write_str("sandbox operation timed out"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    /// Preserve ordinary fixture setup errors as typed recoverable failures.
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A launched worker whose AppContainer profile, ACL grant and process tree
/// remain owned by the returned value until it is explicitly waited or dropped.
#[cfg(windows)]
pub use windows::SandboxChild;

/// Query a fixture PID without relying on process-name or tasklist polling.
#[cfg(windows)]
pub use windows::process_is_alive;

/// Fingerprint a temporary fixture security descriptor for cleanup assertions.
#[cfg(windows)]
pub use windows::acl_fingerprint;

/// Product-side preflight rejects reparse nodes and hardlink descendants before
/// any profile/ACL mutation, so a shared NTFS inode cannot inherit access.
#[cfg(windows)]
pub use windows::reject_reparse_path;

/// Start a native worker inside a real AppContainer and Job Object on Windows.
#[cfg(windows)]
pub fn spawn(spec: SandboxSpec) -> Result<SandboxChild, SandboxError> {
    windows::spawn(spec)
}

/// Refuse to run rather than falling back to a path-only pseudo-sandbox.
#[cfg(not(windows))]
pub fn spawn(_spec: SandboxSpec) -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported)
}

/// A deadline-based wait result that does not expose platform handles to tests.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildOutcome {
    pub exit_code: Option<u32>,
    pub timed_out: bool,
}
