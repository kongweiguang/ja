// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Token-free event and status projection for the trusted WebView.

use super::config::{EventSink, RuntimeCommandError, RuntimeStatusKind};
use crate::agent_process::codec::{self, Limits, RpcFrame};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stable event consumed by the next frontend runtime provider.
pub const RPC_FRAME_EVENT: &str = "ja://rpc/frame";
const STATUS_EVENT_PREFIX: &str = "evt_host_";

/// Emits one response/event frame after recursive ready-token redaction.
pub(super) fn emit_frame(sink: &EventSink, frame: &RpcFrame) -> Result<(), RuntimeCommandError> {
    let value = frame_to_value(frame).map_err(|_| RuntimeCommandError::unavailable())?;
    sink(sanitize_webview_value(value)?).map_err(|_| RuntimeCommandError::event_delivery())
}

/// Converts a validated foundation frame back to JSON without duplicating its
/// parser or writer; the trailing newline is removed only at the IPC edge.
pub(super) fn frame_to_value(frame: &RpcFrame) -> Result<Value, codec::CodecError> {
    let encoded = frame.encode(Limits::default().max_frame_bytes)?;
    serde_json::from_slice(&encoded[..encoded.len().saturating_sub(1)])
        .map_err(|_| codec::CodecError::InvalidJson)
}

/// Removes the legal transport challenge path and blocks every other
/// token-shaped occurrence before a payload reaches the WebView.
fn sanitize_webview_value(mut value: Value) -> Result<Value, RuntimeCommandError> {
    scrub_tokens(&mut value, &mut Vec::new())?;
    Ok(value)
}

/// Recursively rejects token-shaped fields while allowing only the host's
/// already-consumed ready projection to omit its challenge field.
fn scrub_tokens(value: &mut Value, path: &mut Vec<String>) -> Result<(), RuntimeCommandError> {
    match value {
        Value::Object(object) => {
            let keys: Vec<String> = object.keys().cloned().collect();
            for key in keys {
                if key == "readyToken" {
                    if *path == ["params".to_owned()]
                        && object.get("status").and_then(Value::as_str) == Some("ready")
                    {
                        object.remove(&key);
                        continue;
                    }
                    return Err(RuntimeCommandError {
                        code: "SENSITIVE_EVENT_BLOCKED",
                        message: "runtime event contains protected data",
                        retryable: false,
                    });
                }
                if key.to_ascii_lowercase().contains("token") {
                    return Err(RuntimeCommandError {
                        code: "SENSITIVE_EVENT_BLOCKED",
                        message: "runtime event contains protected data",
                        retryable: false,
                    });
                }
                path.push(key.clone());
                if let Some(child) = object.get_mut(&key) {
                    scrub_tokens(child, path)?;
                }
                path.pop();
            }
        }
        Value::Array(values) => {
            for child in values {
                scrub_tokens(child, path)?;
            }
        }
        Value::String(text)
            if text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            return Err(RuntimeCommandError {
                code: "SENSITIVE_EVENT_BLOCKED",
                message: "runtime event contains protected data",
                retryable: false,
            });
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

/// Emits a token-free runtime/statusChanged frame; `ready` intentionally has
/// no challenge because the challenge is consumed inside the Rust supervisor.
pub(super) fn emit_status(
    sink: &EventSink,
    status: RuntimeStatusKind,
    generation: u64,
    server_instance_id: Option<&str>,
    reason: &str,
) -> Result<(), RuntimeCommandError> {
    let server_instance_id = server_instance_id.unwrap_or("srv_host");
    sink(json!({
        "jsonrpc": "2.0",
        "method": "runtime/statusChanged",
        "params": {
            "serverInstanceId": server_instance_id,
            "eventId": format!("{STATUS_EVENT_PREFIX}{generation}_{reason}"),
            "occurredAt": now_timestamp(),
            "status": status.protocol_name(),
            "health": {"generation": generation, "reason": reason}
        }
    }))
    .map_err(|_| RuntimeCommandError::event_delivery())
}

/// Uses a dependency-free UTC formatter so lifecycle events remain bounded and
/// deterministic without adding a second time/date abstraction.
pub(super) fn now_timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let days = (elapsed.as_secs() / 86_400) as i64;
    let seconds = elapsed.as_secs() % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3_600,
        (seconds / 60) % 60,
        seconds % 60,
        elapsed.subsec_millis()
    )
}

/// Converts Unix days to UTC calendar fields without adding a date crate.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ready projection must never carry a challenge into the UI.
    #[test]
    fn ready_projection_is_token_free() {
        let value = json!({"method": "runtime/statusChanged", "params": {"status": "ready", "readyToken": "0123456789abcdef0123456789abcdef"}});
        let sanitized = sanitize_webview_value(value).expect("ready projection is safe");
        assert!(sanitized["params"].get("readyToken").is_none());
    }

    /// A token-shaped value outside the transport path must fail closed.
    #[test]
    fn arbitrary_token_value_is_blocked() {
        let value = json!({"method": "runtime/notice", "params": {"message": "0123456789abcdef0123456789abcdef"}});
        assert_eq!(
            sanitize_webview_value(value).unwrap_err().code,
            "SENSITIVE_EVENT_BLOCKED"
        );
    }

    /// Native emitter failures must reach the bridge as a stable observable
    /// command error rather than disappearing behind a discarded Result.
    #[test]
    fn event_sink_failure_is_reported() {
        let sink: EventSink =
            std::sync::Arc::new(|_| Err(super::super::config::EventEmitError::DeliveryFailed));
        let error = emit_status(&sink, RuntimeStatusKind::Ready, 1, Some("srv_1"), "ready")
            .expect_err("event failure must be observable");
        assert_eq!(error.code, "RUNTIME_EVENT_DELIVERY_FAILED");
    }

    /// Java's durable approval notification is forwarded as-is; Rust does not synthesize a second
    /// sequence or expose a private server-request identity to the WebView.
    #[test]
    fn approval_notification_is_forwarded_without_private_request_id() {
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let target = std::sync::Arc::clone(&received);
        let sink: EventSink = std::sync::Arc::new(move |value| {
            target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(value);
            Ok(())
        });
        let frame = RpcFrame::notification(
            "approval/requested",
            json!({
                "threadId": "thr_1",
                "seq": 2,
                "eventId": "evt_approval_2",
                "occurredAt": "2026-08-18T00:00:00Z",
                "approval": {"approvalId": "appr_1"}
            }),
        )
        .expect("approval notification");
        emit_frame(&sink, &frame).expect("notification sink");
        let value = &received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[0];
        assert!(value.get("id").is_none());
        assert_eq!(value["method"], "approval/requested");
        assert_eq!(value["params"]["approval"]["approvalId"], "appr_1");
    }
}
