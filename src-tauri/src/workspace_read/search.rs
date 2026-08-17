// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::content::decode_content;
use super::error::WorkspaceError;
use super::model::{ContentKind, SearchHit, TextEncoding};
use super::registry::{WorkspaceHandle, entry_kind, is_reparse_point};
use super::tree::join_relative;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::time::{Duration, Instant};

/// Search limits make a query cancelable by construction even on a generated
/// repository with millions of entries or very large binary blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPolicy {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_results: usize,
    pub max_query_bytes: usize,
    pub max_scan_millis: u64,
}

impl Default for SearchPolicy {
    /// Sets bounded traversal and result defaults for interactive search.
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_entries: 100_000,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_results: 500,
            max_query_bytes: 8 * 1024,
            max_scan_millis: 2_000,
        }
    }
}

/// Search result explicitly reports truncation so the UI never presents a
/// bounded partial result as if it were exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSearchResult {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    pub scanned_entries: usize,
    pub skipped_files: usize,
}

/// Performs literal text search over a workspace without following links.
#[derive(Debug, Clone)]
pub struct TextSearch {
    workspace: WorkspaceHandle,
    policy: SearchPolicy,
}

impl TextSearch {
    /// Associates search with the same opaque workspace root used by the tree.
    pub fn new(workspace: WorkspaceHandle, policy: SearchPolicy) -> Self {
        let policy = SearchPolicy {
            max_depth: policy.max_depth.min(256),
            max_entries: policy.max_entries.clamp(1, 1_000_000),
            max_file_bytes: policy.max_file_bytes.min(64 * 1024 * 1024),
            max_total_bytes: policy.max_total_bytes.min(512 * 1024 * 1024),
            max_results: policy.max_results.clamp(1, 100_000),
            max_query_bytes: policy.max_query_bytes.clamp(1, 64 * 1024),
            max_scan_millis: policy.max_scan_millis.clamp(1, 60_000),
        };
        Self { workspace, policy }
    }

    /// Scans depth-first with entry, byte and result budgets; invalid/binary
    /// files are classified and skipped instead of aborting a whole search.
    pub fn search(
        &self,
        relative_path: &str,
        query: &str,
    ) -> Result<TextSearchResult, WorkspaceError> {
        if query.is_empty() || query.len() > self.policy.max_query_bytes {
            return Err(WorkspaceError::InvalidRelativePath);
        }
        let root = self.workspace.resolve_guard(relative_path, Some(true))?;
        let mut queue = VecDeque::from([(relative_path.to_owned(), root, 0usize)]);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(self.policy.max_scan_millis))
            .unwrap_or_else(Instant::now);
        let mut hits = Vec::new();
        let mut scanned_entries = 0usize;
        let mut skipped_files = 0usize;
        let mut total_bytes = 0u64;
        let mut truncated = false;
        while let Some((parent_relative, directory, depth)) = queue.pop_front() {
            if Instant::now() >= deadline {
                return Ok(TextSearchResult {
                    hits,
                    truncated: true,
                    scanned_entries,
                    skipped_files,
                });
            }
            if depth > self.policy.max_depth {
                truncated = true;
                continue;
            }
            self.workspace.verify_resolved(&directory, Some(true))?;
            let entries = fs::read_dir(&directory.path)
                .map_err(|error| WorkspaceError::io("read_dir", error))?;
            for entry in entries {
                if Instant::now() >= deadline {
                    return Ok(TextSearchResult {
                        hits,
                        truncated: true,
                        scanned_entries,
                        skipped_files,
                    });
                }
                scanned_entries = scanned_entries.saturating_add(1);
                if scanned_entries > self.policy.max_entries {
                    truncated = true;
                    break;
                }
                let entry = entry.map_err(|error| WorkspaceError::io("read_dir", error))?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| WorkspaceError::InvalidRelativePath)?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| WorkspaceError::io("stat", error))?;
                let kind = entry_kind(&metadata);
                let relative = join_relative(&parent_relative, &name);
                if kind == super::model::EntryKind::Directory && !is_reparse_point(&metadata) {
                    if depth < self.policy.max_depth {
                        let child = self.workspace.resolve_guard(&relative, Some(true))?;
                        queue.push_back((relative, child, depth.saturating_add(1)));
                    } else {
                        truncated = true;
                    }
                    continue;
                }
                if kind != super::model::EntryKind::File {
                    continue;
                }
                let file_guard = self.workspace.resolve_guard(&relative, Some(false))?;
                self.workspace.verify_resolved(&file_guard, Some(false))?;
                if metadata.len() > self.policy.max_file_bytes
                    || total_bytes.saturating_add(metadata.len()) > self.policy.max_total_bytes
                {
                    skipped_files = skipped_files.saturating_add(1);
                    truncated = true;
                    continue;
                }
                let file = fs::File::open(&file_guard.path)
                    .map_err(|error| WorkspaceError::io("open", error))?;
                let bytes = read_bounded_until(file, self.policy.max_file_bytes, deadline)?;
                if bytes.len() as u64 > self.policy.max_file_bytes {
                    skipped_files = skipped_files.saturating_add(1);
                    truncated = true;
                    continue;
                }
                self.workspace.verify_resolved(&file_guard, Some(false))?;
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);
                let (content_kind, encoding, text) = decode_content(&bytes);
                if content_kind != ContentKind::Text {
                    continue;
                }
                let Some((text, encoding)) = text.zip(encoding) else {
                    continue;
                };
                if let Some(limit_reached) = append_hits(
                    &mut hits,
                    &relative,
                    query,
                    &text,
                    encoding,
                    self.policy.max_results,
                ) {
                    truncated |= limit_reached;
                    if hits.len() >= self.policy.max_results {
                        return Ok(TextSearchResult {
                            hits,
                            truncated: true,
                            scanned_entries,
                            skipped_files,
                        });
                    }
                }
            }
            self.workspace.verify_resolved(&directory, Some(true))?;
            if truncated && scanned_entries >= self.policy.max_entries {
                break;
            }
        }
        Ok(TextSearchResult {
            hits,
            truncated,
            scanned_entries,
            skipped_files,
        })
    }
}

/// Reads one search file in chunks so the scan deadline also bounds a slow
/// filesystem read rather than only the directory traversal between files.
fn read_bounded_until(
    mut file: fs::File,
    max_bytes: u64,
    deadline: Instant,
) -> Result<Vec<u8>, WorkspaceError> {
    let max_bytes = usize::try_from(max_bytes).map_err(|_| WorkspaceError::FileTooLarge)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(WorkspaceError::ScanDeadlineExceeded);
        }
        let count = file
            .read(&mut buffer)
            .map_err(|error| WorkspaceError::io("read", error))?;
        if count == 0 {
            return Ok(bytes);
        }
        let remaining = max_bytes.saturating_add(1).saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(remaining)]);
        if bytes.len() > max_bytes {
            return Ok(bytes);
        }
    }
}

/// Appends bounded line/column matches and returns whether the result cap was
/// reached, allowing the caller to explain partial results to the UI.
fn append_hits(
    hits: &mut Vec<SearchHit>,
    relative_path: &str,
    query: &str,
    text: &str,
    encoding: TextEncoding,
    max_results: usize,
) -> Option<bool> {
    let mut found = false;
    for (offset, _) in text.match_indices(query) {
        found = true;
        if hits.len() >= max_results {
            return Some(true);
        }
        let line = text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = text[..offset]
            .rsplit('\n')
            .next()
            .map(|prefix| prefix.chars().count() + 1)
            .unwrap_or(1);
        hits.push(SearchHit {
            relative_path: relative_path.to_owned(),
            line,
            column,
            snippet: snippet(text, offset, query.len()),
            encoding,
        });
    }
    found.then_some(false)
}

/// Keeps snippets small enough for a timeline/search result payload.
fn snippet(text: &str, offset: usize, query_len: usize) -> String {
    let start = text[..offset]
        .char_indices()
        .rev()
        .nth(80)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let query_end = offset.saturating_add(query_len).min(text.len());
    let end = text[query_end..]
        .char_indices()
        .nth(80)
        .map(|(index, _)| query_end + index)
        .unwrap_or(text.len());
    text.get(start..end).unwrap_or_default().replace('\n', " ")
}
