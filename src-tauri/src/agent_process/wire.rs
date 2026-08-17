// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! stdio wire pumps and terminal propagation for a sidecar session.
//!
//! Each direction has one owner: reader parses stdout, writer owns stdin, and
//! stderr is drained independently so protocol progress cannot wait on UI work.

use super::{STDERR_REDACTED_SUMMARY, SessionEvent, SessionInner, TerminalReason};
use crate::agent_process::codec::{self, FrameKind, RpcFrame};
use crate::agent_process::error::{AgentProcessError, QueueKind};
use crate::agent_process::pending::ResolveDisposition;
use std::io::{BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// 数据队列的预算必须独立于控制保留区，否则 delta 可以把审批/关闭饿死。
pub(super) const MAX_WRITER_DATA_QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// 控制帧保留独立预算，至少容纳一个协商允许的最大 frame（含 JSONL 换行）。
pub(super) fn control_queue_byte_budget(max_frame_bytes: usize) -> usize {
    max_frame_bytes.saturating_add(1)
}

/// Bound one complete stdin frame so a wedged child cannot hold the writer
/// actor beyond the lifecycle deadline; the terminal callback performs cancel.
pub(super) const DEFAULT_WRITE_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventPriority {
    Control,
    Data,
}

pub(super) struct WriterHandle {
    pub(super) control: SyncSender<Vec<u8>>,
    pub(super) data: SyncSender<Vec<u8>>,
    pub(super) closed: Arc<std::sync::atomic::AtomicBool>,
    control_queued_bytes: Arc<AtomicUsize>,
    data_queued_bytes: Arc<AtomicUsize>,
    control_max_bytes: usize,
    data_max_bytes: usize,
    wake: Arc<WriterWake>,
}

struct WriterWake {
    sequence: Mutex<u64>,
    condition: Condvar,
}

impl WriterWake {
    /// 创建无轮询的 writer 唤醒状态，发送者只递增 sequence 并通知 actor。
    fn new() -> Self {
        Self {
            sequence: Mutex::new(0),
            condition: Condvar::new(),
        }
    }

    /// 记录新 frame 或 close 事实，避免 writer 在空队列上固定 sleep。
    fn notify(&self) {
        let mut sequence = self
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *sequence = sequence.wrapping_add(1);
        self.condition.notify_one();
    }

    /// 读取当前 sequence，供 actor 在检查双队列后检测竞态唤醒。
    fn snapshot(&self) -> u64 {
        *self
            .sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl WriterHandle {
    /// 创建带独立唤醒序列的 writer handle，保证 session 不需要了解队列实现细节。
    pub(super) fn new(
        control: SyncSender<Vec<u8>>,
        data: SyncSender<Vec<u8>>,
        closed: Arc<std::sync::atomic::AtomicBool>,
        control_queued_bytes: Arc<AtomicUsize>,
        data_queued_bytes: Arc<AtomicUsize>,
        control_max_bytes: usize,
        data_max_bytes: usize,
    ) -> Self {
        Self {
            control,
            data,
            closed,
            control_queued_bytes,
            data_queued_bytes,
            control_max_bytes,
            data_max_bytes,
            wake: Arc::new(WriterWake::new()),
        }
    }

    /// 先编码再入队，确保 queue 中的每个项目都是完整单 frame。
    pub(super) fn send(
        &self,
        frame: Vec<u8>,
        priority: EventPriority,
    ) -> Result<(), AgentProcessError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AgentProcessError::SessionClosed);
        }
        let frame_bytes = frame.len();
        let (queued_bytes, max_bytes) = match priority {
            EventPriority::Control => (&self.control_queued_bytes, self.control_max_bytes),
            EventPriority::Data => (&self.data_queued_bytes, self.data_max_bytes),
        };
        if !reserve_bytes(queued_bytes, frame_bytes, max_bytes) {
            return Err(match priority {
                EventPriority::Control => AgentProcessError::QueueFull(QueueKind::Control),
                EventPriority::Data => AgentProcessError::QueueFull(QueueKind::Data),
            });
        }
        if self.closed.load(Ordering::Acquire) {
            queued_bytes.fetch_sub(frame_bytes, Ordering::AcqRel);
            return Err(AgentProcessError::SessionClosed);
        }
        let result = match priority {
            EventPriority::Control => self.control.try_send(frame).map_err(|error| match error {
                mpsc::TrySendError::Full(_) => AgentProcessError::QueueFull(QueueKind::Control),
                mpsc::TrySendError::Disconnected(_) => {
                    AgentProcessError::QueueClosed(QueueKind::Control)
                }
            }),
            EventPriority::Data => self.data.try_send(frame).map_err(|error| match error {
                mpsc::TrySendError::Full(_) => AgentProcessError::QueueFull(QueueKind::Data),
                mpsc::TrySendError::Disconnected(_) => {
                    AgentProcessError::QueueClosed(QueueKind::Data)
                }
            }),
        };
        if result.is_err() {
            queued_bytes.fetch_sub(frame_bytes, Ordering::AcqRel);
        } else {
            self.wake.notify();
        }
        result
    }

    /// writer actor 消费成功入队的 frame 后释放对应字节预算。
    fn release(&self, priority: EventPriority, bytes: usize) {
        let counter = match priority {
            EventPriority::Control => &self.control_queued_bytes,
            EventPriority::Data => &self.data_queued_bytes,
        };
        counter.fetch_sub(bytes, Ordering::AcqRel);
    }

    /// 关闭 writer actor 的输入并唤醒等待者，保证 shutdown 不依赖轮询间隔。
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify();
    }
}

/// 以 CAS 保证多个 request 线程不能同时突破 writer 总字节预算。
fn reserve_bytes(counter: &AtomicUsize, bytes: usize, max: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if bytes > max.saturating_sub(current) {
            return false;
        }
        match counter.compare_exchange_weak(
            current,
            current.saturating_add(bytes),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
}

/// writer actor is stdin single owner; control frames keep shutdown reachable.
pub(super) fn writer_loop<W: Write + Send + 'static>(
    mut writer: W,
    control: Receiver<Vec<u8>>,
    data: Receiver<Vec<u8>>,
    inner: Arc<SessionInner>,
) {
    let closed = Arc::clone(&inner.closed);
    let wake = Arc::clone(&inner.writer.wake);
    let mut observed_sequence = wake.snapshot();
    let mut control_burst = 0_usize;
    while !closed.load(Ordering::Acquire) {
        // Control is preferred, but a finite burst prevents a continuous
        // telemetry stream from starving already queued data frames.
        let frame = next_frame(&control, &data, &mut control_burst);
        let Some((frame, priority)) = frame else {
            let mut sequence = wake
                .sequence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if closed.load(Ordering::Acquire) {
                break;
            }
            if *sequence == observed_sequence {
                sequence = wake
                    .condition
                    .wait(sequence)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            observed_sequence = *sequence;
            continue;
        };
        inner.writer.release(priority, frame.len());
        let is_initialized = codec::decode_frame(&frame, inner.limits.max_frame_bytes)
            .ok()
            .and_then(|frame| frame.method().map(str::to_owned))
            .as_deref()
            == Some("initialized");
        let written =
            write_frame_with_watchdog(writer, frame, inner.write_timeout, &inner, is_initialized);
        let next_writer = match written {
            Ok(next_writer) => next_writer,
            Err(error) => {
                let event = if matches!(error, AgentProcessError::DeadlineExceeded) {
                    SessionEvent::WriterTimedOut
                } else {
                    SessionEvent::ProtocolFault(codec::CodecError::Io)
                };
                inner
                    .events
                    .push(event, EventPriority::Control, QueueKind::Control);
                fail_closed(&inner);
                break;
            }
        };
        writer = next_writer;
        observed_sequence = wake.snapshot();
    }
    if !closed.load(Ordering::Acquire) {
        fail_closed(&inner);
    }
}

/// Write on the single writer actor while a joined watchdog invokes terminal
/// cancellation; production ChildStdin is unblocked by closing the owned child
/// process tree, so no second operation thread or detached JoinHandle is needed.
fn write_frame_with_watchdog<W: Write>(
    mut writer: W,
    frame: Vec<u8>,
    timeout: Duration,
    inner: &Arc<SessionInner>,
    is_initialized: bool,
) -> Result<W, AgentProcessError> {
    // Hold the same gate used by reader ready-claiming across both I/O calls;
    // this makes the handshake linearization independent of pipe timing.
    let initialized_guard = if is_initialized {
        Some(inner.initialized_barrier.begin_send()?)
    } else {
        None
    };
    let (cancel_sender, cancel_receiver) = mpsc::sync_channel(1);
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_timed_out = Arc::clone(&timed_out);
    let watchdog_inner = Arc::clone(inner);
    let watchdog = match thread::Builder::new()
        .name("ja-sidecar-write-watchdog".to_owned())
        .spawn(move || {
            match cancel_receiver.recv_timeout(timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => false,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    watchdog_timed_out.store(true, Ordering::Release);
                    // JSONL cannot be interrupted safely in a half-written
                    // frame; the terminal callback closes ChildStdin by
                    // killing the owned process tree and closing the session.
                    fail_closed(&watchdog_inner);
                    true
                }
            }
        }) {
        Ok(watchdog) => watchdog,
        Err(_) => {
            if let Some(guard) = initialized_guard {
                guard.complete(false);
            }
            return Err(AgentProcessError::Spawn);
        }
    };
    let write_result = writer.write_all(&frame);
    let result = write_result.and_then(|_| writer.flush());
    let _ = cancel_sender.send(());
    let watchdog_fired = match watchdog.join() {
        Ok(fired) => fired || timed_out.load(Ordering::Acquire),
        Err(_) => {
            if let Some(guard) = initialized_guard {
                guard.complete(false);
            }
            return Err(AgentProcessError::Spawn);
        }
    };
    // Do not publish Confirmed until the watchdog has joined; a late write
    // completion after timeout must remain fail-closed for any waiting ready.
    if let Some(guard) = initialized_guard {
        guard.complete(result.is_ok() && !watchdog_fired && !inner.closed.load(Ordering::Acquire));
    }
    if watchdog_fired || timed_out.load(Ordering::Acquire) {
        return Err(AgentProcessError::DeadlineExceeded);
    }
    match result {
        Ok(()) => Ok(writer),
        Err(_) => Err(AgentProcessError::SessionClosed),
    }
}

const MAX_CONTROL_BURST: usize = 8;

/// Select the next frame with bounded control preference and no polling delay.
fn next_frame(
    control: &Receiver<Vec<u8>>,
    data: &Receiver<Vec<u8>>,
    control_burst: &mut usize,
) -> Option<(Vec<u8>, EventPriority)> {
    if *control_burst >= MAX_CONTROL_BURST
        && let Ok(frame) = data.try_recv()
    {
        *control_burst = 0;
        return Some((frame, EventPriority::Data));
    }
    if let Ok(frame) = control.try_recv() {
        *control_burst = (*control_burst).saturating_add(1);
        return Some((frame, EventPriority::Control));
    }
    if let Ok(frame) = data.try_recv() {
        *control_burst = 0;
        return Some((frame, EventPriority::Data));
    }
    None
}

/// stdout reader 永久运行并只把完整合法 frame 交给 session dispatcher。
pub(super) fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    inner: Arc<SessionInner>,
) -> Result<(), AgentProcessError> {
    thread::Builder::new()
        .name("ja-sidecar-reader".to_owned())
        .spawn(move || reader_loop(reader, inner))
        .map(|_| ())
        .map_err(|_| AgentProcessError::Spawn)
}

/// 读取线程只负责 framing/dispatch，避免业务等待阻塞 stdout 消费。
fn reader_loop<R: Read>(reader: R, inner: Arc<SessionInner>) {
    let mut reader = BufReader::new(reader);
    while !inner.closed.load(Ordering::Acquire) {
        match codec::read_frame(&mut reader, inner.limits.max_frame_bytes) {
            Ok(frame) => dispatch_frame(frame, &inner),
            Err(codec::CodecError::UnexpectedEof) => {
                push_event(
                    &inner,
                    SessionEvent::Eof,
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(&inner);
                break;
            }
            Err(codec::CodecError::HandshakeFailed) => {
                push_event(
                    &inner,
                    SessionEvent::HandshakeFailed,
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(&inner);
                break;
            }
            Err(error) => {
                push_event(
                    &inner,
                    SessionEvent::ProtocolFault(error),
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(&inner);
                break;
            }
        }
    }
}

/// response 走 pending，server request/notification 走独立 bounded event queue。
fn dispatch_frame(frame: RpcFrame, inner: &Arc<SessionInner>) {
    match frame.validate() {
        Ok(FrameKind::Response) => {
            if !frame_payload_is_safe(&frame, inner, false) {
                push_event(
                    inner,
                    SessionEvent::ProtocolFault(codec::CodecError::InvalidEnvelope),
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
                return;
            }
            let disposition = inner
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .resolve(frame);
            if disposition != ResolveDisposition::Delivered {
                push_event(
                    inner,
                    SessionEvent::ResponseRejected(disposition),
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
            }
        }
        Ok(FrameKind::ServerRequest) => {
            if !frame_payload_is_safe(&frame, inner, false) {
                push_event(
                    inner,
                    SessionEvent::ProtocolFault(codec::CodecError::InvalidEnvelope),
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
                return;
            }
            let id = frame.id().to_owned();
            let registered = inner
                .inbound_server_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .register(&id);
            if registered.is_err() {
                push_event(
                    inner,
                    SessionEvent::ProtocolFault(codec::CodecError::InvalidEnvelope),
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
                return;
            }
            push_event(
                inner,
                SessionEvent::ServerRequest(frame),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        Ok(FrameKind::Notification) => {
            let is_ready = frame.method() == Some("runtime/statusChanged")
                && frame
                    .params()
                    .and_then(|params| params.get("status"))
                    .and_then(serde_json::Value::as_str)
                    == Some("ready");
            let is_initialized = frame.method() == Some("initialized");
            if !frame_payload_is_safe(&frame, inner, is_ready) {
                push_event(
                    inner,
                    if is_ready || is_initialized {
                        SessionEvent::HandshakeFailed
                    } else {
                        SessionEvent::ProtocolFault(codec::CodecError::InvalidEnvelope)
                    },
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
                return;
            }
            if is_ready && !inner.claim_ready_notification() {
                push_event(
                    inner,
                    SessionEvent::HandshakeFailed,
                    EventPriority::Control,
                    QueueKind::Control,
                );
                fail_closed(inner);
                return;
            }
            let priority = if frame
                .method()
                .is_some_and(|method| method.starts_with("runtime/"))
            {
                EventPriority::Control
            } else {
                EventPriority::Data
            };
            push_event(inner, frame.into_notification(), priority, QueueKind::Data);
        }
        Ok(FrameKind::ClientRequest) | Err(_) => {
            push_event(
                inner,
                SessionEvent::ProtocolFault(codec::CodecError::InvalidEnvelope),
                EventPriority::Control,
                QueueKind::Control,
            );
            fail_closed(inner);
        }
    }
}

/// 递归审计 inbound frame；ready 例外只放行当前 token 的顶层 params 字段。
fn frame_payload_is_safe(frame: &RpcFrame, inner: &Arc<SessionInner>, ready: bool) -> bool {
    let forbidden = inner
        .forbidden_ready_tokens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if frame.id_opt().is_some_and(|id| forbidden.contains(id))
        || frame
            .method()
            .is_some_and(|method| forbidden.contains(method))
        || frame
            .error()
            .is_some_and(|error| forbidden.contains(error.message()))
        || frame
            .result()
            .value()
            .is_some_and(|value| contains_forbidden_token(value, &forbidden))
        || frame
            .error()
            .is_some_and(|error| contains_forbidden_token(error.data(), &forbidden))
    {
        return false;
    }
    let Some(params) = frame.params() else {
        return !ready;
    };
    if !ready {
        return !contains_forbidden_token(params, &forbidden);
    }
    let Some(expected) = inner
        .ready_token_challenge
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return false;
    };
    let Some(object) = params.as_object() else {
        return false;
    };
    object.iter().all(|(key, value)| {
        if key == "readyToken" {
            value.as_str() == Some(expected.as_str()) && codec::valid_ready_token(&expected)
        } else {
            !forbidden.contains(key)
                && !codec::is_ready_token_key(key)
                && !codec::is_token_shaped(key)
                && !codec::contains_token_shaped_marker(value)
                && !contains_forbidden_token(value, &forbidden)
        }
    })
}

/// 递归检查 key/value，防止 token 通过 details、数组或扩展字段泄露。
fn contains_forbidden_token(
    value: &serde_json::Value,
    forbidden: &std::collections::HashSet<String>,
) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            codec::is_ready_token_key(key)
                || codec::is_token_shaped(key)
                || forbidden.contains(key)
                || contains_forbidden_token(child, forbidden)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|child| contains_forbidden_token(child, forbidden)),
        serde_json::Value::String(text) => forbidden.contains(text),
        _ => false,
    }
}

/// 将合法 notification 包装为事件，保持事件 API 不暴露无方向的裸 frame。
trait IntoNotification {
    /// 将已通过 codec 校验的 notification 包装成定向 session 事件。
    fn into_notification(self) -> SessionEvent;
}

impl IntoNotification for RpcFrame {
    /// 保持 notification 的方向信息，避免后续 caller 把它当 response 消费。
    fn into_notification(self) -> SessionEvent {
        SessionEvent::Notification(self)
    }
}

/// stderr 永久 drain；超限只截断诊断，不阻塞 stdout 协议 reader。
pub(super) fn spawn_stderr_reader<E: Read + Send + 'static>(
    stderr: E,
    inner: Arc<SessionInner>,
) -> Result<(), AgentProcessError> {
    thread::Builder::new()
        .name("ja-sidecar-stderr".to_owned())
        .spawn(move || stderr_loop(stderr, inner))
        .map(|_| ())
        .map_err(|_| AgentProcessError::Spawn)
}

/// 持续读取 stderr，即使诊断超预算也继续 drain 以免阻塞 stdout 协议。
fn stderr_loop<E: Read>(mut stderr: E, inner: Arc<SessionInner>) {
    let max = inner.limits.max_stderr_line_bytes;
    let max_total = inner.limits.max_log_bytes as u64;
    let mut line = Vec::with_capacity(max.min(4096));
    let mut byte = [0_u8; 1];
    loop {
        match stderr.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => {
                let offset = inner.stderr_bytes.fetch_add(1, Ordering::AcqRel);
                if offset < max_total && !inner.stderr_truncated.load(Ordering::Acquire) {
                    emit_stderr_line(&mut line, &inner);
                } else {
                    emit_stderr_truncated(&inner);
                    line.clear();
                }
            }
            Ok(_) => {
                let offset = inner.stderr_bytes.fetch_add(1, Ordering::AcqRel);
                if offset >= max_total {
                    emit_stderr_truncated(&inner);
                    line.clear();
                } else if line.len() < max {
                    line.push(byte[0]);
                } else {
                    drain_stderr_line(&mut stderr, &mut byte, &inner);
                    emit_stderr_truncated(&inner);
                    line.clear();
                }
            }
            Err(_) => break,
        }
    }
    if !line.is_empty()
        && !inner.stderr_truncated.load(Ordering::Acquire)
        && inner.stderr_bytes.load(Ordering::Acquire) <= max_total
    {
        emit_stderr_line(&mut line, &inner);
    }
}

/// 丢弃超长诊断行的剩余字节，避免 stderr 反压阻塞 stdout 协议 reader。
fn drain_stderr_line<E: Read>(stderr: &mut E, byte: &mut [u8; 1], inner: &Arc<SessionInner>) {
    while let Ok(read) = stderr.read(byte) {
        if read == 0 {
            break;
        }
        let offset = inner.stderr_bytes.fetch_add(1, Ordering::AcqRel);
        if offset >= inner.limits.max_log_bytes as u64 {
            emit_stderr_truncated(inner);
        }
        if byte[0] == b'\n' {
            break;
        }
    }
}

/// 只发送一次稳定截断事件，随后继续 drain 但不把诊断内容留在内存。
fn emit_stderr_truncated(inner: &Arc<SessionInner>) {
    if !inner.stderr_truncated.swap(true, Ordering::AcqRel) {
        push_event(
            inner,
            SessionEvent::StderrTruncated,
            EventPriority::Data,
            QueueKind::Stderr,
        );
    }
}

/// 将预算内的一行转换为脱敏事件并立即清空暂存 buffer。
fn emit_stderr_line(line: &mut Vec<u8>, inner: &Arc<SessionInner>) {
    // Raw stderr may contain API keys, user paths, prompts, or source code; only
    // publish a fixed diagnostic so event queues and Debug output stay safe.
    line.clear();
    push_event(
        inner,
        SessionEvent::StderrLine(STDERR_REDACTED_SUMMARY.to_owned()),
        EventPriority::Data,
        QueueKind::Stderr,
    );
}

/// 一旦发生不可恢复 queue/protocol 故障，统一停止 writer 并释放 pending waiters。
/// 只执行一次 session 终止，确保所有 reader/writer fault 都释放 pending。
pub(super) fn fail_closed(inner: &Arc<SessionInner>) {
    fail_closed_with_reason(inner, TerminalReason::Fault);
}

/// 只执行一次终止通知，并在回调前关闭事实置位，避免回调重入重复旋转 generation。
pub(super) fn fail_closed_with_reason(inner: &Arc<SessionInner>, reason: TerminalReason) {
    let _ready_terminal_gate = inner
        .ready_terminal_gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !inner.closed.swap(true, Ordering::AcqRel) {
        // The reader/writer threads may be the only observers of EOF or an IO
        // fault, so invoke the process owner here instead of waiting for UI poll.
        inner.writer.close();
        inner.events.close();
        if let Some(callback) = inner
            .terminal_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            callback(reason);
        }
        let mut pending = inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.close();
    }
}

/// 把事件写入 bounded queue 后立刻检查 overflow，避免 fatal 被 data queue 延迟遮蔽。
pub(super) fn push_event(
    inner: &Arc<SessionInner>,
    event: SessionEvent,
    priority: EventPriority,
    kind: QueueKind,
) {
    if inner.closed.load(Ordering::Acquire) && !is_terminal_event(&event) {
        return;
    }
    inner.events.push(event, priority, kind);
    if inner.events.is_fatal() {
        fail_closed(inner);
    }
}

/// 终止后只允许 fault/exit 事实继续入队，防止 stderr 或 delta 在 close 后复留。
pub(super) fn is_terminal_event(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::ProtocolFault(_)
            | SessionEvent::WriterTimedOut
            | SessionEvent::HandshakeFailed
            | SessionEvent::Eof
            | SessionEvent::QueueFatalOverflow(_)
            | SessionEvent::ProcessExited { .. }
            | SessionEvent::ResponseRejected(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// 证明数据预算耗尽时控制保留区仍可承载审批/关闭帧，避免单一总计数造成死锁。
    #[test]
    fn data_budget_cannot_consume_control_reserve() {
        let (control_sender, _control_receiver) = mpsc::sync_channel(4);
        let (data_sender, _data_receiver) = mpsc::sync_channel(4);
        let handle = WriterHandle::new(
            control_sender,
            data_sender,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            4,
            4,
        );

        assert!(handle.send(vec![1, 2, 3, 4], EventPriority::Data).is_ok());
        assert_eq!(
            handle.send(vec![5], EventPriority::Data),
            Err(AgentProcessError::QueueFull(QueueKind::Data))
        );
        assert!(
            handle
                .send(vec![6, 7, 8, 9], EventPriority::Control)
                .is_ok()
        );
    }

    /// The derived reserve must accept one frame at the negotiated maximum,
    /// including its JSONL newline byte.
    #[test]
    fn control_budget_accepts_negotiated_frame_boundary() {
        let max_frame = 1_024;
        let (control_sender, _control_receiver) = mpsc::sync_channel(2);
        let (data_sender, _data_receiver) = mpsc::sync_channel(2);
        let handle = WriterHandle::new(
            control_sender,
            data_sender,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            control_queue_byte_budget(max_frame),
            MAX_WRITER_DATA_QUEUE_BYTES,
        );
        assert!(
            handle
                .send(
                    vec![b'x'; control_queue_byte_budget(max_frame)],
                    EventPriority::Control
                )
                .is_ok()
        );
    }

    /// Continuous control production is bounded so an already queued data
    /// frame is written after the finite burst rather than starved forever.
    #[test]
    fn control_burst_allows_data_progress() {
        let (control_sender, control_receiver) = mpsc::sync_channel(32);
        let (data_sender, data_receiver) = mpsc::sync_channel(4);
        for index in 0..16 {
            control_sender
                .send(vec![index as u8])
                .expect("control fixture queued");
        }
        data_sender.send(vec![99]).expect("data fixture queued");
        let mut burst = 0;
        for _ in 0..MAX_CONTROL_BURST {
            assert_eq!(
                next_frame(&control_receiver, &data_receiver, &mut burst)
                    .expect("control frame available")
                    .1,
                EventPriority::Control
            );
        }
        assert_eq!(
            next_frame(&control_receiver, &data_receiver, &mut burst)
                .expect("data must make progress")
                .1,
            EventPriority::Data
        );
    }
}
