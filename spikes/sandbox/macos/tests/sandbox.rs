// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native-only integration gate.  A non-macOS build has no passing fallback.

#[cfg(target_os = "macos")]
mod macos_acceptance {
    use std::process::Command;

    /// Run the real Seatbelt probe twice so profile, xattr, pipe and process
    /// residue cannot hide behind one lucky launch.
    #[test]
    fn seatbelt_and_process_tree_probe_passes_twice() {
        for round in 1..=2 {
            let output = Command::new(env!("CARGO_BIN_EXE_ja-sandbox-probe"))
                .output()
                .expect("spawn macOS sandbox probe");
            assert!(
                output.status.success(),
                "sandbox round {round} failed; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("SANDBOX-PASS"),
                "probe omitted passing marker"
            );
        }
    }
}
