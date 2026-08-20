// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri-managed lazy runtime composition.

use super::bridge::RuntimeBridge;
use super::config::{
    ApprovalResponseInput, EventSink, LaunchConfig, ManualRecoveryConfirmation,
    RuntimeCommandError, RuntimeConfigurationStatus, RuntimeConfigureInput, RuntimeRecoveryState,
    RuntimeReplayConfig, RuntimeStatus, RuntimeStatusKind, TurnAccepted, TurnCancelInput,
    TurnCancelResult, TurnStartInput, recovery_state,
};
use super::history::HistoryMethod;
use super::settings_query::SettingsQueryMethod;
use crate::workspace_read::{WorkspaceHandle, WorkspaceRegistry};
use serde_json::Value;
#[cfg(feature = "tauri-smoke")]
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
#[cfg(feature = "tauri-smoke")]
use std::time::Duration;
use std::time::Instant;

/// Owns the trusted launch configuration and lazily creates exactly one bridge
/// after the recovery gate is clear; setup can therefore render recovery UI
/// without starting a new sidecar over an unknown prior process.
#[derive(Clone)]
pub struct RuntimeHost {
    config: Arc<Mutex<LaunchConfig>>,
    sink: EventSink,
    bridge: Arc<Mutex<Option<RuntimeBridge>>>,
    workspace: Arc<Mutex<Option<ConfiguredWorkspace>>>,
    #[cfg(feature = "tauri-smoke")]
    exit_timeout: Option<Duration>,
    #[cfg(feature = "tauri-smoke")]
    shutdown_failure_injector: Option<Arc<AtomicUsize>>,
}

/// Binds the protocol workspace id to the exact canonical handle admitted by
/// the runtime host; the internal registry UUID never crosses this boundary.
struct ConfiguredWorkspace {
    protocol_id: String,
    handle: WorkspaceHandle,
}

/// Describes why a typed workspace command cannot be admitted without
/// exposing a root path or allowing a caller to select an arbitrary handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceLookup {
    Unconfigured,
    Unknown,
}

impl RuntimeHost {
    /// Creates managed state without touching process I/O or bypassing a
    /// recovery marker; the first typed runtime command performs lazy start.
    pub fn new(config: LaunchConfig, sink: EventSink) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
            sink,
            bridge: Arc::new(Mutex::new(None)),
            workspace: Arc::new(Mutex::new(None)),
            #[cfg(feature = "tauri-smoke")]
            exit_timeout: None,
            #[cfg(feature = "tauri-smoke")]
            shutdown_failure_injector: None,
        }
    }

    /// Builds a feature-gated host with a private exit seam for MockRuntime
    /// lifecycle tests; production composition has no injectable state.
    #[cfg(feature = "tauri-smoke")]
    pub fn new_for_exit_test(
        config: LaunchConfig,
        sink: EventSink,
        timeout: Duration,
        shutdown_failure_injector: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
            sink,
            bridge: Arc::new(Mutex::new(None)),
            workspace: Arc::new(Mutex::new(None)),
            exit_timeout: Some(timeout),
            shutdown_failure_injector: Some(shutdown_failure_injector),
        }
    }

    /// Returns a bridge only after a fresh native recovery-state check; the
    /// mutex covers construction, never slow sidecar I/O or command waiting.
    fn ensure_bridge(&self) -> Result<RuntimeBridge, RuntimeCommandError> {
        let mut bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = bridge.as_ref() {
            return Ok(current.clone());
        }
        let config = self.config_snapshot();
        if recovery_state(&config.sidecar.run_dir).required {
            return Err(RuntimeCommandError::recovery_required());
        }
        #[cfg(feature = "tauri-smoke")]
        let current = match (self.exit_timeout, self.shutdown_failure_injector.clone()) {
            (Some(timeout), Some(injector)) => RuntimeBridge::new_for_exit_test_with_injector(
                config.clone(),
                self.sink.clone(),
                timeout,
                injector,
            )?,
            (Some(timeout), None) => {
                RuntimeBridge::new_for_exit_test(config.clone(), self.sink.clone(), timeout)?
            }
            _ => RuntimeBridge::new(config.clone(), self.sink.clone())?,
        };
        #[cfg(not(feature = "tauri-smoke"))]
        let current = RuntimeBridge::new(config, self.sink.clone())?;
        *bridge = Some(current.clone());
        Ok(current)
    }

    /// Starts the lazy bridge through the one native owner used by all typed
    /// commands; no WebView field can select an executable or environment.
    pub fn start(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        let result = self.ensure_bridge().and_then(|bridge| bridge.start());
        // A failed start leaves the sidecar non-Ready (or absent), so the
        // previous binding must not become a capability to read a stale root
        // after the caller retries configuration or recovery.
        if result
            .as_ref()
            .map(|status| status.status != RuntimeStatusKind::Ready)
            .unwrap_or(true)
        {
            self.clear_workspace();
        }
        result
    }

    /// Stops an existing bridge, while preserving a recovery-required result
    /// when no owner may be created over an unresolved marker.
    pub fn stop(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let result = match bridge {
            Some(bridge) => bridge.stop(),
            None if recovery_state(&self.run_dir()).required => {
                Err(RuntimeCommandError::recovery_required())
            }
            None => Ok(RuntimeStatus {
                status: RuntimeStatusKind::Stopped,
                generation: 0,
                server_instance_id: None,
            }),
        };
        // A stop is a lifecycle boundary even when cleanup reports an error;
        // retaining the old handle would let a file command outlive the
        // generation that owned its admission.
        self.clear_workspace();
        result
    }

    /// Returns bridge state when present, otherwise exposes a minimal
    /// token-free RecoveryRequired/Stopped projection for the UI shell.
    pub fn state(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match bridge {
            Some(bridge) => bridge.state(),
            None if recovery_state(&self.run_dir()).required => Ok(RuntimeStatus {
                status: RuntimeStatusKind::RecoveryRequired,
                generation: 0,
                server_instance_id: None,
            }),
            None => Ok(RuntimeStatus {
                status: RuntimeStatusKind::Stopped,
                generation: 0,
                server_instance_id: None,
            }),
        }
    }

    /// Routes a typed turn only through a bridge that has passed recovery and
    /// trusted configuration checks.
    pub fn turn_start(&self, input: TurnStartInput) -> Result<TurnAccepted, RuntimeCommandError> {
        self.ensure_bridge()?.turn_start(input)
    }

    /// Routes cancellation through the current bridge without replacing or
    /// stopping the sidecar; completion still arrives on the event channel.
    pub fn turn_cancel(
        &self,
        input: TurnCancelInput,
    ) -> Result<TurnCancelResult, RuntimeCommandError> {
        self.ensure_bridge()?.turn_cancel(input)
    }

    /// Routes one allow-listed history request only through a configured
    /// Ready generation, so queries cannot start or reconfigure a sidecar.
    pub(crate) fn history_request(
        &self,
        method: HistoryMethod,
        params: Value,
    ) -> Result<Value, RuntimeCommandError> {
        if self.config_snapshot().replay.is_none() {
            return Err(RuntimeCommandError::configuration());
        }
        // History is a query surface, not a lifecycle trigger: requiring an
        // existing bridge prevents a pre-start command from constructing a
        // fresh actor that has not yet been admitted by `ja_runtime_start`.
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(RuntimeCommandError {
                code: "RUNTIME_NOT_READY",
                message: "runtime is not ready",
                retryable: true,
            })?;
        let status = bridge.state()?;
        if status.status != RuntimeStatusKind::Ready {
            return Err(RuntimeCommandError {
                code: "RUNTIME_NOT_READY",
                message: "runtime is not ready",
                retryable: true,
            });
        }
        bridge.history_request(method, params)
    }

    /// Routes the fixed Skills/MCP settings queries through the current Ready
    /// generation; unlike configuration this command never starts a sidecar.
    pub(crate) fn settings_query(
        &self,
        method: SettingsQueryMethod,
        params: Value,
    ) -> Result<Value, RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(RuntimeCommandError {
                code: "RUNTIME_NOT_READY",
                message: "runtime is not ready",
                retryable: true,
            })?;
        let status = bridge.state()?;
        if status.status != RuntimeStatusKind::Ready {
            return Err(RuntimeCommandError {
                code: "RUNTIME_NOT_READY",
                message: "runtime is not ready",
                retryable: true,
            });
        }
        bridge.settings_query(method, params)
    }

    /// Freezes a validated workspace/profile snapshot and cleanly replaces a
    /// previous generation before storing it; this keeps settings replay out
    /// of a live AgentScope graph and makes every restart use one snapshot.
    pub fn configure(
        &self,
        input: RuntimeConfigureInput,
    ) -> Result<RuntimeConfigurationStatus, RuntimeCommandError> {
        // Clear before validation/admission so every failed reconfiguration
        // leaves no stale workspace binding behind.
        self.clear_workspace();
        // Admit the canonical root through the existing registry so every
        // read/Git command shares one physical identity and no command can
        // substitute its own absolute path.
        let registry = WorkspaceRegistry::default();
        let workspace_info = registry
            .register(&input.root_path)
            .map_err(|_| RuntimeCommandError::configuration())?;
        let workspace_handle = registry
            .get(workspace_info.id)
            .map_err(|_| RuntimeCommandError::configuration())?;
        // RuntimeReplayConfig performs the remaining settings/profile checks;
        // raw link admission above intentionally precedes its canonicalize.
        let replay = RuntimeReplayConfig::from_input(input)?;
        let workspace_id = replay.workspace_id.clone();
        let status = RuntimeConfigurationStatus {
            configured: true,
            profile_revision: replay.profile.profile_revision.clone(),
            mcp_count: replay.mcp_servers.len(),
        };
        let existing = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(bridge) = existing {
            bridge.shutdown()?;
            let mut slot = self
                .bridge
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.as_ref().is_some_and(|current| current.exit_ready()) {
                *slot = None;
            } else {
                return Err(RuntimeCommandError::shutdown_timeout());
            }
        }
        self.config_snapshot_mut().replay = Some(replay);
        *self
            .workspace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ConfiguredWorkspace {
            protocol_id: workspace_id,
            handle: workspace_handle,
        });
        Ok(status)
    }

    /// Runs one read-only operation while holding the binding lock, so a
    /// configuration switch cannot leave a previously selected handle usable
    /// after the switch linearizes.
    pub(crate) fn with_configured_workspace<T>(
        &self,
        workspace_id: &str,
        operation: impl FnOnce(&WorkspaceHandle) -> T,
    ) -> Result<T, WorkspaceLookup> {
        let workspace = self
            .workspace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(binding) = workspace.as_ref() else {
            return Err(WorkspaceLookup::Unconfigured);
        };
        // Configuration alone is not an authorization boundary: a binding is
        // usable only after the same sidecar generation reaches Ready. The
        // lifecycle owner clears it on configure/stop/start failure; a read
        // attempt merely fails closed so a later valid reconfiguration can
        // still replace the frozen snapshot deterministically.
        if !self.workspace_access_ready() {
            return Err(WorkspaceLookup::Unknown);
        }
        if binding.protocol_id != workspace_id {
            return Err(WorkspaceLookup::Unknown);
        }
        Ok(operation(&binding.handle))
    }

    /// Returns whether the current lifecycle still owns a read-capable
    /// workspace. Stop, shutdown, failed start and crashed generations all
    /// fail closed while the lazy post-configure state remains available.
    fn workspace_access_ready(&self) -> bool {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match bridge {
            // A successful configure freezes a binding but does not start a
            // generation; commands stay closed until an authoritative Ready
            // projection exists.
            None => false,
            Some(bridge) => bridge
                .state()
                .map(|status| status.status == RuntimeStatusKind::Ready)
                .unwrap_or(false),
        }
    }

    /// Removes the protocol-to-handle binding at every lifecycle boundary;
    /// this is deliberately the only mutation path for the active binding.
    fn clear_workspace(&self) {
        *self
            .workspace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Routes a user approval only to the currently owned sidecar session;
    /// unlike generic RPC, this command cannot target an arbitrary method or
    /// response ID and therefore keeps the WebView projection typed.
    pub fn approval_respond(
        &self,
        input: ApprovalResponseInput,
    ) -> Result<(), RuntimeCommandError> {
        input.validate()?;
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(RuntimeCommandError::unavailable)?;
        bridge.approval_respond(input)
    }

    /// Exposes only the sanitized recovery identity needed to form a typed
    /// acknowledgement; marker paths and process details stay native.
    pub fn recovery_state(&self) -> RuntimeRecoveryState {
        recovery_state(&self.run_dir())
    }

    /// Atomically acknowledges the current marker/tombstone, then leaves the
    /// bridge lazy so the next start creates a fresh owner after the durable
    /// gate has actually disappeared.
    pub fn acknowledge_recovery(
        &self,
        confirmation: &ManualRecoveryConfirmation,
    ) -> Result<RuntimeRecoveryState, RuntimeCommandError> {
        if self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return Err(RuntimeCommandError::unavailable());
        }
        self.config_snapshot()
            .acknowledge_manual_recovery(confirmation)?;
        Ok(self.recovery_state())
    }

    /// Performs bounded actor/process cleanup when Tauri requests application
    /// exit; an unresolved recovery marker has no live owner to stop.
    pub fn shutdown(&self) -> Result<(), RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let result = match bridge {
            Some(bridge) => bridge.shutdown(),
            None => Ok(()),
        };
        // Shutdown invalidates every old workspace handle even when native
        // cleanup needs recovery; a later start must be preceded by configure.
        self.clear_workspace();
        result
    }

    /// Performs the same host cleanup under one caller-owned absolute
    /// deadline, so time spent closing other native resources is not followed
    /// by a fresh bridge timeout.
    pub fn shutdown_until(&self, deadline: Instant) -> Result<(), RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let result = match bridge {
            Some(bridge) => bridge.shutdown_until(deadline),
            None => Ok(()),
        };
        // Shutdown invalidates every old workspace handle even when native
        // cleanup needs recovery; a later start must be preceded by configure.
        self.clear_workspace();
        result
    }

    /// Reports whether a final Tauri Exit is safe for the currently managed
    /// owner; a host with no bridge is already free of child process owners.
    pub fn exit_ready(&self) -> bool {
        self.bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_none_or(RuntimeBridge::exit_ready)
    }

    /// Persists the native diagnostic marker only when a live bridge could not
    /// be cleaned before a forced platform exit.
    pub fn record_forced_exit(&self) {
        if let Some(bridge) = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            bridge.record_forced_exit();
        }
    }

    /// Copies the immutable launch policy for a bridge constructor without
    /// holding the settings mutex across actor creation or process I/O.
    fn config_snapshot(&self) -> LaunchConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns a mutable launch snapshot only for the validated replay update
    /// performed after any previous bridge has completed shutdown.
    fn config_snapshot_mut(&self) -> std::sync::MutexGuard<'_, LaunchConfig> {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads the trusted run directory while keeping the host's configuration
    /// lock outside recovery-file operations.
    fn run_dir(&self) -> std::path::PathBuf {
        self.config_snapshot().sidecar.run_dir
    }
}
