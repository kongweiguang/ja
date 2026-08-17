// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::GitError;
use super::model::{GitLogEntry, GitStatusEntry, GitStatusKind};

/// Parses porcelain-v2 `-z` without interpreting filenames as shell syntax.
pub(crate) fn parse_status(
    bytes: &[u8],
    max_records: usize,
) -> Result<Vec<GitStatusEntry>, GitError> {
    if max_records == 0 {
        return Err(GitError::Parse);
    }
    let records = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut index = 0usize;
    let mut record_count = 0usize;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        charge_status_records(&mut record_count, 1, max_records)?;
        match record[0] {
            b'#' => result.push(GitStatusEntry {
                kind: GitStatusKind::Head,
                index_status: None,
                worktree_status: None,
                path: text_field(record)?,
                original_path: None,
            }),
            b'1' => {
                let fields = split_fields(record, 9)?;
                let status = fields[1].as_bytes();
                if status.len() != 2 {
                    return Err(GitError::Parse);
                }
                result.push(GitStatusEntry {
                    kind: GitStatusKind::Changed,
                    index_status: Some(status[0] as char),
                    worktree_status: Some(status[1] as char),
                    path: fields[8].to_owned(),
                    original_path: None,
                });
            }
            b'2' => {
                let fields = split_fields(record, 10)?;
                let status = fields[1].as_bytes();
                if status.len() != 2 || index >= records.len() {
                    return Err(GitError::Parse);
                }
                charge_status_records(&mut record_count, 1, max_records)?;
                result.push(GitStatusEntry {
                    kind: GitStatusKind::Renamed,
                    index_status: Some(status[0] as char),
                    worktree_status: Some(status[1] as char),
                    path: fields[9].to_owned(),
                    original_path: Some(text_field(records[index])?),
                });
                index += 1;
            }
            b'u' => {
                let fields = split_fields(record, 11)?;
                let status = fields[1].as_bytes();
                if status.len() != 2 {
                    return Err(GitError::Parse);
                }
                result.push(GitStatusEntry {
                    kind: GitStatusKind::Unmerged,
                    index_status: Some(status[0] as char),
                    worktree_status: Some(status[1] as char),
                    path: fields[10].to_owned(),
                    original_path: None,
                });
            }
            b'?' | b'!' => {
                let kind = if record[0] == b'?' {
                    GitStatusKind::Untracked
                } else {
                    GitStatusKind::Ignored
                };
                let path = record
                    .get(2..)
                    .ok_or(GitError::Parse)
                    .and_then(text_field)?;
                result.push(GitStatusEntry {
                    kind,
                    index_status: None,
                    worktree_status: None,
                    path,
                    original_path: None,
                });
            }
            _ => return Err(GitError::Parse),
        }
    }
    Ok(result)
}

/// Charges every non-empty porcelain record before exposing a parsed entry;
/// rename records charge both the new-path record and its NUL companion.
fn charge_status_records(
    count: &mut usize,
    additional: usize,
    max_records: usize,
) -> Result<(), GitError> {
    *count = count.checked_add(additional).ok_or(GitError::Parse)?;
    if *count > max_records {
        return Err(GitError::Parse);
    }
    Ok(())
}

/// Parses the fixed six-field NUL-delimited log format owned by this adapter.
pub(crate) fn parse_log(bytes: &[u8]) -> Result<Vec<GitLogEntry>, GitError> {
    // Git appends a line terminator after the final NUL; remove only that
    // transport terminator so an empty root-parent field remains a column.
    let payload = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
    let mut fields = payload
        .split(|byte| *byte == 0)
        .map(text_field)
        .collect::<Result<Vec<_>, _>>()?;
    if fields.last().is_some_and(String::is_empty) {
        fields.pop();
    }
    if fields.len() % 6 != 0 {
        return Err(GitError::Parse);
    }
    Ok(fields
        .chunks_exact(6)
        .map(|chunk| GitLogEntry {
            object_id: chunk[0].clone(),
            parents: chunk[1].split_whitespace().map(str::to_owned).collect(),
            author: chunk[2].clone(),
            email: chunk[3].clone(),
            authored_at: chunk[4].clone(),
            subject: chunk[5].clone(),
        })
        .collect())
}

/// Decodes filenames strictly so lossy replacement cannot merge two paths.
fn text_field(field: &[u8]) -> Result<String, GitError> {
    if field.contains(&0) {
        return Err(GitError::Parse);
    }
    String::from_utf8(field.to_vec()).map_err(|_| GitError::Parse)
}

/// Splits fixed porcelain fields while preserving spaces in the final path.
fn split_fields(record: &[u8], expected: usize) -> Result<Vec<String>, GitError> {
    let mut fields = record
        .splitn(expected, |byte| *byte == b' ')
        .map(text_field)
        .collect::<Result<Vec<_>, _>>()?;
    if fields.len() != expected {
        return Err(GitError::Parse);
    }
    if fields.first().is_none_or(|field| field.is_empty()) {
        return Err(GitError::Parse);
    }
    Ok(std::mem::take(&mut fields))
}
