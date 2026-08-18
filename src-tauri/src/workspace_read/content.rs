// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::WorkspaceError;
use super::model::{ContentKind, FileContent, TextEncoding};
use super::registry::{WorkspaceHandle, metadata_for_path};
use std::fs::{self, File};
use std::io::Read;

/// Bounds every read before allocation and keeps binary content out of text IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentPolicy {
    pub max_bytes: u64,
    pub hash_limit_bytes: u64,
}

impl Default for ContentPolicy {
    /// Sets a small enough default to keep editor IPC responsive.
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            hash_limit_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Reads files only through a workspace handle and a bounded content policy.
#[derive(Debug, Clone)]
pub struct FileReader {
    workspace: WorkspaceHandle,
    policy: ContentPolicy,
}

impl FileReader {
    /// Keeps the read policy explicit so callers cannot silently widen limits.
    pub fn new(workspace: WorkspaceHandle, policy: ContentPolicy) -> Self {
        let max_bytes = policy.max_bytes.min(64 * 1024 * 1024);
        let policy = ContentPolicy {
            max_bytes,
            hash_limit_bytes: policy.hash_limit_bytes.min(max_bytes),
        };
        Self { workspace, policy }
    }

    /// Reads a stable regular file, classifies encoding and reports binary or
    /// oversized content without returning unbounded bytes to the caller.
    pub fn read(&self, relative_path: &str) -> Result<FileContent, WorkspaceError> {
        let resolved = self.workspace.resolve_guard(relative_path, Some(false))?;
        self.workspace.verify_resolved(&resolved, Some(false))?;
        let path = &resolved.path;
        let before =
            fs::symlink_metadata(path).map_err(|error| WorkspaceError::io("stat", error))?;
        let hash_limit = self.policy.hash_limit_bytes.min(self.policy.max_bytes);
        let before_metadata = metadata_for_path(path, &before, hash_limit)?;
        if before.len() > self.policy.max_bytes {
            self.workspace.verify_resolved(&resolved, Some(false))?;
            return Ok(FileContent {
                metadata: before_metadata,
                kind: ContentKind::TooLarge,
                encoding: None,
                text: None,
                bytes_read: 0,
                truncated: true,
            });
        }
        let file = File::open(path).map_err(|error| WorkspaceError::io("open", error))?;
        let capacity = usize::try_from(self.policy.max_bytes)
            .map_err(|_| WorkspaceError::FileTooLarge)?
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(capacity.min(64 * 1024));
        file.take(self.policy.max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| WorkspaceError::io("read", error))?;
        let after =
            fs::symlink_metadata(path).map_err(|error| WorkspaceError::io("stat", error))?;
        let after_metadata = metadata_for_path(path, &after, hash_limit)?;
        if before_metadata.revision != after_metadata.revision {
            return Err(WorkspaceError::ChangedDuringRead);
        }
        self.workspace.verify_resolved(&resolved, Some(false))?;
        if bytes.len() as u64 > self.policy.max_bytes {
            return Ok(FileContent {
                metadata: after_metadata,
                kind: ContentKind::TooLarge,
                encoding: None,
                text: None,
                bytes_read: usize::try_from(self.policy.max_bytes).unwrap_or(usize::MAX),
                truncated: true,
            });
        }
        let (kind, encoding, text) = decode_content(&bytes);
        Ok(FileContent {
            metadata: after_metadata,
            kind,
            encoding,
            text,
            bytes_read: bytes.len(),
            truncated: false,
        })
    }
}

/// Decodes only BOM-marked UTF-16 and strict UTF-8; guessing another encoding
/// would make search results non-reproducible across platforms.
pub(crate) fn decode_content(bytes: &[u8]) -> (ContentKind, Option<TextEncoding>, Option<String>) {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return match std::str::from_utf8(&bytes[3..]) {
            Ok(text) => (
                ContentKind::Text,
                Some(TextEncoding::Utf8Bom),
                Some(text.to_owned()),
            ),
            Err(_) => (ContentKind::UnknownEncoding, None, None),
        };
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], false);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => (
            ContentKind::Text,
            Some(TextEncoding::Utf8),
            Some(text.to_owned()),
        ),
        Ok(_) => (ContentKind::Binary, None, None),
        Err(_) if bytes.contains(&0) => (ContentKind::Binary, None, None),
        Err(_) => (ContentKind::UnknownEncoding, None, None),
    }
}

/// Converts UTF-16 code units without a third-party transcoder while rejecting
/// odd byte counts and invalid surrogate sequences instead of replacing data.
fn decode_utf16(
    bytes: &[u8],
    little_endian: bool,
) -> (ContentKind, Option<TextEncoding>, Option<String>) {
    if !bytes.len().is_multiple_of(2) {
        return (ContentKind::UnknownEncoding, None, None);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect::<Vec<_>>();
    let encoding = if little_endian {
        TextEncoding::Utf16Le
    } else {
        TextEncoding::Utf16Be
    };
    match String::from_utf16(&units) {
        Ok(text) => (ContentKind::Text, Some(encoding), Some(text)),
        Err(_) => (ContentKind::UnknownEncoding, None, None),
    }
}
