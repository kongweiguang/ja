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
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::PathBuf;
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
        let settings_root = resolve_settings_root(
            app.path().app_data_dir()?,
            debug_settings_root_override().as_deref(),
        )
        .map_err(|error| {
            tauri::Error::Setup((Box::new(error) as Box<dyn std::error::Error>).into())
        })?;
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

/// Chooses the settings directory without letting a test-only host override
/// alter release behavior; the default remains Tauri's app-data child.
fn resolve_settings_root(
    app_data_dir: PathBuf,
    override_root: Option<&OsStr>,
) -> Result<PathBuf, SettingsRootError> {
    let Some(raw_override) = override_root else {
        return Ok(app_data_dir.join("settings"));
    };

    let override_root = PathBuf::from(raw_override);
    if !override_root.is_absolute() {
        return Err(SettingsRootError::InvalidConfiguration);
    }

    match fs::metadata(&override_root) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(SettingsRootError::InvalidConfiguration);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SettingsRootError::SetupFailed),
    }

    fs::create_dir_all(&override_root).map_err(|_| SettingsRootError::SetupFailed)?;
    let canonical_root =
        fs::canonicalize(&override_root).map_err(|_| SettingsRootError::SetupFailed)?;
    let metadata = fs::metadata(&canonical_root).map_err(|_| SettingsRootError::SetupFailed)?;
    if !metadata.is_dir() {
        return Err(SettingsRootError::InvalidConfiguration);
    }
    Ok(canonical_root)
}

/// Reads the debug-only settings root hook; release has no environment read.
#[cfg(debug_assertions)]
fn debug_settings_root_override() -> Option<OsString> {
    std::env::var_os("JA_E2E_SETTINGS_ROOT")
}

/// Keeps release builds independent from the host's test environment.
#[cfg(not(debug_assertions))]
fn debug_settings_root_override() -> Option<OsString> {
    None
}

/// Keeps setup failures stable and prevents filesystem details from reaching
/// the UI or test logs through an error's display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRootError {
    InvalidConfiguration,
    SetupFailed,
}

impl fmt::Display for SettingsRootError {
    /// Emits only a stable category so setup diagnostics cannot disclose paths.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConfiguration => "invalid settings configuration",
            Self::SetupFailed => "settings setup failed",
        })
    }
}

impl std::error::Error for SettingsRootError {}

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

#[cfg(test)]
mod tests {
    use super::{SettingsRootError, resolve_settings_root};
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;

    /// Creates an isolated path so tests can verify directory creation without
    /// changing process-global environment variables or touching app data.
    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ja-settings-root-{}-{}-{}",
            std::process::id(),
            label,
            uuid::Uuid::new_v4()
        ))
    }

    /// Removes only the test-owned temporary tree after each root-selection
    /// assertion, leaving real settings directories untouched.
    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
        let _ = fs::remove_file(path);
    }

    /// Confirms the absence of the debug hook preserves the original app-data
    /// child path and does not eagerly create it.
    #[test]
    fn default_root_is_app_data_settings_child() {
        let app_data = test_root("default");
        let expected = app_data.join("settings");
        let resolved = resolve_settings_root(app_data.clone(), None).expect("default root");
        assert_eq!(resolved, expected);
        assert!(!app_data.exists());
        cleanup(&app_data);
    }

    /// Confirms a debug override creates and canonicalizes a valid absolute
    /// directory without relying on a mutable environment variable.
    #[test]
    fn absolute_override_is_created_and_canonicalized() {
        let app_data = test_root("absolute-app-data");
        let override_root = test_root("absolute-override");
        let resolved = resolve_settings_root(app_data.clone(), Some(override_root.as_os_str()))
            .expect("absolute override");
        let expected = fs::canonicalize(&override_root).expect("canonical override");
        assert_eq!(resolved, expected);
        assert!(resolved.is_dir());
        cleanup(&app_data);
        cleanup(&override_root);
    }

    /// Confirms relative paths are rejected before any directory mutation.
    #[test]
    fn relative_override_is_rejected() {
        let app_data = test_root("relative-app-data");
        let result = resolve_settings_root(app_data.clone(), Some(OsStr::new("settings-test")));
        assert_eq!(result, Err(SettingsRootError::InvalidConfiguration));
        assert!(!app_data.exists());
        cleanup(&app_data);
    }

    /// Confirms an existing file cannot be promoted to a settings directory.
    #[test]
    fn file_override_is_rejected() {
        let app_data = test_root("file-app-data");
        let file_path = test_root("file-override");
        fs::write(&file_path, b"not a directory").expect("test file");
        let result = resolve_settings_root(app_data.clone(), Some(file_path.as_os_str()));
        assert_eq!(result, Err(SettingsRootError::InvalidConfiguration));
        cleanup(&app_data);
        cleanup(&file_path);
    }

    /// Confirms malformed host paths map to a stable setup category without
    /// echoing the invalid path in the returned error.
    #[test]
    fn malformed_override_is_stable_and_path_free() {
        let app_data = test_root("invalid-app-data");
        let invalid_path = test_root("invalid").join("bad\0root");
        let result = resolve_settings_root(app_data.clone(), Some(invalid_path.as_os_str()));
        assert_eq!(result, Err(SettingsRootError::SetupFailed));
        let error = result.expect_err("invalid path must fail");
        assert_eq!(error.to_string(), "settings setup failed");
        assert!(!error.to_string().contains("bad"));
        cleanup(&app_data);
        cleanup(&invalid_path);
    }
}
