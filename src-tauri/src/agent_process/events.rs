// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded event routing for one sidecar session.
//!
//! Server requests, control facts and data deltas have separate queues so a
//! slow UI cannot hide approval or termination state behind ordinary output.

use super::wire::{EventPriority, control_queue_byte_budget, is_terminal_event};
use super::{MAX_OPERATION_TIMEOUT, SessionEvent};
use crate::agent_process::codec::RpcFrame;
use crate::agent_process::error::QueueKind;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const CONTROL_QUEUE_CAPACITY: usize = 64;
const MAX_EVENT_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EVENT_CONTROL_BURST: usize = 8;

pub(super) struct EventQueue {
    state: Mutex<EventQueueState>,
    wake: Condvar,
    data_capacity: usize,
    control_capacity: usize,
    max_data_bytes: usize,
    max_control_bytes: usize,
    max_frame_bytes: usize,
}

struct EventQueueState {
    server_requests: VecDeque<SessionEvent>,
    data: VecDeque<SessionEvent>,
    control: VecDeque<SessionEvent>,
    server_request_bytes: usize,
    data_bytes: usize,
    control_bytes: usize,
    fatal: Option<QueueKind>,
    fatal_reported: bool,
    data_overflow_reported: bool,
    control_burst: usize,
    closed: bool,
}

/// 估算事件驻留字节，和数量上限一起限制慢消费者造成的内存增长。
fn event_size(event: &SessionEvent, max_frame_bytes: usize) -> usize {
    match event {
        SessionEvent::ServerRequest(frame) | SessionEvent::Notification(frame) => frame
            .encode(max_frame_bytes)
            .map(|encoded| encoded.len())
            .unwrap_or(max_frame_bytes.saturating_add(1)),
        SessionEvent::StderrLine(line) => line.len(),
        SessionEvent::ProtocolFault(_) => 128,
        SessionEvent::WriterTimedOut => 64,
        SessionEvent::HandshakeFailed => 64,
        SessionEvent::ResponseRejected(_) => 64,
        SessionEvent::StderrTruncated
        | SessionEvent::Eof
        | SessionEvent::QueueOverflow(_)
        | SessionEvent::QueueFatalOverflow(_)
        | SessionEvent::ProcessExited { .. } => 64,
    }
}

impl EventQueue {
    /// 将控制事实与普通 delta 分队，保证退出/协议故障不会被慢 UI 挤掉。
    pub(super) fn new(data_capacity: usize, max_frame_bytes: usize) -> Self {
        Self {
            state: Mutex::new(EventQueueState {
                server_requests: VecDeque::with_capacity(CONTROL_QUEUE_CAPACITY),
                data: VecDeque::with_capacity(data_capacity),
                control: VecDeque::with_capacity(CONTROL_QUEUE_CAPACITY),
                server_request_bytes: 0,
                data_bytes: 0,
                control_bytes: 0,
                fatal: None,
                fatal_reported: false,
                data_overflow_reported: false,
                control_burst: 0,
                closed: false,
            }),
            wake: Condvar::new(),
            data_capacity,
            control_capacity: CONTROL_QUEUE_CAPACITY,
            max_data_bytes: MAX_EVENT_QUEUE_BYTES,
            // Keep the event control reserve aligned with writer framing so a
            // negotiated maximum control frame cannot be rejected here.
            max_control_bytes: control_queue_byte_budget(max_frame_bytes),
            max_frame_bytes,
        }
    }

    /// 非阻塞入队；data 满只发布一次可观测 overflow，control 满才终止 session。
    pub(super) fn push(&self, event: SessionEvent, priority: EventPriority, kind: QueueKind) {
        let bytes = event_size(&event, self.max_frame_bytes);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let is_server_request = matches!(&event, SessionEvent::ServerRequest(_));
        let capacity = match priority {
            EventPriority::Control => self.control_capacity,
            EventPriority::Data => self.data_capacity,
        };
        let queue_len = match priority {
            EventPriority::Control => state.control.len(),
            EventPriority::Data => state.data.len(),
        };
        let queue_len = if is_server_request {
            state.server_requests.len()
        } else {
            queue_len
        };
        let queue_bytes = if matches!(priority, EventPriority::Control) {
            state
                .server_request_bytes
                .saturating_add(state.control_bytes)
        } else {
            state.data_bytes
        };
        let byte_limit = if matches!(priority, EventPriority::Control) {
            self.max_control_bytes
        } else {
            self.max_data_bytes
        };
        if queue_len >= capacity || queue_bytes.saturating_add(bytes) > byte_limit {
            if matches!(priority, EventPriority::Data) {
                if !state.data_overflow_reported {
                    state.data_overflow_reported = true;
                    let notice = SessionEvent::QueueOverflow(kind);
                    let notice_bytes = event_size(&notice, self.max_frame_bytes);
                    let control_full = state.control.len() >= self.control_capacity
                        || state
                            .server_request_bytes
                            .saturating_add(state.control_bytes)
                            .saturating_add(notice_bytes)
                            > self.max_control_bytes;
                    if control_full {
                        state.fatal.get_or_insert(QueueKind::Control);
                    } else {
                        state.control.push_back(notice);
                        state.control_bytes = state.control_bytes.saturating_add(notice_bytes);
                    }
                }
            } else if state.fatal.is_none() {
                state.fatal = Some(kind);
            }
        } else if is_server_request {
            state.server_requests.push_back(event);
            state.server_request_bytes = state.server_request_bytes.saturating_add(bytes);
        } else {
            match priority {
                EventPriority::Control => {
                    state.control.push_back(event);
                    state.control_bytes = state.control_bytes.saturating_add(bytes);
                }
                EventPriority::Data => {
                    state.data.push_back(event);
                    state.data_bytes = state.data_bytes.saturating_add(bytes);
                }
            }
        }
        self.wake.notify_all();
    }

    /// 先消费控制队列，再消费数据队列，确保 shutdown/EOF 的可达性。
    pub(super) fn pop(&self, timeout: Duration) -> Option<SessionEvent> {
        let deadline = Instant::now().checked_add(timeout.min(MAX_OPERATION_TIMEOUT));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(kind) = state.fatal.take()
                && !state.fatal_reported
            {
                state.fatal_reported = true;
                return Some(SessionEvent::QueueFatalOverflow(kind));
            }
            if state.control_burst >= MAX_EVENT_CONTROL_BURST
                && let Some(event) = state.data.pop_front()
            {
                state.data_bytes = state
                    .data_bytes
                    .saturating_sub(event_size(&event, self.max_frame_bytes));
                state.control_burst = 0;
                return Some(event);
            }
            if let Some(event) = state.server_requests.pop_front() {
                state.server_request_bytes = state
                    .server_request_bytes
                    .saturating_sub(event_size(&event, self.max_frame_bytes));
                state.control_burst = state.control_burst.saturating_add(1);
                return Some(event);
            }
            if let Some(event) = state.control.pop_front() {
                state.control_bytes = state
                    .control_bytes
                    .saturating_sub(event_size(&event, self.max_frame_bytes));
                state.control_burst = state.control_burst.saturating_add(1);
                return Some(event);
            }
            if let Some(event) = state.data.pop_front() {
                state.data_bytes = state
                    .data_bytes
                    .saturating_sub(event_size(&event, self.max_frame_bytes));
                state.control_burst = 0;
                return Some(event);
            }
            if state.fatal_reported || state.closed {
                return None;
            }
            let deadline = deadline?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_state, wait) = self
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next_state;
            if wait.timed_out() {
                return None;
            }
        }
    }

    /// 只取嵌套 server request，避免等待 response 的内部 handler 吞掉外部事件。
    pub(super) fn pop_server_request(&self, timeout: Duration) -> Option<RpcFrame> {
        let deadline = Instant::now().checked_add(timeout.min(MAX_OPERATION_TIMEOUT));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(SessionEvent::ServerRequest(frame)) = state.server_requests.pop_front() {
                state.server_request_bytes = state.server_request_bytes.saturating_sub(event_size(
                    &SessionEvent::ServerRequest(frame.clone()),
                    self.max_frame_bytes,
                ));
                state.control_burst = state.control_burst.saturating_add(1);
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            let remaining = deadline?.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next_state, wait) = self
                .wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next_state;
            if wait.timed_out() {
                return None;
            }
        }
    }

    /// 返回不可恢复 overflow 标志，供 session 立即关闭 writer 和 pending。
    pub(super) fn is_fatal(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fatal
            .is_some()
    }

    /// 终止 session 时只保留终态控制事实，避免敏感 command/delta 长期驻留。
    pub(super) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.server_requests.clear();
        state.server_request_bytes = 0;
        state.data.clear();
        state.data_bytes = 0;
        state.control.retain(is_terminal_event);
        state.control_bytes = state
            .control
            .iter()
            .map(|event| event_size(event, self.max_frame_bytes))
            .sum();
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 证明 server request 专用读取不会消费普通 notification 或 stderr 事件。
    #[test]
    fn nested_request_pop_isolated_from_external_events() {
        let queue = EventQueue::new(4, 4 * 1024 * 1024);
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("runtime/status", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Control,
            QueueKind::Control,
        );
        queue.push(
            SessionEvent::ServerRequest(
                RpcFrame::server_request(
                    "s:approval-1".to_owned(),
                    "approval/request".to_owned(),
                    serde_json::json!({}),
                )
                .expect("server request fixture is valid"),
            ),
            EventPriority::Control,
            QueueKind::Control,
        );
        assert!(queue.pop_server_request(Duration::ZERO).is_some());
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(SessionEvent::Notification(_))
        ));
    }

    /// data 接近 64 MiB 时仍必须保留独立 control/overflow 预算，不能伪造 fatal。
    #[test]
    fn data_near_cap_keeps_control_notice_reachable() {
        let queue = EventQueue::new(128, 4 * 1024 * 1024);
        for _ in 0..64 {
            queue.push(
                SessionEvent::StderrLine("x".repeat(1024 * 1024)),
                EventPriority::Data,
                QueueKind::Stderr,
            );
        }
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("runtime/status", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Control,
            QueueKind::Control,
        );
        queue.push(
            SessionEvent::StderrLine("overflow".to_owned()),
            EventPriority::Data,
            QueueKind::Stderr,
        );
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(SessionEvent::Notification(_))
        ));
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(SessionEvent::QueueOverflow(QueueKind::Stderr))
        ));
        assert!(!queue.is_fatal());
    }

    /// The event reserve uses the same negotiated frame-derived budget as the
    /// writer, so one legal control frame cannot be rejected by routing.
    #[test]
    fn control_budget_tracks_frame_limit() {
        let max_frame = 1_024;
        let queue = EventQueue::new(4, max_frame);
        assert_eq!(
            queue.max_control_bytes,
            control_queue_byte_budget(max_frame)
        );
        queue.push(
            SessionEvent::Eof,
            EventPriority::Control,
            QueueKind::Control,
        );
        assert!(!queue.is_fatal());
    }

    /// Continuous runtime control events must yield one queued data event
    /// after a finite burst without allowing an approval request to hide.
    #[test]
    fn control_burst_allows_data_progress() {
        let queue = EventQueue::new(32, 4 * 1024 * 1024);
        for _ in 0..16 {
            queue.push(
                SessionEvent::Notification(
                    RpcFrame::notification("runtime/status", serde_json::json!({})).unwrap(),
                ),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("item/delta", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Data,
            QueueKind::Data,
        );
        for _ in 0..MAX_EVENT_CONTROL_BURST {
            assert!(matches!(
                queue.pop(Duration::ZERO),
                Some(SessionEvent::Notification(_))
            ));
        }
        let Some(SessionEvent::Notification(frame)) = queue.pop(Duration::ZERO) else {
            panic!("data event must make progress");
        };
        assert_eq!(frame.method(), Some("item/delta"));
    }

    /// A sustained approval stream still yields data after a bounded burst;
    /// this prevents nested requests from becoming an unbounded starvation
    /// source while keeping every request ahead of the first data burst.
    #[test]
    fn server_request_burst_allows_data_progress() {
        let queue = EventQueue::new(32, 4 * 1024 * 1024);
        for index in 0..16 {
            queue.push(
                SessionEvent::ServerRequest(
                    RpcFrame::server_request(
                        format!("s:approval-{index}"),
                        "approval/request".to_owned(),
                        serde_json::json!({}),
                    )
                    .expect("server request fixture is valid"),
                ),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("item/delta", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Data,
            QueueKind::Data,
        );
        for _ in 0..MAX_EVENT_CONTROL_BURST {
            assert!(matches!(
                queue.pop(Duration::ZERO),
                Some(SessionEvent::ServerRequest(_))
            ));
        }
        let Some(SessionEvent::Notification(frame)) = queue.pop(Duration::ZERO) else {
            panic!("data event must make progress after request burst");
        };
        assert_eq!(frame.method(), Some("item/delta"));
    }

    /// A saturated data lane reports one overflow but still delivers the one
    /// terminal fact through control, so the UI can close a turn reliably.
    #[test]
    fn turn_completed_survives_data_overflow_exactly_once() {
        let queue = EventQueue::new(1, 4 * 1024 * 1024);
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("item/seed", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Data,
            QueueKind::Data,
        );
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("item/delta", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Data,
            QueueKind::Data,
        );
        let terminal = RpcFrame::notification(
            "turn/completed",
            serde_json::json!({
                "turn": {
                    "turnId": "turn_one",
                    "threadId": "thr_one",
                    "status": "completed",
                    "accessMode": "workspace"
                },
                "terminalStatus": "completed"
            }),
        )
        .unwrap();
        queue.push(
            SessionEvent::Notification(terminal),
            EventPriority::Control,
            QueueKind::Control,
        );

        let mut overflow_count = 0;
        let mut terminal_count = 0;
        let mut dropped_delta_count = 0;
        while let Some(event) = queue.pop(Duration::ZERO) {
            match event {
                SessionEvent::QueueOverflow(QueueKind::Data) => overflow_count += 1,
                SessionEvent::Notification(frame) if frame.method() == Some("turn/completed") => {
                    terminal_count += 1
                }
                SessionEvent::Notification(frame) if frame.method() == Some("item/delta") => {
                    dropped_delta_count += 1
                }
                _ => {}
            }
        }
        assert_eq!(overflow_count, 1);
        assert_eq!(terminal_count, 1);
        assert_eq!(dropped_delta_count, 0);
        assert!(!queue.is_fatal());
    }

    /// Closing clears ordinary data and overflow notices but retains a queued
    /// turn terminal, allowing a concurrent shutdown to preserve final state.
    #[test]
    fn close_retains_turn_completed_once() {
        let queue = EventQueue::new(1, 4 * 1024 * 1024);
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification("item/delta", serde_json::json!({})).unwrap(),
            ),
            EventPriority::Data,
            QueueKind::Data,
        );
        queue.push(
            SessionEvent::Notification(
                RpcFrame::notification(
                    "turn/completed",
                    serde_json::json!({
                        "turn": {
                            "turnId": "turn_one",
                            "threadId": "thr_one",
                            "status": "failed",
                            "accessMode": "workspace"
                        },
                        "terminalStatus": "failed"
                    }),
                )
                .unwrap(),
            ),
            EventPriority::Control,
            QueueKind::Control,
        );
        queue.close();
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(SessionEvent::Notification(frame)) if frame.method() == Some("turn/completed")
        ));
        assert!(queue.pop(Duration::ZERO).is_none());
    }

    /// A genuinely full control lane remains fatal; the terminal reserve does
    /// not create a hidden second queue or allow unbounded control growth.
    #[test]
    fn control_saturation_remains_fatal() {
        let queue = EventQueue::new(1, 4 * 1024);
        for _ in 0..CONTROL_QUEUE_CAPACITY + 1 {
            queue.push(
                SessionEvent::Notification(
                    RpcFrame::notification("runtime/status", serde_json::json!({})).unwrap(),
                ),
                EventPriority::Control,
                QueueKind::Control,
            );
        }
        assert!(queue.is_fatal());
        assert!(matches!(
            queue.pop(Duration::ZERO),
            Some(SessionEvent::QueueFatalOverflow(QueueKind::Control))
        ));
    }
}
