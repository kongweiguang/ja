// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Child-process ownership and sidecar stdio startup.

use super::config::SidecarConfig;
use crate::agent_process::error::AgentProcessError;
use crate::agent_process::process_tree::{ProcessTreeGuard, bounded_reap_child};
use crate::agent_process::session::{Session, TerminalCallback, TerminalReason};
use std::collections::VecDeque;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct RunningProcess {
    child: Mutex<Option<Child>>,
    tree: ProcessTreeGuard,
    exited: AtomicBool,
    exit_wake: Condvar,
    exit_lock: Mutex<()>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TerminalSignal {
    pub(super) generation: u64,
    pub(super) reason: TerminalReason,
}

impl RunningProcess {
    /// 终止完整 process group/job，而不是只杀 Java 直接子进程。
    pub(super) fn terminate_tree(&self) -> Result<(), AgentProcessError> {
        self.terminate_tree_until(reap_deadline())
    }

    /// 在 caller 给定的绝对 deadline 内完成 fallback reap，避免 shutdown 超时被 cleanup 延长。
    pub(super) fn terminate_tree_until(&self, deadline: Instant) -> Result<(), AgentProcessError> {
        let result = self.tree.terminate();
        // Keep the Child in RunningProcess for its entire lifetime.  The OS
        // tree adapter is supplemental; bounded reap through this original
        // handle is required even when Job/process-group termination fails.
        let reap_error = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .and_then(|child| bounded_reap_child(child, deadline).err());
        if result.is_err() || reap_error.is_some() {
            Err(AgentProcessError::ProcessTree)
        } else {
            Ok(())
        }
    }

    /// 在 deadline 内等待 monitor 发布退出事实，不能持有 child 锁阻塞 shutdown。
    pub(super) fn wait_until(&self, deadline: Instant) -> bool {
        let mut lock = self
            .exit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.exited.load(Ordering::Acquire) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .exit_wake
                .wait_timeout(lock, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lock = next;
            if wait.timed_out() {
                return self.exited.load(Ordering::Acquire);
            }
        }
    }

    /// 记录 monitor 的单次退出事实并唤醒所有 deadline waiter。
    pub(super) fn mark_exited(&self) -> Result<(), AgentProcessError> {
        // A leader exit does not prove descendants exited; terminate the owned
        // group/job before publishing the event so consumers never observe a
        // dead session while its grandchild is still executing.
        let result = self.tree.mark_exited();
        self.exited.store(true, Ordering::Release);
        self.exit_wake.notify_all();
        result.map_err(|_| AgentProcessError::ProcessTree)
    }
}

/// 启动并绑定完整进程树，任何管道/线程失败都在返回前 kill+wait。
pub(super) fn spawn_process(
    config: &SidecarConfig,
    generation: u64,
    terminal_signals: Arc<Mutex<VecDeque<TerminalSignal>>>,
) -> Result<(Arc<RunningProcess>, Session), AgentProcessError> {
    config.verify_executable_identity()?;
    let mut command = Command::new(config.canonical_executable());
    command
        .args(&config.args)
        .current_dir(config.canonical_run_dir())
        .env_clear()
        .envs(config.env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let tree = ProcessTreeGuard::prepare(&mut command).map_err(|_| AgentProcessError::Spawn)?;
    let mut child = command.spawn().map_err(|_| AgentProcessError::Spawn)?;
    if tree
        .assign(&child)
        .and_then(|_| tree.resume(&child))
        .is_err()
    {
        return Err(cleanup_spawned_child(&tree, &mut child).unwrap_or(AgentProcessError::Spawn));
    }
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => return Err(abort_spawned_child(&tree, &mut child)),
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return Err(abort_spawned_child(&tree, &mut child)),
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return Err(abort_spawned_child(&tree, &mut child)),
    };
    let process = Arc::new(RunningProcess {
        child: Mutex::new(Some(child)),
        tree,
        exited: AtomicBool::new(false),
        exit_wake: Condvar::new(),
        exit_lock: Mutex::new(()),
    });
    // A stale client may outlive the supervisor; weak capture prevents that
    // client from retaining the OS process/job after lifecycle detachment.
    let terminal_process = Arc::downgrade(&process);
    let terminal_callback: TerminalCallback = Arc::new(move |reason| {
        let _ = terminal_process
            .upgrade()
            .map(|process| process.terminate_tree());
        terminal_signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(TerminalSignal { generation, reason });
    });
    let session = match Session::from_io_with_terminal(
        stdout,
        stdin,
        stderr,
        generation,
        config.limits.clone(),
        Some(terminal_callback),
    ) {
        Ok(session) => session,
        Err(error) => {
            let cleanup_error = process.terminate_tree_until(reap_deadline()).err();
            if cleanup_error.is_some() {
                return Err(AgentProcessError::ProcessTree);
            }
            return Err(error);
        }
    };
    let monitor_process = Arc::clone(&process);
    let monitor_session = session.clone();
    thread::Builder::new()
        .name("ja-sidecar-monitor".to_owned())
        .spawn(move || {
            // Probe the retained Child under a short mutex scope.  No blocking
            // wait owns this lock, so shutdown can always enforce its deadline.
            loop {
                let status = monitor_process
                    .child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                    .map(Child::try_wait);
                match status {
                    Some(Ok(Some(status))) => {
                        if monitor_process.mark_exited().is_err() {
                            monitor_session.report_process_fault();
                        } else {
                            monitor_session.report_process_exit(status.code());
                        }
                        break;
                    }
                    Some(Ok(None)) => thread::park_timeout(Duration::from_millis(10)),
                    Some(Err(_)) => {
                        // An error cannot prove the leader is gone; bounded
                        // cleanup keeps the original handle owned and observable.
                        let _ = monitor_process.terminate_tree_until(reap_deadline());
                        let _ = monitor_process.mark_exited();
                        monitor_session.report_process_fault();
                        break;
                    }
                    None => {
                        let _ = monitor_process.mark_exited();
                        monitor_session.report_process_fault();
                        break;
                    }
                }
            }
        })
        .map_err(|_| {
            let _ = process.terminate_tree_until(reap_deadline());
            AgentProcessError::Spawn
        })?;
    Ok((process, session))
}

/// spawn 后任一 stdio pipe 缺失都必须收口 suspended/job child，不能依赖 Drop 偶然清理。
/// 收口已经 spawn 但无法接入 session 的 child，避免 suspended/zombie 泄露。
fn abort_spawned_child(tree: &ProcessTreeGuard, child: &mut Child) -> AgentProcessError {
    cleanup_spawned_child(tree, child).unwrap_or(AgentProcessError::Spawn)
}

/// kill/wait 直接 child，同时尝试 tree adapter；失败只改变可观察错误，不放弃收口。
fn cleanup_spawned_child(tree: &ProcessTreeGuard, child: &mut Child) -> Option<AgentProcessError> {
    // Direct kill/reap is first so a failed Job call can never block leader
    // cleanup; the tree adapter then makes a best-effort descendant sweep.
    let deadline = reap_deadline();
    let reap_error = bounded_reap_child(child, deadline).is_err();
    let tree_error = tree.terminate().is_err();
    if tree_error || reap_error {
        Some(AgentProcessError::ProcessTree)
    } else {
        None
    }
}

/// 为每次收口创建独立绝对 deadline，避免等待时间叠加成无界 cleanup。
fn reap_deadline() -> Instant {
    Instant::now()
        .checked_add(MAX_REAP_TIMEOUT)
        .unwrap_or_else(Instant::now)
}
