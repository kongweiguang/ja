// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Manual Windows 11 acceptance harness.  It intentionally exits non-zero on
//! skipped privilege/API cases because a policy-only or best-effort sandbox is
//! unsafe to call a passing production result.

use ja_windows_sandbox_spike::{
    SandboxError, SandboxSpec, WorkspaceAccess, acl_fingerprint, process_is_alive,
    reject_reparse_path, spawn,
};
use std::env;
use std::fs;
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    if let Err(error) = run() {
        eprintln!("SANDBOX-BLOCKED: {error}");
        std::process::exit(2);
    }
    println!(
        "SANDBOX-PASS: AppContainer, ACL, network denial, environment and Job tree checks passed"
    );
}

/// Run all smoke and process-tree assertions from a temporary Unicode/space
/// directory, then remove the exact directory after every native handle closes.
fn run() -> Result<(), String> {
    let root = temporary_root()?;
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let result = run_fixture(&root);
    let cleanup = remove_tree(&root);
    result.and(cleanup)
}

/// Build the fixture graph and run positive/negative checks under the real
/// AppContainer token.  No production workspace or user ACL is touched.
fn run_fixture(root: &Path) -> Result<(), String> {
    let workspace = root.join("允许 workspace 空格");
    let resource = root.join("JA native resources");
    let outside = root.join("outside sibling.txt");
    let outside_dir = root.join("outside sibling dir");
    let dotdot = root.join("outside.txt");
    let product_db = root.join("ja product.db");
    let secret = root.join("ja secret marker.txt");
    let symlink = workspace.join("outside-link.txt");
    let hardlink = workspace.join("outside-hardlink.txt");
    let junction = workspace.join("outside-junction");
    let worker = resource.join("ja-sandbox-worker.exe");
    let resource_sibling = resource.join("native-sibling-secret.txt");
    let workspace_exe = workspace.join("workspace-worker.exe");
    fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    fs::create_dir_all(&resource).map_err(|error| error.to_string())?;
    fs::create_dir_all(&outside_dir).map_err(|error| error.to_string())?;
    fs::write(workspace.join("allowed.txt"), "allowed").map_err(|error| error.to_string())?;
    fs::write(&outside, "outside").map_err(|error| error.to_string())?;
    fs::write(outside_dir.join("escape.txt"), "junction outside")
        .map_err(|error| error.to_string())?;
    fs::write(&dotdot, "dotdot outside").map_err(|error| error.to_string())?;
    fs::write(&product_db, "database").map_err(|error| error.to_string())?;
    fs::write(&secret, "JA_PARENT_SECRET_VALUE").map_err(|error| error.to_string())?;
    fs::copy(worker_binary()?, &worker).map_err(|error| error.to_string())?;
    fs::write(&resource_sibling, "native sibling secret").map_err(|error| error.to_string())?;
    fs::copy(worker_binary()?, &workspace_exe).map_err(|error| error.to_string())?;
    let links = create_link_fixtures(&outside, &symlink, &hardlink, &junction, &outside_dir)?;
    if !links.symlink_available && links.symlink_error == 0 {
        return Err("symlink fixture reported unavailable without an OS error".into());
    }
    if !links.symlink_available {
        eprintln!(
            "symlink fixture unavailable; CreateSymbolicLinkW error={}",
            links.symlink_error
        );
    }
    if reject_reparse_path(&junction).is_ok() {
        return Err("reparse preflight accepted a junction path".into());
    }

    let outside_before = fs::read(&outside).map_err(|error| error.to_string())?;
    let outside_acl = acl_fingerprint(&outside_dir).map_err(|error| error.to_string())?;
    let acl_snapshot = AclSnapshot::capture(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    let mut escape_spec = SandboxSpec::denied_network(&worker, &workspace);
    add_os_baseline(&mut escape_spec);
    assert_escape_preflight_rejects(
        escape_spec.clone(),
        &outside,
        &outside_before,
        &outside_dir,
        outside_acl,
    )?;
    fs::remove_file(&hardlink).map_err(|error| error.to_string())?;
    assert_escape_preflight_rejects(
        escape_spec,
        &outside,
        &outside_before,
        &outside_dir,
        outside_acl,
    )?;
    remove_reparse_entries(&symlink, &junction, links.symlink_available)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let accept_thread = thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if listener.accept().is_ok() {
                return true;
            }
            thread::park_timeout(Duration::from_millis(10));
        }
        false
    });
    let marker = format!("JA_PARENT_SECRET_{}", std::process::id());
    // SAFETY: the probe owns this unique marker and restores it before return.
    unsafe { env::set_var(&marker, "JA_PARENT_SECRET_VALUE") };
    let _marker_guard = EnvGuard::new(marker.clone());
    let mut spec = SandboxSpec::denied_network(&worker, &workspace);
    add_os_baseline(&mut spec);
    spec.args = vec![
        "--mode".into(),
        "smoke".into(),
        "--workspace".into(),
        workspace.clone().into_os_string(),
        "--outside".into(),
        outside.clone().into_os_string(),
        "--dotdot".into(),
        dotdot.clone().into_os_string(),
        "--product-db".into(),
        product_db.clone().into_os_string(),
        "--secret".into(),
        secret.clone().into_os_string(),
        "--workspace-exe".into(),
        workspace_exe.clone().into_os_string(),
        "--resource-sibling".into(),
        resource_sibling.clone().into_os_string(),
        "--network".into(),
        address.to_string().into(),
        "--external-network".into(),
        "192.0.2.1:443".into(),
        "--parent-marker".into(),
        marker.into(),
    ];
    spec.env.insert("JA_WORKER_MODE".into(), "smoke".into());
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    let (outcome, stdout) = child
        .wait_with_stdout(Duration::from_secs(10), 64 * 1024)
        .map_err(|error| error.to_string())?;
    if outcome.timed_out || outcome.exit_code != Some(0) {
        return Err(format!("smoke worker outcome: {outcome:?}"));
    }
    let report_text = String::from_utf8(stdout).map_err(|error| error.to_string())?;
    assert_report(&report_text, true)?;
    drop(child);
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    if accept_thread.join().unwrap_or(true) {
        return Err("loopback listener observed a connection".into());
    }
    run_read_only_fixture(&workspace, &worker, &workspace_exe, &resource_sibling)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    run_tree_fixture(&workspace, &worker)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    run_tree_exit_fixture(&workspace, &worker)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    run_output_bound_parameter_fixture(&workspace, &worker)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    run_output_overflow_fixture(&workspace, &worker)?;
    acl_snapshot.assert_unchanged(
        &workspace,
        &workspace.join("allowed.txt"),
        &workspace_exe,
        &resource,
        &worker,
        &resource_sibling,
    )?;
    let restored_outside_acl = acl_fingerprint(&outside_dir).map_err(|error| error.to_string())?;
    if restored_outside_acl != outside_acl {
        return Err("junction target security descriptor changed".into());
    }
    let outside_after = fs::read(&outside).map_err(|error| error.to_string())?;
    if outside_after != outside_before {
        return Err("outside file content changed during clean workspace runs".into());
    }
    for generated in [
        workspace.join("tree.report"),
        workspace.join("grandchild.report"),
        workspace.join("read-only-attempt.txt"),
        workspace.join("worker-write.txt"),
        workspace.join("worker-write.renamed"),
    ] {
        if generated.exists() {
            return Err(format!(
                "fixture left generated file: {}",
                generated.display()
            ));
        }
    }
    Ok(())
}

/// Snapshot every ACL touched by the worker so each child lifetime proves its
/// guards restore both workspace and independent native-resource state.
struct AclSnapshot {
    workspace: u64,
    workspace_file: u64,
    workspace_exe: u64,
    resource_dir: u64,
    worker: u64,
    resource_sibling: u64,
}

impl AclSnapshot {
    /// Capture DACL fingerprints before any AppContainer profile is created.
    fn capture(
        workspace: &Path,
        workspace_file: &Path,
        workspace_exe: &Path,
        resource_dir: &Path,
        worker: &Path,
        resource_sibling: &Path,
    ) -> Result<Self, String> {
        Ok(Self {
            workspace: acl_fingerprint(workspace).map_err(|error| error.to_string())?,
            workspace_file: acl_fingerprint(workspace_file).map_err(|error| error.to_string())?,
            workspace_exe: acl_fingerprint(workspace_exe).map_err(|error| error.to_string())?,
            resource_dir: acl_fingerprint(resource_dir).map_err(|error| error.to_string())?,
            worker: acl_fingerprint(worker).map_err(|error| error.to_string())?,
            resource_sibling: acl_fingerprint(resource_sibling)
                .map_err(|error| error.to_string())?,
        })
    }

    /// Compare all captured descriptors after a child has dropped its guards;
    /// one mismatch fails the fixture instead of accepting partial rollback.
    fn assert_unchanged(
        &self,
        workspace: &Path,
        workspace_file: &Path,
        workspace_exe: &Path,
        resource_dir: &Path,
        worker: &Path,
        resource_sibling: &Path,
    ) -> Result<(), String> {
        let current = Self::capture(
            workspace,
            workspace_file,
            workspace_exe,
            resource_dir,
            worker,
            resource_sibling,
        )?;
        let checks = [
            ("workspace", self.workspace, current.workspace),
            (
                "workspace file",
                self.workspace_file,
                current.workspace_file,
            ),
            (
                "workspace executable",
                self.workspace_exe,
                current.workspace_exe,
            ),
            (
                "resource directory",
                self.resource_dir,
                current.resource_dir,
            ),
            ("worker resource", self.worker, current.worker),
            (
                "resource sibling",
                self.resource_sibling,
                current.resource_sibling,
            ),
        ];
        for (label, expected, actual) in checks {
            if expected != actual {
                return Err(format!("{label} ACL was not restored"));
            }
        }
        Ok(())
    }
}

/// Prove that unsafe descendants are rejected before profile/ACL setup, so a
/// failed preflight cannot mutate the hardlink inode or junction target.
fn assert_escape_preflight_rejects(
    spec: SandboxSpec,
    outside: &Path,
    outside_before: &[u8],
    outside_dir: &Path,
    outside_acl: u64,
) -> Result<(), String> {
    match spawn(spec) {
        Err(SandboxError::InvalidConfig(_)) => {}
        Err(error) => return Err(format!("escape preflight returned wrong error: {error}")),
        Ok(mut child) => {
            let _ = child.terminate_tree();
            return Err("escape workspace passed hardlink/reparse preflight".into());
        }
    }
    let outside_after = fs::read(outside).map_err(|error| error.to_string())?;
    if outside_after != outside_before {
        return Err("outside hardlink content changed during rejected preflight".into());
    }
    let target_after =
        fs::read(outside_dir.join("escape.txt")).map_err(|error| error.to_string())?;
    if target_after != b"junction outside" {
        return Err("junction target content changed during rejected preflight".into());
    }
    if acl_fingerprint(outside_dir).map_err(|error| error.to_string())? != outside_acl {
        return Err("junction target DACL changed during rejected preflight".into());
    }
    Ok(())
}

/// Remove only the reparse link nodes after their rejection has been observed;
/// the target directory is never traversed or recursively deleted by cleanup.
fn remove_reparse_entries(
    symlink: &Path,
    junction: &Path,
    symlink_available: bool,
) -> Result<(), String> {
    if symlink_available {
        fs::remove_file(symlink).map_err(|error| error.to_string())?;
    }
    fs::remove_dir(junction).map_err(|error| error.to_string())?;
    Ok(())
}

/// A second launch proves that read-only workspace mode is real and is not
/// silently widened to the writable default used by the smoke case.
fn run_read_only_fixture(
    workspace: &Path,
    worker: &Path,
    workspace_exe: &Path,
    resource_sibling: &Path,
) -> Result<(), String> {
    let attempted_write = workspace.join("read-only-attempt.txt");
    let mut spec = SandboxSpec::denied_network(worker, workspace);
    add_os_baseline(&mut spec);
    spec.workspace_access = WorkspaceAccess::ReadOnly;
    spec.args = vec![
        "--mode".into(),
        "smoke".into(),
        "--workspace".into(),
        workspace.to_path_buf().into_os_string(),
        "--outside".into(),
        workspace.join("..\\outside sibling.txt").into_os_string(),
        "--dotdot".into(),
        workspace.join("..\\outside.txt").into_os_string(),
        "--product-db".into(),
        workspace.join("..\\ja product.db").into_os_string(),
        "--secret".into(),
        workspace.join("..\\ja secret marker.txt").into_os_string(),
        "--workspace-exe".into(),
        workspace_exe.to_path_buf().into_os_string(),
        "--resource-sibling".into(),
        resource_sibling.to_path_buf().into_os_string(),
        "--network".into(),
        "127.0.0.1:9".into(),
        "--external-network".into(),
        "192.0.2.1:443".into(),
        "--parent-marker".into(),
        "JA_PARENT_SECRET_NOT_PRESENT".into(),
        "--expect-write".into(),
        "false".into(),
        "--write-path".into(),
        attempted_write.clone().into_os_string(),
    ];
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    let (outcome, stdout) = child
        .wait_with_stdout(Duration::from_secs(10), 64 * 1024)
        .map_err(|error| error.to_string())?;
    if outcome.timed_out || outcome.exit_code != Some(0) {
        return Err(format!("read-only worker outcome: {outcome:?}"));
    }
    if attempted_write.exists() {
        return Err("read-only worker created a write probe file".into());
    }
    let report_text = String::from_utf8(stdout).map_err(|error| error.to_string())?;
    assert_report(&report_text, false)?;
    drop(child);
    Ok(())
}

/// Exercise Job Object kill-on-close using a child that intentionally remains
/// behind a barrier until the host cancels it.
fn run_tree_fixture(workspace: &Path, worker: &Path) -> Result<(), String> {
    let report = workspace.join("tree.report");
    let child_report = workspace.join("grandchild.report");
    let release = workspace.join("tree.release.never-created");
    let mut spec = SandboxSpec::denied_network(worker, workspace);
    add_os_baseline(&mut spec);
    spec.args = vec![
        "--mode".into(),
        "spawn-grandchild".into(),
        "--report".into(),
        report.clone().into_os_string(),
        "--release".into(),
        release.into_os_string(),
        "--child-report".into(),
        child_report.clone().into_os_string(),
    ];
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    wait_for_file(&report, Duration::from_secs(3))?;
    wait_for_file(&child_report, Duration::from_secs(3))?;
    let (outcome, _stdout) = child
        .wait_with_stdout(Duration::from_millis(50), 64 * 1024)
        .map_err(|error| error.to_string())?;
    if !outcome.timed_out {
        return Err(format!(
            "tree fixture exited before cancellation: {outcome:?}"
        ));
    }
    if !report_text_contains(&report, "grandchild-started=true")? {
        return Err("tree fixture did not start grandchild".into());
    }
    let grandchild_pid = read_pid(&child_report)?;
    drop(child);
    wait_for_process_exit(grandchild_pid, Duration::from_secs(3))?;
    remove_fixture_file(&report)?;
    remove_fixture_file(&child_report)?;
    Ok(())
}

/// Verify that a normally exiting direct worker still closes its Job before
/// joining stdout, even while its grandchild retains the inherited pipe.
fn run_tree_exit_fixture(workspace: &Path, worker: &Path) -> Result<(), String> {
    let report = workspace.join("tree-exit.report");
    let child_report = workspace.join("grandchild-exit.report");
    let release = workspace.join("tree-exit.release.never-created");
    let mut spec = SandboxSpec::denied_network(worker, workspace);
    add_os_baseline(&mut spec);
    spec.args = vec![
        "--mode".into(),
        "spawn-grandchild-exit".into(),
        "--report".into(),
        report.clone().into_os_string(),
        "--release".into(),
        release.into_os_string(),
        "--child-report".into(),
        child_report.clone().into_os_string(),
    ];
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    wait_for_file(&report, Duration::from_secs(3))?;
    wait_for_file(&child_report, Duration::from_secs(3))?;
    let (outcome, _stdout) = child
        .wait_with_stdout(Duration::from_secs(2), 64 * 1024)
        .map_err(|error| error.to_string())?;
    if outcome.timed_out || outcome.exit_code != Some(0) {
        return Err(format!(
            "normal-exit grandchild fixture outcome: {outcome:?}"
        ));
    }
    let grandchild_pid = read_pid(&child_report)?;
    wait_for_process_exit(grandchild_pid, Duration::from_secs(3))?;
    drop(child);
    remove_fixture_file(&report)?;
    remove_fixture_file(&child_report)?;
    Ok(())
}

/// Reject an untrusted stdout limit before reading and close the complete Job
/// immediately, so a caller cannot keep a child alive with an unsafe bound.
fn run_output_bound_parameter_fixture(workspace: &Path, worker: &Path) -> Result<(), String> {
    let report = workspace.join("output-bound.report");
    let child_report = workspace.join("output-bound-grandchild.report");
    let release = workspace.join("output-bound.release.never-created");
    let mut spec = SandboxSpec::denied_network(worker, workspace);
    add_os_baseline(&mut spec);
    spec.args = vec![
        "--mode".into(),
        "spawn-grandchild".into(),
        "--report".into(),
        report.clone().into_os_string(),
        "--release".into(),
        release.into_os_string(),
        "--child-report".into(),
        child_report.clone().into_os_string(),
    ];
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    wait_for_file(&report, Duration::from_secs(3))?;
    wait_for_file(&child_report, Duration::from_secs(3))?;
    let grandchild_pid = read_pid(&child_report)?;
    match child.wait_with_stdout(Duration::from_secs(2), 2 * 1024 * 1024) {
        Err(SandboxError::InvalidConfig(message)) if message.contains("1 MiB") => {}
        Err(error) => return Err(format!("wrong stdout-bound error: {error}")),
        Ok(outcome) => return Err(format!("unsafe stdout bound was accepted: {outcome:?}")),
    }
    wait_for_process_exit(grandchild_pid, Duration::from_secs(3))?;
    drop(child);
    remove_fixture_file(&report)?;
    remove_fixture_file(&child_report)?;
    Ok(())
}

/// Feed more than the caller limit while a grandchild remains alive; an
/// overflow must return by deadline and terminate that whole process tree.
fn run_output_overflow_fixture(workspace: &Path, worker: &Path) -> Result<(), String> {
    let report = workspace.join("output-overflow.report");
    let child_report = workspace.join("output-overflow-grandchild.report");
    let release = workspace.join("output-overflow.release.never-created");
    let mut spec = SandboxSpec::denied_network(worker, workspace);
    add_os_baseline(&mut spec);
    spec.args = vec![
        "--mode".into(),
        "emit-output-grandchild".into(),
        "--report".into(),
        report.clone().into_os_string(),
        "--release".into(),
        release.into_os_string(),
        "--child-report".into(),
        child_report.clone().into_os_string(),
    ];
    let mut child = spawn(spec).map_err(|error| error.to_string())?;
    wait_for_file(&report, Duration::from_secs(3))?;
    wait_for_file(&child_report, Duration::from_secs(3))?;
    let grandchild_pid = read_pid(&child_report)?;
    match child.wait_with_stdout(Duration::from_secs(2), 64 * 1024) {
        Err(SandboxError::InvalidConfig(message)) if message.contains("exceeded") => {}
        Err(error) => return Err(format!("wrong stdout-overflow error: {error}")),
        Ok(outcome) => return Err(format!("stdout overflow was accepted: {outcome:?}")),
    }
    wait_for_process_exit(grandchild_pid, Duration::from_secs(3))?;
    drop(child);
    remove_fixture_file(&report)?;
    remove_fixture_file(&child_report)?;
    Ok(())
}

/// Remove only a known fixture report after its worker and descendants are
/// gone; cleanup failures stay visible instead of hiding ACL residue.
fn remove_fixture_file(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Assert every OS operation expected from the denied-network profile.
fn assert_report(report: &str, expect_write: bool) -> Result<(), String> {
    let expected = [
        "workspace-read=true",
        "outside-read=true",
        "outside-write=true",
        "product-db-read=true",
        "secret-read=true",
        "dotdot-read=true",
        "absolute-read=true",
        "workspace-execute-denied=true",
        "resource-sibling-read=true",
        "parent-secret=true",
        "path-inherited=false",
        "network-denied=true",
        "external-network-denied=true",
    ];
    for line in expected {
        if !report.lines().any(|actual| actual == line) {
            return Err(format!("missing report assertion: {line}"));
        }
    }
    if expect_write {
        if !report.lines().any(|line| line == "workspace-write=true") {
            return Err("writable workspace did not permit the write".into());
        }
    } else if !report.lines().any(|line| line == "workspace-write=false") {
        return Err("read-only workspace permitted a write".into());
    }
    if report.contains("JA_PARENT_SECRET_VALUE") {
        return Err("secret marker appeared in worker report".into());
    }
    Ok(())
}

/// Link setup records symlink privilege diagnostics but requires a directory
/// junction, which is available to ordinary Windows users without Developer
/// Mode on the supported preview hosts.
struct LinkFixture {
    symlink_available: bool,
    symlink_error: i32,
}

/// Create symlink/hardlink/junction candidates.  Junction creation is the
/// mandatory reparse escape vector; silently omitting it is always a failure.
fn create_link_fixtures(
    outside: &Path,
    symlink: &Path,
    hardlink: &Path,
    junction: &Path,
    outside_dir: &Path,
) -> Result<LinkFixture, String> {
    let (symlink_available, symlink_error) =
        match std::os::windows::fs::symlink_file(outside, symlink) {
            Ok(()) => (true, 0),
            Err(error) => (false, error.raw_os_error().unwrap_or(-1)),
        };
    fs::hard_link(outside, hardlink)
        .map_err(|error| format!("hardlink fixture unavailable: {error}"))?;
    let system_root = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| "SystemRoot is unavailable for junction fixture".to_string())?;
    let cmd = system_root.join("System32").join("cmd.exe");
    if !cmd.is_file() {
        return Err("fixed System32\\cmd.exe is unavailable for junction fixture".into());
    }
    let junction_text = junction.to_string_lossy();
    let outside_text = outside_dir.to_string_lossy();
    if [junction_text.as_ref(), outside_text.as_ref()]
        .into_iter()
        .any(|path| {
            path.chars()
                .any(|character| "\"&|<>%\r\n".contains(character))
        })
    {
        return Err("junction fixture path contains a cmd metacharacter".into());
    }
    let command = format!(" mklink /J \"{junction_text}\" \"{outside_text}\"");
    let mut junction_process = std::process::Command::new(&cmd);
    junction_process
        .args(["/d", "/s", "/c"])
        .raw_arg(command)
        .creation_flags(0x0800_0000);
    let output = junction_process
        .output()
        .map_err(|error| format!("junction command failed to start: {error}"))?;
    if !output.status.success() || !junction.is_dir() {
        return Err(format!(
            "junction creation failed code={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(LinkFixture {
        symlink_available,
        symlink_error,
    })
}

/// Locate the sibling worker emitted by Cargo; copying it into the granted
/// workspace proves the AppContainer can execute only the fixture we allow.
fn worker_binary() -> Result<PathBuf, String> {
    let mut path = env::current_exe().map_err(|error| error.to_string())?;
    path.set_file_name("ja-sandbox-worker.exe");
    if path.is_file() {
        Ok(path)
    } else {
        Err("ja-sandbox-worker.exe is not next to the probe".into())
    }
}

/// Wait for a real fixture barrier with a deadline instead of sleeping a
/// guessed duration and thereby hiding scheduling/ACL failures.
fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        thread::park_timeout(Duration::from_millis(10));
    }
    Err(format!("barrier not reached: {}", path.display()))
}

/// Read a report only after its host-controlled existence barrier is observed.
fn report_text_contains(path: &Path, expected: &str) -> Result<bool, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    Ok(text.lines().any(|line| line == expected))
}

/// Parse the child PID emitted by the fixture so cleanup is checked against a
/// real process handle rather than a process-name guess.
fn read_pid(path: &Path) -> Result<u32, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .ok_or_else(|| "grandchild report omitted pid".to_string())?;
    value
        .parse::<u32>()
        .map_err(|error| format!("invalid child pid: {error}"))
}

/// Confirm the Job Object killed the descendant before the fixture directory
/// is removed, using a monotonic deadline and no arbitrary fixed sleep.
fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return Ok(());
        }
        thread::park_timeout(Duration::from_millis(10));
    }
    Err(format!("grandchild process {pid} survived Job termination"))
}

/// Use a collision-resistant Unicode/space directory while retaining a narrow
/// exact cleanup target that can be checked after all handles are closed.
fn temporary_root() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    Ok(env::temp_dir().join(format!(
        "JA sandbox 中文 空格 {} {}",
        std::process::id(),
        nanos
    )))
}

/// Remove only the exact fixture tree; a failure is a hard acceptance error.
fn remove_tree(root: &Path) -> Result<(), String> {
    fs::remove_dir_all(root).map_err(|error| format!("fixture cleanup failed: {error}"))?;
    if root.exists() {
        return Err("fixture directory still exists after cleanup".into());
    }
    Ok(())
}

struct EnvGuard {
    key: String,
}

/// Supply only variables required by the Windows loader/profile plumbing; PATH
/// and all model/provider secrets remain intentionally absent from the block.
fn add_os_baseline(spec: &mut SandboxSpec) {
    for key in [
        "SystemRoot",
        "SystemDrive",
        "WINDIR",
        "ComSpec",
        "PATHEXT",
        "TEMP",
        "TMP",
        "HOMEDRIVE",
        "HOMEPATH",
        "USERPROFILE",
        "ProgramData",
        "ALLUSERSPROFILE",
        "PUBLIC",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "LOCALAPPDATA",
        "APPDATA",
        "OS",
    ] {
        if let Some(value) = env::var_os(key) {
            spec.env.insert(key.into(), value);
        }
    }
}

impl EnvGuard {
    /// Capture the marker key so a probe failure cannot poison the parent test
    /// process with a fixture-only secret.
    fn new(key: String) -> Self {
        Self { key }
    }
}

impl Drop for EnvGuard {
    /// Restore/remove only the unique marker owned by this probe.
    fn drop(&mut self) {
        // SAFETY: this guard owns the unique marker key for the process.
        unsafe { env::remove_var(&self.key) };
    }
}
