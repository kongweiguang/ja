// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Strict allowlist redaction for host denial diagnostics.

/// Extract one JSON string field without retaining or printing unrelated
/// unified-log data; malformed records simply fall back to a category.
pub(super) fn json_string_field(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let start = text.find(&marker)? + marker.len();
    let colon = text[start..].find(':')? + start + 1;
    let value = text[colon..].trim_start();
    let mut chars = value.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut result = String::new();
    let mut escaped = false;
    for character in chars {
        if escaped {
            escaped = false;
            result.push(character);
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return Some(result);
        } else {
            result.push(character);
        }
    }
    None
}

/// Accept only the reviewed Seatbelt operation vocabulary; arbitrary
/// values, even harmless-looking tokens, are never emitted to CI.
pub(super) fn safe_operation(value: Option<String>, lower: &str) -> String {
    if let Some(value) = value {
        return allowed_operation(&value).unwrap_or("redacted").into();
    }
    known_operation(lower).unwrap_or("redacted").into()
}

/// Accept only fixed process labels and redact image paths or unknown
/// process names before they can cross the diagnostic boundary.
pub(super) fn safe_process(value: Option<String>, lower: &str) -> String {
    if let Some(value) = value {
        return allowed_process(&value).unwrap_or("redacted").into();
    }
    known_process(lower).unwrap_or("redacted").into()
}

/// Match an operation field exactly against the small reviewed allowlist.
pub(super) fn allowed_operation(value: &str) -> Option<&'static str> {
    [
        "process-info",
        "process-exec",
        "process-fork",
        "file-read-data",
        "file-read-metadata",
        "file-write-data",
        "file-write-create",
        "network-outbound",
        "network-inbound",
        "mach-lookup",
        "sysctl-read",
        "signal",
    ]
    .iter()
    .copied()
    .find(|operation| value.eq_ignore_ascii_case(operation))
}

/// Recover a known Seatbelt operation from an event message without
/// exposing the original message or any path it may contain.
pub(super) fn known_operation(lower: &str) -> Option<&'static str> {
    [
        "process-info",
        "process-exec",
        "process-fork",
        "file-read-data",
        "file-read-metadata",
        "file-write-data",
        "file-write-create",
        "network-outbound",
        "network-inbound",
        "mach-lookup",
        "sysctl-read",
        "signal",
    ]
    .iter()
    .find(|operation| lower.contains(**operation))
    .copied()
}

/// Match a process field exactly against names emitted by this probe or
/// the host Seatbelt wrapper; arbitrary names remain unavailable.
pub(super) fn allowed_process(value: &str) -> Option<&'static str> {
    [
        "ja-sandbox-worker",
        "ja-sandbox-probe",
        "sandbox-exec",
        "log",
    ]
    .iter()
    .copied()
    .find(|process| value.eq_ignore_ascii_case(process))
}

/// Recover only fixed probe process names from a text-style denial event;
/// arbitrary image paths remain redacted by `safe_process`.
pub(super) fn known_process(lower: &str) -> Option<&'static str> {
    [
        "ja-sandbox-worker",
        "ja-sandbox-probe",
        "sandbox-exec",
        "log",
    ]
    .iter()
    .find(|process| lower.contains(**process))
    .copied()
}
