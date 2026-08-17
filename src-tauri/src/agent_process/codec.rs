// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! `ja-rpc/v1` 的传输边界。
//!
//! 这个模块只负责 framing 和 envelope；业务方法、pending 生命周期和进程状态
//! 保持在其它模块，避免一个“万能 manager”把协议错误误当成业务失败。

use serde_json::{Map, Value};
use std::fmt::{Display, Formatter};
use std::io::BufRead;

#[path = "codec_catalog.rs"]
mod codec_catalog;
#[path = "codec_json.rs"]
mod codec_json;

pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MIN_MAX_FRAME_BYTES: usize = 1024;
pub const MAX_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_REQUEST_ID_BYTES: usize = 98;
pub const MAX_METHOD_BYTES: usize = 128;
const READY_TOKEN_FIELD: &str = "readyToken";
const READY_TOKEN_HEX_BYTES: usize = 32;

/// 协商后的有界资源，使用协议默认值保持 Java 与 Rust 首次握手一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub inbound_queue_frames: usize,
    pub outbound_queue_frames: usize,
    pub max_in_flight_requests: usize,
    pub max_pending_requests: usize,
    pub max_tombstones: usize,
    pub max_stderr_line_bytes: usize,
    pub max_log_bytes: usize,
    pub request_deadline_ms: u64,
    pub approval_deadline_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            inbound_queue_frames: 256,
            outbound_queue_frames: 1_024,
            max_in_flight_requests: 64,
            max_pending_requests: 64,
            max_tombstones: 128,
            max_stderr_line_bytes: 64 * 1024,
            max_log_bytes: 1_048_576,
            request_deadline_ms: 120_000,
            approval_deadline_ms: 300_000,
        }
    }
}

impl Limits {
    /// 拒绝超过协议硬上限的本地配置，避免一次错误配置让 parser 无界分配。
    pub fn validate(&self) -> Result<(), CodecError> {
        if !(MIN_MAX_FRAME_BYTES..=MAX_MAX_FRAME_BYTES).contains(&self.max_frame_bytes) {
            return Err(CodecError::InvalidLimit);
        }
        if !(1..=10_000).contains(&self.inbound_queue_frames)
            || !(1..=10_000).contains(&self.outbound_queue_frames)
            || !(1..=1_024).contains(&self.max_in_flight_requests)
            || !(1..=1_024).contains(&self.max_pending_requests)
            || !(1..=8_192).contains(&self.max_tombstones)
            || !(1..=1_048_576).contains(&self.max_stderr_line_bytes)
            || !(4_096..=67_108_864).contains(&self.max_log_bytes)
            || !(1_000..=3_600_000).contains(&self.request_deadline_ms)
            || !(1_000..=3_600_000).contains(&self.approval_deadline_ms)
        {
            return Err(CodecError::InvalidLimit);
        }
        Ok(())
    }

    /// 把 host 的限制转换成 initialize 需要的 object，保持字段名与冻结 Schema 一致。
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "maxFrameBytes": self.max_frame_bytes,
            "maxInboundQueueFrames": self.inbound_queue_frames,
            "maxOutboundQueueFrames": self.outbound_queue_frames,
            "maxInFlightRequests": self.max_in_flight_requests,
            "maxPendingRequests": self.max_pending_requests,
            "maxItemDeltaBytes": 65_536,
            "maxInlineToolOutputBytes": 1_048_576,
            "maxLogBytes": self.max_log_bytes,
            "defaultRequestDeadlineMs": self.request_deadline_ms,
            "defaultApprovalDeadlineMs": self.approval_deadline_ms,
        })
    }
}

/// parser 失败必须是可枚举的稳定分类；不把原始 JSON、路径或 secret 放进错误文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    UnexpectedEof,
    PartialFrame,
    EmptyFrame,
    InvalidUtf8,
    InvalidJson,
    DuplicateKey,
    NonObject,
    InvalidEnvelope,
    HandshakeFailed,
    InvalidErrorCatalog,
    InvalidId,
    InvalidLimit,
    FrameTooLarge { actual: usize, max: usize },
    Io,
}

impl Display for CodecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedEof => formatter.write_str("unexpected eof"),
            Self::PartialFrame => formatter.write_str("partial jsonl frame"),
            Self::EmptyFrame => formatter.write_str("empty jsonl frame"),
            Self::InvalidUtf8 => formatter.write_str("invalid utf8"),
            Self::InvalidJson => formatter.write_str("invalid json"),
            Self::DuplicateKey => formatter.write_str("duplicate object key"),
            Self::NonObject => formatter.write_str("json-rpc frame is not an object"),
            Self::InvalidEnvelope => formatter.write_str("invalid json-rpc envelope"),
            Self::HandshakeFailed => formatter.write_str("handshake failed"),
            Self::InvalidErrorCatalog => formatter.write_str("invalid error catalog entry"),
            Self::InvalidId => formatter.write_str("invalid request id"),
            Self::InvalidLimit => formatter.write_str("invalid frame limit"),
            Self::FrameTooLarge { actual, max } => {
                write!(formatter, "frame exceeds limit ({actual} > {max})")
            }
            Self::Io => formatter.write_str("jsonl reader io failure"),
        }
    }
}

impl std::error::Error for CodecError {}

/// 保留字段是否出现，避免 serde 的 `Option` 把缺失和显式 null 混成同一个状态。
#[derive(Debug, Clone, PartialEq)]
pub struct Present<T> {
    present: bool,
    value: Option<T>,
}

impl<T> Present<T> {
    /// 构造缺失字段，保留与显式 null 不同的协议语义。
    pub fn missing() -> Self {
        Self {
            present: false,
            value: None,
        }
    }

    /// 构造存在且带值的字段，供 response/result 和测试 fixture 共用。
    pub fn some(value: T) -> Self {
        Self {
            present: true,
            value: Some(value),
        }
    }

    /// 构造存在但为 JSON null 的字段，避免 `Option` 抹掉字段存在性。
    pub fn null() -> Self {
        Self {
            present: true,
            value: None,
        }
    }

    /// 判断 wire object 是否显式包含此字段。
    pub fn is_present(&self) -> bool {
        self.present
    }

    /// 借用字段值，调用方无需消费整个 envelope。
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// 消费字段值，保留 present 状态供序列化方判断。
    pub fn into_value(self) -> Option<T> {
        self.value
    }
}

/// 已通过根 envelope 校验的 JSON-RPC frame。
#[derive(Clone, PartialEq)]
pub struct RpcFrame {
    id: Option<String>,
    method: Option<String>,
    params: Option<Value>,
    result: Present<Value>,
    error: Option<RpcError>,
}

#[derive(Clone, PartialEq)]
pub struct RpcError {
    code: i64,
    message: String,
    data: Value,
}

impl std::fmt::Debug for RpcFrame {
    /// Redact challenge-shaped strings before a frame can enter logs or UI diagnostics.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let params = self.params.as_ref().map(redact_debug_value);
        let result = self.result.value().map(redact_debug_value);
        formatter
            .debug_struct("RpcFrame")
            .field("id", &self.id.as_deref().map(redact_debug_text))
            .field("method", &self.method.as_deref().map(redact_debug_text))
            .field("params", &params)
            .field("result_present", &self.result.is_present())
            .field("result", &result)
            .field("error", &self.error)
            .finish()
    }
}

impl std::fmt::Debug for RpcError {
    /// Keep error debugging useful while removing challenge values and marker keys.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcError")
            .field("code", &self.code)
            .field("message", &redact_debug_text(&self.message))
            .field("data", &redact_debug_value(&self.data))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    ClientRequest,
    ServerRequest,
    Notification,
    Response,
}

impl RpcFrame {
    /// 构造并立即校验 client namespace，防止业务线程把 server ID 发到 wire。
    pub fn client_request(
        id: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Self, CodecError> {
        Self::request("c:", id, method, params)
    }

    /// 构造并立即校验 server namespace，用于 approval/secret/external-tool 回调。
    pub fn server_request(
        id: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Self, CodecError> {
        Self::request("s:", id, method, params)
    }

    /// 共享 request 字段布局并验证期望 namespace，保证构造路径与 decode 一致。
    fn request(
        namespace: &str,
        id: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Self, CodecError> {
        let frame = Self {
            id: Some(id.into()),
            method: Some(method.into()),
            params: Some(params),
            result: Present::missing(),
            error: None,
        };
        let kind = frame.validate()?;
        let expected = if namespace == "c:" {
            FrameKind::ClientRequest
        } else {
            FrameKind::ServerRequest
        };
        if kind != expected {
            return Err(CodecError::InvalidId);
        }
        Ok(frame)
    }

    /// 构造没有 ID 的 notification；notification 不会占用 pending 槽位。
    pub fn notification(method: impl Into<String>, params: Value) -> Result<Self, CodecError> {
        let frame = Self {
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: Present::missing(),
            error: None,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// 构造成功 response，即使 value 是 null 也保留 result 字段存在性。
    pub fn response_result(id: impl Into<String>, value: Value) -> Result<Self, CodecError> {
        let frame = Self {
            id: Some(id.into()),
            method: None,
            params: None,
            result: Present::some(value),
            error: None,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// 只构造 frozen catalog 中的 response，避免 caller 把任意诊断文本写入 wire。
    pub fn response_error(
        id: impl Into<String>,
        code: i64,
        message: impl Into<String>,
        ja_code: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, CodecError> {
        let frame = Self {
            id: Some(id.into()),
            method: None,
            params: None,
            result: Present::missing(),
            error: Some(RpcError {
                code,
                message: message.into(),
                data: serde_json::json!({
                    "jaCode": ja_code.into(),
                    "retryable": retryable,
                }),
            }),
        };
        frame.validate()?;
        Ok(frame)
    }

    /// 判断 root oneOf 分支，供 session 按方向区分 pending 与嵌套 request。
    /// 根据已验证字段推导方向，供 dispatcher 选择 pending 或 nested queue。
    pub fn kind(&self) -> Result<FrameKind, CodecError> {
        self.validate()
    }

    /// 返回 request/notification 的可选 method，避免调用方直接构造非法 envelope。
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// 返回已校验 params 的只读借用，保持 RpcFrame 字段不变式由构造器持有。
    pub fn params(&self) -> Option<&Value> {
        self.params.as_ref()
    }

    /// 返回 response result 的存在性对象，显式 null 与缺失保持可区分。
    pub fn result(&self) -> &Present<Value> {
        &self.result
    }

    /// 返回受控错误投影，调用方不能绕过 catalog 修改 error 字段。
    pub fn error(&self) -> Option<&RpcError> {
        self.error.as_ref()
    }

    /// 返回安全的空默认 ID，避免诊断/拒绝路径因 malformed frame 再次 panic。
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_default()
    }

    /// 返回可选原始 ID，供测试和路由区分缺失 ID 与空字符串。
    pub fn id_opt(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// 以严格单 frame JSONL 编码；调用方可在写队列入队前检查长度。
    pub fn encode(&self, max_frame_bytes: usize) -> Result<Vec<u8>, CodecError> {
        self.validate()?;
        let mut object = Map::new();
        object.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        if let Some(id) = &self.id {
            object.insert("id".to_owned(), Value::String(id.clone()));
        }
        if let Some(method) = &self.method {
            object.insert("method".to_owned(), Value::String(method.clone()));
        }
        if let Some(params) = &self.params {
            object.insert("params".to_owned(), params.clone());
        }
        if self.result.is_present() {
            object.insert(
                "result".to_owned(),
                self.result.value().cloned().unwrap_or(Value::Null),
            );
        }
        if let Some(error) = &self.error {
            let mut error_object = Map::new();
            error_object.insert("code".to_owned(), Value::Number(error.code.into()));
            error_object.insert("message".to_owned(), Value::String(error.message.clone()));
            error_object.insert("data".to_owned(), error.data.clone());
            object.insert("error".to_owned(), Value::Object(error_object));
        }
        let value = Value::Object(object);
        let allow_ready_token_path = self.method.as_deref() == Some("initialized")
            || (self.method.as_deref() == Some("runtime/statusChanged")
                && self
                    .params
                    .as_ref()
                    .and_then(|params| params.get("status"))
                    .and_then(Value::as_str)
                    == Some("ready"));
        // Re-run the raw guard after reconstructing the wire object so future
        // fields added to RpcFrame cannot bypass the outbound redaction gate.
        reject_raw_token_markers(&value, allow_ready_token_path)?;
        let mut encoded = serde_json::to_vec(&value).map_err(|_| CodecError::InvalidJson)?;
        if encoded.len() > max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                actual: encoded.len(),
                max: max_frame_bytes,
            });
        }
        encoded.push(b'\n');
        Ok(encoded)
    }

    /// 在手工构造和 decode 两条路径上共享同一 envelope 校验，防止内部 caller
    /// 通过构造器绕过 namespace、result/error oneOf 或稳定 error data 约束。
    pub fn validate(&self) -> Result<FrameKind, CodecError> {
        if let Some(id) = &self.id
            && !valid_id(id)
        {
            return Err(CodecError::InvalidId);
        }
        if let Some(method) = &self.method {
            if method.is_empty() || method.len() > MAX_METHOD_BYTES {
                return Err(CodecError::InvalidEnvelope);
            }
            if !self.params.as_ref().is_some_and(Value::is_object) {
                return Err(CodecError::InvalidEnvelope);
            }
            let params = self.params.as_ref().ok_or(CodecError::InvalidEnvelope)?;
            validate_ready_token_fields(method, params)?;
        }
        if let Some(error) = &self.error {
            validate_error(error)?;
        }
        if self
            .result
            .value()
            .is_some_and(contains_token_shaped_marker)
            || self
                .error
                .as_ref()
                .is_some_and(|error| contains_token_shaped_marker(&error.data))
        {
            return Err(CodecError::InvalidEnvelope);
        }
        let response = self.method.is_none()
            && self.id.is_some()
            && (self.result.is_present() ^ self.error.is_some());
        let request_or_notification =
            self.method.is_some() && !self.result.is_present() && self.error.is_none();
        if response {
            return Ok(match self.id.as_deref() {
                Some(id) if id.starts_with("c:") => FrameKind::Response,
                Some(id) if id.starts_with("s:") => FrameKind::Response,
                _ => return Err(CodecError::InvalidId),
            });
        }
        if request_or_notification {
            return Ok(match self.id.as_deref() {
                Some(id) if id.starts_with("c:") => FrameKind::ClientRequest,
                Some(id) if id.starts_with("s:") => FrameKind::ServerRequest,
                None => FrameKind::Notification,
                _ => return Err(CodecError::InvalidId),
            });
        }
        Err(CodecError::InvalidEnvelope)
    }
}

impl RpcError {
    /// 返回 response error code，供稳定错误路由使用。
    pub fn code(&self) -> i64 {
        self.code
    }

    /// 返回经过 bounded 脱敏校验的 message；分类仍由 code/jaCode/retryable 决定。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 返回脱敏错误 data 投影；未知 detail 在 decode 时已被丢弃。
    pub fn data(&self) -> &Value {
        &self.data
    }
}

/// 检查冻结 c:/s: namespace、ASCII 字符集和 98-byte 总长度。
fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_REQUEST_ID_BYTES {
        return false;
    }
    let Some((prefix, suffix)) = id.split_once(':') else {
        return false;
    };
    if prefix != "c" && prefix != "s" {
        return false;
    }
    let suffix_bytes = suffix.as_bytes();
    if suffix_bytes.is_empty() || suffix_bytes.len() > 96 {
        return false;
    }
    suffix_bytes[0].is_ascii_alphanumeric()
        && suffix_bytes[1..].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || *byte == b'.' || *byte == b'_' || *byte == b'-'
        })
}

/// 验证一个完整 frame，严格区分 request/notification/response 的 root 分支。
pub fn decode_frame(frame: &[u8], max_frame_bytes: usize) -> Result<RpcFrame, CodecError> {
    if frame.is_empty() {
        return Err(CodecError::UnexpectedEof);
    }
    if !frame.ends_with(b"\n") {
        return Err(CodecError::PartialFrame);
    }
    let payload = &frame[..frame.len() - 1];
    if payload.is_empty() {
        return Err(CodecError::EmptyFrame);
    }
    if payload.len() > max_frame_bytes {
        return Err(CodecError::FrameTooLarge {
            actual: payload.len(),
            max: max_frame_bytes,
        });
    }
    if payload.contains(&b'\n') || payload.ends_with(b"\r") {
        return Err(CodecError::InvalidEnvelope);
    }
    let text = std::str::from_utf8(payload).map_err(|_| CodecError::InvalidUtf8)?;
    let value = codec_json::parse_strict_value(text)?;
    let object = value.as_object().ok_or(CodecError::NonObject)?;
    let ready_frame = object.get("method").and_then(Value::as_str) == Some("runtime/statusChanged")
        && object
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("ready");
    let initialized_frame = object.get("method").and_then(Value::as_str) == Some("initialized");
    reject_raw_token_markers(&value, initialized_frame || ready_frame)?;
    reject_unknown_root_token_metadata(object, ready_frame)?;
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return Err(CodecError::InvalidEnvelope);
    }
    let id = match object.get("id") {
        None => None,
        Some(Value::String(id)) if valid_id(id) => Some(id.clone()),
        Some(Value::String(_)) => return Err(CodecError::InvalidId),
        Some(_) => return Err(CodecError::InvalidId),
    };
    let method = match object.get("method") {
        None => None,
        Some(Value::String(method)) if !method.is_empty() && method.len() <= MAX_METHOD_BYTES => {
            Some(method.clone())
        }
        Some(_) => return Err(CodecError::InvalidEnvelope),
    };
    let params = object.get("params").cloned();
    if method.is_some() && !params.as_ref().is_some_and(Value::is_object) {
        return Err(CodecError::InvalidEnvelope);
    }
    let result_present = object.contains_key("result");
    let result = if result_present {
        Present::some(object.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Present::missing()
    };
    let error = match object.get("error") {
        None => None,
        Some(Value::Object(error)) => Some(parse_error(error)?),
        Some(_) => return Err(CodecError::InvalidEnvelope),
    };
    let frame = RpcFrame {
        id,
        method,
        params,
        result,
        error,
    };
    frame.validate()?;
    Ok(frame)
}

/// 在错误投影和未知字段丢弃之前审计整帧，防止 ready challenge 藏入 error.detail。
///
/// `RpcError` 只保留稳定 catalog 字段，因此必须在构造受限 projection 之前递归检查
/// 原始 JSON；否则 `error.data.details.readyToken` 会在 parse_error 后永久丢失。
fn reject_raw_token_markers(value: &Value, allow_ready_token_path: bool) -> Result<(), CodecError> {
    fn visit(value: &Value, path: &mut Vec<String>, allow_ready_token_path: bool) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(key, child)| {
                path.push(key.clone());
                let legal_ready_path = allow_ready_token_path
                    && path.len() == 2
                    && path[0] == "params"
                    && key == "readyToken";
                let forbidden_key =
                    (is_ready_token_key(key) || is_token_shaped(key)) && !legal_ready_path;
                let forbidden_value =
                    child.as_str().is_some_and(is_token_shaped) && !legal_ready_path;
                let nested = !legal_ready_path && visit(child, path, allow_ready_token_path);
                path.pop();
                forbidden_key || forbidden_value || nested
            }),
            Value::Array(values) => values.iter().enumerate().any(|(index, child)| {
                path.push(format!("[{index}]"));
                let nested = visit(child, path, allow_ready_token_path);
                path.pop();
                nested
            }),
            Value::String(text) => is_token_shaped(text),
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }

    let mut path = Vec::new();
    if visit(value, &mut path, allow_ready_token_path) {
        return Err(if allow_ready_token_path {
            CodecError::HandshakeFailed
        } else {
            CodecError::InvalidEnvelope
        });
    }
    Ok(())
}

/// 只允许握手的两个精确 params 位置携带 readyToken，防止未知 root 扩展绕过脱敏边界。
fn validate_ready_token_fields(method: &str, params: &Value) -> Result<(), CodecError> {
    let object = params.as_object().ok_or(CodecError::InvalidEnvelope)?;
    match method {
        "initialized"
            if object.len() != 1
                || !object
                    .get(READY_TOKEN_FIELD)
                    .and_then(Value::as_str)
                    .is_some_and(valid_ready_token) =>
        {
            return Err(CodecError::HandshakeFailed);
        }
        "initialized" => {}
        "runtime/statusChanged"
            if object.get("status").and_then(Value::as_str) == Some("ready") =>
        {
            if !object
                .get(READY_TOKEN_FIELD)
                .and_then(Value::as_str)
                .is_some_and(valid_ready_token)
            {
                return Err(CodecError::HandshakeFailed);
            }
            for (key, value) in object {
                if key != READY_TOKEN_FIELD
                    && (is_ready_token_key(key)
                        || is_token_shaped(key)
                        || contains_token_shaped_marker(value))
                {
                    return Err(CodecError::HandshakeFailed);
                }
            }
        }
        _ if contains_token_shaped_marker(params) => return Err(CodecError::InvalidEnvelope),
        _ => {}
    }
    Ok(())
}

/// 未知 root 字段不会进入 RpcFrame，先检查其 token 标记，避免丢弃后无法做安全审计。
fn reject_unknown_root_token_metadata(
    object: &Map<String, Value>,
    ready_frame: bool,
) -> Result<(), CodecError> {
    const KNOWN: &[&str] = &["jsonrpc", "id", "method", "params", "result", "error"];
    for (key, value) in object {
        if KNOWN.contains(&key.as_str()) {
            continue;
        }
        if is_ready_token_key(key) || is_token_shaped(key) || contains_token_shaped_marker(value) {
            return Err(if ready_frame {
                CodecError::HandshakeFailed
            } else {
                CodecError::InvalidEnvelope
            });
        }
    }
    Ok(())
}

/// 递归拒绝 ready frame 中任意 token-shaped key/value，避免 challenge 藏入扩展字段。
pub(super) fn contains_token_shaped_marker(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, child)| {
            is_ready_token_key(key) || is_token_shaped(key) || contains_token_shaped_marker(child)
        }),
        Value::Array(values) => values.iter().any(contains_token_shaped_marker),
        Value::String(text) => is_token_shaped(text),
        _ => false,
    }
}

/// 校验 v1 challenge 的固定小写十六进制文本，不接受短值、Unicode 或大小写变体。
pub fn valid_ready_token(value: &str) -> bool {
    value.len() == READY_TOKEN_HEX_BYTES
        && value
            .as_bytes()
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// 识别 token 的固定长度 ASCII hex 形状，大小写不敏感但不接受 Unicode hex。
pub(super) fn is_token_shaped(value: &str) -> bool {
    value.len() == READY_TOKEN_HEX_BYTES && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

/// 识别 readyToken 字段的 ASCII 大小写变体，只有精确字段名才能走握手例外。
pub(super) fn is_ready_token_key(key: &str) -> bool {
    key.eq_ignore_ascii_case(READY_TOKEN_FIELD)
}

/// Redact an exact challenge-shaped text without retaining the original in a debug buffer.
fn redact_debug_text(value: &str) -> String {
    if is_token_shaped(value) {
        "<redacted>".to_owned()
    } else {
        value.to_owned()
    }
}

/// Recursively redact token keys, token-shaped keys, and token-shaped values for Debug.
fn redact_debug_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, child)| {
                    let safe_key = if is_ready_token_key(key) || is_token_shaped(key) {
                        "<redacted>".to_owned()
                    } else {
                        key.to_owned()
                    };
                    (safe_key, redact_debug_value(child))
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_debug_value).collect()),
        Value::String(text) => Value::String(redact_debug_text(text)),
        other => other.clone(),
    }
}

/// 从 wire error 读取 bounded code/message/jaCode/retryable，拒绝伪造错误对象。
fn parse_error(object: &Map<String, Value>) -> Result<RpcError, CodecError> {
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .filter(|code| (-32_768..=-32_000).contains(code))
        .ok_or(CodecError::InvalidEnvelope)?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| valid_error_message(message))
        .ok_or(CodecError::InvalidEnvelope)?
        .to_owned();
    let data = object.get("data").ok_or(CodecError::InvalidEnvelope)?;
    let data_object = data.as_object().ok_or(CodecError::InvalidEnvelope)?;
    let ja_code = data_object
        .get("jaCode")
        .and_then(Value::as_str)
        .filter(|code| valid_ja_code(code))
        .ok_or(CodecError::InvalidEnvelope)?;
    let retryable = data_object
        .get("retryable")
        .and_then(Value::as_bool)
        .ok_or(CodecError::InvalidEnvelope)?;
    // Rebuild a safe projection instead of retaining arbitrary server detail;
    // error data can contain paths, prompts, source snippets, or credentials.
    let error = RpcError {
        code,
        message,
        data: serde_json::json!({
            "jaCode": ja_code,
            "retryable": retryable,
        }),
    };
    validate_error(&error)?;
    Ok(error)
}

/// 检查稳定错误代码的全大写 ASCII 形状，避免把原始异常文本当 jaCode。
fn valid_ja_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    !bytes.is_empty()
        && bytes.len() >= 3
        && bytes.len() <= 64
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// 复用 decode 的 error 约束校验手工 response，防止内部 caller 绕过 schema。
fn validate_error(error: &RpcError) -> Result<(), CodecError> {
    if !(-32_768..=-32_000).contains(&error.code) || !valid_error_message(&error.message) {
        return Err(CodecError::InvalidEnvelope);
    }
    let Some(data) = error.data.as_object() else {
        return Err(CodecError::InvalidEnvelope);
    };
    let ja_code = data
        .get("jaCode")
        .and_then(Value::as_str)
        .filter(|code| valid_ja_code(code))
        .ok_or(CodecError::InvalidErrorCatalog)?;
    let retryable = data
        .get("retryable")
        .and_then(Value::as_bool)
        .ok_or(CodecError::InvalidErrorCatalog)?;
    // code/jaCode/retryable are the sole stable classifier.  Localized bounded
    // messages remain display text and must not decide the catalog entry.
    catalog_entry(error.code, ja_code, retryable).ok_or(CodecError::InvalidErrorCatalog)?;
    Ok(())
}

/// 只允许 bounded、可显示的错误 message，拒绝把路径、凭据或 challenge
/// 形状的原始诊断带入 UI/日志，同时保留合法本地化文案。
fn valid_error_message(message: &str) -> bool {
    if message.is_empty() || message.len() > 512 {
        return false;
    }
    ErrorMessageScanner::new(message).is_safe()
}

/// Bounded lexical scanner for user-visible error text; keeping all safety
/// checks in one pass prevents a new marker rule from bypassing another.
struct ErrorMessageScanner<'a> {
    message: &'a str,
    bytes: &'a [u8],
}

impl<'a> ErrorMessageScanner<'a> {
    /// Borrow a bounded message so every scanner operation has a fixed input cap.
    fn new(message: &'a str) -> Self {
        Self {
            message,
            bytes: message.as_bytes(),
        }
    }

    /// Reject control characters and lexical URI/path/secret/token indicators.
    fn is_safe(&self) -> bool {
        !self.message.chars().any(char::is_control)
            && !self.has_uri()
            && !self.has_posix_path()
            && !self.has_windows_path()
            && !self.has_secret_marker()
            && !self.has_hex_run()
    }

    /// Detect any ASCII scheme followed by `://`, including URL prefixes,
    /// suffixes and query strings without decoding percent escapes.
    fn has_uri(&self) -> bool {
        self.bytes.windows(3).enumerate().any(|(index, window)| {
            if window != b"://" {
                return false;
            }
            let mut start = index;
            while start > 0 && is_scheme_byte(self.bytes[start - 1]) {
                start -= 1;
            }
            start < index && self.bytes[start].is_ascii_alphabetic()
        })
    }

    /// Detect absolute POSIX paths at lexical boundaries while allowing normal
    /// Chinese slash punctuation such as `失败/重试`.
    fn has_posix_path(&self) -> bool {
        for index in 0..self.bytes.len() {
            if self.bytes[index] != b'/' || !is_path_boundary(self.bytes, index) {
                continue;
            }
            let segment_start = index + 1;
            if segment_start >= self.bytes.len() || !is_path_byte(self.bytes[segment_start]) {
                continue;
            }
            let mut end = segment_start + 1;
            while end < self.bytes.len() && is_path_byte(self.bytes[end]) {
                end += 1;
            }
            let segment = &self.bytes[segment_start..end];
            let has_nested_component = self.bytes.get(end) == Some(&b'/');
            let known_root = matches!(
                segment,
                b"etc"
                    | b"home"
                    | b"Users"
                    | b"users"
                    | b"private"
                    | b"tmp"
                    | b"var"
                    | b"usr"
                    | b"opt"
                    | b"root"
                    | b"dev"
                    | b"proc"
                    | b"sys"
                    | b"Volumes"
                    | b"volumes"
            );
            if has_nested_component || known_root {
                return true;
            }
        }
        false
    }

    /// Detect drive-letter and UNC paths without decoding or normalizing input.
    fn has_windows_path(&self) -> bool {
        self.bytes.windows(2).any(|window| window == b"\\\\")
            || self.bytes.windows(3).any(|window| {
                window[0].is_ascii_alphabetic()
                    && window[1] == b':'
                    && matches!(window[2], b'/' | b'\\')
            })
    }

    /// Detect case/separator variants of credential labels without treating
    /// ordinary localized wording as a secret value.
    fn has_secret_marker(&self) -> bool {
        let lower = self.message.to_ascii_lowercase();
        let compact = lower
            .bytes()
            .filter(|byte| byte.is_ascii_alphanumeric())
            .collect::<Vec<_>>();
        [
            b"secret".as_slice(),
            b"token".as_slice(),
            b"password".as_slice(),
            b"apikey".as_slice(),
            b"bearer".as_slice(),
            b"cookie".as_slice(),
            b"authorization".as_slice(),
        ]
        .iter()
        .any(|marker| {
            compact
                .windows(marker.len())
                .any(|window| window == *marker)
        }) || lower.contains("sk-")
            || lower.contains("sk_")
    }

    /// Detect contiguous ASCII hex runs that could carry a challenge even when
    /// embedded in otherwise displayable text.
    fn has_hex_run(&self) -> bool {
        let mut run = 0_usize;
        for byte in self.bytes {
            if byte.is_ascii_hexdigit() {
                run = run.saturating_add(1);
                if run >= 32 {
                    return true;
                }
            } else {
                run = 0;
            }
        }
        false
    }
}

/// Scheme names follow RFC-style ASCII letters/digits plus `+.-`.
fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

/// Restrict path segments to ASCII lexical characters before declaring a path.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'%')
}

/// A path boundary is ASCII punctuation/whitespace; non-ASCII preceding bytes
/// preserve common Chinese slash prose instead of treating it as `/path`.
fn is_path_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0
        || bytes[index - 1].is_ascii_whitespace()
        || matches!(
            bytes[index - 1],
            b':' | b'=' | b'(' | b'[' | b'{' | b'"' | b','
        )
}

/// 通过独立 catalog 模块查询稳定错误映射，避免 framing 文件承担业务表维护。
pub(crate) fn catalog_entry(
    code: i64,
    ja_code: &str,
    retryable: bool,
) -> Option<(&'static str, &'static str)> {
    codec_catalog::catalog_entry(code, ja_code, retryable)
}

/// 逐个读取 LF frame，只消费当前行，避免一次 pipe read 吞掉下一帧。
pub fn read_frame<R: BufRead>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<RpcFrame, CodecError> {
    let mut line = Vec::with_capacity(max_frame_bytes.min(8192).saturating_add(1));
    loop {
        let chunk = reader.fill_buf().map_err(|_| CodecError::Io)?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Err(CodecError::UnexpectedEof)
            } else {
                Err(CodecError::PartialFrame)
            };
        }
        if let Some(index) = chunk.iter().position(|byte| *byte == b'\n') {
            let payload_len = line.len().saturating_add(index);
            if payload_len > max_frame_bytes {
                return Err(CodecError::FrameTooLarge {
                    actual: payload_len,
                    max: max_frame_bytes,
                });
            }
            line.extend_from_slice(&chunk[..=index]);
            reader.consume(index + 1);
            return decode_frame(&line, max_frame_bytes);
        }
        if line.len().saturating_add(chunk.len()) > max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                actual: max_frame_bytes.saturating_add(1),
                max: max_frame_bytes,
            });
        }
        line.extend_from_slice(chunk);
        let consumed = chunk.len();
        reader.consume(consumed);
    }
}

/// 版本协商的最小闭包，未知 minor 只有在双方最低兼容声明满足时才可继续。
pub fn negotiate_version(
    local_major: i64,
    local_minor: i64,
    local_minimum: i64,
    remote_major: i64,
    remote_minor: i64,
    remote_minimum: i64,
) -> Result<i64, CodecError> {
    if local_major != remote_major {
        return Err(CodecError::InvalidEnvelope);
    }
    let selected = local_minor.min(remote_minor);
    if selected < local_minimum || selected < remote_minimum {
        return Err(CodecError::InvalidEnvelope);
    }
    Ok(selected)
}
