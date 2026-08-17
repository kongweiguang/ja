// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use serde::{Deserialize, Serialize};

/// Distinguishes entries that may be traversed from opaque filesystem nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    ReparsePoint,
    Other,
}

/// Describes how a bounded reader classified a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Binary,
    UnknownEncoding,
    TooLarge,
}

/// Supported text encodings are intentionally limited to deterministic,
/// lossless decoders available in the Rust standard library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

/// A stable identity snapshot used to detect external edits between reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevision {
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_millis: Option<u128>,
    pub sha256: Option<String>,
}

/// Metadata never contains an absolute path, because the caller already owns
/// the opaque workspace id and relative path used to request it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_millis: Option<u128>,
    pub revision: FileRevision,
}

/// A paged directory item suitable for a virtualized tree view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub relative_path: String,
    pub metadata: FileMetadata,
    pub can_expand: bool,
}

/// The result of one non-recursive directory page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreePage {
    pub entries: Vec<TreeEntry>,
    pub next_cursor: Option<String>,
    pub snapshot_token: String,
    pub total_entries: usize,
    pub depth: usize,
}

/// The result of a bounded regular-file read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub metadata: FileMetadata,
    pub kind: ContentKind,
    pub encoding: Option<TextEncoding>,
    pub text: Option<String>,
    pub bytes_read: usize,
    pub truncated: bool,
}

/// A text-search match with line/column coordinates relative to the decoded
/// text, not byte offsets from the original encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub relative_path: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
    pub encoding: TextEncoding,
}
