// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Owner-bound secure marker preparation, activation and inode validation.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MARKER_EXECUTABLE_KIND: &str = "log";
const MARKER_STATE_ACTIVE: &str = "active";
const MARKER_STATE_PREPARED: &str = "prepared";
const MARKER_MODE: u32 = 0o600;
const O_NOFOLLOW_FLAG: i32 = 0x0000_0100;
const O_CLOEXEC_FLAG: i32 = 0x0100_0000;
const O_DIRECTORY_FLAG: i32 = 0x0010_0000;

/// Owner-bound marker paths are prepared before spawning so a post-spawn
/// metadata write failure still leaves independent evidence for exact cleanup.
pub(super) struct PreparedHelperMarker {
    pub(super) path: PathBuf,
    pub(super) fallback_path: PathBuf,
    pub(super) emergency_path: PathBuf,
    pub(super) owner_pid: u32,
    pub(super) nonce: u128,
}

/// Prepare owner-only primary, fallback and emergency files before spawning
/// `log`; this proves the runner directory is safe and leaves evidence if
/// activation fails.
pub(super) fn prepare_helper_marker() -> io::Result<PreparedHelperMarker> {
    let root = env::var_os("RUNNER_TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let metadata = fs::symlink_metadata(&root)?;
    let mode = metadata.permissions().mode() & 0o777;
    let owner_uid = unsafe { geteuid() };
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || !marker_directory_mode_safe(mode)
        || metadata.uid() != owner_uid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "marker directory is not owner-private",
        ));
    }
    let owner_pid = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let stem = format!("ja-sandbox-log-helper-{owner_pid}-{nonce}");
    let prepared = PreparedHelperMarker {
        path: root.join(format!("{stem}.marker")),
        fallback_path: root.join(format!("{stem}.fallback")),
        emergency_path: root.join(format!("{stem}.emergency")),
        owner_pid,
        nonce,
    };
    if let Err(error) = write_prepared_marker(&prepared.path, prepared.owner_pid, prepared.nonce) {
        let _ = fs::remove_file(&prepared.path);
        return Err(error);
    }
    if let Err(error) =
        write_prepared_marker(&prepared.fallback_path, prepared.owner_pid, prepared.nonce)
    {
        let _ = fs::remove_file(&prepared.path);
        let _ = fs::remove_file(&prepared.fallback_path);
        return Err(error);
    }
    if let Err(error) =
        write_prepared_marker(&prepared.emergency_path, prepared.owner_pid, prepared.nonce)
    {
        let _ = fs::remove_file(&prepared.path);
        let _ = fs::remove_file(&prepared.fallback_path);
        let _ = fs::remove_file(&prepared.emergency_path);
        return Err(error);
    }
    Ok(prepared)
}

/// Require a marker root that another local user cannot modify or replace;
/// marker contents remain owner-only through each file's 0600 mode.
pub(super) fn marker_directory_mode_safe(mode: u32) -> bool {
    mode & 0o022 == 0
}

/// Write a prepared marker with a create-time 0600 mode, preventing a
/// create-then-chmod visibility window for another local user.
pub(super) fn write_prepared_marker(path: &Path, owner_pid: u32, nonce: u128) -> io::Result<()> {
    let contents = format!("owner_pid={owner_pid}\nnonce={nonce}\nstate={MARKER_STATE_PREPARED}\n");
    write_new_marker_file(path, &contents)
}

/// Activate all independent marker files atomically; in-place rewrite is only
/// the fail-safe fallback that preserves exact identity evidence if rename
/// fails. A third emergency file prevents two-path activation failures from
/// silently losing the helper's exact PID/PGID identity.
pub(super) fn activate_helper_marker(
    prepared: &PreparedHelperMarker,
    pid: u32,
    process_group: i32,
    start_identity: &str,
) -> io::Result<()> {
    let contents = marker_contents(
        prepared.owner_pid,
        prepared.nonce,
        pid,
        process_group,
        start_identity,
    )?;
    let primary = activate_marker_file(&prepared.path, &contents);
    let fallback = activate_marker_file(&prepared.fallback_path, &contents);
    let emergency = activate_marker_file(&prepared.emergency_path, &contents);
    if primary.is_ok() && fallback.is_ok() && emergency.is_ok() {
        Ok(())
    } else {
        match primary
            .err()
            .or_else(|| fallback.err())
            .or_else(|| emergency.err())
        {
            Some(error) => Err(error),
            None => Err(io::Error::other("marker activation failed")),
        }
    }
}

/// Build a fixed-field marker payload without allowing process output or paths
/// to cross the workflow boundary.
pub(super) fn marker_contents(
    owner_pid: u32,
    nonce: u128,
    pid: u32,
    process_group: i32,
    start_identity: &str,
) -> io::Result<String> {
    if !super::query::valid_start_identity(start_identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid process start identity",
        ));
    }
    Ok(format!(
        "owner_pid={owner_pid}\nnonce={nonce}\npid={pid}\npgid={process_group}\nstart_identity={start_identity}\nexecutable_kind={MARKER_EXECUTABLE_KIND}\nstate={MARKER_STATE_ACTIVE}\n"
    ))
}

/// Atomically replace a prepared marker and keep the prepared file as
/// fallback evidence if the filesystem rejects the rename operation.
fn activate_marker_file(path: &Path, contents: &str) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "marker path has no file name")
    })?;
    // Keep primary and fallback pending names distinct; `with_extension` would
    // map both `.marker` and `.fallback` to one shared temporary file.
    let pending = path.with_file_name(format!(".{}.pending", file_name.to_string_lossy()));
    let result = (|| {
        write_new_marker_file(&pending, contents)?;
        fs::rename(&pending, path)?;
        sync_parent_directory(path)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&pending);
            let _ = write_marker_file(path, contents);
            Err(error)
        }
    }
}

/// Write an existing owner-only marker while retaining the same fixed fields
/// after an atomic rename failure; this path never clears the fallback.
pub(super) fn write_marker_file(path: &Path, contents: &str) -> io::Result<()> {
    // Validate the opened descriptor before truncation: checking the path
    // first would allow a same-user symlink/hardlink swap to redirect writes.
    let mut marker = OpenOptions::new()
        .write(true)
        .custom_flags(marker_open_flags())
        .open(path)?;
    let identity = validate_marker_file(&marker)?;
    marker.set_len(0)?;
    marker.write_all(contents.as_bytes())?;
    marker.sync_all()?;
    validate_marker_file_identity(&marker, identity)?;
    sync_parent_directory(path)
}

/// Create a new marker with the restrictive mode applied in the open syscall.
fn write_new_marker_file(path: &Path, contents: &str) -> io::Result<()> {
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MARKER_MODE)
        .custom_flags(marker_open_flags())
        .open(path)?;
    let identity = validate_marker_file(&marker)?;
    marker.write_all(contents.as_bytes())?;
    marker.sync_all()?;
    validate_marker_file_identity(&marker, identity)
}

/// Open marker files with no-follow/close-on-exec flags so descriptor checks
/// remain bound to the inode that will actually receive evidence.
pub(super) fn marker_open_flags() -> i32 {
    O_NOFOLLOW_FLAG | O_CLOEXEC_FLAG
}

#[derive(Clone, Copy)]
pub(super) struct MarkerFileIdentity {
    device: u64,
    inode: u64,
}

/// Enforce owner-only regular-file evidence before any marker write.
pub(super) fn validate_marker_file(file: &File) -> io::Result<MarkerFileIdentity> {
    let metadata = file.metadata()?;
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { geteuid() }
        || mode != MARKER_MODE
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "marker inode is not owner-only",
        ));
    }
    Ok(MarkerFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Reject inode replacement or a newly-created hardlink after the write.
fn validate_marker_file_identity(file: &File, expected: MarkerFileIdentity) -> io::Result<()> {
    let actual = validate_marker_file(file)?;
    if actual.device != expected.device || actual.inode != expected.inode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "marker inode changed",
        ));
    }
    Ok(())
}

/// Make the rename durable as far as the filesystem API allows by syncing the
/// marker's parent directory after the atomic name transition.
pub(super) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY_FLAG | marker_open_flags())
        .open(parent)?;
    directory.sync_all()
}

unsafe extern "C" {
    fn geteuid() -> u32;
}
