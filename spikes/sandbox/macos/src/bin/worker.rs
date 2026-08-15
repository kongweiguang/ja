// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Deliberately small worker fixture.  Assertions are made by Seatbelt and
//! the kernel, while this process only reports boolean observations.

#[cfg(not(target_os = "macos"))]
/// Fail explicitly on unsupported hosts so CI cannot mistake a non-native
/// fixture build for a Seatbelt acceptance result.
fn main() {
    eprintln!("SANDBOX-UNSUPPORTED: macOS worker requires a native macOS runner");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod macos {
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Run the fixture and convert any denied capability into a non-zero exit.
    pub fn main() {
        if let Err(error) = run() {
            eprintln!("fixture error: {error}");
            std::process::exit(17);
        }
    }

    /// Dispatch explicit fixture modes; no mode executes a shell or interprets a
    /// user-provided command string.
    fn run() -> Result<(), String> {
        let args: Vec<String> = env::args().collect();
        match required_arg(&args, "--mode")?.as_str() {
            "smoke" => run_smoke(&args),
            "spawn-grandchild" => run_spawn_grandchild(&args),
            "spawn-grandchild-exit" => run_spawn_grandchild_exit(&args),
            "emit-output-grandchild" => run_emit_output_grandchild(&args),
            "spawn-setsid-grandchild" => run_spawn_setsid_grandchild(&args),
            "idle" => run_idle(&args),
            "idle-output" => run_idle_output(&args),
            _ => Err("unknown fixture mode".into()),
        }
    }

    /// Exercise positive/negative filesystem, executable, network and env cases;
    /// only labels leave the sandbox so fixture contents cannot become a secret
    /// side channel.
    fn run_smoke(args: &[String]) -> Result<(), String> {
        let workspace = path_arg(args, "--workspace")?;
        let outside = path_arg(args, "--outside")?;
        let dotdot = path_arg(args, "--dotdot")?;
        let escape_link = path_arg(args, "--escape-link")?;
        let secret = path_arg(args, "--secret")?;
        let product_db = path_arg(args, "--product-db")?;
        let workspace_exe = path_arg(args, "--workspace-exe")?;
        let resource = path_arg(args, "--resource")?;
        let resource_sibling = path_arg(args, "--resource-sibling")?;
        let loopback = required_arg(args, "--loopback")?;
        let external = required_arg(args, "--external")?;
        let marker = required_arg(args, "--parent-marker")?;
        let mut lines = Vec::new();
        let allowed = workspace.join("allowed.txt");
        lines.push(format!("workspace-read={}", read_ok(&allowed)));
        lines.push(format!(
            "workspace-write={}",
            write_create_rename_delete(&workspace.join("worker-write.txt"))
        ));
        lines.push(format!("outside-read={}", read_denied(&outside)));
        lines.push(format!("outside-write={}", write_denied(&outside)));
        lines.push(format!(
            "secret-metadata-denied={}",
            metadata_denied(&secret)
        ));
        lines.push(format!(
            "product-db-metadata-denied={}",
            metadata_denied(&product_db)
        ));
        lines.push(format!("dotdot-read={}", read_denied(&dotdot)));
        lines.push(format!("escape-link-read={}", read_denied(&escape_link)));
        lines.push(format!("secret-read={}", read_denied(&secret)));
        lines.push(format!("product-db-read={}", read_denied(&product_db)));
        lines.push(format!(
            "workspace-execute-denied={}",
            execute_denied(&workspace_exe)
        ));
        lines.push(format!("resource-read={}", read_ok(&resource)));
        lines.push(format!(
            "resource-sibling-read={}",
            read_denied(&resource_sibling)
        ));
        lines.push(format!("parent-secret={}", env::var_os(marker).is_none()));
        lines.push(format!("path-inherited={}", env::var_os("PATH").is_some()));
        lines.push(format!("loopback-denied={}", network_denied(&loopback)));
        lines.push(format!(
            "external-network-denied={}",
            network_denied(&external)
        ));
        write_stdout(&lines.join("\n"))
    }

    /// Start a descendant that holds a report barrier so timeout cleanup can be
    /// proven against a real PID rather than a process-name guess.
    fn run_spawn_grandchild(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let child_report = path_arg(args, "--child-report")?;
        let release = path_arg(args, "--release")?;
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let child = Command::new(executable)
            .args([
                "--mode",
                "idle",
                "--report",
                child_report.to_str().ok_or("child report is not UTF-8")?,
                "--release",
                release.to_str().ok_or("release is not UTF-8")?,
            ])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("spawn descendant: {error}"))?;
        write_report(
            &report,
            &format!("grandchild-started=true\npid={}", child.id()),
        )
    }

    /// Let the direct parent exit while a grandchild inherits stdout/stderr; the
    /// host must kill the group before joining its readers or the join blocks.
    fn run_spawn_grandchild_exit(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let child_report = path_arg(args, "--child-report")?;
        let release = path_arg(args, "--release")?;
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let _child = Command::new(executable)
            .args([
                "--mode",
                "idle",
                "--report",
                child_report.to_str().ok_or("child report is not UTF-8")?,
                "--release",
                release.to_str().ok_or("release is not UTF-8")?,
            ])
            .env_clear()
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn inherited descendant: {error}"))?;
        write_report(&report, "grandchild-started=true")
    }

    /// Emit beyond the host cap while retaining a descendant, proving overflow
    /// takes the same complete-tree cleanup path as timeout/cancel.
    fn run_emit_output_grandchild(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let child_report = path_arg(args, "--child-report")?;
        let release = path_arg(args, "--release")?;
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let _child = Command::new(executable)
            .args([
                "--mode",
                "idle",
                "--report",
                child_report.to_str().ok_or("child report is not UTF-8")?,
                "--release",
                release.to_str().ok_or("release is not UTF-8")?,
            ])
            .env_clear()
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn overflow descendant: {error}"))?;
        write_report(&report, "grandchild-started=true")?;
        let mut output = io::stdout().lock();
        let bytes = [b'J'; 2 * 1024 * 1024];
        output
            .write_all(&bytes)
            .and_then(|_| output.flush())
            .map_err(|error| error.to_string())?;
        wait_for_barrier(&release, Duration::from_secs(30))
    }

    /// Attempt a new session/group in the descendant; the host probe treats a
    /// surviving PID as a hard sandbox/process-tree production blocker.
    fn run_spawn_setsid_grandchild(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let child_report = path_arg(args, "--child-report")?;
        let release = path_arg(args, "--release")?;
        let child_mode = if arg(args, "--continuous").is_some() {
            "idle-output"
        } else {
            "idle"
        };
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let mut command = Command::new(executable);
        command
            .args([
                "--mode",
                child_mode,
                "--report",
                child_report.to_str().ok_or("child report is not UTF-8")?,
                "--release",
                release.to_str().ok_or("release is not UTF-8")?,
            ])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // SAFETY: pre_exec runs in the forked child before exec and calls only
        // the async-signal-safe setsid operation.
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let started = match command.spawn() {
            Ok(child) => {
                write_report(&report, &format!("setsid-started=true\npid={}", child.id()))?;
                true
            }
            Err(_) => {
                write_report(&report, "setsid-denied=true")?;
                false
            }
        };
        if started {
            wait_for_barrier(&release, Duration::from_secs(30))?;
        }
        Ok(())
    }

    /// Hold a barrier until the host has observed the PID and performed cleanup.
    fn run_idle(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let release = path_arg(args, "--release")?;
        write_report(&report, &format!("idle=true\npid={}", std::process::id()))?;
        wait_for_barrier(&release, Duration::from_secs(30))
    }

    /// Keep producing output even after the host closes its pipe so a detached
    /// descendant cannot disappear merely because SIGPIPE was delivered.
    fn run_idle_output(args: &[String]) -> Result<(), String> {
        let report = path_arg(args, "--report")?;
        let release = path_arg(args, "--release")?;
        write_report(
            &report,
            &format!("idle-output=true\npid={}", std::process::id()),
        )?;
        let mut output = io::stdout().lock();
        let bytes = [b'X'; 8192];
        while !release.is_file() {
            if output
                .write_all(&bytes)
                .and_then(|_| output.flush())
                .is_err()
            {
                thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(())
    }

    /// Create, rename and delete only an allowed workspace marker to prove the
    /// positive write boundary without leaving a mutable fixture behind.
    fn write_create_rename_delete(path: &PathBuf) -> bool {
        let created = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .and_then(|mut file| file.write_all(b"worker-write"))
            .is_ok();
        let renamed = created && fs::rename(path, path.with_extension("renamed")).is_ok();
        let deleted = renamed && fs::remove_file(path.with_extension("renamed")).is_ok();
        created && renamed && deleted
    }

    /// A read is expected to succeed only for the explicitly allowed worker data.
    fn read_ok(path: &PathBuf) -> bool {
        fs::read(path).is_ok()
    }

    /// A read failure is the observable result for every protected target.
    fn read_denied(path: &PathBuf) -> bool {
        fs::read(path).is_err()
    }

    /// Metadata access is checked separately because a path-only deny can still
    /// reveal protected filenames, sizes or timestamps to an agent.
    fn metadata_denied(path: &PathBuf) -> bool {
        fs::metadata(path).is_err()
    }

    /// A write failure must not create or truncate a protected target.
    fn write_denied(path: &PathBuf) -> bool {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .is_err()
    }

    /// Attempt to execute an executable copy from workspace; Seatbelt must deny
    /// the operation even if ordinary Unix mode bits allow it.
    fn execute_denied(path: &PathBuf) -> bool {
        if !fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
        {
            return false;
        }
        Command::new(path)
            .arg("--mode")
            .arg("idle")
            .arg("--report")
            .arg(path.with_extension("exec.report"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|mut child| {
                let _ = child.kill();
                let _ = child.wait();
                false
            })
            .unwrap_or(true)
    }

    /// A denied network capability must reject both a host listener and an
    /// external address; loopback is a strong positive control against a merely
    /// unavailable network rather than an actually enforced denial.
    fn network_denied(address: &str) -> bool {
        address
            .parse::<SocketAddr>()
            .map(|address| {
                TcpStream::connect_timeout(&address, Duration::from_millis(400)).is_err()
            })
            .unwrap_or(false)
    }

    /// Wait on a host-created barrier using a monotonic deadline, avoiding fixed
    /// sleeps that could hide a scheduling or cleanup failure.
    fn wait_for_barrier(path: &std::path::Path, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.is_file() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err("fixture barrier timed out".into())
    }

    /// Report only fixture state labels; the host owns all path-sensitive checks.
    fn write_report(path: &PathBuf, content: &str) -> Result<(), String> {
        fs::write(path, content).map_err(|error| error.to_string())
    }

    /// Write bounded smoke labels to the host-owned stdout pipe.
    fn write_stdout(content: &str) -> Result<(), String> {
        io::stdout()
            .write_all(content.as_bytes())
            .and_then(|_| io::stdout().flush())
            .map_err(|error| error.to_string())
    }

    /// Read a named option without treating a following option as its value.
    fn arg(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|value| value == name)
            .and_then(|index| args.get(index + 1))
            .filter(|value| !value.starts_with('-'))
            .cloned()
    }

    /// Require every path/flag so malformed probes fail closed instead of testing
    /// a default or current-directory path by accident.
    fn required_arg(args: &[String], name: &str) -> Result<String, String> {
        arg(args, name).ok_or_else(|| format!("missing {name}"))
    }

    /// Convert a required path argument without accepting lossy Unicode.
    fn path_arg(args: &[String], name: &str) -> Result<PathBuf, String> {
        Ok(PathBuf::from(required_arg(args, name)?))
    }

    unsafe extern "C" {
        fn setsid() -> i32;
    }
}

#[cfg(target_os = "macos")]
/// Delegate to the macOS-only fixture module after the target gate is known.
fn main() {
    macos::main();
}
