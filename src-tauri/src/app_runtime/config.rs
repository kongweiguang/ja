// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Trusted sidecar launch policy and typed command DTOs.

use crate::agent_process::{AgentProcessError, LifecycleState, SidecarConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(any(test, feature = "test-support", debug_assertions))]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

const TURN_DEADLINE: Duration = Duration::from_secs(4);
const READY_DEADLINE: Duration = Duration::from_secs(10);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);
const RECOVERY_FILE_NAME: &str = "ja-runtime-recovery.json";
const RECOVERY_ACK_FILE_NAME: &str = "ja-runtime-recovery-ack.json";
const RECOVERY_TEMP_PREFIX: &str = "ja-runtime-recovery.json.tmp-";
const RECOVERY_ACK_TEMP_PREFIX: &str = "ja-runtime-recovery-ack.json.tmp-";
const RECOVERY_SCHEMA_VERSION: u64 = 2;
const MAX_RECOVERY_BYTES: u64 = 4096;
static NEXT_RECOVERY_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Event delivery can fail without exposing a Tauri/path/secret diagnostic to
/// the WebView; the bridge turns this into an observable fault projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventEmitError {
    QueueFull,
    DeliveryFailed,
}

/// A sink isolates Tauri event emission from the actor and makes lifecycle
/// tests deterministic without creating a WebView or a real Tauri app.
pub type EventSink = Arc<dyn Fn(Value) -> Result<(), EventEmitError> + Send + Sync + 'static>;

/// The reason is deliberately closed so the recovery command cannot become a
/// free-form path/process acknowledgement channel.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ManualRecoveryReason {
    SystemRestarted,
    ExternallyCleaned,
}

/// The only user action that can clear a recovery marker.  Identity and
/// revision are echoed from the native state projection to reject stale UI.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ManualRecoveryConfirmation {
    pub recovery_id: String,
    pub revision: u64,
    pub reason: ManualRecoveryReason,
}

/// Token-free native projection used while a marker or transaction tombstone
/// requires explicit manual recovery.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRecoveryState {
    pub required: bool,
    pub acknowledgeable: bool,
    pub recovery_id: Option<String>,
    pub revision: Option<u64>,
}

/// Strict on-disk marker written after cleanup was not confirmed before a
/// forced process exit.  Its purpose is diagnosis and an explicit recovery
/// gate, not an ownership or automatic process-reaping protocol.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecoveryMarker {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    status: RecoveryMarkerStatus,
    #[serde(rename = "recoveryId")]
    recovery_id: String,
    revision: u64,
    generation: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RecoveryMarkerStatus {
    ManualRecoveryRequired,
}

/// Separate acknowledgement evidence lets the explicit user action be
/// recorded before the pending marker is atomically removed.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RecoveryAcknowledgement {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    status: RecoveryAcknowledgementStatus,
    #[serde(rename = "recoveryId")]
    recovery_id: String,
    revision: u64,
    reason: ManualRecoveryReason,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RecoveryAcknowledgementStatus {
    ManualRecoveryAckPending,
}

/// Returns the fixed marker path under the trusted app-private runtime dir.
pub(super) fn recovery_marker_path(run_dir: &Path) -> PathBuf {
    run_dir.join(RECOVERY_FILE_NAME)
}

/// Reads the native recovery gate without exposing paths, process ids or
/// marker contents to the WebView.  Any malformed or interrupted transaction
/// remains fail-closed and simply becomes non-acknowledgeable.
pub(super) fn recovery_state(run_dir: &Path) -> RuntimeRecoveryState {
    if ensure_private_run_dir(run_dir).is_err() {
        return RuntimeRecoveryState {
            required: true,
            acknowledgeable: false,
            recovery_id: None,
            revision: None,
        };
    }
    let entries = match fs::read_dir(run_dir) {
        Ok(entries) => entries,
        Err(_) => {
            return RuntimeRecoveryState {
                required: true,
                acknowledgeable: false,
                recovery_id: None,
                revision: None,
            };
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                return RuntimeRecoveryState {
                    required: true,
                    acknowledgeable: false,
                    recovery_id: None,
                    revision: None,
                };
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(RECOVERY_TEMP_PREFIX) || name.starts_with(RECOVERY_ACK_TEMP_PREFIX) {
            return RuntimeRecoveryState {
                required: true,
                acknowledgeable: false,
                recovery_id: None,
                revision: None,
            };
        }
    }
    let marker = read_recovery_marker(&recovery_marker_path(run_dir));
    let acknowledgement = read_recovery_ack(&run_dir.join(RECOVERY_ACK_FILE_NAME));
    match (marker, acknowledgement) {
        (Ok(Some(marker)), Ok(None)) => recovery_projection(Some(&marker), true),
        (Ok(None), Ok(Some(ack))) => recovery_projection(Some(&ack), true),
        (Ok(Some(marker)), Ok(Some(ack)))
            if marker.recovery_id == ack.recovery_id && marker.revision == ack.revision =>
        {
            recovery_projection(Some(&marker), true)
        }
        (Ok(None), Ok(None)) => RuntimeRecoveryState {
            required: false,
            acknowledgeable: false,
            recovery_id: None,
            revision: None,
        },
        _ => RuntimeRecoveryState {
            required: true,
            acknowledgeable: false,
            recovery_id: None,
            revision: None,
        },
    }
}

/// Rejects any marker, acknowledgement tombstone, or temp remnant before a
/// new sidecar is launched; only an explicit typed acknowledgement unlocks it.
pub(super) fn ensure_recovery_clear(run_dir: &Path) -> Result<(), RuntimeCommandError> {
    if recovery_state(run_dir).required {
        Err(RuntimeCommandError::recovery_required())
    } else {
        Ok(())
    }
}

/// Parses the versioned marker with serde's duplicate/unknown/type checks and
/// a bounded UUID-like identity so ambiguous disk data cannot unlock startup.
fn parse_recovery_marker(bytes: &[u8]) -> Result<RecoveryMarker, RuntimeCommandError> {
    if bytes.len() as u64 > MAX_RECOVERY_BYTES {
        return Err(RuntimeCommandError::recovery_required());
    }
    let marker: RecoveryMarker =
        serde_json::from_slice(bytes).map_err(|_| RuntimeCommandError::recovery_required())?;
    if marker.schema_version != RECOVERY_SCHEMA_VERSION
        || marker.revision == 0
        || Uuid::parse_str(&marker.recovery_id).is_err()
    {
        return Err(RuntimeCommandError::recovery_required());
    }
    Ok(marker)
}

/// Parses the short-lived acknowledgement tombstone with the same strict
/// schema and identity checks as the pending marker.
fn parse_recovery_ack(bytes: &[u8]) -> Result<RecoveryAcknowledgement, RuntimeCommandError> {
    if bytes.len() as u64 > MAX_RECOVERY_BYTES {
        return Err(RuntimeCommandError::recovery_required());
    }
    let acknowledgement: RecoveryAcknowledgement =
        serde_json::from_slice(bytes).map_err(|_| RuntimeCommandError::recovery_required())?;
    if acknowledgement.schema_version != RECOVERY_SCHEMA_VERSION
        || Uuid::parse_str(&acknowledgement.recovery_id).is_err()
        || acknowledgement.revision == 0
    {
        return Err(RuntimeCommandError::recovery_required());
    }
    Ok(acknowledgement)
}

/// Reads a regular, bounded marker file; symlink/reparse/permission errors
/// intentionally collapse into a fail-closed parse error.
fn read_recovery_marker(path: &Path) -> Result<Option<RecoveryMarker>, RuntimeCommandError> {
    read_recovery_bytes(path)
        .and_then(|bytes| bytes.map_or(Ok(None), |bytes| parse_recovery_marker(&bytes).map(Some)))
}

/// Reads a regular, bounded transaction tombstone using the same path policy.
fn read_recovery_ack(path: &Path) -> Result<Option<RecoveryAcknowledgement>, RuntimeCommandError> {
    read_recovery_bytes(path)
        .and_then(|bytes| bytes.map_or(Ok(None), |bytes| parse_recovery_ack(&bytes).map(Some)))
}

/// Reads a known recovery file without following reparse indirection.
fn read_recovery_bytes(path: &Path) -> Result<Option<Vec<u8>>, RuntimeCommandError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(RuntimeCommandError::recovery_required()),
    };
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(RuntimeCommandError::recovery_required());
    }
    if metadata.len() > MAX_RECOVERY_BYTES {
        return Err(RuntimeCommandError::recovery_required());
    }
    fs::read(path)
        .map(Some)
        .map_err(|_| RuntimeCommandError::recovery_required())
}

/// Projects either a valid marker or its matching transaction tombstone while
/// retaining only the identity needed for an explicit stale-checking ack.
fn recovery_projection(
    marker: Option<&dyn RecoveryMarkerLike>,
    acknowledgeable: bool,
) -> RuntimeRecoveryState {
    let (recovery_id, revision) = marker
        .map(|marker| {
            (
                Some(marker.recovery_id().to_owned()),
                Some(marker.revision()),
            )
        })
        .unwrap_or((None, None));
    RuntimeRecoveryState {
        required: true,
        acknowledgeable,
        recovery_id,
        revision,
    }
}

trait RecoveryMarkerLike {
    fn recovery_id(&self) -> &str;
    fn revision(&self) -> u64;
}

impl RecoveryMarkerLike for RecoveryMarker {
    fn recovery_id(&self) -> &str {
        &self.recovery_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

impl RecoveryMarkerLike for RecoveryAcknowledgement {
    fn recovery_id(&self) -> &str {
        &self.recovery_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

/// Records a typed acknowledgement in a same-directory tombstone, then
/// removes both tombstone and marker.  If deletion fails, the tombstone stays
/// available so the identical confirmation can be retried without guessing.
pub(super) fn acknowledge_manual_recovery(
    run_dir: impl AsRef<Path>,
    confirmation: &ManualRecoveryConfirmation,
) -> Result<(), RuntimeCommandError> {
    let run_dir = run_dir.as_ref();
    ensure_private_run_dir(run_dir).map_err(|_| RuntimeCommandError::recovery_required())?;
    let marker = read_recovery_marker(&recovery_marker_path(run_dir))?;
    let ack_path = run_dir.join(RECOVERY_ACK_FILE_NAME);
    let pending_ack = read_recovery_ack(&ack_path)?;
    let (current_id, current_revision) = marker
        .as_ref()
        .map(|value| (value.recovery_id.as_str(), value.revision))
        .or_else(|| {
            pending_ack
                .as_ref()
                .map(|value| (value.recovery_id.as_str(), value.revision))
        })
        .ok_or_else(RuntimeCommandError::recovery_required)?;
    if current_id != confirmation.recovery_id || current_revision != confirmation.revision {
        return Err(RuntimeCommandError::recovery_stale());
    }
    if let Some(pending_ack) = pending_ack.as_ref()
        && (pending_ack.recovery_id != confirmation.recovery_id
            || pending_ack.revision != confirmation.revision)
    {
        return Err(RuntimeCommandError::recovery_stale());
    }
    let acknowledgement = RecoveryAcknowledgement {
        schema_version: RECOVERY_SCHEMA_VERSION,
        status: RecoveryAcknowledgementStatus::ManualRecoveryAckPending,
        recovery_id: confirmation.recovery_id.clone(),
        revision: confirmation.revision,
        reason: confirmation.reason,
    };
    let bytes = serde_json::to_vec(&acknowledgement)
        .map_err(|_| RuntimeCommandError::recovery_required())?;
    atomic_write_file(&ack_path, &bytes).map_err(|_| RuntimeCommandError::recovery_required())?;
    remove_file_synced(&recovery_marker_path(run_dir))
        .map_err(|_| RuntimeCommandError::recovery_required())?;
    remove_file_synced(&ack_path).map_err(|_| RuntimeCommandError::recovery_required())?;
    Ok(())
}

/// Writes a sanitized pending marker with a fresh identity.  The operation
/// deliberately leaves a temp remnant on failure so startup remains blocked.
pub(super) fn persist_recovery_record(
    path: &Path,
    attempt_id: u64,
    generation: u64,
) -> io::Result<()> {
    let marker = RecoveryMarker {
        schema_version: RECOVERY_SCHEMA_VERSION,
        status: RecoveryMarkerStatus::ManualRecoveryRequired,
        recovery_id: Uuid::new_v4().to_string(),
        revision: attempt_id.max(generation).max(1),
        generation,
    };
    let bytes = serde_json::to_vec(&marker).map_err(io::Error::other)?;
    atomic_write_file(path, &bytes)
}

/// Removes a marker only after its owner has been confirmed gone; directory
/// metadata is synced before clean exit is reported.
pub(super) fn clear_recovery_record(path: &Path) -> io::Result<()> {
    remove_file_synced(path)
}

/// Confirms that the directory is a real directory under the trusted app-data
/// boundary; production callers pass the canonical Tauri app-data runtime dir.
fn ensure_private_run_dir(run_dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(run_dir)?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is not a private directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime directory is not user-only",
            ));
        }
    }
    Ok(())
}

/// Creates, flushes and atomically replaces a same-directory file.  A unique
/// create-new temp name makes partial writes observable rather than silently
/// overwriting a valid recovery record.
fn atomic_write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "recovery path has no parent")
    })?;
    ensure_private_run_dir(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid recovery filename"))?;
    let temp = create_recovery_temp(parent, file_name)?;
    let result = (|| {
        let mut file = open_private_file(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        atomic_replace(&temp, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        tracing::error!("durable recovery marker write failed; startup remains fail-closed");
    }
    result
}

/// Allocates a bounded create-new temp path without embedding process data in
/// the persisted marker or relying on an unbounded retry loop.
fn create_recovery_temp(parent: &Path, file_name: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let id = NEXT_RECOVERY_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{file_name}.tmp-{id}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                drop(file);
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "recovery temp name space exhausted",
    ))
}

/// Applies a private file mode on Unix and leaves inherited app-private ACLs
/// untouched on Windows; the parent boundary is validated before this call.
fn open_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Performs an atomic replace on every supported platform.  Windows uses the
/// native replace-and-write-through primitive because `rename` cannot replace
/// an existing file atomically there.
fn atomic_replace(temp: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source: Vec<u16> = temp.as_os_str().encode_wide().chain([0]).collect();
        let destination: Vec<u16> = target.as_os_str().encode_wide().chain([0]).collect();
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }
        // SAFETY: both vectors are NUL-terminated and live across the call;
        // the OS performs the replacement without retaining either pointer.
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, target)
    }
}

/// Makes the directory entry durable after a marker replace or deletion.
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let encoded: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const OPEN_EXISTING: u32 = 3;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
        const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1_isize as *mut std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            #[link_name = "CreateFileW"]
            fn recovery_create_file(
                file_name: *const u16,
                desired_access: u32,
                share_mode: u32,
                security_attributes: *const std::ffi::c_void,
                creation_disposition: u32,
                flags_and_attributes: u32,
                template_file: *mut std::ffi::c_void,
            ) -> *mut std::ffi::c_void;
            #[link_name = "FlushFileBuffers"]
            fn recovery_flush_file_buffers(handle: *mut std::ffi::c_void) -> i32;
            #[link_name = "CloseHandle"]
            fn recovery_close_handle(handle: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: the path buffer is NUL-terminated for the duration of each
        // OS call; the handle is closed on every branch before returning.
        let handle = unsafe {
            recovery_create_file(
                encoded.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let flushed = unsafe { recovery_flush_file_buffers(handle) };
        let flush_error = (flushed == 0).then(io::Error::last_os_error);
        let close_result = unsafe { recovery_close_handle(handle) };
        if let Some(error) = flush_error {
            // NTFS does not expose directory handles to `FlushFileBuffers`
            // on every supported Windows build. `MoveFileExW` with
            // WRITE_THROUGH above is the platform's durable replace barrier;
            // retain other failures so a real ACL problem remains visible.
            if !matches!(error.raw_os_error(), Some(1 | 5 | 6 | 87)) {
                return Err(error);
            }
        }
        if close_result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Deletes a marker and syncs its containing directory, tolerating the
/// already-clean state without turning idempotent shutdown into a fault.
fn remove_file_synced(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || is_reparse_point(&metadata)
                || !metadata.is_file()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "recovery marker is not a regular file",
                ));
            }
            fs::remove_file(path)?;
            sync_directory(path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "recovery path has no parent")
            })?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Launch policy comes only from the trusted composition root, never from a
/// WebView.  Tests use `for_test`; production uses `bundled_launch_config`.
#[derive(Clone)]
pub struct LaunchConfig {
    pub(super) sidecar: SidecarConfig,
    pub(super) request_timeout: Duration,
    pub(super) shutdown_timeout: Duration,
}

impl LaunchConfig {
    /// Keeps every bridge wait bounded so a command cannot retain a child
    /// process or actor worker indefinitely.
    pub fn validate(&self) -> Result<(), RuntimeCommandError> {
        if self.request_timeout.is_zero()
            || self.request_timeout > Duration::from_secs(600)
            || self.shutdown_timeout.is_zero()
            || self.shutdown_timeout > Duration::from_secs(120)
        {
            return Err(RuntimeCommandError::configuration());
        }
        Ok(())
    }

    /// Provides the only trusted path for clearing a startup recovery gate;
    /// callers must supply an explicit finite confirmation before the marker
    /// can be removed and a later bridge may launch.
    pub fn acknowledge_manual_recovery(
        &self,
        confirmation: &ManualRecoveryConfirmation,
    ) -> Result<(), RuntimeCommandError> {
        acknowledge_manual_recovery(&self.sidecar.run_dir, confirmation)
    }

    /// Allows only integration tests to inject an explicit process and args;
    /// this constructor is not used by the packaged Tauri composition root.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(executable: PathBuf, args: Vec<OsString>, run_dir: PathBuf) -> Self {
        let mut sidecar = bounded_sidecar(executable, run_dir);
        sidecar.args = args;
        Self {
            sidecar,
            request_timeout: TURN_DEADLINE,
            shutdown_timeout: SHUTDOWN_DEADLINE,
        }
    }
}

/// Builds the only production launch shape: a fixed native resource under
/// Tauri's trusted resource directory and a fixed app-data working directory.
/// The resource is never searched through PATH or JAVA_HOME.
pub fn bundled_launch_config(
    resource_dir: impl AsRef<Path>,
    run_dir: impl Into<PathBuf>,
) -> Result<LaunchConfig, RuntimeCommandError> {
    let resource_root = fs::canonicalize(resource_dir.as_ref())
        .map_err(|_| RuntimeCommandError::configuration())?;
    if !resource_root.is_dir() {
        return Err(RuntimeCommandError::configuration());
    }
    let staged = resource_root.join(sidecar_resource_name());
    validate_resource_components(&resource_root, &staged)?;
    let executable = fs::canonicalize(&staged).map_err(|_| RuntimeCommandError::configuration())?;
    let expected = resource_root.join(sidecar_resource_name());
    if executable != expected || !executable.starts_with(&resource_root) || !executable.is_file() {
        return Err(RuntimeCommandError::configuration());
    }
    Ok(LaunchConfig {
        sidecar: bounded_sidecar(executable, run_dir.into()),
        request_timeout: TURN_DEADLINE,
        shutdown_timeout: SHUTDOWN_DEADLINE,
    })
}

/// Rejects symlink/reparse indirection on every staged path component so a
/// trusted resource root cannot be redirected between the parent directory
/// and the executable after only the final file has been checked.
fn validate_resource_components(
    resource_root: &Path,
    staged: &Path,
) -> Result<(), RuntimeCommandError> {
    let relative = staged
        .strip_prefix(resource_root)
        .map_err(|_| RuntimeCommandError::configuration())?;
    let mut component_path = resource_root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(RuntimeCommandError::configuration());
        }
        component_path.push(component);
        let metadata = fs::symlink_metadata(&component_path)
            .map_err(|_| RuntimeCommandError::configuration())?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(RuntimeCommandError::configuration());
        }
        if component_path != staged && !metadata.is_dir() {
            return Err(RuntimeCommandError::configuration());
        }
        if component_path == staged && !metadata.is_file() {
            return Err(RuntimeCommandError::configuration());
        }
    }
    Ok(())
}

/// Keeps the platform-specific reparse check local so the policy remains
/// identical on Unix while Windows also rejects junction-like indirection.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

/// Creates the trusted sidecar policy with short request/handshake bounds so
/// a priority shutdown cannot wait behind a long protocol operation.
fn bounded_sidecar(executable: PathBuf, run_dir: PathBuf) -> SidecarConfig {
    let mut sidecar = SidecarConfig::new(executable, run_dir);
    sidecar.ready_timeout = READY_DEADLINE;
    sidecar.shutdown_timeout = SHUTDOWN_DEADLINE;
    sidecar
}

/// Creates the app-data runtime directory and applies the narrowest native
/// permission available; Tauri's app-data root remains the Windows boundary.
pub fn prepare_run_dir(run_dir: impl AsRef<Path>) -> Result<PathBuf, RuntimeCommandError> {
    let run_dir = run_dir.as_ref();
    fs::create_dir_all(run_dir).map_err(|_| RuntimeCommandError::configuration())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(run_dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| RuntimeCommandError::configuration())?;
    }
    fs::canonicalize(run_dir).map_err(|_| RuntimeCommandError::configuration())
}

#[cfg(debug_assertions)]
impl LaunchConfig {
    /// Allows only a host-controlled debug environment to select Java25 and a
    /// local jar; WebView input never reaches this constructor.
    pub fn debug_java(
        java: PathBuf,
        jar: PathBuf,
        run_dir: PathBuf,
    ) -> Result<Self, RuntimeCommandError> {
        if !java.is_absolute() || !java.is_file() || !jar.is_absolute() || !jar.is_file() {
            return Err(RuntimeCommandError::configuration());
        }
        let mut sidecar = bounded_sidecar(java, run_dir);
        sidecar.args = vec![
            OsString::from("-jar"),
            jar.into_os_string(),
            OsString::from("--runtime=fake"),
        ];
        let config = Self {
            sidecar,
            request_timeout: TURN_DEADLINE,
            shutdown_timeout: SHUTDOWN_DEADLINE,
        };
        config.validate()?;
        Ok(config)
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-x86_64-pc-windows-msvc.exe"
}

#[cfg(all(windows, target_arch = "aarch64"))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-aarch64-pc-windows-msvc.exe"
}

#[cfg(all(windows, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-unsupported-target.exe"
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-x86_64-apple-darwin"
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-aarch64-apple-darwin"
}

#[cfg(all(not(windows), not(target_os = "macos"), target_arch = "x86_64"))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-x86_64-unknown-linux-gnu"
}

#[cfg(all(not(windows), not(target_os = "macos"), not(target_arch = "x86_64")))]
fn sidecar_resource_name() -> &'static str {
    "sidecars/ja-agent-unsupported-target"
}

/// Runtime state is only a projection; the supervisor remains authoritative
/// and no ready challenge or secret is represented here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub status: RuntimeStatusKind,
    pub generation: u64,
    pub server_instance_id: Option<String>,
}

/// Host statuses include Busy while wire status events keep the protocol's
/// smaller runtime status enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusKind {
    Starting,
    Ready,
    Busy,
    Stopping,
    Stopped,
    RecoveryRequired,
    Crashed,
    Incompatible,
    Faulted,
}

impl RuntimeStatusKind {
    /// Maps frozen lifecycle facts to a UI-safe finite status.
    pub(super) fn from_lifecycle(state: LifecycleState) -> Self {
        match state {
            LifecycleState::Starting => Self::Starting,
            LifecycleState::Ready => Self::Ready,
            LifecycleState::Busy => Self::Busy,
            LifecycleState::Stopping => Self::Stopping,
            LifecycleState::Exited | LifecycleState::Backoff => Self::Stopped,
            LifecycleState::Incompatible => Self::Incompatible,
            LifecycleState::Faulted => Self::Faulted,
        }
    }

    /// Converts host-only statuses to protocol statuses; Busy remains a
    /// snapshot detail and is not invented as an invalid wire enum.
    pub(super) fn protocol_name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready | Self::Busy => "ready",
            Self::Stopping => "shutting_down",
            Self::Stopped => "stopped",
            Self::RecoveryRequired => "recovery_required",
            Self::Crashed | Self::Incompatible | Self::Faulted => "crashed",
        }
    }
}

/// Typed input for the first fake turn path.  Denying unknown fields prevents
/// a future WebView from smuggling executable, environment, or shell settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnStartInput {
    pub thread_id: String,
    pub mode: String,
    pub permission_mode: String,
    pub profile_revision: String,
    pub input: Vec<TurnInputPart>,
}

/// A bounded user input part accepted by the sidecar fixture and future
/// protocol adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnInputPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<String>,
    pub attachment_id: Option<String>,
}

impl TurnStartInput {
    /// Converts typed input to frozen wire params after applying identity and
    /// size constraints before any process request is admitted.
    pub(super) fn into_params(self) -> Result<Value, RuntimeCommandError> {
        if !valid_text_id(&self.thread_id, 128)
            || !matches!(self.mode.as_str(), "plan" | "workspace" | "full_access")
            || self.permission_mode.is_empty()
            || self.permission_mode.len() > 64
            || !self.profile_revision.starts_with("profile_")
            || self.profile_revision.len() > 128
            || self.input.is_empty()
            || self.input.len() > 128
        {
            return Err(RuntimeCommandError::invalid_params());
        }
        let mut parts = Vec::with_capacity(self.input.len());
        for part in self.input {
            if part.kind == "text" {
                let text = part.text.ok_or_else(RuntimeCommandError::invalid_params)?;
                if text.is_empty() || text.len() > 1_048_576 {
                    return Err(RuntimeCommandError::invalid_params());
                }
                parts.push(json!({ "type": "text", "text": text }));
            } else if part.kind == "attachment" {
                let attachment_id = part
                    .attachment_id
                    .ok_or_else(RuntimeCommandError::invalid_params)?;
                if !valid_text_id(&attachment_id, 128) {
                    return Err(RuntimeCommandError::invalid_params());
                }
                parts.push(json!({ "type": "attachment", "attachmentId": attachment_id }));
            } else {
                return Err(RuntimeCommandError::invalid_params());
            }
        }
        Ok(json!({
            "threadId": self.thread_id,
            "mode": self.mode,
            "permissionMode": self.permission_mode,
            "profileRevision": self.profile_revision,
            "input": parts,
        }))
    }
}

/// Accepted response for `ja_turn_start`; events arrive on the fixed event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnAccepted {
    pub accepted: bool,
    pub turn_id: String,
    pub queued: bool,
    pub status: String,
}

/// Stable redacted command error.  Internal process errors remain in logs;
/// the WebView never receives a path, token, stack, or child command.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCommandError {
    pub code: &'static str,
    pub message: &'static str,
    pub retryable: bool,
}

impl std::fmt::Display for RuntimeCommandError {
    /// Keeps setup and invoke conversion stable without exposing diagnostics.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RuntimeCommandError {}

impl RuntimeCommandError {
    /// Creates one configuration error shape for trusted launch setup.
    pub(super) const fn configuration() -> Self {
        Self {
            code: "RUNTIME_CONFIG_INVALID",
            message: "runtime configuration is invalid",
            retryable: false,
        }
    }

    /// Keeps malformed command fields from reaching Java.
    pub(super) const fn invalid_params() -> Self {
        Self {
            code: "INVALID_PARAMS",
            message: "runtime request parameters are invalid",
            retryable: false,
        }
    }

    /// Represents queue/actor lifetime failure without diagnostics.
    pub(super) const fn unavailable() -> Self {
        Self {
            code: "RUNTIME_UNAVAILABLE",
            message: "runtime bridge is unavailable",
            retryable: true,
        }
    }

    /// Gives callers a stable backpressure result instead of blocking a Tauri
    /// command until the actor eventually drains its bounded queue.
    pub(super) const fn queue_full() -> Self {
        Self {
            code: "RUNTIME_QUEUE_FULL",
            message: "runtime bridge queue is full",
            retryable: true,
        }
    }

    /// Makes a failed native event delivery visible to the command caller and
    /// the lifecycle actor without forwarding the native error text.
    pub(super) const fn event_delivery() -> Self {
        Self {
            code: "RUNTIME_EVENT_DELIVERY_FAILED",
            message: "runtime event delivery failed",
            retryable: true,
        }
    }

    /// Makes a cleanup deadline visible so Tauri can prevent a requested exit
    /// while a worker still owns a live process or event-pump handle.
    pub(super) const fn shutdown_timeout() -> Self {
        Self {
            code: "RUNTIME_SHUTDOWN_TIMEOUT",
            message: "runtime shutdown timed out",
            retryable: true,
        }
    }

    /// Prevents a new sidecar from launching while a prior forced exit still
    /// requires explicit user-confirmed recovery and marker acknowledgement.
    pub(super) const fn recovery_required() -> Self {
        Self {
            code: "RECOVERY_REQUIRED",
            message: "manual runtime recovery is required",
            retryable: false,
        }
    }

    /// Rejects an acknowledgement captured from an older native projection;
    /// only the current recovery identity/revision may clear the gate.
    pub(super) const fn recovery_stale() -> Self {
        Self {
            code: "RECOVERY_STALE",
            message: "runtime recovery confirmation is stale",
            retryable: false,
        }
    }

    /// Maps a foundation error to stable desktop categories and retry policy.
    pub(super) fn from_process(error: &AgentProcessError) -> Self {
        match error {
            AgentProcessError::InvalidConfig => Self::configuration(),
            AgentProcessError::Incompatible => Self {
                code: "PROTOCOL_INCOMPATIBLE",
                message: "sidecar protocol is incompatible",
                retryable: false,
            },
            AgentProcessError::Faulted => Self {
                code: "RUNTIME_FAULTED",
                message: "runtime is faulted",
                retryable: false,
            },
            AgentProcessError::Backoff { .. } => Self {
                code: "RUNTIME_BACKOFF",
                message: "runtime restart is cooling down",
                retryable: true,
            },
            AgentProcessError::ShuttingDown => Self {
                code: "SHUTTING_DOWN",
                message: "runtime is shutting down",
                retryable: true,
            },
            AgentProcessError::NotReady | AgentProcessError::InvalidState => Self {
                code: "RUNTIME_NOT_READY",
                message: "runtime is not ready",
                retryable: true,
            },
            AgentProcessError::DeadlineExceeded | AgentProcessError::ShutdownTimeout => Self {
                code: "RUNTIME_TIMEOUT",
                message: "runtime operation timed out",
                retryable: true,
            },
            AgentProcessError::ProcessExited
            | AgentProcessError::ProcessTree
            | AgentProcessError::Spawn
            | AgentProcessError::SessionClosed => Self {
                code: "SIDECAR_CRASHED",
                message: "sidecar process is unavailable",
                retryable: true,
            },
            AgentProcessError::Codec(_)
            | AgentProcessError::HandshakeFailed
            | AgentProcessError::ProtocolFault
            | AgentProcessError::QueueFull(_)
            | AgentProcessError::QueueClosed(_)
            | AgentProcessError::PendingLimit
            | AgentProcessError::RequestLedgerExhausted
            | AgentProcessError::DuplicateRequest
            | AgentProcessError::UnknownRequestId
            | AgentProcessError::DuplicateResponse
            | AgentProcessError::LateResponse
            | AgentProcessError::Cancelled
            | AgentProcessError::InvalidTimeout
            | AgentProcessError::InvalidErrorCatalog
            | AgentProcessError::RestartLimitExceeded => Self {
                code: "RUNTIME_PROTOCOL_ERROR",
                message: "runtime protocol operation failed",
                retryable: false,
            },
        }
    }
}

/// Validates bounded IDs before they enter a sidecar request envelope.
fn valid_text_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates the test runtime boundary with the same private mode required
    /// by production marker operations.
    fn create_private_test_dir(path: &Path) {
        fs::create_dir_all(path).expect("test runtime directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("private test directory mode");
        }
    }

    /// Missing staged native resources must fail closed instead of allowing a
    /// release app to silently search PATH or JAVA_HOME.
    #[test]
    fn bundled_resource_requires_staged_sidecar() {
        let path = std::env::temp_dir().join(format!("ja-resource-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("resource test directory");
        let result = bundled_launch_config(&path, &path);
        let _ = std::fs::remove_dir_all(&path);
        assert!(matches!(
            result,
            Err(error) if error == RuntimeCommandError::configuration()
        ));
    }

    /// Unix runtime directories stay private even when the parent app-data
    /// location was created with broader inherited permissions.
    #[cfg(unix)]
    #[test]
    fn prepare_run_dir_sets_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("ja-private-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let prepared = prepare_run_dir(&path).expect("private run directory");
        let mode = std::fs::metadata(prepared)
            .expect("run directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_dir_all(&path);
        assert_eq!(mode, 0o700);
    }

    /// A staged symlink that resolves outside the resource root must not be
    /// accepted as the packaged executable, otherwise bundle replacement can
    /// redirect the sidecar to an untrusted path.
    #[cfg(unix)]
    #[test]
    fn bundled_resource_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("ja-resource-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("ja-resource-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(root.join("sidecars")).expect("resource root");
        std::fs::create_dir_all(&outside).expect("outside root");
        let target = root.join(sidecar_resource_name());
        let outside_file = outside.join("sidecar");
        std::fs::write(&outside_file, b"not executable").expect("outside file");
        symlink(&outside_file, &target).expect("resource symlink");
        let result = bundled_launch_config(&root, &root);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        assert!(matches!(
            result,
            Err(error) if error == RuntimeCommandError::configuration()
        ));
    }

    /// An intermediate directory link is rejected even when its final file
    /// name appears to remain under the canonical resource root.
    #[cfg(unix)]
    #[test]
    fn bundled_resource_rejects_intermediate_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "ja-resource-intermediate-root-{}",
            std::process::id()
        ));
        let outside = std::env::temp_dir().join(format!(
            "ja-resource-intermediate-outside-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).expect("resource root");
        std::fs::create_dir_all(&outside).expect("outside root");
        let outside_file = outside.join("ja-agent");
        std::fs::write(&outside_file, b"not executable").expect("outside file");
        symlink(&outside, root.join("sidecars")).expect("intermediate resource symlink");
        let result = bundled_launch_config(&root, &root);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        assert!(matches!(
            result,
            Err(error) if error == RuntimeCommandError::configuration()
        ));
    }

    /// Windows reparse-point replacement is rejected before canonical path
    /// resolution; environments without symlink privilege simply skip it.
    #[cfg(windows)]
    #[test]
    fn bundled_resource_rejects_windows_reparse_escape() {
        use std::os::windows::fs::symlink_file;

        let root = std::env::temp_dir().join(format!("ja-resource-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("ja-resource-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(root.join("sidecars")).expect("resource root");
        std::fs::create_dir_all(&outside).expect("outside root");
        let target = root.join(sidecar_resource_name());
        let outside_file = outside.join("sidecar");
        std::fs::write(&outside_file, b"not executable").expect("outside file");
        if symlink_file(&outside_file, &target).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let result = bundled_launch_config(&root, &root);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        assert!(matches!(
            result,
            Err(error) if error == RuntimeCommandError::configuration()
        ));
    }

    /// Windows directory reparse indirection is rejected before the staged
    /// executable can be canonicalized outside the bundle root.
    #[cfg(windows)]
    #[test]
    fn bundled_resource_rejects_windows_intermediate_reparse_escape() {
        use std::os::windows::fs::symlink_dir;

        let root = std::env::temp_dir().join(format!(
            "ja-resource-intermediate-root-{}",
            std::process::id()
        ));
        let outside = std::env::temp_dir().join(format!(
            "ja-resource-intermediate-outside-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).expect("resource root");
        std::fs::create_dir_all(&outside).expect("outside root");
        let outside_file = outside.join("ja-agent.exe");
        std::fs::write(&outside_file, b"not executable").expect("outside file");
        if symlink_dir(&outside, root.join("sidecars")).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            return;
        }
        let result = bundled_launch_config(&root, &root);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        assert!(matches!(
            result,
            Err(error) if error == RuntimeCommandError::configuration()
        ));
    }

    /// Recovery JSON must have one exact versioned shape; accepting a future,
    /// duplicate, unknown, or wrongly typed field could unlock an ambiguous
    /// process state after a crash.
    #[test]
    fn recovery_marker_rejects_ambiguous_schema() {
        let fixtures: &[&[u8]] = &[
            br#"{"schemaVersion":1,"status":"manual_recovery_required","attemptId":1,"generation":1,"extra":true}"#,
            br#"{"schemaVersion":1,"status":"manual_recovery_required","attemptId":1,"attemptId":2,"generation":1}"#,
            br#"{"schemaVersion":"1","status":"manual_recovery_required","attemptId":1,"generation":1}"#,
            br#"{"schemaVersion":2,"status":"manual_recovery_required","attemptId":1,"generation":1}"#,
        ];
        for fixture in fixtures {
            assert!(parse_recovery_marker(fixture).is_err());
        }
    }

    /// The durable write uses a private file and the read gate blocks startup
    /// while the marker is present, even when the marker itself is valid.
    #[test]
    fn recovery_marker_write_read_and_clear_are_durable() {
        let run_dir =
            std::env::temp_dir().join(format!("ja-recovery-durable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let marker = recovery_marker_path(&run_dir);
        persist_recovery_record(&marker, 3, 7).expect("durable marker");
        assert_eq!(
            ensure_recovery_clear(&run_dir)
                .expect_err("marker must block startup")
                .code,
            "RECOVERY_REQUIRED"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&marker)
                    .expect("marker metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        clear_recovery_record(&marker).expect("clear marker");
        assert!(ensure_recovery_clear(&run_dir).is_ok());
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// A temp file left by a failed replace is evidence of an incomplete
    /// durable write and therefore must fail closed on the next startup.
    #[test]
    fn recovery_temp_remnant_blocks_startup() {
        let run_dir = std::env::temp_dir().join(format!("ja-recovery-temp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let remnant = run_dir.join(format!("{RECOVERY_TEMP_PREFIX}power-loss"));
        fs::write(&remnant, b"partial").expect("temp remnant");
        assert_eq!(
            ensure_recovery_clear(&run_dir)
                .expect_err("temp remnant must block startup")
                .code,
            "RECOVERY_REQUIRED"
        );
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// Oversized marker input is rejected before JSON parsing so a corrupted
    /// disk record cannot consume unbounded memory during startup recovery.
    #[test]
    fn recovery_marker_oversize_blocks_startup() {
        let run_dir =
            std::env::temp_dir().join(format!("ja-recovery-oversize-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let marker = recovery_marker_path(&run_dir);
        fs::write(&marker, vec![b'{'; (MAX_RECOVERY_BYTES as usize) + 1]).expect("oversize marker");
        assert_eq!(
            ensure_recovery_clear(&run_dir)
                .expect_err("oversize marker must block startup")
                .code,
            "RECOVERY_REQUIRED"
        );
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// A malformed marker is still a recovery condition, never an invitation
    /// to silently launch a second sidecar over an unknown previous state.
    #[test]
    fn malformed_recovery_marker_blocks_startup() {
        let run_dir =
            std::env::temp_dir().join(format!("ja-recovery-malformed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        fs::write(recovery_marker_path(&run_dir), b"not-json").expect("malformed recovery marker");
        assert_eq!(
            ensure_recovery_clear(&run_dir)
                .expect_err("malformed marker must block startup")
                .code,
            "RECOVERY_REQUIRED"
        );
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// Explicit acknowledgement records the finite user confirmation before
    /// atomically deleting the pending marker; no sidecar owner is inspected.
    #[test]
    fn explicit_recovery_acknowledgement_unlocks_startup() {
        let run_dir = std::env::temp_dir().join(format!("ja-recovery-ack-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let marker = recovery_marker_path(&run_dir);
        persist_recovery_record(&marker, 4, 8).expect("recovery marker");
        acknowledge_manual_recovery(
            &run_dir,
            &ManualRecoveryConfirmation {
                recovery_id: parse_recovery_marker(&fs::read(&marker).expect("marker bytes"))
                    .expect("marker schema")
                    .recovery_id,
                revision: parse_recovery_marker(&fs::read(&marker).expect("marker bytes"))
                    .expect("marker schema")
                    .revision,
                reason: ManualRecoveryReason::ExternallyCleaned,
            },
        )
        .expect("explicit recovery acknowledgement");
        assert!(!marker.exists());
        assert!(!run_dir.join(RECOVERY_ACK_FILE_NAME).exists());
        assert!(ensure_recovery_clear(&run_dir).is_ok());
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// A valid power-loss acknowledgement tombstone remains a blocking,
    /// repeatable recovery state until the same typed confirmation consumes it.
    #[test]
    fn acknowledgement_tombstone_is_repeatable_and_removed() {
        let run_dir =
            std::env::temp_dir().join(format!("ja-recovery-tombstone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let id = "00000000-0000-4000-8000-000000000003";
        fs::write(
            run_dir.join(RECOVERY_ACK_FILE_NAME),
            format!(
                "{{\"schemaVersion\":2,\"status\":\"manual_recovery_ack_pending\",\"recoveryId\":\"{id}\",\"revision\":11,\"reason\":\"SystemRestarted\"}}"
            ),
        )
        .expect("ack tombstone");
        let state = recovery_state(&run_dir);
        assert!(state.required && state.acknowledgeable);
        acknowledge_manual_recovery(
            &run_dir,
            &ManualRecoveryConfirmation {
                recovery_id: id.to_owned(),
                revision: 11,
                reason: ManualRecoveryReason::SystemRestarted,
            },
        )
        .expect("repeatable tombstone acknowledgement");
        assert!(!run_dir.join(RECOVERY_ACK_FILE_NAME).exists());
        assert!(!recovery_state(&run_dir).required);
        let _ = fs::remove_dir_all(&run_dir);
    }

    /// A stale UI acknowledgement must not consume the current marker or its
    /// identity, which keeps a reload from clearing a newer recovery attempt.
    #[test]
    fn stale_recovery_confirmation_is_rejected() {
        let run_dir =
            std::env::temp_dir().join(format!("ja-recovery-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&run_dir);
        create_private_test_dir(&run_dir);
        let marker = recovery_marker_path(&run_dir);
        persist_recovery_record(&marker, 5, 12).expect("recovery marker");
        let error = acknowledge_manual_recovery(
            &run_dir,
            &ManualRecoveryConfirmation {
                recovery_id: "00000000-0000-4000-8000-000000000004".to_owned(),
                revision: 12,
                reason: ManualRecoveryReason::ExternallyCleaned,
            },
        )
        .expect_err("stale identity must fail closed");
        assert_eq!(error.code, "RECOVERY_STALE");
        assert!(marker.exists());
        let _ = fs::remove_dir_all(&run_dir);
    }
}
