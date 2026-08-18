// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

pub mod agent_process;
pub mod app_runtime;
pub mod git_read;
pub mod preview;
pub mod settings;
pub mod terminal;
pub mod workspace_read;

use app_runtime::{
    EventEmitError, EventSink, RPC_FRAME_EVENT, RuntimeHost, cleanup_on_exit, prepare_run_dir,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager, RunEvent};

// Keep a single composition root so plugins, managed state, and shutdown
// resources are registered exactly once and reviewed as a unit.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default().setup(|app| {
        let resource_dir = app.path().resource_dir()?;
        let run_dir = prepare_run_dir(app.path().app_data_dir()?.join("runtime"))?;
        let config = debug_or_bundled_launch_config(resource_dir, run_dir)?;
        let settings_root = app.path().app_data_dir()?.join("settings");
        let settings = settings::SettingsCommandHost::new(settings_root).map_err(|error| {
            tauri::Error::Setup((Box::new(error) as Box<dyn std::error::Error>).into())
        })?;
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
        // These managed hosts are the sole owners of PTY, preview model and
        // settings resources; no command constructs a competing singleton.
        app.manage(terminal::TerminalCommandHost::new());
        app.manage(preview::PreviewCommandHost::default());
        app.manage(settings);
        Ok(())
    });
    // Register the official dialog plugin at the composition root so the
    // frontend can use native open dialogs without adding a duplicate Rust
    // command or changing the existing runtime/single-instance lifecycle.
    let builder = builder.plugin(tauri_plugin_dialog::init());
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    }));
    let app = builder
        .invoke_handler(tauri::generate_handler![
            app_runtime::ja_runtime_start,
            app_runtime::ja_runtime_stop,
            app_runtime::ja_runtime_state,
            app_runtime::ja_runtime_recovery_state,
            app_runtime::ja_runtime_acknowledge_recovery,
            app_runtime::ja_runtime_configure,
            app_runtime::ja_approval_respond,
            app_runtime::ja_turn_start,
            app_runtime::ja_turn_cancel,
            app_runtime::history::ja_workspace_list,
            app_runtime::history::ja_thread_create,
            app_runtime::history::ja_thread_list,
            app_runtime::history::ja_thread_read,
            workspace_read::command::ja_workspace_tree,
            workspace_read::command::ja_workspace_read_file,
            workspace_read::command::ja_workspace_search,
            git_read::commands::ja_git_status,
            git_read::commands::ja_git_diff,
            terminal::commands::ja_terminal_configure,
            terminal::commands::ja_terminal_open,
            terminal::commands::ja_terminal_input,
            terminal::commands::ja_terminal_resize,
            terminal::commands::ja_terminal_poll,
            terminal::commands::ja_terminal_scrollback,
            terminal::commands::ja_terminal_close,
            preview::commands::ja_preview_open,
            preview::commands::ja_preview_navigate,
            preview::commands::ja_preview_close,
            preview::commands::ja_preview_events,
            preview::commands::ja_preview_state,
            settings::commands::ja_settings_load,
            settings::commands::ja_settings_save,
            settings::commands::ja_settings_set_credential,
            settings::commands::ja_settings_delete_credential
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { api, .. } => {
            handle_full_exit_requested(app_handle, &api);
        }
        RunEvent::Exit => {
            let host = app_handle.state::<RuntimeHost>();
            let terminal = app_handle.state::<terminal::TerminalCommandHost>();
            let preview = app_handle.state::<preview::PreviewCommandHost>();
            let preview_empty = preview.manager().active_count().unwrap_or(usize::MAX) == 0;
            if !host.exit_ready() || !terminal.is_empty() || !preview_empty {
                host.record_forced_exit();
                tracing::error!("runtime Exit reached before cleanup confirmation");
            }
        }
        _ => {}
    });
}

/// Closes all owned Rust resources before allowing the native event loop to
/// exit; a failed cleanup keeps the app alive for a retry.
fn handle_full_exit_requested<R: tauri::Runtime>(
    app_handle: &tauri::AppHandle<R>,
    api: &tauri::ExitRequestApi,
) {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(10));
    let runtime = app_handle.state::<RuntimeHost>();
    let terminal = app_handle.state::<terminal::TerminalCommandHost>();
    let preview = app_handle.state::<preview::PreviewCommandHost>();
    let terminal_result = terminal.shutdown_until(deadline);
    let preview_result = preview.shutdown();
    close_preview_windows(app_handle);
    let runtime_result = cleanup_on_exit(&runtime);
    if terminal_result.is_err() || preview_result.is_err() || runtime_result.is_err() {
        tracing::error!("application cleanup did not complete before exit deadline");
        api.prevent_exit();
    }
}

/// Releases every child WebView whose label is owned by the preview command
/// adapter, while leaving the main window lifecycle to Tauri.
fn close_preview_windows<R: tauri::Runtime>(app_handle: &tauri::AppHandle<R>) {
    for (label, window) in app_handle.webview_windows() {
        if label.starts_with("preview_") {
            let _ = window.close();
        }
    }
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
