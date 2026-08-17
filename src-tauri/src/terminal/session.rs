// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端 session supervisor 与 worker ownership。
//!
//! 每个 session 把 child、master、reader、writer、resize worker 和 byte queues
//! 绑定到同一个 generation。关闭先发终态、再终止进程树并等待 worker，防止 late
//! output 重新污染已经关闭的 UI terminal。

use super::error::{TerminalError, TerminalErrorCode};
use super::model::{
    CloseReason, LaunchRequest, TerminalEvent, TerminalEventKind, TerminalId, TerminalSize,
};
use super::policy::TerminalPolicy;
use super::process::{self, ProcessTree};
use super::queue::{EventQueue, InputQueue};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, native_pty_system};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "session_workers.rs"]
mod session_workers;

use session_workers::{ResizeQueue, WorkerReap, WorkerTracker};

/// 管理 workspace 下的多个用户终端，并为每个 id 保留唯一 owner。
#[derive(Clone)]
pub struct TerminalSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    policy: TerminalPolicy,
    sessions: Mutex<HashMap<TerminalId, Arc<TerminalRuntime>>>,
}

impl TerminalSupervisor {
    /// 由 host 以一个已验证 workspace policy 创建 supervisor。
    pub fn new(policy: TerminalPolicy) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                policy,
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Reports the number of owned sessions so workspace reconfiguration and
    /// shutdown can refuse to abandon a live PTY tree.
    pub fn active_count(&self) -> usize {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 启动受控 shell；同一 supervisor 的 session 数受 policy 上限保护。
    pub fn open(&self, request: LaunchRequest) -> Result<SessionHandle, TerminalError> {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A SessionHandle can close itself without holding the supervisor;
        // reclaim terminal generations here so a closed UI tab cannot consume
        // the global session quota forever.
        sessions.retain(|_, runtime| !runtime.is_reclaimable());
        if sessions.len() >= self.inner.policy.limits().max_sessions {
            return Err(TerminalError::new(TerminalErrorCode::SessionLimit));
        }
        let id = TerminalId::new();
        let generation = 1;
        let runtime = TerminalRuntime::spawn(id, generation, self.inner.policy.clone(), request)?;
        sessions.insert(id, runtime.clone());
        Ok(SessionHandle {
            runtime,
            generation,
        })
    }

    /// 以 generation 获取现有 owner，拒绝旧 UI 持有的 stale token。
    pub fn get(&self, id: TerminalId, generation: u64) -> Result<SessionHandle, TerminalError> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runtime = sessions
            .get(&id)
            .cloned()
            .ok_or(TerminalError::new(TerminalErrorCode::SessionNotFound))?;
        runtime.validate_token(generation)?;
        Ok(SessionHandle {
            runtime,
            generation,
        })
    }

    /// 删除唯一 owner 后执行 bounded close，避免关闭完成前被新请求复用。
    pub fn close(
        &self,
        id: TerminalId,
        generation: u64,
        reason: CloseReason,
    ) -> Result<(), TerminalError> {
        let runtime = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runtime = sessions
                .get(&id)
                .cloned()
                .ok_or(TerminalError::new(TerminalErrorCode::SessionNotFound))?;
            runtime.validate_token(generation)?;
            runtime
        };
        let result = runtime.close(reason);
        if result.is_ok() {
            let mut sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if sessions
                .get(&id)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &runtime))
            {
                sessions.remove(&id);
            }
        }
        result
    }

    /// 应用退出时按同一个绝对 deadline 收口全部 terminal process trees。
    pub fn shutdown_until(&self, deadline: Instant) -> Result<(), TerminalError> {
        let runtimes = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions
                .iter()
                .map(|(id, runtime)| (*id, runtime.clone()))
                .collect::<Vec<_>>()
        };
        let mut failed = false;
        for (id, runtime) in runtimes {
            if runtime.close_until(CloseReason::Shutdown, deadline).is_ok() {
                let mut sessions = self
                    .inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if sessions
                    .get(&id)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &runtime))
                {
                    sessions.remove(&id);
                }
            } else {
                failed = true;
            }
        }
        if failed {
            Err(TerminalError::new(TerminalErrorCode::WorkerShutdownTimeout))
        } else {
            Ok(())
        }
    }
}

impl Drop for TerminalSupervisor {
    /// The final supervisor owner performs one bounded shutdown pass so the
    /// map never silently drops live PTY workers without an explicit error.
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            let deadline = Instant::now()
                .checked_add(self.inner.policy.limits().operation_timeout)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
            let _ = self.shutdown_until(deadline);
        }
    }
}

/// 一个 generation 的终端句柄；clone 不会创建第二个 owner，只共享同一 bounded runtime。
#[derive(Clone)]
pub struct SessionHandle {
    runtime: Arc<TerminalRuntime>,
    generation: u64,
}

impl SessionHandle {
    /// 返回不透明 session id，前端可持久化但不能自行生成合法 owner。
    pub fn id(&self) -> TerminalId {
        self.runtime.id
    }

    /// 返回用于拒绝 late event/request 的 generation。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 在 timeout 到期前向 single writer queue 追加原始 input bytes。
    pub fn send_input(&self, data: &[u8], timeout: Duration) -> Result<(), TerminalError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(TerminalError::new(TerminalErrorCode::DeadlineExceeded))?;
        self.send_input_until(data, deadline)
    }

    /// 绝对 deadline 版本供 Tauri command 和 shutdown 编排复用。
    pub fn send_input_until(&self, data: &[u8], deadline: Instant) -> Result<(), TerminalError> {
        self.runtime.validate_open(self.generation)?;
        if Instant::now() >= deadline {
            return Err(TerminalError::new(TerminalErrorCode::DeadlineExceeded));
        }
        let max = self.runtime.limits.max_input_chunk_bytes;
        if data.is_empty() || data.len() > max {
            return Err(TerminalError::new(TerminalErrorCode::InputTooLarge));
        }
        self.runtime.input.push(data.to_vec())
    }

    /// 提交最新尺寸；resize worker 只保留最后一个 pending value 以避免拖垮 PTY。
    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        self.runtime.validate_open(self.generation)?;
        if !size.validate() {
            return Err(TerminalError::new(TerminalErrorCode::InvalidSize));
        }
        self.runtime.resize(size)
    }

    /// 以绝对 deadline 消费一条已经批量化的 terminal event。
    pub fn recv_until(&self, deadline: Instant) -> Result<Option<TerminalEvent>, TerminalError> {
        self.runtime.validate_token(self.generation)?;
        Ok(self.runtime.events.recv_until(deadline))
    }

    /// 读取最近 bounded scrollback；bytes 不做 UTF-8 解码和重写。
    pub fn scrollback(&self) -> Result<Vec<u8>, TerminalError> {
        self.runtime.validate_token(self.generation)?;
        Ok(self.runtime.scrollback())
    }

    /// 幂等关闭当前 generation，并在 host deadline 内等待 worker 结束。
    pub fn close(&self, reason: CloseReason) -> Result<(), TerminalError> {
        self.runtime.validate_token(self.generation)?;
        self.runtime.close(reason)
    }

    /// 将用户取消映射为同一幂等 close 路径，避免额外的未收口 cancellation worker。
    pub fn cancel(&self) -> Result<(), TerminalError> {
        self.close(CloseReason::Timeout)
    }
}

struct TerminalRuntime {
    id: TerminalId,
    generation: u64,
    limits: super::policy::TerminalLimits,
    input: Arc<InputQueue>,
    events: Arc<EventQueue>,
    resize_queue: Arc<ResizeQueue>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    process_tree: Arc<dyn ProcessTree>,
    stop: AtomicBool,
    lifecycle: Mutex<Lifecycle>,
    scrollback: Mutex<Scrollback>,
    workers: Arc<WorkerTracker>,
}

struct Lifecycle {
    terminal_sent: bool,
    closed: bool,
    reader_done: bool,
    exit_status: Option<portable_pty::ExitStatus>,
}

struct Scrollback {
    chunks: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
    limit: usize,
}

impl Scrollback {
    /// 保留最近 bytes，超过上限时从最旧 chunk 淘汰而不是无限累积。
    fn append(&mut self, mut data: Vec<u8>) {
        if data.len() >= self.limit {
            let start = data.len().saturating_sub(self.limit);
            data = data.split_off(start);
            self.chunks.clear();
            self.bytes = 0;
        }
        self.bytes = self.bytes.saturating_add(data.len());
        self.chunks.push_back(data);
        while self.bytes > self.limit {
            if let Some(chunk) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(chunk.len());
            } else {
                self.bytes = 0;
            }
        }
    }

    /// 将 bounded chunk 拼成快照；只有上限范围内的数据会被复制给 caller。
    fn snapshot(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.bytes);
        for chunk in &self.chunks {
            result.extend_from_slice(chunk);
        }
        result
    }
}

impl TerminalRuntime {
    /// 创建 PTY、绑定 tree guard，再启动四个有明确 ownership 的 worker。
    fn spawn(
        id: TerminalId,
        generation: u64,
        policy: TerminalPolicy,
        request: LaunchRequest,
    ) -> Result<Arc<Self>, TerminalError> {
        let limits = policy.limits();
        let prepared = policy.prepare(&request)?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(session_workers::to_pty_size(prepared.size))
            .map_err(|error| {
                tracing::debug!(error = %error, "terminal PTY open failed");
                TerminalError::new(TerminalErrorCode::PtyFailed)
            })?;
        let reader = pair.master.try_clone_reader().map_err(|error| {
            tracing::debug!(error = %error, "terminal PTY reader clone failed");
            TerminalError::new(TerminalErrorCode::PtyFailed)
        })?;
        let writer = pair.master.take_writer().map_err(|error| {
            tracing::debug!(error = %error, "terminal PTY writer acquisition failed");
            TerminalError::new(TerminalErrorCode::PtyFailed)
        })?;
        let mut command = CommandBuilder::new(&prepared.shell.program);
        command.args(&prepared.shell.args);
        command.cwd(&prepared.cwd);
        command.env_clear();
        for (key, value) in &prepared.environment {
            command.env(key, value);
        }
        let mut child = pair.slave.spawn_command(command).map_err(|error| {
            tracing::debug!(error = %error, "terminal shell spawn failed");
            TerminalError::new(TerminalErrorCode::SpawnFailed)
        })?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let tree = match process::attach(child.as_ref()) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let runtime = Arc::new(Self {
            id,
            generation,
            limits,
            input: Arc::new(InputQueue::new(limits.max_input_queue_bytes)),
            events: Arc::new(EventQueue::new(
                id,
                generation,
                limits.max_output_queue_bytes,
                limits.max_event_count,
            )),
            resize_queue: Arc::new(ResizeQueue::new()),
            master: Arc::new(Mutex::new(Some(pair.master))),
            killer: Arc::new(Mutex::new(killer)),
            process_tree: Arc::from(tree),
            stop: AtomicBool::new(false),
            lifecycle: Mutex::new(Lifecycle {
                terminal_sent: false,
                closed: false,
                reader_done: false,
                exit_status: None,
            }),
            scrollback: Mutex::new(Scrollback {
                chunks: std::collections::VecDeque::new(),
                bytes: 0,
                limit: limits.max_scrollback_bytes,
            }),
            workers: Arc::new(WorkerTracker::new(session_workers::WORKER_COUNT)),
        });
        let spawn_result = session_workers::spawn_workers(&runtime, reader, writer, child);
        if spawn_result.is_err() {
            let _ = runtime.request_stop();
            let deadline = Instant::now()
                .checked_add(limits.operation_timeout)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
            let _ = runtime.workers.wait_until(deadline);
            runtime.events.close();
            return Err(TerminalError::new(TerminalErrorCode::SpawnFailed));
        }
        Ok(runtime)
    }

    /// 检查 caller token 是否仍指向同一 session generation。
    fn validate_token(&self, generation: u64) -> Result<(), TerminalError> {
        if generation != self.generation {
            return Err(TerminalError::new(TerminalErrorCode::StaleGeneration));
        }
        Ok(())
    }

    /// command/resize 需要 open 状态；recv/scrollback 则只需要 generation token。
    fn validate_open(&self, generation: u64) -> Result<(), TerminalError> {
        self.validate_token(generation)?;
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.closed || lifecycle.terminal_sent {
            Err(TerminalError::new(TerminalErrorCode::SessionClosed))
        } else {
            Ok(())
        }
    }

    /// Closed or naturally exited runtimes no longer occupy a supervisor slot.
    fn is_reclaimable(&self) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_sent
            && self.workers.is_reaped()
    }

    /// 以 latest-value semantics 写入 resize queue，避免窗口拖动产生无限请求。
    fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        self.resize_queue.set(size)
    }

    /// 记录 output 到 scrollback 与事件队列，任何 overflow 都转成明确终态。
    fn publish_output(&self, data: Vec<u8>) -> bool {
        let allowed = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !lifecycle.closed
                && !lifecycle.terminal_sent
                // A child exit requests process-tree cleanup before the PTY
                // reaches EOF. Keep accepting buffered bytes in that narrow
                // state; explicit close/failure still rejects late output.
                && (!self.stop.load(Ordering::Acquire) || lifecycle.exit_status.is_some())
        };
        if !allowed {
            return false;
        }
        self.scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(data.clone());
        if self.publish_output_event(data) {
            true
        } else {
            self.fail(TerminalErrorCode::OutputLimitExceeded);
            false
        }
    }

    /// 独立函数把 queue 失败与 lifecycle lock 分开，避免持锁调用 event queue。
    fn publish_output_event(&self, data: Vec<u8>) -> bool {
        self.events.push_output(data)
    }

    /// ConPTY may report a temporary zero-byte read while the child remains
    /// interactive; only an observed terminal/close state permits reader exit.
    fn reader_can_finish(&self) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.terminal_sent || lifecycle.closed || lifecycle.exit_status.is_some()
    }

    /// 发布 resize 事件；resize failure 由 worker 映射为稳定错误。
    fn publish_control(&self, kind: TerminalEventKind) {
        if !self.events.push_control(kind) {
            self.fail(TerminalErrorCode::OutputLimitExceeded);
        }
    }

    /// 只发布一次 Error 终态，并立即停止进程树。
    fn fail(&self, code: TerminalErrorCode) {
        let should_publish = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lifecycle.terminal_sent {
                false
            } else {
                lifecycle.terminal_sent = true;
                lifecycle.closed = true;
                true
            }
        };
        if should_publish {
            self.events
                .push_terminal(TerminalEventKind::Error { code: code as u16 });
        }
        let _ = self.request_stop();
        self.events.close();
    }

    /// 正常 child exit 也必须杀掉可能仍存活的 descendants，再通知 UI。
    fn child_exited(&self, status: portable_pty::ExitStatus) {
        let status_to_publish = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lifecycle.terminal_sent {
                None
            } else if lifecycle.reader_done {
                lifecycle.terminal_sent = true;
                Some(status)
            } else {
                lifecycle.exit_status = Some(status);
                None
            }
        };
        let _ = self.process_tree.terminate();
        // Keep the master alive while the reader drains bytes already buffered
        // by the PTY; closing it here can lose the final shell output.
        let _ = self.request_stop_preserve_master();
        if let Some(status) = status_to_publish {
            self.events.push_terminal(TerminalEventKind::Exited {
                code: status.exit_code(),
                signal: status.signal().map(ToOwned::to_owned),
            });
            self.events.close();
        }
    }

    /// reader EOF 证明 PTY backlog 已经消费完，再发布 Exited 以免丢失最后一批 bytes。
    fn reader_finished(&self) {
        let status_to_publish = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lifecycle.reader_done = true;
            if lifecycle.terminal_sent {
                None
            } else if let Some(status) = lifecycle.exit_status.take() {
                lifecycle.terminal_sent = true;
                Some(status)
            } else {
                None
            }
        };
        if let Some(status) = status_to_publish {
            self.events.push_terminal(TerminalEventKind::Exited {
                code: status.exit_code(),
                signal: status.signal().map(ToOwned::to_owned),
            });
            self.events.close();
        }
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Close the PTY master after the bounded post-exit drain window so a
    /// platform reader that blocks waiting for ConPTY EOF cannot outlive the
    /// session forever.
    fn drop_master(&self) {
        self.master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// 用户 close 立即变成终态，再使用同一 deadline 等待所有 worker。
    fn close(&self, reason: CloseReason) -> Result<(), TerminalError> {
        let deadline = Instant::now()
            .checked_add(self.limits.operation_timeout)
            .ok_or(TerminalError::new(TerminalErrorCode::DeadlineExceeded))?;
        self.close_until(reason, deadline)
    }

    /// shutdown 复用一个绝对 deadline，避免每个 session 单独延长应用退出时间。
    fn close_until(&self, reason: CloseReason, deadline: Instant) -> Result<(), TerminalError> {
        let should_publish = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lifecycle.terminal_sent {
                lifecycle.closed = true;
                false
            } else {
                lifecycle.terminal_sent = true;
                lifecycle.closed = true;
                true
            }
        };
        if should_publish {
            self.events
                .push_terminal(TerminalEventKind::Closed { reason });
        }
        let cleanup = self.request_stop();
        let workers_stopped = self.workers.wait_until(deadline);
        self.events.close();
        match (cleanup.is_ok(), workers_stopped) {
            (true, WorkerReap::Complete) => Ok(()),
            (false, _) => Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed)),
            (_, WorkerReap::Timeout | WorkerReap::JoinFailed) => {
                Err(TerminalError::new(TerminalErrorCode::WorkerShutdownTimeout))
            }
        }
    }

    /// 发送 stop、关闭 queues、终止 tree 和释放 master，所有调用都可重复。
    fn request_stop(&self) -> Result<(), TerminalError> {
        self.request_stop_inner(true)
    }

    /// child exit uses this variant so buffered PTY output can be drained first.
    fn request_stop_preserve_master(&self) -> Result<(), TerminalError> {
        self.request_stop_inner(false)
    }

    /// Shared stop path; repeated calls still release master when a close deadline needs it.
    fn request_stop_inner(&self, drop_master: bool) -> Result<(), TerminalError> {
        let first_stop = !self.stop.swap(true, Ordering::AcqRel);
        if first_stop {
            self.input.close();
            self.resize_queue.close();
        }
        let mut first_error = self.process_tree.terminate().err();
        // The direct child killer is a fallback only. Calling it after a
        // successful Job Object/process-group termination turns an expected
        // already-exited child into a false cleanup failure.
        if first_error.is_some()
            && let Err(error) = self
                .killer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .kill()
        {
            first_error.get_or_insert(error);
        }
        if drop_master {
            self.master
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
        if let Some(error) = first_error {
            tracing::debug!(error_kind = ?error.kind(), "terminal process cleanup failed");
            Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed))
        } else {
            Ok(())
        }
    }

    /// 将 scrollback 快照限定为内部 byte budget，避免 UI 请求导致二次无界增长。
    fn scrollback(&self) -> Vec<u8> {
        self.scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }
}

impl Drop for TerminalRuntime {
    /// supervisor 崩溃或 owner 被遗弃时仍尽力终止 child；Drop 不阻塞等待线程。
    fn drop(&mut self) {
        let _ = self.request_stop();
        self.events.close();
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
