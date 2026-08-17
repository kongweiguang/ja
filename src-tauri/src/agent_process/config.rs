// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Sidecar launch configuration and boundary validation.

use crate::agent_process::codec::Limits;
use crate::agent_process::error::AgentProcessError;
use crate::agent_process::handshake::{
    MAX_READY_TIMEOUT, MAX_SHUTDOWN_TIMEOUT, allowed_env_name, contains_secret_marker,
    default_initialize_params, validate_initialize_params,
};
use crate::agent_process::lifecycle::RestartPolicy;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// 启动参数只允许显式 run directory 与 allowlist 环境，不继承桌面进程环境。
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub run_dir: PathBuf,
    /// 可选 workspace 根；配置后 run_dir 必须位于其外部，避免 host 自身目录被 sidecar 复用。
    pub workspace_root: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub limits: Limits,
    pub initialize_params: Value,
    pub ready_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub restart: RestartPolicy,
    canonical_executable: PathBuf,
    canonical_run_dir: PathBuf,
    canonical_workspace_root: Option<PathBuf>,
    #[cfg(windows)]
    executable_identity: Arc<Mutex<Option<ExecutableIdentity>>>,
}

/// Retains only the matrix-proven OS runtime values for the native sidecar; credentials, proxy
/// settings, and arbitrary user environment remain excluded after env_clear. PATH and ComSpec
/// are non-secret coding-runtime inputs needed by AgentScope shell/MCP command lookup. The
/// Windows matrix showed that `SystemRoot` plus either temporary alias is sufficient, so both
/// aliases point at the already-owned sidecar run directory instead of inheriting user temp.
fn default_runtime_environment(run_dir: &Path) -> BTreeMap<OsString, OsString> {
    default_runtime_environment_from(run_dir, |name| std::env::var_os(name))
}

/// Builds the fixed environment from a lookup seam so tests can inject a process environment
/// without mutating the host process while production still reads only the current process.
fn default_runtime_environment_from<F>(run_dir: &Path, lookup: F) -> BTreeMap<OsString, OsString>
where
    F: for<'a> Fn(&'a str) -> Option<OsString>,
{
    let mut environment = BTreeMap::new();
    #[cfg(windows)]
    {
        for name in ["SystemRoot", "PATH", "ComSpec"] {
            if let Some(value) = lookup(name) {
                environment.insert(OsString::from(name), value);
            }
        }
        let temporary = run_dir.as_os_str().to_owned();
        environment.insert(OsString::from("TEMP"), temporary.clone());
        environment.insert(OsString::from("TMP"), temporary);
    }
    #[cfg(target_os = "macos")]
    {
        // Java's macOS temp property is sourced from TMPDIR; HOME and locale
        // inheritance stays off until a real native fixture proves it is needed.
        if let Some(value) = lookup("PATH") {
            environment.insert(OsString::from("PATH"), value);
        }
        environment.insert(OsString::from("TMPDIR"), run_dir.as_os_str().to_owned());
    }
    environment
}

impl SidecarConfig {
    /// 用符合 v1 Schema 的最小 initialize 参数建立可审计默认配置。
    pub fn new(executable: impl Into<PathBuf>, run_dir: impl Into<PathBuf>) -> Self {
        let limits = Limits::default();
        let executable = executable.into();
        let run_dir = run_dir.into();
        let canonical_run_dir = fs::canonicalize(&run_dir).unwrap_or_else(|_| run_dir.clone());
        Self {
            executable: fs::canonicalize(&executable).unwrap_or_else(|_| executable.clone()),
            args: Vec::new(),
            run_dir: canonical_run_dir.clone(),
            workspace_root: None,
            env: default_runtime_environment(&canonical_run_dir),
            initialize_params: default_initialize_params(&limits),
            limits,
            ready_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(3),
            restart: RestartPolicy::default(),
            canonical_executable: fs::canonicalize(&executable).unwrap_or(executable),
            canonical_run_dir,
            canonical_workspace_root: None,
            #[cfg(windows)]
            executable_identity: Arc::new(Mutex::new(None)),
        }
    }

    /// 设置 workspace containment 根并同时冻结 canonical identity，避免 spawn 时解析到替换目录。
    pub fn set_workspace_root(&mut self, workspace_root: Option<PathBuf>) {
        self.workspace_root = workspace_root;
        self.canonical_workspace_root = self
            .workspace_root
            .as_ref()
            .and_then(|root| fs::canonicalize(root).ok());
    }

    /// 返回构造时冻结的 executable，spawn 不再信任外部可变 PathBuf。
    pub(super) fn canonical_executable(&self) -> &PathBuf {
        &self.canonical_executable
    }

    /// 返回构造时冻结的 run directory，避免符号链接替换改变 sidecar 工作目录。
    pub(super) fn canonical_run_dir(&self) -> &PathBuf {
        &self.canonical_run_dir
    }

    /// 在 spawn 前拒绝相对路径、继承环境和疑似 secret 参数，防止 sidecar 越界获得隐式权限。
    pub fn validate(&self) -> Result<(), AgentProcessError> {
        if !self.executable.is_absolute()
            || !self.run_dir.is_absolute()
            || !self.run_dir.is_dir()
            || self.ready_timeout.is_zero()
            || self.shutdown_timeout.is_zero()
            || self.ready_timeout > MAX_READY_TIMEOUT
            || self.shutdown_timeout > MAX_SHUTDOWN_TIMEOUT
        {
            return Err(AgentProcessError::InvalidConfig);
        }
        if self.executable != self.canonical_executable
            || self.run_dir != self.canonical_run_dir
            || self
                .workspace_root
                .as_ref()
                .zip(self.canonical_workspace_root.as_ref())
                .is_some_and(|(configured, canonical)| {
                    fs::canonicalize(configured).ok().as_ref() != Some(canonical)
                })
            || self.workspace_root.is_some() != self.canonical_workspace_root.is_some()
        {
            return Err(AgentProcessError::InvalidConfig);
        }
        let canonical_executable =
            fs::canonicalize(&self.executable).map_err(|_| AgentProcessError::InvalidConfig)?;
        let canonical_run_dir =
            fs::canonicalize(&self.run_dir).map_err(|_| AgentProcessError::InvalidConfig)?;
        if canonical_executable != self.canonical_executable
            || canonical_run_dir != self.canonical_run_dir
            || !canonical_executable.is_file()
            || !canonical_run_dir.is_dir()
        {
            return Err(AgentProcessError::InvalidConfig);
        }
        self.verify_executable_identity()?;
        if let Some(workspace_root) = &self.workspace_root {
            let canonical_workspace =
                fs::canonicalize(workspace_root).map_err(|_| AgentProcessError::InvalidConfig)?;
            if Some(&canonical_workspace) != self.canonical_workspace_root.as_ref()
                || !canonical_workspace.is_dir()
                || canonical_run_dir == canonical_workspace
                || canonical_run_dir.starts_with(&canonical_workspace)
            {
                return Err(AgentProcessError::InvalidConfig);
            }
        }
        self.limits.validate()?;
        self.restart.validate()?;
        if !self.initialize_params.is_object() {
            return Err(AgentProcessError::InvalidConfig);
        }
        validate_initialize_params(&self.initialize_params, &self.limits)
            .map_err(|_| AgentProcessError::InvalidConfig)?;
        if self
            .args
            .iter()
            .any(|arg| contains_secret_marker(&arg.to_string_lossy()))
        {
            return Err(AgentProcessError::InvalidConfig);
        }
        for (name, value) in &self.env {
            let name = name.to_string_lossy();
            if !allowed_env_name(&name)
                || contains_secret_marker(&name)
                || (!matches!(name.as_ref(), "PATH" | "ComSpec")
                    && contains_secret_marker(&value.to_string_lossy()))
            {
                return Err(AgentProcessError::InvalidConfig);
            }
        }
        Ok(())
    }

    /// Validate and retain the executable identity until spawn so a concurrent
    /// replacement cannot redirect the sidecar between config validation and launch.
    pub(super) fn verify_executable_identity(&self) -> Result<(), AgentProcessError> {
        #[cfg(windows)]
        {
            let mut identity = self
                .executable_identity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(identity) = identity.as_ref() {
                identity
                    .verify(&self.canonical_executable)
                    .map_err(|_| AgentProcessError::InvalidConfig)?;
            } else {
                *identity = Some(
                    ExecutableIdentity::open(&self.canonical_executable)
                        .map_err(|_| AgentProcessError::InvalidConfig)?,
                );
            }
        }
        #[cfg(not(windows))]
        {
            // POSIX identity pinning belongs to the platform process adapter;
            // canonical revalidation still rejects the common symlink swap.
            let current =
                fs::canonicalize(&self.executable).map_err(|_| AgentProcessError::InvalidConfig)?;
            if current != self.canonical_executable || !current.is_file() {
                return Err(AgentProcessError::InvalidConfig);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SidecarConfig, default_runtime_environment_from};
    use crate::agent_process::AgentProcessError;
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    /// Locks the Windows launch baseline to the matrix-proven OS/runtime values and prevents
    /// PATH or an unrelated user variable from becoming an implicit sidecar capability.
    #[test]
    fn default_runtime_environment_is_narrow() {
        let run_dir = PathBuf::from("ja-owned-run-dir");
        let process_env = BTreeMap::from([
            ("SystemRoot".to_owned(), OsString::from("C:\\Windows")),
            (
                "PATH".to_owned(),
                OsString::from("C:\\secret-project\\bin;C:\\Windows\\System32"),
            ),
            (
                "ComSpec".to_owned(),
                OsString::from("C:\\Windows\\System32\\cmd.exe"),
            ),
            (
                "OPENAI_API_KEY".to_owned(),
                OsString::from("should-not-cross"),
            ),
            (
                "HTTP_PROXY".to_owned(),
                OsString::from("http://proxy.invalid"),
            ),
        ]);
        let environment =
            default_runtime_environment_from(&run_dir, |name| process_env.get(name).cloned());
        let names = environment
            .keys()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        #[cfg(windows)]
        assert!(names.iter().all(|name| matches!(
            name.as_str(),
            "SystemRoot" | "PATH" | "ComSpec" | "TEMP" | "TMP"
        )));
        #[cfg(windows)]
        assert_eq!(
            environment.get(OsStr::new("PATH")),
            Some(&OsString::from(
                "C:\\secret-project\\bin;C:\\Windows\\System32"
            ))
        );
        #[cfg(windows)]
        assert_eq!(
            environment.get(OsStr::new("ComSpec")),
            Some(&OsString::from("C:\\Windows\\System32\\cmd.exe"))
        );
        #[cfg(windows)]
        for name in ["TEMP", "TMP"] {
            assert_eq!(
                environment.get(OsStr::new(name)).map(OsString::as_os_str),
                Some(run_dir.as_os_str())
            );
        }
        #[cfg(target_os = "macos")]
        assert!(
            names
                .iter()
                .all(|name| matches!(name.as_str(), "PATH" | "TMPDIR"))
        );
        #[cfg(any(windows, target_os = "macos"))]
        assert_eq!(
            environment.get(OsStr::new("PATH")),
            Some(&OsString::from(
                "C:\\secret-project\\bin;C:\\Windows\\System32"
            ))
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            environment
                .get(OsStr::new("TMPDIR"))
                .map(OsString::as_os_str),
            Some(run_dir.as_os_str())
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        assert!(names.is_empty());
        assert!(!names.iter().any(|name| name == "OPENAI_API_KEY"));
        assert!(!names.iter().any(|name| name == "HTTP_PROXY"));
    }

    /// Accepts a PATH directory containing a marker-like name while still rejecting arbitrary
    /// credential variables, because PATH/ComSpec are exact non-secret runtime slots only.
    #[cfg(windows)]
    #[test]
    fn coding_runtime_environment_validates_without_secret_value_false_positive() {
        let executable = std::env::current_exe().expect("current test executable");
        let run_dir = std::env::temp_dir();
        let mut config = SidecarConfig::new(&executable, &run_dir);
        config.env.insert(
            OsString::from("PATH"),
            OsString::from("C:\\secret-project\\bin;C:\\Windows\\System32"),
        );
        config.env.insert(
            OsString::from("ComSpec"),
            OsString::from("C:\\Windows\\System32\\cmd.exe"),
        );
        assert!(config.validate().is_ok());
        config
            .env
            .insert(OsString::from("OPENAI_API_KEY"), OsString::from("sk-test"));
        assert_eq!(config.validate(), Err(AgentProcessError::InvalidConfig));
    }
}

#[cfg(windows)]
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
#[cfg(windows)]
const SYNCHRONIZE: u32 = 0x0010_0000;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

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
const INVALID_HANDLE_VALUE: *mut c_void = -1_isize as *mut c_void;

#[cfg(windows)]
#[derive(Debug)]
struct ExecutableIdentity {
    handle: *mut c_void,
    volume_serial_number: u32,
    file_index: u64,
}

#[cfg(windows)]
unsafe impl Send for ExecutableIdentity {}

#[cfg(windows)]
unsafe impl Sync for ExecutableIdentity {}

#[cfg(windows)]
impl ExecutableIdentity {
    /// Open without write/delete sharing so Windows itself rejects replacement
    /// attempts while the immutable config remains usable by the supervisor.
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
        let ok = unsafe { GetFileInformationByHandle(handle, information.as_mut_ptr()) } != 0;
        if !ok {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }
        let information = unsafe { information.assume_init() };
        Ok(Self {
            handle,
            volume_serial_number: information.volume_serial_number,
            file_index: (u64::from(information.file_index_high) << 32)
                | u64::from(information.file_index_low),
        })
    }

    /// Re-read the immutable handle identity to catch unexpected handle loss or
    /// a path mutation before the process creation API is reached.
    fn verify(&self, path: &std::path::Path) -> std::io::Result<()> {
        if fs::canonicalize(path)
            .map(|canonical| canonical != path)
            .unwrap_or(true)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "executable identity path changed",
            ));
        }
        let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::uninit();
        if unsafe { GetFileInformationByHandle(self.handle, information.as_mut_ptr()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let information = unsafe { information.assume_init() };
        let file_index =
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
        if information.volume_serial_number != self.volume_serial_number
            || file_index != self.file_index
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "executable identity changed",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ExecutableIdentity {
    /// Release the identity guard only after all config clones stop using it.
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
