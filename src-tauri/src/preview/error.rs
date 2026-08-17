// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Preview 对外稳定错误。
//!
//! 错误只携带可枚举分类，避免把用户输入的 URL、证书诊断、页面标题或
//! 站点返回内容带进 IPC/日志；详细诊断应留在真正的 host adapter 边界。

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// Preview caller 可以稳定处理的失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum PreviewErrorCode {
    InvalidConfig = 1,
    UrlTooLong = 2,
    UrlControlCharacter = 3,
    UrlInvalid = 4,
    SchemeNotAllowed = 5,
    HostMissing = 6,
    UserInfoNotAllowed = 7,
    PercentEscapeInvalid = 8,
    SessionLimit = 9,
    SessionNotFound = 10,
    SessionClosed = 11,
    StaleGeneration = 12,
    GenerationExhausted = 13,
    SequenceExhausted = 14,
    EventPayloadTooLarge = 15,
    EventQueueFull = 16,
    TitleTooLong = 17,
    ErrorTooLong = 18,
    NavigationBlocked = 19,
    NewWindowBlocked = 20,
    CertificateRejected = 21,
    DownloadBlocked = 22,
    PermissionDenied = 23,
    PopupBlocked = 24,
    DragDropBlocked = 25,
    SiteDataClearPending = 26,
    SiteDataClearStale = 27,
    SiteDataClearFailed = 28,
    SiteDataClearExpired = 29,
    DependencyRequest = 30,
    InternalStateUnavailable = 31,
}

/// 不包含输入原文的 Preview 领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewError {
    code: PreviewErrorCode,
}

impl PreviewError {
    /// 只创建稳定分类，防止底层 URL/WebView 错误穿透到 UI。
    pub const fn new(code: PreviewErrorCode) -> Self {
        Self { code }
    }

    /// 提供给 IPC 映射和测试的稳定代码。
    pub const fn code(self) -> PreviewErrorCode {
        self.code
    }
}

impl Display for PreviewError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self.code {
            PreviewErrorCode::InvalidConfig => "preview configuration is invalid",
            PreviewErrorCode::UrlTooLong => "preview URL is too long",
            PreviewErrorCode::UrlControlCharacter => "preview URL contains a control character",
            PreviewErrorCode::UrlInvalid => "preview URL is invalid",
            PreviewErrorCode::SchemeNotAllowed => "preview URL scheme is not allowed",
            PreviewErrorCode::HostMissing => "preview URL host is missing",
            PreviewErrorCode::UserInfoNotAllowed => "preview URL user info is not allowed",
            PreviewErrorCode::PercentEscapeInvalid => "preview URL percent escape is invalid",
            PreviewErrorCode::SessionLimit => "preview session limit reached",
            PreviewErrorCode::SessionNotFound => "preview session was not found",
            PreviewErrorCode::SessionClosed => "preview session is closed",
            PreviewErrorCode::StaleGeneration => "preview callback generation is stale",
            PreviewErrorCode::GenerationExhausted => "preview generation is exhausted",
            PreviewErrorCode::SequenceExhausted => "preview event sequence is exhausted",
            PreviewErrorCode::EventPayloadTooLarge => "preview event payload is too large",
            PreviewErrorCode::EventQueueFull => "preview event queue is full",
            PreviewErrorCode::TitleTooLong => "preview title is too long",
            PreviewErrorCode::ErrorTooLong => "preview error is too long",
            PreviewErrorCode::NavigationBlocked => "preview navigation is blocked",
            PreviewErrorCode::NewWindowBlocked => "preview new window is blocked",
            PreviewErrorCode::CertificateRejected => "preview certificate error was rejected",
            PreviewErrorCode::DownloadBlocked => "preview download is blocked",
            PreviewErrorCode::PermissionDenied => "preview permission is denied",
            PreviewErrorCode::PopupBlocked => "preview popup is blocked",
            PreviewErrorCode::DragDropBlocked => "preview drag and drop is blocked",
            PreviewErrorCode::SiteDataClearPending => "preview site data clear is already pending",
            PreviewErrorCode::SiteDataClearStale => "preview site data clear request is stale",
            PreviewErrorCode::SiteDataClearFailed => "preview site data clear failed",
            PreviewErrorCode::SiteDataClearExpired => "preview site data clear deadline expired",
            PreviewErrorCode::DependencyRequest => "preview host integration is not wired",
            PreviewErrorCode::InternalStateUnavailable => "preview state is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PreviewError {}
