// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Rust host contract tests compile the not-yet-wired module through `#[path]`.
//!
//! The in-memory duplex pipe keeps concurrency deterministic while exercising the
//! same reader/writer/pending boundaries used by the real child process.

// The contract test compiles the not-yet-wired module tree in isolation; these
// re-exports and future composition-root methods are intentionally unused here.
#[expect(
    dead_code,
    unused_imports,
    reason = "the contract compiles the complete foundation module tree in isolation"
)]
#[path = "../src/agent_process/mod.rs"]
mod agent_process;

use agent_process::codec::{self, CodecError, Limits, RpcFrame};
use agent_process::lifecycle::{Clock, LifecycleMachine, LifecycleState, RestartPolicy};
use agent_process::pending::{PendingRegistry, ResolveDisposition};
use agent_process::session::{Session, SessionEvent, TerminalCallback, TerminalReason};
use agent_process::supervisor::{SidecarConfig, SidecarSupervisor};
use serde_json::json;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct PipeReader {
    receiver: Receiver<Vec<u8>>,
    buffer: VecDeque<u8>,
}

struct PipeWriter {
    sender: Sender<Vec<u8>>,
}

struct LateWatchdogReader {
    inner: PipeReader,
    frame_read: Sender<()>,
    release: Arc<AtomicBool>,
    first_read: bool,
}

/// 构造一对不会自动关闭的内存管道，让测试能独立控制 EOF 时机。
fn pipe_pair() -> (PipeReader, PipeWriter) {
    let (sender, receiver) = mpsc::channel();
    (
        PipeReader {
            receiver,
            buffer: VecDeque::new(),
        },
        PipeWriter { sender },
    )
}

impl Read for PipeReader {
    /// 阻塞到一段完整的内存管道数据可用，模拟子进程的 pipe read 语义。
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        while self.buffer.is_empty() {
            let Ok(chunk) = self.receiver.recv() else {
                // A closed in-memory sender models an OS pipe returning zero,
                // so the reader exercises the protocol EOF branch directly.
                return Ok(0);
            };
            self.buffer.extend(chunk);
        }
        let count = target.len().min(self.buffer.len());
        for slot in &mut target[..count] {
            *slot = self.buffer.pop_front().expect("buffer length checked");
        }
        Ok(count)
    }
}

impl Read for LateWatchdogReader {
    /// Let the ready frame reach the production dispatcher before holding EOF,
    /// proving the reader is waiting on the barrier rather than missing input.
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if self.first_read {
            self.first_read = false;
            let count = self.inner.read(target)?;
            self.frame_read
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "frame ack closed"))?;
            return Ok(count);
        }
        while !self.release.load(Ordering::Acquire) {
            thread::yield_now();
        }
        Ok(0)
    }
}

impl Write for PipeWriter {
    /// 把每次 host 写入复制到 channel，便于测试逐帧检查 writer actor 输出。
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.sender
            .send(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))?;
        Ok(bytes.len())
    }

    /// 内存管道不需要 flush，但保留 Write 合约以覆盖真实 writer 调用路径。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct EmptyReader;

impl Read for EmptyReader {
    /// 立即 EOF，模拟 sidecar pipe 已关闭的边界。
    fn read(&mut self, _target: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

struct ControlledReader {
    release: Arc<AtomicBool>,
}

impl Read for ControlledReader {
    /// Keep stdout open until terminal cleanup so the watchdog test owns no
    /// permanently blocked reader thread after the callback fires.
    fn read(&mut self, _target: &mut [u8]) -> io::Result<usize> {
        while !self.release.load(Ordering::Acquire) {
            thread::yield_now();
        }
        Ok(0)
    }
}

struct BlockingWriter {
    entered: Sender<()>,
    release: Arc<AtomicBool>,
    finished: Sender<()>,
}

impl Write for BlockingWriter {
    /// Simulate an OS write blocked behind a child that stopped reading stdin.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.entered
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "watchdog entry closed"))?;
        while !self.release.load(Ordering::Acquire) {
            thread::yield_now();
        }
        self.finished
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "watchdog finish closed"))?;
        Ok(bytes.len())
    }

    /// The fixture blocks in write, so flush is reached only after cancellation.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct AckWriter {
    writes: Sender<()>,
}

struct GateWriter {
    entered: Sender<()>,
    release: Receiver<()>,
    completed: Sender<()>,
    blocked: bool,
}

impl Write for GateWriter {
    /// Pause the first frame after enqueue so the test can deliver an immediate
    /// ready token before writer confirmation without using wall-clock sleeps.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.blocked {
            self.blocked = true;
            self.entered
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "gate closed"))?;
            self.release
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "gate released"))?;
        }
        Ok(bytes.len())
    }

    /// The gated fixture has no buffering and therefore needs no extra flush work.
    fn flush(&mut self) -> io::Result<()> {
        self.completed
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "completion closed"))?;
        Ok(())
    }
}

struct ImmediateReadyWriter {
    ready_sender: Sender<Vec<u8>>,
    ready_payload: Option<Vec<u8>>,
    ready_sent: Sender<()>,
    release_flush: Receiver<()>,
    flush_done: Sender<()>,
}

struct ReadyBeforeWriteReturnWriter {
    ready_sender: Sender<Vec<u8>>,
    ready_payload: Option<Vec<u8>>,
    ready_sent: Sender<()>,
    release_write: Receiver<()>,
    write_done: Sender<()>,
    flush_done: Sender<()>,
}

impl Write for ReadyBeforeWriteReturnWriter {
    /// Send ready before write returns so the reader must wait on the shared
    /// barrier instead of observing an intermediate atomic publication.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(ready) = self.ready_payload.take() {
            self.ready_sender
                .send(ready)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready pipe closed"))?;
            self.ready_sent
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready ack closed"))?;
            self.release_write
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write release timeout"))?;
        }
        self.write_done
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "write ack closed"))?;
        Ok(bytes.len())
    }

    /// Keep the success path explicit so the barrier is committed only after
    /// the complete write plus flush operation has returned.
    fn flush(&mut self) -> io::Result<()> {
        self.flush_done
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "flush ack closed"))?;
        Ok(())
    }
}

struct FlushFailureWriter {
    ready_sender: Sender<Vec<u8>>,
    ready_payload: Option<Vec<u8>>,
    ready_sent: Sender<()>,
    release_flush: Receiver<()>,
    flush_done: Sender<()>,
}

struct LateWatchdogReadyWriter {
    ready_sender: Sender<Vec<u8>>,
    ready_payload: Option<Vec<u8>>,
    ready_sent: Sender<()>,
    release_write: Arc<AtomicBool>,
    write_done: Sender<()>,
}

impl Write for LateWatchdogReadyWriter {
    /// Publish ready, then finish write only after the watchdog has closed the
    /// session, reproducing a late OS write completion without a second writer.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(ready) = self.ready_payload.take() {
            self.ready_sender
                .send(ready)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready pipe closed"))?;
            self.ready_sent
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready ack closed"))?;
            while !self.release_write.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        self.write_done
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "write ack closed"))?;
        Ok(bytes.len())
    }

    /// The late completion fixture reaches flush only after watchdog release.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for FlushFailureWriter {
    /// Complete write successfully, leaving flush as the only failure point
    /// so the test proves the barrier never remains Confirmed after flush.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    /// Race a ready reply with a blocked flush, then fail closed after release.
    fn flush(&mut self) -> io::Result<()> {
        if let Some(ready) = self.ready_payload.take() {
            self.ready_sender
                .send(ready)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready pipe closed"))?;
            self.ready_sent
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready ack closed"))?;
            self.release_flush
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "flush release timeout"))?;
        }
        self.flush_done
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "flush ack closed"))?;
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "flush fixture failure",
        ))
    }
}

impl Write for ImmediateReadyWriter {
    /// Publish ready from flush while the writer actor is still inside the
    /// operation, reproducing a child that replies as soon as initialized is
    /// observable without using sleeps or a second protocol implementation.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    /// Hold the writer after publishing ready so the test proves the barrier
    /// is visible before flush returns and before lifecycle promotion runs.
    fn flush(&mut self) -> io::Result<()> {
        if let Some(ready) = self.ready_payload.take() {
            self.ready_sender
                .send(ready)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready pipe closed"))?;
            self.ready_sent
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ready ack closed"))?;
            self.release_flush
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "flush release timeout"))?;
        }
        self.flush_done
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "flush ack closed"))?;
        Ok(())
    }
}

impl Write for AckWriter {
    /// 每收到一帧就发确认，使 pending 注册完成后测试无需 sleep 猜测时序。
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes
            .send(())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ack receiver closed"))?;
        Ok(bytes.len())
    }

    /// Ack writer 没有缓冲，flush 只需保持成功。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailingWriter;

impl Write for FailingWriter {
    /// 强制 writer actor 走 IO fault 分支，回归 fail-closed 语义。
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
    }

    /// 即使 write 未发生，flush 也保持同一失败模型。
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture failure"))
    }
}

#[test]
fn codec_rejects_partial_duplicate_and_invalid_namespace() {
    assert_eq!(
        codec::decode_frame(br#"{"jsonrpc":"2.0","id":"c:a"}"#, 1024),
        Err(CodecError::PartialFrame)
    );
    assert_eq!(
        codec::decode_frame(
            br#"{"jsonrpc":"2.0","id":"c:a","id":"c:b","result":null}
"#,
            1024
        ),
        Err(CodecError::DuplicateKey)
    );
    assert_eq!(
        codec::decode_frame(
            br#"{"jsonrpc":"2.0","id":"c:a","result":{"nested":{"x":1,"x":2}}}
"#,
            1024
        ),
        Err(CodecError::DuplicateKey)
    );
    assert_eq!(
        codec::decode_frame(
            br#"{"jsonrpc":"2.0","id":"c:.bad","result":null}
"#,
            1024
        ),
        Err(CodecError::InvalidId)
    );
    assert!(RpcFrame::client_request("s:wrong", "x", json!({})).is_err());
    assert!(RpcFrame::server_request("c:wrong", "x", json!({})).is_err());
    assert!(RpcFrame::client_request("c:.bad", "x", json!({})).is_err());
    assert_eq!(codec::negotiate_version(1, 1, 0, 1, 7, 0), Ok(1));
    assert_eq!(
        codec::negotiate_version(1, 1, 0, 2, 0, 0),
        Err(CodecError::InvalidEnvelope)
    );
    assert_eq!(
        codec::negotiate_version(1, 1, 2, 1, 1, 0),
        Err(CodecError::InvalidEnvelope)
    );
}

#[test]
fn codec_preserves_null_result_and_validates_hand_built_error() {
    let frame = codec::decode_frame(
        br#"{"jsonrpc":"2.0","id":"c:null","result":null}
"#,
        1024,
    )
    .expect("null result is valid");
    assert!(frame.result().is_present());
    assert_eq!(frame.result().value(), Some(&serde_json::Value::Null));
    assert!(RpcFrame::response_error("c:bad", -1, "x", "bad-code", false).is_err());
    assert!(RpcFrame::response_error("c:bad", -32_001, "x", "A", false).is_err());
}

/// 构造 frozen error fixture，证明 code/jaCode/retryable 决定分类而 message
/// 可以是 bounded 本地化文案，同时拒绝空、超长、路径和敏感诊断。
#[test]
fn codec_accepts_localized_catalog_errors_and_rejects_unsafe_messages() {
    fn fixture(message: &str, retryable: bool) -> Vec<u8> {
        let value = json!({
            "jsonrpc":"2.0",
            "id":"c:localized",
            "error":{
                "code":-32020,
                "message":message,
                "data":{"jaCode":"SHUTTING_DOWN","retryable":retryable}
            }
        });
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    let localized = codec::decode_frame(&fixture("正在关闭", true), 4096)
        .expect("bounded localized message is valid");
    assert_eq!(localized.error().unwrap().message(), "正在关闭");
    assert!(
        RpcFrame::response_error("c:localized", -32_020, "正在关闭", "SHUTTING_DOWN", true,)
            .is_ok()
    );

    let invalid_messages = [
        "",
        &"x".repeat(513),
        "0123456789abcdef0123456789abcdef",
        r"C:\Users\24052\prompt.txt",
        "failed: /Users/24052/project",
        "/etc/passwd",
        "prefix HTTPS://example.test/query?x=%2FUsers%2Fprivate suffix",
        "file:///Users/24052/private.txt",
        "custom+scheme://example.test/resource",
        "api_key=sk-test-secret",
        "API KEY=sk-test-secret",
        "api-key: sk-test-secret",
        "Api_Key sk-test-secret",
    ];
    for message in invalid_messages {
        assert_eq!(
            codec::decode_frame(&fixture(message, true), 4096),
            Err(CodecError::InvalidEnvelope),
            "unsafe error message must fail closed: {message:?}"
        );
    }
    assert_eq!(
        codec::decode_frame(&fixture("正在关闭", false), 4096),
        Err(CodecError::InvalidErrorCatalog)
    );
    assert!(codec::decode_frame(&fixture("失败/重试", true), 4096).is_ok());
}

/// 锁定 readyToken 的 schema 形状与递归拒绝边界，防止伪 ready 进入 supervisor。
#[test]
fn ready_token_codec_rejects_missing_malformed_and_nested_markers() {
    let missing_initialized = br#"{"jsonrpc":"2.0","method":"initialized","params":{}}
"#;
    assert_eq!(
        codec::decode_frame(missing_initialized, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let malformed_ready = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready","readyToken":"short"}}
"#;
    assert_eq!(
        codec::decode_frame(malformed_ready, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let nested_ready = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready","readyToken":"0123456789abcdef0123456789abcdef","details":{"readyToken":"0123456789abcdef0123456789abcdef"}}}
"#;
    assert_eq!(
        codec::decode_frame(nested_ready, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let non_ready_token = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"starting","readyToken":"0123456789abcdef0123456789abcdef"}}
"#;
    assert_eq!(
        codec::decode_frame(non_ready_token, 4096),
        Err(CodecError::InvalidEnvelope)
    );
    let root_token = br#"{"jsonrpc":"2.0","id":"c:result","result":{},"meta":{"readyToken":"0123456789abcdef0123456789abcdef"}}
"#;
    assert_eq!(
        codec::decode_frame(root_token, 4096),
        Err(CodecError::InvalidEnvelope)
    );
    let uppercase_token = br#"{"jsonrpc":"2.0","method":"initialized","params":{"readyToken":"0123456789ABCDEF0123456789ABCDEF"}}
"#;
    assert_eq!(
        codec::decode_frame(uppercase_token, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let ready_nested_value = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready","readyToken":"0123456789abcdef0123456789abcdef","details":{"value":"fedcba9876543210fedcba9876543210"}}}
"#;
    assert_eq!(
        codec::decode_frame(ready_nested_value, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let ready_token_key = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready","readyToken":"0123456789abcdef0123456789abcdef","details":{"0123456789abcdef0123456789abcdef":true}}}
"#;
    assert_eq!(
        codec::decode_frame(ready_token_key, 4096),
        Err(CodecError::HandshakeFailed)
    );
    let ready_root_token = br#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready","readyToken":"0123456789abcdef0123456789abcdef"},"meta":{"old":"fedcba9876543210fedcba9876543210"}}
"#;
    assert_eq!(
        codec::decode_frame(ready_root_token, 4096),
        Err(CodecError::HandshakeFailed)
    );
}

/// 用相同的 hostile corpus 覆盖所有 envelope 分支，防止大小写变体绕过出入站 guard。
#[test]
fn token_marker_policy_is_case_insensitive_and_table_driven() {
    let token_variants = [
        "0123456789abcdef0123456789abcdef",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789aBcDeF0123456789aBcDeF",
    ];
    for token in token_variants {
        let nested = json!({"outer":[{"inner":{"value":token}}]});
        assert!(
            RpcFrame::notification("future/method", nested.clone()).is_err(),
            "notification must reject {token:?}"
        );
        assert!(
            RpcFrame::client_request("c:token", "future/method", nested.clone()).is_err(),
            "request must reject {token:?}"
        );
        assert!(
            RpcFrame::response_result("c:token", nested.clone()).is_err(),
            "result must reject {token:?}"
        );

        let inbound = serde_json::json!({
            "jsonrpc":"2.0",
            "method":"future/method",
            "params":nested
        });
        let mut inbound_bytes = serde_json::to_vec(&inbound).expect("serialize fixture");
        inbound_bytes.push(b'\n');
        assert_eq!(
            codec::decode_frame(&inbound_bytes, 4096),
            Err(CodecError::InvalidEnvelope),
            "inbound notification must reject {token:?}"
        );

        let error = serde_json::json!({
            "jsonrpc":"2.0",
            "id":"c:token",
            "error":{
                "code":-32020,
                "message":"shutting down",
                "data":{
                    "jaCode":"SHUTTING_DOWN",
                    "retryable":true,
                    "details":{"deep":[{"value":token}]}
                }
            }
        });
        let mut error_bytes = serde_json::to_vec(&error).expect("serialize error fixture");
        error_bytes.push(b'\n');
        assert_eq!(
            codec::decode_frame(&error_bytes, 4096),
            Err(CodecError::InvalidEnvelope),
            "error projection must not erase {token:?}"
        );
        assert!(
            RpcFrame::response_error("c:token", -32020, token, "SHUTTING_DOWN", true).is_err(),
            "error message must reject {token:?}"
        );
    }

    for key in ["readyToken", "READYTOKEN", "readyTOKEN"] {
        let mut object = serde_json::Map::new();
        object.insert(key.to_owned(), json!("ordinary value"));
        let payload = json!({"outer":[serde_json::Value::Object(object)]});
        assert!(
            RpcFrame::notification("future/method", payload.clone()).is_err(),
            "marker key variant must reject {key:?}"
        );
        let inbound = serde_json::json!({
            "jsonrpc":"2.0",
            "method":"future/method",
            "params":payload
        });
        let mut inbound_bytes = serde_json::to_vec(&inbound).expect("serialize key fixture");
        inbound_bytes.push(b'\n');
        assert_eq!(
            codec::decode_frame(&inbound_bytes, 4096),
            Err(CodecError::InvalidEnvelope),
            "inbound marker key variant must reject {key:?}"
        );
    }

    for safe in [
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "普通中文描述，不是 token",
    ] {
        let frame = RpcFrame::notification("future/method", json!({"value":safe}))
            .expect("non-token-shaped text remains valid");
        let encoded = frame.encode(4096).expect("safe outbound frame");
        assert!(codec::decode_frame(&encoded, 4096).is_ok());
    }

    let escaped_value = br#"{"jsonrpc":"2.0","method":"future/method","params":{"value":"0123456789abcdef0123456789abcde\u0046"}}
"#;
    assert_eq!(
        codec::decode_frame(escaped_value, 4096),
        Err(CodecError::InvalidEnvelope)
    );
    let escaped_key =
        br#"{"jsonrpc":"2.0","method":"future/method","params":{"\u0052EADYTOKEN":"safe"}}
"#;
    assert_eq!(
        codec::decode_frame(escaped_key, 4096),
        Err(CodecError::InvalidEnvelope)
    );
}

#[test]
fn inbound_error_data_is_rebuilt_as_safe_projection() {
    let frame = codec::decode_frame(
        br#"{"jsonrpc":"2.0","id":"c:error","error":{"code":-32020,"message":"shutting down","data":{"jaCode":"SHUTTING_DOWN","retryable":true,"details":"api-key=secret","path":"C:\\Users\\24052\\prompt.txt","prompt":"source text","unknown":{"token":"bearer"}}}}
"#,
        4096,
    )
    .expect("catalog error is valid");
    let debug = format!("{frame:?}");
    let error = frame.error().expect("error projection");
    assert_eq!(
        error.data(),
        &json!({"jaCode":"SHUTTING_DOWN","retryable":true})
    );
    assert!(!debug.contains("api-key"));
    assert!(!debug.contains("prompt.txt"));
    assert!(!debug.contains("bearer"));

    // Projection must not be allowed to erase a raw challenge marker before
    // the codec audits the complete error object.  This is the regression that
    // protects error.detail from becoming a token exfiltration side channel.
    let nested_ready_token = br#"{"jsonrpc":"2.0","id":"c:error-token","error":{"code":-32020,"message":"shutting down","data":{"jaCode":"SHUTTING_DOWN","retryable":true,"details":{"readyToken":"0123456789abcdef0123456789abcdef"}}}}
"#;
    assert_eq!(
        codec::decode_frame(nested_ready_token, 4096),
        Err(CodecError::InvalidEnvelope)
    );
}

/// 构造已 flush initialized 的 session，让 terminal/ready promotion race 可以
/// 只由显式 pipe 事件驱动，而不是依赖线程 sleep 猜测时序。
fn ready_promotion_fixture(
    generation: u64,
) -> (
    Session,
    PipeWriter,
    agent_process::session::EventPump,
    RpcFrame,
) {
    let (server_reader, server_writer) = pipe_pair();
    let token = "fedcba9876543210fedcba9876543210";
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_gate",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap();
    let (ack_sender, ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        generation,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    let pump = session.take_event_pump().unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    ack_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("initialized frame must reach writer");
    assert!(session.wait_initialized_confirmation(Duration::from_secs(1)));
    (session, server_writer, pump, ready)
}

/// 证明 duplicate ready 在首帧出队后仍先赢 terminal gate，lifecycle closure 不会执行。
#[test]
fn ready_promotion_gate_rejects_duplicate_before_lifecycle_mark() {
    let (session, server_writer, mut pump, ready) = ready_promotion_fixture(36);
    let ready_bytes = ready.encode(Limits::default().max_frame_bytes).unwrap();
    server_writer.sender.send(ready_bytes.clone()).unwrap();
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("first ready notification")
    else {
        panic!("expected first ready notification");
    };
    server_writer.sender.send(ready_bytes).unwrap();
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::HandshakeFailed)
    ));

    let mut lifecycle = LifecycleMachine::new(RestartPolicy::default()).unwrap();
    let generation = lifecycle.begin_start().unwrap();
    let mut promoted = false;
    assert_eq!(
        session.with_ready_promotion(&frame, || {
            promoted = true;
            lifecycle.mark_ready(generation)
        }),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    assert!(!promoted);
    assert_ne!(lifecycle.state(), LifecycleState::Ready);
    session.close();
}

/// 证明 monitor fault 在 ready 出队后也赢得同一 gate，避免 supervisor 误标 Ready。
#[test]
fn ready_promotion_gate_rejects_monitor_fault_before_lifecycle_mark() {
    let (session, server_writer, mut pump, ready) = ready_promotion_fixture(37);
    server_writer
        .sender
        .send(ready.encode(Limits::default().max_frame_bytes).unwrap())
        .unwrap();
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("first ready notification")
    else {
        panic!("expected first ready notification");
    };
    session.report_process_fault();
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::ProtocolFault(codec::CodecError::Io))
    ));

    let mut lifecycle = LifecycleMachine::new(RestartPolicy::default()).unwrap();
    let generation = lifecycle.begin_start().unwrap();
    let mut promoted = false;
    assert_eq!(
        session.with_ready_promotion(&frame, || {
            promoted = true;
            lifecycle.mark_ready(generation)
        }),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    assert!(!promoted);
    assert_ne!(lifecycle.state(), LifecycleState::Ready);
    session.close();
}

/// 证明 stdout EOF 在 ready 出队后仍阻止 lifecycle mark_ready，且 session 已关闭。
#[test]
fn ready_promotion_gate_rejects_eof_before_lifecycle_mark() {
    let (session, server_writer, mut pump, ready) = ready_promotion_fixture(38);
    server_writer
        .sender
        .send(ready.encode(Limits::default().max_frame_bytes).unwrap())
        .unwrap();
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("first ready notification")
    else {
        panic!("expected first ready notification");
    };
    drop(server_writer);
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::Eof)
    ));

    let mut lifecycle = LifecycleMachine::new(RestartPolicy::default()).unwrap();
    let generation = lifecycle.begin_start().unwrap();
    let mut promoted = false;
    assert_eq!(
        session.with_ready_promotion(&frame, || {
            promoted = true;
            lifecycle.mark_ready(generation)
        }),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    assert!(!promoted);
    assert_ne!(lifecycle.state(), LifecycleState::Ready);
    session.close();
}

#[test]
fn prequeued_ready_is_rejected_by_ready_token_barrier() {
    let (server_reader, server_writer) = pipe_pair();
    let token = "0123456789abcdef0123456789abcdef";
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_ready",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap()
    .encode(Limits::default().max_frame_bytes)
    .unwrap();
    let _server_writer = server_writer;
    let (ack_sender, _ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        31,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    _server_writer.sender.send(ready).unwrap();
    let mut pump = session.take_event_pump().unwrap();
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::HandshakeFailed)
    ));
    assert_eq!(
        session.notify("initialized", json!({"readyToken":token})),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    session.close();
}

#[test]
fn ready_during_initialized_write_waits_for_successful_barrier() {
    let (server_reader, server_writer) = pipe_pair();
    let token = "fedcba9876543210fedcba9876543210";
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_preflush",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap();
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (completed_sender, completed_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        GateWriter {
            entered: entered_sender,
            release: release_receiver,
            completed: completed_sender,
            blocked: false,
        },
        EmptyReader,
        32,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    let mut pump = session.take_event_pump().unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    entered_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("writer entered initialized frame");
    server_writer
        .sender
        .send(ready.encode(Limits::default().max_frame_bytes).unwrap())
        .unwrap();
    assert!(pump.next_event(Duration::from_millis(50)).is_none());
    release_sender.send(()).unwrap();
    completed_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("initialized flush completed");
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("ready after initialized barrier")
    else {
        panic!("expected ready notification after successful barrier");
    };
    assert!(session.ready_after_initialized_barrier(&frame));
    session.close();
    session
        .join_writer_until(Instant::now() + Duration::from_secs(1))
        .unwrap();
}

#[test]
fn immediate_ready_after_initialized_flush_accepts_once_and_duplicate_fails() {
    let (server_reader, server_writer) = pipe_pair();
    let token = "fedcba9876543210fedcba9876543210";
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_immediate",
            "occurredAt":"2000-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap();
    let (ack_sender, ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        32,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    let mut pump = session.take_event_pump().unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    ack_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("initialized flush completed");
    assert!(session.wait_initialized_confirmation(Duration::from_secs(1)));
    let ready_bytes = ready.encode(Limits::default().max_frame_bytes).unwrap();
    server_writer.sender.send(ready_bytes.clone()).unwrap();
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("prequeued ready")
    else {
        panic!("expected ready notification");
    };
    assert!(session.ready_after_initialized_barrier(&frame));
    server_writer.sender.send(ready_bytes).unwrap();
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::HandshakeFailed)
    ));
    session.close();
}

/// Run one writer-barrier round with explicit cleanup so a failed assertion
/// cannot leave the blocked writer actor detached from the test process.
fn run_immediate_ready_barrier_round(round: u64) -> Result<(), String> {
    let (server_reader, server_writer) = pipe_pair();
    let token = if round.is_multiple_of(2) {
        "fedcba9876543210fedcba9876543210"
    } else {
        "0123456789abcdef0123456789abcdef"
    };
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":format!("evt_barrier_{round}"),
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .map_err(|error| format!("ready frame construction: {error}"))?
    .encode(Limits::default().max_frame_bytes)
    .map_err(|error| format!("ready frame encoding: {error}"))?;
    let (ready_sent_sender, ready_sent_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (flush_done_sender, flush_done_receiver) = mpsc::channel();
    let writer = ImmediateReadyWriter {
        ready_sender: server_writer.sender.clone(),
        ready_payload: Some(ready),
        ready_sent: ready_sent_sender,
        release_flush: release_receiver,
        flush_done: flush_done_sender,
    };
    let session = Session::from_io(
        server_reader,
        writer,
        EmptyReader,
        round + 100,
        Limits::default(),
    )
    .map_err(|error| format!("session construction: {error}"))?;
    session
        .install_ready_token_challenge(token.to_owned())
        .map_err(|error| format!("challenge install: {error}"))?;
    let mut pump = session
        .take_event_pump()
        .map_err(|error| format!("event pump: {error}"))?;
    let notify_result = session.notify("initialized", json!({"readyToken":token}));
    let ready_sent_result = ready_sent_receiver.recv_timeout(Duration::from_secs(1));
    let release_result = release_sender.send(());
    let flush_result = flush_done_receiver.recv_timeout(Duration::from_secs(1));
    let event = pump.next_event(Duration::from_secs(1));
    let promotion_result = match &event {
        Some(SessionEvent::Notification(frame)) => session
            .with_ready_promotion(frame, || Ok(()))
            .map_err(|error| format!("ready promotion: {error}")),
        _ => Err("immediate ready was rejected before barrier publication".to_owned()),
    };

    // Release the writer before close so even a failed event assertion owns a
    // bounded path to join; this is the same cleanup boundary as production.
    session.close();
    let join_result = session.join_writer_until(Instant::now() + Duration::from_secs(1));

    notify_result.map_err(|error| format!("initialized notify: {error}"))?;
    ready_sent_result.map_err(|error| format!("ready publication: {error}"))?;
    release_result.map_err(|error| format!("flush release: {error}"))?;
    flush_result.map_err(|error| format!("flush completion: {error}"))?;
    join_result.map_err(|error| format!("writer join: {error}"))?;
    promotion_result
}

/// Repeat the production Session reader/writer barrier path without sleeps so
/// the immediate child reply race remains covered across scheduler interleavings.
#[test]
fn immediate_ready_writer_barrier_is_stable_for_100_rounds() {
    for round in 0..100 {
        run_immediate_ready_barrier_round(round)
            .unwrap_or_else(|error| panic!("writer barrier round {round} failed: {error}"));
    }
}

/// 证明 ready 在 write 返回前到达时会等待同一把门，成功 flush 后才能进入事件队列。
#[test]
fn ready_before_write_return_waits_then_promotes() {
    let (server_reader, server_writer) = pipe_pair();
    let token = "0123456789abcdef0123456789abcdef";
    let ready = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_write_window",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap()
    .encode(Limits::default().max_frame_bytes)
    .unwrap();
    let (ready_sent_sender, ready_sent_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (write_done_sender, write_done_receiver) = mpsc::channel();
    let (flush_done_sender, flush_done_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        ReadyBeforeWriteReturnWriter {
            ready_sender: server_writer.sender.clone(),
            ready_payload: Some(ready),
            ready_sent: ready_sent_sender,
            release_write: release_receiver,
            write_done: write_done_sender,
            flush_done: flush_done_sender,
        },
        EmptyReader,
        140,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    let mut pump = session.take_event_pump().unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    ready_sent_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("ready sent before write returned");
    assert!(pump.next_event(Duration::from_millis(50)).is_none());
    release_sender.send(()).unwrap();
    write_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("write returned");
    flush_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("flush completed");
    assert!(session.wait_initialized_confirmation(Duration::from_secs(1)));
    let SessionEvent::Notification(frame) = pump
        .next_event(Duration::from_secs(1))
        .expect("ready after write barrier")
    else {
        panic!("expected ready notification after write barrier");
    };
    assert!(session.ready_after_initialized_barrier(&frame));
    session.close();
    session
        .join_writer_until(Instant::now() + Duration::from_secs(1))
        .unwrap();
}

/// 证明 flush 失败会把共享门置为 Failed，ready 既不能晋级也不能留下 confirmation。
fn run_flush_failure_barrier_round(round: u64) -> Result<(), String> {
    let (server_reader, server_writer) = pipe_pair();
    let token = "fedcba9876543210fedcba9876543210";
    let ready_frame = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_flush_failure",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap();
    let ready = ready_frame
        .encode(Limits::default().max_frame_bytes)
        .unwrap();
    let (ready_sent_sender, ready_sent_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (flush_done_sender, flush_done_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        FlushFailureWriter {
            ready_sender: server_writer.sender.clone(),
            ready_payload: Some(ready),
            ready_sent: ready_sent_sender,
            release_flush: release_receiver,
            flush_done: flush_done_sender,
        },
        EmptyReader,
        round + 141,
        Limits::default(),
    )
    .map_err(|error| format!("session construction failed: {error}"))?;

    let result = (|| {
        session
            .install_ready_token_challenge(token.to_owned())
            .map_err(|error| format!("challenge install failed: {error}"))?;
        let mut pump = session
            .take_event_pump()
            .map_err(|error| format!("event pump failed: {error}"))?;
        session
            .notify("initialized", json!({"readyToken":token}))
            .map_err(|error| format!("initialized notify failed: {error}"))?;
        ready_sent_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "ready was not sent during blocked flush".to_owned())?;
        if pump.next_event(Duration::from_millis(50)).is_some() {
            return Err("ready escaped while flush still held the barrier".to_owned());
        }
        release_sender
            .send(())
            .map_err(|_| "flush release failed".to_owned())?;
        flush_done_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "flush did not reach its failure boundary".to_owned())?;

        // Writer and reader publish independent terminal facts.  Their queue
        // order is scheduler-dependent and is not a public ordering contract;
        // accept either allowed fact, but never an unknown/empty outcome.
        let event = pump
            .next_event(Duration::from_secs(1))
            .ok_or_else(|| "terminal event queue returned empty".to_owned())?;
        match event {
            SessionEvent::HandshakeFailed | SessionEvent::ProtocolFault(CodecError::Io) => {}
            SessionEvent::Notification(_) => {
                return Err("ready notification escaped after flush failure".to_owned());
            }
            _ => return Err("unexpected terminal event classification".to_owned()),
        }
        if session.wait_initialized_confirmation(Duration::from_millis(50)) {
            return Err("failed flush left initialized confirmation set".to_owned());
        }
        if session.ready_after_initialized_barrier(&ready_frame) {
            return Err("failed flush allowed ready promotion".to_owned());
        }
        Ok(())
    })();

    // Always release the fixture before joining so assertion failures cannot
    // leave the deliberately blocked writer outside the test deadline.
    let _ = release_sender.send(());
    session.close();
    let cleanup = session
        .join_writer_until(Instant::now() + Duration::from_secs(1))
        .map_err(|error| format!("writer cleanup failed: {error}"));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
    }
}

/// Flush-failure ordering is intentionally repeated to prove the allowed
/// terminal-fact race does not reintroduce a ready promotion flake.
#[test]
fn ready_during_flush_failure_fails_closed_without_confirmation() {
    for round in 0..100 {
        run_flush_failure_barrier_round(round)
            .unwrap_or_else(|error| panic!("flush failure round {round} failed: {error}"));
    }
}

/// 锁定错误 token 会直接终止当前 generation，而不是拖到 ready deadline。
#[test]
fn ready_token_wrong_value_is_handshake_failure() {
    let (server_reader, server_writer) = pipe_pair();
    let token = "0123456789abcdef0123456789abcdef";
    let (ack_sender, ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        34,
        Limits::default(),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    assert!(session.wait_initialized_confirmation(Duration::from_secs(1)));
    let wrong = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_wrong",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":"fedcba9876543210fedcba9876543210"
        }),
    )
    .unwrap();
    server_writer
        .sender
        .send(wrong.encode(Limits::default().max_frame_bytes).unwrap())
        .unwrap();
    let mut pump = session.take_event_pump().unwrap();
    assert!(matches!(
        pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::HandshakeFailed)
    ));
    assert_eq!(
        session.notify("runtime/statusChanged", json!({})),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    session.close();
}

#[test]
fn ready_token_install_rejects_uppercase_shape() {
    let (server_reader, _server_writer) = pipe_pair();
    let (ack_sender, _ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        35,
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        session.install_ready_token_challenge("0123456789ABCDEF0123456789ABCDEF".to_owned()),
        Err(agent_process::AgentProcessError::HandshakeFailed)
    );
    session.close();
}

#[test]
fn blocked_writer_watchdog_fails_closed_and_unblocks_operation() {
    let reader_release = Arc::new(AtomicBool::new(false));
    let writer_release = Arc::new(AtomicBool::new(false));
    let (entered_sender, entered_receiver) = mpsc::channel();
    let (finished_sender, finished_receiver) = mpsc::channel();
    let (terminal_sender, terminal_receiver) = mpsc::channel();
    let reader = ControlledReader {
        release: Arc::clone(&reader_release),
    };
    let writer = BlockingWriter {
        entered: entered_sender,
        release: Arc::clone(&writer_release),
        finished: finished_sender,
    };
    let callback_release = Arc::clone(&writer_release);
    let callback_reader = Arc::clone(&reader_release);
    let callback: TerminalCallback = Arc::new(move |reason: TerminalReason| {
        callback_release.store(true, Ordering::Release);
        callback_reader.store(true, Ordering::Release);
        let _ = terminal_sender.send(reason);
    });
    let session = Session::from_io_with_terminal_watchdog(
        reader,
        writer,
        EmptyReader,
        33,
        Limits::default(),
        Some(callback),
        Duration::from_millis(40),
    )
    .unwrap();
    session.notify("data/blocked", json!({})).unwrap();
    entered_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("writer operation entered");
    let watchdog_started = Instant::now();
    session.notify("shutdown", json!({})).unwrap();
    assert_eq!(
        terminal_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("watchdog terminal callback"),
        TerminalReason::Fault
    );
    assert!(
        watchdog_started.elapsed() < Duration::from_millis(500),
        "terminal callback exceeded bounded watchdog envelope"
    );
    finished_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("callback must unblock the blocked writer");
    assert_eq!(
        session.request("after/watchdog", json!({}), Duration::from_millis(20)),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    session.close();
    session
        .join_writer_until(Instant::now() + Duration::from_secs(1))
        .expect("fake writer actor must be joined after callback cancellation");
}

/// A watchdog timeout remains terminal even when write completes afterward;
/// ready must stay behind the failed barrier and cleanup must still join.
#[test]
fn late_write_after_watchdog_timeout_cannot_promote_ready() {
    let reader_release = Arc::new(AtomicBool::new(false));
    let writer_release = Arc::new(AtomicBool::new(false));
    let (server_reader, server_writer) = pipe_pair();
    let token = "0123456789abcdef0123456789abcdef";
    let ready_frame = RpcFrame::notification(
        "runtime/statusChanged",
        json!({
            "serverInstanceId":"srv_fixture",
            "eventId":"evt_late_watchdog",
            "occurredAt":"2099-01-01T00:00:00Z",
            "status":"ready",
            "readyToken":token
        }),
    )
    .unwrap();
    let ready = ready_frame
        .encode(Limits::default().max_frame_bytes)
        .unwrap();
    let (ready_sent_sender, ready_sent_receiver) = mpsc::channel();
    let (frame_read_sender, frame_read_receiver) = mpsc::channel();
    let (write_done_sender, write_done_receiver) = mpsc::channel();
    let (terminal_sender, terminal_receiver) = mpsc::channel();
    let writer_release_for_callback = Arc::clone(&writer_release);
    let reader_release_for_callback = Arc::clone(&reader_release);
    let callback: TerminalCallback = Arc::new(move |reason: TerminalReason| {
        writer_release_for_callback.store(true, Ordering::Release);
        reader_release_for_callback.store(true, Ordering::Release);
        let _ = terminal_sender.send(reason);
    });
    let session = Session::from_io_with_terminal_watchdog(
        LateWatchdogReader {
            inner: server_reader,
            frame_read: frame_read_sender,
            release: Arc::clone(&reader_release),
            first_read: true,
        },
        LateWatchdogReadyWriter {
            ready_sender: server_writer.sender.clone(),
            ready_payload: Some(ready),
            ready_sent: ready_sent_sender,
            release_write: Arc::clone(&writer_release),
            write_done: write_done_sender,
        },
        EmptyReader,
        142,
        Limits::default(),
        Some(callback),
        Duration::from_millis(40),
    )
    .unwrap();
    session
        .install_ready_token_challenge(token.to_owned())
        .unwrap();
    let mut pump = session.take_event_pump().unwrap();
    session
        .notify("initialized", json!({"readyToken":token}))
        .unwrap();
    ready_sent_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("ready sent before late write completion");
    frame_read_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("ready frame read before watchdog timeout");
    assert_eq!(
        terminal_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("watchdog terminal callback"),
        TerminalReason::Fault
    );
    write_done_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("late write completed after timeout");
    assert!(!session.wait_initialized_confirmation(Duration::from_millis(50)));
    assert!(!session.ready_after_initialized_barrier(&ready_frame));

    for _ in 0..3 {
        match pump.next_event(Duration::from_secs(1)) {
            Some(SessionEvent::Notification(_)) => {
                panic!("ready notification must not pass a timed-out barrier")
            }
            Some(SessionEvent::HandshakeFailed) => {
                // A closed queue may retain this terminal classification;
                // either way the ready notification must never be delivered.
            }
            Some(SessionEvent::WriterTimedOut | SessionEvent::ProtocolFault(_)) => {}
            Some(SessionEvent::Eof) => {}
            Some(_) | None => break,
        }
    }
    session.close();
    session
        .join_writer_until(Instant::now() + Duration::from_secs(1))
        .expect("late writer must be joined after watchdog timeout");
}

#[test]
fn codec_limits_and_reader_keep_consecutive_frames() {
    let first = RpcFrame::response_result("c:one", json!({}))
        .unwrap()
        .encode(1024)
        .unwrap();
    let second = RpcFrame::response_result("c:two", json!({}))
        .unwrap()
        .encode(1024)
        .unwrap();
    let mut reader = io::BufReader::new(std::io::Cursor::new([first, second].concat()));
    assert_eq!(
        codec::read_frame(&mut reader, 1024).unwrap().id_opt(),
        Some("c:one")
    );
    assert_eq!(
        codec::read_frame(&mut reader, 1024).unwrap().id_opt(),
        Some("c:two")
    );
    assert_eq!(
        codec::read_frame(&mut reader, 1024),
        Err(CodecError::UnexpectedEof)
    );
    let mut oversized = vec![b'x'; 1025];
    oversized.push(b'\n');
    assert_eq!(
        codec::decode_frame(&oversized, 1024),
        Err(CodecError::FrameTooLarge {
            actual: 1025,
            max: 1024
        })
    );
}

#[test]
fn pending64_deadline_late_duplicate_and_bounded_tombstones() {
    let mut pending = PendingRegistry::new(64, 2).unwrap();
    let now = Instant::now();
    for index in 0..64 {
        pending
            .register(format!("c:p-{index}"), now + Duration::from_secs(1))
            .unwrap();
    }
    assert!(
        pending
            .register("c:overflow", now + Duration::from_secs(1))
            .is_err()
    );
    assert_eq!(pending.expire(now + Duration::from_secs(2)), 64);
    assert_eq!(pending.active_len(), 0);
    assert_eq!(pending.tombstone_len(), 2);
    pending
        .register("c:deadline", now + Duration::from_secs(1))
        .unwrap();
    pending.expire(now + Duration::from_secs(2));
    assert_eq!(
        pending.resolve(RpcFrame::response_result("c:deadline", json!({})).unwrap()),
        ResolveDisposition::LateResponse
    );
    pending
        .register("c:new", now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        pending.resolve(RpcFrame::response_result("c:new", json!({})).unwrap()),
        ResolveDisposition::Delivered
    );
    assert_eq!(
        pending.resolve(RpcFrame::response_result("c:new", json!({})).unwrap()),
        ResolveDisposition::DuplicateResponse
    );
}

#[test]
fn session_full_duplex_keeps_reader_alive_for_nested_server_request() {
    let (server_to_host_reader, server_to_host_writer) = pipe_pair();
    let (host_to_server_reader, host_to_server_writer) = pipe_pair();
    let session = Session::from_io(
        server_to_host_reader,
        host_to_server_writer,
        EmptyReader,
        7,
        Limits::default(),
    )
    .unwrap();
    let mut event_pump = session.take_event_pump().unwrap();
    let server = thread::spawn(move || {
        let mut reader = host_to_server_reader;
        let mut writer = server_to_host_writer;
        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        let mut completed = 0;
        loop {
            line.clear();
            loop {
                if reader.read(&mut byte).is_err() {
                    return;
                }
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            let frame = codec::decode_frame(&line, Limits::default().max_frame_bytes).unwrap();
            if frame.method() == Some("nested") {
                let approval =
                    RpcFrame::server_request("s:approval-1", "approval/request", json!({}))
                        .unwrap()
                        .encode(4 * 1024 * 1024)
                        .unwrap();
                writer.write_all(&approval).unwrap();
                line.clear();
                loop {
                    reader.read_exact(&mut byte).unwrap();
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                let response = loop {
                    let response =
                        codec::decode_frame(&line, Limits::default().max_frame_bytes).unwrap();
                    if response.id_opt() == Some("s:approval-1") {
                        break response;
                    }
                    if response.method() == Some("parallel") {
                        let parallel = RpcFrame::response_result(
                            response.id().to_owned(),
                            json!({"parallel": true}),
                        )
                        .unwrap()
                        .encode(4 * 1024 * 1024)
                        .unwrap();
                        writer.write_all(&parallel).unwrap();
                        completed += 1;
                    }
                    line.clear();
                    loop {
                        reader.read_exact(&mut byte).unwrap();
                        line.push(byte[0]);
                        if byte[0] == b'\n' {
                            break;
                        }
                    }
                };
                assert_eq!(response.id_opt(), Some("s:approval-1"));
                let result =
                    RpcFrame::response_result(frame.id().to_owned(), json!({"accepted": true}))
                        .unwrap()
                        .encode(4 * 1024 * 1024)
                        .unwrap();
                writer.write_all(&result).unwrap();
                completed += 1;
            } else if frame.method() == Some("parallel") {
                let result =
                    RpcFrame::response_result(frame.id().to_owned(), json!({"parallel": true}))
                        .unwrap()
                        .encode(4 * 1024 * 1024)
                        .unwrap();
                writer.write_all(&result).unwrap();
                completed += 1;
            }
            if completed == 2 {
                return;
            }
        }
    });
    let nested_caller = session.clone();
    let nested_request = thread::spawn(move || {
        nested_caller
            .request("nested", json!({}), Duration::from_secs(2))
            .unwrap()
    });
    let parallel_caller = session.clone();
    let parallel_request = thread::spawn(move || {
        parallel_caller
            .request("parallel", json!({}), Duration::from_secs(2))
            .unwrap()
    });
    let event = event_pump
        .next_event(Duration::from_secs(1))
        .expect("nested request");
    let SessionEvent::ServerRequest(request_frame) = event else {
        panic!("expected server request");
    };
    session
        .respond_result(request_frame.id(), json!({"decision": "allow_once"}))
        .unwrap();
    let response = nested_request.join().unwrap();
    let parallel_response = parallel_request.join().unwrap();
    assert_eq!(
        response
            .result()
            .value()
            .and_then(|value| value.get("accepted"))
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        parallel_response
            .result()
            .value()
            .and_then(|value| value.get("parallel"))
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    session.close();
    drop(session);
    server.join().unwrap();
}

#[test]
fn session_closes_pending_on_eof_and_writer_fault() {
    let (_server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let (host_to_server_reader, host_to_server_writer) = pipe_pair();
    let session = Session::from_io(
        EmptyReader,
        host_to_server_writer,
        EmptyReader,
        8,
        Limits::default(),
    )
    .unwrap();
    let mut event_pump = session.take_event_pump().unwrap();
    drop(host_to_server_reader);
    assert!(matches!(
        event_pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::Eof)
    ));
    assert!(matches!(
        session.notify("runtime/statusChanged", json!({})),
        Err(agent_process::AgentProcessError::SessionClosed)
    ));
    assert!(matches!(
        session.respond_result("s:closed", json!({})),
        Err(agent_process::AgentProcessError::SessionClosed)
    ));

    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let session = Session::from_io(
        server_to_host_reader,
        FailingWriter,
        EmptyReader,
        9,
        Limits::default(),
    )
    .unwrap();
    let mut event_pump = session.take_event_pump().unwrap();
    let _ = session.notify("runtime/statusChanged", json!({}));
    assert!(matches!(
        event_pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::ProtocolFault(CodecError::Io))
    ));
    assert!(matches!(
        session.notify("runtime/statusChanged", json!({})),
        Err(agent_process::AgentProcessError::SessionClosed)
    ));
}

#[test]
fn notification_gate_rejects_stale_client_during_shutdown() {
    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let (host_to_server_reader, host_to_server_writer) = pipe_pair();
    let session = Session::from_io(
        server_to_host_reader,
        host_to_server_writer,
        EmptyReader,
        12,
        Limits::default(),
    )
    .unwrap();
    let stopping = Mutex::new(true);
    assert_eq!(
        session.notify_with_gate("runtime/statusChanged", json!({}), &stopping),
        Err(agent_process::AgentProcessError::ShuttingDown)
    );
    drop(host_to_server_reader);
    session.close();
}

#[test]
fn session_enforces_minimum_in_flight_and_closes_waiters() {
    let (ack_sender, ack_receiver) = mpsc::channel();
    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let limits = Limits {
        max_in_flight_requests: 2,
        max_pending_requests: 8,
        ..Limits::default()
    };
    let session = Session::from_io(
        server_to_host_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        10,
        limits,
    )
    .unwrap();
    let first = session.clone();
    let first_waiter =
        thread::spawn(move || first.request("parallel/one", json!({}), Duration::from_secs(10)));
    let second = session.clone();
    let second_waiter =
        thread::spawn(move || second.request("parallel/two", json!({}), Duration::from_secs(10)));
    ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    ack_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        session.request("parallel/three", json!({}), Duration::from_secs(1)),
        Err(agent_process::AgentProcessError::PendingLimit)
    );
    session.close();
    assert_eq!(
        first_waiter.join().unwrap(),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    assert_eq!(
        second_waiter.join().unwrap(),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
}

#[test]
fn session_close_during_wait_wakes_request_without_sleep() {
    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let (host_to_server_reader, host_to_server_writer) = pipe_pair();
    let session = Session::from_io(
        server_to_host_reader,
        host_to_server_writer,
        EmptyReader,
        11,
        Limits::default(),
    )
    .unwrap();
    let caller = session.clone();
    let waiter = thread::spawn(move || caller.request("wait", json!({}), Duration::from_secs(10)));
    drop(host_to_server_reader);
    session.close();
    assert_eq!(
        waiter.join().unwrap(),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
}

#[test]
fn event_pump_is_take_once_and_shutdown_gate_is_linearized() {
    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let (ack_sender, ack_receiver) = mpsc::channel();
    let session = Session::from_io(
        server_to_host_reader,
        AckWriter { writes: ack_sender },
        EmptyReader,
        13,
        Limits::default(),
    )
    .unwrap();
    let _pump = session.take_event_pump().unwrap();
    assert!(matches!(
        session.take_event_pump(),
        Err(agent_process::AgentProcessError::InvalidState)
    ));

    let gate = Arc::new(Mutex::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let caller = session.clone();
    let caller_gate = Arc::clone(&gate);
    let caller_barrier = Arc::clone(&barrier);
    let waiter = thread::spawn(move || {
        caller_barrier.wait();
        caller.request_with_gate(
            "race/first",
            json!({}),
            Duration::from_secs(5),
            &caller_gate,
        )
    });
    barrier.wait();
    ack_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("first request crossed admission gate");
    *gate.lock().unwrap() = true;
    assert_eq!(
        session.request_with_gate("race/second", json!({}), Duration::from_secs(1), &gate),
        Err(agent_process::AgentProcessError::ShuttingDown)
    );
    session.close();
    assert_eq!(
        waiter.join().unwrap(),
        Err(agent_process::AgentProcessError::SessionClosed)
    );
}

#[test]
fn stderr_budget_emits_one_truncation_and_keeps_draining() {
    let (server_to_host_reader, _server_to_host_writer) = pipe_pair();
    let (stderr_reader, mut stderr_writer) = pipe_pair();
    let (host_to_server_reader, host_to_server_writer) = pipe_pair();
    let limits = Limits {
        max_log_bytes: 4_096,
        max_stderr_line_bytes: 1_024,
        ..Limits::default()
    };
    let session = Session::from_io(
        server_to_host_reader,
        host_to_server_writer,
        stderr_reader,
        12,
        limits,
    )
    .unwrap();
    let mut event_pump = session.take_event_pump().unwrap();
    stderr_writer
        .write_all(b"api_key=sk-test-secret path=C:\\Users\\private\\project prompt=private source=secret.rs\n")
        .unwrap();
    stderr_writer.write_all(&vec![b'x'; 8_192]).unwrap();
    stderr_writer.write_all(b"\nsecond line\n").unwrap();
    drop(stderr_writer);
    drop(host_to_server_reader);
    let first = event_pump
        .next_event(Duration::from_secs(1))
        .expect("redacted stderr line");
    let first_debug = format!("{first:?}");
    match first {
        SessionEvent::StderrLine(line) => {
            assert_eq!(line, "sidecar stderr output redacted");
        }
        other => panic!("expected redacted stderr line, got {other:?}"),
    }
    for secret in [
        "sk-test-secret",
        "C:\\Users\\private\\project",
        "private",
        "secret.rs",
    ] {
        assert!(!first_debug.contains(secret));
    }
    assert!(matches!(
        event_pump.next_event(Duration::from_secs(1)),
        Some(SessionEvent::StderrTruncated)
    ));
    assert!(!matches!(
        event_pump.next_event(Duration::from_millis(20)),
        Some(SessionEvent::StderrTruncated)
    ));
    session.close();
}

/// 等待真实 child fixture 写出文件而不依赖固定 sleep，避免慢机器上的竞态猜测。
#[cfg(windows)]
fn wait_for_path(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now()
        .checked_add(timeout)
        .expect("fixture deadline fits in Instant");
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::yield_now();
    }
    false
}

/// 查询 PID 是否仍在 Windows 进程表中，用于验证 Job tree 的 descendant 收口。
#[cfg(windows)]
fn process_exists(pid: u32, system_root: &std::path::Path) -> bool {
    let tasklist = system_root.join("System32").join("tasklist.exe");
    let Ok(output) = Command::new(tasklist)
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

/// 通过真实 PowerShell child 回归每种 token 握手错误都立即返回 stable error。
#[test]
#[cfg(windows)]
fn real_child_invalid_ready_tokens_return_handshake_failed() {
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return;
    }
    for mode in ["missing", "malformed", "wrong", "old"] {
        let fixture_dir = TempFixtureDir::new();
        let run_dir = fixture_dir.path.clone();
        let script = r#"
param([string]$Mode)
$ErrorActionPreference = 'Stop'
$init = '{"jsonrpc":"2.0","id":"c:rpc-1","result":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"serverVersion":"fixture-1","serverInstanceId":"srv_fixture","runtime":{"kind":"native-image","agentScopeVersion":"1","javaVersion":"25"},"capabilities":{"methods":[],"events":[],"permissionModes":["plan","workspace","full_access"],"itemKinds":[],"mcp":{"protocolVersions":[],"transports":[],"features":[]}},"limits":{"maxFrameBytes":4194304,"maxInboundQueueFrames":256,"maxOutboundQueueFrames":1024,"maxInFlightRequests":64,"maxPendingRequests":64,"maxItemDeltaBytes":65536,"maxInlineToolOutputBytes":1048576,"maxArtifactBytes":268435456,"maxLogBytes":1048576,"defaultRequestDeadlineMs":120000,"defaultApprovalDeadlineMs":300000}}}'
function Write-Lf([string]$text) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($text + [char]10)
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}
while (($line = [Console]::In.ReadLine()) -ne $null) {
    if ($line -match '"method":"initialize"') {
        Write-Lf $init
        continue
    }
    if ($line -match '"method":"initialized"') {
        switch ($Mode) {
            'missing' {
                $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready"}}'
            }
            'malformed' {
                $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"0123456789ABCDEF0123456789ABCDEF"}}'
            }
            'wrong' {
                $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"00000000000000000000000000000000"}}'
            }
            default {
                $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"fedcba9876543210fedcba9876543210"}}'
            }
        }
        Write-Lf $ready
        Start-Sleep -Seconds 30
        break
    }
}
"#;
        let script_path = run_dir.join("fixture.ps1");
        fs::write(&script_path, script).unwrap();
        let mut config = SidecarConfig::new(&powershell, &run_dir);
        config.args = vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-File"),
            script_path.clone().into_os_string(),
            OsString::from("-Mode"),
            OsString::from(mode),
        ];
        config.env.insert(
            OsString::from("SystemRoot"),
            system_root.clone().into_os_string(),
        );
        config.env.insert(
            OsString::from("WINDIR"),
            system_root.clone().into_os_string(),
        );
        config.env.insert(
            OsString::from("SystemDrive"),
            system_root
                .components()
                .next()
                .map(|component| component.as_os_str().to_owned())
                .unwrap_or_else(|| OsString::from("C:")),
        );
        config.ready_timeout = Duration::from_secs(5);
        config.shutdown_timeout = Duration::from_secs(2);
        let mut supervisor = SidecarSupervisor::new(config).unwrap();
        assert_eq!(
            supervisor.start(),
            Err(agent_process::AgentProcessError::HandshakeFailed),
            "mode {mode} must not degrade to a ready timeout"
        );
        assert_eq!(supervisor.state(), LifecycleState::Faulted);
        supervisor.shutdown(Duration::from_secs(1)).unwrap();
    }
}

/// 真实 child 的首次超时清理必须保留 owner；第二次 shutdown 在同一
/// supervisor 上重试并确认 tree reap 后才允许 Exited，防止失败即假绿。
#[test]
#[cfg(windows)]
fn real_child_shutdown_retries_retained_owner_after_reap_timeout() {
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return;
    }
    let fixture_dir = TempFixtureDir::new();
    let script_path = fixture_dir.path.join("retry-shutdown.ps1");
    let script = r#"
$ErrorActionPreference = 'Stop'
$init = '{"jsonrpc":"2.0","id":"c:rpc-1","result":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"serverVersion":"fixture-1","serverInstanceId":"srv_fixture","runtime":{"kind":"native-image","agentScopeVersion":"1","javaVersion":"25"},"capabilities":{"methods":[],"events":[],"permissionModes":["plan","workspace","full_access"],"itemKinds":[],"mcp":{"protocolVersions":[],"transports":[],"features":[]}},"limits":{"maxFrameBytes":4194304,"maxInboundQueueFrames":256,"maxOutboundQueueFrames":1024,"maxInFlightRequests":64,"maxPendingRequests":64,"maxItemDeltaBytes":65536,"maxInlineToolOutputBytes":1048576,"maxArtifactBytes":268435456,"maxLogBytes":1048576,"defaultRequestDeadlineMs":120000,"defaultApprovalDeadlineMs":300000}}}'
function Write-Lf([string]$text) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($text + [char]10)
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}
while (($line = [Console]::In.ReadLine()) -ne $null) {
    if ($line -match '"method":"initialize"') {
        Write-Lf $init
        continue
    }
    if ($line -match '"method":"initialized"') {
        $token = ($line | ConvertFrom-Json).params.readyToken
        Write-Lf ('{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"' + $token + '"}}')
        Start-Sleep -Seconds 30
        break
    }
}
"#;
    fs::write(&script_path, script).expect("retry fixture script");
    let mut config = SidecarConfig::new(&powershell, &fixture_dir.path);
    config.args = vec![
        OsString::from("-NoProfile"),
        OsString::from("-NonInteractive"),
        OsString::from("-File"),
        script_path.into_os_string(),
    ];
    config.env.insert(
        OsString::from("SystemRoot"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("WINDIR"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("SystemDrive"),
        system_root
            .components()
            .next()
            .map(|component| component.as_os_str().to_owned())
            .unwrap_or_else(|| OsString::from("C:")),
    );
    config.ready_timeout = Duration::from_secs(5);
    config.shutdown_timeout = Duration::from_secs(2);
    let mut supervisor = SidecarSupervisor::new(config).expect("retry supervisor");
    supervisor.start().expect("retry fixture ready");
    let client = supervisor.client().expect("retry client");
    let payload = json!({"blob": "x".repeat(64 * 1024)});
    for _ in 0..256 {
        if client.notify("data/fill", payload.clone()).is_err() {
            break;
        }
    }
    let first = supervisor.shutdown(Duration::from_millis(1));
    assert!(first.is_err(), "zero cleanup budget must retain the owner");
    assert_eq!(supervisor.state(), LifecycleState::Stopping);
    let retry_deadline = Instant::now() + Duration::from_secs(4);
    let second = supervisor.shutdown(Duration::from_secs(3));
    assert!(
        second.is_ok(),
        "retained owner must be retryable: {second:?}"
    );
    assert!(Instant::now() < retry_deadline);
    assert_eq!(supervisor.state(), LifecycleState::Exited);
}

#[cfg(windows)]
struct TempFixtureDir {
    path: PathBuf,
}

#[cfg(windows)]
impl TempFixtureDir {
    /// 创建带进程/时间熵的临时 fixture 根，避免并发测试复用或污染仓库 cwd。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ja-agent-fixture-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

#[cfg(windows)]
impl Drop for TempFixtureDir {
    /// 无论断言或 child 握手何处失败，都回收 fixture 文件与临时目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
#[cfg(windows)]
fn real_child_handshake_concurrency_and_job_cleanup() {
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return;
    }
    let fixture_dir = TempFixtureDir::new();
    let run_dir = fixture_dir.path.clone();
    let pid_path = run_dir.join("grandchild.pid");
    assert!(pid_path.is_absolute());
    let script = r#"
param([string]$PidPath)
$ErrorActionPreference = 'Stop'
$init = '{"jsonrpc":"2.0","id":"c:rpc-1","result":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"serverVersion":"fixture-1","serverInstanceId":"srv_fixture","runtime":{"kind":"native-image","agentScopeVersion":"1","javaVersion":"25"},"capabilities":{"methods":[],"events":[],"permissionModes":["plan","workspace","full_access"],"itemKinds":[],"mcp":{"protocolVersions":[],"transports":[],"features":[]}},"limits":{"maxFrameBytes":4194304,"maxInboundQueueFrames":256,"maxOutboundQueueFrames":1024,"maxInFlightRequests":64,"maxPendingRequests":64,"maxItemDeltaBytes":65536,"maxInlineToolOutputBytes":1048576,"maxArtifactBytes":268435456,"maxLogBytes":1048576,"defaultRequestDeadlineMs":120000,"defaultApprovalDeadlineMs":300000}}}'
function Write-Lf([string]$text) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($text + "`n")
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}
while (($line = [Console]::In.ReadLine()) -ne $null) {
    if ($line -match '"method":"initialize"') {
        Write-Lf $init
        $grandchild = Start-Process -FilePath ($env:SystemRoot + '\System32\WindowsPowerShell\v1.0\powershell.exe') -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30') -WindowStyle Hidden -PassThru
        [IO.File]::WriteAllText($PidPath, [string]$grandchild.Id)
        continue
    }
    if ($line -match '"method":"initialized"') {
        $token = ($line | ConvertFrom-Json).params.readyToken
        $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"' + $token + '"}}'
        Write-Lf $ready
        continue
    }
    if ($line -match '"method":"test/approval"') {
        if ($line -match '"id":"(?<clientId>c:[^"]+)"') {
            $clientRequestId = $Matches['clientId']
            Write-Lf '{"jsonrpc":"2.0","id":"s:approval-1","method":"approval/request","params":{"kind":"command"}}'
            while (($approval = [Console]::In.ReadLine()) -ne $null) {
                if ($approval -match '"id":"s:approval-1"') {
                    Write-Lf ('{"jsonrpc":"2.0","id":"' + $clientRequestId + '","result":{"approved":true}}')
                    break
                }
            }
        }
        continue
    }
    if ($line -match '"method":"test/exit"') {
        if ($line -match '"id":"(?<id>c:[^"]+)"') {
            Write-Lf ('{"jsonrpc":"2.0","id":"' + $Matches['id'] + '","result":{"ok":true}}')
        }
        break
    }
    if ($line -match '"id":"(?<id>c:[^"]+)"') {
        Write-Lf ('{"jsonrpc":"2.0","id":"' + $Matches['id'] + '","result":{"ok":true}}')
    }
}
"#;
    let script_path = run_dir.join("fixture.ps1");
    fs::write(&script_path, script).unwrap();
    let mut config = SidecarConfig::new(&powershell, &run_dir);
    config.args = vec![
        OsString::from("-NoProfile"),
        OsString::from("-NonInteractive"),
        OsString::from("-File"),
        script_path.clone().into_os_string(),
        OsString::from("-PidPath"),
        pid_path.clone().into_os_string(),
    ];
    config.env.insert(
        OsString::from("SystemRoot"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("WINDIR"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("SystemDrive"),
        system_root
            .components()
            .next()
            .map(|component| component.as_os_str().to_owned())
            .unwrap_or_else(|| OsString::from("C:")),
    );
    config.ready_timeout = Duration::from_secs(5);
    config.shutdown_timeout = Duration::from_secs(2);
    let mut supervisor = SidecarSupervisor::new(config).unwrap();
    supervisor.start().unwrap();
    assert_eq!(supervisor.state(), LifecycleState::Ready);
    let client = supervisor.client().unwrap();
    let one = client.clone();
    let two = client.clone();
    let first = thread::spawn(move || one.request("test/one", json!({}), Duration::from_secs(2)));
    let second = thread::spawn(move || two.request("test/two", json!({}), Duration::from_secs(2)));
    assert!(first.join().unwrap().is_ok());
    assert!(second.join().unwrap().is_ok());

    let approval_client = client.clone();
    let approval = thread::spawn(move || {
        approval_client.request("test/approval", json!({}), Duration::from_secs(2))
    });
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("approval deadline fits in Instant");
    let mut saw_approval = false;
    while Instant::now() < deadline {
        if let Some(SessionEvent::ServerRequest(frame)) =
            supervisor.next_event(Duration::from_millis(50))
        {
            assert_eq!(frame.id(), "s:approval-1");
            client
                .respond_result(frame.id(), json!({"approved":true}))
                .unwrap();
            saw_approval = true;
            break;
        }
    }
    assert!(
        saw_approval,
        "supervisor event pump must expose approval request"
    );
    assert!(approval.join().unwrap().is_ok());

    assert!(wait_for_path(&pid_path, Duration::from_secs(2)));
    let grandchild_pid: u32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    assert!(process_exists(grandchild_pid, &system_root));
    assert!(
        client
            .request("test/exit", json!({}), Duration::from_secs(2))
            .is_ok()
    );
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("fixture deadline fits in Instant");
    let mut saw_exit = false;
    while Instant::now() < deadline {
        if matches!(
            supervisor.next_event(Duration::from_millis(50)),
            Some(SessionEvent::ProcessExited { .. } | SessionEvent::Eof)
        ) {
            saw_exit = true;
            break;
        }
    }
    assert!(saw_exit);
    let cleanup_deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("fixture cleanup deadline fits in Instant");
    while process_exists(grandchild_pid, &system_root) && Instant::now() < cleanup_deadline {
        thread::yield_now();
    }
    assert!(!process_exists(grandchild_pid, &system_root));
    supervisor.shutdown(Duration::from_secs(1)).unwrap();
    assert_eq!(
        client.notify("runtime/statusChanged", json!({})),
        Err(agent_process::AgentProcessError::ShuttingDown)
    );
}

/// 证明 stdout EOF 会在没有 event consumer 的情况下立即收口 sidecar Job。
#[test]
#[cfg(windows)]
fn real_child_stdout_eof_kills_tree_without_event_consumer() {
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return;
    }
    let fixture_dir = TempFixtureDir::new();
    let run_dir = fixture_dir.path.clone();
    let pid_path = run_dir.join("sidecar.pid");
    let script = r#"
param([string]$PidPath)
$ErrorActionPreference = 'Stop'
$init = '{"jsonrpc":"2.0","id":"c:rpc-1","result":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"serverVersion":"fixture-1","serverInstanceId":"srv_fixture","runtime":{"kind":"native-image","agentScopeVersion":"1","javaVersion":"25"},"capabilities":{"methods":[],"events":[],"permissionModes":["plan","workspace","full_access"],"itemKinds":[],"mcp":{"protocolVersions":[],"transports":[],"features":[]}},"limits":{"maxFrameBytes":4194304,"maxInboundQueueFrames":256,"maxOutboundQueueFrames":1024,"maxInFlightRequests":64,"maxPendingRequests":64,"maxItemDeltaBytes":65536,"maxInlineToolOutputBytes":1048576,"maxArtifactBytes":268435456,"maxLogBytes":1048576,"defaultRequestDeadlineMs":120000,"defaultApprovalDeadlineMs":300000}}}'
function Write-Lf([string]$text) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($text + "`n")
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}
while (($line = [Console]::In.ReadLine()) -ne $null) {
    if ($line -match '"method":"initialize"') {
        [IO.File]::WriteAllText($PidPath, [string]$PID)
        Write-Lf $init
        continue
    }
    if ($line -match '"method":"initialized"') {
        $token = ($line | ConvertFrom-Json).params.readyToken
        $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"' + $token + '"}}'
        Write-Lf $ready
        continue
    }
    if ($line -match '"method":"test/close-stdout"') {
        $stdout = [Console]::OpenStandardOutput()
        $stdout.SafeFileHandle.Close()
        $stdout.Dispose()
        Start-Sleep -Seconds 30
        continue
    }
}
"#;
    let script_path = run_dir.join("fixture.ps1");
    fs::write(&script_path, script).unwrap();
    let mut config = SidecarConfig::new(&powershell, &run_dir);
    config.args = vec![
        OsString::from("-NoProfile"),
        OsString::from("-NonInteractive"),
        OsString::from("-File"),
        script_path.clone().into_os_string(),
        OsString::from("-PidPath"),
        pid_path.clone().into_os_string(),
    ];
    config.env.insert(
        OsString::from("SystemRoot"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("WINDIR"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("SystemDrive"),
        system_root
            .components()
            .next()
            .map(|component| component.as_os_str().to_owned())
            .unwrap_or_else(|| OsString::from("C:")),
    );
    config.ready_timeout = Duration::from_secs(5);
    config.shutdown_timeout = Duration::from_secs(2);
    let mut supervisor = SidecarSupervisor::new(config).unwrap();
    supervisor.start().unwrap();
    let client = supervisor.client().unwrap();
    assert!(wait_for_path(&pid_path, Duration::from_secs(2)));
    let child_pid: u32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    assert!(process_exists(child_pid, &system_root));

    // Do not consume supervisor events: EOF itself must invoke the terminal
    // callback and close the complete Job while the UI is idle.
    let result = client.request("test/close-stdout", json!({}), Duration::from_secs(2));
    assert_eq!(result, Err(agent_process::AgentProcessError::SessionClosed));
    assert_eq!(supervisor.state(), LifecycleState::Backoff);
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("fixture deadline fits in Instant");
    while process_exists(child_pid, &system_root) && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(!process_exists(child_pid, &system_root));
    supervisor.shutdown(Duration::from_secs(1)).unwrap();
}

/// 真实 ChildStdin 不读数据时，watchdog 必须经 process-tree callback
/// 终止 leader/descendant，并在同一 deadline 内 join writer、释放 pending。
#[test]
#[cfg(windows)]
fn real_child_blocked_stdin_watchdog_joins_and_reaps_tree() {
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        return;
    }
    let fixture_dir = TempFixtureDir::new();
    let run_dir = fixture_dir.path.clone();
    let pid_path = run_dir.join("grandchild.pid");
    let script = r#"
param([string]$PidPath)
$ErrorActionPreference = 'Stop'
$init = '{"jsonrpc":"2.0","id":"c:rpc-1","result":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"serverVersion":"fixture-1","serverInstanceId":"srv_fixture","runtime":{"kind":"native-image","agentScopeVersion":"1","javaVersion":"25"},"capabilities":{"methods":[],"events":[],"permissionModes":["plan","workspace","full_access"],"itemKinds":[],"mcp":{"protocolVersions":[],"transports":[],"features":[]}},"limits":{"maxFrameBytes":4194304,"maxInboundQueueFrames":256,"maxOutboundQueueFrames":1024,"maxInFlightRequests":64,"maxPendingRequests":64,"maxItemDeltaBytes":65536,"maxInlineToolOutputBytes":1048576,"maxArtifactBytes":268435456,"maxLogBytes":1048576,"defaultRequestDeadlineMs":120000,"defaultApprovalDeadlineMs":300000}}}'
function Write-Lf([string]$text) {
    $bytes = [Text.Encoding]::UTF8.GetBytes($text + [char]10)
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}
while (($line = [Console]::In.ReadLine()) -ne $null) {
    if ($line -match '"method":"initialize"') {
        Write-Lf $init
        $grandchild = Start-Process -FilePath ($env:SystemRoot + '\System32\WindowsPowerShell\v1.0\powershell.exe') -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30') -WindowStyle Hidden -PassThru
        [IO.File]::WriteAllText($PidPath, [string]$grandchild.Id)
        continue
    }
    if ($line -match '"method":"initialized"') {
        $token = ($line | ConvertFrom-Json).params.readyToken
        $ready = '{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"serverInstanceId":"srv_fixture","eventId":"evt_ready","occurredAt":"2099-01-01T00:00:00Z","status":"ready","readyToken":"' + $token + '"}}'
        Write-Lf $ready
        Start-Sleep -Seconds 30
        break
    }
}
"#;
    let script_path = run_dir.join("fixture.ps1");
    fs::write(&script_path, script).unwrap();
    let mut config = SidecarConfig::new(&powershell, &run_dir);
    config.args = vec![
        OsString::from("-NoProfile"),
        OsString::from("-NonInteractive"),
        OsString::from("-File"),
        script_path.clone().into_os_string(),
        OsString::from("-PidPath"),
        pid_path.clone().into_os_string(),
    ];
    config.env.insert(
        OsString::from("SystemRoot"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("WINDIR"),
        system_root.clone().into_os_string(),
    );
    config.env.insert(
        OsString::from("SystemDrive"),
        system_root
            .components()
            .next()
            .map(|component| component.as_os_str().to_owned())
            .unwrap_or_else(|| OsString::from("C:")),
    );
    config.ready_timeout = Duration::from_secs(5);
    config.shutdown_timeout = Duration::from_secs(3);
    let mut supervisor = SidecarSupervisor::new(config).unwrap();
    supervisor.start().unwrap();
    let client = supervisor.client().unwrap();
    assert!(wait_for_path(&pid_path, Duration::from_secs(2)));
    let grandchild_pid: u32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
    assert!(process_exists(grandchild_pid, &system_root));

    let pending_client = client.clone();
    let (pending_done, pending_result) = mpsc::channel();
    let pending_waiter = thread::spawn(move || {
        let result = pending_client.request("blocked/request", json!({}), Duration::from_secs(30));
        let _ = pending_done.send(result.is_err());
        result
    });
    thread::yield_now();
    let watchdog_started = Instant::now();
    let payload = json!({"blob": "x".repeat(64 * 1024)});
    let mut sent = 0;
    for _ in 0..256 {
        match client.notify("data/fill", payload.clone()) {
            Ok(()) => sent += 1,
            Err(
                agent_process::AgentProcessError::QueueFull(_)
                | agent_process::AgentProcessError::SessionClosed,
            ) => break,
            Err(error) => panic!("unexpected fill error: {error:?}"),
        }
    }
    assert!(sent > 0, "at least one legal frame must enter the writer");
    assert!(
        pending_result
            .recv_timeout(Duration::from_secs(12))
            .expect("pending request must be released by terminal callback")
    );
    let pending_error = pending_waiter.join().unwrap();
    assert_eq!(
        pending_error,
        Err(agent_process::AgentProcessError::SessionClosed)
    );
    assert!(
        watchdog_started.elapsed() < Duration::from_secs(10),
        "real ChildStdin watchdog exceeded its total deadline"
    );

    let state_deadline = Instant::now() + Duration::from_secs(2);
    while supervisor.state() == LifecycleState::Ready && Instant::now() < state_deadline {
        thread::yield_now();
    }
    assert_eq!(supervisor.state(), LifecycleState::Backoff);
    assert_eq!(
        client.notify("stale/notify", json!({})),
        Err(agent_process::AgentProcessError::SessionClosed)
    );

    let join_deadline = Instant::now() + Duration::from_secs(2);
    supervisor
        .join_writer_until(join_deadline)
        .expect("writer actor must be joined after ChildStdin cancellation");
    let cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while process_exists(grandchild_pid, &system_root) && Instant::now() < cleanup_deadline {
        thread::yield_now();
    }
    assert!(!process_exists(grandchild_pid, &system_root));
    supervisor.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn lifecycle_generation_and_crash_backoff_are_deterministic() {
    #[derive(Default)]
    struct TestClock(Mutex<u64>);
    impl Clock for TestClock {
        fn now_millis(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }
    let clock = Arc::new(TestClock::default());
    let mut lifecycle = LifecycleMachine::with_clock(
        RestartPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
        },
        clock.clone(),
    )
    .unwrap();
    let generation = lifecycle.begin_start().unwrap();
    lifecycle.mark_ready(generation).unwrap();
    assert_eq!(lifecycle.record_crash(generation), LifecycleState::Backoff);
    assert!(!lifecycle.backoff_due());
    *clock.0.lock().unwrap() = 10;
    assert!(lifecycle.backoff_due());
    let next = lifecycle.begin_start().unwrap();
    assert_ne!(generation, next);
    lifecycle.mark_ready(next).unwrap();
    assert_eq!(lifecycle.record_crash(next), LifecycleState::Backoff);
    *clock.0.lock().unwrap() = 30;
    assert!(lifecycle.begin_start().is_ok());
    let current = lifecycle.generation();
    lifecycle.mark_ready(current).unwrap();
    assert_eq!(lifecycle.record_crash(current), LifecycleState::Faulted);
    assert_eq!(
        lifecycle.begin_start(),
        Err(agent_process::AgentProcessError::Faulted)
    );
    assert!(!lifecycle.mark_exited(generation));

    let mut single_attempt = LifecycleMachine::with_clock(
        RestartPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
        },
        clock.clone(),
    )
    .unwrap();
    let first_generation = single_attempt.begin_start().unwrap();
    single_attempt.mark_ready(first_generation).unwrap();
    assert_eq!(
        single_attempt.record_crash(first_generation),
        LifecycleState::Backoff
    );
    *clock.0.lock().unwrap() = 40;
    let second_generation = single_attempt.begin_start().unwrap();
    single_attempt.mark_ready(second_generation).unwrap();
    assert_eq!(
        single_attempt.record_crash(second_generation),
        LifecycleState::Faulted
    );
    assert_eq!(
        single_attempt.record_crash(first_generation),
        LifecycleState::Faulted
    );

    let mut exited = LifecycleMachine::with_clock(RestartPolicy::default(), clock.clone()).unwrap();
    let exited_generation = exited.begin_start().unwrap();
    assert!(exited.mark_exited(exited_generation));
    assert_eq!(
        exited.record_crash(exited_generation),
        LifecycleState::Exited
    );
    let mut incompatible =
        LifecycleMachine::with_clock(RestartPolicy::default(), clock.clone()).unwrap();
    let incompatible_generation = incompatible.begin_start().unwrap();
    assert!(incompatible.mark_incompatible(incompatible_generation));
    assert_eq!(
        incompatible.record_crash(incompatible_generation),
        LifecycleState::Incompatible
    );
}

#[test]
fn limits_reject_unbounded_configuration() {
    let limits = Limits {
        inbound_queue_frames: usize::MAX,
        ..Limits::default()
    };
    assert_eq!(limits.validate(), Err(CodecError::InvalidLimit));
    let stderr = Limits {
        max_stderr_line_bytes: usize::MAX,
        ..Limits::default()
    };
    assert_eq!(stderr.validate(), Err(CodecError::InvalidLimit));
    assert!(PendingRegistry::new(usize::MAX, 1).is_err());
    assert!(PendingRegistry::new(1, usize::MAX).is_err());
}

/// 证明 Native-only 启动边界不允许 JRE/PATH 回退或把 secret 放进 argv。
#[test]
fn native_only_config_rejects_jre_fallback_environment() {
    let executable = std::env::current_exe().unwrap();
    let run_dir = std::env::temp_dir();
    let mut java_home = SidecarConfig::new(&executable, &run_dir);
    java_home
        .env
        .insert(OsString::from("JAVA_HOME"), OsString::from("C:\\JDK"));
    assert_eq!(
        java_home.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut path = SidecarConfig::new(&executable, &run_dir);
    path.env.insert(
        OsString::from("PATH"),
        OsString::from("C:\\Windows\\System32"),
    );
    assert_eq!(
        path.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut secret_arg = SidecarConfig::new(&executable, &run_dir);
    secret_arg
        .args
        .push(OsString::from("api_key=sk-test-secret"));
    assert_eq!(
        secret_arg.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut contained = SidecarConfig::new(&executable, &run_dir);
    contained.workspace_root = Some(run_dir.clone());
    assert_eq!(
        contained.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut long_ready = SidecarConfig::new(&executable, &run_dir);
    long_ready.ready_timeout = Duration::from_secs(601);
    assert_eq!(
        long_ready.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut unverified_policy = SidecarConfig::new(&executable, &run_dir);
    unverified_policy.initialize_params["workspacePolicy"]["enforcement"] = json!("os_enforced");
    assert_eq!(
        unverified_policy.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut verified_policy = SidecarConfig::new(&executable, &run_dir);
    verified_policy.workspace_enforcement_verified = true;
    verified_policy.initialize_params["workspacePolicy"]["enforcement"] = json!("os_enforced");
    assert!(verified_policy.validate().is_ok());

    let mut workspace_mode = SidecarConfig::new(&executable, &run_dir);
    workspace_mode.initialize_params["workspacePolicy"]["mode"] = json!("workspace");
    assert_eq!(
        workspace_mode.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );

    let mut replaced_path = SidecarConfig::new(&executable, &run_dir);
    replaced_path.run_dir = run_dir.join("..");
    assert_eq!(
        replaced_path.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );
}

#[test]
fn canonical_config_survives_link_replacement_without_following_new_target() {
    let root = std::env::temp_dir().join(format!(
        "ja-canonical-link-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let original = root.join("original");
    let replacement = root.join("replacement");
    let link = root.join("run");
    fs::create_dir_all(&original).unwrap();
    fs::create_dir_all(&replacement).unwrap();

    #[cfg(windows)]
    let link_created = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            link.to_str().unwrap(),
            original.to_str().unwrap(),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    #[cfg(unix)]
    let link_created = std::os::unix::fs::symlink(&original, &link).is_ok();
    #[cfg(not(any(unix, windows)))]
    let link_created = false;

    if !link_created {
        let _ = fs::remove_dir_all(&root);
        return;
    }
    let executable = std::env::current_exe().unwrap();
    let mut config = SidecarConfig::new(&executable, &link);
    assert!(config.validate().is_ok());

    #[cfg(windows)]
    {
        fs::remove_dir(&link).unwrap();
        assert!(
            Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    link.to_str().unwrap(),
                    replacement.to_str().unwrap(),
                ])
                .status()
                .unwrap()
                .success()
        );
    }
    #[cfg(unix)]
    {
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&replacement, &link).unwrap();
    }
    assert!(config.validate().is_ok());
    config.run_dir = link;
    assert_eq!(
        config.validate(),
        Err(agent_process::AgentProcessError::InvalidConfig)
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(windows)]
#[test]
fn executable_identity_guard_blocks_target_replacement_until_config_drop() {
    let root = std::env::temp_dir().join(format!(
        "ja-executable-identity-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let executable = root.join("sidecar.exe");
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let config = SidecarConfig::new(&executable, &root);
    config.validate().expect("identity guard opens executable");
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .open(&executable)
            .is_err(),
        "read-only sharing must block writes while config is alive"
    );
    drop(config);
    fs::remove_file(&executable).expect("identity guard releases target");
    fs::remove_dir_all(root).unwrap();
}
