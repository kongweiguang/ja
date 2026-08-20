// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Small typed bridge for the settings page's existing AgentScope methods.
//!
//! This is intentionally not a generic RPC tunnel: the WebView can only ask
//! for Skills/MCP projections that already exist in the Java contract.

use super::{RuntimeCommandError, RuntimeHost};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_METHOD: usize = 64;
const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ROWS: usize = 256;
const MAX_TOOLS: usize = 10_000;
const MAX_REVISION: usize = 128;

/// The only sidecar methods needed by the first Settings surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsQueryMethod {
    SkillList,
    McpList,
    McpTest,
    McpToolsRead,
}

impl SettingsQueryMethod {
    /// Converts the closed product allow-list into the Java wire method name.
    pub(crate) fn parse(value: &str) -> Result<Self, RuntimeCommandError> {
        match value {
            "skill/list" => Ok(Self::SkillList),
            "mcp/list" => Ok(Self::McpList),
            "mcp/test" => Ok(Self::McpTest),
            "mcp/tools/read" => Ok(Self::McpToolsRead),
            _ => Err(RuntimeCommandError::invalid_params()),
        }
    }

    /// Returns whether this query may receive the Java secret/resolve request.
    pub(crate) const fn needs_server_request_handler(self) -> bool {
        matches!(self, Self::McpTest)
    }

    /// Returns the frozen wire spelling used by the bridge actor.
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::SkillList => "skill/list",
            Self::McpList => "mcp/list",
            Self::McpTest => "mcp/test",
            Self::McpToolsRead => "mcp/tools/read",
        }
    }
}

/// Tauri input keeps method and params together so the native command has one
/// validation boundary while still refusing arbitrary method names.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsQueryInput {
    pub method: String,
    pub params: Value,
}

impl SettingsQueryInput {
    /// Validates only bounded identifiers; endpoint, command, and secret data
    /// never enter this query surface.
    pub(crate) fn validate(self) -> Result<(SettingsQueryMethod, Value), RuntimeCommandError> {
        if self.method.is_empty() || self.method.len() > MAX_METHOD {
            return Err(RuntimeCommandError::invalid_params());
        }
        let method = SettingsQueryMethod::parse(&self.method)?;
        let object = self
            .params
            .as_object()
            .ok_or_else(RuntimeCommandError::invalid_params)?;
        match method {
            SettingsQueryMethod::SkillList | SettingsQueryMethod::McpList => {}
            SettingsQueryMethod::McpTest => {
                required_id(object, "mcpRevision", "mcp_")?;
                if let Some(profile) = object.get("profileRevision") {
                    let value = profile
                        .as_str()
                        .ok_or_else(RuntimeCommandError::invalid_params)?;
                    validate_id(value, "profile_", MAX_REVISION)?;
                }
            }
            SettingsQueryMethod::McpToolsRead => {
                required_id(object, "mcpRevision", "mcp_")?;
            }
        }
        Ok((method, Value::Object(object.clone())))
    }
}

/// Reads one allow-listed query through the Ready generation and bounds its
/// result before it reaches the Tauri serializer.
#[tauri::command]
pub fn ja_runtime_query(
    input: SettingsQueryInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<Value, RuntimeCommandError> {
    let (method, params) = input.validate()?;
    let value = state.settings_query(method, params)?;
    validate_result(method, value)
}

/// Validates result shape and size without inventing a second DTO hierarchy.
fn validate_result(
    method: SettingsQueryMethod,
    value: Value,
) -> Result<Value, RuntimeCommandError> {
    let object = value
        .as_object()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let max_rows = match method {
        SettingsQueryMethod::SkillList | SettingsQueryMethod::McpList => MAX_ROWS,
        SettingsQueryMethod::McpTest => 0,
        SettingsQueryMethod::McpToolsRead => MAX_TOOLS,
    };
    let array_name = match method {
        SettingsQueryMethod::SkillList => Some("skills"),
        SettingsQueryMethod::McpList => Some("servers"),
        SettingsQueryMethod::McpToolsRead => Some("tools"),
        SettingsQueryMethod::McpTest => None,
    };
    if let Some(name) = array_name {
        let rows = object
            .get(name)
            .and_then(Value::as_array)
            .ok_or_else(RuntimeCommandError::unavailable)?;
        if rows.len() > max_rows {
            return Err(RuntimeCommandError::unavailable());
        }
    }
    let encoded = serde_json::to_vec(&value).map_err(|_| RuntimeCommandError::unavailable())?;
    if encoded.len() > MAX_RESULT_BYTES {
        return Err(RuntimeCommandError::unavailable());
    }
    Ok(value)
}

/// Requires one revision with the same bounded alphabet used by all native IDs.
fn required_id(
    object: &Map<String, Value>,
    field: &str,
    prefix: &str,
) -> Result<(), RuntimeCommandError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(RuntimeCommandError::invalid_params)?;
    validate_id(value, prefix, MAX_REVISION)
}

/// Rejects paths, URLs, and arbitrary request handles from the query DTO.
fn validate_id(value: &str, prefix: &str, max: usize) -> Result<(), RuntimeCommandError> {
    if !value.starts_with(prefix)
        || value.len() > max
        || value.len() == prefix.len()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The native allow-list must reject a method that is not a Settings projection.
    #[test]
    fn rejects_arbitrary_method() {
        let input = SettingsQueryInput {
            method: "turn/start".to_owned(),
            params: json!({}),
        };
        assert!(input.validate().is_err());
    }

    /// MCP test parameters accept only opaque revision identities, never paths or commands.
    #[test]
    fn validates_mcp_test_revisions() {
        let input = SettingsQueryInput {
            method: "mcp/test".to_owned(),
            params: json!({"mcpRevision": "mcp_docs", "profileRevision": "profile_default"}),
        };
        assert!(input.validate().is_ok());
        let invalid = SettingsQueryInput {
            method: "mcp/test".to_owned(),
            params: json!({"mcpRevision": "C:\\workspace"}),
        };
        assert!(invalid.validate().is_err());
    }

    /// Result arrays are bounded before serialization can amplify a sidecar response.
    #[test]
    fn bounds_projection_rows() {
        let oversized = json!({"skills": vec![json!({}); MAX_ROWS + 1]});
        assert!(validate_result(SettingsQueryMethod::SkillList, oversized).is_err());
    }
}
