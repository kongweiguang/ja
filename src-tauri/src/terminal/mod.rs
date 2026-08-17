// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 用户可见的交互式终端边界。
//!
//! 终端不复用 sidecar 的 JSONL session：它需要保留 ANSI/UTF-8 原始字节、
//! resize 和 shell 的交互语义。`portable-pty` 负责平台 PTY，当前模块只负责
//! 受控启动、队列、生命周期和进程树收口，后续由 Tauri composition root 接线。

pub(crate) mod commands;
mod error;
mod model;
mod policy;
mod process;
mod queue;
mod session;

pub use commands::{
    TerminalCloseInput, TerminalCommandHost, TerminalConfigureInput, TerminalInput,
    TerminalOpenInput, TerminalPollInput, TerminalResizeInput, TerminalSessionInfo,
    TerminalShutdownError, ja_terminal_close, ja_terminal_configure, ja_terminal_input,
    ja_terminal_open, ja_terminal_poll, ja_terminal_resize, ja_terminal_scrollback,
};
pub use error::{TerminalError, TerminalErrorCode};
pub use model::{
    CloseReason, LaunchRequest, ShellProfile, TerminalEvent, TerminalEventKind, TerminalId,
    TerminalSize,
};
pub use policy::{TerminalLimits, TerminalPolicy};
pub use session::{SessionHandle, TerminalSupervisor};
