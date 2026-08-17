// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use serde::{Deserialize, Serialize};

/// Porcelain-v2 status record categories are enough for a read-only sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitStatusKind {
    Head,
    Changed,
    Renamed,
    Unmerged,
    Untracked,
    Ignored,
}

/// A parsed machine-format status record with root-relative paths only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub kind: GitStatusKind,
    pub index_status: Option<char>,
    pub worktree_status: Option<char>,
    pub path: String,
    pub original_path: Option<String>,
}

/// Raw diff bytes preserve binary patches and avoid an invalid UTF-8 guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiff {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// A bounded, NUL-delimited log projection used by timeline/workbench views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLogEntry {
    pub object_id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub authored_at: String,
    pub subject: String,
}

/// `git show` returns bytes because blobs and patches are not guaranteed text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitShow {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}
