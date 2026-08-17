// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri command/state adapter for the PTY supervisor.
//!
//! The adapter owns only the configured workspace supervisor; PTY creation,
//! bounded queues and process-tree cleanup remain in the existing terminal
//! module so commands cannot grow a second terminal implementation.

use super::error::{TerminalError, TerminalErrorCode};
use super::model::{
    CloseReason, LaunchRequest, ShellProfile, TerminalEvent, TerminalId, TerminalSize,
};
use super::policy::TerminalPolicy;
use super::session::TerminalSupervisor;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Managed terminal state; `None` means the UI has not selected a workspace.
#[derive(Clone, Default)]
pub struct TerminalCommandHost {
    supervisor: Arc<Mutex<Option<TerminalSupervisor>>>,
}

impl TerminalCommandHost {
    /// Creates an unconfigured host so startup does not guess a workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures one canonical workspace and refuses to replace live PTYs.
    pub fn configure(&self, workspace: PathBuf) -> Result<(), TerminalError> {
        let policy = TerminalPolicy::new(workspace)?;
        let mut slot = self
            .supervisor
            .lock()
            .map_err(|_| TerminalError::new(TerminalErrorCode::InvalidConfig))?;
        if slot.as_ref().is_some_and(|value| value.active_count() != 0) {
            return Err(TerminalError::new(TerminalErrorCode::InvalidConfig));
        }
        *slot = Some(TerminalSupervisor::new(policy));
        Ok(())
    }

    /// Closes all PTYs against one shared deadline during application exit.
    pub fn shutdown_until(&self, deadline: Instant) -> Result<(), TerminalError> {
        let supervisor = self
            .supervisor
            .lock()
            .map_err(|_| TerminalError::new(TerminalErrorCode::InvalidConfig))?
            .clone();
        supervisor.map_or(Ok(()), |value| value.shutdown_until(deadline))
    }

    /// Returns whether no terminal process remains owned by this host.
    pub fn is_empty(&self) -> bool {
        match self.supervisor.lock() {
            Ok(slot) => slot
                .as_ref()
                .is_none_or(|supervisor| supervisor.active_count() == 0),
            Err(_) => false,
        }
    }

    /// Executes a command against the configured supervisor without exposing
    /// its internal mutex or process handles to the Tauri layer.
    fn with_supervisor<T>(
        &self,
        operation: impl FnOnce(&TerminalSupervisor) -> Result<T, TerminalError>,
    ) -> Result<T, TerminalError> {
        let supervisor = self
            .supervisor
            .lock()
            .map_err(|_| TerminalError::new(TerminalErrorCode::InvalidConfig))?
            .clone()
            .ok_or(TerminalError::new(TerminalErrorCode::InvalidConfig))?;
        operation(&supervisor)
    }
}

/// Selects the workspace before the first terminal is opened.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalConfigureInput {
    pub workspace: PathBuf,
}

/// Opens a terminal using a shell profile, bounded environment and viewport.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalOpenInput {
    #[serde(default)]
    pub profile: ShellProfile,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub size: TerminalSize,
}

/// Identifies one open terminal without exposing native handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionInfo {
    pub session_id: TerminalId,
    pub generation: u64,
}

/// Addresses a terminal owned by the configured supervisor.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalInput {
    pub session_id: TerminalId,
    pub generation: u64,
    pub data: Vec<u8>,
}

/// Polls at most one bounded event and never waits beyond five seconds.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalPollInput {
    pub session_id: TerminalId,
    pub generation: u64,
    #[serde(default)]
    pub timeout_ms: u64,
}

/// Stable command-facing error for shutdown paths that do not have a UI DTO.
pub type TerminalShutdownError = TerminalError;

/// Configures the native workspace boundary before terminal use.
#[tauri::command]
pub fn ja_terminal_configure(
    input: TerminalConfigureInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<(), TerminalError> {
    state.configure(input.workspace)
}

/// Opens one portable-pty session and returns its opaque identity.
#[tauri::command]
pub fn ja_terminal_open(
    input: TerminalOpenInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<TerminalSessionInfo, TerminalError> {
    let request = LaunchRequest {
        profile: input.profile,
        cwd: input.cwd,
        env: input.env,
        size: input.size,
    };
    state.with_supervisor(|supervisor| {
        let handle = supervisor.open(request)?;
        Ok(TerminalSessionInfo {
            session_id: handle.id(),
            generation: handle.generation(),
        })
    })
}

/// Sends bounded raw bytes through the session's single writer queue.
#[tauri::command]
pub fn ja_terminal_input(
    input: TerminalInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<(), TerminalError> {
    state.with_supervisor(|supervisor| {
        supervisor
            .get(input.session_id, input.generation)?
            .send_input(&input.data, Duration::from_secs(5))
    })
}

/// Resizes a PTY using the same latest-value queue used by the session workers.
#[tauri::command]
pub fn ja_terminal_resize(
    input: TerminalResizeInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<(), TerminalError> {
    state.with_supervisor(|supervisor| {
        supervisor
            .get(input.session_id, input.generation)?
            .resize(input.size)
    })
}

/// Polls one output/control event without turning Tauri's command queue into
/// an unbounded stream; the frontend repeats this call while the tab is open.
#[tauri::command]
pub fn ja_terminal_poll(
    input: TerminalPollInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<Option<TerminalEvent>, TerminalError> {
    let timeout = Duration::from_millis(input.timeout_ms.min(5_000));
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(TerminalError::new(TerminalErrorCode::DeadlineExceeded))?;
    state.with_supervisor(|supervisor| {
        supervisor
            .get(input.session_id, input.generation)?
            .recv_until(deadline)
    })
}

/// Returns bounded scrollback for reload/reconnect without decoding PTY bytes.
#[tauri::command]
pub fn ja_terminal_scrollback(
    input: TerminalPollInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<Vec<u8>, TerminalError> {
    state.with_supervisor(|supervisor| {
        supervisor
            .get(input.session_id, input.generation)
            .and_then(|handle| handle.scrollback())
    })
}

/// Closes one session and removes it from the supervisor owner map.
#[tauri::command]
pub fn ja_terminal_close(
    input: TerminalCloseInput,
    state: tauri::State<'_, TerminalCommandHost>,
) -> Result<(), TerminalError> {
    state.with_supervisor(|supervisor| {
        supervisor.close(input.session_id, input.generation, CloseReason::User)
    })
}

/// Addresses a PTY for a viewport resize operation.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalResizeInput {
    pub session_id: TerminalId,
    pub generation: u64,
    pub size: TerminalSize,
}

/// Addresses a PTY close operation without accepting an arbitrary reason.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalCloseInput {
    pub session_id: TerminalId,
    pub generation: u64,
}
