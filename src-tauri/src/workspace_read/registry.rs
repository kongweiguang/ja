// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::WorkspaceError;
use super::model::{EntryKind, FileMetadata, FileRevision};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, UNIX_EPOCH};
use uuid::Uuid;

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// Opaque workspace identity; absolute roots remain native-only state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(Uuid);

impl WorkspaceId {
    /// Creates a random identity so a persisted UI id cannot be confused with
    /// a path after a workspace is removed and later re-added.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for WorkspaceId {
    /// Uses the same random identity path as explicit construction.
    fn default() -> Self {
        Self::new()
    }
}

/// Public workspace summary intentionally omits the canonical root path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
}

/// A trusted canonical root plus the opaque id used by all workbench readers.
#[derive(Debug, Clone)]
pub struct WorkspaceHandle {
    id: WorkspaceId,
    root: Arc<PathBuf>,
    root_identity: FileIdentity,
}

/// Physical identity prevents a path replacement from silently changing the
/// workspace target while a bounded read is still using its canonical name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    volume: u64,
    file: u64,
}

/// A resolved path carries every checked component so callers can revalidate
/// the same physical chain immediately before and after an I/O operation.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedPath {
    pub(crate) path: PathBuf,
    components: Vec<(PathBuf, FileIdentity)>,
    final_identity: FileIdentity,
}

impl WorkspaceHandle {
    /// Returns the only identity safe to pass through a future IPC contract.
    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    /// Validates and resolves a regular existing path without following a
    /// symlink/reparse component, then rechecks canonical containment.
    pub fn resolve_file(&self, relative_path: &str) -> Result<PathBuf, WorkspaceError> {
        Ok(self.resolve_guard(relative_path, Some(false))?.path)
    }

    /// Validates and resolves an existing directory under the canonical root.
    pub fn resolve_directory(&self, relative_path: &str) -> Result<PathBuf, WorkspaceError> {
        Ok(self.resolve_guard(relative_path, Some(true))?.path)
    }

    /// Returns metadata for a root-contained path without exposing its native
    /// absolute spelling to callers.
    pub fn metadata(
        &self,
        relative_path: &str,
        hash_limit: u64,
    ) -> Result<FileMetadata, WorkspaceError> {
        let resolved = self.resolve_guard(relative_path, None)?;
        self.verify_resolved(&resolved, None)?;
        let path = &resolved.path;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| WorkspaceError::io("stat", error))?;
        let result = metadata_for_path(&path, &metadata, hash_limit)?;
        self.verify_resolved(&resolved, None)?;
        Ok(result)
    }

    /// Restricts Git pathspecs to simple root-relative names so no caller can
    /// smuggle pathspec magic, absolute paths or traversal into the adapter.
    pub(crate) fn validate_git_path(&self, relative_path: &str) -> Result<(), WorkspaceError> {
        if relative_path.is_empty() {
            return Err(WorkspaceError::InvalidRelativePath);
        }
        validate_relative_path(relative_path)?;
        if relative_path
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b':'))
        {
            return Err(WorkspaceError::InvalidRelativePath);
        }
        Ok(())
    }

    /// Keeps the root private to native adapters while allowing Git to use the
    /// exact canonical cwd selected by the registry.
    pub(crate) fn root_path(&self) -> &Path {
        self.root.as_path()
    }

    /// Resolves an internal path guard for readers that need pre/post checks.
    pub(crate) fn resolve_guard(
        &self,
        relative_path: &str,
        directory: Option<bool>,
    ) -> Result<ResolvedPath, WorkspaceError> {
        validate_relative_path(relative_path)?;
        self.verify_root_identity()?;
        let mut current = self.root.as_ref().clone();
        let mut components = Vec::new();
        for component in Path::new(relative_path).components() {
            let Component::Normal(name) = component else {
                continue;
            };
            current.push(name);
            let metadata =
                fs::symlink_metadata(&current).map_err(|error| map_path_error(error, "stat"))?;
            if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
                return Err(WorkspaceError::LinkNotAllowed);
            }
            components.push((current.clone(), physical_identity(&current)?));
        }
        let canonical =
            fs::canonicalize(&current).map_err(|error| map_path_error(error, "resolve"))?;
        if !path_is_within(self.root_path(), &canonical) {
            return Err(WorkspaceError::OutsideWorkspace);
        }
        let metadata =
            fs::symlink_metadata(&canonical).map_err(|error| map_path_error(error, "stat"))?;
        if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
            return Err(WorkspaceError::LinkNotAllowed);
        }
        if let Some(directory) = directory {
            if directory && !metadata.is_dir() {
                return Err(WorkspaceError::NotDirectory);
            }
            if !directory && !metadata.is_file() {
                return Err(WorkspaceError::NotFile);
            }
        }
        if metadata.is_file() && hard_link_count(&canonical, &metadata)? > 1 {
            return Err(WorkspaceError::LinkNotAllowed);
        }
        let final_identity = physical_identity(&canonical)?;
        let resolved = ResolvedPath {
            path: canonical,
            components,
            final_identity,
        };
        self.verify_resolved(&resolved, directory)?;
        Ok(resolved)
    }

    /// Revalidates the root and every path component before/after a read so a
    /// junction, symlink or rename race fails closed instead of escaping.
    pub(crate) fn verify_resolved(
        &self,
        resolved: &ResolvedPath,
        directory: Option<bool>,
    ) -> Result<(), WorkspaceError> {
        self.verify_root_identity()?;
        for (path, expected) in &resolved.components {
            let metadata =
                fs::symlink_metadata(path).map_err(|error| map_path_error(error, "recheck"))?;
            if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
                return Err(WorkspaceError::LinkNotAllowed);
            }
            if physical_identity(path)? != *expected {
                return Err(WorkspaceError::PathChanged);
            }
        }
        let canonical =
            fs::canonicalize(&resolved.path).map_err(|error| map_path_error(error, "recheck"))?;
        if canonical != resolved.path || !path_is_within(self.root_path(), &canonical) {
            return Err(WorkspaceError::PathChanged);
        }
        let metadata =
            fs::symlink_metadata(&canonical).map_err(|error| map_path_error(error, "recheck"))?;
        if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
            return Err(WorkspaceError::LinkNotAllowed);
        }
        if physical_identity(&canonical)? != resolved.final_identity {
            return Err(WorkspaceError::PathChanged);
        }
        if let Some(directory) = directory {
            if directory != metadata.is_dir() {
                return Err(if directory {
                    WorkspaceError::NotDirectory
                } else {
                    WorkspaceError::NotFile
                });
            }
        }
        if metadata.is_file() && hard_link_count(&canonical, &metadata)? > 1 {
            return Err(WorkspaceError::LinkNotAllowed);
        }
        self.verify_root_identity()
    }

    /// Checks that the canonical root still names the physical workspace that
    /// was admitted, including after a caller has finished reading children.
    fn verify_root_identity(&self) -> Result<(), WorkspaceError> {
        let metadata = fs::symlink_metadata(self.root_path())
            .map_err(|error| map_path_error(error, "root"))?;
        if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
            return Err(WorkspaceError::LinkNotAllowed);
        }
        if !metadata.is_dir() || physical_identity(self.root_path())? != self.root_identity {
            return Err(WorkspaceError::PathChanged);
        }
        Ok(())
    }
}

/// Owns the registry map so removing a workspace invalidates future lookups
/// without changing already-running read operations that hold a handle.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRegistry {
    workspaces: Arc<RwLock<HashMap<WorkspaceId, WorkspaceHandle>>>,
}

impl WorkspaceRegistry {
    /// Canonicalizes once at admission time so every later reader shares the
    /// same root and cannot accidentally use the caller's mutable spelling.
    pub fn register(&self, root: impl AsRef<Path>) -> Result<WorkspaceInfo, WorkspaceError> {
        let raw_root = absolute_path(root.as_ref())?;
        reject_link_components(&raw_root)?;
        let raw_metadata =
            fs::symlink_metadata(&raw_root).map_err(|error| map_path_error(error, "root"))?;
        if is_reparse_point(&raw_metadata) || raw_metadata.file_type().is_symlink() {
            return Err(WorkspaceError::InvalidRoot);
        }
        let path = fs::canonicalize(&raw_root).map_err(|error| map_path_error(error, "root"))?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| map_path_error(error, "root"))?;
        if !metadata.is_dir() || is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
            return Err(WorkspaceError::InvalidRoot);
        }
        let root_identity = physical_identity(&path)?;
        let id = WorkspaceId::new();
        let handle = WorkspaceHandle {
            id,
            root: Arc::new(path),
            root_identity,
        };
        self.workspaces
            .write()
            .map_err(|_| WorkspaceError::Io {
                operation: "registry",
                kind: std::io::ErrorKind::Other,
            })?
            .insert(id, handle);
        Ok(WorkspaceInfo { id })
    }

    /// Returns a handle whose canonical root is immutable for the operation's
    /// lifetime, avoiding a remove/re-add race with the same physical folder.
    pub fn get(&self, id: WorkspaceId) -> Result<WorkspaceHandle, WorkspaceError> {
        self.workspaces
            .read()
            .map_err(|_| WorkspaceError::Io {
                operation: "registry",
                kind: std::io::ErrorKind::Other,
            })?
            .get(&id)
            .cloned()
            .ok_or(WorkspaceError::WorkspaceNotFound)
    }

    /// Removes only the opaque mapping; callers holding an existing handle can
    /// finish a bounded read without mutating the registry's new state.
    pub fn remove(&self, id: WorkspaceId) -> Result<bool, WorkspaceError> {
        Ok(self
            .workspaces
            .write()
            .map_err(|_| WorkspaceError::Io {
                operation: "registry",
                kind: std::io::ErrorKind::Other,
            })?
            .remove(&id)
            .is_some())
    }

    /// Lists opaque ids for the project sidebar without disclosing root paths.
    pub fn list(&self) -> Result<Vec<WorkspaceInfo>, WorkspaceError> {
        let mut result = self
            .workspaces
            .read()
            .map_err(|_| WorkspaceError::Io {
                operation: "registry",
                kind: std::io::ErrorKind::Other,
            })?
            .keys()
            .copied()
            .map(|id| WorkspaceInfo { id })
            .collect::<Vec<_>>();
        result.sort_by_key(|info| info.id.0);
        Ok(result)
    }
}

/// Makes root admission inspect the exact native spelling before canonicalize
/// can erase a symlink or junction from the evidence.
fn absolute_path(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| map_path_error(error, "root"))?
            .join(path))
    }
}

/// Rejects every existing link/reparse component before canonicalization can
/// hide an alias that would escape the workspace trust boundary.
pub(crate) fn reject_link_components(path: &Path) -> Result<(), WorkspaceError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => {
                current.push(name);
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|error| map_path_error(error, "root"))?;
                if is_reparse_point(&metadata) || metadata.file_type().is_symlink() {
                    return Err(WorkspaceError::InvalidRoot);
                }
            }
        }
    }
    Ok(())
}

/// Returns a stable physical file key, failing closed on targets without a
/// strong identity primitive instead of treating size/time as an identity.
fn physical_identity(path: &Path) -> Result<FileIdentity, WorkspaceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).map_err(|error| WorkspaceError::io("identity", error))?;
        return Ok(FileIdentity {
            volume: metadata.dev(),
            file: metadata.ino(),
        });
    }
    #[cfg(windows)]
    {
        return query_windows_file_information(path)
            .map(|(identity, _)| identity)
            .map_err(|error| WorkspaceError::io("identity", error));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        return Err(WorkspaceError::Io {
            operation: "identity",
            kind: std::io::ErrorKind::Unsupported,
        });
    }
}

/// Hard links are rejected because a path inside the workspace could alias a
/// physical file outside it even though no symlink/reparse point is present.
fn hard_link_count(path: &Path, metadata: &Metadata) -> Result<u64, WorkspaceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        return Ok(metadata.nlink());
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        return query_windows_file_information(path)
            .map(|(_, links)| u64::from(links))
            .map_err(|error| WorkspaceError::io("link_count", error));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, metadata);
        Err(WorkspaceError::Io {
            operation: "link_count",
            kind: std::io::ErrorKind::Unsupported,
        })
    }
}

/// Encodes digest bytes explicitly instead of relying on `LowerHex`, which
/// `sha2` 0.11's hybrid-array output does not implement on Rust 1.88.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        result.push(HEX[usize::from(byte >> 4)] as char);
        result.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    result
}

#[cfg(windows)]
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut c_void = -1_isize as *mut c_void;

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time_low: u32,
    creation_time_high: u32,
    last_access_time_low: u32,
    last_access_time_high: u32,
    last_write_time_low: u32,
    last_write_time_high: u32,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn CreateFileW(
        name: *const u16,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *const c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: *mut c_void,
    ) -> *mut c_void;
    fn GetFileInformationByHandle(
        handle: *mut c_void,
        information: *mut ByHandleFileInformation,
    ) -> i32;
}

#[cfg(windows)]
/// Reads the Windows volume/file index and link count while opening with
/// reparse-point semantics so a replacement cannot be silently followed.
fn query_windows_file_information(path: &Path) -> std::io::Result<(FileIdentity, u32)> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
    let ok = unsafe { GetFileInformationByHandle(handle, information.as_mut_ptr()) } != 0;
    let error = if ok {
        None
    } else {
        Some(std::io::Error::last_os_error())
    };
    let information = if ok {
        Some(unsafe { information.assume_init() })
    } else {
        None
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if let Some(error) = error {
        return Err(error);
    }
    let information = information.expect("Windows file information initialized on success");
    Ok((
        FileIdentity {
            volume: u64::from(information.volume_serial_number),
            file: (u64::from(information.file_index_high) << 32)
                | u64::from(information.file_index_low),
        },
        information.number_of_links,
    ))
}

/// Rejects absolute, drive-prefixed and ambiguous platform path input before
/// any I/O; the empty string alone denotes the workspace root.
pub(crate) fn validate_relative_path(relative_path: &str) -> Result<(), WorkspaceError> {
    if relative_path.contains('\0')
        || relative_path.as_bytes().len() > 4 * 1024
        || relative_path.contains(':')
        || relative_path.contains('\\')
    {
        return Err(WorkspaceError::InvalidRelativePath);
    }
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(WorkspaceError::InvalidRelativePath);
    }
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(WorkspaceError::InvalidRelativePath);
            }
            Component::CurDir if relative_path.is_empty() => {}
            Component::CurDir => {
                return Err(WorkspaceError::InvalidRelativePath);
            }
            Component::Normal(name) =>
            {
                #[cfg(windows)]
                if is_windows_ambiguous_component(name) {
                    return Err(WorkspaceError::InvalidRelativePath);
                }
            }
        }
    }
    Ok(())
}

/// Rejects Windows device and trailing-dot/space aliases before the OS can
/// normalize them to a different native path than the UI requested.
#[cfg(windows)]
fn is_windows_ambiguous_component(name: &std::ffi::OsStr) -> bool {
    let value = name.to_string_lossy();
    if value.ends_with(['.', ' ']) {
        return true;
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ' '])
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Converts metadata into a transport-safe revision and hashes only bounded
/// regular files, keeping a large binary out of both memory and IPC.
pub(crate) fn metadata_for_path(
    path: &Path,
    metadata: &Metadata,
    hash_limit: u64,
) -> Result<FileMetadata, WorkspaceError> {
    metadata_for_path_with_deadline(path, metadata, hash_limit, None)
}

/// Converts metadata with an optional absolute deadline for polling scans.
pub(crate) fn metadata_for_path_with_deadline(
    path: &Path,
    metadata: &Metadata,
    hash_limit: u64,
    deadline: Option<Instant>,
) -> Result<FileMetadata, WorkspaceError> {
    let kind = entry_kind(metadata);
    if kind == EntryKind::File && hard_link_count(path, metadata)? > 1 {
        return Err(WorkspaceError::LinkNotAllowed);
    }
    let identity_before = if matches!(kind, EntryKind::Symlink | EntryKind::ReparsePoint) {
        None
    } else {
        Some(physical_identity(path)?)
    };
    let sha256 = if kind == EntryKind::File && metadata.len() <= hash_limit {
        Some(hash_file_until(path, hash_limit, deadline)?)
    } else {
        None
    };
    let modified_unix_millis = metadata.modified().ok().and_then(|modified| {
        modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis())
    });
    let after = fs::symlink_metadata(path).map_err(|error| WorkspaceError::io("recheck", error))?;
    let identity_changed = identity_before.is_some_and(|identity| {
        physical_identity(path)
            .map(|current| current != identity)
            .unwrap_or(true)
    });
    let modified_after_unix_millis = after.modified().ok().and_then(|modified| {
        modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis())
    });
    if entry_kind(&after) != kind
        || after.len() != metadata.len()
        || modified_after_unix_millis != modified_unix_millis
        || identity_changed
    {
        return Err(WorkspaceError::PathChanged);
    }
    Ok(FileMetadata {
        kind,
        size: metadata.len(),
        modified_unix_millis,
        revision: FileRevision {
            kind,
            size: metadata.len(),
            modified_unix_millis,
            sha256,
        },
    })
}

/// Hashes at most `hash_limit + 1` bytes so a file that grows after the initial
/// stat cannot turn a metadata request into an unbounded read, even when no
/// deadline was supplied by the caller.
fn hash_file_until(
    path: &Path,
    hash_limit: u64,
    deadline: Option<Instant>,
) -> Result<String, WorkspaceError> {
    // The extra byte is the bounded proof that the file exceeded the caller's
    // limit; checked arithmetic keeps a maximum u64 limit from wrapping.
    let mut remaining = hash_limit
        .checked_add(1)
        .ok_or(WorkspaceError::FileTooLarge)?;
    let mut file = fs::File::open(path).map_err(|error| WorkspaceError::io("hash", error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(WorkspaceError::ScanDeadlineExceeded);
        }
        let read_len = usize::try_from(remaining)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        if read_len == 0 {
            return Err(WorkspaceError::FileTooLarge);
        }
        let count = std::io::Read::read(&mut file, &mut buffer[..read_len])
            .map_err(|error| WorkspaceError::io("hash", error))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        remaining = remaining
            .checked_sub(u64::try_from(count).map_err(|_| WorkspaceError::FileTooLarge)?)
            .ok_or(WorkspaceError::FileTooLarge)?;
        if remaining == 0 {
            return Err(WorkspaceError::FileTooLarge);
        }
    }
    Ok(hex_lower(&digest.finalize()))
}

#[cfg(test)]
/// Exposes the bounded hashing primitive to regression tests without adding
/// it to the production workspace IPC surface.
pub(crate) fn hash_file_for_test(
    path: &Path,
    hash_limit: u64,
    deadline: Option<Instant>,
) -> Result<String, WorkspaceError> {
    hash_file_until(path, hash_limit, deadline)
}

/// Classifies reparse points before symlink checks so Windows junctions cannot
/// be mistaken for ordinary directories.
pub(crate) fn entry_kind(metadata: &Metadata) -> EntryKind {
    if is_reparse_point(metadata) {
        return EntryKind::ReparsePoint;
    }
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if metadata.is_file() {
        EntryKind::File
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::Other
    }
}

/// Reparse metadata is only available through the Windows standard extension;
/// other platforms use the portable symlink bit as the equivalent boundary.
pub(crate) fn is_reparse_point(_metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return _metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    {
        let _ = _metadata;
        false
    }
}

/// Canonical containment compares path components instead of string prefixes,
/// with Windows case folding limited to the platform where it is required.
pub(crate) fn path_is_within(root: &Path, candidate: &Path) -> bool {
    let root_components = root.components().collect::<Vec<_>>();
    let candidate_components = candidate.components().collect::<Vec<_>>();
    if candidate_components.len() < root_components.len() {
        return false;
    }
    root_components
        .iter()
        .zip(candidate_components.iter())
        .all(|(root, candidate)| {
            #[cfg(windows)]
            {
                root.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&candidate.as_os_str().to_string_lossy())
            }
            #[cfg(not(windows))]
            {
                root == candidate
            }
        })
}

/// Maps OS path failures to stable categories without embedding an absolute
/// path or a user-controlled filename in a future error payload.
pub(crate) fn map_path_error(error: std::io::Error, operation: &'static str) -> WorkspaceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => WorkspaceError::PathNotFound,
        std::io::ErrorKind::PermissionDenied => WorkspaceError::Io {
            operation,
            kind: std::io::ErrorKind::PermissionDenied,
        },
        _ => WorkspaceError::io(operation, error),
    }
}
