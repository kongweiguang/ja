// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 仅供 sidecar 探针使用的 native child/孙进程 fixture。

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// 根据显式模式执行可重复的协议/进程树场景，不依赖 shell 或随机 sleep。
fn main() {
    let mut args = env::args_os().skip(1);
    let Some(mode) = args.next() else {
        eprintln!("missing fixture mode");
        std::process::exit(64);
    };
    match mode.to_string_lossy().as_ref() {
        "--grandchild" => run_grandchild(args.next().map(PathBuf::from)),
        "--fake-child" => run_fake_child(args.collect()),
        _ => {
            eprintln!("unknown fixture mode");
            std::process::exit(64);
        }
    }
}

/// 持有 PID 文件并等待 Job Object/process group 回收，避免测试只观察主进程。
fn run_grandchild(pid_file: Option<PathBuf>) -> ! {
    if let Some(path) = pid_file {
        let _ = fs::write(path, std::process::id().to_string());
    }
    loop {
        thread::park();
    }
}

/// 响应最小 initialize/shutdown 协议，并按模式制造污染、崩溃和孙进程。
fn run_fake_child(arguments: Vec<std::ffi::OsString>) -> ! {
    let mode = option_value(&arguments, "--mode").unwrap_or_else(|| "normal".to_string());
    let pid_file = option_value(&arguments, "--pid-file").map(PathBuf::from);
    let barrier = option_value(&arguments, "--shutdown-barrier").map(PathBuf::from);
    let flood_barrier = option_value(&arguments, "--flood-barrier").map(PathBuf::from);
    let flood_complete = option_value(&arguments, "--flood-complete").map(PathBuf::from);
    let env_report = option_value(&arguments, "--env-report").map(PathBuf::from);
    let sentinel_name = option_value(&arguments, "--sentinel-name")
        .unwrap_or_else(|| "JA_FIXTURE_PARENT_SENTINEL".to_string());
    let mut grandchild_pid_file = None;

    if let Some(path) = pid_file.as_ref() {
        let _ = write_pid(path, std::process::id());
    }
    if let Some(path) = env_report {
        let allowed = env::var("JA_FIXTURE_ALLOWED").unwrap_or_else(|_| "<missing>".to_string());
        let sentinel = env::var(&sentinel_name).unwrap_or_else(|_| "<missing>".to_string());
        let path_value = env::var("PATH").unwrap_or_else(|_| "<missing>".to_string());
        let _ = fs::write(
            path,
            format!("allowed={allowed}\nsentinel={sentinel}\nPATH={path_value}\n"),
        );
    }
    let tree_mode = matches!(
        mode.as_str(),
        "tree" | "crash-tree" | "control-flood-tree" | "control-flood-hold-tree"
    );
    if tree_mode {
        let child_pid_path = pid_file
            .as_ref()
            .map(|path| path.with_extension("grandchild.pid"));
        let mut command = Command::new(std::env::current_exe().expect("fixture executable"));
        command
            .arg("--grandchild")
            .arg(
                child_pid_path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("grandchild.pid")),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(child) = command.spawn() {
            if let Some(path) = &pid_file {
                let _ = fs::write(
                    path.with_extension("grandchild.spawned"),
                    child.id().to_string(),
                );
            }
            grandchild_pid_file = child_pid_path;
        }
    }

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
    let mut line = String::new();
    let mut ready_sent = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => std::process::exit(0),
            Ok(_) => {}
            Err(_) => std::process::exit(74),
        }
        if !ready_sent && line.contains("\"method\":\"initialize\"") {
            if mode == "pollution" {
                emit(&mut stdout, "STDOUT POLLUTION");
            }
            if mode == "incompatible" {
                emit(
                    &mut stdout,
                    r#"{"jsonrpc":"2.0","id":"c:initialize","error":{"code":-32003,"data":{"jaCode":"PROTOCOL_VERSION_UNSUPPORTED"}}}"#,
                );
                std::process::exit(78);
            }
            if mode == "malformed" {
                emit(&mut stdout, "{garbage}");
                ready_sent = true;
                continue;
            }
            if mode == "forged" {
                emit(
                    &mut stdout,
                    r#"{"jsonrpc":"2.0","method":"item/delta","params":{"text":"runtime/statusChanged ready PROTOCOL_VERSION_UNSUPPORTED"}}"#,
                );
                ready_sent = true;
                continue;
            }
            emit(
                &mut stdout,
                r#"{"jsonrpc":"2.0","id":"c:initialize","result":{"protocolMajor":1,"protocolMinor":0,"serverInstanceId":"srv_fixture"}}"#,
            );
            emit(
                &mut stdout,
                r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            );
            emit(
                &mut stdout,
                r#"{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{"status":"ready"}}"#,
            );
            ready_sent = true;
            if mode == "half" {
                stdout
                    .write_all(br#"{"jsonrpc":"2.0","method":"half""#)
                    .ok();
                stdout.flush().ok();
                std::process::exit(17);
            }
            if matches!(mode.as_str(), "crash" | "crash-tree") {
                std::process::exit(17);
            }
            if mode == "spam" {
                for index in 0..4096 {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","method":"item/delta","params":{{"index":{index}}}}}"#
                    )
                    .ok();
                }
                stdout.flush().ok();
            }
            if mode == "spam-exit" {
                for index in 0..4096 {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","method":"item/delta","params":{{"index":{index}}}}}"#
                    )
                    .ok();
                }
                stdout.flush().ok();
                std::process::exit(17);
            }
            if matches!(
                mode.as_str(),
                "control-flood" | "control-flood-tree" | "control-flood-hold-tree"
            ) {
                if let Some(path) = &flood_barrier {
                    // 首个 ready 先交给 host 建立 barrier；只有 host 明确放行后
                    // 才开始控制事件洪泛，避免启动阶段把 ready 自己挤掉。
                    while !path.exists() {
                        thread::park_timeout(Duration::from_millis(5));
                    }
                }
                for _ in 0..128 {
                    writeln!(
                        stdout,
                        r#"{{"jsonrpc":"2.0","method":"runtime/statusChanged","params":{{"status":"ready"}}}}"#
                    )
                    .ok();
                }
                stdout.flush().ok();
                if let Some(path) = &flood_complete {
                    let _ = fs::write(path, "complete");
                }
                if mode != "control-flood-hold-tree" {
                    std::process::exit(17);
                }
            }
            if mode == "stderr" {
                for index in 0..128 {
                    eprintln!("fixture-stderr-{index}");
                }
            }
            continue;
        }
        if ready_sent && line.contains("\"method\":\"shutdown\"") {
            if let Some(path) = &barrier {
                while !path.exists() {
                    thread::park_timeout(Duration::from_millis(5));
                }
            }
            emit(
                &mut stdout,
                r#"{"jsonrpc":"2.0","id":"c:shutdown","result":{"accepted":true}}"#,
            );
            let _ = grandchild_pid_file;
            std::process::exit(0);
        }
    }
}

/// 将固定 frame 原样写入 stdout，避免 format string 把 JSON 大括号当成占位符。
fn emit(writer: &mut impl Write, frame: &str) {
    writer.write_all(frame.as_bytes()).ok();
    writer.write_all(b"\n").ok();
    writer.flush().ok();
}

/// 读取无 shell 的结构化参数，fixture 不模拟命令拼接。
fn option_value(arguments: &[std::ffi::OsString], name: &str) -> Option<String> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].to_string_lossy().into_owned())
}

/// PID 文件是跨平台进程树验收的 barrier，而不是定时猜测 child 是否启动。
fn write_pid(path: &PathBuf, pid: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    writeln!(file, "{pid}")
}
