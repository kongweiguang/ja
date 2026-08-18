// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Real Java25 process coverage for the Rust/Tauri host bridge.

use base64::Engine;
#[cfg(feature = "tauri-smoke")]
use ja_lib::app_runtime::RuntimeStatus;
use ja_lib::app_runtime::{
    BridgeTestTrace, EventEmitError, EventSink, LaunchConfig, ManualRecoveryConfirmation,
    ManualRecoveryReason, RuntimeBridge, RuntimeConfigureInput, RuntimeHost, RuntimeStatusKind,
    TurnInputPart, TurnStartInput,
};
use ja_lib::settings::{
    ApiProtocol, CredentialPurpose, CredentialRef, ProfileSetting, RuntimeSecretDelivery,
    SecretBackend, SecretError, SettingsDocument,
};
use serde_json::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
#[cfg(feature = "tauri-smoke")]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TempRunDir(PathBuf);

impl TempRunDir {
    /// Uses a unique directory so parallel integration workers never share a
    /// sidecar cwd or accidentally observe another test's files.
    fn create(label: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ja-{label}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&path).expect("test run directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("private test run directory");
        }
        Self(path)
    }
}

impl Drop for TempRunDir {
    /// Test cleanup is best effort after the supervisor has boundedly reaped
    /// the child; a failed cleanup remains visible in the test result logs.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn event_sink() -> (EventSink, Receiver<Value>) {
    let (sender, receiver) = mpsc::sync_channel(512);
    let sink: EventSink = Arc::new(move |value| {
        sender
            .try_send(value)
            .map_err(|_| EventEmitError::QueueFull)
    });
    (sink, receiver)
}

fn fixture_config(run_dir: &TempRunDir) -> LaunchConfig {
    let java = java_executable();
    let jar = test_jar();
    #[cfg(debug_assertions)]
    {
        LaunchConfig::debug_java(java, jar, run_dir.0.clone()).expect("debug Java25 config")
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
            run_dir.0.clone(),
        )
    }
}

fn test_jar() -> PathBuf {
    let jar = std::env::var_os("JA_TEST_JAR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("agent")
                .join("target")
                .join("ja.jar")
        });
    assert!(
        jar.is_file(),
        "build agent/target/ja.jar first or set JA_TEST_JAR"
    );
    jar
}

/// Adds a unique JVM property and data directory so the host test can query the
/// real child command line while profile activation has the same required
/// writable runtime state as the production-shaped launch.
fn marked_fixture_config(run_dir: &TempRunDir, marker: &str) -> LaunchConfig {
    let java = java_executable();
    let data_dir = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(run_dir.0.to_string_lossy().as_bytes());
    LaunchConfig::for_test(
        java,
        vec![
            OsString::from(format!("-Dja.test.marker={marker}")),
            OsString::from("-jar"),
            test_jar().into_os_string(),
            OsString::from("--runtime=fake"),
            OsString::from(format!("--data-dir-base64={data_dir}")),
        ],
        run_dir.0.clone(),
    )
}

/// Builds the production-mode Java fixture with the same explicit executable and
/// arguments shape as the packaged native launch, keeping the replay test on
/// the real production protocol rather than the fake runtime shortcut.
fn production_fixture_config(run_dir: &TempRunDir) -> LaunchConfig {
    let data_dir = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(run_dir.0.to_string_lossy().as_bytes());
    LaunchConfig::for_test(
        java_executable(),
        vec![
            OsString::from("-jar"),
            test_jar().into_os_string(),
            OsString::from("--runtime=production"),
            OsString::from(format!("--data-dir-base64={data_dir}")),
        ],
        run_dir.0.clone(),
    )
}

/// Provides one deterministic model credential through the same opaque
/// delivery seam used by the native keyring, while recording that Java really
/// requested the purpose-bound secret during profile activation.
#[derive(Default)]
struct ReplaySecretFixture {
    resolves: AtomicUsize,
}

impl SecretBackend for ReplaySecretFixture {
    /// Test writes are intentionally rejected because this fixture represents
    /// an already-provisioned credential and must not persist any secret.
    fn set(
        &self,
        _purpose: CredentialPurpose,
        _reference: &CredentialRef,
        _secret: &str,
    ) -> Result<(), SecretError> {
        Err(SecretError::StoreUnavailable)
    }

    /// Returns a short-lived opaque delivery only for the expected model
    /// reference; any other purpose or reference proves the relation check is
    /// not being bypassed by the integration fixture.
    fn get(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<RuntimeSecretDelivery, SecretError> {
        if purpose != CredentialPurpose::Model || reference.as_str() != "cred_replay_fixture" {
            return Err(SecretError::InvalidReference);
        }
        self.resolves.fetch_add(1, Ordering::SeqCst);
        RuntimeSecretDelivery::fixture("fixture-secret")
    }

    /// Deletion is unsupported because the fixture owns no persistent store.
    fn delete(
        &self,
        _purpose: CredentialPurpose,
        _reference: &CredentialRef,
    ) -> Result<(), SecretError> {
        Err(SecretError::StoreUnavailable)
    }
}

/// Builds a production-mode replay config with only a test SecretBackend
/// substitution; all Java handshake, replay ordering, and secret response
/// handling remain the production host path.
fn production_replay_fixture_config(
    run_dir: &TempRunDir,
    backend: Arc<ReplaySecretFixture>,
) -> LaunchConfig {
    production_fixture_config(run_dir).with_secret_backend(backend)
}

/// Resolves the test-only JVM and always verifies major 25, including an
/// explicit override, so a local JDK mismatch cannot make integration tests
/// appear green while exercising a different runtime contract.
fn java_executable() -> PathBuf {
    let java = if let Some(path) = std::env::var_os("JA_TEST_JAVA") {
        PathBuf::from(path)
    } else if let Some(home) = std::env::var_os("JAVA_HOME") {
        let candidate =
            PathBuf::from(home)
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        if candidate.is_file() {
            candidate
        } else {
            resolve_path_java()
        }
    } else {
        resolve_path_java()
    };
    assert!(java.is_file(), "set JA_TEST_JAVA to Liberica JDK25 java");
    let version = Command::new(&java)
        .arg("-version")
        .output()
        .expect("inspect test java version");
    assert!(version.status.success(), "test Java -version failed");
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
        "JA integration requires Java major 25, got {banner:?}"
    );
    java
}

/// Resolves PATH only for test setup; packaged runtime launch never uses this
/// fallback, keeping environment-dependent behavior out of production.
fn resolve_path_java() -> PathBuf {
    let command = if cfg!(windows) { "where.exe" } else { "which" };
    let binary = if cfg!(windows) { "java.exe" } else { "java" };
    let output = Command::new(command)
        .arg(binary)
        .output()
        .expect("resolve test java");
    assert!(
        output.status.success(),
        "set JA_TEST_JAVA to Liberica JDK25 java"
    );
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    assert!(!path.is_empty(), "test java path is empty");
    PathBuf::from(path)
}

/// Queries whether the OS process table still reports a unique sidecar marker;
/// this is stronger than trusting a command response because it observes the
/// actual child process.
#[cfg(windows)]
fn process_marker_visible(marker: &str) -> bool {
    let script = format!(
        "$me=$PID; Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $me -and $_.CommandLine -like '*{marker}*' }} | Select-Object -ExpandProperty ProcessId"
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("query child process tree");
    assert!(output.status.success(), "process tree query failed");
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

/// Uses `ps` on Unix hosts to keep the same child-tree assertion portable.
#[cfg(not(windows))]
fn process_marker_visible(marker: &str) -> bool {
    let output = Command::new("ps")
        .args(["-axo", "command="])
        .output()
        .expect("query child process tree");
    assert!(output.status.success(), "process tree query failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(marker))
}

/// Waits for the OS process table to stop reporting a unique sidecar marker.
fn assert_process_tree_gone(marker: &str, deadline: Instant) {
    loop {
        if !process_marker_visible(marker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("sidecar process marker remains: {marker}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits for the operating system to expose the real child before shutdown;
/// otherwise a test could pass while Java or the slow fixture never started.
fn assert_process_marker_visible(marker: &str, deadline: Instant) {
    loop {
        if process_marker_visible(marker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("sidecar process marker never became visible: {marker}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Joins a test worker through a waiter channel so a broken worker cannot
/// extend the host test beyond the same absolute deadline as process cleanup.
fn join_with_deadline<T: Send + 'static>(
    handle: JoinHandle<T>,
    deadline: Instant,
) -> Result<T, &'static str> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = handle.join().map_err(|_| "test worker panicked");
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "test worker join deadline elapsed")?
}

fn valid_turn(text: &str) -> TurnStartInput {
    // Keep this fixture aligned with the public ja-rpc/v1 DTO so the host test
    // catches contract drift instead of silently accepting legacy aliases.
    TurnStartInput {
        thread_id: "thr_host".to_owned(),
        access_mode: "workspace".to_owned(),
        profile_revision: "profile_host".to_owned(),
        input: vec![TurnInputPart {
            kind: "text".to_owned(),
            text: Some(text.to_owned()),
        }],
    }
}

fn assert_no_token(value: &Value) {
    match value {
        Value::Object(object) => {
            assert!(
                !object
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("readyToken"))
            );
            object.values().for_each(assert_no_token);
        }
        Value::Array(values) => values.iter().for_each(assert_no_token),
        Value::String(text) => {
            assert!(!(text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit())))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn method(value: &Value) -> Option<&str> {
    value.get("method").and_then(Value::as_str)
}

/// Receives a timeline under one caller-owned absolute deadline so each event
/// cannot reset the total host test budget.
fn receive_until_at<F>(receiver: &Receiver<Value>, deadline: Instant, mut done: F) -> Vec<Value>
where
    F: FnMut(&Value) -> bool,
{
    let mut values = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let value = receiver
            .recv_timeout(remaining)
            .expect("bridge event before deadline");
        assert_no_token(&value);
        let complete = done(&value);
        values.push(value);
        if complete {
            return values;
        }
    }
    panic!("bridge event deadline elapsed");
}

/// Proves the native bounded lane has exactly 64 admitted commands before the
/// 65th is rejected; the per-instance actor gate removes scheduler timing from
/// this backpressure contract.
#[test]
fn concurrent_calls_have_bounded_queue_admission() {
    let run_dir = TempRunDir::create("queue");
    let sink: EventSink = Arc::new(|_| Ok(()));
    let (bridge, admission) = RuntimeBridge::new_for_queue_test(
        LaunchConfig::for_test(
            run_dir.0.join("missing-sidecar"),
            Vec::new(),
            run_dir.0.clone(),
        ),
        sink,
    )
    .expect("paused bridge actor");
    assert!(
        admission.wait_until_armed(Instant::now() + Duration::from_secs(5)),
        "queue actor did not reach its admission barrier"
    );
    for _ in 0..64 {
        bridge
            .try_queue_probe_for_test()
            .expect("each bounded slot admits exactly one command");
    }
    let full = bridge
        .try_queue_probe_for_test()
        .expect_err("the 65th command must be rejected while actor is paused");
    assert_eq!(full.code, "RUNTIME_QUEUE_FULL");
    admission.release();
    assert!(
        admission.wait_until_processed(64, Instant::now() + Duration::from_secs(5)),
        "actor did not drain all admitted probes"
    );
    bridge.shutdown().unwrap_or_else(|error| {
        panic!(
            "priority actor shutdown: {error:?}; actor phase={}; exit_ready={}",
            bridge.phase_for_test(),
            bridge.exit_ready()
        )
    });
}

/// Uses Tauri's official MockRuntime to exercise command registration,
/// managed state, native event emission, Java25 fake turn and bounded stop.
#[cfg(feature = "tauri-smoke")]
#[test]
fn tauri_mock_composition_smoke_uses_typed_commands() {
    use ja_lib::app_runtime::{RPC_FRAME_EVENT, cleanup_on_exit, register_commands};
    use tauri::Emitter;
    use tauri::Listener;
    use tauri::Manager;
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
    use tauri::webview::InvokeRequest;

    let run_dir = TempRunDir::create("tauri-mock");
    let marker = format!("ja-tauri-cancel-{}", std::process::id());
    let workspace_root = run_dir.0.join("workspace");
    std::fs::create_dir_all(&workspace_root).expect("mock workspace");
    std::fs::write(
        workspace_root.join("README.md"),
        "hello from the Tauri command composition test\n",
    )
    .expect("mock workspace file");
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&workspace_root)
        .status()
        .expect("git executable")
        .success()
        .then_some(())
        .expect("initialize temporary repository");
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&workspace_root)
        .status()
        .expect("git executable")
        .success()
        .then_some(())
        .expect("stage temporary file for read-only diff");
    let app_slot: Arc<OnceLock<tauri::AppHandle<tauri::test::MockRuntime>>> =
        Arc::new(OnceLock::new());
    let slot_for_sink = Arc::clone(&app_slot);
    let sink: EventSink = Arc::new(move |value| {
        let app = slot_for_sink.get().ok_or(EventEmitError::DeliveryFailed)?;
        app.emit(RPC_FRAME_EVENT, value)
            .map_err(|_| EventEmitError::DeliveryFailed)
    });
    let host = RuntimeHost::new(marked_fixture_config(&run_dir, &marker), sink);
    let app = register_commands(mock_builder())
        .manage(host)
        .build(mock_context(noop_assets()))
        .expect("mock Tauri app");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("mock webview");
    app_slot
        .set(app.handle().clone())
        .expect("single mock app handle");
    // This bounded channel only observes frames in the MockRuntime test. Its
    // 512 slots cover the current 1 MiB fake turn at the negotiated delta
    // size plus lifecycle frames; production backpressure remains enforced
    // by the bridge's own bounded event queues.
    let (event_sender, event_receiver) = mpsc::sync_channel(512);
    let _event_id = app.listen(RPC_FRAME_EVENT, move |event| {
        let payload = serde_json::from_str::<Value>(event.payload()).expect("event JSON");
        let _ = event_sender.try_send(payload);
    });
    let total_deadline = Instant::now() + Duration::from_secs(30);
    let invoke = |cmd: &str, body: Value| -> Result<Value, Value> {
        let request = InvokeRequest {
            cmd: cmd.to_owned(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: if cfg!(any(windows, target_os = "android")) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .expect("mock invoke URL"),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_owned(),
        };
        get_ipc_response(&webview, request).and_then(|body| {
            body.deserialize::<Value>()
                .map_err(|error| Value::String(error.to_string()))
        })
    };
    let configured: ja_lib::app_runtime::RuntimeConfigurationStatus = serde_json::from_value(
        invoke(
            "ja_runtime_configure",
            serde_json::json!({
                "input": {
                    "workspaceId": "ws_mock",
                    "rootPath": workspace_root.to_string_lossy(),
                    "trust": "trusted",
                    "settings": {
                        "schemaVersion": 1,
                        "activeProfileRevision": "profile_host",
                        "profiles": [{
                            "profileRevision": "profile_host",
                            "name": "Fixture",
                            "provider": "fixture",
                            "protocol": "open_ai_chat_completions",
                            "model": "fixture-model",
                            "baseUrl": "http://127.0.0.1:8080/v1",
                            "accessMode": "workspace",
                            "skillRevisions": [],
                            "mcpRevisions": []
                        }],
                        "mcpServers": []
                    }
                }
            }),
        )
        .expect("typed configure command"),
    )
    .expect("configuration response");
    assert!(configured.configured);
    let started: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_start", serde_json::json!({})).expect("typed start command"),
    )
    .expect("runtime start response");
    assert_eq!(started.status, RuntimeStatusKind::Ready);
    assert_process_marker_visible(&marker, total_deadline);
    let workspaces = invoke(
        "ja_workspace_list",
        serde_json::json!({"input": {"includeArchived": false}}),
    )
    .expect("typed workspace history command");
    assert!(
        workspaces["workspaces"]
            .as_array()
            .expect("workspace history rows")
            .iter()
            .any(|workspace| workspace["workspaceId"] == "ws_mock")
    );
    let created_thread = invoke(
        "ja_thread_create",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "title": "Tauri history"}
        }),
    )
    .expect("typed thread create command");
    let history_thread_id = created_thread["thread"]["threadId"]
        .as_str()
        .expect("created thread id")
        .to_owned();
    let listed_threads = invoke(
        "ja_thread_list",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "limit": 10, "cursor": ""}
        }),
    )
    .expect("typed thread list command");
    assert!(
        listed_threads["threads"]
            .as_array()
            .expect("thread history rows")
            .iter()
            .any(|thread| thread["threadId"] == history_thread_id)
    );
    let initial_read = invoke(
        "ja_thread_read",
        serde_json::json!({"input": {"threadId": history_thread_id.clone()}}),
    )
    .expect("typed initial thread read command");
    assert_eq!(
        initial_read["items"]
            .as_array()
            .expect("initial items")
            .len(),
        0
    );
    let tree = invoke(
        "ja_workspace_tree",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "relativePath": ""}
        }),
    )
    .expect("typed workspace tree command");
    assert!(
        tree["entries"]
            .as_array()
            .expect("tree entries")
            .iter()
            .any(|entry| entry["relativePath"] == "README.md")
    );
    let file = invoke(
        "ja_workspace_read_file",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "relativePath": "README.md"}
        }),
    )
    .expect("typed workspace read command");
    assert_eq!(
        file["text"],
        "hello from the Tauri command composition test\n"
    );
    let search = invoke(
        "ja_workspace_search",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "relativePath": "", "query": "hello"}
        }),
    )
    .expect("typed workspace search command");
    assert_eq!(search["hits"].as_array().expect("search hits").len(), 1);
    let status = invoke(
        "ja_git_status",
        serde_json::json!({"input": {"workspaceId": "ws_mock"}}),
    )
    .expect("typed git status command");
    assert!(status.is_array());
    let diff = invoke(
        "ja_git_diff",
        serde_json::json!({
            "input": {"workspaceId": "ws_mock", "staged": true}
        }),
    )
    .expect("typed git diff command");
    assert!(diff["bytes"].is_array());
    let ready = event_receiver
        .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
        .expect("ready event");
    assert_eq!(method(&ready), Some("runtime/statusChanged"));
    assert_no_token(&ready);
    let accepted: ja_lib::app_runtime::TurnAccepted = serde_json::from_value(
        invoke(
            "ja_turn_start",
            serde_json::json!({
                "input": {
                    "threadId": history_thread_id.clone(),
                    "accessMode": "workspace",
                    "profileRevision": "profile_host",
                    "input": [{"type": "text", "text": "hello from Tauri mock command"}]
                }
            }),
        )
        .expect("typed turn command"),
    )
    .expect("turn response");
    assert!(accepted.accepted);
    let mut completed = false;
    for _ in 0..16 {
        let event = event_receiver
            .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
            .expect("turn event");
        assert_no_token(&event);
        if method(&event) == Some("turn/completed") {
            completed = true;
            break;
        }
    }
    assert!(completed, "mock turn must complete through Tauri emitter");
    let completed_read = invoke(
        "ja_thread_read",
        serde_json::json!({"input": {
            "threadId": history_thread_id.clone(),
            "view": "snapshot"
        }}),
    )
    .expect("typed completed thread read command");
    assert!(
        completed_read["items"]
            .as_array()
            .expect("completed items")
            .len()
            >= 2
    );
    assert!(completed_read["snapshotSeq"].as_u64().unwrap_or(0) >= 2);
    assert!(
        Instant::now() < total_deadline,
        "mock turn exceeded total deadline"
    );
    let before_cancel: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_state", serde_json::json!({})).expect("state before cancel"),
    )
    .expect("state before cancel response");
    let cancel_input = "cancel me ".repeat(100_000);
    let cancel_accepted: ja_lib::app_runtime::TurnAccepted = serde_json::from_value(
        invoke(
            "ja_turn_start",
            serde_json::json!({
                "input": {
                    "threadId": "thr_cancel_mock",
                    "accessMode": "workspace",
                    "profileRevision": "profile_host",
                    "input": [{"type": "text", "text": cancel_input}]
                }
            }),
        )
        .expect("cancel fixture turn start"),
    )
    .expect("cancel fixture turn response");
    let cancel_ack: ja_lib::app_runtime::TurnCancelResult = serde_json::from_value(
        invoke(
            "ja_turn_cancel",
            serde_json::json!({
                "input": {
                    "threadId": "thr_cancel_mock",
                    "turnId": cancel_accepted.turn_id,
                    "reason": "test cancel"
                }
            }),
        )
        .expect("typed turn cancel command"),
    )
    .expect("cancel acknowledgement");
    assert!(cancel_ack.accepted);
    assert_eq!(cancel_ack.turn_id, cancel_accepted.turn_id);
    assert!(matches!(
        cancel_ack.status.as_str(),
        "interrupting" | "interrupted"
    ));
    assert_process_marker_visible(&marker, total_deadline);
    let after_cancel: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_state", serde_json::json!({})).expect("state after cancel"),
    )
    .expect("state after cancel response");
    assert_eq!(after_cancel.generation, before_cancel.generation);
    assert_eq!(
        after_cancel.server_instance_id,
        before_cancel.server_instance_id
    );
    let duplicate_cancel = invoke(
        "ja_turn_cancel",
        serde_json::json!({
            "input": {
                "threadId": "thr_cancel_mock",
                "turnId": cancel_accepted.turn_id,
                "reason": "duplicate test cancel"
            }
        }),
    );
    let after_duplicate: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_state", serde_json::json!({})).expect("state after duplicate cancel"),
    )
    .expect("state after duplicate cancel response");
    assert_eq!(after_duplicate.generation, before_cancel.generation);
    assert_eq!(
        after_duplicate.server_instance_id,
        before_cancel.server_instance_id
    );
    if let Ok(value) = duplicate_cancel {
        let duplicate_ack: ja_lib::app_runtime::TurnCancelResult =
            serde_json::from_value(value).expect("duplicate cancel acknowledgement");
        assert!(duplicate_ack.accepted);
        assert_eq!(duplicate_ack.turn_id, cancel_accepted.turn_id);
        assert!(matches!(
            duplicate_ack.status.as_str(),
            "interrupting" | "interrupted"
        ));
    }
    let mut interrupted = false;
    for _ in 0..32 {
        let event = event_receiver
            .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
            .expect("cancel terminal event");
        assert_no_token(&event);
        if method(&event) == Some("turn/completed")
            && event
                .get("params")
                .and_then(|params| params.get("terminalStatus"))
                .and_then(Value::as_str)
                == Some("interrupted")
        {
            interrupted = true;
            break;
        }
    }
    assert!(
        interrupted,
        "cancel fixture must publish interrupted completion"
    );
    let stopped: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_stop", serde_json::json!({})).expect("typed stop command"),
    )
    .expect("runtime stop response");
    assert_eq!(stopped.status, RuntimeStatusKind::Stopped);
    assert_process_tree_gone(&marker, total_deadline);
    cleanup_on_exit(&app.state::<RuntimeHost>()).expect("mock actor shutdown");
}

/// Proves recovery is a reachable desktop state: setup/window creation still
/// succeeds, typed start is blocked, and only the current identity/revision
/// acknowledgement unlocks lazy bridge creation.
#[cfg(feature = "tauri-smoke")]
#[test]
fn tauri_mock_recovery_gate_is_typed_and_lazy() {
    use ja_lib::app_runtime::{
        RPC_FRAME_EVENT, RuntimeRecoveryState, cleanup_on_exit, register_commands,
    };
    use tauri::Emitter;
    use tauri::Manager;
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
    use tauri::webview::InvokeRequest;

    let run_dir = TempRunDir::create("tauri-recovery");
    std::fs::write(
        run_dir.0.join("ja-runtime-recovery.json"),
        br#"{"schemaVersion":2,"status":"manual_recovery_required","recoveryId":"00000000-0000-4000-8000-000000000002","revision":9,"generation":1}"#,
    )
    .expect("recovery marker");
    let app_slot: Arc<OnceLock<tauri::AppHandle<tauri::test::MockRuntime>>> =
        Arc::new(OnceLock::new());
    let slot_for_sink = Arc::clone(&app_slot);
    let sink: EventSink = Arc::new(move |value| {
        let app = slot_for_sink.get().ok_or(EventEmitError::DeliveryFailed)?;
        app.emit(RPC_FRAME_EVENT, value)
            .map_err(|_| EventEmitError::DeliveryFailed)
    });
    let host = RuntimeHost::new(fixture_config(&run_dir), sink);
    let app = register_commands(mock_builder())
        .manage(host)
        .build(mock_context(noop_assets()))
        .expect("setup must succeed with recovery marker");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("window must be created while recovery is required");
    app_slot
        .set(app.handle().clone())
        .expect("single mock app handle");
    let invoke = |cmd: &str, body: Value| -> Result<Value, Value> {
        let request = InvokeRequest {
            cmd: cmd.to_owned(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: if cfg!(any(windows, target_os = "android")) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .expect("mock invoke URL"),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_owned(),
        };
        get_ipc_response(&webview, request).and_then(|body| {
            body.deserialize::<Value>()
                .map_err(|error| Value::String(error.to_string()))
        })
    };
    let state: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_state", serde_json::json!({})).expect("state command"),
    )
    .expect("state envelope");
    assert_eq!(state.status, RuntimeStatusKind::RecoveryRequired);
    let recovery: RuntimeRecoveryState = serde_json::from_value(
        invoke("ja_runtime_recovery_state", serde_json::json!({})).expect("recovery state"),
    )
    .expect("recovery projection");
    assert!(recovery.required && recovery.acknowledgeable);
    assert_eq!(
        recovery.recovery_id.as_deref(),
        Some("00000000-0000-4000-8000-000000000002")
    );
    assert_eq!(recovery.revision, Some(9));
    let blocked = invoke("ja_runtime_start", serde_json::json!({}))
        .expect_err("start must remain blocked before acknowledgement");
    assert!(blocked.to_string().contains("RECOVERY_REQUIRED"));
    let cleared: RuntimeRecoveryState = serde_json::from_value(
        invoke(
            "ja_runtime_acknowledge_recovery",
            serde_json::json!({
                "confirmation": {
                    "recoveryId": "00000000-0000-4000-8000-000000000002",
                    "revision": 9,
                    "reason": "ExternallyCleaned"
                }
            }),
        )
        .expect("typed recovery acknowledgement"),
    )
    .expect("ack response");
    assert!(!cleared.required);
    assert!(!run_dir.0.join("ja-runtime-recovery.json").exists());
    assert!(!run_dir.0.join("ja-runtime-recovery-ack.json").exists());
    let started: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_start", serde_json::json!({})).expect("lazy start after ack"),
    )
    .expect("start response");
    assert_eq!(started.status, RuntimeStatusKind::Ready);
    let stopped: RuntimeStatus = serde_json::from_value(
        invoke("ja_runtime_stop", serde_json::json!({})).expect("typed stop"),
    )
    .expect("stop response");
    assert_eq!(stopped.status, RuntimeStatusKind::Stopped);
    cleanup_on_exit(&app.state::<RuntimeHost>()).expect("recovery host cleanup");
}

/// Exercises the real MockRuntime exit-request path: a permanent cleanup
/// fault must prevent the first close, retain the Java owner as Crashed, and
/// allow a later close only after an explicit retry successfully reaps it.
#[cfg(feature = "tauri-smoke")]
#[test]
fn tauri_exit_request_denied_then_retry_allowed_with_quarantine() {
    use ja_lib::handle_exit_requested;
    use tauri::Manager;
    use tauri::RunEvent;
    use tauri::test::{mock_builder, mock_context, noop_assets};

    let run_dir = TempRunDir::create("exit-quarantine");
    let marker = format!("ja-exit-marker-{}", std::process::id());
    let sink: EventSink = Arc::new(|_| Ok(()));
    let shutdown_failures = Arc::new(AtomicUsize::new(usize::MAX));
    let host = RuntimeHost::new_for_exit_test(
        marked_fixture_config(&run_dir, &marker),
        sink,
        Duration::from_millis(500),
        Arc::clone(&shutdown_failures),
    );
    host.start().expect("sidecar ready");
    assert_process_marker_visible(&marker, Instant::now() + Duration::from_secs(10));
    let observer = host.clone();
    let app = mock_builder()
        .manage(host)
        .build(mock_context(noop_assets()))
        .expect("mock Tauri app");
    let first_window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("first mock window");
    let app_handle = app.handle().clone();
    let (event_sender, event_receiver) = mpsc::sync_channel(8);
    let (duration_sender, duration_receiver) = mpsc::sync_channel(4);
    let runner = std::thread::spawn(move || {
        app.run(move |app_handle, event| match event {
            RunEvent::Ready => {
                let _ = event_sender.send("ready");
            }
            RunEvent::ExitRequested { api, .. } => {
                let started = Instant::now();
                let host = app_handle.state::<RuntimeHost>();
                handle_exit_requested(&host, &api);
                let _ = duration_sender.send(started.elapsed());
                let _ = event_sender.send("exit-requested");
            }
            RunEvent::Exit => {
                let host = app_handle.state::<RuntimeHost>();
                let _ = event_sender.send(if host.exit_ready() {
                    "exit-ready"
                } else {
                    "exit-unsafe"
                });
            }
            _ => {}
        });
    });
    event_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("mock runtime ready");

    first_window.close().expect("first close request");
    assert_eq!(
        event_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("first exit request"),
        "exit-requested"
    );
    assert!(
        duration_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first exit duration")
            < Duration::from_millis(500),
        "ExitRequested must observe the shared short deadline without waiting on a default writer timeout"
    );
    let crash_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(state) = observer.state()
            && state.status == RuntimeStatusKind::Crashed
        {
            break;
        }
        assert!(
            Instant::now() < crash_deadline,
            "first exit was not quarantined"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        process_marker_visible(&marker),
        "permanent cleanup fault must retain the real Java owner"
    );

    shutdown_failures.store(0, std::sync::atomic::Ordering::Release);
    let retry_window = tauri::WebviewWindowBuilder::new(&app_handle, "retry", Default::default())
        .build()
        .expect("retry mock window");
    retry_window.close().expect("retry close request");
    assert_eq!(
        event_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("second exit request"),
        "exit-requested"
    );
    assert_eq!(
        event_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("allowed exit"),
        "exit-ready"
    );
    runner.join().expect("mock runtime exit");
    assert_process_tree_gone(&marker, Instant::now() + Duration::from_secs(10));
}

/// Proves Tauri-host actor -> Rust supervisor -> Java fake Turn and graceful
/// shutdown using the actual packaged JVM sidecar process.
#[test]
fn real_java_turn_and_shutdown_close_without_token_leak() {
    let run_dir = TempRunDir::create("turn");
    let marker = format!("ja-marker-{}", std::process::id());
    let total_deadline = Instant::now() + Duration::from_secs(30);
    let (sink, receiver) = event_sink();
    let bridge =
        RuntimeBridge::new(marked_fixture_config(&run_dir, &marker), sink).expect("bridge actor");
    let started = bridge.start().expect("sidecar ready");
    assert_eq!(started.status, RuntimeStatusKind::Ready);
    let ready_events = receive_until_at(&receiver, total_deadline, |value| {
        value
            .get("params")
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("ready")
    });
    assert!(ready_events.iter().any(|value| {
        value
            .get("params")
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("ready")
    }));

    let accepted = bridge
        .turn_start(valid_turn("hello from rust host"))
        .expect("fake turn accepted");
    assert!(accepted.accepted);
    let events = receive_until_at(&receiver, total_deadline, |value| {
        method(value) == Some("turn/completed")
    });
    let methods: Vec<&str> = events
        .iter()
        .filter_map(method)
        .filter(|name| name.starts_with("turn/") || name.starts_with("item/"))
        .collect();
    assert_eq!(
        methods,
        vec![
            "turn/started",
            "item/started",
            "item/delta",
            "item/completed",
            "turn/completed"
        ]
    );

    assert_process_marker_visible(&marker, total_deadline);
    let stopped = bridge.stop().expect("sidecar shutdown");
    assert_eq!(stopped.status, RuntimeStatusKind::Stopped);
    assert_process_tree_gone(&marker, total_deadline);
    let _ = receive_until_at(&receiver, total_deadline, |value| {
        value
            .get("params")
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("stopped")
    });
    bridge.shutdown().expect("actor shutdown");
}

/// Proves configuration replay reaches the real Java sidecar before the first
/// fake turn, while the host still owns all process and event cleanup.
#[test]
fn real_java_configuration_replay_activates_before_turn() {
    let run_dir = TempRunDir::create("replay");
    let backend = Arc::new(ReplaySecretFixture::default());
    let (sink, receiver) = event_sink();
    let host = RuntimeHost::new(
        production_replay_fixture_config(&run_dir, Arc::clone(&backend)),
        sink,
    );
    let configured = host
        .configure(RuntimeConfigureInput {
            workspace_id: "ws_replay".to_owned(),
            root_path: run_dir.0.to_string_lossy().into_owned(),
            display_name: None,
            trust: "trusted".to_owned(),
            settings: SettingsDocument {
                schema_version: 1,
                active_profile_revision: Some("profile_replay".to_owned()),
                profiles: vec![ProfileSetting {
                    profile_revision: "profile_replay".to_owned(),
                    name: "Replay".to_owned(),
                    provider: "fixture".to_owned(),
                    protocol: ApiProtocol::OpenAiChatCompletions,
                    model: "fixture-model".to_owned(),
                    base_url: Some("http://127.0.0.1:8080/v1".to_owned()),
                    credential_ref: Some(
                        CredentialRef::parse("cred_replay_fixture")
                            .expect("replay credential reference"),
                    ),
                    supports_vision: false,
                    access_mode: Default::default(),
                    skill_revisions: Vec::new(),
                    mcp_revisions: Some(Vec::new()),
                }],
                ..SettingsDocument::default()
            },
        })
        .expect("configuration replay snapshot");
    assert_eq!(configured.profile_revision, "profile_replay");
    let started = host.start().expect("configured sidecar ready");
    assert_eq!(started.status, RuntimeStatusKind::Ready);
    host.shutdown().expect("configured host shutdown");
    assert_eq!(backend.resolves.load(Ordering::SeqCst), 1);
    let events = receiver.try_iter().collect::<Vec<_>>();
    let event_text = serde_json::to_string(&events).expect("serialize observed events");
    assert!(!event_text.contains("fixture-secret"));
    assert!(
        events
            .iter()
            .all(|event| { event.get("method").and_then(Value::as_str) != Some("secret/resolve") })
    );
}

/// Holds a Windows sidecar before handshake to prove priority shutdown does
/// not wait for an unbounded protocol response or leave a child process.
#[cfg(windows)]
#[test]
fn slow_handshake_shutdown_has_one_total_deadline() {
    let run_dir = TempRunDir::create("slow-shutdown");
    let marker = format!("ja-slow-marker-{}", std::process::id());
    let total_deadline = Instant::now() + Duration::from_secs(30);
    let java = java_executable();
    let javac = java.parent().expect("Java bin directory").join("javac.exe");
    assert!(
        javac.is_file(),
        "JDK25 javac is required for slow shutdown test"
    );
    let source = run_dir.0.join("SlowHandshake.java");
    std::fs::write(
        &source,
        "public final class SlowHandshake { public static void main(String[] args) throws Exception { Thread.sleep(30000L); } }\n",
    )
    .expect("slow Java source");
    let compile = Command::new(&javac)
        .args([
            "-d",
            run_dir.0.to_str().expect("run directory"),
            source.to_str().expect("slow source"),
        ])
        .status()
        .expect("compile slow Java fixture");
    assert!(compile.success(), "slow Java fixture compilation failed");
    let args = vec![
        OsString::from(format!("-Dja.test.marker={marker}")),
        OsString::from("-cp"),
        OsString::from(run_dir.0.as_os_str()),
        OsString::from("SlowHandshake"),
    ];
    let (sink, receiver) = event_sink();
    let bridge = RuntimeBridge::new(LaunchConfig::for_test(java, args, run_dir.0.clone()), sink)
        .expect("bridge actor");
    let (starter_sender, starter_receiver) = mpsc::sync_channel(1);
    let starter_bridge = bridge.clone();
    let starter = std::thread::spawn(move || {
        let result = starter_bridge.start();
        let _ = starter_sender.send(result);
    });
    let starting = receive_until_at(&receiver, total_deadline, |value| {
        value
            .get("params")
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("starting")
    });
    if let Ok(result) = starter_receiver.try_recv() {
        panic!("slow starter completed before child marker: {result:?}");
    }
    assert!(starting.iter().any(|value| {
        value
            .get("params")
            .and_then(|params| params.get("status"))
            .and_then(Value::as_str)
            == Some("starting")
    }));
    assert_process_marker_visible(&marker, total_deadline);
    let shutdown_started = Instant::now();
    let shutdown_result = bridge.shutdown();
    assert!(
        shutdown_started.elapsed() < Duration::from_secs(20),
        "priority shutdown exceeded total deadline"
    );
    let starter_result = starter_receiver
        .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
        .expect("slow starter result before total deadline");
    assert!(
        starter_result.is_err(),
        "slow handshake must not report ready"
    );
    assert_process_tree_gone(&marker, total_deadline);
    join_with_deadline(starter, total_deadline).expect("slow starter join before total deadline");
    shutdown_result.expect("slow sidecar shutdown");
}

/// A recovery marker blocks a new sidecar before spawn; only explicit user
/// acknowledgement clears it, after which the normal Java25 path can start.
#[test]
fn recovery_marker_blocks_start_until_explicit_acknowledgement() {
    let run_dir = TempRunDir::create("recovery-gate");
    let marker = format!("ja-recovery-marker-{}", std::process::id());
    let recovery_path = run_dir.0.join("ja-runtime-recovery.json");
    std::fs::write(
        &recovery_path,
        br#"{"schemaVersion":2,"status":"manual_recovery_required","recoveryId":"00000000-0000-4000-8000-000000000001","revision":7,"generation":1}"#,
    )
    .expect("recovery marker");
    let sink: EventSink = Arc::new(|_| Ok(()));
    let config = marked_fixture_config(&run_dir, &marker);
    let host = RuntimeHost::new(config.clone(), sink);
    assert_eq!(
        host.state().expect("recovery state").status,
        RuntimeStatusKind::RecoveryRequired
    );
    let blocked = host.start().expect_err("marker must block startup");
    assert_eq!(blocked.code, "RECOVERY_REQUIRED");
    assert!(!process_marker_visible(&marker));

    config
        .acknowledge_manual_recovery(&ManualRecoveryConfirmation {
            recovery_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            revision: 7,
            reason: ManualRecoveryReason::ExternallyCleaned,
        })
        .expect("explicit recovery acknowledgement");
    assert!(!recovery_path.exists());
    let sink: EventSink = Arc::new(|_| Ok(()));
    let host = RuntimeHost::new(config, sink);
    host.start().expect("sidecar ready after acknowledgement");
    assert_process_marker_visible(&marker, Instant::now() + Duration::from_secs(10));
    host.stop().expect("sidecar stop");
    assert_process_tree_gone(&marker, Instant::now() + Duration::from_secs(10));
    host.shutdown().expect("actor shutdown");
}

/// Proves duplicate lifecycle calls are idempotent and a later start creates
/// a clean process rather than reusing a stale event pump.
#[test]
fn repeated_start_stop_is_bounded_and_idempotent() {
    let run_dir = TempRunDir::create("lifecycle");
    let sink: EventSink = Arc::new(|_| Ok(()));
    let bridge = RuntimeBridge::new(fixture_config(&run_dir), sink).expect("bridge actor");
    assert_eq!(
        bridge.start().expect("first start").status,
        RuntimeStatusKind::Ready
    );
    assert_eq!(
        bridge.start().expect("duplicate start").status,
        RuntimeStatusKind::Ready
    );
    assert_eq!(
        bridge.stop().expect("first stop").status,
        RuntimeStatusKind::Stopped
    );
    assert_eq!(
        bridge.stop().expect("duplicate stop").status,
        RuntimeStatusKind::Stopped
    );
    assert_eq!(
        bridge.start().expect("second generation").status,
        RuntimeStatusKind::Ready
    );
    assert_eq!(
        bridge.stop().expect("second stop").status,
        RuntimeStatusKind::Stopped
    );
    bridge.shutdown().expect("actor shutdown");
}

/// A launch failure remains a stable error and does not create an automatic
/// restart loop or retain any child process.
#[test]
fn launch_failure_does_not_crash_loop() {
    let run_dir = TempRunDir::create("failure");
    let missing = run_dir.0.join("missing-sidecar");
    let sink: EventSink = Arc::new(|_| Ok(()));
    let bridge = RuntimeBridge::new(
        LaunchConfig::for_test(missing, Vec::new(), run_dir.0.clone()),
        sink,
    )
    .expect("bridge actor");
    let first = bridge.start().expect_err("missing executable must fail");
    let second = bridge.start().expect_err("failure must remain explicit");
    assert_eq!(first.code, "RUNTIME_CONFIG_INVALID");
    assert_eq!(second.code, "RUNTIME_CONFIG_INVALID");
    assert_eq!(
        bridge.stop().expect("stop after failed start").status,
        RuntimeStatusKind::Stopped
    );
    bridge.shutdown().unwrap_or_else(|error| {
        panic!(
            "actor shutdown: {error:?}; actor phase={}",
            bridge.phase_for_test()
        )
    });

    let run_dir = TempRunDir::create("failure-admission");
    let missing = run_dir.0.join("missing-sidecar");
    let sink: EventSink = Arc::new(|_| Ok(()));
    let trace = BridgeTestTrace::new();
    let (bridge, admission) = RuntimeBridge::new_for_start_failure_test(
        LaunchConfig::for_test(missing, Vec::new(), run_dir.0.clone()),
        sink,
        &trace,
    )
    .expect("gated bridge actor");
    let total_deadline = Instant::now() + Duration::from_secs(10);
    let (start_sender, start_receiver) = mpsc::sync_channel(1);
    let starter_bridge = bridge.clone();
    let starter = std::thread::spawn(move || {
        let _ = start_sender.send(starter_bridge.start());
    });
    assert!(
        admission.wait_until_armed(total_deadline),
        "start command did not reach the post-admission barrier"
    );

    let (shutdown_sender, shutdown_receiver) = mpsc::sync_channel(1);
    let shutdown_bridge = bridge.clone();
    let shutdowner = std::thread::spawn(move || {
        let _ = shutdown_sender.send(shutdown_bridge.shutdown());
    });
    let shutdown_admitted = trace.wait_for("inner_shutdown_sent", total_deadline);
    admission.release();
    assert!(
        shutdown_admitted,
        "priority shutdown was not admitted; trace={:?}",
        trace.events()
    );

    let start_result = start_receiver
        .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
        .expect("failed start result before deadline")
        .expect_err("missing executable must fail");
    assert_eq!(start_result.code, "RUNTIME_CONFIG_INVALID");
    shutdown_receiver
        .recv_timeout(total_deadline.saturating_duration_since(Instant::now()))
        .expect("shutdown result before deadline")
        .expect("priority shutdown after failed start");
    join_with_deadline(starter, total_deadline).expect("starter joined");
    join_with_deadline(shutdowner, total_deadline).expect("shutdowner joined");
    let events = trace.events();
    let position = |name: &str| {
        events
            .iter()
            .position(|event| event == name)
            .unwrap_or_else(|| panic!("missing lifecycle event {name}: {events:?}"))
    };
    assert!(position("inner_shutdown_sent") < position("start_failure_gate_released"));
    assert!(position("start_supervisor_new_error") < position("actor_start_reply_err"));
    assert!(position("actor_start_reply_err") < position("actor_shutdown_received"));
    assert!(position("actor_shutdown_received") < position("actor_shutdown_confirmed"));
}
