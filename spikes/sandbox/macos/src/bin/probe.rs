// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native macOS acceptance probe.  It intentionally fails on non-macOS and
//! never turns unavailable Seatbelt into a successful path-policy result.

#[cfg(not(target_os = "macos"))]
/// Reject non-native execution rather than silently running an unsandboxed
/// path test on the development host.
fn main() {
    eprintln!("SANDBOX-UNSUPPORTED: macOS Seatbelt probe requires a native macOS runner");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod native {
    use ja_macos_sandbox_spike::{
        SandboxChild, SandboxError, SandboxSpec, kill_process, process_is_alive, spawn,
    };
    use std::collections::BTreeMap;
    use std::env;
    use std::fs;
    use std::net::TcpListener;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const MARKER: &str = "JA_PARENT_SECRET_MARKER";

    macro_rules! argv {
        ($($value:expr),* $(,)?) => {
            vec![$($value.into()),*]
        };
    }

    /// Convert a non-UTF8-safe path without lossy Unicode replacement in the
    /// worker argument vector.
    fn path_arg(path: &Path) -> std::ffi::OsString {
        path.as_os_str().to_os_string()
    }

    pub fn run() -> Result<(), String> {
        require_seatbelt()?;
        let root = temporary_root()?;
        // The mode is passed to mkdir itself so another local user cannot read
        // the fixture during a create-then-chmod window.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|error| error.to_string())?;
        if let Err(error) = fs::set_permissions(&root, fs::Permissions::from_mode(0o700)) {
            let _ = remove_tree(&root);
            return Err(error.to_string());
        }
        if let Err(error) = assert_private_mode(&root, 0o700) {
            let _ = remove_tree(&root);
            return Err(error);
        }
        let result = run_all(&root);
        let cleanup = remove_tree(&root);
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
        }
    }

    /// Probe the real Seatbelt executable and reject a missing/blocked host
    /// before creating any fixture, avoiding an accidental unsandboxed pass.
    fn require_seatbelt() -> Result<(), String> {
        let executable = Path::new("/usr/bin/sandbox-exec");
        if !executable.is_file() {
            return Err("/usr/bin/sandbox-exec is unavailable".into());
        }
        let output = Command::new(executable)
            .args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
            .output()
            .map_err(|error| format!("sandbox-exec capability probe failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "sandbox-exec capability probe rejected with code {:?}",
                output.status.code()
            ));
        }
        Ok(())
    }

    /// Run positive/negative access cases and all process-lifecycle hazards in
    /// one temporary tree so cleanup and security snapshots share a boundary.
    fn run_all(root: &Path) -> Result<(), String> {
        let workspace = root.join("workspace 中文 空格");
        let outside_dir = root.join("outside");
        let resource_dir = root.join("resource");
        fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
        fs::create_dir_all(&outside_dir).map_err(|error| error.to_string())?;
        fs::create_dir_all(&resource_dir).map_err(|error| error.to_string())?;
        let worker_source = worker_binary()?;
        let worker = resource_dir.join("ja-sandbox-worker");
        fs::copy(&worker_source, &worker).map_err(|error| error.to_string())?;
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let resource_sibling = resource_dir.join("sibling.txt");
        fs::write(
            &resource_sibling,
            b"resource sibling must remain unreadable",
        )
        .map_err(|error| error.to_string())?;
        let allowed = workspace.join("allowed.txt");
        fs::write(&allowed, b"allowed workspace content").map_err(|error| error.to_string())?;
        let workspace_exe = workspace.join("workspace-executable");
        fs::copy(&worker_source, &workspace_exe).map_err(|error| error.to_string())?;
        fs::set_permissions(&workspace_exe, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let outside = outside_dir.join("outside.txt");
        let secret = outside_dir.join("provider-secret.txt");
        let product_db = outside_dir.join("ja.sqlite");
        fs::write(&outside, b"outside content").map_err(|error| error.to_string())?;
        fs::write(&secret, b"JA_PROVIDER_SECRET_VALUE").map_err(|error| error.to_string())?;
        fs::write(&product_db, b"SQLite format 3\0").map_err(|error| error.to_string())?;
        let escape_link = workspace.join("escape-link");
        let dotdot = workspace.join("..").join("outside").join("outside.txt");
        let loopback = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let loopback_address = loopback.local_addr().map_err(|error| error.to_string())?;
        let external_address = "1.1.1.1:80";
        run_case(
            "hardlink-preflight",
            run_hardlink_case(root, &workspace, &worker, &secret),
        )?;
        symlink(&secret, &escape_link)
            .map_err(|error| format!("symlink fixture unavailable: {error}"))?;
        let baseline = snapshot(&[
            &workspace,
            &allowed,
            &resource_dir,
            &worker,
            &resource_sibling,
            &outside,
            &secret,
            &product_db,
        ])?;

        let smoke_report = run_smoke(
            root,
            &workspace,
            &worker,
            &outside,
            &dotdot,
            &escape_link,
            &secret,
            &product_db,
            &workspace_exe,
            &resource_sibling,
            loopback_address,
            external_address,
        )?;
        assert_smoke(&smoke_report)?;
        println!("SANDBOX-CASE-PASS: smoke");
        let after_smoke = snapshot(&[
            &workspace,
            &allowed,
            &resource_dir,
            &worker,
            &resource_sibling,
            &outside,
            &secret,
            &product_db,
        ])?;
        if baseline != after_smoke {
            return Err("security attributes/content changed during smoke".into());
        }
        run_case("timeout", run_timeout_case(root, &workspace, &worker))?;
        run_case(
            "parent-exit",
            run_parent_exit_case(root, &workspace, &worker),
        )?;
        run_case("overflow", run_overflow_case(root, &workspace, &worker))?;
        run_case("cancel", run_cancel_case(root, &workspace, &worker))?;
        run_case("setsid", run_setsid_case(root, &workspace, &worker))?;
        run_case(
            "setsid-continuous-output",
            run_setsid_output_case(root, &workspace, &worker),
        )?;
        println!(
            "SANDBOX-PASS: seatbelt, paths, environment, network, output and process-tree cleanup"
        );
        Ok(())
    }

    /// Add a stable case label to failures and emit evidence for each native
    /// gate so CI logs show exactly which security invariant was exercised.
    fn run_case(label: &str, result: Result<(), String>) -> Result<(), String> {
        result.map_err(|error| format!("{label}: {error}"))?;
        println!("SANDBOX-CASE-PASS: {label}");
        Ok(())
    }

    /// Construct and run the positive/negative access fixture under Seatbelt.
    #[allow(clippy::too_many_arguments)]
    fn run_smoke(
        root: &Path,
        workspace: &Path,
        worker: &Path,
        outside: &Path,
        dotdot: &Path,
        escape_link: &Path,
        secret: &Path,
        product_db: &Path,
        workspace_exe: &Path,
        resource_sibling: &Path,
        loopback: std::net::SocketAddr,
        external: &str,
    ) -> Result<String, String> {
        let profile = root.join("profile-smoke.sb");
        let mut spec = SandboxSpec::new(worker, workspace, &profile);
        spec.args = argv![
            "--mode",
            "smoke",
            "--workspace",
            path_arg(workspace),
            "--outside",
            path_arg(outside),
            "--dotdot",
            path_arg(dotdot),
            "--escape-link",
            path_arg(escape_link),
            "--secret",
            path_arg(secret),
            "--product-db",
            path_arg(product_db),
            "--workspace-exe",
            path_arg(workspace_exe),
            "--resource",
            path_arg(worker),
            "--resource-sibling",
            path_arg(resource_sibling),
            "--loopback",
            loopback.to_string(),
            "--external",
            external,
            "--parent-marker",
            MARKER,
        ];
        spec.env = baseline_env();
        // SAFETY: this probe runs as a single-purpose test process and the
        // marker is restored by process exit; no library thread observes it.
        unsafe { env::set_var(MARKER, "JA_PARENT_SECRET_VALUE") };
        let mut child = spawn(spec).map_err(|error| error.to_string())?;
        let output = child
            .wait_with_output(Duration::from_secs(10))
            .map_err(|error| error.to_string())?;
        drop(child);
        if output.outcome.timed_out || !output.outcome.status.success() {
            return Err("smoke worker did not exit successfully".into());
        }
        String::from_utf8(output.stdout).map_err(|error| error.to_string())
    }

    /// Check every expected marker so an accidentally permissive profile can
    /// never be reported as a generic process success.
    fn assert_smoke(report: &str) -> Result<(), String> {
        let expected = [
            "workspace-read=true",
            "workspace-write=true",
            "outside-read=true",
            "outside-write=true",
            "secret-metadata-denied=true",
            "product-db-metadata-denied=true",
            "dotdot-read=true",
            "escape-link-read=true",
            "secret-read=true",
            "product-db-read=true",
            "workspace-execute-denied=true",
            "resource-read=true",
            "resource-sibling-read=true",
            "parent-secret=true",
            "path-inherited=false",
            "loopback-denied=true",
            "external-network-denied=true",
        ];
        for marker in expected {
            if !report.lines().any(|line| line == marker) {
                return Err(format!("smoke marker missing: {marker}"));
            }
        }
        Ok(())
    }

    /// Attempt a same-filesystem hardlink to protected content; success must
    /// make spawn fail before profile creation or worker execution.
    fn run_hardlink_case(
        root: &Path,
        workspace: &Path,
        worker: &Path,
        secret: &Path,
    ) -> Result<(), String> {
        let hardlink = workspace.join("protected-hardlink");
        fs::hard_link(secret, &hardlink)
            .map_err(|error| format!("hardlink fixture unavailable on macOS: {error}"))?;
        let report = root.join("hardlink-worker.report");
        let release = root.join("hardlink.release");
        let profile = root.join("profile-hardlink.sb");
        let mut spec = SandboxSpec::new(worker, workspace, &profile);
        spec.args = argv![
            "--mode",
            "idle",
            "--report",
            path_arg(&report),
            "--release",
            path_arg(&release),
        ];
        spec.env = baseline_env();
        let result = spawn(spec);
        let _ = fs::remove_file(&hardlink);
        match result {
            Err(SandboxError::InvalidConfig(_)) => {}
            Err(_) => return Err("hardlink preflight returned the wrong error".into()),
            Ok(mut child) => {
                let _ = child.cancel();
                let _ = child.wait_with_output(Duration::from_secs(1));
                return Err("workspace hardlink preflight did not fail closed".into());
            }
        }
        if profile.exists() || report.exists() {
            return Err("hardlink rejection created profile or ran worker".into());
        }
        run_clean_preflight_case(root, workspace, worker)
    }

    /// Prove a normal workspace still passes link preflight before the
    /// adversarial hardlink is introduced in a later fixture.
    fn run_clean_preflight_case(
        root: &Path,
        workspace: &Path,
        worker: &Path,
    ) -> Result<(), String> {
        let report = workspace.join("clean-preflight.report");
        let release = workspace.join("clean-preflight.release");
        let profile = root.join("profile-clean-preflight.sb");
        let mut spec = SandboxSpec::new(worker, workspace, &profile);
        spec.args = argv![
            "--mode",
            "idle",
            "--report",
            path_arg(&report),
            "--release",
            path_arg(&release),
        ];
        spec.env = baseline_env();
        let mut child = spawn(spec).map_err(|error| {
            format!(
                "clean preflight spawn failed: {}",
                sandbox_error_category(&error)
            )
        })?;
        assert_private_mode(&profile, 0o600)?;
        wait_for_report_or_exit(&mut child, &report, Duration::from_secs(2))?;
        child.cancel().map_err(|error| error.to_string())?;
        let result = child.wait_with_output(Duration::from_secs(2));
        drop(child);
        let _ = fs::remove_file(&report);
        let _ = fs::remove_file(&release);
        if !matches!(result, Err(SandboxError::Cancelled)) {
            return Err("clean workspace preflight did not run normally".into());
        }
        if profile.exists() {
            return Err("clean workspace profile was not cleaned".into());
        }
        Ok(())
    }

    /// Wait for the startup marker while polling the direct child, so loader,
    /// profile and exec failures are reported instead of becoming a generic
    /// marker timeout.
    fn wait_for_report_or_exit(
        child: &mut SandboxChild,
        report: &Path,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if report.is_file() {
                return Ok(());
            }
            if let Some(status) = child.poll_status().map_err(|error| {
                format!("startup poll failed: {}", sandbox_error_category(&error))
            })? {
                return Err(diagnose_terminal_child(child, &status));
            }
            if Instant::now() >= deadline {
                let terminal = child.wait_with_output(Duration::ZERO);
                return Err(match terminal {
                    Ok(output) => format!(
                        "startup marker deadline: child={}; stderr={}",
                        exit_status_category(&output.outcome.status),
                        stderr_category(&output.stderr)
                    ),
                    Err(error) => format!(
                        "startup marker deadline: wait={}",
                        sandbox_error_category(&error)
                    ),
                });
            }
            // This sleep only backs off between status/file observations; the
            // report and direct-child state, not elapsed sleep, ends the wait.
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Drain a terminal worker within the normal bounded cleanup path and
    /// expose only stable status/stderr categories, never raw process output.
    fn diagnose_terminal_child(
        child: &mut SandboxChild,
        status: &std::process::ExitStatus,
    ) -> String {
        match child.wait_with_output(Duration::from_millis(100)) {
            Ok(output) => format!(
                "worker exited before startup marker: child={}; stderr={}",
                exit_status_category(status),
                stderr_category(&output.stderr)
            ),
            Err(error) => format!(
                "worker exited before startup marker: child={}; wait={}",
                exit_status_category(status),
                sandbox_error_category(&error)
            ),
        }
    }

    /// Reduce sandbox errors to an audit-friendly category without leaking
    /// filesystem paths, command arguments or provider credentials.
    fn sandbox_error_category(error: &SandboxError) -> &'static str {
        match error {
            SandboxError::InvalidConfig(_) => "invalid-config",
            SandboxError::Io(_) => "io",
            SandboxError::Profile(_) => "profile",
            SandboxError::Unsupported => "unsupported",
            SandboxError::Timeout => "timeout",
            SandboxError::Cancelled => "cancelled",
            SandboxError::OutputOverflow => "output-overflow",
            SandboxError::ChildCleanup(_) => "child-cleanup",
        }
    }

    /// Report only exit class and not a platform-specific raw status string.
    fn exit_status_category(status: &std::process::ExitStatus) -> String {
        if status.success() {
            "success".into()
        } else if let Some(code) = status.code() {
            format!("exit-code-{code}")
        } else if let Some(signal) = status.signal() {
            format!("signal-{signal}")
        } else {
            "unknown".into()
        }
    }

    /// Classify stderr by known failure families and discard its contents so
    /// profile paths, secrets and runner-specific diagnostics stay private.
    fn stderr_category(stderr: &[u8]) -> &'static str {
        let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
        if text.is_empty() {
            "empty"
        } else if text.contains("permission denied") || text.contains("operation not permitted") {
            "permission-denied"
        } else if text.contains("no such file") {
            "not-found"
        } else if text.contains("sandbox") || text.contains("seatbelt") {
            "sandbox-policy"
        } else {
            "nonempty"
        }
    }

    /// Timeout must kill a blocked parent and its real descendant before the
    /// fixture root can be reused or removed.
    fn run_timeout_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("timeout.report");
        let child_report = workspace.join("timeout-child.report");
        let release = workspace.join("timeout.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "timeout",
            "spawn-grandchild",
            &report,
            &child_report,
            &release,
            false,
        )?;
        let result = child.wait_with_output(Duration::from_millis(500));
        if !matches!(result, Err(SandboxError::Timeout)) {
            return Err("blocked descendant did not fail with timeout".into());
        }
        verify_descendant_gone(&child_report)?;
        Ok(())
    }

    /// A parent that exits normally must still close an inherited pipe held by
    /// its child; this is the regression case for reader-join deadlocks.
    fn run_parent_exit_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("parent-exit.report");
        let child_report = workspace.join("parent-exit-child.report");
        let release = workspace.join("parent-exit.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "parent-exit",
            "spawn-grandchild-exit",
            &report,
            &child_report,
            &release,
            false,
        )?;
        let output = child
            .wait_with_output(Duration::from_secs(5))
            .map_err(|error| format!("parent exit cleanup failed: {error}"))?;
        if !output.outcome.status.success() {
            return Err("parent exit fixture returned failure".into());
        }
        verify_descendant_gone(&child_report)
    }

    /// Overflow is fatal and must close the same descendant tree as timeout.
    fn run_overflow_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("overflow.report");
        let child_report = workspace.join("overflow-child.report");
        let release = workspace.join("overflow.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "overflow",
            "emit-output-grandchild",
            &report,
            &child_report,
            &release,
            false,
        )?;
        let result = child.wait_with_output(Duration::from_secs(5));
        if !matches!(result, Err(SandboxError::OutputOverflow)) {
            return Err("output overflow did not fail closed".into());
        }
        verify_descendant_gone(&child_report)
    }

    /// Explicit cancellation is separate from timeout so the host contract
    /// can distinguish user stop from an expired operation deadline.
    fn run_cancel_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("cancel.report");
        let child_report = workspace.join("cancel-child.report");
        let release = workspace.join("cancel.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "cancel",
            "spawn-grandchild",
            &report,
            &child_report,
            &release,
            false,
        )?;
        wait_for_file(&child_report, Duration::from_secs(2))?;
        child.cancel().map_err(|error| error.to_string())?;
        let result = child.wait_with_output(Duration::from_secs(2));
        if !matches!(result, Err(SandboxError::Cancelled)) {
            return Err("cancelled worker did not report cancellation".into());
        }
        verify_descendant_gone(&child_report)
    }

    /// Exercise a descendant that calls setsid; survival after group cleanup is
    /// an explicit production blocker rather than a silently accepted escape.
    fn run_setsid_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("setsid.report");
        let child_report = workspace.join("setsid-child.report");
        let release = workspace.join("setsid.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "setsid",
            "spawn-setsid-grandchild",
            &report,
            &child_report,
            &release,
            false,
        )?;
        let result = child.wait_with_output(Duration::from_millis(700));
        wait_for_file(&report, Duration::from_secs(2))?;
        let report_text = fs::read_to_string(&report).map_err(|error| error.to_string())?;
        if report_text.lines().any(|line| line == "setsid-denied=true") {
            if !matches!(result, Ok(output) if output.outcome.status.success()) {
                return Err("setsid denial fixture did not exit cleanly".into());
            }
            return Ok(());
        }
        let pid = report_text
            .lines()
            .find_map(|line| line.strip_prefix("pid=")?.parse::<i32>().ok())
            .ok_or_else(|| "setsid fixture omitted descendant pid".to_string())?;
        if process_is_alive(pid) {
            let _ = kill_process(pid);
            return Err("setsid descendant escaped process-group cleanup".into());
        }
        if result.is_ok() {
            return Err("setsid fixture unexpectedly reported success".into());
        }
        Ok(())
    }

    /// Repeat the setsid negative case while the escaped child continuously
    /// writes, proving overflow stops reading before an escaped writer can
    /// keep cleanup blocked indefinitely.
    fn run_setsid_output_case(root: &Path, workspace: &Path, worker: &Path) -> Result<(), String> {
        let report = workspace.join("setsid-output.report");
        let child_report = workspace.join("setsid-output-child.report");
        let release = workspace.join("setsid-output.release");
        let mut child = child_for(
            root,
            workspace,
            worker,
            "setsid-output",
            "spawn-setsid-grandchild",
            &report,
            &child_report,
            &release,
            true,
        )?;
        let started = Instant::now();
        let result = child.wait_with_output(Duration::from_secs(5));
        if started.elapsed() > Duration::from_secs(4) {
            return Err("continuous escaped output exceeded cleanup deadline".into());
        }
        wait_for_file(&report, Duration::from_secs(2))?;
        let report_text = fs::read_to_string(&report).map_err(|error| error.to_string())?;
        if report_text.lines().any(|line| line == "setsid-denied=true") {
            if !matches!(result, Ok(output) if output.outcome.status.success()) {
                return Err("continuous setsid denial fixture did not exit cleanly".into());
            }
            return Ok(());
        }
        let pid = report_text
            .lines()
            .find_map(|line| line.strip_prefix("pid=")?.parse::<i32>().ok())
            .ok_or_else(|| "continuous setsid fixture omitted descendant pid".to_string())?;
        if process_is_alive(pid) {
            let _ = kill_process(pid);
            return Err("continuous setsid descendant escaped process-group cleanup".into());
        }
        if !matches!(result, Err(SandboxError::OutputOverflow)) {
            return Err("continuous escaped writer did not trigger bounded overflow".into());
        }
        Ok(())
    }

    /// Build a worker spec with a unique profile and no inherited parent env.
    #[allow(clippy::too_many_arguments)]
    fn child_for(
        root: &Path,
        workspace: &Path,
        worker: &Path,
        label: &str,
        mode: &str,
        report: &Path,
        child_report: &Path,
        release: &Path,
        continuous: bool,
    ) -> Result<ja_macos_sandbox_spike::SandboxChild, String> {
        let profile = root.join(format!("profile-{label}.sb"));
        let mut spec = SandboxSpec::new(worker, workspace, profile);
        spec.args = argv![
            "--mode",
            mode,
            "--report",
            path_arg(report),
            "--child-report",
            path_arg(child_report),
            "--release",
            path_arg(release),
        ];
        if continuous {
            spec.args.push("--continuous".into());
        }
        spec.env = baseline_env();
        spawn(spec).map_err(|error| error.to_string())
    }

    /// Verify report PID and wait until the kernel no longer exposes it.
    fn verify_descendant_gone(report: &Path) -> Result<(), String> {
        wait_for_file(report, Duration::from_secs(2))?;
        let pid = fs::read_to_string(report)
            .map_err(|error| error.to_string())?
            .lines()
            .find_map(|line| line.strip_prefix("pid=")?.parse::<i32>().ok())
            .ok_or_else(|| "descendant report omitted pid".to_string())?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !process_is_alive(pid) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err("descendant survived process-group cleanup".into())
    }

    /// Use only the minimum runtime variables needed by the worker and omit
    /// PATH plus all parent/provider secrets.
    fn baseline_env() -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
        let mut env = BTreeMap::new();
        for key in ["HOME", "USER", "LOGNAME", "SHELL", "TERM", "TMPDIR"] {
            if let Some(value) = env::var_os(key) {
                env.insert(key.into(), value);
            }
        }
        env
    }

    /// Snapshot mode, content hash and xattrs through host utilities before
    /// and after the worker, so a passing access test also proves no protected
    /// fixture mutation occurred.
    fn snapshot(paths: &[&Path]) -> Result<Vec<String>, String> {
        paths
            .iter()
            .map(|path| {
                let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
                let xattrs = Command::new("/usr/bin/xattr")
                    .arg("-l")
                    .arg(path)
                    .output()
                    .map_err(|error| error.to_string())?;
                if !xattrs.status.success() {
                    return Err("xattr inspection failed".into());
                }
                let acl = Command::new("/bin/ls")
                    .args(["-lde"])
                    .arg(path)
                    .output()
                    .map_err(|error| error.to_string())?;
                if !acl.status.success() {
                    return Err("ACL inspection failed".into());
                }
                let digest = if metadata.is_file() {
                    let hash = Command::new("/usr/bin/shasum")
                        .args(["-a", "256"])
                        .arg(path)
                        .output()
                        .map_err(|error| error.to_string())?;
                    if !hash.status.success() {
                        return Err("content hash inspection failed".into());
                    }
                    String::from_utf8_lossy(&hash.stdout)
                        .split_whitespace()
                        .next()
                        .ok_or_else(|| "content hash output was empty".to_string())?
                        .to_string()
                } else {
                    "directory".to_string()
                };
                Ok(format!(
                    "mode={:o};size={};hash={};xattr={};acl={}",
                    metadata.permissions().mode(),
                    metadata.len(),
                    digest,
                    String::from_utf8_lossy(&xattrs.stdout),
                    String::from_utf8_lossy(&acl.stdout)
                ))
            })
            .collect()
    }

    /// Confirm the probe's temporary root and generated policy never expose
    /// fixture paths to another local user while a worker is active.
    fn assert_private_mode(path: &Path, expected: u32) -> Result<(), String> {
        let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode != expected {
            return Err(format!(
                "private fixture mode mismatch: expected {expected:o}, got {mode:o}"
            ));
        }
        Ok(())
    }

    /// Locate the sibling fixture emitted by Cargo, refusing a system or
    /// user-provided replacement executable.
    fn worker_binary() -> Result<PathBuf, String> {
        let mut path = env::current_exe().map_err(|error| error.to_string())?;
        path.set_file_name("ja-sandbox-worker");
        if path.is_file() {
            Ok(path)
        } else {
            Err("ja-sandbox-worker is not next to the probe".into())
        }
    }

    /// Wait on a fixture file rather than sleeping a guessed duration.
    fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.is_file() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err("fixture report barrier timed out".into())
    }

    /// Create a collision-resistant Unicode/space path and remove only this
    /// exact tree after all children and profiles have been closed.
    fn temporary_root() -> Result<PathBuf, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        Ok(env::temp_dir().join(format!(
            "JA sandbox 中文 空格 {} {nanos}",
            std::process::id()
        )))
    }

    /// Remove the exact fixture tree; cleanup failure is a hard acceptance
    /// error because stale secrets or workers invalidate subsequent runs.
    fn remove_tree(root: &Path) -> Result<(), String> {
        fs::remove_dir_all(root).map_err(|error| format!("fixture cleanup failed: {error}"))?;
        if root.exists() {
            return Err("fixture root remained after cleanup".into());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
/// Execute the native probe and turn any missing enforcement into a hard CI
/// failure for the platform matrix.
fn main() {
    if let Err(error) = native::run() {
        eprintln!("SANDBOX-FAIL: {error}");
        std::process::exit(1);
    }
}
