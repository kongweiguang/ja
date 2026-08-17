// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! session facade 的平台与生命周期验收。
//!
//! 测试单独放置，是为了让 worker 实现和公开 session API 可以分别阅读、
//! 编译与审查；它们仍然通过同一个 production facade 驱动真实 PTY。

use super::*;
use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
/// 真实 Unix PTY 必须保留 ANSI/raw bytes，并在 shell exit 后送出 Exited。
#[test]
fn real_pty_echo_resize_exit_and_repeated_close() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let policy = TerminalPolicy::new(&root).unwrap();
    let supervisor = TerminalSupervisor::new(policy);
    let session = supervisor
        .open(LaunchRequest {
            profile: super::super::model::ShellProfile::Bash,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            size: TerminalSize::default(),
        })
        .unwrap();
    session
        .resize(TerminalSize {
            rows: 40,
            cols: 120,
            ..TerminalSize::default()
        })
        .unwrap();
    session
        .send_input(
            b"printf '\\033[31mja-pty-echo\\033[0m\\n'; exit\n",
            Duration::from_secs(2),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut resized = false;
    let mut exited = false;
    while Instant::now() < deadline {
        if let Some(event) = session.recv_until(deadline).unwrap() {
            match event.kind {
                TerminalEventKind::Output { data } => output.extend(data),
                TerminalEventKind::Resized { size } => {
                    resized = size.rows == 40 && size.cols == 120
                }
                TerminalEventKind::Exited { .. } => {
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(resized, "resize event was not observed");
    assert!(exited, "PTY child did not exit before deadline");
    assert!(
        output
            .windows(b"ja-pty-echo".len())
            .any(|window| window == b"ja-pty-echo")
    );
    session.close(CloseReason::User).unwrap();
    session.close(CloseReason::User).unwrap();
    fs::remove_dir_all(root).unwrap();
}

/// Zero timeout must be classified before queue mutation, and a stale token cannot write.
#[test]
fn timeout_and_stale_generation_are_rejected() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let policy = TerminalPolicy::new(&root).unwrap();
    let supervisor = TerminalSupervisor::new(policy);
    let session = supervisor.open(LaunchRequest::default()).unwrap();
    assert_eq!(
        session.send_input(b"x", Duration::ZERO).unwrap_err().code(),
        TerminalErrorCode::DeadlineExceeded
    );
    let stale = SessionHandle {
        runtime: session.runtime.clone(),
        generation: session.generation.saturating_add(1),
    };
    assert_eq!(
        stale.resize(TerminalSize::default()).unwrap_err().code(),
        TerminalErrorCode::StaleGeneration
    );
    session.close(CloseReason::User).unwrap();
    fs::remove_dir_all(root).unwrap();
}

/// A handle-owned close must release its quota slot before the next tab opens.
#[test]
fn handle_close_releases_supervisor_slot() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let limits = super::super::policy::TerminalLimits {
        max_sessions: 1,
        ..super::super::policy::TerminalLimits::default()
    };
    let policy = TerminalPolicy::with_limits(&root, limits).unwrap();
    let supervisor = TerminalSupervisor::new(policy);
    let first = supervisor.open(LaunchRequest::default()).unwrap();
    first.close(CloseReason::User).unwrap();
    let second = supervisor.open(LaunchRequest::default()).unwrap();
    second.close(CloseReason::User).unwrap();
    fs::remove_dir_all(root).unwrap();
}

/// scrollback 保存 raw bytes 且只保留 configured tail，避免 UTF-8 解码破坏终端状态。
#[test]
fn scrollback_is_byte_bounded() {
    let mut scrollback = Scrollback {
        chunks: std::collections::VecDeque::new(),
        bytes: 0,
        limit: 4,
    };
    scrollback.append(vec![0xff, 0xfe, 0x1b, b'[', b'0', b'm']);
    assert_eq!(scrollback.snapshot(), vec![0x1b, b'[', b'0', b'm']);
}

#[cfg(unix)]
/// close 会终止由独立 process group 管理的 sleep shell，不能只结束 leader。
#[test]
fn real_pty_close_cleans_process_tree() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let supervisor = TerminalSupervisor::new(TerminalPolicy::new(&root).unwrap());
    let session = supervisor.open(LaunchRequest::default()).unwrap();
    session
        .send_input(b"sleep 30\n", Duration::from_secs(2))
        .unwrap();
    session.close(CloseReason::Timeout).unwrap();
    session.close(CloseReason::Timeout).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
/// Windows ConPTY must expose a real cmd.exe interaction, not a pipe-only fake shell.
#[test]
fn real_conpty_echo_and_exit() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let supervisor = TerminalSupervisor::new(TerminalPolicy::new(&root).unwrap());
    let session = supervisor
        .open(LaunchRequest {
            profile: super::super::model::ShellProfile::Cmd,
            ..LaunchRequest::default()
        })
        .unwrap();
    let mut output = Vec::new();
    let mut exited = false;
    let query_deadline = Instant::now() + Duration::from_secs(5);
    // cmd.exe asks the terminal for cursor position before accepting the first prompt;
    // the production xterm frontend answers this control sequence itself.
    while Instant::now() < query_deadline && !output.windows(4).any(|window| window == b"\x1b[6n") {
        if let Some(event) = session.recv_until(query_deadline).unwrap() {
            match event.kind {
                TerminalEventKind::Output { data } => output.extend(data),
                TerminalEventKind::Exited { .. } => {
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(
        !exited,
        "ConPTY child exited before its cursor query was answered"
    );
    session
        .send_input(b"\x1b[1;1R", Duration::from_secs(2))
        .unwrap();
    let prompt_offset = output.len();
    let prompt_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < prompt_deadline && !output[prompt_offset..].contains(&b'>') {
        if let Some(event) = session.recv_until(prompt_deadline).unwrap() {
            match event.kind {
                TerminalEventKind::Output { data } => output.extend(data),
                TerminalEventKind::Exited { .. } => break,
                _ => {}
            }
        }
    }
    assert!(
        output[prompt_offset..].contains(&b'>'),
        "cmd prompt was not observed after cursor response"
    );
    session
        .send_input(b"echo ja-conpty-echo\r", Duration::from_secs(2))
        .unwrap();
    let echo_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < echo_deadline
        && !output
            .windows(b"ja-conpty-echo".len())
            .any(|window| window == b"ja-conpty-echo")
    {
        if let Some(event) = session.recv_until(echo_deadline).unwrap() {
            match event.kind {
                TerminalEventKind::Output { data } => output.extend(data),
                TerminalEventKind::Exited { .. } => {
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(
        output
            .windows(b"ja-conpty-echo".len())
            .any(|window| window == b"ja-conpty-echo"),
        "ConPTY echo marker was not observed"
    );
    session
        .send_input(b"exit\r", Duration::from_secs(2))
        .unwrap();
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < exit_deadline {
        if let Some(event) = session.recv_until(exit_deadline).unwrap() {
            match event.kind {
                TerminalEventKind::Output { data } => output.extend(data),
                TerminalEventKind::Exited { .. } => {
                    exited = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(exited, "ConPTY child did not exit before deadline");
    session.close(CloseReason::User).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
/// Repeated real ConPTY open/resize/input/exit/close cycles must not accumulate
/// a stale owner or leave a shell outside the Job Object cleanup boundary.
#[test]
fn real_conpty_repeated_open_close_thirty_rounds() {
    let root = test_root();
    fs::create_dir_all(&root).unwrap();
    let supervisor = TerminalSupervisor::new(TerminalPolicy::new(&root).unwrap());
    for _round in 0..30 {
        let session = supervisor
            .open(LaunchRequest {
                profile: super::super::model::ShellProfile::Cmd,
                ..LaunchRequest::default()
            })
            .unwrap();
        session
            .resize(TerminalSize {
                rows: 30,
                cols: 100,
                ..TerminalSize::default()
            })
            .unwrap();
        let mut output = Vec::new();
        let query_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < query_deadline
            && !output.windows(4).any(|window| window == b"\x1b[6n")
        {
            if let Some(event) = session.recv_until(query_deadline).unwrap()
                && let TerminalEventKind::Output { data } = event.kind
            {
                output.extend(data);
            }
        }
        session
            .send_input(b"\x1b[1;1R", Duration::from_secs(2))
            .unwrap();
        session
            .send_input(b"echo ja-conpty-round\r", Duration::from_secs(2))
            .unwrap();
        session
            .send_input(b"exit\r", Duration::from_secs(2))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline {
            if let Some(event) = session.recv_until(deadline).unwrap() {
                match event.kind {
                    TerminalEventKind::Output { data } => output.extend(data),
                    TerminalEventKind::Exited { .. } => {
                        exited = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(exited, "ConPTY round did not emit Exited");
        session.close(CloseReason::User).unwrap();
    }
    fs::remove_dir_all(root).unwrap();
}

/// 每个测试使用独立 workspace，避免失败测试留下 cwd 影响后续 session。
fn test_root() -> PathBuf {
    std::env::temp_dir().join(format!("ja-terminal-test-{}", uuid::Uuid::new_v4()))
}
