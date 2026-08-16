// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! sidecar host 对外错误。
//!
//! 这里故意不携带 command、绝对路径、环境变量或底层错误字符串；日志可以在
//! 上层用诊断 ID 关联，但 UI/协议错误不能把 secret 或用户目录泄露出去。

use crate::agent_process::codec::CodecError;
use crate::agent_process::pending::PendingRegisterError;
use std::fmt::{Display, Formatter};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Control,
    Data,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentProcessError {
    InvalidConfig,
    InvalidTimeout,
    Codec(CodecError),
    QueueFull(QueueKind),
    QueueClosed(QueueKind),
    PendingLimit,
    RequestLedgerExhausted,
    DuplicateRequest,
    UnknownRequestId,
    DuplicateResponse,
    LateResponse,
    DeadlineExceeded,
    Cancelled,
    SessionClosed,
    NotReady,
    Incompatible,
    InvalidState,
    Spawn,
    ProcessTree,
    ProcessExited,
    ShuttingDown,
    HandshakeFailed,
    ProtocolFault,
    InvalidErrorCatalog,
    ShutdownTimeout,
    Backoff { retry_after: Duration },
    RestartLimitExceeded,
    Faulted,
}

impl Display for AgentProcessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig => formatter.write_str("invalid sidecar configuration"),
            Self::InvalidTimeout => formatter.write_str("sidecar timeout exceeds hard limit"),
            Self::Codec(error) => write!(formatter, "sidecar protocol framing failed: {error}"),
            Self::QueueFull(kind) => write!(formatter, "sidecar {kind:?} queue is full"),
            Self::QueueClosed(kind) => write!(formatter, "sidecar {kind:?} queue is closed"),
            Self::PendingLimit => formatter.write_str("sidecar pending request limit reached"),
            Self::RequestLedgerExhausted => {
                formatter.write_str("sidecar request id ledger exhausted")
            }
            Self::DuplicateRequest => formatter.write_str("duplicate sidecar request"),
            Self::UnknownRequestId => formatter.write_str("unknown sidecar request id"),
            Self::DuplicateResponse => formatter.write_str("duplicate sidecar response"),
            Self::LateResponse => formatter.write_str("late sidecar response"),
            Self::DeadlineExceeded => formatter.write_str("sidecar request deadline exceeded"),
            Self::Cancelled => formatter.write_str("sidecar request cancelled"),
            Self::SessionClosed => formatter.write_str("sidecar session closed"),
            Self::NotReady => formatter.write_str("sidecar is not ready"),
            Self::Incompatible => formatter.write_str("sidecar protocol is incompatible"),
            Self::InvalidState => formatter.write_str("invalid sidecar lifecycle state"),
            Self::Spawn => formatter.write_str("sidecar process could not be started"),
            Self::ProcessTree => formatter.write_str("sidecar process tree cleanup failed"),
            Self::ProcessExited => formatter.write_str("sidecar process exited"),
            Self::ShuttingDown => formatter.write_str("sidecar is shutting down"),
            Self::HandshakeFailed => formatter.write_str("sidecar handshake failed"),
            Self::ProtocolFault => formatter.write_str("sidecar protocol fault"),
            Self::InvalidErrorCatalog => {
                formatter.write_str("unsupported sidecar error catalog entry")
            }
            Self::ShutdownTimeout => formatter.write_str("sidecar shutdown deadline exceeded"),
            Self::Backoff { retry_after } => {
                write!(formatter, "sidecar restart is in backoff ({retry_after:?})")
            }
            Self::RestartLimitExceeded => formatter.write_str("sidecar restart limit exceeded"),
            Self::Faulted => formatter.write_str("sidecar host is faulted"),
        }
    }
}

impl std::error::Error for AgentProcessError {}

impl From<CodecError> for AgentProcessError {
    /// 保留 codec 的稳定分类，让 session 不把原始 frame 内容泄露到错误消息。
    fn from(error: CodecError) -> Self {
        if matches!(error, CodecError::HandshakeFailed) {
            Self::HandshakeFailed
        } else {
            Self::Codec(error)
        }
    }
}

impl From<PendingRegisterError> for AgentProcessError {
    /// 将 pending registry 的有限分类映射到 host API 的稳定错误集合。
    fn from(error: PendingRegisterError) -> Self {
        match error {
            PendingRegisterError::DuplicateRequest => Self::DuplicateRequest,
            PendingRegisterError::LimitReached => Self::PendingLimit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ErrorCatalogEntry {
    pub(crate) code: i64,
    pub(crate) ja_code: &'static str,
    pub(crate) message: &'static str,
    pub(crate) retryable: bool,
}

/// 返回 frozen server-request error catalog，拒绝内部 caller 自定义错误语义。
pub(crate) fn error_catalog(
    code: i64,
    ja_code: &str,
    retryable: bool,
) -> Option<ErrorCatalogEntry> {
    crate::agent_process::codec::catalog_entry(code, ja_code, retryable).map(
        |(canonical_ja_code, message)| ErrorCatalogEntry {
            code,
            ja_code: canonical_ja_code,
            message,
            retryable,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 证明手写 code/jaCode/retryable 组合不能绕过 frozen catalog。
    #[test]
    fn error_catalog_rejects_drift() {
        assert!(error_catalog(-32_020, "SHUTTING_DOWN", true).is_some());
        assert!(error_catalog(-32_017, "HANDSHAKE_FAILED", false).is_some());
        assert!(error_catalog(-32_020, "SHUTTING_DOWN", false).is_none());
        assert!(error_catalog(-32_001, "CUSTOM", false).is_none());
    }
}
