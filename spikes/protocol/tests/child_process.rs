// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 跨进程回归：证明 server 等待 approval 时 reader 仍处理另一个 client request。

use ja_rpc_protocol_spike::{DEFAULT_MAX_FRAME_BYTES, FrameError, RpcFrame, read_frame};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    // 测试失败或断言提前退出时主动终止 child，避免留下孤儿 sidecar。
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 只从测试主线程写入 stdin，确保 child 看到的 request 不会发生半 frame 交错。
fn send_frame(stdin: &mut ChildStdin, value: Value) {
    serde_json::to_writer(&mut *stdin, &value).expect("serialize request");
    stdin.write_all(b"\n").expect("write LF");
    stdin.flush().expect("flush request");
}

/// 独立 reader 持续消费 stdout，证明主线程等待结果时不会阻塞嵌套 request。
fn spawn_reader(stdout: impl std::io::Read + Send + 'static) -> Receiver<Result<RpcFrame, String>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES) {
                Ok(frame) => {
                    let result = Ok(frame);
                    if sender.send(result).is_err() {
                        break;
                    }
                }
                Err(FrameError::Eof) => break,
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

/// 持续消费 stderr 但只保留有限尾部，避免日志 pipe 填满而反向阻塞 child。
fn spawn_stderr_tail(stderr: impl Read + Send + 'static) -> Receiver<Result<Vec<u8>, String>> {
    const STDERR_TAIL_BYTES: usize = 4096;
    let (sender, receiver) = mpsc::channel::<Result<Vec<u8>, String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0_u8; 256];
        let mut tail = VecDeque::with_capacity(STDERR_TAIL_BYTES);
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    let _ = sender.send(Ok(tail.into_iter().collect()));
                    break;
                }
                Ok(count) => {
                    for byte in &chunk[..count] {
                        if tail.len() == STDERR_TAIL_BYTES {
                            tail.pop_front();
                        }
                        tail.push_back(*byte);
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    receiver
}

/// 用明确 deadline 收取 frame；只有最终 watchdog 使用时间，不依赖任意 sleep。
fn receive_until<F>(receiver: &Receiver<Result<RpcFrame, String>>, predicate: F) -> Vec<RpcFrame>
where
    F: Fn(&RpcFrame) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frames = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(Ok(frame)) => {
                let matched = predicate(&frame);
                frames.push(frame);
                if matched {
                    return frames;
                }
            }
            Ok(Err(error)) => panic!("child emitted invalid stdout: {error}"),
            Err(error) => panic!("child did not produce expected frame: {error}"),
        }
    }
    panic!("child protocol deadline exceeded")
}

/// 轮询 child 终态并在 deadline 后清理，避免测试挂起或泄漏进程。
fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            assert!(status.success(), "probe child exited with {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("probe child did not exit by deadline");
        }
        std::thread::yield_now();
    }
}

#[test]
/// 让 child 在等待 approval 时仍响应 version，直接覆盖双向 stdio 的死锁风险。
fn child_process_handles_nested_server_request_without_deadlock() {
    let binary =
        std::env::var("CARGO_BIN_EXE_probe-child").expect("cargo exposes probe-child path");
    let mut guard = ChildGuard {
        child: Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn protocol child"),
    };
    let mut stdin = guard.child.stdin.take().expect("child stdin");
    let stdout = guard.child.stdout.take().expect("child stdout");
    let stderr = guard.child.stderr.take().expect("child stderr");
    let receiver = spawn_reader(stdout);
    let stderr_receiver = spawn_stderr_tail(stderr);

    // 三个 request 连续写入，验证 child 不能因为等待 s:approval-1 而停止读取 c:version-1。
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"c:init-1","method":"initialize","params":{"protocolMajor":1,"protocolMinor":0}}),
    );
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"c:turn-start","method":"turn/start","params":{"threadId":"thread-probe","input":[]}}),
    );
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"c:version-1","method":"version","params":{}}),
    );

    let frames = receive_until(&receiver, |frame| {
        frame.id.as_deref() == Some("c:version-1")
    });
    assert!(
        frames
            .iter()
            .any(|frame| frame.id.as_deref() == Some("s:approval-1")
                && frame.method.as_deref() == Some("approval/request"))
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.id.as_deref() == Some("c:init-1") && frame.result.is_some())
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.id.as_deref() == Some("c:version-1") && frame.result.is_some())
    );

    // 只有 reader 正常工作时，下面的 nested response 才能让 turn request 收口。
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"s:approval-1","result":{"decision":"allow_once","scope":"once"}}),
    );
    let frames = receive_until(&receiver, |frame| {
        frame.id.as_deref() == Some("c:turn-start")
    });
    assert!(
        frames
            .iter()
            .any(|frame| frame.id.as_deref() == Some("c:turn-start") && frame.result.is_some())
    );

    // 重复 response 不应再次恢复副作用；仅发送一次 shutdown 作为终止帧。
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"s:approval-1","result":{"decision":"allow_once","scope":"once"}}),
    );
    send_frame(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":"c:shutdown-1","method":"shutdown","params":{"deadlineMs":1000}}),
    );
    let frames = receive_until(&receiver, |frame| {
        frame.id.as_deref() == Some("c:shutdown-1")
    });
    assert!(
        frames
            .iter()
            .any(|frame| frame.id.as_deref() == Some("c:shutdown-1") && frame.result.is_some())
    );
    drop(stdin);
    wait_for_exit(&mut guard.child);
    let stderr_tail = stderr_receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("stderr drainer should finish after child exit")
        .expect("stderr drainer should not fail");
    assert!(
        stderr_tail.is_empty(),
        "child emitted unexpected stderr: {}",
        String::from_utf8_lossy(&stderr_tail)
    );
}
