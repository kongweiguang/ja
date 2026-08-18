// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Versioned, non-sensitive settings DTOs.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "openai_chat_completions", alias = "open_ai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "openai_responses", alias = "open_ai_responses")]
    OpenAiResponses,
}

/// The three product-visible access modes are persisted as part of the
/// profile snapshot so replay cannot silently widen a user's selected policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    ReadOnly,
    #[default]
    Workspace,
    FullAccess,
}

/// MCP authentication placement is explicit and secret-free; the credential
/// value itself remains in the OS keyring behind `CredentialRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthKind {
    None,
    Bearer,
    Header,
    Env,
}

/// Structured MCP auth metadata mirrors the frozen Java wire shape while
/// retaining only an opaque keyring reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpAuthSetting {
    pub kind: McpAuthKind,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub credential_ref: Option<CredentialRef>,
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
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub access_mode: AccessMode,
    #[serde(default)]
    pub skill_revisions: Vec<String>,
    /// `None` preserves legacy settings semantics (all enabled definitions);
    /// `Some([])` is an explicit profile with no MCP servers selected.
    #[serde(default)]
    pub mcp_revisions: Option<Vec<String>>,
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
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub query_params: BTreeMap<String, String>,
    /// New settings use this field; `credential_ref` remains a legacy alias.
    #[serde(default)]
    pub auth: Option<McpAuthSetting>,
    #[serde(default)]
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
            validate_revision_identifier(&profile.profile_revision, "profile_")?;
            validate_text(&profile.name)?;
            validate_text(&profile.provider)?;
            validate_text(&profile.model)?;
            validate_revision_list(&profile.skill_revisions, "skill_")?;
            if let Some(revisions) = &profile.mcp_revisions {
                validate_revision_list(revisions, "mcp_")?;
                for revision in revisions {
                    let selected = self
                        .mcp_servers
                        .iter()
                        .find(|server| server.mcp_revision == *revision)
                        .ok_or(SettingsModelError::InvalidText)?;
                    if !selected.enabled {
                        return Err(SettingsModelError::InvalidText);
                    }
                }
            }
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
            validate_revision_identifier(&server.mcp_revision, "mcp_")?;
            validate_text(&server.name)?;
            if !matches!(server.transport.as_str(), "stdio" | "streamable_http") {
                return Err(SettingsModelError::InvalidText);
            }
            validate_text(&server.endpoint)?;
            validate_text(&server.protocol_version)?;
            if !matches!(
                server.protocol_version.as_str(),
                "2024-11-05" | "2025-03-26" | "2025-06-18"
            ) {
                return Err(SettingsModelError::InvalidText);
            }
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
            validate_mcp_shape(server)?;
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

/// Validates selected skill/MCP references without inventing a second catalog
/// or accepting arbitrary provider-owned identifiers.
fn validate_revision_list(values: &[String], prefix: &str) -> Result<(), SettingsModelError> {
    if values.len() > 128 {
        return Err(SettingsModelError::TooManyEntries);
    }
    let mut unique = std::collections::HashSet::with_capacity(values.len());
    for value in values {
        if value.len() < prefix.len() + 1
            || value.len() > 128
            || !value.starts_with(prefix)
            || !value[prefix.len()..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || !unique.insert(value)
        {
            return Err(SettingsModelError::InvalidText);
        }
    }
    Ok(())
}

/// Keeps persisted revisions aligned with the frozen Java identifiers while
/// retaining a single validation rule for profile and MCP definitions.
fn validate_revision_identifier(value: &str, prefix: &str) -> Result<(), SettingsModelError> {
    if value.len() < prefix.len() + 1
        || value.len() > 128
        || !value.starts_with(prefix)
        || !value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(SettingsModelError::InvalidText);
    }
    Ok(())
}

/// Applies the frozen transport/auth matrix before a definition is replayed
/// into Java, keeping arguments and header/query maps secret-free and bounded.
fn validate_mcp_shape(server: &McpServerSetting) -> Result<(), SettingsModelError> {
    if server.args.len() > 64 || server.env.len() > 64 || server.headers.len() > 64 {
        return Err(SettingsModelError::TooManyEntries);
    }
    if server.query_params.len() > 64 {
        return Err(SettingsModelError::TooManyEntries);
    }
    for value in &server.args {
        validate_mcp_value(value)?;
    }
    for (key, value) in server
        .env
        .iter()
        .chain(server.headers.iter())
        .chain(server.query_params.iter())
    {
        if key.is_empty()
            || key.len() > 128
            || contains_sensitive_config_key(key)
            || contains_sensitive_text(value)
        {
            return Err(SettingsModelError::InvalidText);
        }
        validate_mcp_value(value)?;
    }
    if server.transport == "stdio"
        && (!server.headers.is_empty() || !server.query_params.is_empty())
    {
        return Err(SettingsModelError::InvalidText);
    }
    if server.transport == "streamable_http" && (!server.args.is_empty() || !server.env.is_empty())
    {
        return Err(SettingsModelError::InvalidText);
    }
    if let Some(auth) = &server.auth {
        if server.credential_ref.is_some() {
            return Err(SettingsModelError::InvalidCredentialRef);
        }
        match auth.kind {
            McpAuthKind::None => {
                if auth.name.is_some() || auth.credential_ref.is_some() {
                    return Err(SettingsModelError::InvalidCredentialRef);
                }
            }
            McpAuthKind::Env => {
                if server.transport != "stdio"
                    || !valid_env_name(auth.name.as_deref())
                    || auth.credential_ref.is_none()
                    || auth
                        .name
                        .as_ref()
                        .is_some_and(|name| server.env.contains_key(name))
                {
                    return Err(SettingsModelError::InvalidText);
                }
            }
            McpAuthKind::Bearer => {
                if server.transport != "streamable_http"
                    || auth.name.is_some()
                    || auth.credential_ref.is_none()
                    || server
                        .headers
                        .keys()
                        .any(|key| key.eq_ignore_ascii_case("authorization"))
                {
                    return Err(SettingsModelError::InvalidText);
                }
            }
            McpAuthKind::Header => {
                if server.transport != "streamable_http"
                    || !valid_header_name(auth.name.as_deref())
                    || auth.credential_ref.is_none()
                    || auth.name.as_ref().is_some_and(|name| {
                        server
                            .headers
                            .keys()
                            .any(|key| key.eq_ignore_ascii_case(name))
                    })
                {
                    return Err(SettingsModelError::InvalidText);
                }
            }
        }
        if let Some(reference) = &auth.credential_ref {
            CredentialRef::parse(reference.as_str())?;
        }
    }
    Ok(())
}

/// Rejects control, NUL, inline-secret, and oversized MCP values before they
/// can enter a child environment, command argument, or HTTP transport.
fn validate_mcp_value(value: &str) -> Result<(), SettingsModelError> {
    if value.is_empty()
        || value.len() > 4096
        || value.contains('\0')
        || value.chars().any(|character| character.is_control())
        || contains_sensitive_text(value)
    {
        return Err(SettingsModelError::InvalidText);
    }
    Ok(())
}

/// Mirrors Java's key-name denylist so credentials cannot be smuggled through
/// an ordinary MCP environment/header/query map key.
fn contains_sensitive_config_key(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "access_token",
        "accesstoken",
        "authorization",
        "credential",
        "password",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

/// Checks the platform-neutral environment-name grammar used by Java's MCP
/// definition, while leaving the actual secret in the keyring.
fn valid_env_name(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let bytes = value.as_bytes();
    (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// Checks an RFC-token-like HTTP header name before the provider adapter sees
/// it; values still remain opaque configuration and never contain secrets.
fn valid_header_name(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
    })
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
