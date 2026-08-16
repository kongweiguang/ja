// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Unit and native lifecycle tests for bounded host diagnostics.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::redaction::{safe_operation, safe_process};
use super::*;

/// Prove diagnostic output cannot echo a fixture path while retaining
/// the operation token needed to identify a missing Seatbelt rule.
#[test]
fn diagnostic_fields_are_path_safe() {
    assert_eq!(
        safe_operation(Some("workspace/secret".into()), ""),
        "redacted"
    );
    assert_eq!(
        safe_operation(Some("file-read-data".into()), ""),
        "file-read-data"
    );
    assert_eq!(
        safe_process(Some("sandbox-exec".into()), ""),
        "sandbox-exec"
    );
    assert_eq!(
        safe_process(Some("arbitrary-process".into()), ""),
        "redacted"
    );
}

/// Prove only denial-shaped records enter counters; ordinary log
/// records are discarded before any field extraction occurs.
#[test]
fn diagnostic_counts_only_denial_records() {
    let mut diagnostics = SandboxDenialDiagnostics::disabled();
    diagnostics.record_line(
        br#"{"operation":"file-read-data","category":"deny","process":"secret path"}"#,
    );
    diagnostics.record_line(br#"{"operation":"file-read-data","category":"allow"}"#);
    assert_eq!(diagnostics.events, 1);
    assert_eq!(diagnostics.counts.len(), 1);
    let key = diagnostics.counts.keys().next().expect("denial key");
    assert_eq!(key.operation, "file-read-data");
    assert_eq!(key.category, "sandbox-denial");
    assert_eq!(key.process, "redacted");
}

/// Prove the helper marker is owner-only and contains only the exact
/// identity needed by the outer workflow cleanup step.
#[test]
fn helper_marker_is_private_and_identity_only() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = env::temp_dir().join(format!(
        "ja-sandbox-marker-test-{}-{nonce}.marker",
        std::process::id()
    ));
    let owner_pid = std::process::id();
    write_prepared_marker(&path, owner_pid, nonce).expect("prepared marker");
    let prepared_mode = fs::metadata(&path)
        .expect("prepared marker metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(prepared_mode, 0o600);
    let contents =
        marker_contents(owner_pid, nonce, 1234, 5678, "test-start").expect("marker fields");
    write_marker_file(&path, &contents).expect("marker write");
    let mode = fs::metadata(&path)
        .expect("marker metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let content = fs::read_to_string(&path).expect("marker content");
    assert!(content.contains(&format!("owner_pid={owner_pid}\n")));
    assert!(content.contains(&format!("nonce={nonce}\n")));
    assert!(content.contains("pid=1234\npgid=5678\n"));
    assert!(content.contains("start_identity=test-start\n"));
    assert!(content.contains("executable_kind=log\nstate=active\n"));
    fs::remove_file(path).expect("marker cleanup");
}

/// Prove a same-filesystem hardlink cannot be reused as marker evidence: the
/// opened descriptor is rejected before truncation when its link count changes.
#[test]
fn marker_hardlink_is_rejected_before_write() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let primary = env::temp_dir().join(format!(
        "ja-sandbox-marker-hardlink-{}-{nonce}.marker",
        std::process::id()
    ));
    let alias = env::temp_dir().join(format!(
        "ja-sandbox-marker-hardlink-{}-{nonce}.alias",
        std::process::id()
    ));
    write_prepared_marker(&primary, std::process::id(), nonce).expect("prepared marker");
    fs::hard_link(&primary, &alias).expect("same filesystem hardlink");
    let contents = marker_contents(std::process::id(), nonce, 1234, 5678, "test-start")
        .expect("marker fields");
    assert!(write_marker_file(&primary, &contents).is_err());
    assert!(
        fs::read_to_string(&primary)
            .expect("prepared contents")
            .contains("state=prepared")
    );
    fs::remove_file(alias).expect("alias cleanup");
    fs::remove_file(primary).expect("primary cleanup");
}

/// Prove a symlink path is rejected by the no-follow marker open flags rather
/// than being followed to an unrelated regular file.
#[test]
fn marker_symlink_is_rejected() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let primary = env::temp_dir().join(format!(
        "ja-sandbox-marker-symlink-{}-{nonce}.marker",
        std::process::id()
    ));
    let link = env::temp_dir().join(format!(
        "ja-sandbox-marker-symlink-{}-{nonce}.link",
        std::process::id()
    ));
    write_prepared_marker(&primary, std::process::id(), nonce).expect("prepared marker");
    symlink(&primary, &link).expect("symlink fixture");
    let contents = marker_contents(std::process::id(), nonce, 1234, 5678, "test-start")
        .expect("marker fields");
    assert!(write_marker_file(&link, &contents).is_err());
    fs::remove_file(link).expect("link cleanup");
    fs::remove_file(primary).expect("primary cleanup");
}

/// Prove process start identity validation cannot inject another marker field
/// or newline into the workflow-owned cleanup evidence.
#[test]
fn marker_start_identity_rejects_field_injection() {
    assert!(marker_contents(7, 8, 9, 10, "bad=value").is_err());
    assert!(marker_contents(7, 8, 9, 10, "bad\nstate=active").is_err());
}

/// Spawn a real helper in its own group so native CI proves that marker
/// identity, group/direct-child signalling, and bounded reap share one
/// ownership path rather than only exercising pure state transitions.
#[cfg(target_os = "macos")]
fn controlled_helper_diagnostics() -> (
    SandboxDenialDiagnostics,
    u32,
    i32,
    PathBuf,
    PathBuf,
    PathBuf,
) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let owner_pid = std::process::id();
    let root = env::temp_dir();
    let primary = root.join(format!("ja-sandbox-lifecycle-{owner_pid}-{nonce}.marker"));
    let fallback = root.join(format!("ja-sandbox-lifecycle-{owner_pid}-{nonce}.fallback"));
    let emergency = root.join(format!(
        "ja-sandbox-lifecycle-{owner_pid}-{nonce}.emergency"
    ));
    let prepared = PreparedHelperMarker {
        path: primary.clone(),
        fallback_path: fallback.clone(),
        emergency_path: emergency.clone(),
        owner_pid,
        nonce,
    };
    write_prepared_marker(&primary, owner_pid, nonce).expect("prepared primary marker");
    write_prepared_marker(&fallback, owner_pid, nonce).expect("prepared fallback marker");
    write_prepared_marker(&emergency, owner_pid, nonce).expect("prepared emergency marker");

    let mut child = Command::new("/bin/sh")
        .args(["-c", "/bin/sleep 30 & wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("controlled helper spawn");
    let pid = child.id();
    let process_group = i32::try_from(pid).expect("helper pid fits process group");
    let start_identity = process_start_identity(pid).expect("helper start identity");
    activate_helper_marker(&prepared, pid, process_group, &start_identity)
        .expect("activate helper marker");
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let diagnostics = SandboxDenialDiagnostics {
        enabled: true,
        child: Some(child),
        process_group: Some(process_group),
        marker_path: Some(primary.clone()),
        fallback_marker_path: Some(fallback.clone()),
        emergency_marker_path: Some(emergency.clone()),
        stdout,
        stderr,
        line_buffer: Vec::new(),
        bytes: 0,
        events: 0,
        truncated: false,
        unavailable: None,
        cleanup_failed: false,
        read_error: None,
        pipes_nonblocking: false,
        group_empty: false,
        counts: BTreeMap::new(),
    };
    (
        diagnostics,
        pid,
        process_group,
        primary,
        fallback,
        emergency,
    )
}

/// Poll until both the direct child and its dedicated process group have
/// disappeared; this avoids treating signal delivery as successful reap.
#[cfg(target_os = "macos")]
fn helper_group_gone(pid: u32, process_group: i32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let child_gone =
            unsafe { kill(i32::try_from(pid).unwrap_or(-1), 0) == -1 && *__error() == ESRCH };
        let group_gone = unsafe { kill(-process_group, 0) == -1 && *__error() == ESRCH };
        if child_gone && group_gone {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Prove the normal bounded cleanup path reaps both direct child and
/// process group before marker removal is allowed.
#[cfg(target_os = "macos")]
#[test]
fn controlled_helper_group_lifecycle_reaps() {
    let (mut diagnostics, pid, process_group, primary, fallback, emergency) =
        controlled_helper_diagnostics();
    assert!(diagnostics.cleanup_child(false));
    assert!(diagnostics.child.is_none());
    assert!(diagnostics.remove_marker());
    assert!(helper_group_gone(pid, process_group));
    assert!(!primary.exists());
    assert!(!fallback.exists());
    assert!(!emergency.exists());
}

/// Prove Drop uses the same bounded group/direct-child reap contract when
/// the caller abandons diagnostics before an explicit finish call.
#[cfg(target_os = "macos")]
#[test]
fn controlled_helper_group_drop_reaps() {
    let (diagnostics, pid, process_group, primary, fallback, emergency) =
        controlled_helper_diagnostics();
    drop(diagnostics);
    assert!(helper_group_gone(pid, process_group));
    assert!(!primary.exists());
    assert!(!fallback.exists());
    assert!(!emergency.exists());
}

/// Prove Drop continues group cleanup after the direct shell has exited but a
/// background descendant still owns the dedicated process group.
#[cfg(target_os = "macos")]
#[test]
fn reaped_parent_does_not_skip_descendant_group_cleanup() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "/bin/sleep 30 & exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("descendant helper spawn");
    let pid = child.id();
    let process_group = i32::try_from(pid).expect("descendant group fits");
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => panic!("direct shell did not reap"),
        }
    }
    std::thread::sleep(Duration::from_millis(20));
    assert!(!helper_group_gone(pid, process_group));
    let diagnostics = SandboxDenialDiagnostics {
        enabled: true,
        child: None,
        process_group: Some(process_group),
        marker_path: None,
        fallback_marker_path: None,
        emergency_marker_path: None,
        stdout: None,
        stderr: None,
        line_buffer: Vec::new(),
        bytes: 0,
        events: 0,
        truncated: false,
        unavailable: None,
        cleanup_failed: false,
        read_error: None,
        pipes_nonblocking: false,
        group_empty: false,
        counts: BTreeMap::new(),
    };
    drop(diagnostics);
    assert!(helper_group_gone(pid, process_group));
}

/// Prove opt-in diagnostics can be tested without mutating global
/// environment state shared by parallel test workers.
#[test]
fn diagnostic_switch_is_pure() {
    assert!(diagnostics_enabled_from(Some("true"), None));
    assert!(diagnostics_enabled_from(None, Some("1")));
    assert!(!diagnostics_enabled_from(Some("0"), Some("false")));
}

/// Prove marker preparation rejects shared roots before a helper can be
/// spawned, preserving the owner-only marker invariant.
#[test]
fn marker_directory_rejects_shared_write_modes() {
    assert!(marker_directory_mode_safe(0o700));
    assert!(marker_directory_mode_safe(0o755));
    assert!(!marker_directory_mode_safe(0o702));
    assert!(!marker_directory_mode_safe(0o777));
}

/// Prove marker activation failure retains both prepared paths after finish
/// and Drop, allowing the workflow fallback gate to audit the failed owner.
#[test]
fn marker_activation_failure_retains_evidence() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let owner_pid = std::process::id();
    let primary = env::temp_dir().join(format!(
        "ja-sandbox-marker-failure-{owner_pid}-{nonce}.marker"
    ));
    let fallback = env::temp_dir().join(format!(
        "ja-sandbox-marker-failure-{owner_pid}-{nonce}.fallback"
    ));
    let emergency = env::temp_dir().join(format!(
        "ja-sandbox-marker-failure-{owner_pid}-{nonce}.emergency"
    ));
    write_prepared_marker(&primary, owner_pid, nonce).expect("prepared primary marker");
    write_prepared_marker(&fallback, owner_pid, nonce).expect("prepared fallback marker");
    write_prepared_marker(&emergency, owner_pid, nonce).expect("prepared emergency marker");
    let mut diagnostics = SandboxDenialDiagnostics {
        enabled: false,
        child: None,
        process_group: None,
        marker_path: Some(primary.clone()),
        fallback_marker_path: Some(fallback.clone()),
        emergency_marker_path: Some(emergency.clone()),
        stdout: None,
        stderr: None,
        line_buffer: Vec::new(),
        bytes: 0,
        events: 0,
        truncated: false,
        unavailable: Some("marker"),
        cleanup_failed: false,
        read_error: None,
        pipes_nonblocking: false,
        group_empty: true,
        counts: BTreeMap::new(),
    };
    assert!(diagnostics.finish().is_err());
    drop(diagnostics);
    assert!(primary.exists());
    assert!(fallback.exists());
    fs::remove_file(primary).expect("primary marker cleanup");
    fs::remove_file(fallback).expect("fallback marker cleanup");
    fs::remove_file(emergency).expect("emergency marker cleanup");
}

/// Prove a no-newline record is dropped immediately at its own bound,
/// before the aggregate byte budget can be consumed by raw data.
#[test]
fn diagnostic_line_cap_discards_raw_buffer() {
    let mut diagnostics = SandboxDenialDiagnostics::disabled();
    let mut reader = io::Cursor::new(vec![b'x'; DIAGNOSTIC_MAX_LINE_BYTES + 1]);
    diagnostics
        .drain_reader(&mut reader, true)
        .expect("cursor read");
    assert!(diagnostics.truncated);
    assert!(diagnostics.line_buffer.is_empty());
}

/// Prove the aggregate byte budget remains independent of line
/// framing and cannot grow after it is reached.
#[test]
fn diagnostic_byte_cap_is_hard() {
    let mut diagnostics = SandboxDenialDiagnostics::disabled();
    let mut reader = io::Cursor::new(vec![b'x'; DIAGNOSTIC_MAX_BYTES + 1]);
    diagnostics
        .drain_reader(&mut reader, false)
        .expect("cursor read");
    assert_eq!(diagnostics.bytes, DIAGNOSTIC_MAX_BYTES);
    assert!(diagnostics.truncated);
}

/// Prove read failures become a fixed state instead of being silently
/// ignored while the security cases continue to run.
#[test]
fn diagnostic_read_error_is_recorded() {
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("test read failure"))
        }
    }

    let mut diagnostics = SandboxDenialDiagnostics::disabled();
    let mut reader = FailingReader;
    diagnostics.pump_reader(&mut reader, true);
    assert_eq!(diagnostics.read_error, Some("io"));
    assert!(diagnostics.truncated);
    assert!(diagnostics.line_buffer.is_empty());
}

/// Prove a continuously-writing stderr-like reader cannot bypass its own
/// byte cap or absolute deadline while identity diagnostics are supervised.
#[test]
fn identity_query_continuous_reader_is_bounded() {
    struct ContinuousReader;

    impl Read for ContinuousReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    let mut reader = ContinuousReader;
    let mut bytes = 0;
    let deadline = std::time::Instant::now() + Duration::from_millis(100);
    assert!(!drain_query_pipe(
        &mut reader,
        None,
        &mut bytes,
        512,
        deadline
    ));
    assert!(bytes >= 512);
}

/// Prove both event and distinct-key caps are fail-closed before any
/// unbounded diagnostic map or event list can be built.
#[test]
fn diagnostic_event_and_key_caps_are_hard() {
    let mut events = SandboxDenialDiagnostics::disabled();
    for _ in 0..=DIAGNOSTIC_MAX_EVENTS {
        events.record_line(br#"{"operation":"signal","category":"deny"}"#);
    }
    assert!(events.truncated);
    assert_eq!(events.events, DIAGNOSTIC_MAX_EVENTS);

    let mut keys = SandboxDenialDiagnostics::disabled();
    for index in 0..DIAGNOSTIC_MAX_KEYS {
        keys.counts.insert(
            DiagnosticKey {
                operation: format!("fixture-{index}"),
                category: "sandbox-denial".into(),
                process: "log".into(),
            },
            1,
        );
    }
    keys.record_line(br#"{"operation":"signal","category":"deny"}"#);
    assert!(keys.truncated);
}
