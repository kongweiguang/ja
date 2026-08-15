// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Cross-platform path preparation shared by the native sandbox adapter.
//! Keeping canonicalization independent from macOS policy code lets Windows
//! CI exercise alias handling without pretending to run Seatbelt.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Canonical paths captured once before profile generation and process spawn.
/// Reusing this value prevents profile/command alias drift across `/var` and
/// `/private/var`, symlinked workspaces, or other filesystem aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedPaths {
    pub(crate) worker: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) profile_path: PathBuf,
}

/// Resolve all path aliases once while retaining the requested profile file
/// name under its canonical parent for an atomic create-new operation later.
pub(crate) fn prepare_paths(
    worker: &Path,
    workspace: &Path,
    profile_path: &Path,
) -> io::Result<PreparedPaths> {
    let worker = fs::canonicalize(worker)?;
    let workspace = fs::canonicalize(workspace)?;
    let profile_parent = profile_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "profile has no parent"))?;
    let profile_name = profile_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "profile has no file name"))?;
    let profile_parent = fs::canonicalize(profile_parent)?;
    Ok(PreparedPaths {
        worker,
        workspace,
        profile_path: profile_parent.join(profile_name),
    })
}

#[cfg(test)]
mod tests {
    use super::prepare_paths;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Verify aliases are resolved once so the prepared command/profile paths
    /// are identical even when the caller supplied `.` or `..` components.
    #[test]
    fn preparation_resolves_worker_workspace_and_profile_parent() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ja-path-preparation-{}-{nanos}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let resource = root.join("resource");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&resource).expect("create resource");
        let worker = resource.join("worker");
        fs::write(&worker, b"worker").expect("create worker");

        let aliased_workspace = workspace.join(".").join("..").join("workspace");
        let aliased_worker = resource.join(".").join("worker");
        let aliased_profile = root.join("resource").join("..").join("profile.sb");
        let prepared = prepare_paths(&aliased_worker, &aliased_workspace, &aliased_profile)
            .expect("prepare aliased paths");

        assert_eq!(
            prepared.worker,
            fs::canonicalize(&worker).expect("canonical worker")
        );
        assert_eq!(
            prepared.workspace,
            fs::canonicalize(&workspace).expect("canonical workspace")
        );
        assert_eq!(
            prepared.profile_path,
            fs::canonicalize(&root)
                .expect("canonical profile parent")
                .join("profile.sb")
        );

        fs::remove_dir_all(PathBuf::from(&root)).expect("remove path fixture");
    }
}
