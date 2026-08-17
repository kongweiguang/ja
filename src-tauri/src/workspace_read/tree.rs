// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::WorkspaceError;
use super::model::{TreeEntry, TreePage};
use super::registry::{WorkspaceHandle, entry_kind, hex_lower, is_reparse_point};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Hard bounds prevent a virtualized UI request from turning into a full tree
/// walk or an unbounded directory allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreePolicy {
    pub max_depth: usize,
    pub max_entries_per_page_scan: usize,
    pub max_page_size: usize,
    pub hash_limit_bytes: u64,
    pub max_total_bytes: u64,
    pub max_scan_millis: u64,
}

impl Default for TreePolicy {
    /// Provides conservative defaults that keep one UI request bounded.
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_entries_per_page_scan: 100_000,
            max_page_size: 500,
            hash_limit_bytes: 4 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_scan_millis: 2_000,
        }
    }
}

/// Request DTO kept separate so pagination semantics are testable without IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreePageRequest {
    pub relative_path: String,
    pub cursor: Option<String>,
    pub page_size: Option<usize>,
    pub snapshot_token: Option<String>,
}

/// Reads exactly one directory level; children are only inspected after the UI
/// explicitly requests that directory, which keeps large repositories usable.
#[derive(Debug, Clone)]
pub struct TreeReader {
    workspace: WorkspaceHandle,
    policy: TreePolicy,
}

impl TreeReader {
    /// Associates a reader with an immutable canonical root and explicit limits.
    pub fn new(workspace: WorkspaceHandle, policy: TreePolicy) -> Self {
        let policy = TreePolicy {
            max_depth: policy.max_depth.min(256),
            max_entries_per_page_scan: policy.max_entries_per_page_scan.clamp(1, 1_000_000),
            max_page_size: policy.max_page_size.clamp(1, 10_000),
            hash_limit_bytes: policy.hash_limit_bytes.min(64 * 1024 * 1024),
            max_total_bytes: policy.max_total_bytes.min(2 * 1024 * 1024 * 1024),
            max_scan_millis: policy.max_scan_millis.clamp(1, 60_000),
        };
        Self { workspace, policy }
    }

    /// Returns a sorted page with a numeric opaque cursor and never recurses.
    pub fn read_page(&self, request: &TreePageRequest) -> Result<TreePage, WorkspaceError> {
        let relative_path = request.relative_path.as_str();
        let resolved = self.workspace.resolve_guard(relative_path, Some(true))?;
        self.workspace.verify_resolved(&resolved, Some(true))?;
        let directory = &resolved.path;
        let depth = relative_depth(relative_path);
        if depth > self.policy.max_depth {
            return Err(WorkspaceError::DepthLimitExceeded);
        }
        if request.cursor.is_some() && request.snapshot_token.is_none() {
            return Err(WorkspaceError::StaleCursor);
        }
        let start = request
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| WorkspaceError::InvalidRelativePath)?;
        let page_size = request
            .page_size
            .unwrap_or(self.policy.max_page_size)
            .clamp(1, self.policy.max_page_size);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.policy.max_scan_millis))
            .unwrap_or_else(Instant::now);
        let mut entries = Vec::new();
        let mut scanned_bytes = 0_u64;
        for entry in
            fs::read_dir(directory).map_err(|error| WorkspaceError::io("read_dir", error))?
        {
            if Instant::now() >= deadline {
                return Err(WorkspaceError::ScanDeadlineExceeded);
            }
            let entry = entry.map_err(|error| WorkspaceError::io("read_dir", error))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| WorkspaceError::InvalidRelativePath)?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| WorkspaceError::io("stat", error))?;
            if entries.len() >= self.policy.max_entries_per_page_scan {
                return Err(WorkspaceError::EntryBudgetExceeded);
            }
            scanned_bytes = scanned_bytes
                .saturating_add(metadata.len())
                .saturating_add(name.len() as u64);
            if scanned_bytes > self.policy.max_total_bytes {
                return Err(WorkspaceError::EntryBudgetExceeded);
            }
            entries.push((name, path, metadata));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let total_entries = entries.len();
        let mut token_hasher = Sha256::new();
        for (name, path, metadata) in &entries {
            token_hasher.update(name.as_bytes());
            token_hasher.update([0]);
            let file_metadata = super::registry::metadata_for_path_with_deadline(
                path,
                metadata,
                self.policy.hash_limit_bytes,
                Some(deadline),
            )?;
            token_hasher.update(
                format!("{:?}:{:?}\n", file_metadata.kind, file_metadata.revision).as_bytes(),
            );
        }
        self.workspace.verify_resolved(&resolved, Some(true))?;
        let snapshot_token = hex_lower(&token_hasher.finalize());
        if request
            .snapshot_token
            .as_deref()
            .is_some_and(|token| token != snapshot_token)
        {
            return Err(WorkspaceError::StaleCursor);
        }
        let end = start.saturating_add(page_size).min(total_entries);
        let mut page_entries = Vec::with_capacity(end.saturating_sub(start));
        for (name, path, metadata) in entries.into_iter().skip(start).take(page_size) {
            let kind = entry_kind(&metadata);
            let relative = join_relative(relative_path, &name);
            let child_depth = depth.saturating_add(1);
            let can_expand = kind == super::model::EntryKind::Directory
                && !is_reparse_point(&metadata)
                && child_depth < self.policy.max_depth;
            page_entries.push(TreeEntry {
                name,
                relative_path: relative,
                metadata: super::registry::metadata_for_path_with_deadline(
                    &path,
                    &metadata,
                    self.policy.hash_limit_bytes,
                    Some(deadline),
                )?,
                can_expand,
            });
        }
        self.workspace.verify_resolved(&resolved, Some(true))?;
        Ok(TreePage {
            entries: page_entries,
            next_cursor: (end < total_entries).then(|| end.to_string()),
            snapshot_token,
            total_entries,
            depth,
        })
    }
}

/// Counts only normal components so repeated separators cannot bypass depth.
fn relative_depth(relative_path: &str) -> usize {
    Path::new(relative_path)
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count()
}

/// Normalizes UI-facing paths to slash separators regardless of host OS.
pub(crate) fn join_relative(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}
