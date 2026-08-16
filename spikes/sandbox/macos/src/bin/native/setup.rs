// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

// Fixture setup and shared host-side helpers for the native Seatbelt probe.

/// Convert a non-UTF8-safe path without lossy Unicode replacement in the
/// worker argument vector.
fn path_arg(path: &Path) -> std::ffi::OsString {
    path.as_os_str().to_os_string()
}

/// Run every native security case under one private fixture root so failure
/// aggregation cannot skip diagnostics or leave a protected artifact behind.
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
    let mut diagnostics = SandboxDenialDiagnostics::start();
    let result = run_all(&root, &mut diagnostics);
    let diagnostic_result = diagnostics.finish();
    let cleanup = remove_tree(&root);
    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error);
    }
    if let Err(error) = diagnostic_result {
        errors.push(error);
    }
    if let Err(error) = cleanup {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
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
fn run_all(root: &Path, diagnostics: &mut SandboxDenialDiagnostics) -> Result<(), String> {
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
        run_hardlink_case(root, &workspace, &worker, &secret, diagnostics),
        diagnostics,
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
    diagnostics.pump();
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
    run_case(
        "timeout",
        run_timeout_case(root, &workspace, &worker),
        diagnostics,
    )?;
    run_case(
        "parent-exit",
        run_parent_exit_case(root, &workspace, &worker),
        diagnostics,
    )?;
    run_case(
        "overflow",
        run_overflow_case(root, &workspace, &worker),
        diagnostics,
    )?;
    run_case(
        "cancel",
        run_cancel_case(root, &workspace, &worker),
        diagnostics,
    )?;
    run_case(
        "setsid",
        run_setsid_case(root, &workspace, &worker),
        diagnostics,
    )?;
    run_case(
        "setsid-continuous-output",
        run_setsid_output_case(root, &workspace, &worker),
        diagnostics,
    )?;
    println!(
        "SANDBOX-PASS: seatbelt, paths, environment, network, output and process-tree cleanup"
    );
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
