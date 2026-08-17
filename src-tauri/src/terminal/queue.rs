// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端输入与事件的有界队列。
//!
//! `sync_channel` 只限制消息数，不能限制大块 bytes；这里同时记录数量和字节，
//! 让高频输出无法把 host 内存推到不可预测状态，并保持单 writer 的顺序。

use super::error::{TerminalError, TerminalErrorCode};
use super::model::{TerminalEvent, TerminalEventKind, TerminalId};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// 终端输入队列；caller 只负责追加，writer worker 独占消费。
pub(crate) struct InputQueue {
    state: Mutex<InputState>,
    wake: Condvar,
    max_bytes: usize,
}

struct InputState {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

impl InputQueue {
    /// 用 byte budget 初始化队列，确保单次 input 不会无界复制。
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(InputState {
                queue: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
            max_bytes,
        }
    }

    /// 非阻塞追加 input；UI 卡顿时立即返回 queue full，不能把 PTY writer 反压到 IPC 线程。
    pub(crate) fn push(&self, data: Vec<u8>) -> Result<(), TerminalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(TerminalError::new(TerminalErrorCode::QueueClosed));
        }
        if data.len() > self.max_bytes.saturating_sub(state.bytes) {
            return Err(TerminalError::new(TerminalErrorCode::QueueFull));
        }
        state.bytes = state.bytes.saturating_add(data.len());
        state.queue.push_back(data);
        self.wake.notify_one();
        Ok(())
    }

    /// writer worker 阻塞取下一块，close 后保证最终返回 None。
    pub(crate) fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(data) = state.queue.pop_front() {
                state.bytes = state.bytes.saturating_sub(data.len());
                return Some(data);
            }
            if state.closed {
                return None;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// 关闭 input 队列并唤醒 writer，使 close 不依赖额外 sentinel bytes。
    pub(crate) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        self.wake.notify_all();
    }
}

/// 有界事件队列；终态事件只淘汰必要的最旧输出以保证 close/错误始终可达。
pub(crate) struct EventQueue {
    state: Mutex<EventState>,
    wake: Condvar,
    session_id: TerminalId,
    generation: u64,
    max_bytes: usize,
    max_count: usize,
    terminal: AtomicBool,
}

struct EventState {
    queue: VecDeque<TerminalEvent>,
    bytes: usize,
    next_sequence: u64,
    closed: bool,
}

impl EventQueue {
    /// 创建与 session generation 绑定的事件通道，防止跨 session 复用事件身份。
    pub(crate) fn new(
        session_id: TerminalId,
        generation: u64,
        max_bytes: usize,
        max_count: usize,
    ) -> Self {
        Self {
            state: Mutex::new(EventState {
                queue: VecDeque::new(),
                bytes: 0,
                next_sequence: 1,
                closed: false,
            }),
            wake: Condvar::new(),
            session_id,
            generation,
            max_bytes,
            max_count,
            terminal: AtomicBool::new(false),
        }
    }

    /// 仅接收当前 generation 的普通 output；overflow 由 caller 转换成终态错误。
    pub(crate) fn push_output(&self, data: Vec<u8>) -> bool {
        self.push_kind(TerminalEventKind::Output { data })
    }

    /// 添加 resize 或其它非终态控制事件；控制事件很小，满时仍拒绝而不覆盖输出。
    pub(crate) fn push_control(&self, kind: TerminalEventKind) -> bool {
        self.push_kind(kind)
    }

    /// 清空普通 output 后写入 terminal event，保证 close/错误不会被慢 UI 永久遮蔽。
    pub(crate) fn push_terminal(&self, kind: TerminalEventKind) -> bool {
        if self.terminal.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }
        let event = self.make_event(&mut state, kind);
        let size = event_size(&event);
        if size > self.max_bytes || self.max_count == 0 {
            state.closed = true;
            return false;
        }
        // Preserve as much recent output as possible; only evict the oldest
        // ordinary events needed to reserve space for the terminal fact.
        while (state.bytes.saturating_add(size) > self.max_bytes
            || state.queue.len() >= self.max_count)
            && !state.queue.is_empty()
        {
            if let Some(evicted) = state.queue.pop_front() {
                state.bytes = state.bytes.saturating_sub(event_size(&evicted));
            }
        }
        if state.bytes.saturating_add(size) > self.max_bytes || state.queue.len() >= self.max_count
        {
            state.closed = true;
            return false;
        }
        state.bytes = state.bytes.saturating_add(size);
        state.queue.push_back(event);
        self.wake.notify_all();
        true
    }

    /// 按绝对 deadline 读取下一条事件，避免多次相对 timeout 累积超时。
    pub(crate) fn recv_until(&self, deadline: Instant) -> Option<TerminalEvent> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(event) = state.queue.pop_front() {
                state.bytes = state.bytes.saturating_sub(event_size(&event));
                return Some(event);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, result) = self
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if result.timed_out() {
                return None;
            }
        }
    }

    /// 关闭队列但保留已经排队的 terminal event，供 UI 最后消费。
    pub(crate) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.wake.notify_all();
    }

    /// Push one bounded event while the terminal bit prevents late producers
    /// from re-opening a generation after its terminal event.
    fn push_kind(&self, kind: TerminalEventKind) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed
            || self.terminal.load(Ordering::Acquire)
            || state.queue.len() >= self.max_count
        {
            return false;
        }
        let event = self.make_event(&mut state, kind);
        let size = event_size(&event);
        if state.bytes.saturating_add(size) > self.max_bytes {
            return false;
        }
        state.bytes = state.bytes.saturating_add(size);
        state.queue.push_back(event);
        self.wake.notify_one();
        true
    }

    /// Stamp the immutable owner identity and monotonic sequence at the queue boundary.
    fn make_event(&self, state: &mut EventState, kind: TerminalEventKind) -> TerminalEvent {
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        TerminalEvent {
            session_id: self.session_id,
            generation: self.generation,
            sequence,
            kind,
        }
    }
}

/// 估算 JSON/channel event 的驻留大小；output data 是唯一可大幅变化的字段。
fn event_size(event: &TerminalEvent) -> usize {
    match &event.kind {
        TerminalEventKind::Output { data } => data.len().saturating_add(64),
        TerminalEventKind::OutputDropped { .. } => 64,
        TerminalEventKind::Exited { signal, .. } => {
            96usize.saturating_add(signal.as_ref().map_or(0, String::len))
        }
        TerminalEventKind::Resized { .. } => 64,
        TerminalEventKind::Closed { .. } | TerminalEventKind::Error { .. } => 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 队列满时必须立即失败而不是等待未定义时间，保护 IPC 调用线程。
    #[test]
    fn input_queue_is_bounded_by_bytes() {
        let queue = InputQueue::new(3);
        assert!(queue.push(vec![1, 2, 3]).is_ok());
        assert_eq!(
            queue.push(vec![4]).unwrap_err().code(),
            TerminalErrorCode::QueueFull
        );
        assert_eq!(queue.pop(), Some(vec![1, 2, 3]));
    }

    /// 终态必须能从满 output 队列中胜出，避免 UI 慢时无法显示关闭原因。
    #[test]
    fn terminal_event_replaces_queued_output() {
        let id = TerminalId::new();
        let queue = EventQueue::new(id, 1, 80, 8);
        assert!(queue.push_output(vec![1; 16]));
        assert!(queue.push_terminal(TerminalEventKind::Closed {
            reason: super::super::model::CloseReason::User,
        }));
        let event = queue
            .recv_until(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(matches!(event.kind, TerminalEventKind::Closed { .. }));
    }

    /// 终态之后到达的 reader bytes 必须被丢弃，且事件身份固定在原 generation。
    #[test]
    fn late_output_is_rejected_after_terminal() {
        let id = TerminalId::new();
        let queue = EventQueue::new(id, 7, 256, 8);
        assert!(queue.push_terminal(TerminalEventKind::Closed {
            reason: super::super::model::CloseReason::Timeout,
        }));
        assert!(!queue.push_output(vec![0xff, 0x1b, b'[', b'2', b'J']));
        let event = queue
            .recv_until(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(event.generation, 7);
    }
}
