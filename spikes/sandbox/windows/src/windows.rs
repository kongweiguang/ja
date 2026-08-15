// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Narrow Win32 adapter for the sandbox probe.  All unsafe code is kept here
//! so callers cannot accidentally replace an AppContainer with string checks.

use crate::{
    ChildOutcome, NetworkCapability, ResourceBudget, SandboxError, SandboxSpec, WorkspaceAccess,
};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::hash::{Hash, Hasher};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    LocalFree, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    BuildTrusteeWithSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, FreeSid, GetSecurityDescriptorLength,
    INHERIT_ONLY, NO_INHERITANCE, OBJECT_INHERIT_ACE, PSID, SECURITY_ATTRIBUTES,
    SECURITY_CAPABILITIES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, GetFileInformationByHandle, OPEN_EXISTING,
    ReadFile,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

const ERROR_SUCCESS: u32 = 0;
const ERROR_BROKEN_PIPE: u32 = 109;
const MAX_STDOUT_BYTES: usize = 1024 * 1024;
static NEXT_PROFILE_ID: AtomicU64 = AtomicU64::new(1);

/// Spawn a worker only after the profile, ACL and Job Object are ready.  The
/// suspended-create barrier makes it impossible for a child to execute before
/// it has been assigned to the kill-on-close job.
pub fn spawn(spec: SandboxSpec) -> Result<SandboxChild, SandboxError> {
    let workspace = validate_spec(&spec)?;
    let worker = std::fs::canonicalize(&spec.worker)?;
    if spec.network == NetworkCapability::InternetClient {
        return Err(SandboxError::InvalidConfig(
            "InternetClient capability is intentionally not enabled by this probe".into(),
        ));
    }

    let profile = AppContainerProfile::create()?;
    let workspace_acl = WorkspaceAcl::grant(&workspace, profile.sid, spec.workspace_access)?;
    let resource_acl = ResourceAcl::grant(&worker, profile.sid)?;
    let job = Job::create(spec.budget)?;
    let (process, thread, stdout_read) = create_suspended_process(&spec, &workspace, profile.sid)?;
    if unsafe { AssignProcessToJobObject(job.handle, process.hProcess) } == 0 {
        let error = last_error();
        unsafe {
            terminate_process_for_startup(process.hProcess);
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
            CloseHandle(stdout_read);
        }
        drop(resource_acl);
        drop(workspace_acl);
        drop(profile);
        return Err(os_error("AssignProcessToJobObject", error));
    }
    let resume_result = unsafe { ResumeThread(thread) };
    if resume_result == u32::MAX {
        let error = last_error();
        unsafe {
            TerminateJobObject(job.handle, 1);
            CloseHandle(thread);
            CloseHandle(process.hProcess);
            CloseHandle(stdout_read);
        }
        drop(resource_acl);
        drop(workspace_acl);
        drop(profile);
        return Err(os_error("ResumeThread", error));
    }
    unsafe { CloseHandle(thread) };

    Ok(SandboxChild {
        process: process.hProcess,
        job,
        stdout_read,
        job_terminated: false,
        _profile: profile,
        _workspace_acl: workspace_acl,
        _resource_acl: resource_acl,
    })
}

/// The child owns the native handles and cleanup guards, making every exit
/// path restore the fixture ACL and delete the temporary profile.
pub struct SandboxChild {
    process: HANDLE,
    job: Job,
    stdout_read: HANDLE,
    job_terminated: bool,
    _profile: AppContainerProfile,
    _workspace_acl: WorkspaceAcl,
    _resource_acl: ResourceAcl,
}

impl SandboxChild {
    /// Wait for normal completion until a monotonic deadline, then terminate
    /// the whole Job Object instead of leaving grandchildren behind.
    fn wait(&mut self, timeout: Duration) -> Result<ChildOutcome, SandboxError> {
        let milliseconds = timeout.as_millis().min(u32::MAX as u128) as u32;
        let wait = unsafe { WaitForSingleObject(self.process, milliseconds) };
        if wait == WAIT_TIMEOUT {
            self.terminate_tree()?;
            return Ok(ChildOutcome {
                exit_code: read_exit_code(self.process),
                timed_out: true,
            });
        }
        if wait != WAIT_OBJECT_0 {
            let error = last_error();
            self.terminate_tree()?;
            return Err(os_error("WaitForSingleObject", error));
        }
        let outcome = ChildOutcome {
            exit_code: read_exit_code(self.process),
            timed_out: false,
        };
        // Preserve the direct process outcome before closing the Job, because
        // the parent may already be gone while a grandchild still holds a
        // stdio handle that would otherwise outlive this wait operation.
        self.terminate_tree()?;
        Ok(outcome)
    }

    /// Terminate and wait for the complete process tree.  Keeping this method
    /// explicit lets tests prove timeout/cancel semantics without arbitrary
    /// sleeps or reliance on process-name polling.
    pub fn terminate_tree(&mut self) -> Result<(), SandboxError> {
        if !self.job_terminated {
            if unsafe { TerminateJobObject(self.job.handle, 1) } == 0 {
                return Err(os_error("TerminateJobObject", last_error()));
            }
            self.job_terminated = true;
        }
        let wait = unsafe { WaitForSingleObject(self.process, 5_000) };
        if wait != WAIT_OBJECT_0 {
            return Err(os_error("WaitForSingleObject", last_error()));
        }
        Ok(())
    }

    /// Wait while a dedicated reader drains the bounded stdout/stderr pipe;
    /// always close the Job before joining it so a normally exited parent
    /// cannot leave a grandchild-held writer blocking lifecycle cleanup.
    pub fn wait_with_stdout(
        &mut self,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<(ChildOutcome, Vec<u8>), SandboxError> {
        if max_bytes > MAX_STDOUT_BYTES {
            self.terminate_tree()?;
            return Err(SandboxError::InvalidConfig(
                "worker stdout bound exceeds 1 MiB hard limit".into(),
            ));
        }
        if self.stdout_read.is_null() || self.stdout_read == INVALID_HANDLE_VALUE {
            return Err(SandboxError::InvalidConfig(
                "worker stdout pipe was already consumed".into(),
            ));
        }
        let stdout_read = std::mem::replace(&mut self.stdout_read, null_mut()) as usize;
        let reader = std::thread::spawn(move || read_stdout_pipe(stdout_read as HANDLE, max_bytes));
        let outcome = self.wait(timeout);
        let cleanup = self.terminate_tree();
        let output = reader
            .join()
            .map_err(|_| SandboxError::InvalidConfig("stdout reader panicked".into()))??;
        let outcome = outcome?;
        cleanup?;
        Ok((outcome, output))
    }

    /// Return the OS process identifier for barrier-driven fixture assertions.
    pub fn process_id(&self) -> u32 {
        unsafe { windows_sys::Win32::System::Threading::GetProcessId(self.process) }
    }
}

impl Drop for SandboxChild {
    /// Drop is a last-resort fail-closed path because UI cancellation can occur
    /// while no caller is waiting for a normal worker result.
    fn drop(&mut self) {
        if !self.job_terminated {
            let _ = unsafe { TerminateJobObject(self.job.handle, 1) };
            self.job_terminated = true;
            let _ = unsafe { WaitForSingleObject(self.process, 5_000) };
        }
        unsafe {
            CloseHandle(self.process);
            if !self.stdout_read.is_null() && self.stdout_read != INVALID_HANDLE_VALUE {
                CloseHandle(self.stdout_read);
            }
        }
    }
}

/// Determine whether a reported grandchild remains live using only a limited
/// query/synchronize handle; this is the process-tree acceptance observation.
pub fn process_is_alive(pid: u32) -> bool {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    alive
}

/// Reject a symlink/junction/reparse node during product preflight before any
/// profile or ACL mutation can expose an aliased target.
pub fn reject_reparse_path(path: &Path) -> Result<(), SandboxError> {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
                {
                    return Err(SandboxError::InvalidConfig(
                        "workspace path contains a reparse point".into(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if !current.pop() {
            break;
        }
    }
    Ok(())
}

/// Return a stable fixture-only security-descriptor fingerprint so tests can
/// prove that a reparse target never received an AppContainer ACE.
pub fn acl_fingerprint(path: &Path) -> Result<u64, SandboxError> {
    let path_w = wide(path.as_os_str());
    let mut original_dacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut original_dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(os_error("GetNamedSecurityInfoW", status));
    }
    let length = unsafe { GetSecurityDescriptorLength(descriptor.cast()) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(descriptor.cast::<u8>(), length) };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    unsafe { LocalFree(descriptor) };
    Ok(hasher.finish())
}

/// Validate canonical roots before any profile or ACL mutation, because a
/// reparse point could otherwise make a temporary fixture escape its owner.
fn validate_spec(spec: &SandboxSpec) -> Result<PathBuf, SandboxError> {
    if !spec.worker.is_absolute() || !spec.workspace.is_absolute() {
        return Err(SandboxError::InvalidConfig(
            "worker and workspace must be absolute".into(),
        ));
    }
    reject_reparse_path(&spec.workspace)?;
    reject_reparse_path(&spec.worker)?;
    let workspace = std::fs::canonicalize(&spec.workspace)?;
    if !workspace.is_dir() {
        return Err(SandboxError::InvalidConfig(
            "workspace must be a directory".into(),
        ));
    }
    validate_real_descendants(&workspace)?;
    let worker = std::fs::canonicalize(&spec.worker)?;
    if !worker.is_file() || worker.starts_with(&workspace) {
        return Err(SandboxError::InvalidConfig(
            "worker must be an existing resource file outside workspace".into(),
        ));
    }
    if has_multiple_hardlinks(&worker)? {
        return Err(SandboxError::InvalidConfig(
            "worker resource must not have hardlink aliases".into(),
        ));
    }
    if spec.budget.max_processes == 0 || spec.budget.max_processes > 64 {
        return Err(SandboxError::InvalidConfig(
            "max_processes must be between 1 and 64".into(),
        ));
    }
    if spec.budget.process_memory_bytes < 16 * 1024 * 1024 {
        return Err(SandboxError::InvalidConfig(
            "process memory budget is below the minimum probe size".into(),
        ));
    }
    for (key, value) in &spec.env {
        if key.is_empty()
            || key.to_string_lossy().contains('=')
            || key.to_string_lossy().contains('\0')
            || value.to_string_lossy().contains('\0')
        {
            return Err(SandboxError::InvalidConfig(
                "environment keys/values contain an invalid character".into(),
            ));
        }
    }
    Ok(workspace)
}

/// Create the AppContainer profile with no capabilities.  An empty capability
/// list is the important default: it denies internet and loopback access.
struct AppContainerProfile {
    name: Vec<u16>,
    sid: PSID,
}

impl AppContainerProfile {
    /// A per-process profile avoids cross-test capability/ACL state while
    /// remaining usable by an ordinary non-administrator desktop account.
    fn create() -> Result<Self, SandboxError> {
        let name = wide(format!(
            "JA.Sandbox.{}.{}",
            std::process::id(),
            unique_nonce()
        ));
        let display = wide("JA Windows sandbox probe");
        let description = wide("Temporary AppContainer for a JA tool worker");
        let mut sid: PSID = null_mut();
        let result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        if result < 0 || sid.is_null() {
            return Err(os_error("CreateAppContainerProfile", result as u32));
        }
        Ok(Self { name, sid })
    }
}

impl Drop for AppContainerProfile {
    /// Delete both the profile record and the SID allocation even when process
    /// creation fails halfway through setup.
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteAppContainerProfile(self.name.as_ptr());
            let _ = FreeSid(self.sid);
        }
    }
}

/// This guard stores every original DACL and restores it byte-for-byte after
/// the child exits, including files that existed before the root ACE grant.
struct WorkspaceAcl {
    entries: Vec<AclEntry>,
}

/// The worker resource is read/execute-only, keeping self-modification outside
/// the workspace policy and making the executable boundary explicit.
struct ResourceAcl {
    entries: Vec<AclEntry>,
}

struct AclEntry {
    path: Vec<u16>,
    original_dacl: *mut ACL,
    original_descriptor: *mut c_void,
}

impl WorkspaceAcl {
    /// Add only the AppContainer SID to the workspace, with no execute access
    /// for files and a caller-selected read-only/read-write mode.
    fn grant(path: &Path, sid: PSID, access: WorkspaceAccess) -> Result<Self, SandboxError> {
        let mut paths = vec![path.to_path_buf()];
        collect_real_descendants(path, &mut paths)?;
        let mut entries = Vec::with_capacity(paths.len());
        for candidate in paths {
            let grants = if candidate.is_dir() {
                let mut directory_mask = FILE_GENERIC_READ | FILE_TRAVERSE;
                let mut inherited_mask = FILE_GENERIC_READ;
                if access == WorkspaceAccess::ReadWrite {
                    directory_mask |= FILE_GENERIC_WRITE
                        | FILE_ADD_FILE
                        | FILE_ADD_SUBDIRECTORY
                        | FILE_DELETE_CHILD;
                    inherited_mask |= FILE_GENERIC_WRITE;
                }
                vec![
                    (directory_mask, NO_INHERITANCE),
                    (
                        inherited_mask | FILE_TRAVERSE,
                        CONTAINER_INHERIT_ACE | INHERIT_ONLY,
                    ),
                    (inherited_mask, OBJECT_INHERIT_ACE | INHERIT_ONLY),
                ]
            } else {
                let mask = match access {
                    WorkspaceAccess::ReadOnly => FILE_GENERIC_READ,
                    WorkspaceAccess::ReadWrite => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                };
                vec![(mask, NO_INHERITANCE)]
            };
            match grant_acl(&candidate, sid, &grants) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    drop(Self { entries });
                    return Err(error);
                }
            }
        }
        Ok(Self { entries })
    }
}

impl ResourceAcl {
    /// Grant execute/read on the resource directory and the worker itself;
    /// resource files cannot be changed by the AppContainer.
    fn grant(worker: &Path, sid: PSID) -> Result<Self, SandboxError> {
        let resource_dir = worker
            .parent()
            .ok_or_else(|| SandboxError::InvalidConfig("worker has no parent".into()))?;
        let grants = [
            (
                resource_dir.to_path_buf(),
                vec![(FILE_TRAVERSE, NO_INHERITANCE)],
            ),
            (
                worker.to_path_buf(),
                vec![(FILE_GENERIC_READ | FILE_GENERIC_EXECUTE, NO_INHERITANCE)],
            ),
        ];
        let mut entries = Vec::with_capacity(grants.len());
        for (candidate, candidate_grants) in grants {
            match grant_acl(&candidate, sid, &candidate_grants) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    drop(Self { entries });
                    return Err(error);
                }
            }
        }
        Ok(Self { entries })
    }
}

/// Apply exact self/inheritance ACEs and retain the original descriptor for
/// deterministic rollback when any later path fails.  Separate container and
/// object inheritance keeps directory traversal while excluding file execute.
fn grant_acl(path: &Path, sid: PSID, grants: &[(u32, u32)]) -> Result<AclEntry, SandboxError> {
    let path_w = wide(path.as_os_str());
    let mut original_dacl: *mut ACL = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut original_dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(os_error("GetNamedSecurityInfoW", status));
    }
    let mut trustee = TRUSTEE_W {
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_USER,
        ..Default::default()
    };
    unsafe { BuildTrusteeWithSidW(&mut trustee, sid) };
    let accesses: Vec<EXPLICIT_ACCESS_W> = grants
        .iter()
        .map(|(mask, inheritance)| EXPLICIT_ACCESS_W {
            grfAccessPermissions: *mask,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: *inheritance,
            Trustee: trustee,
        })
        .collect();
    let mut updated_dacl: *mut ACL = null_mut();
    let status = unsafe {
        SetEntriesInAclW(
            accesses.len() as u32,
            accesses.as_ptr(),
            original_dacl,
            &mut updated_dacl,
        )
    };
    if status != ERROR_SUCCESS {
        unsafe { LocalFree(descriptor) };
        return Err(os_error("SetEntriesInAclW", status));
    }
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            updated_dacl,
            null_mut(),
        )
    };
    unsafe { LocalFree(updated_dacl.cast()) };
    if status != ERROR_SUCCESS {
        unsafe { LocalFree(descriptor) };
        return Err(os_error("SetNamedSecurityInfoW", status));
    }
    Ok(AclEntry {
        path: path_w,
        original_dacl,
        original_descriptor: descriptor,
    })
}

/// Enumerate only real descendants.  Encountering a reparse or hardlink is a
/// hard error rather than a skipped entry, so ACL inheritance cannot alias an
/// outside inode after a partial walk.
fn collect_real_descendants(path: &Path, paths: &mut Vec<PathBuf>) -> Result<(), SandboxError> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let candidate = entry.path();
        let metadata = std::fs::symlink_metadata(&candidate)?;
        if metadata.file_type().is_symlink()
            || (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(SandboxError::InvalidConfig(
                "workspace contains a reparse descendant".into(),
            ));
        }
        if metadata.is_file() && has_multiple_hardlinks(&candidate)? {
            return Err(SandboxError::InvalidConfig(
                "workspace contains a hardlink descendant".into(),
            ));
        }
        paths.push(candidate.clone());
        if metadata.is_dir() {
            collect_real_descendants(&candidate, paths)?;
        }
    }
    Ok(())
}

/// Reject every unsafe descendant before any profile or ACL mutation, because
/// an inherited ACE on a hardlink aliases the outside inode itself and cannot
/// be repaired by skipping that directory entry during authorization.
fn validate_real_descendants(path: &Path) -> Result<(), SandboxError> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let candidate = entry.path();
        let metadata = std::fs::symlink_metadata(&candidate)?;
        if metadata.file_type().is_symlink()
            || (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return Err(SandboxError::InvalidConfig(
                "workspace contains a reparse descendant".into(),
            ));
        }
        if metadata.is_file() && has_multiple_hardlinks(&candidate)? {
            return Err(SandboxError::InvalidConfig(
                "workspace contains a hardlink descendant".into(),
            ));
        }
        if metadata.is_dir() {
            validate_real_descendants(&candidate)?;
        }
    }
    Ok(())
}

/// Query the native file index because Rust's Windows link-count accessor is
/// still unstable; granting an ACL to any alias would grant the shared inode.
fn has_multiple_hardlinks(path: &Path) -> Result<bool, SandboxError> {
    let path_w = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(os_error("CreateFileW(hardlink preflight)", last_error()));
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let error = if result == 0 {
        Some(last_error())
    } else {
        None
    };
    unsafe { CloseHandle(handle) };
    match error {
        Some(code) => Err(os_error("GetFileInformationByHandle", code)),
        None => Ok(information.nNumberOfLinks > 1),
    }
}

impl Drop for WorkspaceAcl {
    /// Restore every path in reverse order before freeing native descriptors.
    fn drop(&mut self) {
        restore_acl_entries(&mut self.entries);
    }
}

impl Drop for ResourceAcl {
    /// Restore the resource directory and executable even when startup fails.
    fn drop(&mut self) {
        restore_acl_entries(&mut self.entries);
    }
}

/// Restore a set of DACLs; best-effort native cleanup is still attempted for
/// every path so one transient failure cannot skip later entries.
fn restore_acl_entries(entries: &mut Vec<AclEntry>) {
    for entry in entries.iter().rev() {
        let status = unsafe {
            SetNamedSecurityInfoW(
                entry.path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                entry.original_dacl,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            // Drop cannot return an error; emit a non-secret operation marker
            // while still attempting every remaining entry in reverse order.
            eprintln!("sandbox ACL restore failed; Win32 status={status}");
        }
        unsafe {
            let _ = LocalFree(entry.original_descriptor);
        }
    }
    entries.clear();
}

/// Job limits are configured before process creation so the same handle can be
/// assigned in the suspended window and kill descendants on close.
struct Job {
    handle: HANDLE,
}

impl Job {
    /// Create a private unnamed Job Object with process and memory ceilings.
    fn create(budget: ResourceBudget) -> Result<Self, SandboxError> {
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(os_error("CreateJobObjectW", last_error()));
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            ActiveProcessLimit: budget.max_processes,
            ..unsafe { zeroed() }
        };
        limits.ProcessMemoryLimit = budget.process_memory_bytes;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = last_error();
            unsafe { CloseHandle(handle) };
            return Err(os_error("SetInformationJobObject", error));
        }
        Ok(Self { handle })
    }
}

impl Drop for Job {
    /// Closing a kill-on-close job is the final protection against orphaned
    /// grandchildren if the host is cancelled while cleanup is in progress.
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// Build and launch an AppContainer process suspended, carrying only the
/// explicitly supplied environment block and command line.
fn create_suspended_process(
    spec: &SandboxSpec,
    workspace: &Path,
    sid: PSID,
) -> Result<(PROCESS_INFORMATION, HANDLE, HANDLE), SandboxError> {
    let worker = std::fs::canonicalize(&spec.worker)?;
    let worker_w = wide(worker.as_os_str());
    let mut command_line = quote_windows_arg(&worker);
    for arg in &spec.args {
        command_line.push(' ');
        command_line.push_str(&quote_windows_arg(arg));
    }
    let mut command_line_w = wide(&command_line);
    let current_directory = wide(workspace.as_os_str());
    let environment = environment_block(&spec.env, workspace)?;
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let io = ChildIo::create()?;
    let attributes = match AttributeList::new(&capabilities, &[io.stdin, io.stdout_write]) {
        Ok(attributes) => attributes,
        Err(error) => {
            io.close_all();
            return Err(error);
        }
    };

    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = io.stdin;
    startup.StartupInfo.hStdOutput = io.stdout_write;
    startup.StartupInfo.hStdError = io.stdout_write;
    startup.lpAttributeList = attributes.list;
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            worker_w.as_ptr(),
            command_line_w.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            current_directory.as_ptr(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut process,
        )
    };
    if created == 0 {
        io.close_all();
        return Err(os_error("CreateProcessW", last_error()));
    }
    io.close_parent_handles();
    Ok((process, process.hThread, io.stdout_read))
}

/// Own the explicit stdio handles so only the intended stdout/stderr writer is
/// inherited by the AppContainer; the parent keeps the read side private.
struct ChildIo {
    stdin: HANDLE,
    stdout_read: HANDLE,
    stdout_write: HANDLE,
}

impl ChildIo {
    /// Build inheritable NUL/stdout handles and then clear inheritance on the
    /// parent read side, preventing unrelated parent handles from leaking.
    fn create() -> Result<Self, SandboxError> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut stdout_read = null_mut();
        let mut stdout_write = null_mut();
        if unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, &attributes, 0) } == 0 {
            return Err(os_error("CreatePipe(stdout)", last_error()));
        }
        if unsafe { SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT, 0) } == 0 {
            let error = last_error();
            unsafe {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
            }
            return Err(os_error("SetHandleInformation(stdout)", error));
        }
        let nul = wide("NUL");
        let stdin = unsafe {
            CreateFileW(
                nul.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if stdin.is_null() || stdin == INVALID_HANDLE_VALUE {
            let error = last_error();
            unsafe {
                CloseHandle(stdout_read);
                CloseHandle(stdout_write);
            }
            return Err(os_error("CreateFileW(NUL)", error));
        }
        Ok(Self {
            stdin,
            stdout_read,
            stdout_write,
        })
    }

    /// Close handles retained by the parent after CreateProcess duplicated
    /// the inheritable writer and standard input into the child.
    fn close_parent_handles(&self) {
        unsafe {
            CloseHandle(self.stdin);
            CloseHandle(self.stdout_write);
        }
    }

    /// Close all handles when process creation fails before child ownership.
    fn close_all(&self) {
        unsafe {
            CloseHandle(self.stdin);
            CloseHandle(self.stdout_read);
            CloseHandle(self.stdout_write);
        }
    }
}

/// Drain a worker's combined stdout/stderr with a hard byte limit, closing the
/// native handle on every return so a cancelled child cannot keep the reader
/// thread or profile cleanup alive indefinitely.
fn read_stdout_pipe(handle: HANDLE, max_bytes: usize) -> Result<Vec<u8>, SandboxError> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let remaining = max_bytes.saturating_sub(output.len());
        let request = remaining.saturating_add(1).min(buffer.len()) as u32;
        let mut bytes_read = 0u32;
        let read = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                request,
                &mut bytes_read,
                null_mut(),
            )
        };
        if read == 0 {
            let error = last_error();
            unsafe { CloseHandle(handle) };
            if error == ERROR_BROKEN_PIPE {
                return Ok(output);
            }
            return Err(os_error("ReadFile(stdout)", error));
        }
        if bytes_read == 0 {
            unsafe { CloseHandle(handle) };
            return Ok(output);
        }
        if bytes_read as usize > remaining {
            unsafe { CloseHandle(handle) };
            return Err(SandboxError::InvalidConfig(
                "worker stdout exceeded configured bound".into(),
            ));
        }
        output.extend_from_slice(&buffer[..bytes_read as usize]);
    }
}

/// Own the native attribute-list allocation so every process-create error path
/// invokes DeleteProcThreadAttributeList rather than leaking heap state.
struct AttributeList {
    buffer: Vec<u8>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    /// Build the security-capabilities attribute required for AppContainer
    /// creation and an explicit stdio handle allowlist, retaining the buffer
    /// until CreateProcessW has copied both attributes.
    fn new(capabilities: &SECURITY_CAPABILITIES, handles: &[HANDLE]) -> Result<Self, SandboxError> {
        let mut size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size);
        }
        if size == 0 {
            return Err(os_error("InitializeProcThreadAttributeList", last_error()));
        }
        let mut buffer = vec![0u8; size];
        let list = buffer.as_mut_ptr().cast::<std::ffi::c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
            return Err(os_error("InitializeProcThreadAttributeList", last_error()));
        }
        let attribute_list = Self { buffer, list };
        if unsafe {
            UpdateProcThreadAttribute(
                attribute_list.list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (capabilities as *const SECURITY_CAPABILITIES).cast(),
                size_of::<SECURITY_CAPABILITIES>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(os_error("UpdateProcThreadAttribute", last_error()));
        }
        if unsafe {
            UpdateProcThreadAttribute(
                attribute_list.list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(os_error(
                "UpdateProcThreadAttribute(handle list)",
                last_error(),
            ));
        }
        Ok(attribute_list)
    }
}

impl Drop for AttributeList {
    /// Delete the list before its backing buffer is freed, satisfying the Win32
    /// lifetime contract on success and all failure branches.
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
        self.buffer.clear();
    }
}

/// Encode a sorted, double-NUL-terminated environment block without inheriting
/// PATH, model tokens or any other parent secret by accident.
fn environment_block(
    env: &BTreeMap<OsString, OsString>,
    current_directory: &Path,
) -> Result<Vec<u16>, SandboxError> {
    let mut normalized = BTreeMap::<String, (OsString, OsString)>::new();
    for (key, value) in env {
        let key_text = key.to_string_lossy().to_ascii_uppercase();
        let is_drive_entry = key_text.len() == 3
            && key_text.starts_with('=')
            && key_text.as_bytes()[2] == b':'
            && key_text.as_bytes()[1].is_ascii_alphabetic();
        if (!is_drive_entry && key_text.contains('='))
            || key_text.contains('\0')
            || value.to_string_lossy().contains('\0')
        {
            return Err(SandboxError::InvalidConfig(
                "environment keys/values contain an invalid character".into(),
            ));
        }
        if normalized
            .insert(key_text.clone(), (key.clone(), value.clone()))
            .is_some()
        {
            return Err(SandboxError::InvalidConfig(format!(
                "duplicate environment key: {key_text}"
            )));
        }
    }
    let drive = current_directory
        .to_string_lossy()
        .chars()
        .next()
        .filter(|character| character.is_ascii_alphabetic())
        .map(|character| character.to_ascii_uppercase());
    if let Some(drive) = drive {
        let drive_key = format!("={drive}:");
        let drive_key_os = OsString::from(&drive_key);
        let drive_value = OsString::from(current_directory.as_os_str());
        if normalized
            .insert(drive_key, (drive_key_os, drive_value))
            .is_some()
        {
            return Err(SandboxError::InvalidConfig(
                "environment already contains a current-drive entry".into(),
            ));
        }
    }
    let mut block = Vec::new();
    for (_, (key, value)) in normalized {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// Apply the documented Windows command-line quoting rules while still passing
/// the executable separately to CreateProcessW, so no shell is involved.
fn quote_windows_arg(value: impl AsRef<OsStr>) -> String {
    let text = value.as_ref().to_string_lossy();
    if !text.is_empty() && !text.chars().any(|c| c.is_whitespace() || c == '"') {
        return text.into_owned();
    }
    let mut result = String::from("\"");
    let mut backslashes = 0usize;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                result.push_str(&"\\".repeat(backslashes * 2 + 1));
                result.push('"');
                backslashes = 0;
            }
            _ => {
                result.push_str(&"\\".repeat(backslashes));
                result.push(character);
                backslashes = 0;
            }
        }
    }
    result.push_str(&"\\".repeat(backslashes * 2));
    result.push('"');
    result
}

/// Convert an OS string to a NUL-terminated UTF-16 buffer accepted by Win32.
fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

/// Generate a collision-resistant profile suffix without exposing timestamps
/// or host paths in diagnostics.
fn unique_nonce() -> u128 {
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos());
    clock
        ^ ((std::process::id() as u128) << 32)
        ^ (NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed) as u128)
}

/// Convert a Win32 status into a stable operation/code error.
fn os_error(operation: &'static str, code: u32) -> SandboxError {
    SandboxError::Os { operation, code }
}

/// Read the process exit code without turning a still-running process into a
/// false success; STILL_ACTIVE is represented as None.
fn read_exit_code(process: HANDLE) -> Option<u32> {
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 || code == 259 {
        None
    } else {
        Some(code)
    }
}

/// Return the last Win32 error while keeping all unsafe calls local to adapter
/// helpers that already translate errors into a stable Rust type.
fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Kill a process handle during the narrow startup failure window.  The Job is
/// not yet guaranteed to own it, so this is intentionally a direct fallback.
unsafe fn terminate_process_for_startup(process: HANDLE) {
    unsafe {
        let _ = windows_sys::Win32::System::Threading::TerminateProcess(process, 1);
    }
}
