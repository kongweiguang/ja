// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use std::io;
use thiserror::Error;

/// Git errors omit command lines, cwd and raw stderr so a future IPC mapper
/// cannot accidentally expose a user path or inherited secret.
#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git policy is invalid")]
    InvalidPolicy,
    #[error("Git executable is unavailable")]
    GitUnavailable,
    #[error("Git path argument is invalid")]
    InvalidPath,
    #[error("Git object selector is invalid")]
    InvalidObject,
    #[error("external Git worktree metadata is not allowed")]
    ExternalWorktree,
    #[error("Git command could not be started: {kind}")]
    Spawn { kind: io::ErrorKind },
    #[error("Git command failed with exit code {code:?}")]
    CommandFailed { code: Option<i32> },
    #[error("Git command timed out")]
    TimedOut,
    #[error("Git command was cancelled")]
    Cancelled,
    #[error("Git output exceeded the configured cap")]
    OutputLimitExceeded,
    #[error("Git process cleanup exceeded its deadline")]
    CleanupTimedOut,
    #[error("Git output could not be parsed")]
    Parse,
    #[error("workspace error")]
    Workspace,
}
