// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native-only integration gate.  A non-macOS build has no passing fallback.

// Keep the cleanup hook/evidence seam executable on the Windows host; the
// native acceptance test below remains macOS-only because it needs Seatbelt.
#[path = "../src/marker_cleanup/hook_seam.rs"]
mod portable_hook_seam;

#[cfg(target_os = "macos")]
mod macos_acceptance {
    use ja_macos_sandbox_spike::run_bounded_command;
    use std::process::Command;
    use std::time::Duration;

    /// Run the real Seatbelt probe twice so profile, xattr, pipe and process
    /// residue cannot hide behind one lucky launch.
    #[test]
    fn seatbelt_and_process_tree_probe_passes_twice() {
        for round in 1..=2 {
            let output = run_bounded_command(
                Command::new(env!("CARGO_BIN_EXE_ja-sandbox-probe")),
                Duration::from_secs(120),
                512 * 1024,
                128 * 1024,
            )
            .expect("spawn macOS sandbox probe");
            assert!(
                output.status.success(),
                "sandbox round {round} failed; status={:?}",
                output.status
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("SANDBOX-PASS"),
                "probe omitted passing marker"
            );
        }
    }
}
