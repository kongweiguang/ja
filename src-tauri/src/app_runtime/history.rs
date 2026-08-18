// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Typed history commands for the small `ja-rpc/v1` desktop surface.
//!
//! The Java sidecar remains the history owner.  This module only validates the
//! four frozen request shapes and projects their bounded JSON results so the
//! WebView cannot turn the runtime bridge into a generic RPC tunnel.

use super::{RuntimeCommandError, RuntimeHost};
use serde::de::{DeserializeOwned, Error as SerdeError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_WORKSPACE_ID: usize = 99;
const MAX_THREAD_ID: usize = 100;
const MAX_PROFILE_REVISION: usize = 104;
const MAX_TURN_ID: usize = 101;
const MAX_ITEM_ID: usize = 101;
const MAX_SERVER_INSTANCE_ID: usize = 101;
const MAX_TITLE: usize = 512;
const MAX_DISPLAY_NAME: usize = 256;
const MAX_ROOT_PATH: usize = 4096;
const MAX_ITEM_TEXT: usize = 1_048_576;
const MAX_CURSOR: usize = 256;
const MAX_LIST_LIMIT: u32 = 500;
const MAX_READ_LIMIT: u32 = 1_000;
const MAX_RESULT_ROWS: usize = 500;
const MAX_RESULT_ITEMS: usize = 10_000;
const MAX_RESULT_EVENTS: usize = 10_000;
const MAX_SEQ: u64 = 9_007_199_254_740_991;

/// Internal allow-list shared by the bridge actor; keeping it as an enum
/// prevents any caller from selecting an arbitrary Java method name.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HistoryMethod {
    WorkspaceList,
    ThreadCreate,
    ThreadList,
    ThreadRead,
}

impl HistoryMethod {
    /// Returns the frozen wire method name at the final native boundary.
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::WorkspaceList => "workspace/list",
            Self::ThreadCreate => "thread/create",
            Self::ThreadList => "thread/list",
            Self::ThreadRead => "thread/read",
        }
    }
}

/// Requests the current workspace history projection without selecting an
/// arbitrary root; the configured sidecar generation remains authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListInput {
    #[serde(default)]
    pub include_archived: bool,
}

/// Creates one thread beneath the configured workspace identity.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateInput {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_revision: Option<String>,
    #[serde(skip)]
    invalid_optional: bool,
}

impl<'de> Deserialize<'de> for ThreadCreateInput {
    /// Preserves an explicit null marker so Tauri commands can return the
    /// stable `INVALID_PARAMS` error instead of leaking serde's argument error.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let object = deserialize_input_object(deserializer)?;
        let mut invalid_optional = false;
        let workspace_id = read_required_field(&object, "workspaceId")?;
        let title = read_optional_field(&object, "title", &mut invalid_optional)?;
        let profile_revision =
            read_optional_field(&object, "profileRevision", &mut invalid_optional)?;
        Ok(Self {
            workspace_id,
            title,
            profile_revision,
            invalid_optional,
        })
    }
}

/// Lists the bounded thread projection for one workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListInput {
    pub workspace_id: String,
    #[serde(default)]
    pub include_archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip)]
    invalid_optional: bool,
}

impl<'de> Deserialize<'de> for ThreadListInput {
    /// Keeps optional null handling inside the typed DTO while ignoring
    /// compatible minor fields that are not part of this frozen request.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let object = deserialize_input_object(deserializer)?;
        let mut invalid_optional = false;
        let workspace_id = read_required_field(&object, "workspaceId")?;
        let include_archived = read_default_field(&object, "includeArchived", false)?;
        let limit = read_optional_field(&object, "limit", &mut invalid_optional)?;
        let cursor = read_optional_field(&object, "cursor", &mut invalid_optional)?;
        Ok(Self {
            workspace_id,
            include_archived,
            limit,
            cursor,
            invalid_optional,
        })
    }
}

/// The only snapshot view currently admitted by `thread/read`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadReadView {
    Snapshot,
}

/// Reads one durable thread snapshot with an optional sequence boundary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadInput {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ThreadReadView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip)]
    invalid_optional: bool,
}

impl<'de> Deserialize<'de> for ThreadReadInput {
    /// Converts explicit nulls into a validation marker while retaining the
    /// regular typed values needed by the frozen `thread/read` method.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let object = deserialize_input_object(deserializer)?;
        let mut invalid_optional = false;
        let thread_id = read_required_field(&object, "threadId")?;
        let view = read_optional_field(&object, "view", &mut invalid_optional)?;
        let after_seq = read_optional_field(&object, "afterSeq", &mut invalid_optional)?;
        let limit = read_optional_field(&object, "limit", &mut invalid_optional)?;
        Ok(Self {
            thread_id,
            view,
            after_seq,
            limit,
            invalid_optional,
        })
    }
}

/// Projects one persisted workspace without exposing Rust or JDBC types.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
    pub workspace_id: String,
    pub display_name: String,
    pub root_path: String,
    pub trust: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

/// Result of the workspace list command; the sidecar may add a cursor later.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListResult {
    pub workspaces: Vec<WorkspaceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Projects one durable thread used by both list and read views.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDto {
    pub thread_id: String,
    pub workspace_id: String,
    pub title: String,
    pub status: String,
    pub last_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
}

/// Result of the thread list command, retaining the opaque continuation cursor.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResult {
    pub threads: Vec<ThreadDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Result of a snapshot read. Items intentionally remain JSON because their
/// kind-specific metadata is owned by the AgentScope/Java event projection.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResult {
    pub server_instance_id: String,
    pub thread: ThreadDto,
    pub items: Vec<ThreadItemDto>,
    pub snapshot_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<Map<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_seq: Option<u64>,
}

/// Keeps required item identity/status typed while retaining future fields
/// without forwarding unknown request fields back into Java.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItemDto {
    pub item_id: String,
    pub turn_id: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Reads only an object-shaped input and deliberately discards unknown minor
/// fields so they can never be forwarded to the Java sidecar.
fn deserialize_input_object<'de, D>(deserializer: D) -> Result<Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| D::Error::custom("history input must be an object"))
}

/// Reads a required JSON field through serde's normal type conversion while
/// keeping malformed input at the command's typed-parameter boundary.
fn read_required_field<T, E>(object: &Map<String, Value>, name: &str) -> Result<T, E>
where
    T: DeserializeOwned,
    E: SerdeError,
{
    object
        .get(name)
        .cloned()
        .ok_or_else(|| E::custom(format!("missing required field: {name}")))
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| E::custom(error.to_string()))
        })
}

/// Reads an optional field and records explicit null for stable command-level
/// `INVALID_PARAMS`; omission remains distinct and is serialized as absent.
fn read_optional_field<T, E>(
    object: &Map<String, Value>,
    name: &str,
    invalid_optional: &mut bool,
) -> Result<Option<T>, E>
where
    T: DeserializeOwned,
    E: SerdeError,
{
    match object.get(name) {
        None => Ok(None),
        Some(Value::Null) => {
            *invalid_optional = true;
            Ok(None)
        }
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| E::custom(error.to_string())),
    }
}

/// Applies the schema default for a non-optional scalar input field.
fn read_default_field<T, E>(object: &Map<String, Value>, name: &str, default: T) -> Result<T, E>
where
    T: DeserializeOwned,
    E: SerdeError,
{
    match object.get(name) {
        None => Ok(default),
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|error| E::custom(error.to_string()))
        }
    }
}

/// Lists workspaces through the configured Ready generation, even though the
/// request has no workspace id and therefore cannot bypass configuration.
#[tauri::command]
pub fn ja_workspace_list(
    input: WorkspaceListInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<WorkspaceListResult, RuntimeCommandError> {
    let params = serde_json::to_value(input).map_err(|_| RuntimeCommandError::invalid_params())?;
    let result = state.history_request(HistoryMethod::WorkspaceList, params)?;
    parse_workspace_list_result(result)
}

/// Creates a thread while keeping title/profile fields bounded at the IPC edge.
#[tauri::command]
pub fn ja_thread_create(
    input: ThreadCreateInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<ThreadCreateResult, RuntimeCommandError> {
    validate_thread_create(&input)?;
    let params = serde_json::to_value(input).map_err(|_| RuntimeCommandError::invalid_params())?;
    let result = state.history_request(HistoryMethod::ThreadCreate, params)?;
    parse_thread_create_result(result)
}

/// Lists threads using the sidecar's durable ordering and bounded page size.
#[tauri::command]
pub fn ja_thread_list(
    input: ThreadListInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<ThreadListResult, RuntimeCommandError> {
    validate_thread_list(&input)?;
    let params = serde_json::to_value(input).map_err(|_| RuntimeCommandError::invalid_params())?;
    let result = state.history_request(HistoryMethod::ThreadList, params)?;
    parse_thread_list_result(result)
}

/// Reads one snapshot and leaves item details extensible for future AgentScope
/// item kinds without creating a second Rust domain hierarchy.
#[tauri::command]
pub fn ja_thread_read(
    input: ThreadReadInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<ThreadReadResult, RuntimeCommandError> {
    validate_thread_read(&input)?;
    let params = serde_json::to_value(input).map_err(|_| RuntimeCommandError::invalid_params())?;
    let result = state.history_request(HistoryMethod::ThreadRead, params)?;
    parse_thread_read_result(result)
}

/// The create result is a separate wrapper because the v1 schema nests the
/// thread under `result.thread`, not alongside list rows.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateResult {
    pub thread: ThreadDto,
}

/// Parses and bounds the workspace list instead of trusting a sidecar JSON
/// object to satisfy the desktop result contract by accident.
fn parse_workspace_list_result(value: Value) -> Result<WorkspaceListResult, RuntimeCommandError> {
    let object = require_object(value)?;
    let rows = required_array(&object, "workspaces")?;
    if rows.len() > MAX_RESULT_ROWS {
        return Err(RuntimeCommandError::unavailable());
    }
    let workspaces = rows
        .iter()
        .map(parse_workspace)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = optional_text(&object, "nextCursor", MAX_CURSOR)?;
    Ok(WorkspaceListResult {
        workspaces,
        next_cursor,
    })
}

/// Parses the nested thread create result while retaining only the stable
/// thread projection used by the left navigation.
fn parse_thread_create_result(value: Value) -> Result<ThreadCreateResult, RuntimeCommandError> {
    let object = require_object(value)?;
    let thread = parse_thread(required_value(&object, "thread")?)?;
    Ok(ThreadCreateResult { thread })
}

/// Parses and bounds the thread list result using the same row validator as
/// thread creation/read so status and identity rules cannot drift.
fn parse_thread_list_result(value: Value) -> Result<ThreadListResult, RuntimeCommandError> {
    let object = require_object(value)?;
    let rows = required_array(&object, "threads")?;
    if rows.len() > MAX_RESULT_ROWS {
        return Err(RuntimeCommandError::unavailable());
    }
    let threads = rows
        .iter()
        .map(parse_thread)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = optional_text(&object, "nextCursor", MAX_CURSOR)?;
    Ok(ThreadListResult {
        threads,
        next_cursor,
    })
}

/// Parses a snapshot result with explicit array, safe-integer, object-event,
/// and item-core checks before it reaches the WebView.
fn parse_thread_read_result(value: Value) -> Result<ThreadReadResult, RuntimeCommandError> {
    let object = require_object(value)?;
    let server_instance_id = required_text(&object, "serverInstanceId", MAX_SERVER_INSTANCE_ID)?;
    validate_id(&server_instance_id, "srv_", MAX_SERVER_INSTANCE_ID)?;
    let thread = parse_thread(required_value(&object, "thread")?)?;
    let item_values = required_array(&object, "items")?;
    if item_values.len() > MAX_RESULT_ITEMS {
        return Err(RuntimeCommandError::unavailable());
    }
    let items = item_values
        .iter()
        .map(parse_item)
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot_seq = required_safe_seq(&object, "snapshotSeq", 0)?;
    let events = match object.get("events") {
        None => None,
        Some(Value::Null) => return Err(RuntimeCommandError::unavailable()),
        Some(Value::Array(values)) if values.len() <= MAX_RESULT_EVENTS => Some(
            values
                .iter()
                .map(|value| {
                    value
                        .as_object()
                        .cloned()
                        .ok_or_else(RuntimeCommandError::unavailable)
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(RuntimeCommandError::unavailable()),
    };
    let next_seq = optional_safe_seq(&object, "nextSeq", 0)?;
    Ok(ThreadReadResult {
        server_instance_id,
        thread,
        items,
        snapshot_seq,
        events,
        next_seq,
    })
}

/// Validates the required workspace fields, including path/trust boundaries
/// that serde alone cannot express without leaking malformed values forward.
fn parse_workspace(value: &Value) -> Result<WorkspaceDto, RuntimeCommandError> {
    let object = value
        .as_object()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let workspace_id = required_text(object, "workspaceId", MAX_WORKSPACE_ID)?;
    validate_id(&workspace_id, "ws_", MAX_WORKSPACE_ID)?;
    let display_name = required_text(object, "displayName", MAX_DISPLAY_NAME)?;
    let root_path = required_text(object, "rootPath", MAX_ROOT_PATH)?;
    if root_path.is_empty() {
        return Err(RuntimeCommandError::unavailable());
    }
    let trust = required_text(object, "trust", 16)?;
    if !matches!(trust.as_str(), "untrusted" | "trusted") {
        return Err(RuntimeCommandError::unavailable());
    }
    let archived = optional_bool(object, "archived")?;
    Ok(WorkspaceDto {
        workspace_id,
        display_name,
        root_path,
        trust,
        archived,
    })
}

/// Validates a thread projection and all optional active-turn identity data.
fn parse_thread(value: &Value) -> Result<ThreadDto, RuntimeCommandError> {
    let object = value
        .as_object()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let thread_id = required_text(object, "threadId", MAX_THREAD_ID)?;
    validate_id(&thread_id, "thr_", MAX_THREAD_ID)?;
    let workspace_id = required_text(object, "workspaceId", MAX_WORKSPACE_ID)?;
    validate_id(&workspace_id, "ws_", MAX_WORKSPACE_ID)?;
    let title = required_text(object, "title", MAX_TITLE)?;
    let status = required_text(object, "status", 32)?;
    if !matches!(
        status.as_str(),
        "idle" | "running" | "waiting_approval" | "archived"
    ) {
        return Err(RuntimeCommandError::unavailable());
    }
    let last_seq = required_safe_seq(object, "lastSeq", 0)?;
    let active_turn_id = match object.get("activeTurnId") {
        None => None,
        Some(Value::Null) => return Err(RuntimeCommandError::unavailable()),
        Some(value) => {
            let value = value
                .as_str()
                .ok_or_else(RuntimeCommandError::unavailable)?;
            validate_id(value, "turn_", MAX_TURN_ID)?;
            Some(value.to_owned())
        }
    };
    Ok(ThreadDto {
        thread_id,
        workspace_id,
        title,
        status,
        last_seq,
        active_turn_id,
    })
}

/// Keeps item core fields strict while preserving unknown minor-version fields
/// in a flattened extension map for read-only UI display.
fn parse_item(value: &Value) -> Result<ThreadItemDto, RuntimeCommandError> {
    let object = value
        .as_object()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    let item_id = required_text(object, "itemId", MAX_ITEM_ID)?;
    validate_id(&item_id, "item_", MAX_ITEM_ID)?;
    let turn_id = required_text(object, "turnId", MAX_TURN_ID)?;
    validate_id(&turn_id, "turn_", MAX_TURN_ID)?;
    let kind = required_text(object, "kind", 32)?;
    if !matches!(
        kind.as_str(),
        "user_message"
            | "agent_message"
            | "commentary"
            | "tool_call"
            | "command"
            | "file_change"
            | "approval"
    ) {
        return Err(RuntimeCommandError::unavailable());
    }
    let status = required_text(object, "status", 32)?;
    if !matches!(
        status.as_str(),
        "started" | "in_progress" | "completed" | "failed" | "cancelled"
    ) {
        return Err(RuntimeCommandError::unavailable());
    }
    let title = optional_text(object, "title", MAX_TITLE)?;
    let text = optional_text(object, "text", MAX_ITEM_TEXT)?;
    let metadata = match object.get("metadata") {
        None => None,
        Some(Value::Null) => return Err(RuntimeCommandError::unavailable()),
        Some(value) => Some(
            value
                .as_object()
                .cloned()
                .ok_or_else(RuntimeCommandError::unavailable)?,
        ),
    };
    let mut extra = object.clone();
    for key in [
        "itemId", "turnId", "kind", "status", "title", "text", "metadata",
    ] {
        extra.remove(key);
    }
    Ok(ThreadItemDto {
        item_id,
        turn_id,
        kind,
        status,
        title,
        text,
        metadata,
        extra,
    })
}

fn require_object(value: Value) -> Result<Map<String, Value>, RuntimeCommandError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(RuntimeCommandError::unavailable)
}

fn required_value<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Value, RuntimeCommandError> {
    object.get(key).ok_or_else(RuntimeCommandError::unavailable)
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Vec<Value>, RuntimeCommandError> {
    required_value(object, key)?
        .as_array()
        .ok_or_else(RuntimeCommandError::unavailable)
}

fn required_text(
    object: &Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<String, RuntimeCommandError> {
    let value = required_value(object, key)?
        .as_str()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    validate_text_result(value, max)?;
    Ok(value.to_owned())
}

fn optional_text(
    object: &Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<Option<String>, RuntimeCommandError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(RuntimeCommandError::unavailable)?;
    validate_text_result(value, max)?;
    Ok(Some(value.to_owned()))
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, RuntimeCommandError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(RuntimeCommandError::unavailable)
}

fn required_safe_seq(
    object: &Map<String, Value>,
    key: &str,
    minimum: u64,
) -> Result<u64, RuntimeCommandError> {
    let value = required_value(object, key)?
        .as_u64()
        .filter(|value| (minimum..=MAX_SEQ).contains(value))
        .ok_or_else(RuntimeCommandError::unavailable)?;
    Ok(value)
}

fn optional_safe_seq(
    object: &Map<String, Value>,
    key: &str,
    minimum: u64,
) -> Result<Option<u64>, RuntimeCommandError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    value
        .as_u64()
        .filter(|value| (minimum..=MAX_SEQ).contains(value))
        .map(Some)
        .ok_or_else(RuntimeCommandError::unavailable)
}

fn validate_text_result(value: &str, max: usize) -> Result<(), RuntimeCommandError> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(RuntimeCommandError::unavailable());
    }
    Ok(())
}

fn validate_thread_create(input: &ThreadCreateInput) -> Result<(), RuntimeCommandError> {
    if input.invalid_optional {
        return Err(RuntimeCommandError::invalid_params());
    }
    validate_id(&input.workspace_id, "ws_", MAX_WORKSPACE_ID)?;
    if let Some(title) = input.title.as_deref() {
        validate_text(title, MAX_TITLE)?;
    }
    if let Some(profile) = input.profile_revision.as_deref() {
        validate_id(profile, "profile_", MAX_PROFILE_REVISION)?;
    }
    Ok(())
}

fn validate_thread_list(input: &ThreadListInput) -> Result<(), RuntimeCommandError> {
    if input.invalid_optional {
        return Err(RuntimeCommandError::invalid_params());
    }
    validate_id(&input.workspace_id, "ws_", MAX_WORKSPACE_ID)?;
    if input
        .limit
        .is_some_and(|limit| !(1..=MAX_LIST_LIMIT).contains(&limit))
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    if input
        .cursor
        .as_deref()
        .is_some_and(|cursor| cursor.len() > MAX_CURSOR)
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    Ok(())
}

fn validate_thread_read(input: &ThreadReadInput) -> Result<(), RuntimeCommandError> {
    if input.invalid_optional {
        return Err(RuntimeCommandError::invalid_params());
    }
    validate_id(&input.thread_id, "thr_", MAX_THREAD_ID)?;
    if input
        .after_seq
        .is_some_and(|seq| !(1..=MAX_SEQ).contains(&seq))
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    if input
        .limit
        .is_some_and(|limit| !(1..=MAX_READ_LIMIT).contains(&limit))
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str, max: usize) -> Result<(), RuntimeCommandError> {
    if value.len() <= prefix.len()
        || value.len() > max
        || !value.starts_with(prefix)
        || !value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(RuntimeCommandError::invalid_params());
    }
    Ok(())
}

fn validate_text(value: &str, max: usize) -> Result<(), RuntimeCommandError> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(RuntimeCommandError::invalid_params());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ignores compatible minor-version fields but never serializes them back
    /// into the Java request because the DTO only emits declared fields.
    #[test]
    fn request_unknown_fields_are_ignored_without_forwarding() {
        let input = serde_json::from_value::<ThreadListInput>(serde_json::json!({
            "workspaceId": "ws_demo",
            "unexpected": true
        }))
        .expect("minor-version field is ignored");
        let outbound = serde_json::to_value(input).expect("serialize known fields");
        assert!(outbound.get("unexpected").is_none());
    }

    /// Explicit null is not the same as an omitted optional field at the
    /// protocol boundary and therefore fails with stable command validation.
    #[test]
    fn optional_null_is_rejected_but_omission_is_allowed() {
        let omitted = serde_json::from_value::<ThreadCreateInput>(serde_json::json!({
            "workspaceId": "ws_demo"
        }))
        .expect("omitted optional fields");
        assert!(omitted.title.is_none());
        let explicit_null = serde_json::from_value::<ThreadCreateInput>(serde_json::json!({
            "workspaceId": "ws_demo",
            "title": null
        }));
        let explicit_null = explicit_null.expect("null marker is retained for command validation");
        assert!(explicit_null.invalid_optional);
        assert!(validate_thread_create(&explicit_null).is_err());
    }

    /// Locks the v1 numeric limits before a sidecar request is admitted.
    #[test]
    fn sequence_and_page_bounds_are_enforced() {
        let invalid_seq = ThreadReadInput {
            thread_id: "thr_demo".to_owned(),
            view: Some(ThreadReadView::Snapshot),
            after_seq: Some(MAX_SEQ + 1),
            limit: Some(1),
            invalid_optional: false,
        };
        assert!(validate_thread_read(&invalid_seq).is_err());
        let zero_seq = ThreadReadInput {
            after_seq: Some(0),
            ..invalid_seq
        };
        assert!(validate_thread_read(&zero_seq).is_err());
        let invalid_limit = ThreadListInput {
            workspace_id: "ws_demo".to_owned(),
            include_archived: false,
            limit: Some(MAX_LIST_LIMIT + 1),
            cursor: None,
            invalid_optional: false,
        };
        assert!(validate_thread_list(&invalid_limit).is_err());
    }

    /// Keeps cursor opaque and bounded; a non-empty value is forwarded so the
    /// current snapshot-only Java implementation can return its own RPC error.
    #[test]
    fn cursor_and_view_shapes_are_bounded() {
        let empty_cursor = ThreadListInput {
            workspace_id: "ws_demo".to_owned(),
            include_archived: false,
            limit: Some(1),
            cursor: Some(String::new()),
            invalid_optional: false,
        };
        assert!(validate_thread_list(&empty_cursor).is_ok());
        let opaque_cursor = ThreadListInput {
            cursor: Some("opaque-page-token".to_owned()),
            ..empty_cursor
        };
        assert!(validate_thread_list(&opaque_cursor).is_ok());
        let read = serde_json::from_value::<ThreadReadInput>(serde_json::json!({
            "threadId": "thr_demo",
            "view": "snapshot",
            "afterSeq": 1,
            "limit": 1
        }))
        .expect("snapshot view shape");
        assert!(validate_thread_read(&read).is_ok());
    }

    /// Locks frozen identity lengths and the profile revision namespace.
    #[test]
    fn identity_boundaries_match_v1() {
        let thread_id = format!("thr_{}", "a".repeat(96));
        assert!(validate_id(&thread_id, "thr_", MAX_THREAD_ID).is_ok());
        assert!(validate_id("profile_demo", "profile_", MAX_PROFILE_REVISION).is_ok());
        assert!(validate_id("profile_bad", "thr_", MAX_THREAD_ID).is_err());
    }

    /// Rejects malformed result objects before they can become UI state.
    #[test]
    fn result_parsers_bound_core_fields_and_extensions() {
        let value = serde_json::json!({
            "serverInstanceId": "srv_demo",
            "thread": {
                "threadId": "thr_demo",
                "workspaceId": "ws_demo",
                "title": "Demo",
                "status": "idle",
                "lastSeq": 1
            },
            "items": [{
                "itemId": "item_demo",
                "turnId": "turn_demo",
                "kind": "agent_message",
                "status": "completed",
                "text": "done",
                "futureField": {"kept": true}
            }],
            "snapshotSeq": 1,
            "events": [{"method": "thread/changed"}]
        });
        let parsed = parse_thread_read_result(value).expect("valid read result");
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].extra["futureField"]["kept"], true);
        assert!(
            parse_thread_read_result(serde_json::json!({
                "serverInstanceId": "srv_demo",
                "thread": {
                    "threadId": "thr_demo",
                    "workspaceId": "ws_demo",
                    "title": "Demo",
                    "status": "idle",
                    "lastSeq": 1
                },
                "items": [{"itemId": "item_demo"}],
                "snapshotSeq": 1
            }))
            .is_err()
        );
    }

    /// Verifies the nested create response remains distinct from list results.
    #[test]
    fn result_projection_uses_camel_case() {
        let value = serde_json::to_value(ThreadCreateResult {
            thread: ThreadDto {
                thread_id: "thr_demo".to_owned(),
                workspace_id: "ws_demo".to_owned(),
                title: "Demo".to_owned(),
                status: "idle".to_owned(),
                last_seq: 0,
                active_turn_id: None,
            },
        })
        .expect("create result");
        assert_eq!(value["thread"]["threadId"], "thr_demo");
        assert!(value["thread"].get("thread_id").is_none());
    }
}
