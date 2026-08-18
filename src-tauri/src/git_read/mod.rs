// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Fixed-command, read-only Git adapter for the workbench.
//!
//! The public surface contains no arbitrary argv escape hatch.  Every command
//! is built from a typed operation, runs in the canonical workspace cwd, and
//! has bounded output, timeout, cancellation and process-tree cleanup.

mod command;
pub mod commands;
mod error;
mod model;
mod parse;
mod process;

pub use command::{CancellationToken, DiffOptions, GitPolicy, GitReadOnly};
pub use commands::{
    GitCommandError, GitCommandErrorCode, GitDiffDto, GitDiffInput, GitStatusEntryDto,
    GitStatusInput, ja_git_diff, ja_git_status,
};
pub use error::GitError;
pub use model::{GitDiff, GitLogEntry, GitShow, GitStatusEntry, GitStatusKind};

#[cfg(test)]
mod tests;
