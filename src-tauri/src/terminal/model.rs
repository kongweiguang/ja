// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端与 Tauri Channel 之间共享的纯数据模型。
//!
//! 输出使用 `Vec<u8>` 而不是 `String`，因为 PTY 可能在 UTF-8 code point 或 ANSI
//! escape sequence 中间切块；保留原字节让前端 terminal emulator 自己完成重组。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// 单个终端 session 的不透明身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalId(Uuid);

impl TerminalId {
    /// 由 host 生成不可预测 id，避免前端自行伪造 session owner。
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// 受控 shell profile；前端只能选择 profile，不能把 raw command 作为启动器传入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellProfile {
    Default,
    PowerShell,
    Cmd,
    Bash,
    Zsh,
    Fish,
}

impl Default for ShellProfile {
    /// The platform-native interactive shell is the least surprising default
    /// while explicit profiles remain available for deterministic workflows.
    fn default() -> Self {
        Self::Default
    }
}

/// 关闭 session 的用户可观察原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    User,
    Shutdown,
    Timeout,
    QueueOverflow,
    ProcessExited,
    Fault,
}

/// 跨平台终端尺寸；零值被拒绝，避免不同 PTY backend 对零列/零行解释不一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub pixel_width: u16,
    #[serde(default)]
    pub pixel_height: u16,
}

impl Default for TerminalSize {
    /// A conventional initial viewport avoids backend-specific zero-size PTY behavior.
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl TerminalSize {
    /// 限制尺寸，防止前端通过极端数值触发平台坐标溢出或无意义分配。
    pub(crate) fn validate(self) -> bool {
        (1..=4_096).contains(&self.rows) && (1..=4_096).contains(&self.cols)
    }
}

/// 启动一个终端 session 的内部请求；cwd 是由 policy 做 canonical containment 校验的路径。
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub profile: ShellProfile,
    pub cwd: Option<PathBuf>,
    pub env: std::collections::BTreeMap<String, String>,
    pub size: TerminalSize,
}

impl Default for LaunchRequest {
    /// Start in the policy workspace with the platform-selected interactive shell.
    fn default() -> Self {
        Self {
            profile: ShellProfile::Default,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            size: TerminalSize::default(),
        }
    }
}

/// 单个 reader/writer/resize worker 送出的批量事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEvent {
    pub session_id: TerminalId,
    pub generation: u64,
    pub sequence: u64,
    pub kind: TerminalEventKind,
}

/// 事件 payload；Output 永远携带未经解码的 PTY bytes。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TerminalEventKind {
    Output { data: Vec<u8> },
    Resized { size: TerminalSize },
    Exited { code: u32, signal: Option<String> },
    Closed { reason: CloseReason },
    Error { code: u16 },
    OutputDropped { bytes: usize },
}

/// 仅用于启动阶段，避免把 PTY backend 的 command builder 暴露给 UI。
#[derive(Debug, Clone)]
pub(crate) struct ResolvedShell {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<std::ffi::OsString>,
}
