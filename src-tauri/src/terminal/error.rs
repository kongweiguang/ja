// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端公开错误。
//!
//! 错误只携带稳定分类，不把 cwd、argv、环境变量或底层平台错误原文送进
//! WebView；底层诊断留在 tracing 边界，避免用户输入或 secret 进入 IPC。

use serde::Serialize;
use std::fmt::{Display, Formatter};

/// UI/IPC 可依赖的稳定错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u16)]
pub enum TerminalErrorCode {
    InvalidConfig = 1,
    InvalidShell = 2,
    InvalidCwd = 3,
    CwdOutsideWorkspace = 4,
    EnvironmentNotAllowed = 5,
    EnvironmentSecret = 6,
    InvalidSize = 7,
    SessionLimit = 8,
    SessionNotFound = 9,
    StaleGeneration = 10,
    SessionClosed = 11,
    QueueFull = 12,
    QueueClosed = 13,
    InputTooLarge = 14,
    OutputLimitExceeded = 15,
    DeadlineExceeded = 16,
    Cancelled = 17,
    SpawnFailed = 18,
    PtyFailed = 19,
    ProcessCleanupFailed = 20,
    WorkerShutdownTimeout = 21,
    UnsupportedPlatform = 22,
    EnvironmentLimit = 26,
    EnvironmentDangerous = 27,
}

/// 终端 API 的领域错误；不保存非脱敏平台字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TerminalError {
    code: TerminalErrorCode,
}

impl TerminalError {
    /// 构造稳定分类，避免 caller 把底层 path 或 secret 携带到错误对象。
    pub const fn new(code: TerminalErrorCode) -> Self {
        Self { code }
    }

    /// 返回可安全写入 IPC 的错误分类。
    pub const fn code(self) -> TerminalErrorCode {
        self.code
    }
}

impl Display for TerminalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.code {
            TerminalErrorCode::InvalidConfig => "invalid terminal configuration",
            TerminalErrorCode::InvalidShell => "terminal shell profile is unavailable",
            TerminalErrorCode::InvalidCwd => "terminal working directory is invalid",
            TerminalErrorCode::CwdOutsideWorkspace => {
                "terminal working directory is outside workspace"
            }
            TerminalErrorCode::EnvironmentNotAllowed => {
                "terminal environment variable is not allowed"
            }
            TerminalErrorCode::EnvironmentSecret => "terminal environment variable is sensitive",
            TerminalErrorCode::InvalidSize => "terminal size is invalid",
            TerminalErrorCode::SessionLimit => "terminal session limit reached",
            TerminalErrorCode::SessionNotFound => "terminal session was not found",
            TerminalErrorCode::StaleGeneration => "terminal session generation is stale",
            TerminalErrorCode::SessionClosed => "terminal session is closed",
            TerminalErrorCode::QueueFull => "terminal queue is full",
            TerminalErrorCode::QueueClosed => "terminal queue is closed",
            TerminalErrorCode::InputTooLarge => "terminal input exceeds its limit",
            TerminalErrorCode::OutputLimitExceeded => "terminal output exceeds its limit",
            TerminalErrorCode::DeadlineExceeded => "terminal operation deadline exceeded",
            TerminalErrorCode::Cancelled => "terminal operation was cancelled",
            TerminalErrorCode::SpawnFailed => "terminal process could not be started",
            TerminalErrorCode::PtyFailed => "terminal PTY operation failed",
            TerminalErrorCode::ProcessCleanupFailed => "terminal process cleanup failed",
            TerminalErrorCode::WorkerShutdownTimeout => {
                "terminal workers did not stop before deadline"
            }
            TerminalErrorCode::UnsupportedPlatform => "terminal platform is unsupported",
            TerminalErrorCode::EnvironmentLimit => "terminal environment exceeds its limits",
            TerminalErrorCode::EnvironmentDangerous => "terminal environment variable is dangerous",
        })
    }
}

impl std::error::Error for TerminalError {}

/// 把内部 I/O/PTY 错误压缩到稳定领域分类，同时保留诊断给调用方日志使用。
pub(crate) fn map_io(code: TerminalErrorCode, error: &std::io::Error) -> TerminalError {
    tracing::debug!(error_kind = ?error.kind(), ?code, "terminal operation failed");
    TerminalError::new(code)
}
