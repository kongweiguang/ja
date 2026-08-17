// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Small, private atomic-file primitives shared by the settings store.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Writes a bounded payload through a same-directory temp file so a crash
/// cannot leave a partially-written JSON document at the target path.
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|candidate| !candidate.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    fs::create_dir_all(parent)?;

    let temp = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        replace_existing(&temp, path)?;
        sync_parent(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Reads a file with a preflight and post-read size check so a concurrent
/// writer cannot make the parser allocate beyond its documented limit.
pub(crate) fn read_bounded(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bounded settings file rejected",
        ));
    }
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bounded settings file grew",
        ));
    }
    Ok(bytes)
}

/// Copies an existing file through the same atomic path used for settings so
/// the backup is complete before the primary document is replaced.
pub(crate) fn atomic_copy(source: &Path, target: &Path, max_bytes: usize) -> io::Result<()> {
    let bytes = read_bounded(source, max_bytes)?;
    atomic_replace(target, &bytes)
}

/// Uses an unpredictable same-directory name so concurrent instances cannot
/// accidentally share or overwrite a partially-written settings temp file.
fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    path.with_file_name(format!(".{name}.tmp-{}", Uuid::new_v4()))
}

/// Replaces the target while retaining a restorable displaced file on Windows
/// where rename cannot overwrite an open destination.
fn replace_existing(source: &Path, target: &Path) -> io::Result<()> {
    if fs::rename(source, target).is_ok() {
        return Ok(());
    }

    // Windows does not replace an existing file with rename.  Moving the old
    // target aside first preserves a recoverable copy if the second rename
    // fails; the caller already maintains a durable backup before this step.
    let displaced = target.with_file_name(format!(
        ".{}.old-{}",
        target
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("settings.json"),
        Uuid::new_v4()
    ));
    let had_target = target.exists();
    if had_target {
        fs::rename(target, &displaced)?;
    }
    match fs::rename(source, target) {
        Ok(()) => {
            if had_target {
                let _ = fs::remove_file(displaced);
            }
            Ok(())
        }
        Err(error) => {
            if had_target && !target.exists() {
                let _ = fs::rename(&displaced, target);
            }
            Err(error)
        }
    }
}

#[cfg(unix)]
/// Flushes the directory entry itself because file fsync alone does not make
/// a rename durable after a power loss on Unix filesystems.
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
/// Keeps the same call site on Windows, whose standard library has no portable
/// directory fsync primitive.
fn sync_parent(_parent: &Path) -> io::Result<()> {
    // Windows has no portable directory fsync API in std; the file handle was
    // flushed and synced before rename, which is the strongest portable
    // guarantee available without introducing platform FFI.
    Ok(())
}
