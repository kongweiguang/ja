// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! macOS-only security-boundary probe used before the JA worker adapter is
//! promoted into the Tauri host.  Unsupported hosts fail explicitly rather
//! than turning this smoke test into a path-only pseudo-sandbox.

#[cfg(any(target_os = "macos", test))]
mod path;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub mod process;

#[cfg(target_os = "macos")]
pub mod marker_cleanup;

#[cfg(target_os = "macos")]
pub use macos::{
    ChildOutcome, MAX_OUTPUT_BYTES, RunOutput, SandboxChild, SandboxError, SandboxSpec,
    process_group_is_gone, process_is_alive, spawn,
};

#[cfg(target_os = "macos")]
pub use process::{
    BoundedCommandOutput, run_bounded_command, safe_signal_group, safe_signal_pid, spawn_grouped,
};

/// Keep the crate linkable during Windows/Linux cross-target checks while
/// making the platform boundary visible to callers and CI.
#[cfg(not(target_os = "macos"))]
pub fn platform_supported() -> bool {
    false
}

/// A small process identifier helper is exposed only on macOS because the
/// acceptance probe must verify a real process-group descendant disappeared.
#[cfg(target_os = "macos")]
pub fn platform_supported() -> bool {
    true
}
