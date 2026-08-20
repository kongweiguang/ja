// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri composition surface for the Java sidecar bridge.

use std::time::Instant;

mod bridge;
mod config;
pub(crate) mod history;
mod host;
mod projection;
pub(crate) mod settings_query;

#[cfg(feature = "test-support")]
pub use bridge::BridgeTestTrace;
pub use bridge::RuntimeBridge;
pub use config::{
    ApprovalResponseInput, EventEmitError, EventSink, LaunchConfig, ManualRecoveryConfirmation,
    ManualRecoveryReason, RuntimeCommandError, RuntimeConfigurationStatus, RuntimeConfigureInput,
    RuntimeRecoveryState, RuntimeStatus, RuntimeStatusKind, TurnAccepted, TurnCancelInput,
    TurnCancelResult, TurnInputPart, TurnStartInput, bundled_launch_config, prepare_run_dir,
};
pub use host::RuntimeHost;
pub(crate) use host::WorkspaceLookup;
pub use projection::RPC_FRAME_EVENT;

/// Registers each sidecar command once at the native composition root.
pub fn register_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        ja_runtime_start,
        ja_runtime_stop,
        ja_runtime_state,
        ja_runtime_recovery_state,
        ja_runtime_acknowledge_recovery,
        ja_runtime_configure,
        settings_query::ja_runtime_query,
        ja_approval_respond,
        ja_turn_start,
        ja_turn_cancel,
        history::ja_workspace_list,
        history::ja_thread_create,
        history::ja_thread_list,
        history::ja_thread_read,
        crate::workspace_read::command::ja_workspace_tree,
        crate::workspace_read::command::ja_workspace_read_file,
        crate::workspace_read::command::ja_workspace_search,
        crate::git_read::commands::ja_git_status,
        crate::git_read::commands::ja_git_diff
    ])
}

/// Starts the trusted packaged sidecar and returns a token-free state.
#[tauri::command]
pub fn ja_runtime_start(
    state: tauri::State<'_, RuntimeHost>,
) -> Result<RuntimeStatus, RuntimeCommandError> {
    state.start()
}

/// Stops the sidecar with a bounded process-tree shutdown.
#[tauri::command]
pub fn ja_runtime_stop(
    state: tauri::State<'_, RuntimeHost>,
) -> Result<RuntimeStatus, RuntimeCommandError> {
    state.stop()
}

/// Reads the authoritative host projection after reload or late subscription.
#[tauri::command]
pub fn ja_runtime_state(
    state: tauri::State<'_, RuntimeHost>,
) -> Result<RuntimeStatus, RuntimeCommandError> {
    state.state()
}

/// Starts one typed turn without allowing UI control of executable, cwd, or
/// handshake values.
#[tauri::command]
pub fn ja_turn_start(
    input: TurnStartInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<TurnAccepted, RuntimeCommandError> {
    state.turn_start(input)
}

/// Requests interruption of one active turn while preserving the sidecar
/// process and waiting for its eventual completion event.
#[tauri::command]
pub fn ja_turn_cancel(
    input: TurnCancelInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<TurnCancelResult, RuntimeCommandError> {
    state.turn_cancel(input)
}

/// Returns the token-free native recovery gate for the settings/recovery UI.
#[tauri::command]
pub fn ja_runtime_recovery_state(state: tauri::State<'_, RuntimeHost>) -> RuntimeRecoveryState {
    state.recovery_state()
}

/// Acknowledges only the current native recovery identity/revision; arbitrary
/// paths, executable values, raw handshake IDs and generic RPC are excluded.
#[tauri::command]
pub fn ja_runtime_acknowledge_recovery(
    confirmation: ManualRecoveryConfirmation,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<RuntimeRecoveryState, RuntimeCommandError> {
    state.acknowledge_recovery(&confirmation)
}

/// Freezes one validated workspace/settings snapshot before the next start;
/// changing it uses the host's bounded restart path rather than hot-swapping
/// a live AgentScope generation.
#[tauri::command]
pub fn ja_runtime_configure(
    input: RuntimeConfigureInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<RuntimeConfigurationStatus, RuntimeCommandError> {
    state.configure(input)
}

/// Sends one typed approval decision to the pending Java request without
/// exposing a generic server-response command to the WebView.
#[tauri::command]
pub fn ja_approval_respond(
    input: ApprovalResponseInput,
    state: tauri::State<'_, RuntimeHost>,
) -> Result<(), RuntimeCommandError> {
    state.approval_respond(input)
}

/// Shares the production exit cleanup path with MockRuntime tests so a Tauri
/// callback cannot silently diverge from the managed-state shutdown contract.
pub fn cleanup_on_exit(state: &RuntimeHost) -> Result<(), RuntimeCommandError> {
    state.shutdown()
}

/// Shares the Tauri composition root's absolute deadline with the Java host;
/// callers that already cleaned another native resource must not restart the
/// bridge's standalone shutdown timeout.
pub fn cleanup_on_exit_until(
    state: &RuntimeHost,
    deadline: Instant,
) -> Result<(), RuntimeCommandError> {
    state.shutdown_until(deadline)
}
