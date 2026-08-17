// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

pub mod agent_process;
pub mod app_runtime;

use app_runtime::{
    EventEmitError, EventSink, RPC_FRAME_EVENT, RuntimeHost, cleanup_on_exit, prepare_run_dir,
    register_commands,
};
use std::sync::Arc;
use tauri::{Emitter, Manager, RunEvent};

// Keep a single composition root so plugins, managed state, and shutdown
// resources are registered exactly once and reviewed as a unit.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().setup(|app| {
        let resource_dir = app.path().resource_dir()?;
        let run_dir = prepare_run_dir(app.path().app_data_dir()?.join("runtime"))?;
        let config = debug_or_bundled_launch_config(resource_dir, run_dir)?;
        let app_handle = app.handle().clone();
        let sink: EventSink = Arc::new(move |payload| {
            app_handle
                .emit(RPC_FRAME_EVENT, payload)
                .map_err(|_| EventEmitError::DeliveryFailed)
        });
        // Keep the host managed even when recovery is required so the window
        // can render a typed recovery screen instead of failing setup before
        // Tauri creates any UI surface.
        app.manage(RuntimeHost::new(config, sink));
        Ok(())
    });
    let app = register_commands(builder)
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { api, .. } => {
            let host = app_handle.state::<RuntimeHost>();
            handle_exit_requested(&host, &api);
        }
        RunEvent::Exit => {
            let host = app_handle.state::<RuntimeHost>();
            if !host.exit_ready() {
                host.record_forced_exit();
                tracing::error!("runtime Exit reached before cleanup confirmation");
            }
        }
        _ => {}
    });
}

/// Applies the same cleanup/prevent policy in production and MockRuntime
/// tests; a failed first attempt leaves the managed owner alive for retry.
pub fn handle_exit_requested(host: &RuntimeHost, api: &tauri::ExitRequestApi) {
    if let Err(error) = cleanup_on_exit(host) {
        tracing::error!(
            code = error.code,
            "runtime cleanup blocked application exit"
        );
        api.prevent_exit();
    }
}

/// Uses explicit host-only Java25 debug variables when present; release builds
/// can only resolve a fixed native resource under Tauri's trusted directory.
fn debug_or_bundled_launch_config(
    resource_dir: std::path::PathBuf,
    run_dir: std::path::PathBuf,
) -> Result<app_runtime::LaunchConfig, app_runtime::RuntimeCommandError> {
    #[cfg(debug_assertions)]
    if let (Some(java), Some(jar)) = (
        std::env::var_os("JA_DEBUG_JAVA"),
        std::env::var_os("JA_DEBUG_JAR"),
    ) {
        return app_runtime::LaunchConfig::debug_java(java.into(), jar.into(), run_dir);
    }
    app_runtime::bundled_launch_config(resource_dir, run_dir)
}
