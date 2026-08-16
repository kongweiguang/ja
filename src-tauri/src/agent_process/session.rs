// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 全双工 JSONL session。
//!
//! stdout 永久 reader、stdin single-writer actor、stderr 独立 drain 和 pending
//! registry 各自拥有单一职责；这样 Java 等待 Rust approval 时 reader 仍可收发。

#[path = "wire.rs"]
mod wire;

use crate::agent_process::codec::{self, Limits, RpcFrame};
use crate::agent_process::error::{AgentProcessError, QueueKind, error_catalog};
use crate::agent_process::handshake::valid_ready_token;
use crate::agent_process::pending::{PendingRegistry, ResolveDisposition, deadline_after};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use wire::{
    DEFAULT_WRITE_WATCHDOG_TIMEOUT, EventPriority, MAX_WRITER_DATA_QUEUE_BYTES, WriterHandle,
    control_queue_byte_budget, fail_closed, push_event, spawn_reader, spawn_stderr_reader,
    writer_loop,
};

const CONTROL_QUEUE_CAPACITY: usize = 64;
const MAX_INBOUND_SERVER_REQUEST_LEDGER: usize = 8_192;
const MAX_OUTBOUND_REQUEST_LEDGER: usize = 8_192;
const STDERR_REDACTED_SUMMARY: &str = "sidecar stderr output redacted";
const MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(3_600);
const MAX_WRITER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// session 对外暴露的事件；响应在 pending 内部消费，避免 caller 误把未知 response 当业务事实。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    ServerRequest(RpcFrame),
    Notification(RpcFrame),
    StderrLine(String),
    StderrTruncated,
    ResponseRejected(ResolveDisposition),
    ProtocolFault(codec::CodecError),
    HandshakeFailed,
    Eof,
    QueueOverflow(QueueKind),
    QueueFatalOverflow(QueueKind),
    ProcessExited { generation: u64, code: Option<i32> },
}

#[path = "events.rs"]
mod events;
use events::EventQueue;

/// 进程 stdin/stdout/stderr 被拆成泛型 IO 后，测试可以用内存 pipe 而不启动真实 Java。
pub struct Session {
    inner: Arc<SessionInner>,
    writer_join: Arc<WriterJoinState>,
}

/// 唯一外部事件消费句柄；不实现 Clone，避免多个 UI reducer 竞争同一事件序列。
pub struct EventPump {
    inner: Arc<SessionInner>,
}

/// supervisor 对外唯一的非 Clone 事件句柄别名，明确事件 reducer 只有一个 owner。
pub type SupervisorEventPump = EventPump;

impl EventPump {
    /// 由唯一 owner 消费事件；request 等待路径使用独立 server-request 队列。
    pub fn next_event(&mut self, timeout: Duration) -> Option<SessionEvent> {
        self.inner.events.pop(timeout)
    }
}

/// 连接终止原因供唯一 lifecycle owner 区分 fault、自然退出和主动关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    Fault,
    ProcessExited,
    Closed,
}

/// 连接终态回调；由 supervisor 注入以便协议线程立即终止其拥有的进程树。
pub type TerminalCallback = Arc<dyn Fn(TerminalReason) + Send + Sync + 'static>;

type NestedRequestHandler<'a> = Option<&'a mut dyn FnMut(&Session, RpcFrame)>;

struct WriterCompletion {
    done: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

impl WriterCompletion {
    /// 创建 writer completion gate，使生命周期 owner 可等待而不轮询。
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            wait_lock: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// 标记 writer actor 已返回，唤醒所有 bounded join waiter。
    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// 在绝对 deadline 内等待 writer actor 结束，避免 shutdown 持锁或无界 sleep。
    fn wait_until(&self, deadline: Instant) -> bool {
        if self.done.load(Ordering::Acquire) {
            return true;
        }
        let mut lock = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if self.done.load(Ordering::Acquire) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .wake
                .wait_timeout(lock, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lock = next;
            if wait.timed_out() {
                return self.done.load(Ordering::Acquire);
            }
        }
    }
}

struct WriterJoinState {
    handle: Mutex<Option<thread::JoinHandle<()>>>,
    completion: Arc<WriterCompletion>,
}

impl WriterJoinState {
    /// 创建 writer handle 所有权槽，保证成功启动的 actor 不被立即 detached。
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            completion: Arc::new(WriterCompletion::new()),
        }
    }
}

struct SessionInner {
    generation: u64,
    limits: Limits,
    events: Arc<EventQueue>,
    pending: Mutex<PendingRegistry>,
    inbound_server_requests: Mutex<InboundServerRequests>,
    outbound_request_ids: Mutex<OutboundRequestLedger>,
    writer: WriterHandle,
    write_timeout: Duration,
    closed: Arc<AtomicBool>,
    /// ready promotion and terminal close share this mutex so no fault can be
    /// observed between frame validation and lifecycle mark_ready.
    ready_terminal_gate: Mutex<()>,
    event_pump_claimed: AtomicBool,
    terminal_callback: Mutex<Option<TerminalCallback>>,
    /// 只有 writer 成功 flush initialized 后才置位，防止预排队 ready 晋级。
    initialized_confirmed: AtomicBool,
    initialized_lock: Mutex<()>,
    initialized_wake: Condvar,
    /// 当前 generation 的一次性 challenge；token 只存在内存中且不进入诊断。
    ready_token_challenge: Mutex<Option<String>>,
    /// 当前 generation 的 token 只用于递归拒绝重放，不对外暴露原值。
    forbidden_ready_tokens: Mutex<HashSet<String>>,
    /// initialized notification 只能成功发送一次，重复发送必须终止握手。
    initialized_sent: AtomicBool,
    /// ready notification 先由 reader 原子占位，再由 lifecycle owner 消费一次。
    ready_notification_claimed: AtomicBool,
    ready_token_consumed: AtomicBool,
    stderr_bytes: AtomicU64,
    stderr_truncated: AtomicBool,
    writer_data_overflow_reported: AtomicBool,
}

struct InboundServerRequests {
    active: HashSet<String>,
    seen: HashSet<String>,
    max: usize,
}

struct OutboundRequestLedger {
    seen: HashSet<String>,
    next: u64,
}

impl SessionInner {
    /// Reader 先占用唯一 ready 槽位，防止多个 ready 在 lifecycle owner 处理前排队。
    fn claim_ready_notification(&self) -> bool {
        self.initialized_confirmed.load(Ordering::Acquire)
            && self
                .ready_notification_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
}

impl OutboundRequestLedger {
    /// 为 c: request 分配连接内永不复用的 ID，达到硬上限时轮换 session。
    fn new() -> Self {
        Self {
            seen: HashSet::with_capacity(MAX_OUTBOUND_REQUEST_LEDGER),
            next: 1,
        }
    }

    /// 原子地登记下一个 outbound ID，防止并发 caller 复用或绕过上限。
    fn allocate(&mut self) -> Result<String, AgentProcessError> {
        if self.seen.len() >= MAX_OUTBOUND_REQUEST_LEDGER {
            return Err(AgentProcessError::RequestLedgerExhausted);
        }
        let number = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(AgentProcessError::RequestLedgerExhausted)?;
        let id = format!("c:rpc-{number}");
        if !self.seen.insert(id.clone()) {
            return Err(AgentProcessError::RequestLedgerExhausted);
        }
        Ok(id)
    }
}

impl InboundServerRequests {
    /// 对嵌套 server request 设独立上限，并为连接保留不可复用的 ID ledger。
    fn new(max: usize) -> Self {
        Self {
            active: HashSet::new(),
            seen: HashSet::with_capacity(max.min(MAX_INBOUND_SERVER_REQUEST_LEDGER)),
            max,
        }
    }

    /// 登记 server request；ledger 达到硬上限时必须终止连接，不能静默淘汰旧 ID。
    fn register(&mut self, id: &str) -> Result<(), AgentProcessError> {
        if self.seen.contains(id) {
            return Err(AgentProcessError::DuplicateRequest);
        }
        if self.active.len() >= self.max {
            return Err(AgentProcessError::PendingLimit);
        }
        if self.seen.len() >= MAX_INBOUND_SERVER_REQUEST_LEDGER {
            return Err(AgentProcessError::RequestLedgerExhausted);
        }
        self.seen.insert(id.to_owned());
        self.active.insert(id.to_owned());
        Ok(())
    }

    /// 只允许 active request 被回应；已消费 ID 永久留在 ledger，避免响应重放。
    fn resolve(&mut self, id: &str) -> Result<(), AgentProcessError> {
        if !self.active.remove(id) {
            if self.seen.contains(id) {
                return Err(AgentProcessError::DuplicateResponse);
            }
            return Err(AgentProcessError::UnknownRequestId);
        }
        Ok(())
    }
}

impl Session {
    /// 启动没有外部终止回调的 session，供纯协议测试或已由上层托管生命周期的 IO 使用。
    pub fn from_io<R, W, E>(
        reader: R,
        writer: W,
        stderr: E,
        generation: u64,
        limits: Limits,
    ) -> Result<Self, AgentProcessError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        E: Read + Send + 'static,
    {
        Self::from_io_with_terminal(reader, writer, stderr, generation, limits, None)
    }

    /// 启动 reader/writer/stderr 三条单向线程，并把终态直接转发给进程树 owner。
    pub fn from_io_with_terminal<R, W, E>(
        reader: R,
        writer: W,
        stderr: E,
        generation: u64,
        limits: Limits,
        terminal_callback: Option<TerminalCallback>,
    ) -> Result<Self, AgentProcessError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        E: Read + Send + 'static,
    {
        Self::from_io_with_terminal_timeout(
            reader,
            writer,
            stderr,
            generation,
            limits,
            terminal_callback,
            DEFAULT_WRITE_WATCHDOG_TIMEOUT,
        )
    }

    /// Test-only constructor injects a short watchdog without changing the
    /// production timeout, allowing deterministic blocked-stdio regressions.
    #[cfg(test)]
    pub fn from_io_with_terminal_watchdog<R, W, E>(
        reader: R,
        writer: W,
        stderr: E,
        generation: u64,
        limits: Limits,
        terminal_callback: Option<TerminalCallback>,
        write_timeout: Duration,
    ) -> Result<Self, AgentProcessError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        E: Read + Send + 'static,
    {
        Self::from_io_with_terminal_timeout(
            reader,
            writer,
            stderr,
            generation,
            limits,
            terminal_callback,
            write_timeout,
        )
    }

    /// Build all pumps with one explicit write deadline so timeout ownership
    /// remains in Session rather than hidden in a transport implementation.
    fn from_io_with_terminal_timeout<R, W, E>(
        reader: R,
        writer: W,
        stderr: E,
        generation: u64,
        limits: Limits,
        terminal_callback: Option<TerminalCallback>,
        write_timeout: Duration,
    ) -> Result<Self, AgentProcessError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        E: Read + Send + 'static,
    {
        limits.validate()?;
        let events = Arc::new(EventQueue::new(
            limits.inbound_queue_frames,
            limits.max_frame_bytes,
        ));
        let closed = Arc::new(AtomicBool::new(false));
        let (control, control_receiver) = mpsc::sync_channel(CONTROL_QUEUE_CAPACITY);
        let (data, data_receiver) = mpsc::sync_channel(limits.outbound_queue_frames);
        let control_queued_bytes = Arc::new(AtomicUsize::new(0));
        let data_queued_bytes = Arc::new(AtomicUsize::new(0));
        let writer_handle = WriterHandle::new(
            control,
            data,
            Arc::clone(&closed),
            control_queued_bytes,
            data_queued_bytes,
            control_queue_byte_budget(limits.max_frame_bytes),
            MAX_WRITER_DATA_QUEUE_BYTES,
        );
        let max_active_requests = limits
            .max_pending_requests
            .min(limits.max_in_flight_requests);
        let pending = PendingRegistry::new(max_active_requests, limits.max_tombstones)?;
        let writer_join = Arc::new(WriterJoinState::new());
        let inner = Arc::new(SessionInner {
            generation,
            limits: limits.clone(),
            events: Arc::clone(&events),
            pending: Mutex::new(pending),
            inbound_server_requests: Mutex::new(InboundServerRequests::new(max_active_requests)),
            outbound_request_ids: Mutex::new(OutboundRequestLedger::new()),
            writer: writer_handle,
            write_timeout,
            closed: Arc::clone(&closed),
            ready_terminal_gate: Mutex::new(()),
            event_pump_claimed: AtomicBool::new(false),
            terminal_callback: Mutex::new(terminal_callback),
            initialized_confirmed: AtomicBool::new(false),
            initialized_lock: Mutex::new(()),
            initialized_wake: Condvar::new(),
            ready_token_challenge: Mutex::new(None),
            forbidden_ready_tokens: Mutex::new(HashSet::new()),
            initialized_sent: AtomicBool::new(false),
            ready_notification_claimed: AtomicBool::new(false),
            ready_token_consumed: AtomicBool::new(false),
            stderr_bytes: AtomicU64::new(0),
            stderr_truncated: AtomicBool::new(false),
            writer_data_overflow_reported: AtomicBool::new(false),
        });
        let writer_inner = Arc::clone(&inner);
        let writer_completion = Arc::clone(&writer_join.completion);
        let writer_thread = thread::Builder::new()
            .name("ja-sidecar-writer".to_owned())
            .spawn(move || {
                writer_loop(writer, control_receiver, data_receiver, writer_inner);
                writer_completion.mark_done();
            })
            .map_err(|_| {
                fail_closed(&inner);
                AgentProcessError::Spawn
            })?;
        *writer_join
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer_thread);
        if let Err(error) = spawn_reader(reader, Arc::clone(&inner)) {
            fail_closed(&inner);
            if join_writer_state_until(
                &writer_join,
                Instant::now()
                    .checked_add(MAX_WRITER_JOIN_GRACE)
                    .unwrap_or_else(Instant::now),
            )
            .is_err()
            {
                std::process::abort();
            }
            return Err(error);
        }
        if let Err(error) = spawn_stderr_reader(stderr, Arc::clone(&inner)) {
            fail_closed(&inner);
            if join_writer_state_until(
                &writer_join,
                Instant::now()
                    .checked_add(MAX_WRITER_JOIN_GRACE)
                    .unwrap_or_else(Instant::now),
            )
            .is_err()
            {
                std::process::abort();
            }
            return Err(error);
        }
        Ok(Self { inner, writer_join })
    }

    /// 向 Java 发起 client request；等待期间 reader 线程仍可处理 server request。
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RpcFrame, AgentProcessError> {
        self.request_inner(method, params, timeout, None, None)
    }

    /// 在线性化准入锁内完成 pending 注册与 writer 入队，关闭不会留下竞态窗口。
    pub fn request_with_gate(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        gate: &Mutex<bool>,
    ) -> Result<RpcFrame, AgentProcessError> {
        self.request_inner(method, params, timeout, None, Some(gate))
    }

    /// shutdown 等必须等待 response 的宿主操作可在等待期间拒绝嵌套 server request，避免互相等待。
    pub fn request_with_server_request_handler<F>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        mut handler: F,
    ) -> Result<RpcFrame, AgentProcessError>
    where
        F: FnMut(&Session, RpcFrame),
    {
        self.request_inner(method, params, timeout, Some(&mut handler), None)
    }

    /// 共用 request 编码/pending 约束；可选 handler 只在显式 reentrant 调用中消费嵌套事件。
    fn request_inner(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        mut handler: NestedRequestHandler<'_>,
        admission: Option<&Mutex<bool>>,
    ) -> Result<RpcFrame, AgentProcessError> {
        if timeout > MAX_OPERATION_TIMEOUT {
            return Err(AgentProcessError::InvalidTimeout);
        }
        let admission_guard = if let Some(gate) = admission {
            let guard = gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *guard {
                return Err(AgentProcessError::ShuttingDown);
            }
            Some(guard)
        } else {
            None
        };
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        // Reject challenge markers before allocating an irreversible request ID;
        // only the two exact handshake notification paths may carry a token.
        if contains_forbidden_ready_token(&params, &self.inner.forbidden_ready_tokens) {
            return Err(AgentProcessError::HandshakeFailed);
        }
        let id = {
            let mut ledger = self
                .inner
                .outbound_request_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match ledger.allocate() {
                Ok(id) => id,
                Err(error) => {
                    fail_closed(&self.inner);
                    return Err(error);
                }
            }
        };
        let frame = RpcFrame::client_request(id.clone(), method.to_owned(), params)?;
        // 只有完整编码并成功送入 writer 后才消耗 initialized 一次性状态，避免本地编码失败让握手永久卡死。
        let encoded = match frame.encode(self.inner.limits.max_frame_bytes) {
            Ok(encoded) => encoded,
            Err(error) => {
                if method == "initialized" {
                    self.inner.initialized_sent.store(false, Ordering::Release);
                }
                return Err(error.into());
            }
        };
        let receiver = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.register(id, deadline_after(timeout)?)?
        };
        let priority = if method == "shutdown" || method.starts_with("runtime/") {
            EventPriority::Control
        } else {
            EventPriority::Data
        };
        if let Err(error) = self.inner.writer.send(encoded, priority) {
            handle_writer_error(&self.inner, &error);
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = pending.cancel(frame.id());
            drop(admission_guard);
            return Err(error);
        }
        // Only admission is serialized; response waiting remains concurrent.
        drop(admission_guard);
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(AgentProcessError::InvalidTimeout)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.expire(Instant::now());
                return Err(AgentProcessError::DeadlineExceeded);
            }
            let wait = if handler.is_some() {
                remaining.min(Duration::from_millis(20))
            } else {
                remaining
            };
            match receiver.recv_timeout(wait) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AgentProcessError::SessionClosed);
                }
                Err(RecvTimeoutError::Timeout) if handler.is_some() => {
                    while let Some(frame) = self.inner.events.pop_server_request(Duration::ZERO) {
                        if let Some(handler) = handler.as_deref_mut() {
                            handler(self, frame);
                        }
                    }
                    if self.inner.closed.load(Ordering::Acquire) {
                        return Err(AgentProcessError::SessionClosed);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let mut pending = self
                        .inner
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    pending.expire(Instant::now());
                    return Err(AgentProcessError::DeadlineExceeded);
                }
            }
        }
    }

    /// 发送 initialized/普通事件；控制通知进入控制队列以保证握手不被 delta 饿死。
    pub fn notify(&self, method: &str, params: Value) -> Result<(), AgentProcessError> {
        self.notify_inner(method, params, None)
    }

    /// 在与 shutdown 共用的准入锁内发送 notification，阻止 stale client 越过停止线。
    pub fn notify_with_gate(
        &self,
        method: &str,
        params: Value,
        gate: &Mutex<bool>,
    ) -> Result<(), AgentProcessError> {
        self.notify_inner(method, params, Some(gate))
    }

    /// 共用 notification 编码和 admission 线性化，避免普通 notify 绕过 shutdown gate。
    fn notify_inner(
        &self,
        method: &str,
        params: Value,
        admission: Option<&Mutex<bool>>,
    ) -> Result<(), AgentProcessError> {
        let admission_guard = admission.map(|gate| {
            gate.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if admission_guard.as_ref().is_some_and(|guard| **guard) {
            return Err(AgentProcessError::ShuttingDown);
        }
        if method == "initialized" {
            self.validate_initialized_params(&params)?;
            if self
                .inner
                .initialized_sent
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(AgentProcessError::HandshakeFailed);
            }
            self.inner
                .initialized_confirmed
                .store(false, Ordering::Release);
            self.inner
                .ready_token_consumed
                .store(false, Ordering::Release);
        } else if contains_forbidden_ready_token(&params, &self.inner.forbidden_ready_tokens) {
            return Err(AgentProcessError::HandshakeFailed);
        }
        let frame = match RpcFrame::notification(method.to_owned(), params) {
            Ok(frame) => frame,
            Err(error) => {
                if method == "initialized" {
                    self.inner.initialized_sent.store(false, Ordering::Release);
                }
                return Err(error.into());
            }
        };
        let priority = if method == "initialized" || method.starts_with("runtime/") {
            EventPriority::Control
        } else {
            EventPriority::Data
        };
        // 只有完整编码并成功送入 writer 后才消耗 initialized 一次性状态，避免本地编码失败让握手永久卡死。
        let encoded = match frame.encode(self.inner.limits.max_frame_bytes) {
            Ok(encoded) => encoded,
            Err(error) => {
                if method == "initialized" {
                    self.inner.initialized_sent.store(false, Ordering::Release);
                }
                return Err(error.into());
            }
        };
        let result = self.inner.writer.send(encoded, priority);
        if let Err(error) = &result {
            handle_writer_error(&self.inner, error);
            if method == "initialized" {
                self.inner.initialized_sent.store(false, Ordering::Release);
            }
        }
        result
    }

    /// 只接受当前 generation 的单字段 initialized challenge，防止重复或嵌套伪造。
    fn validate_initialized_params(&self, params: &Value) -> Result<(), AgentProcessError> {
        let expected = self
            .inner
            .ready_token_challenge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(AgentProcessError::HandshakeFailed)?;
        let valid = params.as_object().is_some_and(|object| {
            object.len() == 1
                && object.get("readyToken").and_then(Value::as_str) == Some(expected.as_str())
                && valid_ready_token(&expected)
        });
        if valid {
            Ok(())
        } else {
            Err(AgentProcessError::HandshakeFailed)
        }
    }

    /// 为 supervisor 构造当前 generation 的唯一 initialized DTO，不让 token 成为公开 accessor。
    pub(super) fn initialized_params(&self) -> Result<Value, AgentProcessError> {
        let token = self
            .inner
            .ready_token_challenge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(AgentProcessError::HandshakeFailed)?;
        Ok(serde_json::json!({"readyToken": token}))
    }

    /// 等待 writer 发布 initialized flush 事实，避免测试或 handshake 依赖时间猜测。
    pub fn wait_initialized_confirmation(&self, timeout: Duration) -> bool {
        if self.inner.initialized_confirmed.load(Ordering::Acquire) {
            return true;
        }
        let deadline = Instant::now()
            .checked_add(timeout.min(MAX_OPERATION_TIMEOUT))
            .unwrap_or_else(Instant::now);
        let mut lock = self
            .inner
            .initialized_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !self.inner.initialized_confirmed.load(Ordering::Acquire) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, wait) = self
                .inner
                .initialized_wake
                .wait_timeout(lock, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            lock = next;
            if wait.timed_out() {
                return self.inner.initialized_confirmed.load(Ordering::Acquire);
            }
        }
        true
    }

    /// 安装当前 generation 的 challenge；ready 接受只比较当前 session 的精确 token。
    pub fn install_ready_token_challenge(&self, token: String) -> Result<(), AgentProcessError> {
        if !valid_ready_token(&token) {
            return Err(AgentProcessError::HandshakeFailed);
        }
        let mut forbidden = self
            .inner
            .forbidden_ready_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        forbidden.clear();
        // 不保留有限历史 raw token：ready 接受必须精确等于当前 session challenge，
        // 而 ready frame 的任意 token-shaped 扩展值均被拒绝，因此历史淘汰不会扩大接受面。
        forbidden.insert(token.clone());
        *self
            .inner
            .ready_token_challenge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token);
        self.inner.initialized_sent.store(false, Ordering::Release);
        self.inner
            .initialized_confirmed
            .store(false, Ordering::Release);
        self.inner
            .ready_notification_claimed
            .store(false, Ordering::Release);
        self.inner
            .ready_token_consumed
            .store(false, Ordering::Release);
        Ok(())
    }

    /// 只有 initialized 已 flush 且 ready 原样回显当前 token 时才可晋级。
    pub fn ready_after_initialized_barrier(&self, frame: &RpcFrame) -> bool {
        let _gate = self
            .inner
            .ready_terminal_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ready_after_initialized_barrier_locked(frame)
    }

    /// 在线性化 gate 内完成 ready 校验和 lifecycle 提交，防止 terminal fault
    /// 在两步之间把 supervisor 留在 Ready。
    pub fn with_ready_promotion<F>(
        &self,
        frame: &RpcFrame,
        promote: F,
    ) -> Result<(), AgentProcessError>
    where
        F: FnOnce() -> Result<(), AgentProcessError>,
    {
        let _gate = self
            .inner
            .ready_terminal_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        if !self.ready_after_initialized_barrier_locked(frame) {
            return Err(AgentProcessError::HandshakeFailed);
        }
        promote()
    }

    /// 在已持有 ready/terminal gate 时执行不再可被 fault 打断的 token 晋级。
    fn ready_after_initialized_barrier_locked(&self, frame: &RpcFrame) -> bool {
        let Some(params) = frame.params() else {
            return false;
        };
        if self.inner.closed.load(Ordering::Acquire)
            || !self.inner.initialized_confirmed.load(Ordering::Acquire)
        {
            return false;
        }
        if !self
            .inner
            .ready_notification_claimed
            .load(Ordering::Acquire)
        {
            return false;
        }
        let Some(expected) = self
            .inner
            .ready_token_challenge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return false;
        };
        if params.get("readyToken").and_then(Value::as_str) != Some(expected.as_str())
            || !valid_ready_token(&expected)
            || !ready_params_are_safe(params, &expected, &self.inner.forbidden_ready_tokens)
        {
            return false;
        }
        self.inner
            .ready_token_consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 回应 Java 的 s: server request；请求 ID 只允许消费一次。
    pub fn respond_result(&self, id: &str, result: Value) -> Result<(), AgentProcessError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        if contains_forbidden_ready_token(&result, &self.inner.forbidden_ready_tokens) {
            return Err(AgentProcessError::HandshakeFailed);
        }
        self.respond(RpcFrame::response_result(id.to_owned(), result)?)
    }

    /// 返回脱敏稳定错误；secret/command 不应由这里拼入 message。
    pub fn respond_error(
        &self,
        id: &str,
        code: i64,
        message: &str,
        ja_code: &str,
        retryable: bool,
    ) -> Result<(), AgentProcessError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        let entry = error_catalog(code, ja_code, retryable)
            .filter(|entry| entry.message == message)
            .ok_or(AgentProcessError::InvalidErrorCatalog)?;
        self.respond(RpcFrame::response_error(
            id.to_owned(),
            entry.code,
            entry.message,
            entry.ja_code,
            entry.retryable,
        )?)
    }

    /// 将 server response 放进控制队列，审批结果不能排在普通 delta 后面。
    fn respond(&self, frame: RpcFrame) -> Result<(), AgentProcessError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        let id = frame.id().to_owned();
        let encoded = frame.encode(self.inner.limits.max_frame_bytes)?;
        {
            let mut requests = self
                .inner
                .inbound_server_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            requests.resolve(&id)?;
        }
        let result = self.inner.writer.send(encoded, EventPriority::Control);
        if let Err(error) = &result {
            handle_writer_error(&self.inner, error);
        }
        result
    }

    /// 领取连接唯一的事件消费权，防止 Session clone 产生多个竞争 reducer。
    pub fn take_event_pump(&self) -> Result<EventPump, AgentProcessError> {
        if self
            .inner
            .event_pump_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentProcessError::InvalidState);
        }
        Ok(EventPump {
            inner: Arc::clone(&self.inner),
        })
    }

    /// 把 monitor 的退出事实送入同一个控制队列，保证 supervisor 可统一 poll。
    pub fn report_process_exit(&self, code: Option<i32>) {
        push_event(
            &self.inner,
            SessionEvent::ProcessExited {
                generation: self.inner.generation,
                code,
            },
            EventPriority::Control,
            QueueKind::Control,
        );
        wire::fail_closed_with_reason(&self.inner, TerminalReason::ProcessExited);
    }

    /// 报告 monitor 无法取得稳定退出码，并释放 pending waiters，避免 wait 失败留下悬挂请求。
    pub fn report_process_fault(&self) {
        push_event(
            &self.inner,
            SessionEvent::ProtocolFault(codec::CodecError::Io),
            EventPriority::Control,
            QueueKind::Control,
        );
        fail_closed(&self.inner);
    }

    /// 在绝对 deadline 内 join writer actor；超时保留 handle，禁止静默 detach。
    pub fn join_writer_until(&self, deadline: Instant) -> Result<(), AgentProcessError> {
        join_writer_state_until(&self.writer_join, deadline)
    }

    /// 关闭 session、释放 pending，并在 writer actor 结束后才完成生命周期收口。
    pub fn close(&self) {
        let _ = self.close_until(default_writer_join_deadline(&self.inner));
    }

    /// 让 supervisor 把 writer join 纳入同一个 shutdown deadline。
    pub(super) fn close_until(&self, deadline: Instant) -> Result<(), AgentProcessError> {
        wire::fail_closed_with_reason(&self.inner, TerminalReason::Closed);
        let result = self.join_writer_until(deadline);
        if result.is_err() {
            push_event(
                &self.inner,
                SessionEvent::ProtocolFault(codec::CodecError::Io),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        result
    }

    /// 生命周期 owner 丢弃 session 前清除 callback，避免 stale client 触发旧 generation 路由。
    pub(super) fn detach_terminal_callback(&self) {
        let _ = self
            .inner
            .terminal_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// 返回 session 所属 generation，供 supervisor 过滤旧进程事件。
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }
}

/// 递归检查普通 payload 的字段和值，避免当前 challenge 在扩展字段中重放。
fn contains_forbidden_ready_token(value: &Value, forbidden: &Mutex<HashSet<String>>) -> bool {
    let forbidden = forbidden
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    contains_forbidden_ready_token_with_set(value, &forbidden)
}

/// 在已持有当前 token 集合时递归执行 marker 检查，不返回原始 token 诊断。
fn contains_forbidden_ready_token_with_set(value: &Value, forbidden: &HashSet<String>) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, child)| {
            key == "readyToken"
                || forbidden.contains(key)
                || contains_forbidden_ready_token_with_set(child, forbidden)
        }),
        Value::Array(values) => values
            .iter()
            .any(|child| contains_forbidden_ready_token_with_set(child, forbidden)),
        Value::String(text) => forbidden.contains(text),
        _ => false,
    }
}

/// 验证 ready 的单个 token 例外，同时拒绝其余位置的嵌套 marker/value。
fn ready_params_are_safe(
    params: &Value,
    expected: &str,
    forbidden: &Mutex<HashSet<String>>,
) -> bool {
    let Some(object) = params.as_object() else {
        return false;
    };
    let forbidden = forbidden
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    object.iter().all(|(key, child)| {
        if key == "readyToken" {
            child.as_str() == Some(expected) && valid_ready_token(expected)
        } else {
            !forbidden.contains(key)
                && !valid_ready_token(key)
                && !codec::contains_token_shaped_marker(child)
                && !contains_forbidden_ready_token_with_set(child, &forbidden)
        }
    })
}

impl Clone for Session {
    /// 只复制 Arc handle，不复制 pending 或创建第二套 reader/writer。
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            writer_join: Arc::clone(&self.writer_join),
        }
    }
}

impl Drop for Session {
    /// 最后一个 client 丢弃时仍执行终止与 bounded join；无法 join 时 abort，
    /// 这样不会把运行中的 writer JoinHandle 静默 drop 成 detached thread。
    fn drop(&mut self) {
        if Arc::strong_count(&self.writer_join) != 1 {
            return;
        }
        wire::fail_closed_with_reason(&self.inner, TerminalReason::Closed);
        if join_writer_state_until(&self.writer_join, default_writer_join_deadline(&self.inner))
            .is_err()
        {
            std::process::abort();
        }
    }
}

/// 计算默认 close join deadline，限制恶意 write timeout 不能扩张到无界等待。
fn default_writer_join_deadline(inner: &SessionInner) -> Instant {
    let timeout = inner
        .write_timeout
        .min(MAX_OPERATION_TIMEOUT)
        .saturating_add(MAX_WRITER_JOIN_GRACE);
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

/// 只在 completion gate 已满足时调用 JoinHandle::join，避免 deadline 后阻塞。
fn join_writer_state_until(
    state: &Arc<WriterJoinState>,
    deadline: Instant,
) -> Result<(), AgentProcessError> {
    let mut handle = state
        .handle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(writer) = handle.take() else {
        return Ok(());
    };
    if writer.thread().id() == thread::current().id() {
        *state
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
        return Err(AgentProcessError::InvalidState);
    }
    if !state.completion.wait_until(deadline) {
        *state
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
        return Err(AgentProcessError::ShutdownTimeout);
    }
    writer.join().map_err(|_| AgentProcessError::ProtocolFault)
}

/// 把 writer 背压分成可重试 data 溢出与不可恢复 control 故障，避免误杀 session。
fn handle_writer_error(inner: &Arc<SessionInner>, error: &AgentProcessError) {
    match error {
        AgentProcessError::QueueFull(QueueKind::Data)
            if !inner
                .writer_data_overflow_reported
                .swap(true, Ordering::AcqRel) =>
        {
            push_event(
                inner,
                SessionEvent::QueueOverflow(QueueKind::Data),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        AgentProcessError::QueueFull(QueueKind::Data) => {}
        AgentProcessError::QueueFull(QueueKind::Control) => {
            push_event(
                inner,
                SessionEvent::QueueFatalOverflow(QueueKind::Control),
                EventPriority::Control,
                QueueKind::Control,
            );
            fail_closed(inner);
        }
        AgentProcessError::QueueClosed(_) => fail_closed(inner),
        AgentProcessError::SessionClosed => {}
        _ => {}
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
