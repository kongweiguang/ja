// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Deliberately boring native fixture.  Its value is that every assertion is
//! made by the Windows access check, not by a Rust policy function inside the
//! same process that is being tested.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    if let Err(error) = run() {
        eprintln!("fixture error: {error}");
        std::process::exit(17);
    }
}

/// Dispatch the small fixture modes used by the integration tests.  The
/// binary has no shell interpretation, so a test can reason about each OS
/// operation independently.
fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let mode = arg(&args, "--mode")?.unwrap_or_else(|| "smoke".into());
    match mode.as_str() {
        "smoke" => run_smoke(&args),
        "spawn-grandchild" => {
            let report = PathBuf::from(required_arg(&args, "--report")?);
            run_spawn_grandchild(&args, &report)
        }
        "spawn-grandchild-exit" => {
            let report = PathBuf::from(required_arg(&args, "--report")?);
            run_spawn_grandchild_exit(&args, &report)
        }
        "emit-output-grandchild" => {
            let report = PathBuf::from(required_arg(&args, "--report")?);
            run_emit_output_grandchild(&args, &report)
        }
        "idle" => {
            let report = PathBuf::from(required_arg(&args, "--report")?);
            run_idle(&args, &report)
        }
        _ => Err(format!("unknown mode: {mode}")),
    }
}

/// Check positive and negative filesystem/env/network cases and emit only
/// labels.  This prevents fixture reports from becoming a secret side channel.
fn run_smoke(args: &[String]) -> Result<(), String> {
    let workspace = PathBuf::from(required_arg(args, "--workspace")?);
    let outside = PathBuf::from(required_arg(args, "--outside")?);
    let dotdot = PathBuf::from(required_arg(args, "--dotdot")?);
    let secret = PathBuf::from(required_arg(args, "--secret")?);
    let product_db = PathBuf::from(required_arg(args, "--product-db")?);
    let workspace_exe = PathBuf::from(required_arg(args, "--workspace-exe")?);
    let resource_sibling = PathBuf::from(required_arg(args, "--resource-sibling")?);
    let network = required_arg(args, "--network")?;
    let external_network = required_arg(args, "--external-network")?;
    let parent_marker = required_arg(args, "--parent-marker")?;
    let expect_write = arg(args, "--expect-write")?.as_deref() != Some("false");
    let write_path = arg(args, "--write-path")?
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("worker-write.txt"));
    let allowed = workspace.join("allowed.txt");

    let mut lines = Vec::new();
    lines.push(format!("workspace-read={}", read_ok(&allowed)));
    let write_succeeded = if expect_write {
        write_ok(&write_path)
    } else {
        !write_denied(&write_path)
    };
    lines.push(format!("workspace-write={write_succeeded}"));
    lines.push(format!("outside-read={}", read_denied(&outside)));
    lines.push(format!("outside-write={}", write_denied(&outside)));
    lines.push(format!("product-db-read={}", read_denied(&product_db)));
    lines.push(format!("secret-read={}", read_denied(&secret)));
    lines.push(format!("dotdot-read={}", read_denied(&dotdot)));
    lines.push(format!("absolute-read={}", read_denied(&outside)));
    lines.push(format!(
        "workspace-execute-denied={}",
        execute_denied(&workspace_exe)
    ));
    lines.push(format!(
        "resource-sibling-read={}",
        read_denied(&resource_sibling)
    ));
    lines.push(format!(
        "parent-secret={}",
        env::var_os(&parent_marker).is_none()
    ));
    lines.push(format!("path-inherited={}", env::var_os("PATH").is_some()));
    lines.push(format!("network-denied={}", network_denied(&network)));
    lines.push(format!(
        "external-network-denied={}",
        network_denied(&external_network)
    ));
    write_stdout(&lines.join("\n"))
}

/// Start the same worker as a grandchild and wait for a host-controlled
/// release barrier.  The host can terminate the Job before release and prove
/// that the full tree, not just the parent, disappeared.
fn run_spawn_grandchild(args: &[String], report: &PathBuf) -> Result<(), String> {
    let release = PathBuf::from(required_arg(args, "--release")?);
    let child_report = PathBuf::from(required_arg(args, "--child-report")?);
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let child = Command::new(executable)
        .args(["--mode", "idle", "--report"])
        .arg(&child_report)
        .env_clear()
        .env("JA_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn grandchild: {error}"))?;
    write_report(
        report,
        &format!("grandchild-started=true\npid={}", child.id()),
    )?;
    wait_for_barrier(&release, Duration::from_secs(30))?;
    Ok(())
}

/// Start a grandchild that inherits the host pipe, then let the direct worker
/// exit normally; the host must still close the whole Job before joining its
/// stdout reader or the inherited handle would keep that reader blocked.
fn run_spawn_grandchild_exit(args: &[String], report: &PathBuf) -> Result<(), String> {
    let release = PathBuf::from(required_arg(args, "--release")?);
    let child_report = PathBuf::from(required_arg(args, "--child-report")?);
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let _child = Command::new(executable)
        .args(["--mode", "idle", "--report"])
        .arg(&child_report)
        .arg("--release")
        .arg(&release)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("spawn inheriting grandchild: {error}"))?;
    write_report(report, "grandchild-started=true")
}

/// Emit more than a normal bounded result while retaining a grandchild.  The
/// host must stop the Job when the bounded reader reports overflow, rather
/// than waiting for the worker's own barrier or leaking its descendant.
fn run_emit_output_grandchild(args: &[String], report: &PathBuf) -> Result<(), String> {
    let release = PathBuf::from(required_arg(args, "--release")?);
    let child_report = PathBuf::from(required_arg(args, "--child-report")?);
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let _child = Command::new(executable)
        .args(["--mode", "idle", "--report"])
        .arg(&child_report)
        .arg("--release")
        .arg(&release)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("spawn output grandchild: {error}"))?;
    write_report(report, "grandchild-started=true")?;
    let mut stdout = io::stdout().lock();
    let output = [b'J'; 128 * 1024];
    let _ = stdout.write_all(&output);
    let _ = stdout.flush();
    wait_for_barrier(&release, Duration::from_secs(30))
}

/// Write a child marker and hold until the parent/Job closes it.  It is kept
/// side-effect free so the process tree test cannot confuse fixture cleanup.
fn run_idle(_args: &[String], report: &PathBuf) -> Result<(), String> {
    write_report(report, &format!("idle=true\npid={}", std::process::id()))?;
    wait_for_barrier(&report.with_extension("release"), Duration::from_secs(30))
}

/// Read a file and report only whether the OS permitted it.
fn read_ok(path: &PathBuf) -> bool {
    fs::read(path).is_ok()
}

/// Write a bounded marker to the allowed workspace file.
fn write_ok(path: &PathBuf) -> bool {
    let created = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .and_then(|mut file| file.write_all(b"worker-write"))
        .is_ok();
    let renamed_path = path.with_extension("renamed");
    let renamed = created && fs::rename(path, &renamed_path).is_ok();
    let deleted = renamed && fs::remove_file(renamed_path).is_ok();
    created && renamed && deleted
}

/// A read is expected to fail outside the ACL grant.
fn read_denied(path: &PathBuf) -> bool {
    fs::read(path).is_err()
}

/// A write is expected to fail outside the ACL grant; no file is created by
/// the assertion itself because OpenOptions only runs as the AppContainer.
fn write_denied(path: &PathBuf) -> bool {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .is_err()
}

/// A workspace file is deliberately granted read/write but never execute; a
/// failed process creation proves the ACL did not propagate FILE_EXECUTE.
fn execute_denied(path: &PathBuf) -> bool {
    match Command::new(path)
        .args(["--mode", "idle", "--report", "workspace-exec.report"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
        Err(_) => true,
    }
}

/// Test a loopback endpoint with a short deadline; AppContainer without a
/// network capability must reject both loopback and external sockets.
fn network_denied(address: &str) -> bool {
    let parsed = address.parse::<SocketAddr>();
    let Ok(parsed) = parsed else { return false };
    TcpStream::connect_timeout(&parsed, Duration::from_millis(500)).is_err()
}

/// Wait on a file barrier using a monotonic deadline, avoiding fixed sleeps in
/// the test protocol and making timeout a visible fixture failure.
fn wait_for_barrier(path: &std::path::Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        thread::park_timeout(Duration::from_millis(10));
    }
    Err("barrier timeout".into())
}

/// Atomically replace a small report only within the fixture's granted root.
fn write_report(path: &PathBuf, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|error| error.to_string())
}

/// Send bounded fixture results over the host-owned pipe so read-only
/// workspaces never need a writable report file or a broader ACL grant.
fn write_stdout(content: &str) -> Result<(), String> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(content.as_bytes())
        .and_then(|_| stdout.flush())
        .map_err(|error| error.to_string())
}

/// Return an optional flag value while rejecting an accidental value that
/// consumes the next option; all fixture inputs remain explicit.
fn arg(args: &[String], name: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|value| value == name) else {
        return Ok(None);
    };
    args.get(index + 1)
        .cloned()
        .filter(|value| !value.starts_with('-'))
        .map(Some)
        .ok_or_else(|| format!("missing value for {name}"))
}

/// Require a fixture argument so a malformed invocation fails closed rather
/// than silently testing a default path.
fn required_arg(args: &[String], name: &str) -> Result<String, String> {
    arg(args, name)?.ok_or_else(|| format!("missing {name}"))
}
