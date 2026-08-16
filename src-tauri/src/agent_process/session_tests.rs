// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Unit tests for session-local ledgers and queue overflow invariants.

use super::*;

/// 证明连接内 ID 不能因有限 tombstone 淘汰而重新获得副作用语义。
#[test]
fn inbound_request_ids_are_never_reused_after_resolution() {
    let mut requests = InboundServerRequests::new(1);
    requests.register("s:first").unwrap();
    requests.resolve("s:first").unwrap();
    assert_eq!(
        requests.register("s:first"),
        Err(AgentProcessError::DuplicateRequest)
    );
    for index in 0..(MAX_INBOUND_SERVER_REQUEST_LEDGER - 1) {
        let id = format!("s:ledger-{index}");
        requests.register(&id).unwrap();
        requests.resolve(&id).unwrap();
    }
    assert_eq!(
        requests.register("s:after-ledger"),
        Err(AgentProcessError::RequestLedgerExhausted)
    );
}

/// 证明 outbound c: ID 达到连接硬上限后显式轮换，而不是回绕复用。
#[test]
fn outbound_request_ids_are_never_reused_or_evicted() {
    let mut ledger = OutboundRequestLedger::new();
    let first = ledger.allocate().unwrap();
    assert_ne!(first, ledger.allocate().unwrap());
    for _ in 2..MAX_OUTBOUND_REQUEST_LEDGER {
        ledger.allocate().unwrap();
    }
    assert_eq!(
        ledger.allocate(),
        Err(AgentProcessError::RequestLedgerExhausted)
    );
}

/// 证明 data overflow 只发布一次可重试事件，不会把 session 错误关闭。
#[test]
fn data_overflow_is_nonfatal_and_reported_once() {
    let queue = EventQueue::new(1, Limits::default().max_frame_bytes);
    queue.push(
        SessionEvent::Notification(
            RpcFrame::notification("data/one", serde_json::json!({})).unwrap(),
        ),
        EventPriority::Data,
        QueueKind::Data,
    );
    queue.push(
        SessionEvent::Notification(
            RpcFrame::notification("data/two", serde_json::json!({})).unwrap(),
        ),
        EventPriority::Data,
        QueueKind::Data,
    );
    assert_eq!(
        queue.pop(Duration::ZERO),
        Some(SessionEvent::QueueOverflow(QueueKind::Data))
    );
    assert!(matches!(
        queue.pop(Duration::ZERO),
        Some(SessionEvent::Notification(_))
    ));
    assert_eq!(queue.pop(Duration::ZERO), None);
}

/// 证明 control overflow 仍是 fatal，防止 shutdown/approval 事实被静默丢弃。
#[test]
fn control_overflow_is_fatal() {
    let queue = EventQueue::new(1, Limits::default().max_frame_bytes);
    for _ in 0..=CONTROL_QUEUE_CAPACITY {
        queue.push(
            SessionEvent::Eof,
            EventPriority::Control,
            QueueKind::Control,
        );
    }
    assert_eq!(
        queue.pop(Duration::ZERO),
        Some(SessionEvent::QueueFatalOverflow(QueueKind::Control))
    );
}
