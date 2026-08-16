// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! initialize handshake and configuration validation.
//!
//! These checks are kept separate from process supervision so protocol schema
//! validation cannot become entangled with child lifecycle cleanup.

use crate::agent_process::codec::{self, Limits, RpcFrame};
use crate::agent_process::error::AgentProcessError;
use serde_json::Value;
use std::collections::HashSet;
#[cfg(unix)]
use std::io::Read;
use std::time::{Duration, Instant};

const READY_TOKEN_BYTES: usize = 16;
const READY_TOKEN_HEX_BYTES: usize = READY_TOKEN_BYTES * 2;

pub(super) const MAX_READY_TIMEOUT: Duration = Duration::from_secs(600);
pub(super) const MAX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(120);

/// 构造与 session 实际预算完全相等的 v1 initialize，避免握手后动态缩容失配。
pub(super) fn default_initialize_params(limits: &Limits) -> Value {
    serde_json::json!({
        "protocolMajor": 1,
        "protocolMinor": 0,
        "minimumCompatibleMinor": 0,
        "clientVersion": "ja-host",
        "capabilities": {
            "methods": [],
            "events": [],
            "permissionModes": ["plan", "workspace", "full_access"],
            "itemKinds": [],
            "mcp": {"protocolVersions": [], "transports": [], "features": []}
        },
        "limits": limits.to_value(),
        "workspacePolicy": {"mode": "plan", "network": "disabled", "enforcement": "unavailable", "protectedRoots": []}
    })
}

/// 只允许 sidecar 运行所需的稳定环境变量，避免隐式继承凭据或用户状态。
pub(super) fn allowed_env_name(name: &str) -> bool {
    matches!(
        name,
        "JA_LOG_LEVEL"
            | "JA_DATA_DIR"
            | "RUST_LOG"
            | "LANG"
            | "LC_ALL"
            | "SystemRoot"
            | "SystemDrive"
            | "WINDIR"
            | "TEMP"
            | "TMP"
    )
}

/// 检查参数/环境名中的凭据标记，防止 secret 通过不可审计启动边界泄露。
pub(super) fn contains_secret_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "secret", "token", "password", "api_key", "apikey", "bearer", "cookie",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

/// 在启动前完整验证 host initialize，保证握手发送的声明就是 session 实际使用的预算。
pub(super) fn validate_initialize_params(
    value: &Value,
    local: &Limits,
    workspace_enforcement_verified: bool,
) -> Result<(), AgentProcessError> {
    let object = value.as_object().ok_or(AgentProcessError::InvalidConfig)?;
    if object.get("protocolMajor").and_then(Value::as_i64) != Some(1) {
        return Err(AgentProcessError::InvalidConfig);
    }
    let minor = object
        .get("protocolMinor")
        .and_then(Value::as_i64)
        .filter(|minor| (0..=i64::from(i32::MAX)).contains(minor))
        .ok_or(AgentProcessError::InvalidConfig)?;
    let _minimum = object
        .get("minimumCompatibleMinor")
        .and_then(Value::as_i64)
        .filter(|minimum| (0..=minor).contains(minimum))
        .ok_or(AgentProcessError::InvalidConfig)?;
    if !bounded_string(object.get("clientVersion"), 128) {
        return Err(AgentProcessError::InvalidConfig);
    }
    validate_capabilities(object.get("capabilities"))?;
    validate_remote_limits(object.get("limits"), local)
        .map_err(|_| AgentProcessError::InvalidConfig)?;
    validate_workspace_policy(
        object.get("workspacePolicy"),
        workspace_enforcement_verified,
    )
    .map_err(|_| AgentProcessError::InvalidConfig)?;
    Ok(())
}

/// 校验 workspace policy 的有限枚举与 root 字符串，避免握手声明绕过宿主边界。
pub(super) fn validate_workspace_policy(
    value: Option<&Value>,
    workspace_enforcement_verified: bool,
) -> Result<(), AgentProcessError> {
    let policy = value
        .and_then(Value::as_object)
        .ok_or(AgentProcessError::ProtocolFault)?;
    if !matches!(
        policy.get("mode").and_then(Value::as_str),
        Some("plan" | "workspace" | "full_access")
    ) || !matches!(
        policy.get("network").and_then(Value::as_str),
        Some("disabled" | "restricted" | "enabled")
    ) || !matches!(
        policy.get("enforcement").and_then(Value::as_str),
        Some("os_enforced" | "partial" | "unavailable")
    ) {
        return Err(AgentProcessError::ProtocolFault);
    }
    if policy.get("enforcement").and_then(Value::as_str) == Some("os_enforced")
        && !workspace_enforcement_verified
    {
        return Err(AgentProcessError::ProtocolFault);
    }
    if let Some(roots) = policy.get("protectedRoots") {
        let roots = roots.as_array().ok_or(AgentProcessError::ProtocolFault)?;
        if roots.len() > 32
            || roots
                .iter()
                .any(|root| root.as_str().is_none_or(|path| path.len() > 4096))
        {
            return Err(AgentProcessError::ProtocolFault);
        }
    }
    Ok(())
}

/// 检查 server 错误是否明确宣告协议不兼容，避免普通 fault 被误标为配置问题。
/// 只把明确的协议版本错误映射为 Incompatible，普通错误仍进入 Faulted。
pub(super) fn error_is_incompatible(code: i64, data: &Value) -> bool {
    code == -32_003
        && data.get("jaCode").and_then(Value::as_str) == Some("PROTOCOL_VERSION_UNSUPPORTED")
        && data.get("retryable").and_then(Value::as_bool) == Some(false)
}

/// 只接受 initialize 完成后、同一 server instance 发出的结构化 ready notification。
pub(super) fn is_ready_notification(frame: &RpcFrame, expected_instance: Option<&str>) -> bool {
    frame.method() == Some("runtime/statusChanged")
        && frame
            .params()
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("ready")
        && frame
            .params()
            .and_then(|params| params.get("serverInstanceId"))
            .and_then(Value::as_str)
            == expected_instance
        && frame
            .params()
            .and_then(|params| params.get("eventId"))
            .and_then(Value::as_str)
            .is_some_and(|id| valid_schema_id(id, "evt_", 100))
        && frame
            .params()
            .and_then(|params| params.get("occurredAt"))
            .and_then(Value::as_str)
            .is_some_and(valid_timestamp)
}

/// 判断 frame 是否是 runtime ready 控制事实；其余字段由严格握手校验继续验证，
/// 这样缺失 token 或错误 instance 不会被当成普通事件拖到 deadline 才暴露。
pub(super) fn is_runtime_ready_notification(frame: &RpcFrame) -> bool {
    frame.method() == Some("runtime/statusChanged")
        && frame
            .params()
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("ready")
}

/// 只接受 codec 定义的固定小写 hex challenge，避免大小写变体伪造握手。
pub(super) fn valid_ready_token(value: &str) -> bool {
    codec::valid_ready_token(value)
}

/// 从操作系统 CSPRNG 生成每个 generation 一次的新 challenge；失败必须让握手失败。
pub(super) fn generate_ready_token() -> Result<String, AgentProcessError> {
    let mut bytes = [0_u8; READY_TOKEN_BYTES];
    fill_csprng(&mut bytes)?;
    let mut token = String::with_capacity(READY_TOKEN_HEX_BYTES);
    for byte in bytes {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(token)
}

/// 选择平台原生随机源，拒绝时间、线程序号等可预测替代物。
fn fill_csprng(bytes: &mut [u8; READY_TOKEN_BYTES]) -> Result<(), AgentProcessError> {
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut core::ffi::c_void,
                buffer: *mut u8,
                length: u32,
                flags: u32,
            ) -> i32;
        }
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        // SAFETY: BCryptGenRandom writes exactly the fixed-size stack buffer and
        // does not retain either pointer; the null algorithm selects the system RNG.
        let status = unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(AgentProcessError::HandshakeFailed)
        }
    }
    #[cfg(unix)]
    {
        let mut source =
            std::fs::File::open("/dev/urandom").map_err(|_| AgentProcessError::HandshakeFailed)?;
        source
            .read_exact(bytes)
            .map_err(|_| AgentProcessError::HandshakeFailed)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = bytes;
        Err(AgentProcessError::HandshakeFailed)
    }
}

/// 校验 initialize result 的能力 object，防止伪造 ready 绕过未声明的 host 能力。
pub(super) fn validate_capabilities(value: Option<&Value>) -> Result<(), AgentProcessError> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Err(AgentProcessError::ProtocolFault);
    };
    validate_string_array(object.get("methods"), 256, 128, None)?;
    validate_string_array(object.get("events"), 256, 128, None)?;
    validate_string_array(
        object.get("permissionModes"),
        3,
        32,
        Some(&["plan", "workspace", "full_access"]),
    )?;
    validate_string_array(object.get("itemKinds"), 64, 64, None)?;
    let Some(mcp) = object.get("mcp").and_then(Value::as_object) else {
        return Err(AgentProcessError::ProtocolFault);
    };
    validate_string_array(mcp.get("protocolVersions"), 16, 32, None)?;
    validate_string_array(
        mcp.get("transports"),
        2,
        32,
        Some(&["stdio", "streamable_http"]),
    )?;
    validate_string_array(
        mcp.get("features"),
        2,
        32,
        Some(&["tools_list", "tools_call"]),
    )?;
    Ok(())
}

/// 校验 capability 数组的大小、字符串形状和 uniqueItems，防止握手伪造无界能力表。
fn validate_string_array(
    value: Option<&Value>,
    max_items: usize,
    max_len: usize,
    allowed: Option<&[&str]>,
) -> Result<(), AgentProcessError> {
    let Some(values) = value.and_then(Value::as_array) else {
        return Err(AgentProcessError::ProtocolFault);
    };
    if values.is_empty() {
        return Ok(());
    }
    if values.len() > max_items {
        return Err(AgentProcessError::ProtocolFault);
    }
    let mut unique = HashSet::with_capacity(values.len());
    for value in values {
        let Some(item) = value.as_str() else {
            return Err(AgentProcessError::ProtocolFault);
        };
        if item.is_empty()
            || item.len() > max_len
            || !unique.insert(item)
            || allowed.is_some_and(|choices| !choices.contains(&item))
        {
            return Err(AgentProcessError::ProtocolFault);
        }
    }
    Ok(())
}

/// 复用冻结 ID 的前缀/ASCII 规则，避免 starts_with 检查接受伪造 instance/event。
pub(super) fn valid_schema_id(value: &str, prefix: &str, max_len: usize) -> bool {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    if value.len() > max_len || suffix.is_empty() || suffix.len() > 96 {
        return false;
    }
    let bytes = suffix.as_bytes();
    bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || *byte == b'.' || *byte == b'_' || *byte == b'-'
        })
}

/// 校验 serverVersion 的受控 ASCII 形状，避免把任意诊断文本当版本字段。
pub(super) fn valid_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-' | b'+'))
}

/// 只接受完整 RFC3339 date-time，避免格式伪造绕过 ready 事件的协议校验。
pub(super) fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() > 64
        || bytes.len() < 20
        || bytes.iter().any(|byte| byte.is_ascii_whitespace())
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !ascii_digits(bytes, 0, 4)
        || !ascii_digits(bytes, 5, 2)
        || !ascii_digits(bytes, 8, 2)
        || !ascii_digits(bytes, 11, 2)
        || !ascii_digits(bytes, 14, 2)
        || !ascii_digits(bytes, 17, 2)
    {
        return false;
    }

    let year = decimal_component(bytes, 0, 4);
    let month = decimal_component(bytes, 5, 2);
    let day = decimal_component(bytes, 8, 2);
    let hour = decimal_component(bytes, 11, 2);
    let minute = decimal_component(bytes, 14, 2);
    let second = decimal_component(bytes, 17, 2);
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }

    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return false;
        }
    }

    match bytes.get(index) {
        Some(b'Z') => index + 1 == bytes.len(),
        Some(b'+') | Some(b'-') => {
            index + 6 == bytes.len()
                && bytes[index + 3] == b':'
                && ascii_digits(bytes, index + 1, 2)
                && ascii_digits(bytes, index + 4, 2)
                && decimal_component(bytes, index + 1, 2) <= 23
                && decimal_component(bytes, index + 4, 2) <= 59
        }
        _ => false,
    }
}

/// 保持 RFC3339 解析不依赖宽松 Unicode/整数转换，防止非 ASCII 数字被接受。
fn ascii_digits(bytes: &[u8], start: usize, len: usize) -> bool {
    bytes
        .get(start..start.saturating_add(len))
        .is_some_and(|part| part.len() == len && part.iter().all(u8::is_ascii_digit))
}

/// 将已经通过 ASCII 数字校验的日期片段转换为无失败的十进制值。
fn decimal_component(bytes: &[u8], start: usize, len: usize) -> u32 {
    bytes[start..start + len]
        .iter()
        .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'))
}

/// 按 Gregorian 闰年规则计算日期上限，阻止二月三十日等伪造时间戳。
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 检查 runtime 版本字段的存在性与长度，避免将任意对象误当握手能力。
pub(super) fn bounded_string(value: Option<&Value>, max_len: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty() && text.len() <= max_len)
}

/// 统一限制 supervisor deadline，并拒绝 Instant 加法溢出导致的无限等待。
pub(super) fn checked_deadline(
    timeout: Duration,
    maximum: Duration,
) -> Result<Instant, AgentProcessError> {
    if timeout > maximum {
        return Err(AgentProcessError::InvalidTimeout);
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or(AgentProcessError::InvalidTimeout)
}

/// 校验 server 宣告的每项 limit 均在冻结 Schema 且不超过 host 预算。
pub(super) fn validate_remote_limits(
    value: Option<&Value>,
    local: &Limits,
) -> Result<(), AgentProcessError> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Err(AgentProcessError::ProtocolFault);
    };
    let number = |key: &str| object.get(key).and_then(Value::as_u64);
    let number_usize = |key: &str| {
        number(key)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(AgentProcessError::ProtocolFault)
    };
    let max_frame = number_usize("maxFrameBytes")?;
    let inbound = number_usize("maxInboundQueueFrames")?;
    let outbound = number_usize("maxOutboundQueueFrames")?;
    let in_flight = number_usize("maxInFlightRequests")?;
    let pending = number_usize("maxPendingRequests")?;
    let item_delta = number("maxItemDeltaBytes").ok_or(AgentProcessError::ProtocolFault)?;
    let inline_output =
        number("maxInlineToolOutputBytes").ok_or(AgentProcessError::ProtocolFault)?;
    let artifact = number("maxArtifactBytes").ok_or(AgentProcessError::ProtocolFault)?;
    let logs = number("maxLogBytes").ok_or(AgentProcessError::ProtocolFault)?;
    let request_deadline =
        number("defaultRequestDeadlineMs").ok_or(AgentProcessError::ProtocolFault)?;
    let approval_deadline =
        number("defaultApprovalDeadlineMs").ok_or(AgentProcessError::ProtocolFault)?;
    if !(codec::MIN_MAX_FRAME_BYTES..=codec::MAX_MAX_FRAME_BYTES).contains(&max_frame)
        || !(1..=10_000).contains(&inbound)
        || !(1..=10_000).contains(&outbound)
        || !(1..=1_024).contains(&in_flight)
        || !(1..=1_024).contains(&pending)
        || !(256..=1_048_576).contains(&item_delta)
        || !(1_024..=16_777_216).contains(&inline_output)
        || !(1_048_576..=1_073_741_824).contains(&artifact)
        || !(4_096..=67_108_864).contains(&logs)
        || !(1_000..=3_600_000).contains(&request_deadline)
        || !(1_000..=3_600_000).contains(&approval_deadline)
        || max_frame != local.max_frame_bytes
        || inbound != local.inbound_queue_frames
        || outbound != local.outbound_queue_frames
        || in_flight != local.max_in_flight_requests
        || pending != local.max_pending_requests
        || item_delta != 65_536
        || inline_output != 1_048_576
        || artifact != 268_435_456
        || logs != local.max_log_bytes as u64
        || request_deadline != local.request_deadline_ms
        || approval_deadline != local.approval_deadline_ms
    {
        return Err(AgentProcessError::ProtocolFault);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁定 RFC3339 的结构/日期边界，防止 ready 校验退化为 starts_with 检查。
    #[test]
    fn timestamp_validation_is_strict_and_bounded() {
        assert!(valid_timestamp("2026-02-28T23:59:59Z"));
        assert!(valid_timestamp("2024-02-29T00:00:00.123456789+08:00"));
        assert!(valid_timestamp("2026-08-16T12:00:00-00:00"));
        assert!(!valid_timestamp("2026-02-29T23:59:59Z"));
        assert!(!valid_timestamp("2026-13-01T00:00:00Z"));
        assert!(!valid_timestamp("2026-01-01T24:00:00Z"));
        assert!(!valid_timestamp("2026-01-01T00:00:00"));
        assert!(!valid_timestamp("2026-01-01T00:00:00+8:00"));
    }

    /// 证明每个生成 challenge 都是固定长度的小写十六进制且不会按序号复用。
    #[test]
    fn ready_token_generation_is_csprng_shaped_and_unique() {
        let first = generate_ready_token().expect("platform CSPRNG available");
        let second = generate_ready_token().expect("platform CSPRNG available");
        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(first, second);
    }
}
