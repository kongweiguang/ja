// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Preview URL 与 WebView callback 策略。
//!
//! 所有入口（用户导航、重定向和新窗口）都必须经过同一个纯策略，避免
//! 只在地址栏校验而让 WebView callback 绕过 scheme/userinfo 边界。

use super::error::{PreviewError, PreviewErrorCode};
use super::model::{
    NavigationSource, PermissionDecision, PreviewLimits, PreviewPermission, PreviewUrl,
};
use url::Url;

/// 新窗口 callback 的默认处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewWindowBehavior {
    /// 继续使用同一策略创建另一个 zero-capability Preview。
    ControlledPreview,
    /// 对没有明确用户操作的新窗口直接拒绝。
    Reject,
}

/// TLS certificate error 的 fail-closed 默认策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateErrorPolicy {
    Reject,
}

/// 拖放策略；远程页面不能把本机路径变成网页输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragDropPolicy {
    Reject,
}

/// URL 经过策略后的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewNavigationDecision {
    Allow { url: PreviewUrl },
    OpenControlled { url: PreviewUrl },
    Reject { code: PreviewErrorCode },
}

/// 只允许 http(s) 的 Preview policy；不包含网络代理或搜索能力。
#[derive(Debug, Clone)]
pub struct PreviewPolicy {
    limits: PreviewLimits,
    new_window: NewWindowBehavior,
    certificate_error: CertificateErrorPolicy,
    drag_drop: DragDropPolicy,
}

impl Default for PreviewPolicy {
    /// 默认拒绝所有会扩大本地能力的 WebView 行为，并支持受控链接窗口。
    fn default() -> Self {
        Self {
            limits: PreviewLimits::default(),
            new_window: NewWindowBehavior::ControlledPreview,
            certificate_error: CertificateErrorPolicy::Reject,
            drag_drop: DragDropPolicy::Reject,
        }
    }
}

impl PreviewPolicy {
    /// 使用默认的 URL/事件预算建立 policy。
    pub fn new() -> Result<Self, PreviewError> {
        Self::with_limits(PreviewLimits::default())
    }

    /// 在 registry 创建前固定并校验资源预算。
    pub fn with_limits(limits: PreviewLimits) -> Result<Self, PreviewError> {
        let limits = limits.validate()?;
        Ok(Self {
            limits,
            ..Self::default()
        })
    }

    /// 配置受控的新窗口策略；窗口仍不能获得 opener 或 Tauri capability。
    pub fn with_new_window_behavior(mut self, behavior: NewWindowBehavior) -> Self {
        self.new_window = behavior;
        self
    }

    /// 返回已经验证的预算副本，供 registry 建立 bounded state。
    pub(crate) fn limits(&self) -> PreviewLimits {
        self.limits
    }

    /// 解析并规范化 URL；localhost/private host 和自定义端口不在 denylist 中。
    pub fn validate_url(&self, raw: &str) -> Result<PreviewUrl, PreviewError> {
        if raw.len() > self.limits.max_url_bytes {
            return Err(PreviewError::new(PreviewErrorCode::UrlTooLong));
        }
        if raw.is_empty() {
            return Err(PreviewError::new(PreviewErrorCode::UrlInvalid));
        }
        if raw
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(PreviewError::new(PreviewErrorCode::UrlControlCharacter));
        }
        // Chromium and URL parsers differ on backslash authority handling; rejecting it
        // keeps redirects from changing the host interpretation between platforms.
        if raw.contains('\\') {
            return Err(PreviewError::new(PreviewErrorCode::UrlInvalid));
        }
        let Some(prefix_len) = strict_http_prefix(raw) else {
            let parsed = Url::parse(raw);
            if parsed
                .as_ref()
                .is_ok_and(|url| !matches!(url.scheme(), "http" | "https"))
            {
                return Err(PreviewError::new(PreviewErrorCode::SchemeNotAllowed));
            }
            return Err(PreviewError::new(PreviewErrorCode::UrlInvalid));
        };
        // The exact prefix consumes the two authority slashes; another slash would
        // change the URL into an empty-host form on some WebView implementations.
        if raw
            .as_bytes()
            .get(prefix_len)
            .is_some_and(|byte| *byte == b'/')
        {
            return Err(PreviewError::new(PreviewErrorCode::UrlInvalid));
        }
        validate_percent_escapes(raw)?;
        let parsed =
            Url::parse(raw).map_err(|_| PreviewError::new(PreviewErrorCode::UrlInvalid))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(PreviewError::new(PreviewErrorCode::SchemeNotAllowed));
        }
        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(PreviewError::new(PreviewErrorCode::HostMissing));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(PreviewError::new(PreviewErrorCode::UserInfoNotAllowed));
        }
        if authority_contains_userinfo(raw) {
            return Err(PreviewError::new(PreviewErrorCode::UserInfoNotAllowed));
        }
        let normalized = parsed.as_str().to_owned();
        if normalized.len() > self.limits.max_url_bytes {
            return Err(PreviewError::new(PreviewErrorCode::UrlTooLong));
        }
        Ok(PreviewUrl::from_normalized(normalized))
    }

    /// 为用户导航、重定向和新窗口统一执行 URL 重验。
    pub fn navigation(
        &self,
        source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewNavigationDecision, PreviewError> {
        let url = self.validate_url(raw_url)?;
        Ok(match source {
            NavigationSource::User | NavigationSource::Redirect => {
                PreviewNavigationDecision::Allow { url }
            }
            NavigationSource::NewWindow => match self.new_window {
                NewWindowBehavior::ControlledPreview => {
                    PreviewNavigationDecision::OpenControlled { url }
                }
                NewWindowBehavior::Reject => PreviewNavigationDecision::Reject {
                    code: PreviewErrorCode::NewWindowBlocked,
                },
            },
        })
    }

    /// TLS 错误默认拒绝，避免把证书绕过变成隐式信任。
    pub fn certificate_error(&self) -> Result<(), PreviewError> {
        match self.certificate_error {
            CertificateErrorPolicy::Reject => {
                Err(PreviewError::new(PreviewErrorCode::CertificateRejected))
            }
        }
    }

    /// 下载默认拒绝，Preview 不承担文件落盘/病毒扫描/用户确认流程。
    pub fn download(&self) -> Result<PermissionDecision, PreviewError> {
        Err(PreviewError::new(PreviewErrorCode::DownloadBlocked))
    }

    /// file chooser 默认拒绝，防止网页把本机路径或文件句柄引入页面。
    pub fn file_chooser(&self) -> Result<PermissionDecision, PreviewError> {
        Err(PreviewError::new(PreviewErrorCode::PermissionDenied))
    }

    /// 摄像头、麦克风、剪贴板等权限全部走同一个拒绝策略。
    pub fn permission(&self, _permission: PreviewPermission) -> PermissionDecision {
        PermissionDecision::Deny
    }

    /// 弹窗默认拒绝；window.open 只有在 navigation callback 中明确走受控窗口。
    pub fn popup(&self) -> Result<(), PreviewError> {
        Err(PreviewError::new(PreviewErrorCode::PopupBlocked))
    }

    /// 拖放默认拒绝，防止本机路径被远程页面读取。
    pub fn drag_drop(&self) -> Result<(), PreviewError> {
        match self.drag_drop {
            DragDropPolicy::Reject => Err(PreviewError::new(PreviewErrorCode::DragDropBlocked)),
        }
    }

    /// 非 HTTP(S) 外链不交给系统 shell，避免自定义协议扩大副作用。
    pub fn external_navigation(&self, raw_url: &str) -> Result<PreviewUrl, PreviewError> {
        self.validate_url(raw_url)
            .map_err(|_| PreviewError::new(PreviewErrorCode::NavigationBlocked))
    }
}

/// 拒绝 URL 中未完整编码或编码控制字符，避免 parser/adapter 解释不一致。
fn validate_percent_escapes(raw: &str) -> Result<(), PreviewError> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(PreviewError::new(PreviewErrorCode::PercentEscapeInvalid));
            }
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            let Some(high) = high else {
                return Err(PreviewError::new(PreviewErrorCode::PercentEscapeInvalid));
            };
            let Some(low) = low else {
                return Err(PreviewError::new(PreviewErrorCode::PercentEscapeInvalid));
            };
            let decoded = (high << 4) | low;
            if decoded < 0x20 || decoded == 0x7f || decoded == b'\\' {
                return Err(PreviewError::new(PreviewErrorCode::UrlControlCharacter));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

/// URL crate 允许空 userinfo；authority 扫描仍拒绝这种容易被误读的形态。
fn authority_contains_userinfo(raw: &str) -> bool {
    let Some(scheme_end) = raw.find("://") else {
        return false;
    };
    let authority_start = scheme_end + 3;
    let authority_end = raw[authority_start..]
        .find(['/', '?', '#'])
        .map_or(raw.len(), |offset| authority_start + offset);
    raw[authority_start..authority_end].contains('@')
}

/// 只允许 ASCII case-insensitive 的 `http://`/`https://` authority 入口。
fn strict_http_prefix(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        return Some(7);
    }
    if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        return Some(8);
    }
    None
}

/// 十六进制字节解析保持在无分配的 URL 校验路径上。
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
