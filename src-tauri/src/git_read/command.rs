// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::error::GitError;
use super::model::{GitDiff, GitLogEntry, GitShow, GitStatusEntry};
use super::parse::{parse_log, parse_status};
use super::process::run_git;
use crate::workspace_read::{WorkspaceError, WorkspaceHandle};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Limits apply to every typed Git operation and cannot be changed by argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPolicy {
    pub timeout: Duration,
    pub cleanup_timeout: Duration,
    pub poll_interval: Duration,
    pub max_output_bytes: usize,
    pub max_error_bytes: usize,
    pub max_status_records: usize,
}

impl Default for GitPolicy {
    /// Uses interactive-safe timeout and output defaults for local Git.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            cleanup_timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(10),
            max_output_bytes: 8 * 1024 * 1024,
            max_error_bytes: 256 * 1024,
            max_status_records: 100_000,
        }
    }
}

/// A cloneable cancellation handle lets UI close/workspace-switch code stop a
/// Git request without exposing a process handle to the WebView.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Signals cancellation exactly once; running adapters observe it at the
    /// same bounded polling cadence as timeout checks.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Reads cancellation without blocking the caller or taking a shared lock.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Restricts diff requests to worktree or index state and an optional exact
/// root-relative path; no user-provided pathspec grammar is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffOptions {
    pub staged: bool,
    pub relative_path: Option<String>,
}

impl Default for DiffOptions {
    /// Starts with the worktree diff because it is the least surprising view.
    fn default() -> Self {
        Self {
            staged: false,
            relative_path: None,
        }
    }
}

/// Read-only Git facade bound to one immutable canonical workspace handle.
#[derive(Debug, Clone)]
pub struct GitReadOnly {
    workspace: WorkspaceHandle,
    program: PathBuf,
    policy: GitPolicy,
}

impl GitReadOnly {
    /// Resolves Git once from the trusted launch environment; later commands
    /// use the canonical executable even though their child env is cleared.
    pub fn new(workspace: WorkspaceHandle) -> Result<Self, GitError> {
        validate_worktree(&workspace)?;
        let program = resolve_git_program().ok_or(GitError::GitUnavailable)?;
        Ok(Self {
            workspace,
            program,
            policy: GitPolicy::default(),
        })
    }

    /// Applies tighter caller limits while preventing a caller from widening
    /// the adapter's command surface or executable identity.
    pub fn with_policy(mut self, policy: GitPolicy) -> Result<Self, GitError> {
        if policy.timeout.is_zero()
            || policy.cleanup_timeout.is_zero()
            || policy.poll_interval.is_zero()
            || policy.max_output_bytes == 0
            || policy.max_error_bytes == 0
            || policy.max_status_records == 0
            || policy.timeout > Duration::from_secs(300)
            || policy.cleanup_timeout > Duration::from_secs(30)
            || policy.poll_interval > Duration::from_secs(1)
            || policy.max_output_bytes > 64 * 1024 * 1024
            || policy.max_error_bytes > 4 * 1024 * 1024
            || policy.max_status_records > 1_000_000
        {
            return Err(GitError::InvalidPolicy);
        }
        self.policy = policy;
        Ok(self)
    }

    /// Reads status through porcelain-v2 NUL records so filenames with spaces
    /// and Unicode never depend on human-oriented Git formatting.
    pub fn status(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitStatusEntry>, GitError> {
        let output = self.run(
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
            cancellation,
        )?;
        parse_status(&output, self.policy.max_status_records)
    }

    /// Returns bounded binary-safe diff bytes with external diff/textconv off.
    pub fn diff(
        &self,
        options: &DiffOptions,
        cancellation: &CancellationToken,
    ) -> Result<GitDiff, GitError> {
        let mut args = vec![
            OsString::from("diff"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-color"),
            OsString::from("--no-textconv"),
            OsString::from("--binary"),
        ];
        if options.staged {
            args.push(OsString::from("--cached"));
        }
        args.push(OsString::from("--"));
        if let Some(path) = &options.relative_path {
            self.validate_path(path)?;
            args.push(OsString::from(path));
        }
        let output = self.run_os(&args, cancellation)?;
        Ok(GitDiff {
            bytes: output,
            truncated: false,
        })
    }

    /// Returns a fixed NUL-delimited log projection with a bounded commit count.
    pub fn log(
        &self,
        max_count: usize,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitLogEntry>, GitError> {
        let count = max_count.clamp(1, 500);
        let format = "%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00";
        let args = vec![
            OsString::from("log"),
            OsString::from("--no-color"),
            OsString::from("--no-decorate"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from(format!("--format={format}")),
            OsString::from(format!("-n{count}")),
            OsString::from("--"),
        ];
        parse_log(&self.run_os(&args, cancellation)?)
    }

    /// Shows a validated object/path pair without allowing option injection or
    /// write-capable Git subcommands.
    pub fn show(
        &self,
        object: &str,
        relative_path: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<GitShow, GitError> {
        validate_object(object)?;
        let mut args = vec![
            OsString::from("show"),
            OsString::from("--no-color"),
            OsString::from("--no-ext-diff"),
            OsString::from("--no-textconv"),
            OsString::from("--format=fuller"),
            OsString::from(object),
            OsString::from("--"),
        ];
        if let Some(path) = relative_path {
            self.validate_path(path)?;
            args.push(OsString::from(path));
        }
        Ok(GitShow {
            bytes: self.run_os(&args, cancellation)?,
            truncated: false,
        })
    }

    fn validate_path(&self, path: &str) -> Result<(), GitError> {
        self.workspace
            .validate_git_path(path)
            .map_err(|error| match error {
                WorkspaceError::InvalidRelativePath => GitError::InvalidPath,
                _ => GitError::Workspace,
            })
    }

    fn run(&self, args: &[&str], cancellation: &CancellationToken) -> Result<Vec<u8>, GitError> {
        let args = args.iter().map(OsString::from).collect::<Vec<_>>();
        self.run_os(&args, cancellation)
    }

    fn run_os(
        &self,
        args: &[OsString],
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, GitError> {
        self.workspace
            .resolve_directory("")
            .map_err(|_| GitError::Workspace)?;
        validate_worktree(&self.workspace)?;
        let command = self.build_command(args);
        let output = run_git(command, &self.policy, cancellation)?;
        self.workspace
            .resolve_directory("")
            .map_err(|_| GitError::Workspace)?;
        validate_worktree(&self.workspace)?;
        if !output.status.success() {
            return Err(GitError::CommandFailed {
                code: output.status.code(),
            });
        }
        Ok(output.stdout)
    }

    /// Constructs a fixed environment and option prefix for every operation;
    /// local config cannot enable pager, textconv, external diff or prompts.
    fn build_command(&self, args: &[OsString]) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command
            .env_clear()
            .current_dir(self.workspace.root_path())
            .args([
                OsString::from("--no-optional-locks"),
                OsString::from("--no-pager"),
                OsString::from("-c"),
                OsString::from("core.pager=cat"),
                OsString::from("-c"),
                OsString::from("pager.diff=false"),
                OsString::from("-c"),
                OsString::from("color.ui=false"),
                OsString::from("-c"),
                OsString::from("diff.external="),
                OsString::from("-c"),
                OsString::from("diff.trustExitCode=false"),
                OsString::from("-c"),
                OsString::from("core.fsmonitor=false"),
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .env("GIT_CONFIG_COUNT", "4")
            .env("GIT_CONFIG_KEY_0", "alias.status")
            .env("GIT_CONFIG_VALUE_0", "")
            .env("GIT_CONFIG_KEY_1", "alias.diff")
            .env("GIT_CONFIG_VALUE_1", "")
            .env("GIT_CONFIG_KEY_2", "alias.log")
            .env("GIT_CONFIG_VALUE_2", "")
            .env("GIT_CONFIG_KEY_3", "alias.show")
            .env("GIT_CONFIG_VALUE_3", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", ":")
            .env("GIT_SEQUENCE_EDITOR", ":")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("TZ", "UTC");
        command
    }

    /// Exposes only the hardening assertion to unit tests without exposing
    /// arbitrary command construction to production callers.
    #[cfg(test)]
    pub(crate) fn replace_refs_are_disabled_for_test(&self) -> bool {
        self.build_command(&[OsString::from("status")])
            .get_envs()
            .find(|(key, _)| key.to_string_lossy() == "GIT_NO_REPLACE_OBJECTS")
            .and_then(|(_, value)| value)
            .is_some_and(|value| value == "1")
    }

    /// Exposes only environment hardening assertions so tests prove object
    /// directories cannot be redirected through inherited Git variables.
    #[cfg(test)]
    pub(crate) fn object_fetch_environment_is_hardened_for_test(&self) -> bool {
        let command = self.build_command(&[OsString::from("status")]);
        let mut lazy_fetch = false;
        let mut object_directory_clean = true;
        let mut alternates_clean = true;
        for (key, value) in command.get_envs() {
            match key.to_string_lossy().as_ref() {
                "GIT_NO_LAZY_FETCH" => {
                    lazy_fetch = value.is_some_and(|value| value == "1");
                }
                "GIT_OBJECT_DIRECTORY" => object_directory_clean &= value.is_none(),
                "GIT_ALTERNATE_OBJECT_DIRECTORIES" => alternates_clean &= value.is_none(),
                _ => {}
            }
        }
        lazy_fetch && object_directory_clean && alternates_clean
    }
}

/// Resolves a canonical native Git executable before child environment
/// isolation removes PATH, preventing a later command from selecting a new
/// executable through inherited process state.
fn resolve_git_program() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["git.exe", "git"]
    } else {
        &["git"]
    };
    for directory in std::env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file()
                && let Ok(canonical) = candidate.canonicalize()
            {
                return Some(canonical);
            }
        }
    }
    None
}

/// Selects the platform null device used to disable global/system config.
fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Allows only Git revision spelling needed by read-only show, excluding all
/// option prefixes and shell/path metacharacters.
fn validate_object(object: &str) -> Result<(), GitError> {
    if object.is_empty() || object.len() > 256 || object.starts_with('-') {
        return Err(GitError::InvalidObject);
    }
    if object.chars().any(|character| {
        !(character.is_ascii_alphanumeric()
            || matches!(character, '.' | '_' | '/' | ':' | '@' | '^' | '~' | '-'))
    }) {
        return Err(GitError::InvalidObject);
    }
    if object.split(['/', ':']).any(|component| component == "..") {
        return Err(GitError::InvalidObject);
    }
    Ok(())
}

const MAX_POINTER_BYTES: usize = 4 * 1024;
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_CONFIG_LINE_BYTES: usize = 64 * 1024;
const MAX_OBJECT_DIRECTORIES: usize = 8 * 1024;
const MAX_OBJECT_FILES: usize = 200_000;
const MAX_OBJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_OBJECT_DEPTH: usize = 4;
const MAX_OBJECT_SCAN_TIME: Duration = Duration::from_secs(2);
const MAX_OBJECT_TEXT_BYTES: usize = 1024 * 1024;

/// Keeps object-store admission bounded even when a repository contains many
/// loose objects or a malformed recursive metadata tree.
#[derive(Debug, Clone, Copy)]
struct ObjectScanLimits {
    max_directories: usize,
    max_files: usize,
    max_bytes: u64,
    max_depth: usize,
    max_duration: Duration,
}

impl Default for ObjectScanLimits {
    /// Uses production limits that cover ordinary SHA-1/SHA-256 repositories
    /// while keeping pre-Git validation finite and interruptible by deadline.
    fn default() -> Self {
        Self {
            max_directories: MAX_OBJECT_DIRECTORIES,
            max_files: MAX_OBJECT_FILES,
            max_bytes: MAX_OBJECT_BYTES,
            max_depth: MAX_OBJECT_DEPTH,
            max_duration: MAX_OBJECT_SCAN_TIME,
        }
    }
}

/// Tracks absolute object-store scan budgets instead of allowing each nested
/// directory to reset the limit independently.
#[derive(Debug)]
struct ObjectScanBudget {
    limits: ObjectScanLimits,
    directories: usize,
    files: usize,
    bytes: u64,
    deadline: Instant,
}

impl ObjectScanBudget {
    /// Creates one absolute deadline for the complete object-store walk.
    fn new(limits: ObjectScanLimits) -> Self {
        let now = Instant::now();
        let deadline = now.checked_add(limits.max_duration).unwrap_or(now);
        Self {
            limits,
            directories: 0,
            files: 0,
            bytes: 0,
            deadline,
        }
    }

    /// Fails closed when the walk has exceeded its absolute time budget.
    fn check_deadline(&self) -> Result<(), GitError> {
        if Instant::now() >= self.deadline {
            Err(GitError::ExternalWorktree)
        } else {
            Ok(())
        }
    }

    /// Charges a directory and enforces the maximum recursion depth/count.
    fn visit_directory(&mut self, depth: usize) -> Result<(), GitError> {
        self.check_deadline()?;
        if depth > self.limits.max_depth {
            return Err(GitError::ExternalWorktree);
        }
        self.directories = self.directories.saturating_add(1);
        if self.directories > self.limits.max_directories {
            return Err(GitError::ExternalWorktree);
        }
        Ok(())
    }

    /// Charges file metadata bytes without reading loose object contents.
    fn visit_file(&mut self, bytes: u64) -> Result<(), GitError> {
        self.check_deadline()?;
        self.files = self.files.saturating_add(1);
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(GitError::ExternalWorktree)?;
        if self.files > self.limits.max_files || self.bytes > self.limits.max_bytes {
            return Err(GitError::ExternalWorktree);
        }
        Ok(())
    }
}

/// Rejects `.git` indirection, alternates, partial-clone metadata and config
/// hooks before Git can resolve an object or process outside the workspace.
fn validate_worktree(workspace: &WorkspaceHandle) -> Result<(), GitError> {
    workspace
        .resolve_directory("")
        .map_err(|_| GitError::ExternalWorktree)?;
    let git_path = workspace.root_path().join(".git");
    let metadata = match fs::symlink_metadata(&git_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Bare repositories keep the same object indirections directly
            // under the workspace, so validate them before Git can read HEAD.
            let has_config = match workspace.resolve_file("config") {
                Ok(_) => true,
                Err(WorkspaceError::PathNotFound) => false,
                Err(_) => return Err(GitError::ExternalWorktree),
            };
            if !has_config {
                return Ok(());
            }
            match workspace.resolve_file("HEAD") {
                Ok(_) => {}
                Err(WorkspaceError::PathNotFound) => return Ok(()),
                Err(_) => return Err(GitError::ExternalWorktree),
            }
            match workspace.resolve_directory("objects") {
                Ok(_) => {
                    validate_repo_location(workspace, "", workspace.root_path(), true)?;
                    reject_submodule_metadata(workspace, "")?;
                }
                Err(WorkspaceError::PathNotFound) => return Ok(()),
                Err(_) => return Err(GitError::ExternalWorktree),
            }
            return Ok(());
        }
        Err(_) => return Err(GitError::ExternalWorktree),
    };
    if metadata.file_type().is_symlink() || crate::workspace_read::is_reparse_point(&metadata) {
        return Err(GitError::ExternalWorktree);
    }
    if metadata.is_dir() {
        let git_dir = fs::canonicalize(&git_path).map_err(|_| GitError::ExternalWorktree)?;
        ensure_internal_pointer(workspace, &git_path, &git_dir)?;
        validate_repo_location(workspace, ".git", &git_dir, true)?;
        let common_rel = repo_child(".git", "commondir");
        if let Some(pointer) = read_optional_text_file(workspace, &common_rel, MAX_POINTER_BYTES)? {
            let common_dir = resolve_git_pointer(workspace.root_path(), &git_dir, pointer.trim())?;
            let common_rel = relative_path_text(workspace.root_path(), &common_dir)?;
            validate_repo_location(workspace, &common_rel, &common_dir, true)?;
            reject_submodule_metadata(workspace, &common_rel)?;
        }
        reject_submodule_metadata(workspace, ".git")?;
        return Ok(());
    }
    if metadata.is_file() {
        let contents = read_required_text_file(workspace, ".git", MAX_POINTER_BYTES)?;
        let value = contents
            .strip_prefix("gitdir:")
            .ok_or(GitError::ExternalWorktree)?
            .trim();
        let git_dir = resolve_git_pointer(workspace.root_path(), workspace.root_path(), value)?;
        let git_rel = relative_path_text(workspace.root_path(), &git_dir)?;
        validate_repo_location(workspace, &git_rel, &git_dir, false)?;
        let common_rel = repo_child(&git_rel, "commondir");
        if let Some(pointer) = read_optional_text_file(workspace, &common_rel, MAX_POINTER_BYTES)? {
            let common_dir = resolve_git_pointer(workspace.root_path(), &git_dir, pointer.trim())?;
            let common_rel = relative_path_text(workspace.root_path(), &common_dir)?;
            validate_repo_location(workspace, &common_rel, &common_dir, true)?;
            reject_submodule_metadata(workspace, &common_rel)?;
        }
        reject_submodule_metadata(workspace, &git_rel)?;
        return Ok(());
    }
    Err(GitError::ExternalWorktree)
}

/// Validates one trusted Git metadata directory and every object indirection
/// that could otherwise make a local read resolve an external object store.
fn validate_repo_location(
    workspace: &WorkspaceHandle,
    base_rel: &str,
    base_abs: &Path,
    objects_required: bool,
) -> Result<(), GitError> {
    if !validate_directory(workspace, base_rel, true)? {
        return Err(GitError::ExternalWorktree);
    }
    for config_name in ["config", "config.worktree"] {
        let config_rel = repo_child(base_rel, config_name);
        if let Some(config) = read_optional_text_file(workspace, &config_rel, MAX_CONFIG_BYTES)? {
            validate_config_text(workspace, base_abs, &config)?;
        }
    }
    let objects_rel = repo_child(base_rel, "objects");
    if !validate_directory(workspace, &objects_rel, objects_required)? {
        return Ok(());
    }
    validate_object_directory(workspace, &objects_rel)?;
    Ok(())
}

/// Checks an existing directory with the workspace guard so links, reparse
/// points, hard-link aliases and root replacement all fail closed.
fn validate_directory(
    workspace: &WorkspaceHandle,
    relative_path: &str,
    required: bool,
) -> Result<bool, GitError> {
    match workspace.resolve_directory(relative_path) {
        Ok(_) => Ok(true),
        Err(WorkspaceError::PathNotFound) if !required => Ok(false),
        Err(WorkspaceError::PathNotFound) => Err(GitError::ExternalWorktree),
        Err(_) => Err(GitError::ExternalWorktree),
    }
}

/// Validates the object root and every Git-readable object-store entry before
/// Git can resolve a loose or packed object outside the admitted tree.
fn validate_object_directory(
    workspace: &WorkspaceHandle,
    objects_rel: &str,
) -> Result<(), GitError> {
    validate_object_directory_with_limits(workspace, objects_rel, ObjectScanLimits::default())
}

/// Runs the bounded object walk with explicit limits so tests can exercise
/// overflow/deadline behavior without creating hundreds of thousands of files.
fn validate_object_directory_with_limits(
    workspace: &WorkspaceHandle,
    objects_rel: &str,
    limits: ObjectScanLimits,
) -> Result<(), GitError> {
    let objects = workspace
        .resolve_guard(objects_rel, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    let mut budget = ObjectScanBudget::new(limits);
    budget.visit_directory(0)?;
    workspace
        .verify_resolved(&objects, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    for entry in fs::read_dir(&objects.path).map_err(|_| GitError::ExternalWorktree)? {
        budget.check_deadline()?;
        let (name, metadata) = object_entry(entry.map_err(|_| GitError::ExternalWorktree)?)?;
        let child_rel = repo_child(objects_rel, &name);
        if metadata.is_dir() {
            if name == "info" {
                scan_info_directory(workspace, &child_rel, &mut budget, 1)?;
            } else if name == "pack" {
                scan_pack_directory(workspace, &child_rel, &mut budget, 1)?;
            } else if is_fanout_directory_name(&name) {
                scan_fanout_directory(workspace, &child_rel, &mut budget, 1)?;
            } else {
                return Err(GitError::ExternalWorktree);
            }
        } else {
            return Err(GitError::ExternalWorktree);
        }
    }
    workspace
        .verify_resolved(&objects, Some(true))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Exposes only bounded-limit injection to the module tests, keeping the
/// production adapter free of a caller-controlled object-store policy.
#[cfg(test)]
pub(crate) fn validate_object_directory_with_test_limits(
    workspace: &WorkspaceHandle,
    objects_rel: &str,
    max_directories: usize,
    max_files: usize,
    max_bytes: u64,
    max_depth: usize,
    max_duration: Duration,
) -> Result<(), GitError> {
    validate_object_directory_with_limits(
        workspace,
        objects_rel,
        ObjectScanLimits {
            max_directories,
            max_files,
            max_bytes,
            max_depth,
            max_duration,
        },
    )
}

/// Reads one directory entry without following links or accepting lossy names.
fn object_entry(entry: fs::DirEntry) -> Result<(String, fs::Metadata), GitError> {
    let name = entry
        .file_name()
        .into_string()
        .map_err(|_| GitError::ExternalWorktree)?;
    let metadata = fs::symlink_metadata(entry.path()).map_err(|_| GitError::ExternalWorktree)?;
    if metadata.file_type().is_symlink() || crate::workspace_read::is_reparse_point(&metadata) {
        return Err(GitError::ExternalWorktree);
    }
    Ok((name, metadata))
}

/// Scans a two-hex fanout directory and validates every loose-object filename
/// without opening or parsing its compressed object contents.
fn scan_fanout_directory(
    workspace: &WorkspaceHandle,
    relative: &str,
    budget: &mut ObjectScanBudget,
    depth: usize,
) -> Result<(), GitError> {
    let directory = workspace
        .resolve_guard(relative, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    budget.visit_directory(depth)?;
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    for entry in fs::read_dir(&directory.path).map_err(|_| GitError::ExternalWorktree)? {
        budget.check_deadline()?;
        let (name, metadata) = object_entry(entry.map_err(|_| GitError::ExternalWorktree)?)?;
        if !is_loose_object_name(&name) || !metadata.is_file() {
            return Err(GitError::ExternalWorktree);
        }
        scan_object_file(workspace, &repo_child(relative, &name), &mut *budget)?;
    }
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Scans the object info directory, including split commit-graph metadata.
fn scan_info_directory(
    workspace: &WorkspaceHandle,
    relative: &str,
    budget: &mut ObjectScanBudget,
    depth: usize,
) -> Result<(), GitError> {
    let directory = workspace
        .resolve_guard(relative, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    budget.visit_directory(depth)?;
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    for entry in fs::read_dir(&directory.path).map_err(|_| GitError::ExternalWorktree)? {
        budget.check_deadline()?;
        let (name, metadata) = object_entry(entry.map_err(|_| GitError::ExternalWorktree)?)?;
        let child = repo_child(relative, &name);
        if metadata.is_dir() && name == "commit-graphs" {
            scan_commit_graph_directory(workspace, &child, budget, depth + 1)?;
        } else if metadata.is_file() && is_info_file_name(&name) {
            scan_object_file(workspace, &child, budget)?;
            if matches!(name.as_str(), "alternates" | "http-alternates") {
                validate_alternates(workspace, relative)?;
            } else if name == "packs" {
                validate_pack_listing(workspace, &child)?;
            }
        } else {
            return Err(GitError::ExternalWorktree);
        }
    }
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Scans split commit-graph files and validates the chain's local filenames.
fn scan_commit_graph_directory(
    workspace: &WorkspaceHandle,
    relative: &str,
    budget: &mut ObjectScanBudget,
    depth: usize,
) -> Result<(), GitError> {
    let directory = workspace
        .resolve_guard(relative, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    budget.visit_directory(depth)?;
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    for entry in fs::read_dir(&directory.path).map_err(|_| GitError::ExternalWorktree)? {
        budget.check_deadline()?;
        let (name, metadata) = object_entry(entry.map_err(|_| GitError::ExternalWorktree)?)?;
        if !metadata.is_file() || !(name == "commit-graph-chain" || is_graph_file_name(&name)) {
            return Err(GitError::ExternalWorktree);
        }
        let child = repo_child(relative, &name);
        scan_object_file(workspace, &child, budget)?;
        if name == "commit-graph-chain" {
            validate_commit_graph_chain(workspace, &child)?;
        }
    }
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Scans pack/index/multi-pack-index sidecars without parsing their bytes.
fn scan_pack_directory(
    workspace: &WorkspaceHandle,
    relative: &str,
    budget: &mut ObjectScanBudget,
    depth: usize,
) -> Result<(), GitError> {
    let directory = workspace
        .resolve_guard(relative, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    budget.visit_directory(depth)?;
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)?;
    for entry in fs::read_dir(&directory.path).map_err(|_| GitError::ExternalWorktree)? {
        budget.check_deadline()?;
        let (name, metadata) = object_entry(entry.map_err(|_| GitError::ExternalWorktree)?)?;
        if !metadata.is_file() || !is_pack_file_name(&name) || is_promisor_file_name(&name) {
            return Err(GitError::ExternalWorktree);
        }
        scan_object_file(workspace, &repo_child(relative, &name), budget)?;
    }
    workspace
        .verify_resolved(&directory, Some(true))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Validates a regular object-store file's identity and hard-link count while
/// deliberately avoiding content reads of loose objects and pack binaries.
fn scan_object_file(
    workspace: &WorkspaceHandle,
    relative: &str,
    budget: &mut ObjectScanBudget,
) -> Result<(), GitError> {
    let resolved = workspace
        .resolve_guard(relative, Some(false))
        .map_err(|_| GitError::ExternalWorktree)?;
    let metadata = fs::symlink_metadata(&resolved.path).map_err(|_| GitError::ExternalWorktree)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || crate::workspace_read::is_reparse_point(&metadata)
    {
        return Err(GitError::ExternalWorktree);
    }
    budget.visit_file(metadata.len())?;
    workspace
        .verify_resolved(&resolved, Some(false))
        .map_err(|_| GitError::ExternalWorktree)
}

/// Rejects both alternates transports even when their value points back inside
/// the workspace; empty files are harmless, while any non-empty value is refused.
fn validate_alternates(workspace: &WorkspaceHandle, info_rel: &str) -> Result<(), GitError> {
    for name in ["alternates", "http-alternates"] {
        let relative_path = repo_child(info_rel, name);
        if let Some(value) = read_optional_text_file(workspace, &relative_path, MAX_POINTER_BYTES)?
            && !value.is_empty()
        {
            return Err(GitError::ExternalWorktree);
        }
    }
    Ok(())
}

/// Validates `info/packs` without allowing a content line to name an external
/// or path-like pack file.
fn validate_pack_listing(workspace: &WorkspaceHandle, relative: &str) -> Result<(), GitError> {
    let Some(contents) = read_optional_text_file(workspace, relative, MAX_OBJECT_TEXT_BYTES)?
    else {
        return Ok(());
    };
    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let name = line.strip_prefix("P ").ok_or(GitError::ExternalWorktree)?;
        if !is_pack_file_name(name) || is_promisor_file_name(name) {
            return Err(GitError::ExternalWorktree);
        }
    }
    Ok(())
}

/// Validates split commit-graph chain entries before Git follows their names.
fn validate_commit_graph_chain(
    workspace: &WorkspaceHandle,
    relative: &str,
) -> Result<(), GitError> {
    let Some(contents) = read_optional_text_file(workspace, relative, MAX_OBJECT_TEXT_BYTES)?
    else {
        return Ok(());
    };
    for line in contents.lines() {
        if !matches!(line.len(), 40 | 64) || !is_lower_hex(line.as_bytes()) {
            return Err(GitError::ExternalWorktree);
        }
    }
    Ok(())
}

/// Allows only Git's two-hex fanout directory names.
fn is_fanout_directory_name(name: &str) -> bool {
    name.len() == 2 && is_lower_hex(name.as_bytes())
}

/// Allows SHA-1 and SHA-256 loose object basenames only.
fn is_loose_object_name(name: &str) -> bool {
    matches!(name.len(), 38 | 62) && is_lower_hex(name.as_bytes())
}

/// Allows the bounded object metadata files Git can read from `objects/info`.
fn is_info_file_name(name: &str) -> bool {
    matches!(
        name,
        "alternates" | "http-alternates" | "packs" | "commit-graph" | "commit-graph-chain"
    )
}

/// Allows split commit-graph file names without accepting path syntax.
fn is_graph_file_name(name: &str) -> bool {
    let Some(hash) = name
        .strip_prefix("graph-")
        .and_then(|value| value.strip_suffix(".graph"))
    else {
        return false;
    };
    matches!(hash.len(), 40 | 64) && is_lower_hex(hash.as_bytes())
}

/// Allows pack/index/bitmap/reverse-index/multi-pack-index sidecars only.
fn is_pack_file_name(name: &str) -> bool {
    if name == "multi-pack-index" {
        return true;
    }
    if let Some(hash) = name
        .strip_prefix("multi-pack-index-")
        .and_then(|value| value.strip_suffix(".bitmap"))
    {
        return matches!(hash.len(), 40 | 64) && is_lower_hex(hash.as_bytes());
    }
    let Some(rest) = name.strip_prefix("pack-") else {
        return false;
    };
    let Some((hash, extension)) = rest.rsplit_once('.') else {
        return false;
    };
    matches!(hash.len(), 40 | 64)
        && is_lower_hex(hash.as_bytes())
        && matches!(
            extension,
            "pack" | "idx" | "bitmap" | "rev" | "mtimes" | "keep"
        )
}

/// Identifies lazy-fetch marker files before the generic pack grammar accepts
/// any future-looking extension.
fn is_promisor_file_name(name: &str) -> bool {
    name.strip_prefix("pack-")
        .and_then(|value| value.rsplit_once('.'))
        .is_some_and(|(_, extension)| extension == "promisor")
}

/// Accepts ASCII lowercase hexadecimal only to avoid case/Unicode aliases.
fn is_lower_hex(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Rejects submodule metadata in the first read-only release rather than
/// allowing Git to resolve another repository with a separate trust root.
fn reject_submodule_metadata(workspace: &WorkspaceHandle, base_rel: &str) -> Result<(), GitError> {
    let modules_rel = repo_child(base_rel, "modules");
    if validate_directory(workspace, &modules_rel, false)? {
        return Err(GitError::ExternalWorktree);
    }
    if read_optional_text_file(workspace, ".gitmodules", MAX_CONFIG_BYTES)?.is_some() {
        return Err(GitError::ExternalWorktree);
    }
    Ok(())
}

/// Parses only local config text and refuses includes or process/network
/// indirections before any referenced path is opened.
fn validate_config_text(
    workspace: &WorkspaceHandle,
    base_abs: &Path,
    config: &str,
) -> Result<(), GitError> {
    let mut section = String::new();
    for raw_line in config.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.len() > MAX_CONFIG_LINE_BYTES {
            return Err(GitError::ExternalWorktree);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') {
            if !trimmed.ends_with(']') {
                return Err(GitError::ExternalWorktree);
            }
            let inside = trimmed[1..trimmed.len() - 1].trim();
            section = inside
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
                .map(|value| value.trim_matches('"').to_ascii_lowercase())
                .ok_or(GitError::ExternalWorktree)?;
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .map(|(key, value)| (key.trim(), value.trim()))
            .or_else(|| {
                trimmed
                    .split_once(char::is_whitespace)
                    .map(|(key, value)| (key.trim(), value.trim()))
            })
            .ok_or(GitError::ExternalWorktree)?;
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            return Err(GitError::ExternalWorktree);
        }
        let key = key.to_ascii_lowercase();
        let value = value.trim_matches('"').trim_matches('\'');
        if section.starts_with("include") && key == "path" {
            return Err(GitError::ExternalWorktree);
        }
        if section == "extensions"
            && matches!(
                key.as_str(),
                "worktreeconfig" | "partialclone" | "partialclonefilter"
            )
        {
            return Err(GitError::ExternalWorktree);
        }
        if section == "remote" && key == "promisor" {
            return Err(GitError::ExternalWorktree);
        }
        if section == "core" {
            match key.as_str() {
                "worktree" => validate_config_worktree(workspace, base_abs, value)?,
                "fsmonitor" | "sshcommand" | "hookspath" => return Err(GitError::ExternalWorktree),
                _ => {}
            }
        }
        if (section == "filter" && matches!(key.as_str(), "process" | "clean" | "smudge"))
            || (section == "diff" && matches!(key.as_str(), "external" | "textconv"))
            || (section == "credential" && key == "helper")
            || section == "submodule"
            || (section == "url" && matches!(key.as_str(), "insteadof" | "pushinsteadof"))
        {
            return Err(GitError::ExternalWorktree);
        }
    }
    Ok(())
}

/// Allows only an internal `core.worktree` directory without opening an
/// external path while deciding whether the config is safe.
fn validate_config_worktree(
    workspace: &WorkspaceHandle,
    base_abs: &Path,
    value: &str,
) -> Result<(), GitError> {
    let target = resolve_internal_pointer(workspace.root_path(), base_abs, value)?;
    let relative = relative_path_text(workspace.root_path(), &target)?;
    workspace
        .resolve_directory(&relative)
        .map_err(|_| GitError::ExternalWorktree)?;
    Ok(())
}

/// Reads a trusted, bounded UTF-8 file while retaining the component guard
/// across the read so an alternates/config swap cannot bypass validation.
fn read_optional_text_file(
    workspace: &WorkspaceHandle,
    relative_path: &str,
    max_bytes: usize,
) -> Result<Option<String>, GitError> {
    let resolved = match workspace.resolve_guard(relative_path, Some(false)) {
        Ok(resolved) => resolved,
        Err(WorkspaceError::PathNotFound) => return Ok(None),
        Err(_) => return Err(GitError::ExternalWorktree),
    };
    let file = fs::File::open(&resolved.path).map_err(|_| GitError::ExternalWorktree)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| GitError::ExternalWorktree)?;
    workspace
        .verify_resolved(&resolved, Some(false))
        .map_err(|_| GitError::ExternalWorktree)?;
    if bytes.len() > max_bytes || bytes.contains(&0) {
        return Err(GitError::ExternalWorktree);
    }
    let text = String::from_utf8(bytes).map_err(|_| GitError::ExternalWorktree)?;
    if text.lines().any(|line| line.len() > MAX_CONFIG_LINE_BYTES) {
        return Err(GitError::ExternalWorktree);
    }
    Ok(Some(text))
}

/// Converts a required metadata file read into the stable worktree error.
fn read_required_text_file(
    workspace: &WorkspaceHandle,
    relative_path: &str,
    max_bytes: usize,
) -> Result<String, GitError> {
    read_optional_text_file(workspace, relative_path, max_bytes)?.ok_or(GitError::ExternalWorktree)
}

/// Builds slash-separated root-relative metadata paths without exposing an
/// absolute path to the future IPC contract.
fn repo_child(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

/// Rejects an internal pointer whose raw spelling already contains a link and
/// then confirms its canonical target remains under the admitted root.
fn resolve_git_pointer(root: &Path, base: &Path, value: &str) -> Result<PathBuf, GitError> {
    resolve_internal_pointer(root, base, value)
}

/// Resolves a config/pointer path only after a lexical containment check, so
/// an external value is rejected before any external filesystem access.
fn resolve_internal_pointer(root: &Path, base: &Path, value: &str) -> Result<PathBuf, GitError> {
    if value.is_empty() || value.len() > MAX_POINTER_BYTES || value.contains('\0') {
        return Err(GitError::ExternalWorktree);
    }
    let path = Path::new(value);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    if !lexical_path_is_within(root, &candidate) {
        return Err(GitError::ExternalWorktree);
    }
    crate::workspace_read::reject_link_components(&candidate)
        .map_err(|_| GitError::ExternalWorktree)?;
    let canonical = fs::canonicalize(candidate).map_err(|_| GitError::ExternalWorktree)?;
    if !crate::workspace_read::path_is_within(root, &canonical) {
        return Err(GitError::ExternalWorktree);
    }
    Ok(canonical)
}

/// Normalizes only path syntax, never filesystem links, for a pre-I/O escape
/// check on config and worktree pointer values.
fn lexical_path_is_within(root: &Path, candidate: &Path) -> bool {
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    crate::workspace_read::path_is_within(root, &normalized)
}

/// Derives a strict UTF-8 relative spelling from an already canonical path.
fn relative_path_text(root: &Path, path: &Path) -> Result<String, GitError> {
    if !crate::workspace_read::path_is_within(root, path) {
        return Err(GitError::ExternalWorktree);
    }
    let root_count = root.components().count();
    let mut parts = Vec::new();
    for component in path.components().skip(root_count) {
        let Component::Normal(name) = component else {
            return Err(GitError::ExternalWorktree);
        };
        parts.push(name.to_str().ok_or(GitError::ExternalWorktree)?);
    }
    Ok(parts.join("/"))
}

/// Confirms the raw and canonical `.git` directory identities before any
/// pointer-derived metadata path is accepted.
fn ensure_internal_pointer(
    workspace: &WorkspaceHandle,
    raw: &Path,
    canonical: &Path,
) -> Result<(), GitError> {
    crate::workspace_read::reject_link_components(raw).map_err(|_| GitError::ExternalWorktree)?;
    if !crate::workspace_read::path_is_within(workspace.root_path(), canonical) {
        return Err(GitError::ExternalWorktree);
    }
    Ok(())
}
