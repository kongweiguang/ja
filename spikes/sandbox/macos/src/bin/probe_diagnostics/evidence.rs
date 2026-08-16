// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Durable marker evidence and failure-path removal.

use super::SandboxDenialDiagnostics;
use super::marker::{
    marker_open_flags, sync_parent_directory, validate_marker_file, validate_marker_file_identity,
    validate_marker_path_identity,
};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Remove one marker while retaining its path when unlink fails so workflow
/// cleanup can still identify the unresolved helper.
fn remove_marker_path(slot: &mut Option<PathBuf>) -> bool {
    let Some(path) = slot.as_ref() else {
        return true;
    };
    let expected = match OpenOptions::new()
        .read(true)
        .custom_flags(marker_open_flags())
        .open(path)
        .and_then(|file| validate_marker_file(&file))
    {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            *slot = None;
            return true;
        }
        Err(_) => return false,
    };
    // Re-open and re-check the identity immediately before unlink so a
    // same-user path replacement cannot make cleanup delete a new file.
    let current = OpenOptions::new()
        .read(true)
        .custom_flags(marker_open_flags())
        .open(path)
        .and_then(|file| {
            validate_marker_file_identity(&file, expected)?;
            Ok(file)
        });
    let Ok(current) = current else {
        return false;
    };
    if validate_marker_path_identity(path, expected).is_err() {
        return false;
    }
    // Keep the descriptor alive through unlink so the second identity check
    // is not separated from the path operation by another open/close window.
    match fs::remove_file(path) {
        Ok(()) => {
            drop(current);
            if sync_parent_directory(path).is_err() {
                return false;
            }
            *slot = None;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            drop(current);
            let _ = sync_parent_directory(path);
            *slot = None;
            true
        }
        Err(_) => {
            drop(current);
            false
        }
    }
}

impl SandboxDenialDiagnostics {
    /// Flush all marker descriptors and their parent directory before an
    /// explicit abort, so external cleanup has the strongest available proof.
    pub(super) fn flush_failure_evidence(&self) {
        for path in [
            self.marker_path.as_deref(),
            self.fallback_marker_path.as_deref(),
            self.emergency_marker_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(file) = OpenOptions::new()
                .read(true)
                .custom_flags(marker_open_flags())
                .open(path)
            {
                let _ = validate_marker_file(&file);
                let _ = file.sync_all();
            }
            let _ = sync_parent_directory(path);
        }
        let _ = io::stderr().flush();
    }

    /// Remove all marker paths only after the helper is confirmed reaped and
    /// its process group reports ESRCH; an unlink failure retains evidence.
    pub(super) fn remove_marker(&mut self) -> bool {
        // Do not short-circuit: the fallback marker is the cleanup evidence
        // that matters when removing the primary path fails.
        let primary_removed = remove_marker_path(&mut self.marker_path);
        let fallback_removed = remove_marker_path(&mut self.fallback_marker_path);
        let emergency_removed = remove_marker_path(&mut self.emergency_marker_path);
        primary_removed && fallback_removed && emergency_removed
    }
}
