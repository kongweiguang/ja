// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Stable preview errors that never echo URL or WebView diagnostics.

use serde::Serialize;
use std::fmt::{Display, Formatter};

/// UI-safe preview failure categories for IPC and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    DependencyRequest = 20,
    InternalStateUnavailable = 21,
}

/// Error carries only the stable category, never caller input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PreviewError {
    pub code: PreviewErrorCode,
}

impl PreviewError {
    /// Constructs a redacted error suitable for a Tauri command result.
    pub const fn new(code: PreviewErrorCode) -> Self {
        Self { code }
    }

    /// Returns the category used by focused unit tests.
    pub const fn code(self) -> PreviewErrorCode {
        self.code
    }
}

impl Display for PreviewError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.code {
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
            PreviewErrorCode::ErrorTooLong => "preview load error is too long",
            PreviewErrorCode::NavigationBlocked => "preview navigation is blocked",
            PreviewErrorCode::DependencyRequest => "preview host integration is unavailable",
            PreviewErrorCode::InternalStateUnavailable => "preview state is unavailable",
        })
    }
}

impl std::error::Error for PreviewError {}
