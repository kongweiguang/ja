// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::WorkspaceError;
use super::model::{EntryKind, FileRevision};
use super::registry::{WorkspaceHandle, entry_kind, is_reparse_point};
use super::tree::join_relative;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::time::{Duration, Instant};

/// Polling is an explicit fallback abstraction until an OS watcher can be
/// added without changing the frontend event contract.
pub trait ChangeDetector: Send {
    /// Returns a bounded change batch or a not-due marker.
    fn poll(&mut self) -> Result<ChangeBatch, WorkspaceError>;

    /// Forces a new authoritative baseline after an overflow or rescan hint.
    fn rescan(&mut self) -> Result<ChangeBatch, WorkspaceError>;
}

/// Limits polling frequency and scan size so a busy workspace cannot starve
/// the agent or flood the WebView with duplicate events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingPolicy {
    pub min_interval_millis: u64,
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_changes: usize,
    pub hash_limit_bytes: u64,
    pub max_total_bytes: u64,
    pub max_scan_millis: u64,
}

impl Default for PollingPolicy {
    /// Keeps fallback polling sparse and each scan explicitly bounded.
    fn default() -> Self {
        Self {
            min_interval_millis: 250,
            max_depth: 64,
            max_entries: 100_000,
            max_changes: 2_000,
            hash_limit_bytes: 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_scan_millis: 2_000,
        }
    }
}

/// Describes why a polling result may not be treated as a complete snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollState {
    NotDue,
    Updated,
    Overflow,
}

/// A change kind is intentionally small; callers can request a full tree page
/// when they need details instead of trusting one event as authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Replaced,
}

/// A relative path plus old/new revision, never an absolute OS path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub relative_path: String,
    pub kind: ChangeKind,
    pub previous: Option<FileRevision>,
    pub current: Option<FileRevision>,
}

/// Poll result carries overflow/rescan state explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub state: PollState,
    pub generation: u64,
    pub changes: Vec<ChangeRecord>,
    pub requires_rescan: bool,
}

/// A deterministic, bounded detector that can later be replaced by a native
/// watcher without changing workspace identity or overflow semantics.
#[derive(Debug)]
pub struct PollingChangeDetector {
    workspace: WorkspaceHandle,
    policy: PollingPolicy,
    snapshot: BTreeMap<String, FileRevision>,
    last_poll: Option<Instant>,
    generation: u64,
}

impl PollingChangeDetector {
    /// Takes an initial bounded baseline so the first poll reports only later
    /// external edits rather than re-emitting every existing file.
    pub fn new(workspace: WorkspaceHandle, policy: PollingPolicy) -> Result<Self, WorkspaceError> {
        let policy = PollingPolicy {
            min_interval_millis: policy.min_interval_millis.clamp(1, 60_000),
            max_depth: policy.max_depth.min(256),
            max_entries: policy.max_entries.clamp(1, 1_000_000),
            max_changes: policy.max_changes.clamp(1, 100_000),
            hash_limit_bytes: policy.hash_limit_bytes.min(16 * 1024 * 1024),
            max_total_bytes: policy.max_total_bytes.min(2 * 1024 * 1024 * 1024),
            max_scan_millis: policy.max_scan_millis.clamp(1, 60_000),
        };
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(policy.max_scan_millis))
            .unwrap_or_else(Instant::now);
        let scan = scan_workspace(&workspace, &policy, deadline)?;
        if scan.overflow {
            return Err(WorkspaceError::EntryBudgetExceeded);
        }
        let snapshot = scan.snapshot;
        Ok(Self {
            workspace,
            policy,
            snapshot,
            last_poll: None,
            generation: 0,
        })
    }
}

impl ChangeDetector for PollingChangeDetector {
    /// Enforces a minimum interval and returns overflow when the bounded scan
    /// cannot prove a complete view; callers must then rescan explicitly.
    fn poll(&mut self) -> Result<ChangeBatch, WorkspaceError> {
        let now = Instant::now();
        if self.last_poll.is_some_and(|last| {
            now.duration_since(last) < Duration::from_millis(self.policy.min_interval_millis)
        }) {
            return Ok(ChangeBatch {
                state: PollState::NotDue,
                generation: self.generation,
                changes: Vec::new(),
                requires_rescan: false,
            });
        }
        self.last_poll = Some(now);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.policy.max_scan_millis))
            .unwrap_or_else(Instant::now);
        let scan = match scan_workspace(&self.workspace, &self.policy, deadline) {
            Ok(scan) => scan,
            Err(WorkspaceError::ScanDeadlineExceeded) => {
                self.generation = self.generation.saturating_add(1);
                return Ok(ChangeBatch {
                    state: PollState::Overflow,
                    generation: self.generation,
                    changes: Vec::new(),
                    requires_rescan: true,
                });
            }
            Err(error) => return Err(error),
        };
        self.generation = self.generation.saturating_add(1);
        if scan.overflow {
            return Ok(ChangeBatch {
                state: PollState::Overflow,
                generation: self.generation,
                changes: Vec::new(),
                requires_rescan: true,
            });
        }
        let changes = diff_snapshots(&self.snapshot, &scan.snapshot, self.policy.max_changes);
        let overflow = changes.len() >= self.policy.max_changes;
        self.snapshot = scan.snapshot;
        Ok(ChangeBatch {
            state: if overflow {
                PollState::Overflow
            } else {
                PollState::Updated
            },
            generation: self.generation,
            changes,
            requires_rescan: overflow,
        })
    }

    /// Replaces the baseline only after a complete scan, preventing an
    /// overflowed partial view from silently discarding future changes.
    fn rescan(&mut self) -> Result<ChangeBatch, WorkspaceError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.policy.max_scan_millis))
            .unwrap_or_else(Instant::now);
        let scan = match scan_workspace(&self.workspace, &self.policy, deadline) {
            Ok(scan) => scan,
            Err(WorkspaceError::ScanDeadlineExceeded) => {
                self.generation = self.generation.saturating_add(1);
                return Ok(ChangeBatch {
                    state: PollState::Overflow,
                    generation: self.generation,
                    changes: Vec::new(),
                    requires_rescan: true,
                });
            }
            Err(error) => return Err(error),
        };
        self.generation = self.generation.saturating_add(1);
        if scan.overflow {
            return Ok(ChangeBatch {
                state: PollState::Overflow,
                generation: self.generation,
                changes: Vec::new(),
                requires_rescan: true,
            });
        }
        self.snapshot = scan.snapshot;
        self.last_poll = Some(Instant::now());
        Ok(ChangeBatch {
            state: PollState::Updated,
            generation: self.generation,
            changes: Vec::new(),
            requires_rescan: false,
        })
    }
}

struct ScanResult {
    snapshot: BTreeMap<String, FileRevision>,
    overflow: bool,
}

/// Walks metadata only, never follows links/reparse points and stops at the
/// first budget violation so the caller can request a deliberate rescan.
fn scan_workspace(
    workspace: &WorkspaceHandle,
    policy: &PollingPolicy,
    deadline: Instant,
) -> Result<ScanResult, WorkspaceError> {
    let root = workspace.resolve_guard("", Some(true))?;
    let mut queue = VecDeque::from([(String::new(), root, 0usize)]);
    let mut snapshot = BTreeMap::new();
    let mut visited = 0usize;
    let mut total_bytes = 0_u64;
    while let Some((parent, directory, depth)) = queue.pop_front() {
        if Instant::now() >= deadline {
            return Ok(ScanResult {
                snapshot,
                overflow: true,
            });
        }
        if depth > policy.max_depth {
            return Ok(ScanResult {
                snapshot,
                overflow: true,
            });
        }
        workspace.verify_resolved(&directory, Some(true))?;
        for entry in
            fs::read_dir(&directory.path).map_err(|error| WorkspaceError::io("read_dir", error))?
        {
            if Instant::now() >= deadline {
                return Ok(ScanResult {
                    snapshot,
                    overflow: true,
                });
            }
            visited = visited.saturating_add(1);
            if visited > policy.max_entries {
                return Ok(ScanResult {
                    snapshot,
                    overflow: true,
                });
            }
            let entry = entry.map_err(|error| WorkspaceError::io("read_dir", error))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| WorkspaceError::InvalidRelativePath)?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| WorkspaceError::io("stat", error))?;
            total_bytes = total_bytes
                .saturating_add(metadata.len())
                .saturating_add(name.len() as u64);
            if total_bytes > policy.max_total_bytes {
                return Ok(ScanResult {
                    snapshot,
                    overflow: true,
                });
            }
            let kind = entry_kind(&metadata);
            let relative = join_relative(&parent, &name);
            let revision = super::registry::metadata_for_path_with_deadline(
                &path,
                &metadata,
                policy.hash_limit_bytes,
                Some(deadline),
            )?
            .revision;
            snapshot.insert(relative, revision);
            if kind == EntryKind::Directory && !is_reparse_point(&metadata) {
                if depth >= policy.max_depth {
                    return Ok(ScanResult {
                        snapshot,
                        overflow: true,
                    });
                }
                let child_relative = join_relative(&parent, &name);
                let child = workspace.resolve_guard(&child_relative, Some(true))?;
                queue.push_back((child_relative, child, depth.saturating_add(1)));
            }
        }
        workspace.verify_resolved(&directory, Some(true))?;
    }
    Ok(ScanResult {
        snapshot,
        overflow: false,
    })
}

/// Produces deterministic create/modify/delete records and caps event volume.
fn diff_snapshots(
    previous: &BTreeMap<String, FileRevision>,
    current: &BTreeMap<String, FileRevision>,
    max_changes: usize,
) -> Vec<ChangeRecord> {
    let mut changes = Vec::new();
    let paths = previous
        .keys()
        .chain(current.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        if changes.len() >= max_changes {
            break;
        }
        let old = previous.get(&path);
        let new = current.get(&path);
        let kind = match (old, new) {
            (None, Some(_)) => Some(ChangeKind::Created),
            (Some(_), None) => Some(ChangeKind::Deleted),
            (Some(old), Some(new)) if old != new => {
                if old.size == new.size && old.modified_unix_millis == new.modified_unix_millis {
                    Some(ChangeKind::Replaced)
                } else {
                    Some(ChangeKind::Modified)
                }
            }
            _ => None,
        };
        if let Some(kind) = kind {
            changes.push(ChangeRecord {
                relative_path: path.clone(),
                kind,
                previous: old.cloned(),
                current: new.cloned(),
            });
        }
    }
    changes
}
