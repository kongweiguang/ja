// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! One URL policy for preview open, user navigation and redirects.

use super::error::{PreviewError, PreviewErrorCode};
use super::model::{NavigationSource, PreviewLimits, PreviewUrl};
use url::Url;

/// The only navigation decision needed by the first preview release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewNavigationDecision {
    Allow { url: PreviewUrl },
}

/// Validates external HTTP(S) URLs without adding browser automation policy.
#[derive(Debug, Clone)]
pub struct PreviewPolicy {
    limits: PreviewLimits,
}

impl Default for PreviewPolicy {
    /// Keep policy limited to the scheme/authority boundary required by UI.
    fn default() -> Self {
        Self {
            limits: PreviewLimits::default(),
        }
    }
}

impl PreviewPolicy {
    /// Builds the policy after validating all queue budgets.
    pub fn new() -> Result<Self, PreviewError> {
        Self::with_limits(PreviewLimits::default())
    }

    /// Allows tests and future host tuning to provide explicit budgets.
    pub fn with_limits(limits: PreviewLimits) -> Result<Self, PreviewError> {
        Ok(Self {
            limits: limits.validate()?,
        })
    }

    /// Returns the validated bounded budgets for the session registry.
    pub(crate) fn limits(&self) -> PreviewLimits {
        self.limits
    }

    /// Rejects every scheme except HTTP(S) before creating a WebView.
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
            || raw.contains('\\')
        {
            return Err(PreviewError::new(PreviewErrorCode::UrlControlCharacter));
        }
        let Some(prefix_len) = strict_http_prefix(raw) else {
            let parsed = Url::parse(raw);
            return if parsed
                .as_ref()
                .is_ok_and(|url| !matches!(url.scheme(), "http" | "https"))
            {
                Err(PreviewError::new(PreviewErrorCode::SchemeNotAllowed))
            } else {
                Err(PreviewError::new(PreviewErrorCode::UrlInvalid))
            };
        };
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
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || authority_has_userinfo(raw)
        {
            return Err(PreviewError::new(PreviewErrorCode::UserInfoNotAllowed));
        }
        let normalized = parsed.as_str().to_owned();
        if normalized.len() > self.limits.max_url_bytes {
            return Err(PreviewError::new(PreviewErrorCode::UrlTooLong));
        }
        Ok(PreviewUrl::from_normalized(normalized))
    }

    /// Revalidates a user/redirect callback through the same URL parser.
    pub fn navigation(
        &self,
        _source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewNavigationDecision, PreviewError> {
        Ok(PreviewNavigationDecision::Allow {
            url: self.validate_url(raw_url)?,
        })
    }
}

/// Rejects malformed percent escapes and encoded control/backslash bytes.
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

/// Rejects userinfo forms that a browser and URL parser could display differently.
fn authority_has_userinfo(raw: &str) -> bool {
    let Some(scheme_end) = raw.find("://") else {
        return false;
    };
    let start = scheme_end + 3;
    let end = raw[start..]
        .find(['/', '?', '#'])
        .map_or(raw.len(), |offset| start + offset);
    raw[start..end].contains('@')
}

/// Restricts the authority entry point to ASCII case-insensitive HTTP(S).
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

/// Parses a hexadecimal escape nibble without allocation.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
