// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Thin Tauri adapters for the fixed, read-only Git service.
//!
//! Git command construction, worktree admission, bounded output, and process
//! cleanup remain in `GitReadOnly`; this module only validates wire DTOs and
//! projects stable errors for the WebView.

use crate::app_runtime::{RuntimeHost, WorkspaceLookup};
use crate::workspace_read::WorkspaceHandle;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

use super::{
    CancellationToken, DiffOptions, GitDiff, GitError, GitReadOnly, GitStatusEntry, GitStatusKind,
};

/// Stable Git command categories never echo cwd, argv, or raw stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GitCommandErrorCode {
    NotConfigured,
    UnknownWorkspace,
    InvalidInput,
    InvalidPath,
    ExternalWorktree,
    GitUnavailable,
    CommandFailed,
    TimedOut,
    Cancelled,
    OutputLimitExceeded,
    CleanupTimedOut,
    Parse,
    Io,
}

/// Error DTO intentionally omits the native command, workspace root, and
/// platform diagnostic so it is safe to serialize through Tauri.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GitCommandError {
    pub code: GitCommandErrorCode,
}

impl GitCommandError {
    /// Projects one service error into the small command-facing vocabulary.
    fn from_git(error: GitError) -> Self {
        let code = match error {
            GitError::InvalidPolicy | GitError::InvalidObject => GitCommandErrorCode::InvalidInput,
            GitError::InvalidPath => GitCommandErrorCode::InvalidPath,
            GitError::ExternalWorktree | GitError::Workspace => {
                GitCommandErrorCode::ExternalWorktree
            }
            GitError::GitUnavailable => GitCommandErrorCode::GitUnavailable,
            GitError::CommandFailed { .. } => GitCommandErrorCode::CommandFailed,
            GitError::TimedOut => GitCommandErrorCode::TimedOut,
            GitError::Cancelled => GitCommandErrorCode::Cancelled,
            GitError::OutputLimitExceeded => GitCommandErrorCode::OutputLimitExceeded,
            GitError::CleanupTimedOut => GitCommandErrorCode::CleanupTimedOut,
            GitError::Parse => GitCommandErrorCode::Parse,
            GitError::Spawn { .. } => GitCommandErrorCode::Io,
        };
        Self { code }
    }

    /// Creates an input error before Git discovery or process creation.
    const fn invalid_input() -> Self {
        Self {
            code: GitCommandErrorCode::InvalidInput,
        }
    }
}

impl Display for GitCommandError {
    /// Keeps Tauri's fallback display path stable and free of command details.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.code {
            GitCommandErrorCode::NotConfigured => "workspace is not configured",
            GitCommandErrorCode::UnknownWorkspace => "workspace is unknown",
            GitCommandErrorCode::InvalidInput => "Git request is invalid",
            GitCommandErrorCode::InvalidPath => "Git path is invalid",
            GitCommandErrorCode::ExternalWorktree => "Git worktree is not allowed",
            GitCommandErrorCode::GitUnavailable => "Git is unavailable",
            GitCommandErrorCode::CommandFailed => "Git command failed",
            GitCommandErrorCode::TimedOut => "Git command timed out",
            GitCommandErrorCode::Cancelled => "Git command was cancelled",
            GitCommandErrorCode::OutputLimitExceeded => "Git output exceeded its limit",
            GitCommandErrorCode::CleanupTimedOut => "Git process cleanup timed out",
            GitCommandErrorCode::Parse => "Git output could not be parsed",
            GitCommandErrorCode::Io => "Git read failed",
        })
    }
}

impl std::error::Error for GitCommandError {}

/// Camel-case status projection separates the internal parser model from IPC.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusEntryDto {
    pub kind: GitStatusKind,
    pub index_status: Option<char>,
    pub worktree_status: Option<char>,
    pub path: String,
    pub original_path: Option<String>,
}

/// Binary diff bytes remain a byte array; only the field names are projected.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffDto {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Maps parsed status records without changing Git's read-only semantics.
fn project_status(entries: Vec<GitStatusEntry>) -> Vec<GitStatusEntryDto> {
    entries
        .into_iter()
        .map(|entry| GitStatusEntryDto {
            kind: entry.kind,
            index_status: entry.index_status,
            worktree_status: entry.worktree_status,
            path: entry.path,
            original_path: entry.original_path,
        })
        .collect()
}

/// Maps binary-safe diff output while preserving bytes rather than textifying.
fn project_diff(diff: GitDiff) -> GitDiffDto {
    GitDiffDto {
        bytes: diff.bytes,
        truncated: diff.truncated,
    }
}

/// Addresses the configured workspace for a read-only Git status query.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitStatusInput {
    pub workspace_id: String,
}

/// Restricts the diff command to staged/worktree state and one exact path.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitDiffInput {
    pub workspace_id: String,
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub relative_path: Option<String>,
}

/// Validates the external protocol id before host binding lookup.
fn validate_workspace_id(value: &str) -> bool {
    value.starts_with("ws_")
        && value.len() <= 99
        && value.len() > 3
        && value[3..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Bounds path text at the IPC edge while leaving lexical containment to the
/// shared WorkspaceHandle/Git service policy.
fn validate_relative_path(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Converts host binding failures into the Git command's stable error space.
fn map_lookup(error: WorkspaceLookup) -> GitCommandError {
    GitCommandError {
        code: match error {
            WorkspaceLookup::Unconfigured => GitCommandErrorCode::NotConfigured,
            WorkspaceLookup::Unknown => GitCommandErrorCode::UnknownWorkspace,
        },
    }
}

/// Runs one fixed Git operation while the active binding is held by the host.
fn with_workspace<T>(
    state: &RuntimeHost,
    workspace_id: &str,
    operation: impl FnOnce(&WorkspaceHandle) -> Result<T, GitError>,
) -> Result<T, GitCommandError> {
    if !validate_workspace_id(workspace_id) {
        return Err(GitCommandError::invalid_input());
    }
    state
        .with_configured_workspace(workspace_id, operation)
        .map_err(map_lookup)?
        .map_err(GitCommandError::from_git)
}

/// Reads porcelain-v2 status from the currently configured workspace only.
#[tauri::command]
pub fn ja_git_status(
    input: GitStatusInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<Vec<GitStatusEntryDto>, GitCommandError> {
    with_workspace(&state, &input.workspace_id, |workspace| {
        GitReadOnly::new(workspace.clone())?.status(&CancellationToken::default())
    })
    .map(project_status)
}

/// Reads a binary-safe staged/worktree diff without enabling any Git writes.
#[tauri::command]
pub fn ja_git_diff(
    input: GitDiffInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<GitDiffDto, GitCommandError> {
    if input
        .relative_path
        .as_deref()
        .is_some_and(|path| !validate_relative_path(path))
    {
        return Err(GitCommandError::invalid_input());
    }
    let options = DiffOptions {
        staged: input.staged,
        relative_path: input.relative_path,
    };
    with_workspace(&state, &input.workspace_id, |workspace| {
        GitReadOnly::new(workspace.clone())?.diff(&options, &CancellationToken::default())
    })
    .map(project_diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures command binding uses the protocol id rather than a UUID.
    #[test]
    fn workspace_id_validation_is_strict() {
        assert!(validate_workspace_id("ws_project"));
        assert!(!validate_workspace_id("workspace"));
        assert!(!validate_workspace_id("ws_"));
    }

    /// Confirms diff bytes retain the existing array shape for binary patches.
    #[test]
    fn diff_bytes_keep_binary_shape() {
        let diff = GitDiff {
            bytes: vec![0, 255, 10],
            truncated: false,
        };
        let value = serde_json::to_value(diff).expect("serialize binary diff");
        assert_eq!(value["bytes"], serde_json::json!([0, 255, 10]));
    }

    /// Locks status field naming at the Tauri boundary without exposing the
    /// parser model's snake_case field names.
    #[test]
    fn status_projection_is_camel_case() {
        let value = serde_json::to_value(project_status(vec![GitStatusEntry {
            kind: GitStatusKind::Changed,
            index_status: Some('M'),
            worktree_status: Some(' '),
            path: "src/main.rs".to_owned(),
            original_path: None,
        }]))
        .expect("serialize status DTO");
        assert_eq!(value[0]["indexStatus"], "M");
        assert!(value[0].get("index_status").is_none());
    }
}
