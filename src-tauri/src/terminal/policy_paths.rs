// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! workspace/cwd canonicalization and containment.
//!
//! This small boundary is intentionally limited to mature filesystem APIs;
//! the terminal policy rejects an escaped canonical cwd without inventing a
//! second identity protocol or platform-specific handle proof.

use super::super::error::{TerminalError, TerminalErrorCode, map_io};
use std::path::{Path, PathBuf};

/// Resolve a relative root against the host process before canonicalization.
pub(super) fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Canonicalize an existing directory so the PTY receives a stable cwd.
pub(super) fn canonical_directory(path: &Path) -> Result<PathBuf, TerminalError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| map_io(TerminalErrorCode::InvalidCwd, &error))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| map_io(TerminalErrorCode::InvalidCwd, &error))?;
    if metadata.is_dir() {
        Ok(canonical)
    } else {
        Err(TerminalError::new(TerminalErrorCode::InvalidCwd))
    }
}

/// Compare path components rather than raw strings, preventing a sibling
/// directory from being accepted as a child of the workspace.
pub(super) fn path_is_within(root: &Path, candidate: &Path) -> bool {
    let root_components = root.components().collect::<Vec<_>>();
    let candidate_components = candidate.components().collect::<Vec<_>>();
    candidate_components.len() >= root_components.len()
        && root_components
            .iter()
            .zip(candidate_components.iter())
            .all(|(left, right)| {
                #[cfg(windows)]
                {
                    left.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
                }
                #[cfg(not(windows))]
                {
                    left == right
                }
            })
}
