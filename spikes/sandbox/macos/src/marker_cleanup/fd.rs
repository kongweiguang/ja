// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Small Darwin descriptor-relative filesystem adapter.
//!
//! The standard-library directory iterator is pathname based.  Marker cleanup
//! must keep the authorization namespace attached to the already verified root
//! descriptor, so this module owns the narrow `*at`/`fdopendir` calls and their
//! resource lifetimes in one auditable boundary.

use libc::{c_int, c_uint};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd, RawFd};
use std::ptr;
use std::slice;

pub(super) const O_CLOEXEC: c_int = libc::O_CLOEXEC;
pub(super) const O_NOFOLLOW: c_int = libc::O_NOFOLLOW;
pub(super) const O_RDONLY: c_int = libc::O_RDONLY;
pub(super) const O_WRONLY: c_int = libc::O_WRONLY;
pub(super) const O_CREAT: c_int = libc::O_CREAT;
pub(super) const O_EXCL: c_int = libc::O_EXCL;
pub(super) const AT_SYMLINK_NOFOLLOW: c_int = libc::AT_SYMLINK_NOFOLLOW;
pub(super) const F_DUPFD_CLOEXEC: c_int = libc::F_DUPFD_CLOEXEC;
pub(super) const ENOENT: i32 = libc::ENOENT;
pub(super) const EEXIST: i32 = libc::EEXIST;

/// Own a duplicated directory stream descriptor; `fdopendir` takes ownership
/// of the duplicate, never of the caller's verified root descriptor.
pub(super) struct DirectoryStream {
    directory: *mut libc::DIR,
}

impl DirectoryStream {
    /// Duplicate the root descriptor before `fdopendir`, because `closedir`
    /// closes its input and must not invalidate the cleanup root held by the
    /// caller.
    pub(super) fn from_fd(fd: RawFd) -> io::Result<Self> {
        let duplicate = unsafe { libc::fcntl(fd, F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            unsafe {
                libc::close(duplicate);
            }
            return Err(io::Error::last_os_error());
        }
        Ok(Self { directory })
    }

    /// Read one bounded directory name directly from the kernel-owned stream;
    /// an errno after a null result is a hard scan failure, not end-of-directory.
    pub(super) fn next_name(&mut self) -> io::Result<Option<Vec<u8>>> {
        set_errno(0);
        let entry = unsafe { libc::readdir(self.directory) };
        if entry.is_null() {
            let error = last_errno();
            return if error == 0 {
                Ok(None)
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
        }
        let entry = unsafe { &*entry };
        let length = usize::from(entry.d_namlen);
        if length == 0 || length > entry.d_name.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Darwin directory entry",
            ));
        }
        let bytes = unsafe { slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), length) };
        Ok(Some(bytes.to_vec()))
    }
}

impl Drop for DirectoryStream {
    /// Close only the duplicated stream descriptor; the caller's root fd stays
    /// open so later `openat`/`unlinkat` operations retain the same namespace.
    fn drop(&mut self) {
        if !self.directory.is_null() {
            unsafe {
                libc::closedir(self.directory);
            }
            self.directory = ptr::null_mut();
        }
    }
}

/// Open one regular marker relative to the verified root and refuse symlink
/// traversal at the final component before the caller inspects its metadata.
pub(super) fn open_at_file(root_fd: RawFd, name: &[u8]) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe { libc::openat(root_fd, name.as_ptr(), O_RDONLY | O_NOFOLLOW | O_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Create a recovery evidence file relative to the verified root; O_EXCL and
/// no-follow prevent an existing marker or symlink from being overwritten.
pub(super) fn create_at_file(root_fd: RawFd, name: &[u8], mode: c_uint) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe {
        libc::openat(
            root_fd,
            name.as_ptr(),
            O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Ask Darwin for the directory-entry stat without following a symlink; the
/// opened descriptor's `File::metadata` remains the authoritative identity
/// comparison used by marker cleanup.
pub(super) fn fstatat_no_follow(root_fd: RawFd, name: &[u8]) -> io::Result<()> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            root_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Rename one sibling inside the verified directory, preserving the original
/// marker as bounded recoverable evidence before destructive cleanup.
pub(super) fn rename_at(root_fd: RawFd, old_name: &[u8], new_name: &[u8]) -> io::Result<()> {
    let old_name =
        CString::new(old_name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let new_name =
        CString::new(new_name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let result = unsafe { libc::renameat(root_fd, old_name.as_ptr(), root_fd, new_name.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Publish a sibling without replacing an already durable final entry.
/// Darwin's `RENAME_EXCL` keeps the pending-to-final transaction atomic while
/// making an unexpected concurrent final creation a fail-closed error rather
/// than destroying the only good evidence copy.
pub(super) fn rename_at_no_replace(
    root_fd: RawFd,
    old_name: &[u8],
    new_name: &[u8],
) -> io::Result<()> {
    let old_name =
        CString::new(old_name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let new_name =
        CString::new(new_name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let result = unsafe {
        libc::renameatx_np(
            root_fd,
            old_name.as_ptr(),
            root_fd,
            new_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Remove one exact sibling only; callers must perform identity and deadline
/// checks before and after this syscall and must sync the parent directory.
pub(super) fn unlink_at(root_fd: RawFd, name: &[u8]) -> io::Result<()> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let result = unsafe { libc::unlinkat(root_fd, name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Keep errno access in one libc boundary so scan EOF and unlink failures
/// never depend on locale text or hand-written platform symbols.
pub(super) fn last_errno() -> i32 {
    unsafe { *libc::__error() }
}

fn set_errno(value: i32) {
    unsafe {
        *libc::__error() = value;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DirectoryStream, ENOENT, create_at_file, fstatat_no_follow, open_at_file, rename_at,
        rename_at_no_replace, unlink_at,
    };
    use std::fs::{self, DirBuilder, OpenOptions};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Exercise the complete descriptor-relative lifecycle so production
    /// cleanup cannot regress to synthetic descriptor paths or pathname-based
    /// deletion.
    #[test]
    fn descriptor_relative_scan_rename_and_unlink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ja-fd-adapter-{}-{nonce}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("private root");
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(super::O_NOFOLLOW | super::O_CLOEXEC)
            .open(&root)
            .expect("root fd");
        let root_fd = directory.as_raw_fd();
        let mut marker = create_at_file(root_fd, b"entry", 0o600).expect("marker create");
        marker.write_all(b"marker").expect("marker write");
        marker.sync_all().expect("marker sync");
        fstatat_no_follow(root_fd, b"entry").expect("marker stat");
        open_at_file(root_fd, b"entry").expect("marker open");

        let mut entries = DirectoryStream::from_fd(root_fd).expect("directory stream");
        let mut found = false;
        for _ in 0..32 {
            match entries.next_name().expect("directory read") {
                Some(name) if name == b"entry" => {
                    found = true;
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(found, "descriptor stream omitted marker");
        rename_at(root_fd, b"entry", b".ja-sandbox-cleaned.entry").expect("marker rename");
        fstatat_no_follow(root_fd, b".ja-sandbox-cleaned.entry").expect("tombstone stat");
        unlink_at(root_fd, b".ja-sandbox-cleaned.entry").expect("marker unlink");
        assert_eq!(
            fstatat_no_follow(root_fd, b".ja-sandbox-cleaned.entry")
                .expect_err("unlinked marker still present")
                .raw_os_error(),
            Some(ENOENT)
        );
        directory.sync_all().expect("directory sync");
        assert_eq!(
            fs::metadata(&root)
                .expect("root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir(&root).expect("private root cleanup");
    }

    /// Refuse to replace a pre-existing final sibling so a late concurrent
    /// writer cannot destroy the only complete recovery evidence.
    #[test]
    fn exclusive_rename_preserves_existing_destination() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ja-fd-exclusive-{0}-{nonce}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .expect("private root");
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(super::O_NOFOLLOW | super::O_CLOEXEC)
            .open(&root)
            .expect("root fd");
        let root_fd = directory.as_raw_fd();
        let mut pending = create_at_file(root_fd, b"pending", 0o600).expect("pending");
        pending.write_all(b"pending").expect("pending write");
        pending.sync_all().expect("pending sync");
        let mut final_file = create_at_file(root_fd, b"final", 0o600).expect("final");
        final_file.write_all(b"good").expect("final write");
        final_file.sync_all().expect("final sync");
        assert!(rename_at_no_replace(root_fd, b"pending", b"final").is_err());
        assert_eq!(
            fs::read(root.join("final")).expect("final contents"),
            b"good"
        );
        assert_eq!(
            fs::read(root.join("pending")).expect("pending contents"),
            b"pending"
        );
        drop(final_file);
        drop(pending);
        directory.sync_all().expect("directory sync");
        fs::remove_dir_all(&root).expect("private root cleanup");
    }
}
