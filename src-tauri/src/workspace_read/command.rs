// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Thin Tauri adapters for the read-only workspace services.
//!
//! The command layer owns only wire validation and error projection.  The
//! existing `WorkspaceRegistry`, `TreeReader`, `FileReader`, and `TextSearch`
//! remain the single implementation of path containment and bounded IO.

use crate::app_runtime::{RuntimeHost, WorkspaceLookup};
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

use super::{
    ContentKind, ContentPolicy, EntryKind, FileContent, FileMetadata, FileReader, FileRevision,
    SearchHit, SearchPolicy, TextEncoding, TextSearch, TextSearchResult, TreeEntry, TreePage,
    TreePageRequest, TreeReader, WorkspaceError,
};

/// Stable workspace command categories keep native paths and IO diagnostics
/// out of the WebView while preserving the retry/action decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkspaceCommandErrorCode {
    NotConfigured,
    UnknownWorkspace,
    InvalidInput,
    InvalidPath,
    PathRejected,
    NotFound,
    NotDirectory,
    NotFile,
    StaleCursor,
    LimitExceeded,
    ChangedDuringRead,
    Io,
}

/// Error DTO deliberately contains no absolute path, OS message, or stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceCommandError {
    pub code: WorkspaceCommandErrorCode,
}

impl WorkspaceCommandError {
    /// Creates an input error before a reader can observe unbounded data.
    const fn invalid_input() -> Self {
        Self {
            code: WorkspaceCommandErrorCode::InvalidInput,
        }
    }

    /// Maps service failures into the narrow categories the UI can recover.
    fn from_workspace(error: WorkspaceError) -> Self {
        let code = match error {
            WorkspaceError::InvalidRelativePath => WorkspaceCommandErrorCode::InvalidPath,
            WorkspaceError::OutsideWorkspace
            | WorkspaceError::PathChanged
            | WorkspaceError::LinkNotAllowed => WorkspaceCommandErrorCode::PathRejected,
            WorkspaceError::PathNotFound => WorkspaceCommandErrorCode::NotFound,
            WorkspaceError::NotDirectory => WorkspaceCommandErrorCode::NotDirectory,
            WorkspaceError::NotFile => WorkspaceCommandErrorCode::NotFile,
            WorkspaceError::StaleCursor => WorkspaceCommandErrorCode::StaleCursor,
            WorkspaceError::EntryBudgetExceeded
            | WorkspaceError::DepthLimitExceeded
            | WorkspaceError::ScanDeadlineExceeded
            | WorkspaceError::FileTooLarge => WorkspaceCommandErrorCode::LimitExceeded,
            WorkspaceError::ChangedDuringRead => WorkspaceCommandErrorCode::ChangedDuringRead,
            WorkspaceError::InvalidRoot | WorkspaceError::WorkspaceNotFound => {
                WorkspaceCommandErrorCode::UnknownWorkspace
            }
            WorkspaceError::Io { .. } => WorkspaceCommandErrorCode::Io,
        };
        Self { code }
    }
}

impl Display for WorkspaceCommandError {
    /// Keeps command diagnostics stable and path-free for Tauri's error path.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.code {
            WorkspaceCommandErrorCode::NotConfigured => "workspace is not configured",
            WorkspaceCommandErrorCode::UnknownWorkspace => "workspace is unknown",
            WorkspaceCommandErrorCode::InvalidInput => "workspace request is invalid",
            WorkspaceCommandErrorCode::InvalidPath => "workspace path is invalid",
            WorkspaceCommandErrorCode::PathRejected => "workspace path is rejected",
            WorkspaceCommandErrorCode::NotFound => "workspace entry was not found",
            WorkspaceCommandErrorCode::NotDirectory => "workspace entry is not a directory",
            WorkspaceCommandErrorCode::NotFile => "workspace entry is not a file",
            WorkspaceCommandErrorCode::StaleCursor => "workspace cursor is stale",
            WorkspaceCommandErrorCode::LimitExceeded => "workspace request exceeded its limit",
            WorkspaceCommandErrorCode::ChangedDuringRead => "workspace entry changed during read",
            WorkspaceCommandErrorCode::Io => "workspace read failed",
        })
    }
}

impl std::error::Error for WorkspaceCommandError {}

/// Camel-case projection keeps internal service snapshots out of the IPC
/// contract while preserving every bounded metadata field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileRevisionDto {
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_millis: Option<u128>,
    pub sha256: Option<String>,
}

/// Camel-case metadata projection used by both tree and file commands.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileMetadataDto {
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_millis: Option<u128>,
    pub revision: WorkspaceFileRevisionDto,
}

/// One tree child projected for the virtualized React file browser.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTreeEntryDto {
    pub name: String,
    pub relative_path: String,
    pub metadata: WorkspaceFileMetadataDto,
    pub can_expand: bool,
}

/// One bounded tree page with an opaque cursor and snapshot token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTreePageDto {
    pub entries: Vec<WorkspaceTreeEntryDto>,
    pub next_cursor: Option<String>,
    pub snapshot_token: String,
    pub total_entries: usize,
    pub depth: usize,
}

/// One bounded file projection with explicit binary/encoding classification.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFileContentDto {
    pub metadata: WorkspaceFileMetadataDto,
    pub kind: ContentKind,
    pub encoding: Option<TextEncoding>,
    pub text: Option<String>,
    pub bytes_read: usize,
    pub truncated: bool,
}

/// One literal-search hit with frontend-friendly line/column names.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSearchHitDto {
    pub relative_path: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
    pub encoding: TextEncoding,
}

/// Search result projection makes bounded partial results explicit to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSearchResultDto {
    pub hits: Vec<WorkspaceSearchHitDto>,
    pub truncated: bool,
    pub scanned_entries: usize,
    pub skipped_files: usize,
}

/// Converts one internal revision into the camel-case command DTO.
fn project_revision(revision: FileRevision) -> WorkspaceFileRevisionDto {
    WorkspaceFileRevisionDto {
        kind: revision.kind,
        size: revision.size,
        modified_unix_millis: revision.modified_unix_millis,
        sha256: revision.sha256,
    }
}

/// Converts metadata once so tree and file projections cannot drift.
fn project_metadata(metadata: FileMetadata) -> WorkspaceFileMetadataDto {
    WorkspaceFileMetadataDto {
        kind: metadata.kind,
        size: metadata.size,
        modified_unix_millis: metadata.modified_unix_millis,
        revision: project_revision(metadata.revision),
    }
}

/// Maps the bounded tree page without exposing internal path/reader types.
fn project_tree(page: TreePage) -> WorkspaceTreePageDto {
    WorkspaceTreePageDto {
        entries: page
            .entries
            .into_iter()
            .map(|entry: TreeEntry| WorkspaceTreeEntryDto {
                name: entry.name,
                relative_path: entry.relative_path,
                metadata: project_metadata(entry.metadata),
                can_expand: entry.can_expand,
            })
            .collect(),
        next_cursor: page.next_cursor,
        snapshot_token: page.snapshot_token,
        total_entries: page.total_entries,
        depth: page.depth,
    }
}

/// Maps file bytes classification while retaining text absence for binaries.
fn project_file(content: FileContent) -> WorkspaceFileContentDto {
    WorkspaceFileContentDto {
        metadata: project_metadata(content.metadata),
        kind: content.kind,
        encoding: content.encoding,
        text: content.text,
        bytes_read: content.bytes_read,
        truncated: content.truncated,
    }
}

/// Maps search hits and bounded result counters to camel-case wire fields.
fn project_search(result: TextSearchResult) -> WorkspaceSearchResultDto {
    WorkspaceSearchResultDto {
        hits: result
            .hits
            .into_iter()
            .map(|hit: SearchHit| WorkspaceSearchHitDto {
                relative_path: hit.relative_path,
                line: hit.line,
                column: hit.column,
                snippet: hit.snippet,
                encoding: hit.encoding,
            })
            .collect(),
        truncated: result.truncated,
        scanned_entries: result.scanned_entries,
        skipped_files: result.skipped_files,
    }
}

/// Requests one bounded directory page from the currently configured root.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceTreeInput {
    pub workspace_id: String,
    pub relative_path: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub page_size: Option<usize>,
    #[serde(default)]
    pub snapshot_token: Option<String>,
}

/// Requests one bounded regular-file projection without exposing its native path.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceReadFileInput {
    pub workspace_id: String,
    pub relative_path: String,
}

/// Requests a literal bounded text search rooted at a relative directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSearchInput {
    pub workspace_id: String,
    pub relative_path: String,
    pub query: String,
}

/// Validates a protocol workspace id without confusing it with the internal
/// UUID carried by `WorkspaceHandle`.
fn validate_workspace_id(value: &str) -> bool {
    value.starts_with("ws_")
        && value.len() <= 99
        && value.len() > 3
        && value[3..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Applies a common bounded string policy before dispatching to a reader.
fn validate_relative_path(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Projects a host workspace lookup into the command's stable error space.
fn map_lookup(error: WorkspaceLookup) -> WorkspaceCommandError {
    WorkspaceCommandError {
        code: match error {
            WorkspaceLookup::Unconfigured => WorkspaceCommandErrorCode::NotConfigured,
            WorkspaceLookup::Unknown => WorkspaceCommandErrorCode::UnknownWorkspace,
        },
    }
}

/// Runs a reader only while the host holds the active protocol binding, so a
/// configure switch cannot leave an older workspace handle active afterward.
fn with_workspace<T>(
    state: &RuntimeHost,
    workspace_id: &str,
    operation: impl FnOnce(&super::WorkspaceHandle) -> Result<T, WorkspaceError>,
) -> Result<T, WorkspaceCommandError> {
    if !validate_workspace_id(workspace_id) {
        return Err(WorkspaceCommandError::invalid_input());
    }
    state
        .with_configured_workspace(workspace_id, operation)
        .map_err(map_lookup)?
        .map_err(WorkspaceCommandError::from_workspace)
}

/// Reads one directory page through the existing bounded `TreeReader`.
#[tauri::command]
pub fn ja_workspace_tree(
    input: WorkspaceTreeInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<WorkspaceTreePageDto, WorkspaceCommandError> {
    if !validate_relative_path(&input.relative_path)
        || input.cursor.as_ref().is_some_and(|value| value.len() > 256)
        || input
            .snapshot_token
            .as_ref()
            .is_some_and(|value| value.len() > 128)
    {
        return Err(WorkspaceCommandError::invalid_input());
    }
    let request = TreePageRequest {
        relative_path: input.relative_path,
        cursor: input.cursor,
        page_size: input.page_size,
        snapshot_token: input.snapshot_token,
    };
    with_workspace(&state, &input.workspace_id, |workspace| {
        TreeReader::new(workspace.clone(), super::TreePolicy::default()).read_page(&request)
    })
    .map(project_tree)
}

/// Reads a regular file using the existing bounded encoding/binary classifier.
#[tauri::command]
pub fn ja_workspace_read_file(
    input: WorkspaceReadFileInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<WorkspaceFileContentDto, WorkspaceCommandError> {
    if !validate_relative_path(&input.relative_path) {
        return Err(WorkspaceCommandError::invalid_input());
    }
    with_workspace(&state, &input.workspace_id, |workspace| {
        FileReader::new(workspace.clone(), ContentPolicy::default()).read(&input.relative_path)
    })
    .map(project_file)
}

/// Searches literal text through the existing no-link traversal and result cap.
#[tauri::command]
pub fn ja_workspace_search(
    input: WorkspaceSearchInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<WorkspaceSearchResultDto, WorkspaceCommandError> {
    if !validate_relative_path(&input.relative_path)
        || input.query.is_empty()
        || input.query.len() > 8192
        || input.query.chars().any(char::is_control)
    {
        return Err(WorkspaceCommandError::invalid_input());
    }
    with_workspace(&state, &input.workspace_id, |workspace| {
        TextSearch::new(workspace.clone(), SearchPolicy::default())
            .search(&input.relative_path, &input.query)
    })
    .map(project_search)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_runtime::{EventSink, LaunchConfig, RuntimeConfigureInput};
    use crate::settings::{ApiProtocol, ProfileSetting, SettingsDocument};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Keeps the public id rule distinct from the internal workspace UUID.
    #[test]
    fn workspace_id_validation_is_protocol_scoped() {
        assert!(validate_workspace_id("ws_project"));
        assert!(!validate_workspace_id("project"));
        assert!(!validate_workspace_id("ws_"));
        assert!(!validate_workspace_id("ws_project:internal"));
    }

    /// Rejects control characters before path services perform filesystem IO.
    #[test]
    fn relative_path_validation_rejects_controls_only_at_wire_edge() {
        assert!(validate_relative_path("src/main.rs"));
        assert!(validate_relative_path("../escape"));
        assert!(!validate_relative_path("src/\u{0000}main.rs"));
    }

    /// Locks the IPC projection to camelCase without changing the service
    /// model's snake_case field names used by Rust callers.
    #[test]
    fn tree_projection_is_camel_case_and_path_free() {
        let revision = FileRevision {
            kind: EntryKind::File,
            size: 3,
            modified_unix_millis: Some(7),
            sha256: Some("abc".to_owned()),
        };
        let page = project_tree(TreePage {
            entries: vec![TreeEntry {
                name: "main.rs".to_owned(),
                relative_path: "src/main.rs".to_owned(),
                metadata: FileMetadata {
                    kind: EntryKind::File,
                    size: 3,
                    modified_unix_millis: Some(7),
                    revision,
                },
                can_expand: false,
            }],
            next_cursor: Some("1".to_owned()),
            snapshot_token: "token".to_owned(),
            total_entries: 1,
            depth: 1,
        });
        let value = serde_json::to_value(page).expect("serialize tree DTO");
        assert_eq!(value["nextCursor"], "1");
        assert_eq!(value["entries"][0]["relativePath"], "src/main.rs");
        assert!(value["entries"][0].get("relative_path").is_none());
        assert_eq!(value["entries"][0]["metadata"]["revision"]["sha256"], "abc");
    }

    /// Proves configure is the only binding owner and old ids fail after a
    /// switch instead of retaining a stale canonical handle.
    #[test]
    fn host_workspace_binding_switch_invalidates_old_protocol_id() {
        let base =
            std::env::temp_dir().join(format!("ja-command-binding-{}", uuid::Uuid::new_v4()));
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).expect("first workspace");
        std::fs::create_dir_all(&second).expect("second workspace");
        let sink: EventSink = Arc::new(|_| Ok(()));
        let host = RuntimeHost::new(
            LaunchConfig::for_test(PathBuf::from("java"), Vec::new(), base.join("run")),
            sink,
        );
        assert_eq!(
            host.with_configured_workspace("ws_first", |_| ())
                .expect_err("unconfigured host"),
            WorkspaceLookup::Unconfigured
        );
        host.configure(RuntimeConfigureInput {
            workspace_id: "ws_first".to_owned(),
            root_path: first.to_string_lossy().into_owned(),
            display_name: None,
            trust: "trusted".to_owned(),
            settings: fixture_settings(),
        })
        .expect("first binding");
        assert_eq!(
            host.with_configured_workspace("ws_first", |_| ())
                .expect_err("configure is not Ready yet"),
            WorkspaceLookup::Unknown
        );
        host.configure(RuntimeConfigureInput {
            workspace_id: "ws_second".to_owned(),
            root_path: second.to_string_lossy().into_owned(),
            display_name: None,
            trust: "trusted".to_owned(),
            settings: fixture_settings(),
        })
        .expect("second binding");
        assert_eq!(
            host.with_configured_workspace("ws_first", |_| ())
                .expect_err("old binding must be gone"),
            WorkspaceLookup::Unknown
        );
        assert_eq!(
            host.with_configured_workspace("ws_second", |_| ())
                .expect_err("configure is not Ready yet"),
            WorkspaceLookup::Unknown
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// Failed reconfiguration and lifecycle stop/shutdown boundaries must not
    /// leave an older handle readable through the same host instance.
    #[test]
    fn host_workspace_binding_clears_on_failure_stop_and_shutdown() {
        let base = std::env::temp_dir().join(format!(
            "ja-command-binding-lifecycle-{}",
            uuid::Uuid::new_v4()
        ));
        let root = base.join("root");
        std::fs::create_dir_all(&root).expect("workspace root");
        std::fs::create_dir_all(base.join("run")).expect("runtime run directory");
        let sink: EventSink = Arc::new(|_| Ok(()));
        let host = RuntimeHost::new(
            LaunchConfig::for_test(PathBuf::from("java"), Vec::new(), base.join("run")),
            sink,
        );
        let configure = |host: &RuntimeHost| {
            host.configure(RuntimeConfigureInput {
                workspace_id: "ws_lifecycle".to_owned(),
                root_path: root.to_string_lossy().into_owned(),
                display_name: None,
                trust: "trusted".to_owned(),
                settings: fixture_settings(),
            })
        };

        configure(&host).expect("initial binding");
        assert_eq!(
            host.with_configured_workspace("ws_lifecycle", |_| ())
                .expect_err("configure is not Ready yet"),
            WorkspaceLookup::Unknown
        );
        let failed = host.configure(RuntimeConfigureInput {
            workspace_id: "ws_replacement".to_owned(),
            root_path: base.join("missing").to_string_lossy().into_owned(),
            display_name: None,
            trust: "trusted".to_owned(),
            settings: fixture_settings(),
        });
        assert!(failed.is_err());
        assert_eq!(
            host.with_configured_workspace("ws_lifecycle", |_| ())
                .expect_err("failed configure clears old binding"),
            WorkspaceLookup::Unconfigured
        );

        configure(&host).expect("binding after failed configure");
        host.stop().expect("stop boundary");
        assert_eq!(
            host.with_configured_workspace("ws_lifecycle", |_| ())
                .expect_err("stop clears binding"),
            WorkspaceLookup::Unconfigured
        );

        configure(&host).expect("binding before shutdown");
        host.shutdown().expect("shutdown boundary");
        assert_eq!(
            host.with_configured_workspace("ws_lifecycle", |_| ())
                .expect_err("shutdown clears binding"),
            WorkspaceLookup::Unconfigured
        );

        // A failed restart must also invalidate a binding that was admitted
        // before the sidecar could reach Ready; the invalid executable keeps
        // this assertion independent from a bundled Java installation.
        let failed_start_base = base.join("failed-start");
        let failed_start_root = failed_start_base.join("root");
        std::fs::create_dir_all(&failed_start_root).expect("failed-start root");
        let failed_start_host = RuntimeHost::new(
            LaunchConfig::for_test(
                failed_start_base.join("missing-sidecar.exe"),
                Vec::new(),
                failed_start_base.join("run"),
            ),
            Arc::new(|_| Ok(())),
        );
        failed_start_host
            .configure(RuntimeConfigureInput {
                workspace_id: "ws_failed_start".to_owned(),
                root_path: failed_start_root.to_string_lossy().into_owned(),
                display_name: None,
                trust: "trusted".to_owned(),
                settings: fixture_settings(),
            })
            .expect("binding before failed start");
        assert!(
            failed_start_host.start().is_err(),
            "missing sidecar must fail"
        );
        assert_eq!(
            failed_start_host
                .with_configured_workspace("ws_failed_start", |_| ())
                .expect_err("failed start clears binding"),
            WorkspaceLookup::Unconfigured
        );
        let _ = failed_start_host.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// Builds the smallest valid profile snapshot for host binding tests.
    fn fixture_settings() -> SettingsDocument {
        SettingsDocument {
            active_profile_revision: Some("profile_fixture".to_owned()),
            profiles: vec![ProfileSetting {
                profile_revision: "profile_fixture".to_owned(),
                name: "Fixture".to_owned(),
                provider: "fixture".to_owned(),
                protocol: ApiProtocol::OpenAiChatCompletions,
                model: "fixture-model".to_owned(),
                base_url: None,
                credential_ref: None,
                supports_vision: false,
                access_mode: Default::default(),
                skill_revisions: Vec::new(),
                mcp_revisions: Some(Vec::new()),
            }],
            ..SettingsDocument::default()
        }
    }
}
