// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Bounded host diagnostics for the native Seatbelt probe.

use super::super::{
    CleanupPhase, CleanupPhaseResult, CleanupPoll, cleanup_decision, diagnostic_pump_allowed,
    marker_identity_safe,
};
use std::collections::BTreeMap;
use std::env;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::Duration;

const DIAGNOSTIC_MAX_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_MAX_LINE_BYTES: usize = 8 * 1024;
const DIAGNOSTIC_MAX_EVENTS: usize = 256;
const DIAGNOSTIC_MAX_KEYS: usize = 64;
const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(3);
const DIAGNOSTIC_PREDICATE: &str = "subsystem == 'com.apple.sandbox.reporting'";

/// A safe aggregate key for the small whitelist emitted by sandbox denial
/// diagnostics; no raw log line or path is retained.
#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
struct DiagnosticKey {
    operation: String,
    category: String,
    process: String,
}

/// Host-side bounded reader for Apple's sandbox reporting subsystem.
/// It is inactive outside CI or an explicit probe diagnostic request.
pub(super) struct SandboxDenialDiagnostics {
    enabled: bool,
    child: Option<Child>,
    process_group: Option<i32>,
    marker_path: Option<PathBuf>,
    fallback_marker_path: Option<PathBuf>,
    emergency_marker_path: Option<PathBuf>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    line_buffer: Vec<u8>,
    bytes: usize,
    events: usize,
    truncated: bool,
    unavailable: Option<&'static str>,
    cleanup_failed: bool,
    read_error: Option<&'static str>,
    pipes_nonblocking: bool,
    group_empty: bool,
    counts: BTreeMap<DiagnosticKey, usize>,
}

mod capture;
mod evidence;
mod lifecycle;
mod marker;
mod query;
mod redaction;
#[cfg(test)]
mod tests;

#[cfg(test)]
use marker::{
    PreparedHelperMarker, marker_contents, marker_directory_mode_safe, write_marker_file,
    write_prepared_marker,
};
use marker::{activate_helper_marker, prepare_helper_marker};
use query::process_start_identity_owned;
#[cfg(test)]
use query::{drain_query_pipe, process_start_identity};

impl SandboxDenialDiagnostics {
    /// Start `log stream` only for CI/explicit diagnostics; a missing log
    /// utility never weakens or skips a Seatbelt assertion.
    pub(super) fn start() -> Self {
        if !diagnostics_enabled() {
            return Self::disabled();
        }
        let prepared_marker = match prepare_helper_marker() {
            Ok(marker) => marker,
            Err(_) => return Self::unavailable("marker-preparation"),
        };
        let mut child = match Command::new("/usr/bin/log")
            .args([
                "stream",
                "--style",
                "ndjson",
                "--predicate",
                DIAGNOSTIC_PREDICATE,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dedicated group lets cleanup target only this log helper
            // and any descendants, never the runner's other log clients.
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                let mut diagnostics = Self::unavailable("spawn");
                diagnostics.marker_path = Some(prepared_marker.path);
                diagnostics.fallback_marker_path = Some(prepared_marker.fallback_path);
                diagnostics.emergency_marker_path = Some(prepared_marker.emergency_path);
                let _ = diagnostics.remove_marker();
                return diagnostics;
            }
        };
        let process_group_id = i32::try_from(child.id())
            .ok()
            .filter(|group| marker_identity_safe(*group, std::process::id() as i32));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut diagnostics = Self {
            enabled: true,
            child: Some(child),
            process_group: process_group_id,
            marker_path: Some(prepared_marker.path.clone()),
            fallback_marker_path: Some(prepared_marker.fallback_path.clone()),
            emergency_marker_path: Some(prepared_marker.emergency_path.clone()),
            stdout,
            stderr,
            line_buffer: Vec::new(),
            bytes: 0,
            events: 0,
            truncated: false,
            unavailable: None,
            cleanup_failed: false,
            read_error: None,
            pipes_nonblocking: false,
            group_empty: false,
            counts: BTreeMap::new(),
        };
        // Construct the owner before any post-spawn query. A failed or
        // timed-out identity query must still flow through the same owned
        // kill/reap path instead of dropping a live Child at this boundary.
        let marker_failed = match (
            diagnostics.process_group,
            diagnostics.child.as_ref().map(Child::id),
        ) {
            (Some(group), Some(pid)) => match process_start_identity_owned(&mut diagnostics, pid) {
                Some(start_identity) => {
                    activate_helper_marker(&prepared_marker, pid, group, &start_identity).is_err()
                }
                None => true,
            },
            _ => false,
        };
        if marker_failed {
            diagnostics.unavailable = Some("marker");
            let _ = diagnostics.stdout.take();
            let _ = diagnostics.stderr.take();
            diagnostics.cleanup_failed = !diagnostics.cleanup_child(false);
            diagnostics.enabled = false;
            return diagnostics;
        }
        if diagnostics.process_group.is_none() {
            diagnostics.unavailable = Some("process-group");
            let _ = diagnostics.stdout.take();
            let _ = diagnostics.stderr.take();
            diagnostics.cleanup_failed = !diagnostics.cleanup_child(false);
            diagnostics.enabled = false;
            return diagnostics;
        }
        if diagnostics.stdout.is_none() || diagnostics.stderr.is_none() {
            diagnostics.unavailable = Some("pipe");
            let _ = diagnostics.stdout.take();
            let _ = diagnostics.stderr.take();
            diagnostics.cleanup_failed = !diagnostics.cleanup_child(false);
            diagnostics.enabled = false;
            return diagnostics;
        }
        let stdout_ready = diagnostics
            .stdout
            .as_ref()
            .map(set_nonblocking)
            .unwrap_or(false);
        let stderr_ready = diagnostics
            .stderr
            .as_ref()
            .map(set_nonblocking)
            .unwrap_or(false);
        if !stdout_ready || !stderr_ready {
            diagnostics.unavailable = Some("nonblocking");
            let _ = diagnostics.stdout.take();
            let _ = diagnostics.stderr.take();
            diagnostics.cleanup_failed = !diagnostics.cleanup_child(false);
            diagnostics.enabled = false;
        } else {
            diagnostics.pipes_nonblocking = true;
        }
        diagnostics
    }

    /// Construct an inactive diagnostic object without touching host logs.
    fn disabled() -> Self {
        Self {
            enabled: false,
            child: None,
            process_group: None,
            marker_path: None,
            fallback_marker_path: None,
            emergency_marker_path: None,
            stdout: None,
            stderr: None,
            line_buffer: Vec::new(),
            bytes: 0,
            events: 0,
            truncated: false,
            unavailable: None,
            cleanup_failed: false,
            read_error: None,
            pipes_nonblocking: false,
            counts: BTreeMap::new(),
            group_empty: true,
        }
    }

    /// Keep optional diagnostics non-fatal while preserving a safe reason
    /// for CI logs if the host does not expose `log stream`.
    fn unavailable(reason: &'static str) -> Self {
        let mut diagnostics = Self::disabled();
        diagnostics.unavailable = Some(reason);
        diagnostics
    }

    /// Kill and bounded-reap the helper, then emit only aggregate safe
    /// fields. Failure is surfaced so diagnostics cannot orphan `log`.
    pub(super) fn finish(&mut self) -> Result<(), String> {
        if !self.enabled {
            let cleanup_ok = self.cleanup_child(false);
            let marker_ok = if self.child.is_none() && self.group_empty {
                // Preserve prepared/partial marker evidence when activation
                // failed; the outer workflow must audit that exact owner
                // identity instead of losing the cleanup contract.
                if self.unavailable == Some("marker") {
                    false
                } else {
                    self.remove_marker()
                }
            } else {
                false
            };
            if let Some(reason) = self.unavailable {
                eprintln!("SANDBOX-DIAGNOSTICS: unavailable={reason}");
            }
            let setup_failed = matches!(
                self.unavailable,
                Some("marker" | "marker-preparation" | "process-group")
            );
            return if !cleanup_ok || !marker_ok || self.cleanup_failed {
                Err("sandbox diagnostics helper cleanup failed".into())
            } else if setup_failed {
                Err("sandbox diagnostics setup failed".into())
            } else {
                Ok(())
            };
        }
        self.pump();
        let cleanup_ok = self.cleanup_child(true) && !self.cleanup_failed;
        // The child is confirmed reaped before this final drain, so EOF
        // can be observed without risking a writer held by a live helper.
        self.pump();
        self.record_complete_lines();
        self.record_remaining_line();
        self.close_pipes();
        let marker_ok = if self.child.is_none() && self.group_empty {
            self.remove_marker()
        } else {
            false
        };
        if let Some(reason) = self.read_error {
            eprintln!("SANDBOX-DIAGNOSTICS: unavailable={reason}");
        }
        for (key, count) in &self.counts {
            eprintln!(
                "SANDBOX-DIAGNOSTIC: operation={}; category={}; process={}; count={count}",
                key.operation, key.category, key.process
            );
        }
        eprintln!(
            "SANDBOX-DIAGNOSTICS: events={}; bytes={}; truncated={}",
            self.events, self.bytes, self.truncated
        );
        if cleanup_ok && marker_ok && self.read_error.is_none() {
            Ok(())
        } else if self.read_error.is_some() {
            Err("sandbox diagnostics read failed".into())
        } else if !marker_ok {
            Err("sandbox diagnostics cleanup marker failed".into())
        } else {
            Err("sandbox diagnostics helper cleanup failed".into())
        }
    }
}

/// Enable host log diagnostics only for CI or an explicit probe request;
/// normal local runs remain quiet and do not inspect the unified log.
fn diagnostics_enabled() -> bool {
    diagnostics_enabled_from(
        env::var("JA_SANDBOX_DIAGNOSTICS").ok().as_deref(),
        env::var("CI").ok().as_deref(),
    )
}

/// Keep environment interpretation pure so tests can cover the opt-in
/// contract without racing process-global variables.
fn diagnostics_enabled_from(explicit: Option<&str>, ci: Option<&str>) -> bool {
    fn enabled(value: Option<&str>) -> bool {
        matches!(value, Some("1")) || value.is_some_and(|value| value.eq_ignore_ascii_case("true"))
    }

    enabled(explicit) || enabled(ci)
}

/// Set a log pipe nonblocking so a noisy or stalled system reporter cannot
/// turn the diagnostic helper into an unbounded wait.
fn set_nonblocking<R: AsRawFd>(reader: &R) -> bool {
    let flags = unsafe { fcntl(reader.as_raw_fd(), F_GETFL) };
    flags != -1 && unsafe { fcntl(reader.as_raw_fd(), F_SETFL, flags | O_NONBLOCK) } != -1
}

const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0x0004;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const ESRCH: i32 = 3;

// The raw calls are limited to the probe-owned log pipes and process group so
// a noisy unified-log producer cannot make diagnostic cleanup unbounded.
unsafe extern "C" {
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn __error() -> *mut i32;
}
