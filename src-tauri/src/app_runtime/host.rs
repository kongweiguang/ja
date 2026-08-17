// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri-managed lazy runtime composition.

use super::bridge::RuntimeBridge;
use super::config::{
    EventSink, LaunchConfig, ManualRecoveryConfirmation, RuntimeCommandError, RuntimeRecoveryState,
    RuntimeStatus, RuntimeStatusKind, TurnAccepted, TurnStartInput, recovery_state,
};
#[cfg(feature = "tauri-smoke")]
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
#[cfg(feature = "tauri-smoke")]
use std::time::Duration;

/// Owns the trusted launch configuration and lazily creates exactly one bridge
/// after the recovery gate is clear; setup can therefore render recovery UI
/// without starting a new sidecar over an unknown prior process.
#[derive(Clone)]
pub struct RuntimeHost {
    config: Arc<LaunchConfig>,
    sink: EventSink,
    bridge: Arc<Mutex<Option<RuntimeBridge>>>,
    #[cfg(feature = "tauri-smoke")]
    exit_timeout: Option<Duration>,
    #[cfg(feature = "tauri-smoke")]
    shutdown_failure_injector: Option<Arc<AtomicUsize>>,
}

impl RuntimeHost {
    /// Creates managed state without touching process I/O or bypassing a
    /// recovery marker; the first typed runtime command performs lazy start.
    pub fn new(config: LaunchConfig, sink: EventSink) -> Self {
        Self {
            config: Arc::new(config),
            sink,
            bridge: Arc::new(Mutex::new(None)),
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
            config: Arc::new(config),
            sink,
            bridge: Arc::new(Mutex::new(None)),
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
        if recovery_state(&self.config.sidecar.run_dir).required {
            return Err(RuntimeCommandError::recovery_required());
        }
        #[cfg(feature = "tauri-smoke")]
        let current = match (self.exit_timeout, self.shutdown_failure_injector.clone()) {
            (Some(timeout), Some(injector)) => RuntimeBridge::new_for_exit_test_with_injector(
                (*self.config).clone(),
                self.sink.clone(),
                timeout,
                injector,
            )?,
            (Some(timeout), None) => RuntimeBridge::new_for_exit_test(
                (*self.config).clone(),
                self.sink.clone(),
                timeout,
            )?,
            _ => RuntimeBridge::new((*self.config).clone(), self.sink.clone())?,
        };
        #[cfg(not(feature = "tauri-smoke"))]
        let current = RuntimeBridge::new((*self.config).clone(), self.sink.clone())?;
        *bridge = Some(current.clone());
        Ok(current)
    }

    /// Starts the lazy bridge through the one native owner used by all typed
    /// commands; no WebView field can select an executable or environment.
    pub fn start(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        self.ensure_bridge()?.start()
    }

    /// Stops an existing bridge, while preserving a recovery-required result
    /// when no owner may be created over an unresolved marker.
    pub fn stop(&self) -> Result<RuntimeStatus, RuntimeCommandError> {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match bridge {
            Some(bridge) => bridge.stop(),
            None if recovery_state(&self.config.sidecar.run_dir).required => {
                Err(RuntimeCommandError::recovery_required())
            }
            None => Ok(RuntimeStatus {
                status: RuntimeStatusKind::Stopped,
                generation: 0,
                server_instance_id: None,
            }),
        }
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
            None if recovery_state(&self.config.sidecar.run_dir).required => Ok(RuntimeStatus {
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

    /// Exposes only the sanitized recovery identity needed to form a typed
    /// acknowledgement; marker paths and process details stay native.
    pub fn recovery_state(&self) -> RuntimeRecoveryState {
        recovery_state(&self.config.sidecar.run_dir)
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
        self.config.acknowledge_manual_recovery(confirmation)?;
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
        match bridge {
            Some(bridge) => bridge.shutdown(),
            None => Ok(()),
        }
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
}
