// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Versioned, non-sensitive settings DTOs.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const MAX_PROFILES: usize = 128;
pub const MAX_MCP_SERVERS: usize = 128;
pub const MAX_SETTINGS_STRING: usize = 512;

/// A closed provider protocol keeps the settings file from becoming an
/// executable transport configuration supplied by a WebView.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
}

/// Theme is intentionally an enum so unknown values fail closed at the
/// persistence boundary instead of silently changing the user's appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// A credential reference is safe to persist and safe to expose in UI DTOs;
/// the corresponding secret never lives in this value.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub const MAX_LENGTH: usize = 100;

    /// Validates the stable opaque identifier before it can reach the keyring
    /// adapter, preventing arbitrary service/account names from being used.
    pub fn parse(value: &str) -> Result<Self, SettingsModelError> {
        if value.len() > Self::MAX_LENGTH
            || !value.starts_with("cred_")
            || value.len() <= "cred_".len()
            || !value[5..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(SettingsModelError::InvalidCredentialRef);
        }
        Ok(Self(value.to_owned()))
    }

    /// Returns only the opaque reference, never the value stored in the OS
    /// keychain.
    /// Returns only the opaque keychain selector so callers can persist or
    /// compare a reference without ever receiving the underlying secret.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CredentialRef")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    /// Validates deserialized references immediately so an opaque ID cannot
    /// bypass the keychain namespace checks before document validation.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// The provider profile contains only model routing metadata and a keychain
/// reference.  A custom type makes adding a raw `apiKey` field impossible
/// without an explicit schema review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileSetting {
    pub profile_revision: String,
    pub name: String,
    pub provider: String,
    pub protocol: ApiProtocol,
    pub model: String,
    pub base_url: Option<String>,
    pub credential_ref: Option<CredentialRef>,
    #[serde(default)]
    pub supports_vision: bool,
}

/// MCP settings intentionally store an opaque credential reference rather
/// than headers, bearer tokens, or arbitrary process environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerSetting {
    pub mcp_revision: String,
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub protocol_version: String,
    pub credential_ref: Option<CredentialRef>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Window preferences are bounded scalar data and do not contain native
/// handles, paths, or platform-specific command text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindowSettings {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub maximized: bool,
}

/// The persisted document is deliberately small and strict.  Runtime state,
/// secrets, selected absolute paths, and sidecar process handles belong to
/// other native services and must never be folded into this JSON schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsDocument {
    pub schema_version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default = "default_theme")]
    pub theme: ThemePreference,
    #[serde(default)]
    pub active_profile_revision: Option<String>,
    #[serde(default)]
    pub profiles: Vec<ProfileSetting>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSetting>,
    #[serde(default)]
    pub window: WindowSettings,
}

impl Default for SettingsDocument {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            revision: 0,
            theme: ThemePreference::System,
            active_profile_revision: None,
            profiles: Vec::new(),
            mcp_servers: Vec::new(),
            window: WindowSettings::default(),
        }
    }
}

impl WindowSettings {
    pub fn validate(&self) -> Result<(), SettingsModelError> {
        if !(640..=16_384).contains(&self.width) || !(480..=16_384).contains(&self.height) {
            return Err(SettingsModelError::InvalidWindowSize);
        }
        Ok(())
    }
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 820,
            maximized: false,
        }
    }
}

impl SettingsDocument {
    /// Validates all user-editable fields before serialization so malformed
    /// settings cannot be persisted and later interpreted inconsistently.
    pub fn validate(&self) -> Result<(), SettingsModelError> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(SettingsModelError::UnsupportedSchema);
        }
        if self.profiles.len() > MAX_PROFILES || self.mcp_servers.len() > MAX_MCP_SERVERS {
            return Err(SettingsModelError::TooManyEntries);
        }
        if let Some(active) = &self.active_profile_revision {
            validate_text(active)?;
        }
        for profile in &self.profiles {
            validate_text(&profile.profile_revision)?;
            validate_text(&profile.name)?;
            validate_text(&profile.provider)?;
            validate_text(&profile.model)?;
            if matches!(profile.protocol, ApiProtocol::OpenAiResponses) {
                return Err(SettingsModelError::UnsupportedProtocol);
            }
            if let Some(base_url) = &profile.base_url {
                validate_url(base_url)?;
            }
            if let Some(reference) = &profile.credential_ref {
                CredentialRef::parse(reference.as_str())?;
            }
        }
        for server in &self.mcp_servers {
            validate_text(&server.mcp_revision)?;
            validate_text(&server.name)?;
            validate_text(&server.transport)?;
            validate_text(&server.endpoint)?;
            validate_text(&server.protocol_version)?;
            if contains_sensitive_text(&server.endpoint) {
                return Err(SettingsModelError::InvalidText);
            }
            if let Ok(parsed) = url::Url::parse(&server.endpoint)
                && matches!(parsed.scheme(), "http" | "https")
            {
                validate_url(&server.endpoint)?;
            }
            if let Some(reference) = &server.credential_ref {
                CredentialRef::parse(reference.as_str())?;
            }
        }
        self.window.validate()
    }
}

/// Model validation errors use stable categories so callers can report a
/// useful failure without echoing a user path, provider URL, or secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsModelError {
    InvalidCredentialRef,
    InvalidText,
    InvalidUrl,
    InvalidWindowSize,
    TooManyEntries,
    UnsupportedProtocol,
    UnsupportedSchema,
}

impl fmt::Display for SettingsModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidCredentialRef => "invalid credential reference",
            Self::InvalidText => "invalid settings text",
            Self::InvalidUrl => "invalid provider URL",
            Self::InvalidWindowSize => "invalid window size",
            Self::TooManyEntries => "too many settings entries",
            Self::UnsupportedProtocol => "settings protocol is not available in this release",
            Self::UnsupportedSchema => "unsupported settings schema",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SettingsModelError {}

fn default_true() -> bool {
    true
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    820
}

fn default_theme() -> ThemePreference {
    ThemePreference::System
}

/// Rejects control characters and oversized labels before they become a
/// persistence or log-amplification vector.
fn validate_text(value: &str) -> Result<(), SettingsModelError> {
    if value.is_empty()
        || value.len() > MAX_SETTINGS_STRING
        || value.chars().any(|character| character.is_control())
    {
        return Err(SettingsModelError::InvalidText);
    }
    Ok(())
}

/// Restricts provider endpoints to credential-free HTTP(S) URLs so secrets
/// cannot hide in userinfo or common query parameter names.
fn validate_url(value: &str) -> Result<(), SettingsModelError> {
    if value.len() > MAX_SETTINGS_STRING {
        return Err(SettingsModelError::InvalidUrl);
    }
    let parsed = url::Url::parse(value).map_err(|_| SettingsModelError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(SettingsModelError::InvalidUrl);
    }
    Ok(())
}

/// Rejects common inline secret assignments in stdio/MCP endpoint text while
/// still allowing non-secret executable paths and ordinary URL parameters.
fn contains_sensitive_text(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(*character, ':' | '='))
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "token:",
        "token=",
        "secret:",
        "secret=",
        "password:",
        "password=",
        "apikey:",
        "apikey=",
        "authorization:",
        "authorization=",
        "bearer:",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
        || normalized.contains("apikey")
}
