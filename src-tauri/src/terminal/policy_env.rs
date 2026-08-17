// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端环境变量 allowlist 与 bounded budget。
//!
//! 环境策略独立出来，是为了让 shell policy 的路径 containment 与 env
//! secret/dangerous-key 判断可以分别演进而不增加 session 的隐式规则。

use super::super::error::{TerminalError, TerminalErrorCode};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

const SAFE_OVERRIDE_ENV: &[&str] = &[
    "COLORTERM",
    "CLICOLOR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NO_COLOR",
    "TERM",
    "TERM_PROGRAM",
    "TZ",
];
const BASE_ENV: &[&str] = &[
    "COLORTERM",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "PATH",
    "PATHEXT",
    "SHELL",
    "SystemRoot",
    "TEMP",
    "TERM",
    "TERM_PROGRAM",
    "TMP",
    "USER",
    "USERPROFILE",
    "WINDIR",
];

pub(super) const MAX_ENV_VARS: usize = 64;
pub(super) const MAX_ENV_KEY_BYTES: usize = 128;
pub(super) const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
pub(super) const MAX_ENV_TOTAL_BYTES: usize = 256 * 1024;

/// 复制最小 host 环境，防止 portable-pty 默认继承全部 secret。
pub(super) fn build_environment(
    overrides: &BTreeMap<String, String>,
    shell: &Path,
) -> Result<BTreeMap<OsString, OsString>, TerminalError> {
    let mut result = BTreeMap::new();
    for key in BASE_ENV {
        if let Some(value) = std::env::var_os(key) {
            result.insert(OsString::from(key), value);
        }
    }
    result.insert(OsString::from("SHELL"), shell.as_os_str().to_owned());
    for (key, value) in overrides {
        validate_env_key(key)?;
        if is_sensitive_env_key(key) {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentSecret));
        }
        if !SAFE_OVERRIDE_ENV.contains(&key.as_str()) {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentNotAllowed));
        }
        if value.len() > MAX_ENV_VALUE_BYTES || value.contains('\0') {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentNotAllowed));
        }
        result.insert(OsString::from(key), OsString::from(value));
    }
    validate_environment_map(&result)?;
    Ok(result)
}

/// Validate the assembled environment once more before it reaches the PTY.
fn validate_environment_map(
    environment: &BTreeMap<OsString, OsString>,
) -> Result<(), TerminalError> {
    if environment.len() > MAX_ENV_VARS {
        return Err(TerminalError::new(TerminalErrorCode::EnvironmentLimit));
    }
    let mut total = 0_usize;
    for (key, value) in environment {
        let key_text = key.to_string_lossy();
        validate_env_key(&key_text)?;
        if is_sensitive_env_key(&key_text) {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentSecret));
        }
        if is_dangerous_env_key(&key_text) {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentDangerous));
        }
        if !BASE_ENV.contains(&key_text.as_ref()) && !SAFE_OVERRIDE_ENV.contains(&key_text.as_ref())
        {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentNotAllowed));
        }
        let key_bytes = key_text.len();
        let value_bytes = value.to_string_lossy().len();
        if key_bytes > MAX_ENV_KEY_BYTES || value_bytes > MAX_ENV_VALUE_BYTES {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentLimit));
        }
        total = total
            .checked_add(key_bytes)
            .and_then(|value| value.checked_add(value_bytes))
            .ok_or(TerminalError::new(TerminalErrorCode::EnvironmentLimit))?;
        if total > MAX_ENV_TOTAL_BYTES {
            return Err(TerminalError::new(TerminalErrorCode::EnvironmentLimit));
        }
    }
    Ok(())
}

/// 拒绝会改变 Windows/Unix 环境解析语义的特殊 key。
fn validate_env_key(key: &str) -> Result<(), TerminalError> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(TerminalError::new(TerminalErrorCode::EnvironmentNotAllowed));
    }
    Ok(())
}

/// 以 key 名称做保守拒绝，避免 secret 在请求或子进程中扩散。
fn is_sensitive_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "COOKIE",
        "AUTH",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

/// Loader injection and shell startup hooks are not user-configurable env
/// overrides because they can change which code runs before a command.
fn is_dangerous_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "BASH_ENV",
        "ENV",
        "ZDOTDIR",
        "PROMPT_COMMAND",
    ]
    .iter()
    .any(|marker| upper == *marker)
}
