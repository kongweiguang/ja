// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Secure marker discovery and fixed-field parsing for workflow cleanup.

use super::fd;
use super::process::current_uid;
use std::fs::{self, File, OpenOptions};
#[cfg(test)]
use std::io;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

const MARKER_MODE: u32 = 0o600;
const MAX_SCAN_ENTRIES: usize = 128;
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;

/// A validated active marker whose path and identity are safe to hand to the
/// exact process cleanup routine.
#[derive(Debug, Clone)]
pub(super) struct MarkerRecord {
    pub(super) path: PathBuf,
    pub(super) suffix: String,
    pub(super) file_identity: MarkerFileIdentity,
    pub(super) owner_pid: u32,
    pub(super) nonce: u128,
    pub(super) pid: u32,
    pub(super) pgid: i32,
    pub(super) start_identity: String,
    pub(super) executable_kind: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MarkerFileIdentity {
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) nlink: u64,
    pub(super) mode: u32,
    pub(super) uid: u32,
}

#[derive(Debug)]
pub(super) struct PendingMarker {
    pub(super) path: PathBuf,
    pub(super) file_identity: MarkerFileIdentity,
    pub(super) cleaned: bool,
    pub(super) cleaned_record: Option<MarkerRecord>,
    /// A controlled cleaned/recovery basename whose image is malformed but
    /// whose owner-only inode is still safe to pair with its valid sibling;
    /// keeping this bit separate prevents arbitrary stat-invalid files from
    /// becoming deletion targets.
    pub(super) damaged: bool,
    /// Distinguish a pending alias from an active cleaned/recovery marker so
    /// cleanup never feeds a `.pending` basename into active-marker grammar.
    pub(super) pending_recovery: bool,
}

/// Discovery output keeps invalid/pending categories separate so no invalid
/// file can accidentally be promoted to a signal target.
#[derive(Debug)]
pub(super) struct ScanResult {
    pub(super) records: Vec<MarkerRecord>,
    pub(super) pending: Vec<PendingMarker>,
    pub(super) categories: Vec<&'static str>,
}

/// Scan only the fixed marker filename grammar and preserve no raw file text
/// or path in the returned categories.
#[cfg(test)]
pub(super) fn scan_root(root: &Path, allow_fixture: bool) -> ScanResult {
    let mut result = ScanResult {
        records: Vec::new(),
        pending: Vec::new(),
        categories: Vec::new(),
    };
    // Open first with no-follow semantics so enumeration is tied to the same
    // directory object that cleanup later uses for descriptor-relative
    // opens/unlinks; a pathname-only scan could authorize one tree and delete
    // from another after a root replacement.
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(root)
    {
        Ok(directory) => directory,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    scan_root_from_directory(
        root,
        &directory,
        allow_fixture,
        Instant::now() + Duration::from_secs(20),
    )
}

/// Enumerate a previously verified directory descriptor with Darwin's
/// `fdopendir/readdir` API.  The stream owns only a duplicated fd, so every
/// marker open remains relative to the exact root descriptor held by cleanup.
pub(super) fn scan_root_from_directory(
    root: &Path,
    directory: &File,
    allow_fixture: bool,
    deadline: Instant,
) -> ScanResult {
    let mut result = ScanResult {
        records: Vec::new(),
        pending: Vec::new(),
        categories: Vec::new(),
    };
    if Instant::now() >= deadline {
        result.categories.push("marker-query-unreaped");
        return result;
    }
    let descriptor_metadata = match directory.metadata() {
        Ok(metadata) => metadata,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    if Instant::now() >= deadline {
        result.categories.push("marker-query-unreaped");
        return result;
    }
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    if Instant::now() >= deadline {
        result.categories.push("marker-query-unreaped");
        return result;
    }
    let descriptor_mode = descriptor_metadata.permissions().mode() & 0o777;
    let root_mode = root_metadata.permissions().mode() & 0o777;
    if !descriptor_metadata.file_type().is_dir()
        || root_metadata.file_type().is_symlink()
        || descriptor_metadata.uid() != current_uid()
        || descriptor_mode != 0o700
        || root_metadata.uid() != descriptor_metadata.uid()
        || root_mode != descriptor_mode
        || root_metadata.dev() != descriptor_metadata.dev()
        || root_metadata.ino() != descriptor_metadata.ino()
        || root_metadata.nlink() != descriptor_metadata.nlink()
    {
        result.categories.push("marker-root-invalid");
        return result;
    }
    let mut entries = match fd::DirectoryStream::from_fd(std::os::fd::AsRawFd::as_raw_fd(directory))
    {
        Ok(entries) => entries,
        Err(_) => {
            result.categories.push("marker-root-invalid");
            return result;
        }
    };
    scan_fd_entries(
        &mut entries,
        std::os::fd::AsRawFd::as_raw_fd(directory),
        root,
        allow_fixture,
        &mut result,
        deadline,
    );
    result
}

/// Read and validate every marker using only names returned by the held
/// descriptor stream; no synthetic descriptor pathname or root pathname is
/// used for the authorization read.
fn scan_fd_entries(
    entries: &mut fd::DirectoryStream,
    root_fd: std::os::fd::RawFd,
    root: &Path,
    allow_fixture: bool,
    result: &mut ScanResult,
    deadline: Instant,
) {
    let mut entry_count = 0;
    loop {
        if Instant::now() >= deadline {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-query-unreaped");
            return;
        }
        let name_bytes = match entries.next_name() {
            Ok(Some(name)) => name,
            Ok(None) => return,
            Err(_) => {
                result.records.clear();
                result.pending.clear();
                result.categories.push("marker-entry-invalid");
                return;
            }
        };
        entry_count += 1;
        if entry_count > MAX_SCAN_ENTRIES {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-entry-invalid");
            return;
        }
        let Ok(name) = std::str::from_utf8(&name_bytes) else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        let display_path = root.join(name);
        if let Some(original) = parse_cleaned_name(name) {
            let Some((owner_pid, nonce, suffix)) = parse_marker_name(original) else {
                result.categories.push("marker-entry-invalid");
                continue;
            };
            if fd::fstatat_no_follow(root_fd, name_bytes.as_slice()).is_err() {
                result.categories.push("marker-entry-invalid");
                continue;
            }
            let file = match fd::open_at_file(root_fd, name_bytes.as_slice()) {
                Ok(file) => file,
                Err(_) => {
                    result.categories.push("marker-stat-invalid");
                    continue;
                }
            };
            // A recovery file is not merely metadata: validating the complete
            // original marker grammar prevents a short write from becoming a
            // silent clean state on the next bounded cleanup pass.
            let file_identity = validate_marker_file(&file).ok();
            match parse_marker_from_file(
                file,
                &display_path,
                owner_pid,
                nonce,
                suffix,
                allow_fixture,
            ) {
                Ok(record) => result.pending.push(PendingMarker {
                    path: display_path,
                    file_identity: record.file_identity,
                    cleaned: true,
                    cleaned_record: Some(record),
                    damaged: false,
                    pending_recovery: false,
                }),
                Err(_) => {
                    if let Some(file_identity) = file_identity {
                        // The basename is a recognized pair member and the
                        // inode is owner-only, so the parent cleanup state
                        // machine may remove it only after proving the valid
                        // sibling is durable.  An arbitrary invalid file is
                        // still reported and never promoted to this branch.
                        result.pending.push(PendingMarker {
                            path: display_path,
                            file_identity,
                            cleaned: true,
                            cleaned_record: None,
                            damaged: true,
                            pending_recovery: false,
                        });
                    } else {
                        result.categories.push("marker-stat-invalid");
                    }
                }
            }
            continue;
        }
        if let Some(_pending_name) = parse_pending_alias_name(name) {
            if fd::fstatat_no_follow(root_fd, name_bytes.as_slice()).is_err() {
                result.categories.push("marker-entry-invalid");
                continue;
            }
            let file = match fd::open_at_file(root_fd, name_bytes.as_slice()) {
                Ok(file) => file,
                Err(_) => {
                    result.categories.push("marker-stat-invalid");
                    continue;
                }
            };
            match validate_marker_file(&file) {
                Ok(file_identity) => result.pending.push(PendingMarker {
                    path: display_path,
                    file_identity,
                    cleaned: false,
                    cleaned_record: None,
                    damaged: false,
                    pending_recovery: true,
                }),
                Err(_) => result.categories.push("marker-stat-invalid"),
            }
            continue;
        }
        if is_pending_name(name) {
            if parse_pending_name(name).is_none()
                || fd::fstatat_no_follow(root_fd, name_bytes.as_slice()).is_err()
            {
                result.categories.push("marker-owner-mismatch");
                continue;
            }
            let file = match fd::open_at_file(root_fd, name_bytes.as_slice()) {
                Ok(file) => file,
                Err(_) => {
                    result.categories.push("marker-stat-invalid");
                    continue;
                }
            };
            match validate_marker_file(&file) {
                Ok(file_identity) => result.pending.push(PendingMarker {
                    path: display_path,
                    file_identity,
                    cleaned: false,
                    cleaned_record: None,
                    damaged: false,
                    pending_recovery: false,
                }),
                Err(_) => result.categories.push("marker-stat-invalid"),
            }
            continue;
        }
        let Some((owner_pid, nonce, suffix)) = parse_marker_name(name) else {
            continue;
        };
        if fd::fstatat_no_follow(root_fd, name_bytes.as_slice()).is_err() {
            result.categories.push("marker-stat-invalid");
            continue;
        }
        let file = match fd::open_at_file(root_fd, name_bytes.as_slice()) {
            Ok(file) => file,
            Err(_) => {
                result.categories.push("marker-stat-invalid");
                continue;
            }
        };
        match parse_marker_from_file(file, &display_path, owner_pid, nonce, suffix, allow_fixture) {
            Ok(record) => result.records.push(record),
            Err(category) => result.categories.push(category),
        }
        if Instant::now() >= deadline {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-query-unreaped");
            return;
        }
    }
}

/// Walk directory entries through an injectable iterator; an entry read error
/// clears all pending signal targets so an incomplete scan can never proceed.
#[cfg(test)]
pub(super) fn scan_entries<I>(entries: I, allow_fixture: bool, result: &mut ScanResult)
where
    I: Iterator<Item = io::Result<fs::DirEntry>>,
{
    scan_entries_at(
        entries,
        Path::new(""),
        None,
        allow_fixture,
        result,
        Instant::now() + Duration::from_secs(20),
    );
}

/// Walk an injectable iterator for parser tests; production uses the separate
/// Darwin `fdopendir/readdir` path and never falls back to pathname scanning.
#[cfg(test)]
fn scan_entries_at<I>(
    entries: I,
    display_root: &Path,
    namespace: Option<&Path>,
    allow_fixture: bool,
    result: &mut ScanResult,
    deadline: Instant,
) where
    I: Iterator<Item = io::Result<fs::DirEntry>>,
{
    let mut entry_count = 0;
    for entry in entries {
        if Instant::now() >= deadline {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-query-unreaped");
            return;
        }
        entry_count += 1;
        if entry_count > MAX_SCAN_ENTRIES {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-entry-invalid");
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result.records.clear();
                result.pending.clear();
                result.categories.push("marker-entry-invalid");
                return;
            }
        };
        let entry_path = entry.path();
        let Some(name) = entry_path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let read_path = namespace
            .map(|namespace| namespace.join(name))
            .unwrap_or_else(|| entry_path.clone());
        let display_path = if display_root.as_os_str().is_empty() {
            entry_path.clone()
        } else {
            display_root.join(name)
        };
        if is_pending_name(name) {
            if parse_pending_name(name).is_none() {
                result.categories.push("marker-owner-mismatch");
                continue;
            }
            match marker_path_identity(&read_path) {
                Ok(file_identity) => result.pending.push(PendingMarker {
                    path: display_path,
                    file_identity,
                    cleaned: false,
                    cleaned_record: None,
                    damaged: false,
                    pending_recovery: false,
                }),
                Err(_) => result.categories.push("marker-stat-invalid"),
            }
            if Instant::now() >= deadline {
                result.records.clear();
                result.pending.clear();
                result.categories.push("marker-query-unreaped");
                return;
            }
            continue;
        }
        let Some((owner_pid, nonce, suffix)) = parse_marker_name(name) else {
            continue;
        };
        match parse_marker(
            &read_path,
            &display_path,
            owner_pid,
            nonce,
            suffix,
            allow_fixture,
        ) {
            Ok(record) => result.records.push(record),
            Err(category) => result.categories.push(category),
        }
        if Instant::now() >= deadline {
            result.records.clear();
            result.pending.clear();
            result.categories.push("marker-query-unreaped");
            return;
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
    validate_marker_file(&file)
        .map(|_| ())
        .map_err(|_| "marker-stat-invalid")
}

/// Parse a trusted marker only after descriptor-bound metadata validation.
#[cfg(test)]
fn parse_marker(
    read_path: &Path,
    display_path: &Path,
    file_owner: u32,
    file_nonce: u128,
    suffix: &str,
    allow_fixture: bool,
) -> Result<MarkerRecord, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(read_path)
        .map_err(|_| "marker-stat-invalid")?;
    parse_marker_from_file(
        file,
        display_path,
        file_owner,
        file_nonce,
        suffix,
        allow_fixture,
    )
}

/// Parse an already descriptor-relative marker file; keeping the file handle
/// from `openat` through content and metadata validation prevents a pathname
/// replacement from changing what was authorized.
fn parse_marker_from_file(
    file: File,
    display_path: &Path,
    file_owner: u32,
    file_nonce: u128,
    suffix: &str,
    allow_fixture: bool,
) -> Result<MarkerRecord, &'static str> {
    let file_identity = validate_marker_file(&file).map_err(|_| "marker-stat-invalid")?;
    let mut contents = String::new();
    file.take(4097)
        .read_to_string(&mut contents)
        .map_err(|_| "marker-incomplete")?;
    if contents.len() > 4096 {
        return Err("marker-incomplete");
    }
    let mut record = parse_contents(file_owner, file_nonce, &contents, allow_fixture)?;
    record.path = display_path.to_owned();
    record.suffix = suffix.to_owned();
    record.file_identity = file_identity;
    Ok(record)
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
        path: PathBuf::new(),
        suffix: String::new(),
        file_identity: MarkerFileIdentity {
            dev: 0,
            ino: 0,
            nlink: 1,
            mode: MARKER_MODE,
            uid: current_uid(),
        },
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
fn validate_marker_file(file: &File) -> Result<MarkerFileIdentity, ()> {
    let metadata = file.metadata().map_err(|_| ())?;
    let identity = MarkerFileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        mode: metadata.permissions().mode() & 0o777,
        uid: metadata.uid(),
    };
    if !metadata.file_type().is_file()
        || identity.uid != current_uid()
        || identity.mode != MARKER_MODE
        || identity.nlink != 1
    {
        return Err(());
    }
    Ok(identity)
}

/// Capture an owner-only regular marker inode before a pending file can be
/// removed; deletion callers must compare this identity again immediately.
#[cfg(test)]
fn marker_path_identity(path: &Path) -> Result<MarkerFileIdentity, ()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG)
        .open(path)
        .map_err(|_| ())?;
    validate_marker_file(&file)
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
        owner.parse::<u32>().ok().filter(|value| *value > 1)?,
        nonce.parse::<u128>().ok().filter(|value| *value > 0)?,
        suffix,
    ))
}

/// Expose only the boolean grammar check needed by the parent cleanup state
/// machine; keeping field extraction private prevents backup-name code from
/// manufacturing a process target from an unchecked basename.
pub(super) fn parse_marker_name_for_cleanup(name: &str) -> Option<()> {
    parse_marker_name(name).map(|_| ())
}

/// Reparse a cleaned/recovery image after reopening its expected inode so a
/// valid sibling cannot authorize cleanup using stale pre-scan bytes.
pub(super) fn parse_cleaned_record_from_file(
    file: File,
    display_path: &Path,
    allow_fixture: bool,
) -> Result<MarkerRecord, &'static str> {
    let name = display_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("marker-entry-invalid")?;
    let original = parse_cleaned_name(name).ok_or("marker-entry-invalid")?;
    let (owner_pid, nonce, suffix) = parse_marker_name(original).ok_or("marker-entry-invalid")?;
    parse_marker_from_file(file, display_path, owner_pid, nonce, suffix, allow_fixture)
}

/// Recognize only cleanup tombstones created by the same finite-state marker
/// transition; arbitrary dotfiles never become deletion targets.
fn parse_cleaned_name(name: &str) -> Option<&str> {
    let original = name
        .strip_prefix(".ja-sandbox-cleaned.")
        .or_else(|| name.strip_prefix(".ja-sandbox-recovery."))?;
    parse_marker_name(original).map(|_| original)
}

/// Recognize pending names without parsing their incomplete identity fields.
fn is_pending_name(name: &str) -> bool {
    name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".marker.pending")
        || name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".fallback.pending")
        || name.starts_with(".ja-sandbox-log-helper-") && name.ends_with(".emergency.pending")
}

/// Recognize only the two fixed pending recovery aliases.  The nested
/// `.pending` basename is parsed by the dedicated pending grammar, never by
/// the active marker parser, so arbitrary nested names cannot be authorized.
fn parse_pending_alias_name(name: &str) -> Option<&str> {
    let pending = name
        .strip_prefix(".ja-sandbox-recovery.")
        .or_else(|| name.strip_prefix(".ja-sandbox-cleaned."))?;
    parse_pending_name(pending).map(|_| pending)
}

/// Expose the finite pending-alias grammar to the descriptor-relative cleanup
/// state machine without exposing active marker field extraction.
pub(super) fn parse_pending_alias_for_cleanup(name: &str) -> Option<&str> {
    parse_pending_alias_name(name)
}

/// Expose the ordinary pending basename grammar for the same state machine.
pub(super) fn parse_pending_name_for_cleanup(name: &str) -> Option<()> {
    parse_pending_name(name).map(|_| ())
}

/// Parse pending owner/nonce names too; pending files carry no trusted
/// process identity, but reserved owner values must still fail closed.
fn parse_pending_name(name: &str) -> Option<(u32, u128)> {
    let body = name.strip_prefix(".ja-sandbox-log-helper-")?;
    let mut ids = None;
    for suffix in [".marker.pending", ".fallback.pending", ".emergency.pending"] {
        if let Some(value) = body.strip_suffix(suffix) {
            ids = Some(value);
            break;
        }
    }
    let ids = ids?;
    let (owner, nonce) = ids.split_once('-')?;
    Some((
        owner.parse::<u32>().ok().filter(|value| *value > 1)?,
        nonce.parse::<u128>().ok().filter(|value| *value > 0)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        parse_pending_alias_name, parse_pending_name, parse_positive_i32, parse_positive_u32,
    };

    /// Marker parsing rejects all reserved PID/PGID values before cleanup can
    /// derive a direct or negative process-group signal target.
    #[test]
    fn reserved_marker_ids_are_rejected() {
        for value in ["-1", "0", "1"] {
            assert!(parse_positive_u32(value).is_none());
            assert!(parse_positive_i32(value).is_none());
        }
    }

    /// Pending activation suffixes contain two dots; parsing the complete
    /// suffix prevents valid pending evidence from being misclassified as an
    /// owner mismatch before cleanup can remove it.
    #[test]
    fn pending_name_grammar_is_complete() {
        assert_eq!(
            parse_pending_name(".ja-sandbox-log-helper-42-12.marker.pending"),
            Some((42, 12))
        );
        assert_eq!(
            parse_pending_name(".ja-sandbox-log-helper-42-12.fallback.pending"),
            Some((42, 12))
        );
        assert!(parse_pending_name(".ja-sandbox-log-helper-42-12.marker").is_none());
    }

    /// Pending aliases accept only the two fixed prefixes and one complete
    /// pending basename; active-marker-looking nested names remain rejected.
    #[test]
    fn pending_alias_grammar_is_finite() {
        assert_eq!(
            parse_pending_alias_name(
                ".ja-sandbox-recovery..ja-sandbox-log-helper-42-12.marker.pending"
            ),
            Some(".ja-sandbox-log-helper-42-12.marker.pending")
        );
        assert_eq!(
            parse_pending_alias_name(
                ".ja-sandbox-cleaned..ja-sandbox-log-helper-42-12.fallback.pending"
            ),
            Some(".ja-sandbox-log-helper-42-12.fallback.pending")
        );
        assert!(
            parse_pending_alias_name(".ja-sandbox-recovery.ja-sandbox-log-helper-42-12.marker")
                .is_none()
        );
        assert!(parse_pending_alias_name(
            ".ja-sandbox-recovery..ja-sandbox-recovery.ja-sandbox-log-helper-42-12.marker.pending"
        )
        .is_none());
    }
}
