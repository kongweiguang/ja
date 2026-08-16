// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Workflow cleanup wrapper; all native behavior lives in the crate module.

#[cfg(target_os = "macos")]
fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let Err(category) = ja_macos_sandbox_spike::marker_cleanup::run_cli(&arguments) {
        eprintln!("SANDBOX-MARKER-CLEANUP: {category}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("SANDBOX-MARKER-CLEANUP: macOS-only");
    std::process::exit(2);
}
