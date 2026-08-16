// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Bounded kill/reap lifecycle for the unified-log helper.

use super::{
    __error, CleanupPhase, CleanupPhaseResult, CleanupPoll, DIAGNOSTIC_DEADLINE, ESRCH, SIGKILL,
    SIGTERM, SandboxDenialDiagnostics, cleanup_decision, kill,
};
use ja_macos_sandbox_spike::{safe_signal_group, safe_signal_pid};
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

impl SandboxDenialDiagnostics {
    /// Kill and bounded-reap while retaining the child handle until the OS
    /// confirms exit; a second failure leaves it for `Drop` to retry.
    pub(super) fn cleanup_child(&mut self, preserve_pipes: bool) -> bool {
        if self.child.is_none() && self.group_empty {
            self.cleanup_failed = false;
            return true;
        }
        if !preserve_pipes {
            self.close_pipes();
        }
        let mut phase = CleanupPhase::FirstAttempt;
        loop {
            match phase {
                CleanupPhase::FirstAttempt => {
                    // Give a cooperative helper a bounded grace period before the
                    // second phase escalates the entire process group.
                    self.kill_helper_group(SIGTERM);
                    self.signal_direct_child(SIGTERM);
                }
                CleanupPhase::RetryAttempt => {
                    self.kill_helper_group(SIGKILL);
                    if let Some(child) = self.child.as_mut() {
                        let _ = child.kill();
                    }
                }
            }
            let poll = self.wait_child_until(Instant::now() + DIAGNOSTIC_DEADLINE);
            match cleanup_decision(phase, poll) {
                CleanupPhaseResult::Reaped => {
                    // Marker deletion is intentionally gated on the group
                    // being observed absent, not merely on direct-child reap.
                    // A reaped parent can still leave a descendant holding
                    // the diagnostic pipe or the helper identity alive.
                    self.cleanup_failed = false;
                    return true;
                }
                CleanupPhaseResult::Retry => {
                    // A second attempt must not retain a writer that could
                    // keep any later drain blocked; the child handle itself
                    // remains owned until wait/reap confirms termination.
                    self.close_pipes();
                    phase = CleanupPhase::RetryAttempt;
                }
                CleanupPhaseResult::Failed => {
                    self.cleanup_failed = true;
                    return false;
                }
            }
        }
    }

    /// Drop both pipe handles before a final group kill so an escaped
    /// descendant cannot keep the diagnostic host boundary open.
    pub(super) fn close_pipes(&mut self) {
        let _ = self.stdout.take();
        let _ = self.stderr.take();
        self.pipes_nonblocking = false;
    }

    /// Signal only the helper's dedicated process group; direct Child
    /// kill/reap remains mandatory because a group signal is not a reap.
    pub(super) fn kill_helper_group(&self, signal: i32) {
        if let Some(group) = self.process_group {
            if group > 1 {
                let _ = safe_signal_group(group, signal);
            }
        }
    }

    /// Signal the direct child without taking its ownership; the caller still
    /// must observe `try_wait` before releasing the `Child` handle.
    fn signal_direct_child(&self, signal: i32) {
        if let Some(child) = self.child.as_ref()
            && let Ok(pid) = i32::try_from(child.id())
            && pid > 1
        {
            let _ = safe_signal_pid(pid, signal);
        }
    }

    /// Poll without taking the `Child`; a timeout or poll error therefore
    /// leaves ownership available for the retry phase and `Drop`.
    pub(super) fn wait_child_until(&mut self, deadline: Instant) -> CleanupPoll {
        loop {
            // A missing or failed nonblocking setup means the pipe may still
            // block; cleanup must close it and poll ownership without reading.
            if self.pipes_nonblocking {
                self.pump();
            }
            let poll = match self.child.as_mut() {
                Some(child) => child.try_wait(),
                None => {
                    if self.group_empty {
                        return CleanupPoll::Reaped;
                    }
                    if self.wait_group_empty_until(deadline) {
                        return CleanupPoll::Reaped;
                    }
                    return CleanupPoll::GroupMembers;
                }
            };
            match poll {
                Ok(Some(_)) => {
                    self.child = None;
                    if self.wait_group_empty_until(deadline) {
                        return CleanupPoll::Reaped;
                    }
                    return CleanupPoll::GroupMembers;
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => return CleanupPoll::Deadline,
                Err(_) => return CleanupPoll::Error,
            }
        }
    }

    /// Require the kernel's `ESRCH` result for the dedicated process group;
    /// any other result is treated as live/unknown so cleanup fails closed.
    fn wait_group_empty_until(&mut self, deadline: Instant) -> bool {
        let Some(process_group) = self.process_group else {
            return self.group_empty;
        };
        loop {
            if process_group_is_empty(process_group) {
                self.group_empty = true;
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// After both bounded polls fail, make one final bounded SIGKILL/reap pass;
    /// an unresolved child remains owned for the explicit abort fail-safe.
    pub(super) fn final_reap_after_bounded_cleanup(&mut self) -> bool {
        if self.child.is_none() && self.group_empty {
            return true;
        }
        self.close_pipes();
        self.kill_helper_group(SIGKILL);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        match self.wait_child_until(Instant::now() + DIAGNOSTIC_DEADLINE) {
            CleanupPoll::Reaped => {
                self.cleanup_failed = false;
                true
            }
            CleanupPoll::GroupMembers | CleanupPoll::Deadline | CleanupPoll::Error => {
                self.cleanup_failed = true;
                false
            }
        }
    }
}

impl Drop for SandboxDenialDiagnostics {
    /// Keep a panic or early-return path from orphaning the optional log
    /// subscriber even when the normal aggregate report was not reached.
    fn drop(&mut self) {
        // A direct child may already have been reaped while an escaped
        // descendant keeps the process group alive; retain the group cleanup
        // phases in that state instead of treating `child == None` as done.
        if self.child.is_some() || !self.group_empty {
            let _ = self.cleanup_child(false);
        }
        if self.child.is_some() || !self.group_empty {
            let _ = self.final_reap_after_bounded_cleanup();
        }
        if self.child.is_none() && self.group_empty {
            // Keep marker activation evidence available to the outer cleanup
            // gate when setup failed; successful helpers may remove both paths.
            if self.unavailable != Some("marker") {
                let _ = self.remove_marker();
            }
        } else {
            // Never let Rust's `Child` destructor silently discard an
            // unconfirmed live helper or descendant group. Flush evidence
            // before the explicit abort so an outer watchdog can audit it.
            self.flush_failure_evidence();
            eprintln!("SANDBOX-DIAGNOSTICS: cleanup=unreaped");
            let _ = std::io::stderr().flush();
            std::process::abort();
        }
    }
}

/// Treat a process group as gone only when `kill(-pgid, 0)` reports ESRCH;
/// EPERM and other errors are deliberately not interpreted as success.
fn process_group_is_empty(process_group: i32) -> bool {
    if process_group <= 1 {
        return false;
    }
    let result = unsafe { kill(-process_group, 0) };
    result == -1 && unsafe { *__error() } == ESRCH
}
