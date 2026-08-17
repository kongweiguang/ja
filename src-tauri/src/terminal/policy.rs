// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端启动策略。
//!
//! 该层把前端可表达的 profile/cwd/env 转换成受控的 `CommandBuilder` 输入；
//! 这样 session 生命周期不需要在每次 spawn 时重新猜测信任边界。

use super::error::{TerminalError, TerminalErrorCode};
use super::model::{LaunchRequest, ResolvedShell, ShellProfile, TerminalSize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "policy_env.rs"]
mod env;
#[path = "policy_paths.rs"]
mod paths;

use env::{
    MAX_ENV_KEY_BYTES, MAX_ENV_TOTAL_BYTES, MAX_ENV_VALUE_BYTES, MAX_ENV_VARS, build_environment,
};
use paths::{absolute_path, canonical_directory, path_is_within};

const MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_TERMINAL_EVENT_BYTES: usize = 64;

/// 所有队列、session 和后台 worker 的硬上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalLimits {
    pub max_sessions: usize,
    pub max_input_chunk_bytes: usize,
    pub max_input_queue_bytes: usize,
    pub max_output_batch_bytes: usize,
    pub max_output_queue_bytes: usize,
    pub max_event_count: usize,
    pub max_scrollback_bytes: usize,
    pub max_env_vars: usize,
    pub max_env_key_bytes: usize,
    pub max_env_value_bytes: usize,
    pub max_env_total_bytes: usize,
    pub operation_timeout: Duration,
}

impl Default for TerminalLimits {
    /// Defaults keep one desktop workspace responsive without allowing an
    /// unbounded number of PTY workers or retained output bytes.
    fn default() -> Self {
        Self {
            max_sessions: 8,
            max_input_chunk_bytes: 64 * 1024,
            max_input_queue_bytes: 1024 * 1024,
            max_output_batch_bytes: 64 * 1024,
            max_output_queue_bytes: 8 * 1024 * 1024,
            max_event_count: 2_048,
            max_scrollback_bytes: 4 * 1024 * 1024,
            max_env_vars: MAX_ENV_VARS,
            max_env_key_bytes: MAX_ENV_KEY_BYTES,
            max_env_value_bytes: MAX_ENV_VALUE_BYTES,
            max_env_total_bytes: MAX_ENV_TOTAL_BYTES,
            operation_timeout: Duration::from_secs(30),
        }
    }
}

impl TerminalLimits {
    /// 在启动前拒绝会导致整数溢出或不可控内存增长的配置，而不是让队列自行兜底。
    pub(crate) fn validate(self) -> Result<Self, TerminalError> {
        let valid = self.max_sessions > 0
            && self.max_sessions <= 64
            && self.max_input_chunk_bytes > 0
            && self.max_input_chunk_bytes <= 4 * 1024 * 1024
            && self.max_input_queue_bytes >= self.max_input_chunk_bytes
            && self.max_input_queue_bytes <= 64 * 1024 * 1024
            && self.max_output_batch_bytes > 0
            && self.max_output_batch_bytes <= 4 * 1024 * 1024
            && self.max_output_queue_bytes >= self.max_output_batch_bytes
            // Closed/Error/Exited must fit even after all ordinary output is
            // evicted; queue.rs uses the same fixed event-size floor.
            && self.max_output_queue_bytes >= MIN_TERMINAL_EVENT_BYTES
            && self.max_output_queue_bytes <= 256 * 1024 * 1024
            && self.max_event_count > 0
            && self.max_event_count <= 65_536
            && self.max_scrollback_bytes >= self.max_output_batch_bytes
            && self.max_scrollback_bytes <= 256 * 1024 * 1024
            && self.max_env_vars > 0
            && self.max_env_vars <= MAX_ENV_VARS
            && self.max_env_key_bytes > 0
            && self.max_env_key_bytes <= MAX_ENV_KEY_BYTES
            && self.max_env_value_bytes > 0
            && self.max_env_value_bytes <= MAX_ENV_VALUE_BYTES
            && self.max_env_total_bytes >= self.max_env_value_bytes
            && self.max_env_total_bytes <= MAX_ENV_TOTAL_BYTES
            && !self.operation_timeout.is_zero()
            && self.operation_timeout <= MAX_OPERATION_TIMEOUT;
        if valid {
            Ok(self)
        } else {
            Err(TerminalError::new(TerminalErrorCode::InvalidConfig))
        }
    }
}

/// 一个 supervisor 允许使用的工作区根与资源上限。
#[derive(Debug, Clone)]
pub struct TerminalPolicy {
    workspace_root: PathBuf,
    limits: TerminalLimits,
}

impl TerminalPolicy {
    /// canonicalize workspace root，保证后续 cwd containment 使用同一物理基准。
    pub fn new(root: impl AsRef<Path>) -> Result<Self, TerminalError> {
        Self::with_limits(root, TerminalLimits::default())
    }

    /// 使用显式上限创建 policy；限制在 session 共享前完成校验。
    pub fn with_limits(
        root: impl AsRef<Path>,
        limits: TerminalLimits,
    ) -> Result<Self, TerminalError> {
        let limits = limits.validate()?;
        let root = absolute_path(root.as_ref())
            .map_err(|_| TerminalError::new(TerminalErrorCode::InvalidCwd))?;
        let canonical = canonical_directory(&root)?;
        Ok(Self {
            workspace_root: canonical,
            limits,
        })
    }

    /// 返回已验证的资源上限。
    pub(crate) fn limits(&self) -> TerminalLimits {
        self.limits
    }

    /// 将 launch request 解析为受控 shell、canonical cwd 和最小环境。
    pub(super) fn prepare(&self, request: &LaunchRequest) -> Result<PreparedLaunch, TerminalError> {
        if !request.size.validate() {
            return Err(TerminalError::new(TerminalErrorCode::InvalidSize));
        }
        let cwd = self.resolve_cwd(request.cwd.as_deref())?;
        let shell = resolve_shell(request.profile)?;
        let environment = build_environment(&request.env, &shell.program)?;
        Ok(PreparedLaunch {
            shell,
            cwd,
            environment,
            size: request.size,
        })
    }

    /// 只允许现有 workspace 目录，避免 shell 通过 cwd 逃逸到用户未授权位置。
    fn resolve_cwd(&self, requested: Option<&Path>) -> Result<PathBuf, TerminalError> {
        let raw = match requested {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => self.workspace_root.join(path),
            None => self.workspace_root.clone(),
        };
        let canonical = canonical_directory(&raw)?;
        if !path_is_within(&self.workspace_root, &canonical) {
            return Err(TerminalError::new(TerminalErrorCode::CwdOutsideWorkspace));
        }
        Ok(canonical)
    }
}

/// 供 PTY spawn 使用的已经完成策略校验的数据。
#[derive(Debug, Clone)]
pub(super) struct PreparedLaunch {
    pub(super) shell: ResolvedShell,
    pub(super) cwd: PathBuf,
    pub(super) environment: BTreeMap<OsString, OsString>,
    pub(super) size: TerminalSize,
}

/// 解析默认 profile，使不同桌面平台提供自然的原生 shell。
pub(crate) fn resolve_shell(profile: ShellProfile) -> Result<ResolvedShell, TerminalError> {
    let profile = match profile {
        ShellProfile::Default => {
            #[cfg(windows)]
            {
                ShellProfile::PowerShell
            }
            #[cfg(target_os = "macos")]
            {
                ShellProfile::Zsh
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                ShellProfile::Bash
            }
            #[cfg(not(any(unix, windows)))]
            {
                return Err(TerminalError::new(TerminalErrorCode::UnsupportedPlatform));
            }
        }
        selected => selected,
    };

    let (program, args): (PathBuf, Vec<OsString>) = {
        #[cfg(windows)]
        {
            match profile {
                ShellProfile::PowerShell => {
                    let candidates = [
                        PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
                        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
                    ];
                    let program = candidates.into_iter().find(|path| path.is_file());
                    (
                        program.ok_or(TerminalError::new(TerminalErrorCode::InvalidShell))?,
                        vec!["-NoLogo".into()],
                    )
                }
                ShellProfile::Cmd => {
                    let root = std::env::var_os("SystemRoot")
                        .unwrap_or_else(|| OsString::from(r"C:\Windows"));
                    (
                        PathBuf::from(root).join("System32").join("cmd.exe"),
                        vec!["/Q".into()],
                    )
                }
                _ => return Err(TerminalError::new(TerminalErrorCode::InvalidShell)),
            }
        }
        #[cfg(unix)]
        {
            match profile {
                ShellProfile::Bash => (PathBuf::from("/bin/bash"), vec!["-i".into()]),
                ShellProfile::Zsh => (PathBuf::from("/bin/zsh"), vec!["-i".into()]),
                ShellProfile::Fish => (PathBuf::from("/usr/bin/fish"), vec!["-i".into()]),
                _ => return Err(TerminalError::new(TerminalErrorCode::InvalidShell)),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err(TerminalError::new(TerminalErrorCode::UnsupportedPlatform));
        }
    };

    if !program.is_file() {
        return Err(TerminalError::new(TerminalErrorCode::InvalidShell));
    }
    Ok(ResolvedShell { program, args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 限制关系必须在构造时失败，避免后续队列出现不可证明的组合。
    #[test]
    fn limits_reject_inverted_byte_budgets() {
        let mut limits = TerminalLimits::default();
        limits.max_input_chunk_bytes = limits.max_input_queue_bytes + 1;
        assert_eq!(
            limits.validate().unwrap_err().code(),
            TerminalErrorCode::InvalidConfig
        );
    }

    /// The reserved terminal lane must remain representable even when a
    /// caller supplies a very small output budget.
    #[test]
    fn limits_reserve_terminal_event_bytes() {
        let limits = TerminalLimits {
            max_output_queue_bytes: MIN_TERMINAL_EVENT_BYTES - 1,
            ..TerminalLimits::default()
        };
        assert_eq!(
            limits.validate().unwrap_err().code(),
            TerminalErrorCode::InvalidConfig
        );
    }

    /// secret-like key 即使不是默认 allowlist 也要返回专门的脱敏错误。
    #[test]
    fn environment_secret_is_rejected_before_allowlist() {
        let error = build_environment(
            &BTreeMap::from([(String::from("OPENAI_API_KEY"), String::from("hidden"))]),
            Path::new("/bin/sh"),
        )
        .unwrap_err();
        assert_eq!(error.code(), TerminalErrorCode::EnvironmentSecret);
    }

    /// policy 只接受 canonical workspace 内的目录，验证正常子目录仍可启动。
    #[test]
    fn policy_accepts_child_directory() {
        let root = std::env::temp_dir().join(format!("ja-terminal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("child")).unwrap();
        let policy = TerminalPolicy::new(&root).unwrap();
        let prepared = policy
            .prepare(&LaunchRequest {
                cwd: Some(PathBuf::from("child")),
                ..LaunchRequest::default()
            })
            .unwrap();
        assert!(prepared.cwd.ends_with("child"));
        fs::remove_dir_all(root).unwrap();
    }
}
