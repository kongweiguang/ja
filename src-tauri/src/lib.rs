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
    EventEmitError, EventSink, RPC_FRAME_EVENT, RuntimeHost, cleanup_on_exit,
    cleanup_on_exit_until, prepare_run_dir,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
#[cfg(debug_assertions)]
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager, RunEvent};

#[cfg(debug_assertions)]
const EXIT_TRACE_PATH_ENV: &str = "JA_E2E_EXIT_TRACE_PATH";
#[cfg(debug_assertions)]
const E2E_RUNTIME_ROOT_ENV: &str = "JA_E2E_RUNTIME_ROOT";

/// Fixed debug-only lifecycle vocabulary; keeping the values closed prevents
/// paths, ids, errors, or secrets from entering the E2E trace file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugExitTraceEvent {
    RequestEntered,
    RequestReturned,
    ExitEntered,
    ExitReturned,
}

impl DebugExitTraceEvent {
    /// Returns the complete finite event order used by the desktop smoke
    /// harness; `App::run` never returns, so `ExitReturned` is the terminal
    /// observable stage rather than a synthetic post-run marker.
    #[cfg(all(test, debug_assertions))]
    const ALL: [Self; 4] = [
        Self::RequestEntered,
        Self::RequestReturned,
        Self::ExitEntered,
        Self::ExitReturned,
    ];

    /// Maps an event to its stable single-line representation.
    #[cfg(debug_assertions)]
    const fn line(self) -> &'static str {
        match self {
            Self::RequestEntered => "stage=exit_requested_enter",
            Self::RequestReturned => "stage=exit_requested_return",
            Self::ExitEntered => "stage=exit_enter",
            Self::ExitReturned => "stage=exit_return",
        }
    }
}

/// Owns the optional debug trace destination captured once before `app.run`.
/// Release builds keep the same zero-state shape but never inspect the
/// environment or perform file I/O, so tracing cannot affect product behavior.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DebugExitTrace {
    path: Option<PathBuf>,
}

#[cfg(not(debug_assertions))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DebugExitTrace;

impl DebugExitTrace {
    /// Constructs the disabled state used when the debug E2E contract is not
    /// completely satisfied; no fallback path is ever invented implicitly.
    #[cfg(debug_assertions)]
    const fn disabled() -> Self {
        Self { path: None }
    }

    /// Keeps the release trace state zero-sized and permanently disabled; no
    /// environment-controlled destination exists in the production binary.
    #[cfg(not(debug_assertions))]
    const fn disabled() -> Self {
        Self
    }

    /// Appends one fixed line on a best-effort basis; diagnostics must never
    /// change cleanup results or panic while the native event loop is exiting.
    #[cfg(debug_assertions)]
    fn record(&self, event: DebugExitTraceEvent) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = writeln!(file, "{}", event.line());
    }

    /// Compiles the release trace call into a no-op: production never reads
    /// E2E environment variables and never opens a diagnostic file.
    #[cfg(not(debug_assertions))]
    fn record(&self, _event: DebugExitTraceEvent) {}
}

/// Reads and validates the frozen debug trace configuration once per app run.
/// The destination must be absolute, have an existing canonical parent, and
/// use the canonical `JA_E2E_RUNTIME_ROOT` itself as that parent.
#[cfg(debug_assertions)]
fn debug_exit_trace_from_environment() -> DebugExitTrace {
    debug_exit_trace_from_lookup(|name| std::env::var_os(name))
}

/// Release binaries intentionally have no environment-controlled trace path.
#[cfg(not(debug_assertions))]
fn debug_exit_trace_from_environment() -> DebugExitTrace {
    DebugExitTrace::disabled()
}

/// Keeps debug environment tests deterministic by injecting lookup rather than
/// mutating process-global environment state concurrently with other tests.
#[cfg(debug_assertions)]
fn debug_exit_trace_from_lookup<F>(lookup: F) -> DebugExitTrace
where
    F: Fn(&str) -> Option<OsString>,
{
    let Some(raw_path) = lookup(EXIT_TRACE_PATH_ENV) else {
        return DebugExitTrace::disabled();
    };
    let Some(raw_runtime_root) = lookup(E2E_RUNTIME_ROOT_ENV) else {
        return DebugExitTrace::disabled();
    };
    let path = PathBuf::from(raw_path);
    let runtime_root = PathBuf::from(raw_runtime_root);
    if !path.is_absolute() || !runtime_root.is_absolute() || path.file_name().is_none() {
        return DebugExitTrace::disabled();
    }
    let Ok(runtime_root) = fs::canonicalize(runtime_root) else {
        return DebugExitTrace::disabled();
    };
    let Ok(runtime_metadata) = fs::metadata(&runtime_root) else {
        return DebugExitTrace::disabled();
    };
    if !runtime_metadata.is_dir() {
        return DebugExitTrace::disabled();
    }
    let Some(parent) = path.parent() else {
        return DebugExitTrace::disabled();
    };
    let Ok(parent) = fs::canonicalize(parent) else {
        return DebugExitTrace::disabled();
    };
    if parent != runtime_root {
        return DebugExitTrace::disabled();
    }
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return DebugExitTrace::disabled();
    }
    DebugExitTrace { path: Some(path) }
}

/// Builds and runs the single Tauri composition root so managed resources and
/// the final cleanup/forced-recovery boundary are registered exactly once.
/// Tauri's desktop `App::run` delegates to the native event loop and does not
/// return; the four-stage trace therefore ends at `ExitReturned` and cannot
/// include a post-`run` marker without describing an unreachable path.
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
    let exit_trace = debug_exit_trace_from_environment();
    #[cfg(debug_assertions)]
    let callback_trace = exit_trace.clone();
    #[cfg(not(debug_assertions))]
    let callback_trace = exit_trace;
    app.run(move |app_handle, event| match event {
        RunEvent::ExitRequested { code, .. }
            if handle_full_exit_request_event(app_handle, code, &callback_trace) =>
        {
            // This is the sole application-owned programmatic exit path in
            // the initial desktop product.  Tauri emits a second
            // `ExitRequested` carrying `Some(0)`; that follow-up is left to
            // Tauri so cleanup and the trace cannot loop or repeat.
            app_handle.exit(0);
        }
        RunEvent::Exit => {
            callback_trace.record(DebugExitTraceEvent::ExitEntered);
            let host = app_handle.state::<RuntimeHost>();
            let terminal = app_handle.state::<terminal::TerminalCommandHost>();
            let preview = app_handle.state::<preview::PreviewCommandHost>();
            let preview_empty = preview.manager().active_count().unwrap_or(usize::MAX) == 0;
            if !host.exit_ready() || !terminal.is_empty() || !preview_empty {
                host.record_forced_exit();
                tracing::error!("runtime Exit reached before cleanup confirmation");
            }
            callback_trace.record(DebugExitTraceEvent::ExitReturned);
        }
        _ => {}
    });
}

/// Handles only the initial no-code exit request and reports whether Tauri
/// should receive the one explicit `exit(0)` request.  Code-bearing requests
/// are already owned by Tauri (including restart/non-zero semantics), so they
/// intentionally do no cleanup or trace work here.
fn handle_full_exit_request_event<R: tauri::Runtime>(
    app_handle: &tauri::AppHandle<R>,
    code: Option<i32>,
    trace: &DebugExitTrace,
) -> bool {
    if code.is_some() {
        return false;
    }
    trace.record(DebugExitTraceEvent::RequestEntered);
    handle_full_exit_requested(app_handle);
    trace.record(DebugExitTraceEvent::RequestReturned);
    true
}

/// Closes all owned Rust resources under one absolute deadline.  The final
/// window is already being destroyed when this callback runs, so a failed
/// cleanup must not prevent exit and strand a headless process; the following
/// `RunEvent::Exit` records recovery while existing process-tree/Drop cleanup
/// remains the last safety boundary.
fn handle_full_exit_requested<R: tauri::Runtime>(app_handle: &tauri::AppHandle<R>) {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(10));
    let runtime = app_handle.state::<RuntimeHost>();
    let terminal = app_handle.state::<terminal::TerminalCommandHost>();
    let preview = app_handle.state::<preview::PreviewCommandHost>();
    let terminal_result = terminal.shutdown_until(deadline);
    let preview_result = preview.shutdown();
    close_preview_windows(app_handle);
    let runtime_result = cleanup_on_exit_until(&runtime, deadline);
    if terminal_result.is_err() || preview_result.is_err() || runtime_result.is_err() {
        tracing::error!("application cleanup did not complete before exit deadline");
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
    #[cfg(feature = "tauri-smoke")]
    use std::path::Path;
    use std::path::PathBuf;

    #[cfg(feature = "tauri-smoke")]
    use super::app_runtime::{EventEmitError, EventSink, LaunchConfig, RuntimeHost};
    #[cfg(feature = "tauri-smoke")]
    use super::preview::PreviewCommandHost;
    #[cfg(feature = "tauri-smoke")]
    use super::terminal::TerminalCommandHost;
    #[cfg(all(feature = "tauri-smoke", not(debug_assertions)))]
    use std::ffi::OsString;
    #[cfg(feature = "tauri-smoke")]
    use std::sync::atomic::AtomicUsize;
    #[cfg(feature = "tauri-smoke")]
    use std::sync::{Arc, mpsc};
    #[cfg(feature = "tauri-smoke")]
    use std::thread;
    #[cfg(feature = "tauri-smoke")]
    use std::time::Duration;
    #[cfg(feature = "tauri-smoke")]
    use tauri::Manager;

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

    #[cfg(debug_assertions)]
    /// Confirms that an unset debug trace environment cannot invent or touch a
    /// file, keeping diagnostics opt-in and outside the normal app-data path.
    #[test]
    fn debug_exit_trace_without_environment_writes_nothing() {
        let root = test_root("exit-trace-unset");
        let path = root.join("events.log");
        let trace = super::debug_exit_trace_from_lookup(|_| None);
        trace.record(super::DebugExitTraceEvent::RequestEntered);
        assert!(!path.exists());
        cleanup(&root);
    }

    #[cfg(debug_assertions)]
    /// Confirms the accepted trace path is rooted under the canonical debug
    /// runtime directory and emits only the frozen lifecycle vocabulary/order.
    #[test]
    fn debug_exit_trace_writes_fixed_event_sequence() {
        let root = test_root("exit-trace-valid");
        fs::create_dir_all(&root).expect("trace runtime root");
        let path = root.join("events.log");
        let trace = super::debug_exit_trace_from_lookup(|name| match name {
            super::EXIT_TRACE_PATH_ENV => Some(path.clone().into_os_string()),
            super::E2E_RUNTIME_ROOT_ENV => Some(root.clone().into_os_string()),
            _ => None,
        });
        for event in super::DebugExitTraceEvent::ALL {
            trace.record(event);
        }
        let content = fs::read_to_string(&path).expect("trace file");
        assert_eq!(
            content,
            "stage=exit_requested_enter\nstage=exit_requested_return\nstage=exit_enter\nstage=exit_return\n"
        );
        cleanup(&root);
    }

    #[cfg(not(debug_assertions))]
    /// Proves the release build uses the compile-time disabled trace hook and
    /// cannot obtain a destination from the process environment.
    #[test]
    fn release_exit_trace_hook_is_compile_time_disabled() {
        let trace = super::debug_exit_trace_from_environment();
        assert_eq!(trace, super::DebugExitTrace);
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

    #[cfg(feature = "tauri-smoke")]
    /// Resolves the JDK25 test executable without changing the production
    /// launch path, keeping the MockRuntime lifecycle test deterministic.
    fn full_exit_java() -> PathBuf {
        let java = std::env::var_os("JA_TEST_JAVA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("JAVA_HOME").map(|home| {
                    PathBuf::from(home).join("bin").join(if cfg!(windows) {
                        "java.exe"
                    } else {
                        "java"
                    })
                })
            })
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "java.exe" } else { "java" }));
        assert!(java.is_file(), "JA_TEST_JAVA must point to JDK25 java");
        let version = std::process::Command::new(&java)
            .arg("-version")
            .output()
            .expect("inspect JDK25 version");
        let banner = format!(
            "{}{}",
            String::from_utf8_lossy(&version.stdout),
            String::from_utf8_lossy(&version.stderr)
        );
        assert!(
            banner.lines().any(|line| {
                line.split(|character: char| !character.is_ascii_digit())
                    .find(|part| !part.is_empty())
                    == Some("25")
            }),
            "full-exit test requires JDK25, got {banner:?}"
        );
        java
    }

    #[cfg(feature = "tauri-smoke")]
    /// Resolves the already-built fake Java sidecar instead of inventing a
    /// second protocol fixture for the full composition-root test.
    fn full_exit_jar() -> PathBuf {
        let jar = std::env::var_os("JA_TEST_JAR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("agent")
                    .join("target")
                    .join("ja.jar")
            });
        assert!(jar.is_file(), "build agent/target/ja.jar first");
        jar
    }

    #[cfg(feature = "tauri-smoke")]
    /// Creates one fake sidecar launch snapshot for the MockRuntime tests;
    /// production still obtains its executable only from bundled resources.
    fn full_exit_fixture_config(run_dir: PathBuf) -> LaunchConfig {
        let java = full_exit_java();
        let jar = full_exit_jar();
        #[cfg(debug_assertions)]
        {
            LaunchConfig::debug_java(java, jar, run_dir).expect("debug Java25 config")
        }
        #[cfg(not(debug_assertions))]
        {
            LaunchConfig::for_test(
                java,
                vec![
                    OsString::from("-jar"),
                    jar.into_os_string(),
                    OsString::from("--runtime=fake"),
                ],
                run_dir,
            )
        }
    }

    #[cfg(feature = "tauri-smoke")]
    /// Builds an isolated run directory while leaving all real app-data paths
    /// untouched, so failed cleanup can be inspected without cross-test state.
    fn full_exit_run_dir(label: &str) -> PathBuf {
        let path = test_root(label);
        fs::create_dir_all(&path).expect("full-exit run directory");
        path
    }

    #[cfg(feature = "tauri-smoke")]
    /// Creates the same fixed trace destination used by the lifecycle test;
    /// release builds receive the zero-sized disabled seam instead.
    fn full_exit_trace(run_dir: &Path) -> (PathBuf, super::DebugExitTrace) {
        let path = run_dir.join("exit-trace.log");
        #[cfg(debug_assertions)]
        {
            let runtime_root = run_dir.to_path_buf();
            let trace = super::debug_exit_trace_from_lookup(|name| match name {
                super::EXIT_TRACE_PATH_ENV => Some(path.clone().into_os_string()),
                super::E2E_RUNTIME_ROOT_ENV => Some(runtime_root.clone().into_os_string()),
                _ => None,
            });
            (path, trace)
        }
        #[cfg(not(debug_assertions))]
        {
            (path, super::DebugExitTrace::disabled())
        }
    }

    #[cfg(all(feature = "tauri-smoke", debug_assertions))]
    /// Checks the complete five-stage trace after the MockRuntime has joined,
    /// so the assertion observes the same persisted file as the E2E harness.
    fn assert_full_exit_trace(path: &Path) {
        let content = fs::read_to_string(path).expect("full-exit trace");
        assert_eq!(
            content,
            "stage=exit_requested_enter\nstage=exit_requested_return\nstage=exit_enter\nstage=exit_return\n"
        );
    }

    #[cfg(feature = "tauri-smoke")]
    /// Runs the native MockRuntime event loop with the actual full-exit handler
    /// and mirrors production's forced-exit recovery accounting after a
    /// cleanup fault, without repeating cleanup for the programmatic exit
    /// request that Tauri emits after the first no-code request.
    fn run_full_exit_mock(
        host: RuntimeHost,
        trace: super::DebugExitTrace,
    ) -> (
        tauri::AppHandle<tauri::test::MockRuntime>,
        tauri::WebviewWindow<tauri::test::MockRuntime>,
        mpsc::Receiver<&'static str>,
        thread::JoinHandle<()>,
    ) {
        use tauri::RunEvent;
        use tauri::test::{mock_builder, mock_context, noop_assets};

        let app = mock_builder()
            .manage(host)
            .manage(TerminalCommandHost::new())
            .manage(PreviewCommandHost::default())
            .build(mock_context(noop_assets()))
            .expect("mock full-exit app");
        let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock full-exit window");
        let app_handle = app.handle().clone();
        let (sender, receiver) = mpsc::sync_channel(8);
        #[cfg(debug_assertions)]
        let callback_trace = trace.clone();
        #[cfg(not(debug_assertions))]
        let callback_trace = trace;
        let runner = thread::spawn(move || {
            app.run(move |app_handle, event| match event {
                RunEvent::Ready => {
                    let _ = sender.send("ready");
                }
                RunEvent::ExitRequested { code, .. } => {
                    let initial =
                        super::handle_full_exit_request_event(app_handle, code, &callback_trace);
                    if initial {
                        let _ = sender.send("exit-requested");
                        // Tauri's MockRuntime leaves request_exit unimplemented;
                        // dispatch its code-bearing follow-up through the same
                        // production handler without calling the unsupported
                        // runtime API or introducing a lifecycle gate.
                        let _ = super::handle_full_exit_request_event(
                            app_handle,
                            Some(0),
                            &callback_trace,
                        );
                        let _ = sender.send("exit-programmatic-requested");
                    }
                }
                RunEvent::Exit => {
                    callback_trace.record(super::DebugExitTraceEvent::ExitEntered);
                    let host = app_handle.state::<RuntimeHost>();
                    let terminal = app_handle.state::<TerminalCommandHost>();
                    let preview = app_handle.state::<PreviewCommandHost>();
                    let preview_empty = preview.manager().active_count().unwrap_or(usize::MAX) == 0;
                    let exit_safe = host.exit_ready() && terminal.is_empty() && preview_empty;
                    if !exit_safe {
                        host.record_forced_exit();
                    }
                    let _ = sender.send(if exit_safe {
                        "exit-ready"
                    } else {
                        "exit-unsafe"
                    });
                    callback_trace.record(super::DebugExitTraceEvent::ExitReturned);
                }
                _ => {}
            });
        });
        (app_handle, window, receiver, runner)
    }

    #[cfg(feature = "tauri-smoke")]
    /// Confirms the full composition root shares its absolute deadline and
    /// permits the native exit after one cleanup and one programmatic exit
    /// request after Java, terminal, and preview cleanup.
    #[test]
    fn full_exit_request_allows_clean_runtime_cleanup() {
        let run_dir = full_exit_run_dir("full-exit-success");
        let (_trace_path, trace) = full_exit_trace(&run_dir);
        let sink: EventSink = Arc::new(|_| Ok::<(), EventEmitError>(()));
        let host = RuntimeHost::new(full_exit_fixture_config(run_dir.clone()), sink);
        host.start().expect("fake sidecar ready");
        let (app_handle, window, receiver, runner) = run_full_exit_mock(host, trace);
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok("ready"));
        window.close().expect("full-exit close request");
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-requested")
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-programmatic-requested")
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-ready")
        );
        runner.join().expect("mock full-exit runner");
        assert!(app_handle.state::<RuntimeHost>().exit_ready());
        #[cfg(debug_assertions)]
        assert_full_exit_trace(&_trace_path);
        cleanup(&run_dir);
    }

    #[cfg(feature = "tauri-smoke")]
    /// Confirms a failed full-exit cleanup still reaches the native Exit event,
    /// records the recovery marker, and reports the retained owner as unsafe
    /// after the same single cleanup/programmatic-exit sequence.
    #[test]
    fn full_exit_request_allows_forced_exit_after_cleanup_fault() {
        let run_dir = full_exit_run_dir("full-exit-forced");
        let (_trace_path, trace) = full_exit_trace(&run_dir);
        let sink: EventSink = Arc::new(|_| Ok::<(), EventEmitError>(()));
        let failures = Arc::new(AtomicUsize::new(usize::MAX));
        let host = RuntimeHost::new_for_exit_test(
            full_exit_fixture_config(run_dir.clone()),
            sink,
            Duration::from_millis(500),
            Arc::clone(&failures),
        );
        host.start().expect("fake sidecar ready");
        let (app_handle, first_window, receiver, runner) = run_full_exit_mock(host, trace);
        assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok("ready"));
        first_window.close().expect("first close request");
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-requested")
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-programmatic-requested")
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok("exit-unsafe")
        );
        runner.join().expect("mock forced-exit runner");
        let host = app_handle.state::<RuntimeHost>();
        assert!(!host.exit_ready());
        let recovery = host.recovery_state();
        assert!(recovery.required);
        assert!(recovery.recovery_id.is_some());
        #[cfg(debug_assertions)]
        assert_full_exit_trace(&_trace_path);
        cleanup(&run_dir);
    }
}
