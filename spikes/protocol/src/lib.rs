// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! `ja-rpc/v1` 的隔离协议探针。
//!
//! 这里不复刻生产协议实现，而是把最容易在 Java/Rust 双进程中出错的
//! framing、pending、背压和 snapshot/live 状态机压缩成可重复的测试模型。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

/// 探针使用的默认 frame 上限，必须始终小于等于协商后的生产上限。
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4096;

/// 可由强类型实现读取的 JSON-RPC envelope；未声明字段保留以模拟 minor 兼容。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RpcFrame {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// JSON-RPC error 的最小强类型表示，便于探针检查错误不是静默丢弃。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

/// LF-JSONL framing 的失败原因；每种输入错误都必须让连接进入明确失败路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Eof,
    PartialFrame,
    EmptyFrame,
    FrameTooLarge { actual: usize, max: usize },
    InvalidUtf8,
    InvalidJson(String),
    NonObject,
    InvalidEnvelope(String),
    Io(String),
}

impl Display for FrameError {
    // 将内部失败分类保留在诊断文本中，方便探针区分 framing、envelope 和 IO 故障。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => formatter.write_str("eof"),
            Self::PartialFrame => formatter.write_str("partial frame without LF"),
            Self::EmptyFrame => formatter.write_str("empty frame"),
            Self::FrameTooLarge { actual, max } => {
                write!(formatter, "frame too large: {actual} > {max}")
            }
            Self::InvalidUtf8 => formatter.write_str("invalid utf-8"),
            Self::InvalidJson(message) => write!(formatter, "invalid json: {message}"),
            Self::NonObject => formatter.write_str("json-rpc frame is not an object"),
            Self::InvalidEnvelope(message) => write!(formatter, "invalid envelope: {message}"),
            Self::Io(message) => write!(formatter, "frame io error: {message}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 仅接受完整 LF 结尾并限制 frame 大小，避免半行或日志污染被当成协议消息。
pub fn decode_frame(frame: &[u8], max_frame_bytes: usize) -> Result<RpcFrame, FrameError> {
    if frame.is_empty() {
        return Err(FrameError::Eof);
    }
    if !frame.ends_with(b"\n") {
        return Err(FrameError::PartialFrame);
    }
    let payload = &frame[..frame.len() - 1];
    if payload.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    if payload.len() > max_frame_bytes {
        return Err(FrameError::FrameTooLarge {
            actual: payload.len(),
            max: max_frame_bytes,
        });
    }
    let text = std::str::from_utf8(payload).map_err(|_| FrameError::InvalidUtf8)?;
    let value: Value =
        serde_json::from_str(text).map_err(|error| FrameError::InvalidJson(error.to_string()))?;
    if !value.is_object() {
        return Err(FrameError::NonObject);
    }
    let Some(object) = value.as_object() else {
        return Err(FrameError::NonObject);
    };
    let has_method = object.contains_key("method");
    let has_id = object.contains_key("id");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if let Some(method) = object.get("method")
        && !method.is_string()
    {
        return Err(FrameError::InvalidEnvelope(
            "method must be a string when present".to_owned(),
        ));
    }
    if let Some(id) = object.get("id") {
        let Some(id) = id.as_str() else {
            return Err(FrameError::InvalidEnvelope(
                "id must be a c:/s: string when present".to_owned(),
            ));
        };
        if !(id.starts_with("c:") || id.starts_with("s:")) {
            return Err(FrameError::InvalidEnvelope(
                "id must use c: or s: namespace".to_owned(),
            ));
        }
    }
    if has_error && object.get("error").is_some_and(Value::is_null) {
        return Err(FrameError::InvalidEnvelope(
            "error cannot be null".to_owned(),
        ));
    }
    let frame: RpcFrame = serde_json::from_value(value)
        .map_err(|error| FrameError::InvalidEnvelope(error.to_string()))?;
    let mut frame = frame;
    if has_result && frame.result.is_none() {
        // serde Option treats null as None; restoring Value::Null preserves result presence.
        frame.result = Some(Value::Null);
    }
    if frame.jsonrpc != "2.0" {
        return Err(FrameError::InvalidEnvelope(
            "jsonrpc must be exactly 2.0".to_owned(),
        ));
    }
    let has_method = has_method && frame.method.is_some();
    let is_valid_request = has_method && !has_result && !has_error;
    let is_valid_response = !has_method && has_id && (has_result ^ has_error);
    if !is_valid_request && !is_valid_response {
        return Err(FrameError::InvalidEnvelope(
            "request/notification needs method; response needs exactly one of result/error"
                .to_owned(),
        ));
    }
    Ok(frame)
}

/// 从有界 BufRead 逐个消费 LF frame；只 consume 当前行，避免一次 read 丢掉后续 frame。
pub fn read_frame<R: std::io::BufRead>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<RpcFrame, FrameError> {
    let mut frame = Vec::with_capacity(max_frame_bytes.saturating_add(1));
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|error| FrameError::Io(error.to_string()))?;
        if buffer.is_empty() {
            return if frame.is_empty() {
                Err(FrameError::Eof)
            } else {
                Err(FrameError::PartialFrame)
            };
        }

        if let Some(line_end) = buffer.iter().position(|byte| *byte == b'\n') {
            let payload_len = frame.len().saturating_add(line_end);
            if payload_len > max_frame_bytes {
                return Err(FrameError::FrameTooLarge {
                    actual: max_frame_bytes.saturating_add(1),
                    max: max_frame_bytes,
                });
            }
            frame.extend_from_slice(&buffer[..=line_end]);
            reader.consume(line_end + 1);
            return decode_frame(&frame, max_frame_bytes);
        }

        let remaining = max_frame_bytes.saturating_sub(frame.len());
        if buffer.len() > remaining {
            return Err(FrameError::FrameTooLarge {
                actual: max_frame_bytes.saturating_add(1),
                max: max_frame_bytes,
            });
        }
        frame.extend_from_slice(buffer);
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

/// 有界 channel 的发送端，使用 try_send 让 backpressure 成为可观察的协议结果。
pub struct BoundedSender<T> {
    sender: SyncSender<T>,
}

impl<T> Clone for BoundedSender<T> {
    // 克隆 sender 只复制 channel handle，不复制队列容量，确保所有 writer 共享同一上限。
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

/// 有界 channel 的接收端；关闭时所有发送者都能确定收到错误而不是无限等待。
pub struct BoundedReceiver<T> {
    receiver: Receiver<T>,
}

/// 建立固定容量的 inbound/outbound 队列，防止慢消费者导致无限 buffer。
pub fn bounded_channel<T>(capacity: usize) -> (BoundedSender<T>, BoundedReceiver<T>) {
    assert!(capacity > 0, "a protocol queue must have a positive bound");
    let (sender, receiver) = mpsc::sync_channel(capacity);
    (BoundedSender { sender }, BoundedReceiver { receiver })
}

impl<T> BoundedSender<T> {
    /// 非阻塞入队；队列满时返回明确的 overload 信号，调用方可以发送 QUEUE_FULL。
    pub fn try_send(&self, item: T) -> Result<(), QueueError<T>> {
        self.sender.try_send(item).map_err(|error| match error {
            TrySendError::Full(item) => QueueError::Full(item),
            TrySendError::Disconnected(item) => QueueError::Closed(item),
        })
    }
}

impl<T> BoundedReceiver<T> {
    /// 读取一个已排队项目；断开时返回 None，促使上层释放 pending 和 child。
    pub fn recv(&self) -> Option<T> {
        self.receiver.recv().ok()
    }
}

/// 有界队列发送失败的稳定分类。
#[derive(Debug, PartialEq, Eq)]
pub enum QueueError<T> {
    Full(T),
    Closed(T),
}

/// pending request 的生命周期；终态之后的 response 绝不能再次触发副作用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    Pending,
    Resolved,
    TimedOut,
    Cancelled,
}

/// response 到达 pending registry 后的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    First,
    DuplicateResponse,
    LateResponse,
    UnknownRequest,
}

/// 受上限保护的 pending registry，同时建模 deadline/cancel/duplicate/late 语义。
#[derive(Debug)]
pub struct PendingRegistry {
    active: HashSet<String>,
    tombstones: VecDeque<(String, PendingState)>,
    max_pending: usize,
    max_tombstones: usize,
}

impl PendingRegistry {
    /// 固定 pending 上限，避免嵌套请求把内存和等待句柄耗尽。
    pub fn new(max_pending: usize) -> Self {
        Self::with_tombstone_limit(max_pending, max_pending.max(1))
    }

    /// 独立限制 tombstone，使终态可审计但不会永久占用 active pending 容量。
    pub fn with_tombstone_limit(max_pending: usize, max_tombstones: usize) -> Self {
        Self {
            active: HashSet::new(),
            tombstones: VecDeque::new(),
            max_pending,
            max_tombstones,
        }
    }

    /// 注册唯一 request id；重复 id 和超上限都在发送副作用前失败。
    pub fn register(&mut self, request_id: impl Into<String>) -> Result<(), PendingError> {
        let request_id = request_id.into();
        if self.active.contains(&request_id) || self.tombstone_state(&request_id).is_some() {
            return Err(PendingError::DuplicateRequest);
        }
        if self.active.len() >= self.max_pending {
            return Err(PendingError::LimitReached);
        }
        self.active.insert(request_id);
        Ok(())
    }

    /// 标记 deadline 到期；迟到 response 只能被观察，不能恢复原操作。
    pub fn expire(&mut self, request_id: &str) -> bool {
        self.terminate(request_id, PendingState::TimedOut)
    }

    /// 标记用户或 shutdown 取消；取消是幂等的并且不会清除审计状态。
    pub fn cancel(&mut self, request_id: &str) -> bool {
        self.terminate(request_id, PendingState::Cancelled)
    }

    /// 只允许 Pending 进入 Resolved，保证 approval/tool 副作用 exactly-once。
    pub fn resolve(&mut self, request_id: &str) -> ResolveOutcome {
        if self.active.remove(request_id) {
            self.remember_terminal(request_id, PendingState::Resolved);
            return ResolveOutcome::First;
        }
        match self.tombstone_state(request_id) {
            Some(PendingState::Resolved) => ResolveOutcome::DuplicateResponse,
            Some(PendingState::TimedOut | PendingState::Cancelled) => ResolveOutcome::LateResponse,
            Some(PendingState::Pending) | None => ResolveOutcome::UnknownRequest,
        }
    }

    /// active entry 进入终态时立即释放容量，同时保留有限 tombstone 供迟到响应分类。
    fn terminate(&mut self, request_id: &str, target: PendingState) -> bool {
        if self.active.remove(request_id) {
            self.remember_terminal(request_id, target);
            true
        } else {
            false
        }
    }

    /// 把 terminal 分类压入有界 tombstone，旧记录淘汰后只会退化为 unknown。
    fn remember_terminal(&mut self, request_id: &str, state: PendingState) {
        if self.max_tombstones == 0 {
            return;
        }
        if let Some(index) = self
            .tombstones
            .iter()
            .position(|(known_id, _)| known_id == request_id)
        {
            self.tombstones.remove(index);
        }
        while self.tombstones.len() >= self.max_tombstones {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back((request_id.to_owned(), state));
    }

    /// 查询最近的终态分类；不把 tombstone 伪装成仍占用的 active request。
    fn tombstone_state(&self, request_id: &str) -> Option<PendingState> {
        self.tombstones
            .iter()
            .find_map(|(known_id, state)| (known_id == request_id).then_some(*state))
    }

    /// 返回 active pending 数，终态一旦记录就不再阻塞新 request。
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// 判断是否没有 active pending，供调用方在关闭和压力路径快速分支。
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// 返回 tombstone 数，便于验收其自身也受上限保护。
    pub fn tombstone_len(&self) -> usize {
        self.tombstones.len()
    }
}

/// 注册 pending 失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingError {
    DuplicateRequest,
    LimitReached,
}

/// 事件 reducer 对 snapshot/live 的最小状态，显式拒绝 seq 缺口而不猜测补齐。
#[derive(Debug)]
pub struct SequenceReducer {
    last_seq: u64,
    event_ids: HashSet<String>,
    buffered: BTreeMap<u64, SequencedEvent>,
    buffered_event_ids: HashSet<String>,
    max_buffer: usize,
}

impl Default for SequenceReducer {
    // 默认缓存只用于探针；生产端仍应使用 initialize 协商后的更严格上限。
    fn default() -> Self {
        Self::new(64)
    }
}

/// 用于 reducer 验证的精简事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencedEvent {
    pub seq: u64,
    pub event_id: String,
}

/// reducer 应用事件后的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceOutcome {
    Applied,
    Duplicate,
    Gap { expected: u64, received: u64 },
    ResyncRequired { expected: u64, received: u64 },
}

impl SequenceReducer {
    /// 创建有界乱序缓存，避免异常 seq 流耗尽 reducer 内存。
    pub fn new(max_buffer: usize) -> Self {
        Self {
            last_seq: 0,
            event_ids: HashSet::new(),
            buffered: BTreeMap::new(),
            buffered_event_ids: HashSet::new(),
            max_buffer,
        }
    }

    /// 只接受连续 seq；重复事件幂等，缺口触发 snapshot/resync 而不是伪造状态。
    pub fn apply_live(&mut self, event: SequencedEvent) -> SequenceOutcome {
        if self.event_ids.contains(&event.event_id) || event.seq <= self.last_seq {
            return SequenceOutcome::Duplicate;
        }
        if self.buffered_event_ids.contains(&event.event_id) {
            return SequenceOutcome::Duplicate;
        }
        let expected = self.last_seq.saturating_add(1);
        if self.buffered.contains_key(&event.seq) {
            return SequenceOutcome::ResyncRequired {
                expected,
                received: event.seq,
            };
        }
        if event.seq == expected {
            self.apply_contiguous(event);
            return SequenceOutcome::Applied;
        }
        if self.buffered.len() >= self.max_buffer {
            return SequenceOutcome::ResyncRequired {
                expected,
                received: event.seq,
            };
        }
        self.buffered_event_ids.insert(event.event_id.clone());
        self.buffered.insert(event.seq, event);
        SequenceOutcome::Gap {
            expected,
            received: self.buffered.keys().next().copied().unwrap_or(expected),
        }
    }

    /// 用权威 snapshot 重新定位 seq，并丢弃不可能再应用的旧 buffer。
    pub fn apply_snapshot(&mut self, snapshot_seq: u64) {
        self.last_seq = snapshot_seq;
        let stale: Vec<u64> = self
            .buffered
            .keys()
            .copied()
            .filter(|seq| *seq <= snapshot_seq)
            .collect();
        for seq in stale {
            if let Some(event) = self.buffered.remove(&seq) {
                self.buffered_event_ids.remove(&event.event_id);
            }
        }
    }

    /// 当前权威 seq，便于确认 snapshot/live 收口后的状态。
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// 只按 next expected 取出缓存，避免 4→3 的乱序让高 seq 永久挡住低 seq。
    pub fn drain_buffered(&mut self) -> SequenceOutcome {
        loop {
            let expected = self.last_seq.saturating_add(1);
            let Some(event) = self.buffered.remove(&expected) else {
                return self
                    .buffered
                    .keys()
                    .next()
                    .map_or(SequenceOutcome::Applied, |received| SequenceOutcome::Gap {
                        expected,
                        received: *received,
                    });
            };
            self.buffered_event_ids.remove(&event.event_id);
            self.apply_contiguous(event);
        }
    }

    /// 应用已确定连续的 event；调用方已负责从乱序缓存和重复集合中移除它。
    fn apply_contiguous(&mut self, event: SequencedEvent) {
        self.event_ids.insert(event.event_id);
        self.last_seq = event.seq;
    }

    /// 返回乱序缓存数量，便于验证其上限不会被异常流突破。
    pub fn buffered_len(&self) -> usize {
        self.buffered.len()
    }
}

/// 读取冻结 golden fixture，并用强类型 envelope 验证跨语言输入不是“只看字符串”。
pub fn read_golden_core() -> Result<Vec<RpcFrame>, Box<dyn std::error::Error>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/golden/valid/core.jsonl");
    let content = std::fs::read_to_string(path)?;
    content
        .lines()
        .map(|line| {
            let mut framed = line.as_bytes().to_vec();
            framed.push(b'\n');
            decode_frame(&framed, DEFAULT_MAX_FRAME_BYTES)
                .map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    /// 这些输入必须失败而不是被当成日志或隐式补全，以免 stdout 污染隐藏 sidecar 故障。
    fn frame_rejects_partial_invalid_utf8_and_stdout_noise() {
        assert_eq!(
            decode_frame(b"{\"jsonrpc\":\"2.0\"}", 128),
            Err(FrameError::PartialFrame)
        );
        assert_eq!(
            decode_frame(&[0xff, b'\n'], 128),
            Err(FrameError::InvalidUtf8)
        );
        assert!(matches!(
            decode_frame(b"INFO startup\n", 128),
            Err(FrameError::InvalidJson(_))
        ));
        assert_eq!(decode_frame(b"[]\n", 128), Err(FrameError::NonObject));
        assert!(matches!(
            decode_frame(b"{\"jsonrpc\":\"1.0\",\"method\":\"version\"}\n", 128),
            Err(FrameError::InvalidEnvelope(_))
        ));
        assert!(matches!(
            decode_frame(
                b"{\"jsonrpc\":\"2.0\",\"id\":\"c:bad\",\"result\":{},\"error\":{\"code\":-1,\"message\":\"bad\"}}\n",
                256
            ),
            Err(FrameError::InvalidEnvelope(_))
        ));
        assert!(matches!(
            decode_frame(b"{\"jsonrpc\":\"2.0\",\"id\":\"c:bad\"}\n", 128),
            Err(FrameError::InvalidEnvelope(_))
        ));
        assert!(matches!(
            decode_frame(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{}}\n", 128),
            Err(FrameError::InvalidEnvelope(_))
        ));
        assert!(matches!(
            decode_frame(
                b"{\"jsonrpc\":\"2.0\",\"id\":\"c:error-null\",\"error\":null}\n",
                128
            ),
            Err(FrameError::InvalidEnvelope(_))
        ));
        assert!(matches!(
            decode_frame(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n", 128),
            Err(FrameError::InvalidEnvelope(_))
        ));
    }

    #[test]
    /// result:null 是合法 response，字段存在性必须与字段值 null 分离判断。
    fn frame_accepts_explicit_null_result() {
        let frame = decode_frame(
            b"{\"jsonrpc\":\"2.0\",\"id\":\"c:null-result\",\"result\":null}\n",
            128,
        )
        .expect("explicit null result is valid");
        assert_eq!(frame.result, Some(Value::Null));
        assert_eq!(frame.error, None);
    }

    #[test]
    /// 非 null error 必须完整解析为 RpcError，不能因 oneOf 检查丢掉错误详情。
    fn frame_accepts_structured_error_response() {
        let frame = decode_frame(
            b"{\"jsonrpc\":\"2.0\",\"id\":\"c:error\",\"error\":{\"code\":-32080,\"message\":\"failed\"}}\n",
            128,
        )
        .expect("structured error response is valid");
        let error = frame.error.expect("error is present");
        assert_eq!(error.code, -32080);
        assert_eq!(error.message, "failed");
        assert_eq!(frame.result, None);
    }

    #[test]
    /// BufRead 必须只消费第一行，保证一次 OS read 取到两帧时第二帧仍可读取。
    fn bounded_reader_preserves_consecutive_frames() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":\"c:one\",\"result\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":\"c:two\",\"result\":{}}\n";
        let mut reader = BufReader::new(Cursor::new(input));
        let first = read_frame(&mut reader, 128).expect("first frame");
        let second = read_frame(&mut reader, 128).expect("second frame");
        assert_eq!(first.id.as_deref(), Some("c:one"));
        assert_eq!(second.id.as_deref(), Some("c:two"));
        assert_eq!(read_frame(&mut reader, 128), Err(FrameError::Eof));
    }

    #[test]
    /// 没有 LF 的输入在达到上限时立即失败，不能依赖无界 read_until 等待更多数据。
    fn bounded_reader_rejects_oversized_partial_frame() {
        let input = vec![b'x'; 64];
        let mut reader = BufReader::with_capacity(8, Cursor::new(input));
        assert_eq!(
            read_frame(&mut reader, 8),
            Err(FrameError::FrameTooLarge { actual: 9, max: 8 })
        );
    }

    #[test]
    /// 小于协商上限的 frame 才能进入 JSON 解析，避免解析器先分配无界内存。
    fn frame_enforces_max_bytes() {
        assert!(matches!(
            decode_frame(b"{\"x\":123}\n", 3),
            Err(FrameError::FrameTooLarge { .. })
        ));
    }

    #[test]
    /// 队列满时必须暴露 backpressure，调用方才能稳定返回 QUEUE_FULL。
    fn bounded_queue_reports_backpressure() {
        let (inbound_sender, inbound_receiver) = bounded_channel(1);
        let (outbound_sender, outbound_receiver) = bounded_channel(1);
        inbound_sender
            .try_send(1_u8)
            .expect("first inbound item fits");
        outbound_sender
            .try_send(2_u8)
            .expect("first outbound item fits");
        assert_eq!(inbound_sender.try_send(3_u8), Err(QueueError::Full(3)));
        assert_eq!(outbound_sender.try_send(4_u8), Err(QueueError::Full(4)));
        assert_eq!(inbound_receiver.recv(), Some(1));
        assert_eq!(outbound_receiver.recv(), Some(2));
    }

    #[test]
    /// c/s 两个 namespace 同时存在时各自只恢复自己的 pending，防止嵌套 approval 串线。
    fn pending_is_bounded_and_response_is_exactly_once() {
        let mut pending = PendingRegistry::new(2);
        pending
            .register("s:approval-1")
            .expect("first pending fits");
        pending
            .register("c:version-1")
            .expect("second namespace fits");
        assert_eq!(
            pending.register("c:duplicate"),
            Err(PendingError::LimitReached)
        );
        assert_eq!(pending.resolve("s:approval-1"), ResolveOutcome::First);
        assert_eq!(pending.resolve("c:version-1"), ResolveOutcome::First);
        assert_eq!(
            pending.resolve("s:approval-1"),
            ResolveOutcome::DuplicateResponse
        );
        assert_eq!(pending.resolve("s:missing"), ResolveOutcome::UnknownRequest);
        assert_eq!(pending.len(), 0);
        assert_eq!(pending.tombstone_len(), 2);
    }

    #[test]
    /// terminal request 必须释放 active 容量，而 tombstone 只能保留有限近期记录。
    fn pending_releases_capacity_and_bounds_tombstones() {
        let mut pending = PendingRegistry::with_tombstone_limit(1, 2);
        for round in 0..32 {
            let request_id = format!("c:round-{round}");
            pending
                .register(&request_id)
                .expect("active slot is reusable");
            assert_eq!(pending.resolve(&request_id), ResolveOutcome::First);
            assert_eq!(pending.len(), 0);
            assert!(pending.tombstone_len() <= 2);
        }
        assert_eq!(pending.resolve("c:round-0"), ResolveOutcome::UnknownRequest);
        pending
            .register("c:round-0")
            .expect("evicted tombstone no longer consumes active capacity");
        assert_eq!(pending.resolve("c:round-0"), ResolveOutcome::First);
    }

    #[test]
    /// deadline/cancel 的终态不能被迟到 response 改写，保证副作用 fail-closed。
    fn deadline_and_cancel_make_late_responses_fail_closed() {
        let mut pending = PendingRegistry::new(4);
        pending.register("c:deadline").expect("deadline entry");
        assert!(pending.expire("c:deadline"));
        assert_eq!(pending.resolve("c:deadline"), ResolveOutcome::LateResponse);
        pending.register("s:cancel").expect("cancel entry");
        assert!(pending.cancel("s:cancel"));
        assert_eq!(pending.resolve("s:cancel"), ResolveOutcome::LateResponse);
    }

    #[test]
    /// live 缺口必须等待权威 snapshot，重复 event 则幂等丢弃。
    fn snapshot_live_reducer_requires_resync_for_gap_and_deduplicates() {
        let mut reducer = SequenceReducer::default();
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 1,
                event_id: "e1".into()
            }),
            SequenceOutcome::Applied
        );
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 3,
                event_id: "e3".into()
            }),
            SequenceOutcome::Gap {
                expected: 2,
                received: 3
            }
        );
        reducer.apply_snapshot(2);
        assert_eq!(reducer.drain_buffered(), SequenceOutcome::Applied);
        assert_eq!(reducer.last_seq(), 3);
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 3,
                event_id: "e3".into()
            }),
            SequenceOutcome::Duplicate
        );
    }

    #[test]
    /// BTreeMap 按 seq 排序后，4→3 的乱序可以在 1、2 到达后连续收口，不会重复 re-buffer。
    fn snapshot_live_reducer_orders_out_of_order_events() {
        let mut reducer = SequenceReducer::new(4);
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 4,
                event_id: "e4".into()
            }),
            SequenceOutcome::Gap {
                expected: 1,
                received: 4
            }
        );
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 3,
                event_id: "e3".into()
            }),
            SequenceOutcome::Gap {
                expected: 1,
                received: 3
            }
        );
        assert_eq!(reducer.buffered_len(), 2);
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 1,
                event_id: "e1".into()
            }),
            SequenceOutcome::Applied
        );
        assert_eq!(
            reducer.drain_buffered(),
            SequenceOutcome::Gap {
                expected: 2,
                received: 3
            }
        );
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 2,
                event_id: "e2".into()
            }),
            SequenceOutcome::Applied
        );
        assert_eq!(reducer.drain_buffered(), SequenceOutcome::Applied);
        assert_eq!(reducer.last_seq(), 4);
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 99,
                event_id: "e3".into()
            }),
            SequenceOutcome::Duplicate
        );
    }

    #[test]
    /// 乱序缓存满时必须进入 RESYNC_REQUIRED，而不是继续增长内存。
    fn snapshot_live_reducer_bounds_out_of_order_buffer() {
        let mut reducer = SequenceReducer::new(2);
        assert!(matches!(
            reducer.apply_live(SequencedEvent {
                seq: 4,
                event_id: "e4".into()
            }),
            SequenceOutcome::Gap { .. }
        ));
        assert!(matches!(
            reducer.apply_live(SequencedEvent {
                seq: 5,
                event_id: "e5".into()
            }),
            SequenceOutcome::Gap { .. }
        ));
        assert_eq!(
            reducer.apply_live(SequencedEvent {
                seq: 6,
                event_id: "e6".into()
            }),
            SequenceOutcome::ResyncRequired {
                expected: 1,
                received: 6
            }
        );
        assert_eq!(reducer.buffered_len(), 2);
    }

    #[test]
    /// 直接读取冻结 fixture 可以在跨语言实现前发现 envelope 字段猜测不一致。
    fn frozen_core_fixture_is_read_by_typed_envelope() {
        let frames = read_golden_core().expect("golden core should parse");
        assert!(frames.len() >= 8);
        assert_eq!(frames[0].id.as_deref(), Some("c:init-1"));
        assert_eq!(frames[0].method.as_deref(), Some("initialize"));
    }
}
