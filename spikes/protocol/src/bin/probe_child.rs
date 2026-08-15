// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 真实 child-process probe：在等待 S→C approval response 时继续处理 C→S request。

use ja_rpc_protocol_spike::{DEFAULT_MAX_FRAME_BYTES, RpcFrame, read_frame};
use serde_json::{Value, json};
use std::io::{BufReader, Write};

/// 统一 writer 保证每个响应是完整 LF frame，避免并发输出互相穿插。
fn write_frame<W: Write>(writer: &mut W, value: Value) -> std::io::Result<()> {
    // 单 writer 保证一个 JSON frame 不会被并发线程交错；日志只允许写 stderr。
    serde_json::to_writer(&mut *writer, &value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// 统一构造 response，确保 child 不会误把嵌套 server request 当作普通响应。
fn response(id: &str, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

/// 通过单 reader 循环同时处理 client request 与 server request response，验证无嵌套死锁。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut approval_pending = false;
    let mut approval_resolved = false;

    loop {
        let frame = match read_frame(&mut reader, DEFAULT_MAX_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(ja_rpc_protocol_spike::FrameError::Eof) => return Ok(()),
            Err(error) => {
                eprintln!("probe child protocol error: {error}");
                return Err(error.into());
            }
        };
        if let Some(method) = frame.method.as_deref() {
            match method {
                "initialize" => {
                    let id = frame.id.as_deref().unwrap_or("c:initialize");
                    write_frame(
                        &mut writer,
                        response(
                            id,
                            json!({"protocolMajor":1,"protocolMinor":0,"serverInstanceId":"srv_probe","capabilities":{},"limits":{}}),
                        ),
                    )?;
                }
                "version" => {
                    let id = frame.id.as_deref().unwrap_or("c:version");
                    write_frame(&mut writer, response(id, json!({"server":"probe-child"})))?;
                }
                "turn/start" => {
                    let id = frame.id.as_deref().unwrap_or("c:turn-start");
                    approval_pending = true;
                    write_frame(
                        &mut writer,
                        json!({
                            "jsonrpc":"2.0",
                            "id":"s:approval-1",
                            "method":"approval/request",
                            "params":{"approvalId":"approval-probe","action":{"kind":"shell","fingerprint":"probe-action"},"risk":"medium","expiresAt":"2099-01-01T00:00:00Z"}
                        }),
                    )?;
                    if approval_resolved {
                        write_frame(
                            &mut writer,
                            response(id, json!({"accepted":true,"turnId":"turn-probe"})),
                        )?;
                    }
                }
                "shutdown" => {
                    let id = frame.id.as_deref().unwrap_or("c:shutdown");
                    write_frame(
                        &mut writer,
                        response(id, json!({"accepted":true,"status":"shutting_down"})),
                    )?;
                    return Ok(());
                }
                _ => {
                    let id = frame.id.as_deref().unwrap_or("c:unknown");
                    write_frame(
                        &mut writer,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32006,"message":"unknown method","data":{"jaCode":"METHOD_NOT_FOUND","retryable":false}}}),
                    )?;
                }
            }
        } else if frame.id.as_deref() == Some("s:approval-1") && frame.result.is_some() {
            // 这个状态转换只允许第一次 response 恢复 approval，重复 response 不会再次发 turn。
            if approval_pending && !approval_resolved {
                approval_resolved = true;
                approval_pending = false;
                write_frame(
                    &mut writer,
                    response(
                        "c:turn-start",
                        json!({"accepted":true,"turnId":"turn-probe"}),
                    ),
                )?;
            }
        } else {
            let _typed: RpcFrame = frame;
        }
    }
}
