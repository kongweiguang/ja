// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native macOS acceptance probe.  It intentionally fails on non-macOS and
//! never turns unavailable Seatbelt into a successful path-policy result.

#[cfg(not(target_os = "macos"))]
/// Reject non-native execution rather than silently running an unsandboxed
/// path test on the development host.
fn main() {
    eprintln!("SANDBOX-UNSUPPORTED: macOS Seatbelt probe requires a native macOS runner");
    std::process::exit(2);
}

#[cfg(any(target_os = "macos", test))]
mod probe_lifecycle;

#[cfg(any(target_os = "macos", test))]
use probe_lifecycle::{
    CleanupPhase, CleanupPhaseResult, CleanupPoll, cleanup_decision, diagnostic_pump_allowed,
    marker_identity_safe,
};

#[cfg(test)]
use probe_lifecycle::{CleanupOwnership, child_ownership};

#[cfg(target_os = "macos")]
mod native;

#[cfg(target_os = "macos")]
/// Execute the native probe and turn any missing enforcement into a hard CI
/// failure for the platform matrix.
fn main() {
    if let Err(error) = native::run() {
        eprintln!("SANDBOX-FAIL: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "probe_unit_tests/mod.rs"]
mod cleanup_state_tests;
