// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Native Seatbelt access, denial, and process-tree probe cases.

/// Add a stable case label to failures and emit evidence for each native
/// gate so CI logs show exactly which security invariant was exercised.
fn run_case(
    label: &str,
    result: Result<(), String>,
    diagnostics: &mut SandboxDenialDiagnostics,
) -> Result<(), String> {
    diagnostics.pump();
    result.map_err(|error| format!("{label}: {error}"))?;
    diagnostics.pump();
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
    scope: &mut ProbeScope,
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
    let mut child =
        spawn_registered(scope, spec, "ja-sandbox-worker").map_err(|error| error.to_string())?;
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
    diagnostics: &mut SandboxDenialDiagnostics,
    scope: &mut ProbeScope,
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
    // Keep the hardlink in place until preparation observes its inode; remove
    // it only after the single spawn gate has rejected or adopted the child.
    let hardlink_result: Result<(), String> = {
        let result = spawn_registered(scope, spec, "ja-sandbox-worker");
        let removed = fs::remove_file(&hardlink).is_ok();
        if !removed {
            if let Ok(mut child) = result {
                let _ = child.cancel();
                let _ = child.wait_with_output(Duration::from_secs(1));
            }
            Err("hardlink fixture cleanup failed".into())
        } else {
            match result {
                Err(SandboxError::InvalidConfig(_)) => Ok(()),
                Err(_) => Err("hardlink preflight returned the wrong error".into()),
                Ok(mut child) => {
                    let _ = child.cancel();
                    let _ = child.wait_with_output(Duration::from_secs(1));
                    Err("workspace hardlink preflight did not fail closed".into())
                }
            }
        }
    };
    hardlink_result?;
    if profile.exists() || report.exists() {
        return Err("hardlink rejection created profile or ran worker".into());
    }
    run_clean_preflight_case(root, workspace, worker, diagnostics, scope)
}

/// Prove a normal workspace still passes link preflight before the
/// adversarial hardlink is introduced in a later fixture.
fn run_clean_preflight_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    diagnostics: &mut SandboxDenialDiagnostics,
    scope: &mut ProbeScope,
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
    let mut child = spawn_registered(scope, spec, "ja-sandbox-worker").map_err(|error| {
        format!(
            "clean preflight spawn failed: {}",
            sandbox_error_category(&error)
        )
    })?;
    assert_private_mode(&profile, 0o600)?;
    if let Err(startup_error) =
        wait_for_report_or_exit(&mut child, &report, Duration::from_secs(2), diagnostics)
    {
        // The direct child has already entered the bounded cleanup path.  A
        // separate system-loader control now distinguishes a worker-specific
        // launch failure from a Seatbelt/profile initialization failure while
        // keeping the production worker profile deny-by-default.
        drop(child);
        let baseline = run_startup_baseline_case(root, workspace, scope);
        let _ = fs::remove_file(&report);
        let _ = fs::remove_file(&release);
        return Err(format!(
            "clean preflight startup failed: {startup_error}; loader-baseline={baseline}"
        ));
    }
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

/// Launch a fixed system executable under the same generated Seatbelt shape;
/// this control is only reported after worker startup fails and never replaces
/// the worker security assertion or grants it an additional capability.
fn run_startup_baseline_case(
    root: &Path,
    workspace: &Path,
    scope: &mut ProbeScope,
) -> &'static str {
    let profile = root.join("profile-startup-baseline.sb");
    let mut spec = SandboxSpec::new("/usr/bin/true", workspace, &profile);
    spec.env = baseline_env();
    let mut child = match spawn_registered(scope, spec, "true") {
        Ok(child) => child,
        Err(error) => return sandbox_error_category(&error),
    };
    let result = child.wait_with_output(Duration::from_secs(1));
    drop(child);
    if profile.exists() {
        return "profile-cleanup";
    }
    match result {
        Ok(output) if output.outcome.status.success() => "pass",
        Ok(output) if output.outcome.status.code().is_some() => "exit",
        Ok(_) => "signal",
        Err(error) => sandbox_error_category(&error),
    }
}

/// Wait for the startup marker while polling the direct child, so loader,
/// profile and exec failures are reported instead of becoming a generic
/// marker timeout.
fn wait_for_report_or_exit(
    child: &mut ScopedNativeChild<'_>,
    report: &Path,
    timeout: Duration,
    diagnostics: &mut SandboxDenialDiagnostics,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        diagnostics.pump();
        if report.is_file() {
            return Ok(());
        }
        if let Some(status) = child
            .poll_status()
            .map_err(|error| format!("startup poll failed: {}", sandbox_error_category(&error)))?
        {
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
    child: &mut ScopedNativeChild<'_>,
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
fn run_timeout_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
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
        scope,
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
fn run_parent_exit_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
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
        scope,
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
fn run_overflow_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
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
        scope,
    )?;
    let result = child.wait_with_output(Duration::from_secs(5));
    if !matches!(result, Err(SandboxError::OutputOverflow)) {
        return Err("output overflow did not fail closed".into());
    }
    verify_descendant_gone(&child_report)
}

/// Explicit cancellation is separate from timeout so the host contract
/// can distinguish user stop from an expired operation deadline.
fn run_cancel_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
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
        scope,
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
fn run_setsid_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
    let mut evidence = prepare_escape_evidence(root, "setsid")?;
    let report = workspace.join("setsid.report");
    let child_report = workspace.join("setsid-child.report");
    let release = workspace.join("setsid.release");
    bind_escape_report(&mut evidence, &child_report);
    let child = match child_for(
        root,
        workspace,
        worker,
        "setsid",
        "spawn-setsid-grandchild",
        &report,
        &child_report,
        &release,
        false,
        scope,
    ) {
        Ok(child) => child,
        Err(_) => {
            if mark_escape_failure(&mut evidence, None, "spawn").is_err() {
                eprintln!("SANDBOX-NATIVE: setsid evidence retention failed");
            }
            return Err("setsid worker spawn failed".into());
        }
    };
    if wait_for_file(&report, Duration::from_secs(2)).is_err() {
        return finish_setsid_case(
            child,
            evidence,
            None,
            Duration::from_millis(700),
            SetSidExpectation::DeniedSuccess,
            "setsid-report-timeout",
        );
    }
    let report_text = match read_bounded_report(&report) {
        Ok(text) => text,
        Err(_) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_millis(700),
                SetSidExpectation::DeniedSuccess,
                "setsid-report-read",
            );
        }
    };
    let pid = match parse_setsid_report(&report_text) {
        Ok(SetsidReport::Denied) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_millis(700),
                SetSidExpectation::NoDescendantSuccess,
                "setsid-denial-exit",
            );
        }
        Ok(SetsidReport::Descendant { pid }) => pid,
        Err(_) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_millis(700),
                SetSidExpectation::DeniedSuccess,
                "setsid-report-parse",
            );
        }
    };
    // Capture the escaped descendant while the report's long-lived fixture is
    // still alive, before host cleanup can create a PID-reuse ambiguity.
    let identity = match capture_escaped_identity(pid, "setsid descendant identity query failed") {
        Ok(identity) => Some(identity),
        Err(_) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_millis(700),
                SetSidExpectation::DeniedSuccess,
                "setsid-identity-capture",
            );
        }
    };
    if let Some(identity) = identity.as_ref()
        && upgrade_escape_evidence(&mut evidence, Some(identity), "active", "none").is_err()
    {
        return finish_setsid_case(
            child,
            evidence,
            Some(identity.clone()),
            Duration::from_millis(700),
            SetSidExpectation::Escaped,
            "setsid-evidence-upgrade",
        );
    }
    finish_setsid_case(
        child,
        evidence,
        identity,
        Duration::from_millis(700),
        SetSidExpectation::Escaped,
        "setsid-lifecycle",
    )
}

/// Repeat the setsid negative case while the escaped child continuously
/// writes, proving overflow stops reading before an escaped writer can
/// keep cleanup blocked indefinitely.
fn run_setsid_output_case(
    root: &Path,
    workspace: &Path,
    worker: &Path,
    scope: &mut ProbeScope,
) -> Result<(), String> {
    let mut evidence = prepare_escape_evidence(root, "setsid-output")?;
    let report = workspace.join("setsid-output.report");
    let child_report = workspace.join("setsid-output-child.report");
    let release = workspace.join("setsid-output.release");
    bind_escape_report(&mut evidence, &child_report);
    let child = match child_for(
        root,
        workspace,
        worker,
        "setsid-output",
        "spawn-setsid-grandchild",
        &report,
        &child_report,
        &release,
        true,
        scope,
    ) {
        Ok(child) => child,
        Err(_) => {
            if mark_escape_failure(&mut evidence, None, "spawn").is_err() {
                eprintln!("SANDBOX-NATIVE: setsid evidence retention failed");
            }
            return Err("continuous setsid worker spawn failed".into());
        }
    };
    let started = Instant::now();
    if wait_for_file(&report, Duration::from_secs(2)).is_err() {
        return finish_setsid_case(
            child,
            evidence,
            None,
            Duration::from_secs(5),
            SetSidExpectation::Overflow,
            "setsid-output-report-timeout",
        );
    }
    let report_text = match read_bounded_report(&report) {
        Ok(text) => text,
        Err(_) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_secs(5),
                SetSidExpectation::Overflow,
                "setsid-output-report-read",
            );
        }
    };
    let pid = match parse_setsid_report(&report_text) {
        Ok(SetsidReport::Denied) => {
            if started.elapsed() > Duration::from_secs(4) {
                return finish_setsid_case(
                    child,
                    evidence,
                    None,
                    Duration::from_secs(5),
                    SetSidExpectation::NoDescendantSuccess,
                    "setsid-output-deadline",
                );
            }
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_secs(5),
                SetSidExpectation::NoDescendantSuccess,
                "setsid-output-denial-exit",
            );
        }
        Ok(SetsidReport::Descendant { pid }) => pid,
        Err(_) => {
            return finish_setsid_case(
                child,
                evidence,
                None,
                Duration::from_secs(5),
                SetSidExpectation::Overflow,
                "setsid-output-report-parse",
            );
        }
    };
    let identity =
        match capture_escaped_identity(pid, "continuous setsid descendant identity query failed") {
            Ok(identity) => Some(identity),
            Err(_) => {
                return finish_setsid_case(
                    child,
                    evidence,
                    None,
                    Duration::from_secs(5),
                    SetSidExpectation::Overflow,
                    "setsid-output-identity-capture",
                );
            }
        };
    if started.elapsed() > Duration::from_secs(4) {
        return finish_setsid_case(
            child,
            evidence,
            identity,
            Duration::from_secs(5),
            SetSidExpectation::Overflow,
            "setsid-output-deadline",
        );
    }
    if let Some(identity) = identity.as_ref()
        && upgrade_escape_evidence(&mut evidence, Some(identity), "active", "none").is_err()
    {
        return finish_setsid_case(
            child,
            evidence,
            Some(identity.clone()),
            Duration::from_secs(5),
            SetSidExpectation::Overflow,
            "setsid-output-evidence-upgrade",
        );
    }
    finish_setsid_case(
        child,
        evidence,
        identity,
        Duration::from_secs(5),
        SetSidExpectation::Overflow,
        "setsid-output-lifecycle",
    )
}

enum SetSidExpectation {
    /// The worker explicitly proved Seatbelt denied setsid before creating a
    /// descendant; this is the only branch where an absent PID is success.
    NoDescendantSuccess,
    DeniedSuccess,
    Escaped,
    Overflow,
}

/// Make one bounded recovery capture from the child report when an earlier
/// wait/read/parse path failed.  A report that is missing, empty, malformed or
/// lacks a live identity is a typed failure: the provisional evidence remains
/// the recovery anchor and the caller must fail closed.
fn recover_escaped_identity(
    evidence: &EscapeEvidence,
) -> Result<ControlledProcessIdentity, String> {
    let Some(report) = evidence.recovery_report.as_ref() else {
        return Err("setsid recovery report missing".into());
    };
    let text = read_bounded_report(report)?;
    if text.trim().is_empty() {
        return Err("setsid recovery report empty".into());
    }
    let pid = parse_report_pid(&text, "setsid recovery report pid missing")?;
    capture_escaped_identity(pid, "setsid recovery identity query failed")
}

/// Finish every setsid path through one bounded child/descendant finalizer.
/// Failure evidence is upgraded before ownership is released; only a proven
/// direct reap, escaped identity cleanup and durable unlink can remove it.
fn finish_setsid_case(
    mut child: ScopedNativeChild<'_>,
    mut evidence: EscapeEvidence,
    escaped: Option<ControlledProcessIdentity>,
    timeout: Duration,
    expectation: SetSidExpectation,
    failure_category: &'static str,
) -> Result<(), String> {
    let mut escaped = escaped;
    let recovery_error =
        if escaped.is_none() && !matches!(expectation, SetSidExpectation::NoDescendantSuccess) {
            match recover_escaped_identity(&evidence) {
                Ok(identity) => {
                    escaped = Some(identity);
                    false
                }
                Err(_) => true,
            }
        } else {
            false
        };
    let result = child.wait_with_output(timeout);
    let child_cleanup_ok = !matches!(result, Err(SandboxError::ChildCleanup(_)));
    let outcome_ok = match expectation {
        SetSidExpectation::NoDescendantSuccess => {
            matches!(&result, Ok(output) if output.outcome.status.success())
        }
        SetSidExpectation::DeniedSuccess => {
            matches!(&result, Ok(output) if output.outcome.status.success())
        }
        SetSidExpectation::Escaped => result.is_err(),
        SetSidExpectation::Overflow => matches!(result, Err(SandboxError::OutputOverflow)),
    };
    let escaped_cleanup_ok = !recovery_error
        && escaped.as_ref().is_none_or(|identity| {
            let Ok(pid) = i32::try_from(identity.pid) else {
                return false;
            };
            terminate_controlled_identity(identity, Duration::from_secs(2)).is_ok()
                && verify_process_gone(pid).is_ok()
        });
    let escape_observed = escaped.is_some();
    if !child_cleanup_ok || !outcome_ok || !escaped_cleanup_ok {
        fail_closed_setsid(&mut evidence, escaped.as_ref(), failure_category);
    }
    let mut evidence_io = RealEscapeEvidenceIo;
    if remove_escape_evidence(&mut evidence_io, &evidence).is_err() {
        fail_closed_setsid(&mut evidence, escaped.as_ref(), "evidence-remove");
    }
    if escape_observed {
        return Err("setsid descendant escaped process-group cleanup".into());
    }
    Ok(())
}

/// Abort after durable failure evidence has been attempted because returning
/// would release a child/descendant whose complete cleanup is not proven.
fn fail_closed_setsid(
    evidence: &mut EscapeEvidence,
    escaped: Option<&ControlledProcessIdentity>,
    category: &'static str,
) -> ! {
    let _ = mark_escape_failure(evidence, escaped, category);
    eprintln!("SANDBOX-NATIVE: setsid cleanup unconfirmed: {category}");
    std::process::abort()
}

/// Capture all immutable fields before the production cleanup path; a missing
/// process is a failure once a descendant report claims it, while a live PID
/// must be queried as a full identity rather than trusted from the report.
fn capture_escaped_identity(
    pid: i32,
    failure: &'static str,
) -> Result<ControlledProcessIdentity, String> {
    match process_is_alive(pid) {
        Ok(false) => return Err(failure.to_owned()),
        Ok(true) => {}
        Err(_) => return Err(failure.to_owned()),
    }
    let pid = u32::try_from(pid).map_err(|_| failure.to_owned())?;
    query_controlled_identity(pid).map_err(|_| failure.to_owned())
}
