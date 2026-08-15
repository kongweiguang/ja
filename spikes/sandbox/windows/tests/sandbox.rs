// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Real Windows integration tests.  A failure to create a native boundary is
//! surfaced as a test failure; this suite never converts an unavailable
//! AppContainer API into a passing policy/mock result.

#[cfg(windows)]
mod windows_acceptance {
    use std::process::Command;

    /// Run the probe twice to catch profile/ACL/process-tree residue that can
    /// remain hidden in a single successful launch.
    #[test]
    fn appcontainer_escape_and_job_tree_probe_passes_twice() {
        for round in 1..=2 {
            let output = Command::new(env!("CARGO_BIN_EXE_ja-sandbox-probe"))
                .output()
                .expect("spawn sandbox probe");
            assert!(
                output.status.success(),
                "sandbox round {round} failed; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("SANDBOX-PASS"),
                "probe did not emit the passing marker"
            );
        }
    }
}
