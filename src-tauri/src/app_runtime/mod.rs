// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri composition surface for the Java sidecar bridge.

mod bridge;
mod config;
mod host;
mod projection;

#[cfg(feature = "test-support")]
pub use bridge::BridgeTestTrace;
pub use bridge::RuntimeBridge;
pub use config::{
    EventEmitError, EventSink, LaunchConfig, ManualRecoveryConfirmation, ManualRecoveryReason,
    RuntimeCommandError, RuntimeRecoveryState, RuntimeStatus, RuntimeStatusKind, TurnAccepted,
    TurnInputPart, TurnStartInput, bundled_launch_config, prepare_run_dir,
};
pub use host::RuntimeHost;
pub use projection::RPC_FRAME_EVENT;

/// Registers each sidecar command once at the native composition root.
pub fn register_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        ja_runtime_start,
        ja_runtime_stop,
        ja_runtime_state,
        ja_runtime_recovery_state,
        ja_runtime_acknowledge_recovery,
        ja_turn_start
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

/// Shares the production exit cleanup path with MockRuntime tests so a Tauri
/// callback cannot silently diverge from the managed-state shutdown contract.
pub fn cleanup_on_exit(state: &RuntimeHost) -> Result<(), RuntimeCommandError> {
    state.shutdown()
}
