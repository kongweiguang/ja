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
pub use macos::{
    ChildOutcome, MAX_OUTPUT_BYTES, RunOutput, SandboxChild, SandboxError, SandboxSpec,
    kill_process, process_is_alive, spawn,
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
