// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Secure marker discovery and fixed-field parsing for workflow cleanup.

use super::process::current_uid;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MARKER_MODE: u32 = 0o600;
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;

/// A validated active marker whose path and identity are safe to hand to the
/// exact process cleanup routine.
#[derive(Debug, Clone)]
pub(super) struct MarkerRecord {
    pub(super) owner_pid: u32,
    pub(super) nonce: u128,
    pub(super) pid: u32,
    pub(super) pgid: i32,
    pub(super) start_identity: String,
    pub(super) executable_kind: String,
}

/// Discovery output keeps invalid/pending categories separate so no invalid
/// file can accidentally be promoted to a signal target.
#[derive(Debug)]
pub(super) struct ScanResult {
    pub(super) records: Vec<MarkerRecord>,
    pub(super) pending: Vec<PathBuf>,
    pub(super) categories: Vec<&'static str>,
}

/// Scan only the fixed marker filename grammar and preserve no raw file text
/// or path in the returned categories.
pub(super) fn scan_root(root: &Path, allow_fixture: bool) -> ScanResult {
    let mut result = ScanResult {
        records: Vec::new(),
        pending: Vec::new(),
        categories: Vec::new(),
    };
    // Validate the directory before enumeration so a replaced/symlinked
    // runner root cannot redirect marker discovery to an attacker-controlled
    // tree; file-level checks below then protect each signal target.
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    let root_mode = root_metadata.permissions().mode() & 0o777;
    if !root_metadata.file_type().is_dir()
        || root_metadata.file_type().is_symlink()
        || root_metadata.uid() != current_uid()
        || root_mode & 0o022 != 0
    {
        result.categories.push("marker-root-invalid");
        return result;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    scan_entries(entries, allow_fixture, &mut result);
    result
}

/// Walk directory entries through an injectable iterator; an entry read error
/// clears all pending signal targets so an incomplete scan can never proceed.
pub(super) fn scan_entries<I>(entries: I, allow_fixture: bool, result: &mut ScanResult)
where
    I: Iterator<Item = io::Result<fs::DirEntry>>,
{
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result.records.clear();
                result.pending.clear();
                result.categories.push("marker-entry-invalid");
                return;
            }
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if is_pending_name(name) {
            result.pending.push(path);
            continue;
        }
        let Some((owner_pid, nonce, _suffix)) = parse_marker_name(name) else {
            continue;
        };
        match parse_marker(&path, owner_pid, nonce, allow_fixture) {
            Ok(record) => result.records.push(record),
            Err(category) => result.categories.push(category),
        }
    }
}

/// Write a fixture marker through the same no-follow, owner-only open path as
/// production evidence; tests use it to exercise real cleanup, not a script.
#[cfg(target_os = "macos")]
pub(super) fn write_fixture_marker(path: &Path, contents: &str) -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| "marker-create-failed")?;
    validate_marker_file(&file).map_err(|_| "marker-stat-invalid")?;
    std::io::Write::write_all(&mut file, contents.as_bytes()).map_err(|_| "marker-write-failed")?;
    file.sync_all().map_err(|_| "marker-write-failed")?;
    validate_marker_file(&file).map_err(|_| "marker-stat-invalid")
}

/// Parse a trusted marker only after descriptor-bound metadata validation.
fn parse_marker(
    path: &Path,
    file_owner: u32,
    file_nonce: u128,
    allow_fixture: bool,
) -> Result<MarkerRecord, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| "marker-stat-invalid")?;
    validate_marker_file(&file).map_err(|_| "marker-stat-invalid")?;
    let mut contents = String::new();
    file.take(4096)
        .read_to_string(&mut contents)
        .map_err(|_| "marker-incomplete")?;
    parse_contents(file_owner, file_nonce, &contents, allow_fixture)
}

/// Validate exactly seven fixed fields, rejecting duplicates, unknown keys,
/// newlines in identity and fixture executable kinds in production mode.
fn parse_contents(
    file_owner: u32,
    file_nonce: u128,
    contents: &str,
    allow_fixture: bool,
) -> Result<MarkerRecord, &'static str> {
    let mut owner_pid = None;
    let mut nonce = None;
    let mut pid = None;
    let mut pgid = None;
    let mut start_identity = None;
    let mut executable_kind = None;
    let mut state = None;
    let mut line_count = 0;
    for line in contents.lines() {
        line_count += 1;
        let Some((key, value)) = line.split_once('=') else {
            return Err("marker-incomplete");
        };
        match key {
            "owner_pid" => set_once(&mut owner_pid, parse_positive_u32(value))?,
            "nonce" => set_once(
                &mut nonce,
                value.parse::<u128>().ok().filter(|value| *value > 0),
            )?,
            "pid" => set_once(&mut pid, parse_positive_u32(value))?,
            "pgid" => set_once(&mut pgid, parse_positive_i32(value))?,
            "start_identity" => set_once(
                &mut start_identity,
                valid_identity(value).then(|| value.to_owned()),
            )?,
            "executable_kind" => set_once(&mut executable_kind, Some(value.to_owned()))?,
            "state" => set_once(&mut state, Some(value.to_owned()))?,
            _ => return Err("marker-incomplete"),
        }
    }
    if line_count != 7 || state.as_deref() != Some("active") {
        return Err("marker-incomplete");
    }
    let owner_pid = owner_pid.ok_or("marker-owner-mismatch")?;
    let nonce = nonce.ok_or("marker-owner-mismatch")?;
    let pid = pid.ok_or("marker-owner-mismatch")?;
    let pgid = pgid.ok_or("marker-owner-mismatch")?;
    let start_identity = start_identity.ok_or("marker-owner-mismatch")?;
    let executable_kind = executable_kind.ok_or("marker-incomplete")?;
    if owner_pid != file_owner
        || nonce != file_nonce
        || pgid <= 1
        || pid <= 1
        || (executable_kind != "log" && !(allow_fixture && executable_kind == "fixture"))
    {
        return Err("marker-owner-mismatch");
    }
    Ok(MarkerRecord {
        owner_pid,
        nonce,
        pid,
        pgid,
        start_identity,
        executable_kind,
    })
}

/// Keep duplicate field handling explicit so a repeated key cannot hide an
/// attacker-controlled value behind the first occurrence.
fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> Result<(), &'static str> {
    if slot.is_some() || value.is_none() {
        return Err("marker-incomplete");
    }
    *slot = value;
    Ok(())
}

/// Require a strictly positive decimal PID without accepting signs or spaces.
fn parse_positive_u32(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|value| *value > 1)
}

/// Require a positive non-reserved process-group identifier.
fn parse_positive_i32(value: &str) -> Option<i32> {
    value.parse::<i32>().ok().filter(|value| *value > 1)
}

/// Keep the identity value short and field-safe; no process output path can
/// cross into the workflow report.
fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character == ' ' || character.is_ascii_graphic())
        && !value.contains('=')
}

/// Reject symlinks, hardlinks, wrong owners and mode changes on the opened
/// descriptor before its contents become a signal target.
fn validate_marker_file(file: &File) -> Result<(), ()> {
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.permissions().mode() & 0o777 != MARKER_MODE
        || metadata.nlink() != 1
    {
        return Err(());
    }
    Ok(())
}

/// Parse active names and bind filename owner/nonce to content fields.
fn parse_marker_name(name: &str) -> Option<(u32, u128, &str)> {
    let body = name.strip_prefix("ja-sandbox-log-helper-")?;
    let (ids, suffix) = body.rsplit_once('.')?;
    if !matches!(suffix, "marker" | "fallback" | "emergency") {
        return None;
    }
    let (owner, nonce) = ids.split_once('-')?;
    Some((
        owner.parse::<u32>().ok().filter(|value| *value > 0)?,
        nonce.parse::<u128>().ok().filter(|value| *value > 0)?,
        suffix,
    ))
}

/// Recognize pending names without parsing their incomplete identity fields.
fn is_pending_name(name: &str) -> bool {
    name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".marker.pending")
        || name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".fallback.pending")
        || name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".emergency.pending")
}
