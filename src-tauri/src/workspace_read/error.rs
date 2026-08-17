// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use std::io;
use thiserror::Error;

/// Stable, path-redacted errors keep filesystem details out of the IPC layer.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace root is invalid")]
    InvalidRoot,
    #[error("workspace is not registered")]
    WorkspaceNotFound,
    #[error("relative path is invalid")]
    InvalidRelativePath,
    #[error("path is outside the workspace")]
    OutsideWorkspace,
    #[error("workspace path identity changed")]
    PathChanged,
    #[error("symlink or reparse point is not readable through this policy")]
    LinkNotAllowed,
    #[error("path does not exist")]
    PathNotFound,
    #[error("path is not a directory")]
    NotDirectory,
    #[error("path is not a regular file")]
    NotFile,
    #[error("workspace entry budget exceeded")]
    EntryBudgetExceeded,
    #[error("workspace depth limit exceeded")]
    DepthLimitExceeded,
    #[error("workspace scan deadline exceeded")]
    ScanDeadlineExceeded,
    #[error("tree cursor is stale and requires a fresh snapshot")]
    StaleCursor,
    #[error("file exceeds the configured size limit")]
    FileTooLarge,
    #[error("file changed while it was being read")]
    ChangedDuringRead,
    #[error("I/O operation {operation} failed: {kind}")]
    Io {
        operation: &'static str,
        kind: io::ErrorKind,
    },
}

impl WorkspaceError {
    /// Redacts the path while preserving enough operation context for the UI
    /// to choose a retry or a recovery action.
    pub(crate) fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            kind: error.kind(),
        }
    }
}
