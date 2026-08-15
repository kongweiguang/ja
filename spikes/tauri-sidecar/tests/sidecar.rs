// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! sidecar spike 的真实 child/process-tree 集成验证。

use ja_tauri_sidecar_spike::{
    LifecycleState, OutputStream, ProtocolViolation, RestartPolicy, ShutdownOutcome, SidecarConfig,
    SidecarError, SidecarEvent, SidecarSupervisor,
};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct EnvReset(String);

impl Drop for EnvReset {
    /// 即使断言 panic 也恢复测试进程环境，避免并发 fixture 继承临时 sentinel。
    fn drop(&mut self) {
        // SAFETY: this guard owns the unique test sentinel name.
        unsafe { std::env::remove_var(&self.0) };
    }
}

/// 为每个场景创建带空格/Unicode 的 cwd 和 pid barrier，证明路径不靠 shell quoting。
fn fixture_dir(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("ja sidecar 探针 {name} {}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("fixture directory");
    path
}

/// 通过 Cargo 提供的 bin path 启动 native fixture，避免测试依赖 PATH 中的 shell。
fn helper_path() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_ja-sidecar-spike-helper").expect("helper binary"))
}

/// 构造最小配置并清空继承环境，测试侧只允许一个无敏感值变量。
fn config_for(mode: &str, dir: &Path) -> SidecarConfig {
    let mut config = SidecarConfig::new(helper_path(), dir);
    config.args = vec![
        "--fake-child".into(),
        "--mode".into(),
        mode.into(),
        "--pid-file".into(),
        dir.join("主进程.pid").into_os_string(),
    ];
    let workspace_root = dir.join("workspace-root");
    fs::create_dir_all(&workspace_root).expect("workspace root");
    config.workspace_root = Some(workspace_root);
    config.env = BTreeMap::from([("JA_FIXTURE_ALLOWED".into(), "yes".into())]);
    config.ready_timeout = Duration::from_secs(3);
    config.shutdown_timeout = Duration::from_millis(500);
    config
}

/// 读取文件是 fixture 已启动的 barrier，避免以任意 sleep 猜测进程状态。
fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .expect("pid barrier")
        .trim()
        .parse()
        .expect("numeric pid")
}

/// 等待 fixture 写入 PID barrier；文件存在是启动事实，短 park 仅避免忙等。
fn read_pid_when_ready(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "pid barrier timeout: {}",
            path.display()
        );
        std::thread::park_timeout(Duration::from_millis(5));
    }
    read_pid(path)
}

/// 等待 fixture 完成输出洪泛；文件是 child 的显式 barrier，不用任意延时猜测。
fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !path.is_file() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

/// 最终等待只防止平台回收延迟；触发条件由 Job/process-group 关闭事实决定。
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

/// 通过平台现有进程查询判断孙进程是否仍存在，不只观察主 child 的 wait。
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        output
            .ok()
            .map(|value| String::from_utf8_lossy(&value.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // SAFETY: signal 0 performs existence check without changing the process.
        return unsafe { kill(pid as i32, 0) == 0 };
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

#[test]
/// workspace cwd 必须在 spawn 前拒绝，避免 sidecar 获得隐式项目根权限。
fn config_rejects_workspace_cwd() {
    let dir = fixture_dir("config");
    let mut config = config_for("normal", &dir);
    config.workspace_root = Some(dir.clone());
    let result = SidecarSupervisor::new(config);
    assert!(matches!(result, Err(SidecarError::InvalidConfig(_))));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// workspace 子目录也必须拒绝，避免用子路径绕过 workspace containment。
fn config_rejects_workspace_child() {
    let dir = fixture_dir("config child");
    let workspace = dir.join("workspace");
    let child = workspace.join("sidecar-cwd");
    fs::create_dir_all(&child).expect("workspace child");
    let mut config = SidecarConfig::new(helper_path(), &child);
    config.workspace_root = Some(workspace);
    assert!(matches!(
        SidecarSupervisor::new(config),
        Err(SidecarError::InvalidConfig(_))
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// canonicalize 必须穿透 symlink/junction，不能用链接路径绕过 workspace containment。
fn config_rejects_workspace_symlink() {
    let dir = fixture_dir("config symlink");
    let workspace = dir.join("workspace-target");
    let linked_cwd = dir.join("sidecar-cwd-link");
    fs::create_dir_all(&workspace).expect("workspace target");

    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&workspace, &linked_cwd);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&workspace, &linked_cwd);
    if linked.is_err() {
        eprintln!("SKIP: symlink creation is unavailable on this runner");
        let _ = fs::remove_dir_all(dir);
        return;
    }

    let mut config = SidecarConfig::new(helper_path(), &linked_cwd);
    config.workspace_root = Some(workspace);
    assert!(matches!(
        SidecarSupervisor::new(config),
        Err(SidecarError::InvalidConfig(_))
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// 正常 JVM/native child 应完成 ready、stderr 读取和 graceful shutdown。
fn fake_child_ready_stderr_and_graceful_shutdown() {
    let dir = fixture_dir("graceful");
    let pid_path = dir.join("主进程.pid");
    let mut supervisor = SidecarSupervisor::new(config_for("normal", &dir)).expect("config");
    supervisor.start().expect("ready barrier");
    assert_eq!(supervisor.state(), LifecycleState::Ready);
    let pid = read_pid_when_ready(&pid_path);
    assert!(process_alive(pid));
    let outcome = supervisor
        .shutdown(Duration::from_secs(2))
        .expect("shutdown");
    assert!(matches!(
        outcome,
        ShutdownOutcome::Graceful { code: Some(0) }
    ));
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    assert!(wait_until_dead(pid, Duration::from_secs(2)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// 父进程正常退出时，Job Object 仍必须回收独立孙进程。
fn process_tree_is_cleaned_after_graceful_parent_exit() {
    let dir = fixture_dir("tree");
    let mut supervisor = SidecarSupervisor::new(config_for("tree", &dir)).expect("config");
    supervisor.start().expect("ready barrier");
    let grandchild_pid = read_pid_when_ready(&dir.join("主进程.grandchild.pid"));
    assert!(process_alive(grandchild_pid));
    let _ = supervisor
        .shutdown(Duration::from_secs(2))
        .expect("shutdown");
    assert!(wait_until_dead(grandchild_pid, Duration::from_secs(2)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// barrier 未释放时关闭必须超时并强制终止完整进程树。
fn shutdown_deadline_forces_complete_process_tree() {
    let dir = fixture_dir("forced shutdown");
    let barrier = dir.join("release-shutdown.barrier");
    let mut config = config_for("tree", &dir);
    config.args.extend([
        "--shutdown-barrier".into(),
        barrier.clone().into_os_string(),
    ]);
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("ready barrier");
    let grandchild_pid = read_pid_when_ready(&dir.join("主进程.grandchild.pid"));
    let outcome = supervisor
        .shutdown(Duration::from_millis(25))
        .expect("forced shutdown");
    assert!(matches!(outcome, ShutdownOutcome::Forced { .. }));
    assert!(wait_until_dead(grandchild_pid, Duration::from_secs(2)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// stdout 污染和无 LF 半帧都必须 fail-closed，不能交给协议层误解析。
fn pollution_and_half_frame_are_protocol_failures() {
    let pollution_dir = fixture_dir("pollution");
    let mut pollution =
        SidecarSupervisor::new(config_for("pollution", &pollution_dir)).expect("config");
    let error = pollution.start().expect_err("pollution must fail closed");
    assert!(matches!(
        error,
        SidecarError::Protocol(ProtocolViolation::InvalidJsonObject)
    ));
    assert_eq!(pollution.state(), LifecycleState::Exited);
    let _ = pollution.shutdown(Duration::from_secs(1));

    let half_dir = fixture_dir("half");
    let mut half = SidecarSupervisor::new(config_for("half", &half_dir)).expect("config");
    half.start().expect("ready before half frame");
    let mut saw_half = false;
    for _ in 0..5 {
        if let Some(SidecarEvent::ProtocolViolation(ProtocolViolation::UnexpectedEof)) =
            half.poll_event(Duration::from_secs(1))
        {
            saw_half = true;
            break;
        }
    }
    assert!(saw_half, "half frame must be surfaced");
    let _ = half.shutdown(Duration::from_secs(1));
    let _ = fs::remove_dir_all(pollution_dir);
    let _ = fs::remove_dir_all(half_dir);
}

#[test]
/// 合法 JSON 里的普通文本不能伪造 ready/incompatible 握手控制语义。
fn forged_handshake_strings_are_not_control_frames() {
    let forged_dir = fixture_dir("forged handshake");
    let mut forged_config = config_for("forged", &forged_dir);
    forged_config.ready_timeout = Duration::from_millis(100);
    let mut forged = SidecarSupervisor::new(forged_config).expect("config");
    assert!(matches!(forged.start(), Err(SidecarError::ReadyTimeout)));

    let malformed_dir = fixture_dir("malformed json");
    let mut malformed =
        SidecarSupervisor::new(config_for("malformed", &malformed_dir)).expect("config");
    assert!(matches!(
        malformed.start(),
        Err(SidecarError::Protocol(ProtocolViolation::InvalidJsonObject))
    ));
    let _ = forged.shutdown(Duration::from_millis(100));
    let _ = malformed.shutdown(Duration::from_millis(100));
    let _ = fs::remove_dir_all(forged_dir);
    let _ = fs::remove_dir_all(malformed_dir);
}

#[test]
/// crash 后只允许有限指数退避重启，验证第二次 crash 不会无限 loop。
fn crash_enters_backoff_and_restarts_with_limit() {
    let dir = fixture_dir("crash");
    let mut config = config_for("crash", &dir);
    config.restart = RestartPolicy {
        max_attempts: 2,
        base_delay: Duration::from_millis(25),
        max_delay: Duration::from_millis(50),
    };
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("crash fixture ready barrier");
    let deadline = Instant::now() + Duration::from_secs(2);
    let error = loop {
        let event = supervisor.poll_event(Duration::from_millis(100));
        if matches!(event, Some(SidecarEvent::ProcessExited { .. })) {
            break event.expect("crash exit event");
        }
        assert!(Instant::now() < deadline, "crash exit event timeout");
    };
    assert!(matches!(
        error,
        SidecarEvent::ProcessExited { code: Some(17), .. }
    ));
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    supervisor.note_crash_for_restart();
    assert_eq!(supervisor.state(), LifecycleState::Backoff);
    assert!(matches!(
        supervisor.restart(),
        Err(SidecarError::Backoff { .. })
    ));
    assert!(supervisor.wait_until_restartable(Duration::from_secs(1)));
    // The crash fixture is deterministic; a second attempt is bounded and cannot loop forever.
    supervisor.restart().expect("bounded restart");
    let second_deadline = Instant::now() + Duration::from_secs(2);
    let second_exit = loop {
        let event = supervisor.poll_event(Duration::from_millis(100));
        if matches!(event, Some(SidecarEvent::ProcessExited { .. })) {
            break event.expect("second crash exit event");
        }
        assert!(
            Instant::now() < second_deadline,
            "second crash exit event timeout"
        );
    };
    assert!(matches!(
        second_exit,
        SidecarEvent::ProcessExited { code: Some(17), .. }
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// crash 的 parent/孙进程必须在 ProcessExited 观察点收口，旧 generation 不能残留。
fn crash_with_grandchild_is_reaped_before_restart() {
    let dir = fixture_dir("crash tree");
    let parent_pid_path = dir.join("主进程.pid");
    let grandchild_pid_path = dir.join("主进程.grandchild.pid");
    let mut config = config_for("crash-tree", &dir);
    config.restart = RestartPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(25),
        max_delay: Duration::from_millis(50),
    };
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("crash tree ready barrier");
    let parent_pid = read_pid_when_ready(&parent_pid_path);
    let grandchild_pid = read_pid_when_ready(&grandchild_pid_path);

    let deadline = Instant::now() + Duration::from_secs(2);
    let first_exit = loop {
        let event = supervisor.poll_event(Duration::from_millis(100));
        if matches!(event, Some(SidecarEvent::ProcessExited { .. })) {
            break event.expect("crash tree exit event");
        }
        assert!(Instant::now() < deadline, "crash tree exit event timeout");
    };
    assert!(matches!(
        first_exit,
        SidecarEvent::ProcessExited { code: Some(17), .. }
    ));
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    assert!(
        supervisor
            .send_frame(r#"{"jsonrpc":"2.0","id":"after-crash","method":"ping","params":{}}"#)
            .is_err(),
        "crashed sidecar must reject later sends"
    );
    assert!(wait_until_dead(parent_pid, Duration::from_secs(2)));
    assert!(wait_until_dead(grandchild_pid, Duration::from_secs(2)));

    supervisor.note_crash_for_restart();
    assert_eq!(supervisor.state(), LifecycleState::Backoff);
    assert!(supervisor.wait_until_restartable(Duration::from_secs(1)));
    fs::remove_file(&parent_pid_path).expect("remove first parent pid barrier");
    fs::remove_file(&grandchild_pid_path).expect("remove first grandchild pid barrier");
    supervisor.restart().expect("bounded crash-tree restart");
    let second_parent_pid = read_pid_when_ready(&parent_pid_path);
    let second_grandchild_pid = read_pid_when_ready(&grandchild_pid_path);
    let second_deadline = Instant::now() + Duration::from_secs(2);
    let second_exit = loop {
        let event = supervisor.poll_event(Duration::from_millis(100));
        if matches!(event, Some(SidecarEvent::ProcessExited { .. })) {
            break event.expect("second crash tree exit event");
        }
        assert!(
            Instant::now() < second_deadline,
            "second crash tree exit event timeout"
        );
    };
    assert!(matches!(
        second_exit,
        SidecarEvent::ProcessExited { code: Some(17), .. }
    ));
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    assert!(wait_until_dead(second_parent_pid, Duration::from_secs(2)));
    assert!(wait_until_dead(
        second_grandchild_pid,
        Duration::from_secs(2)
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// major version 不兼容必须进入终态，不能被 restart policy 覆盖。
fn incompatible_version_never_enters_restart_loop() {
    let dir = fixture_dir("incompatible");
    let mut supervisor = SidecarSupervisor::new(config_for("incompatible", &dir)).expect("config");
    let error = supervisor.start().expect_err("incompatible fixture");
    assert!(matches!(error, SidecarError::Incompatible));
    assert_eq!(supervisor.state(), LifecycleState::Incompatible);
    assert!(matches!(
        supervisor.restart(),
        Err(SidecarError::Incompatible)
    ));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// 大量 delta 只能触发有界 overflow，reader 不得等待慢消费方。
fn bounded_output_queue_does_not_block_reader() {
    let dir = fixture_dir("queue");
    let mut config = config_for("spam", &dir);
    config.event_queue_capacity = 2;
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("ready before spam");
    let mut overflow = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let event = supervisor.poll_event(Duration::from_millis(100));
        if matches!(
            event,
            Some(SidecarEvent::QueueOverflow {
                stream: OutputStream::Stdout
            })
        ) {
            overflow = true;
            break;
        }
    }
    assert!(overflow, "reader must expose bounded queue overflow");
    let _ = supervisor.shutdown(Duration::from_secs(1));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// ready 后的大量 delta 不得挤掉随后到达的 ProcessExited 控制事实。
fn ready_spam_exit_preserves_exit_fact() {
    let dir = fixture_dir("spam exit");
    let mut config = config_for("spam-exit", &dir);
    config.event_queue_capacity = 2;
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("ready before spam exit");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_exit = false;
    while Instant::now() < deadline {
        if matches!(
            supervisor.poll_event(Duration::from_millis(100)),
            Some(SidecarEvent::ProcessExited { code: Some(17), .. })
        ) {
            saw_exit = true;
            break;
        }
    }
    assert!(saw_exit, "exit fact must survive delta overflow");
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    let _ = supervisor.shutdown(Duration::from_secs(1));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// stderr 必须持续 drain，诊断内容不能污染 stdout 协议流。
fn stderr_is_drained_without_stdout_protocol_pollution() {
    let dir = fixture_dir("stderr");
    let mut supervisor = SidecarSupervisor::new(config_for("stderr", &dir)).expect("config");
    supervisor.start().expect("ready before stderr drain");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_stderr = false;
    while Instant::now() < deadline {
        if matches!(
            supervisor.poll_event(Duration::from_millis(100)),
            Some(SidecarEvent::StderrLine(line)) if line.contains("fixture-stderr-")
        ) {
            saw_stderr = true;
            break;
        }
    }
    assert!(
        saw_stderr,
        "stderr reader must continuously drain child output"
    );
    let _ = supervisor.shutdown(Duration::from_secs(1));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// env_clear 只保留 allowlist 值，父进程 sentinel/PATH 均不能泄漏到 child。
fn child_environment_is_allowlisted() {
    let dir = fixture_dir("env allowlist");
    let report = dir.join("environment.report");
    let sentinel = format!("JA_FIXTURE_PARENT_SENTINEL_{}", std::process::id());
    let mut config = config_for("env-report", &dir);
    config.args.extend([
        "--env-report".into(),
        report.clone().into_os_string(),
        "--sentinel-name".into(),
        sentinel.clone().into(),
    ]);
    // SAFETY: this test owns a unique process-wide sentinel and restores it before returning.
    unsafe { std::env::set_var(&sentinel, "must-not-inherit") };
    let _reset = EnvReset(sentinel);
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("ready after env report");
    let report_text = fs::read_to_string(&report).expect("env report");
    assert!(report_text.contains("allowed=yes"));
    assert!(report_text.contains("sentinel=<missing>"));
    assert!(report_text.contains("PATH=<missing>"));
    let _ = supervisor.shutdown(Duration::from_secs(1));
}

#[test]
/// 控制事实使用独立队列；控制洪泛只能产生可观察 fatal，而不能静默覆盖退出/ready。
fn control_queue_overflow_is_fail_closed() {
    let dir = fixture_dir("control overflow");
    let flood_barrier = dir.join("control-flood.barrier");
    let flood_complete = dir.join("control-flood.complete");
    let _ = fs::remove_file(&flood_barrier);
    let _ = fs::remove_file(&flood_complete);
    let mut config = config_for("control-flood-hold-tree", &dir);
    config.event_queue_capacity = 4;
    config.args.extend([
        "--flood-barrier".into(),
        flood_barrier.clone().into_os_string(),
        "--flood-complete".into(),
        flood_complete.clone().into_os_string(),
    ]);
    let mut supervisor = SidecarSupervisor::new(config).expect("config");
    supervisor.start().expect("first ready barrier");
    let parent_pid = read_pid_when_ready(&dir.join("主进程.pid"));
    let grandchild_pid = read_pid_when_ready(&dir.join("主进程.grandchild.pid"));
    assert!(
        process_alive(parent_pid),
        "parent must still be running before fatal"
    );
    assert!(
        process_alive(grandchild_pid),
        "grandchild must still be running before fatal"
    );
    fs::write(&flood_barrier, "go").expect("release control flood");
    assert!(
        wait_for_file(&flood_complete, Duration::from_secs(2)),
        "fixture must finish control flood"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_fatal = false;
    while Instant::now() < deadline {
        if matches!(
            supervisor.poll_event(Duration::from_millis(100)),
            Some(SidecarEvent::QueueFatalOverflow { .. })
        ) {
            saw_fatal = true;
            break;
        }
    }
    assert!(
        saw_fatal,
        "control overflow must be observable and fail-closed"
    );
    assert_eq!(supervisor.state(), LifecycleState::Exited);
    assert!(
        supervisor
            .send_frame(r#"{"jsonrpc":"2.0","id":"after-fatal","method":"ping","params":{}}"#)
            .is_err(),
        "fatal overflow must make later sends fail"
    );
    assert!(wait_until_dead(parent_pid, Duration::from_secs(2)));
    assert!(wait_until_dead(grandchild_pid, Duration::from_secs(2)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
/// 使用真实 javac/java executable 完成 JVM sidecar 的 ready/shutdown 闭环。
fn real_java_fixture_round_trip_when_jdk_is_available() {
    let Some((java, javac)) = java_tools() else {
        eprintln!("SKIP: JAVA_HOME/javac not available");
        return;
    };
    let dir = fixture_dir("java fixture 中文");
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).expect("classes");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/java/JaFixture.java");
    let compile = Command::new(&javac)
        .args(["-encoding", "UTF-8", "-d"])
        .arg(&classes)
        .arg(&source)
        .output()
        .expect("javac");
    assert!(
        compile.status.success(),
        "javac failed: {:?}",
        compile.stderr
    );

    let mut config = SidecarConfig::new(java, &dir);
    config.args = vec![
        "-cp".into(),
        classes.into_os_string(),
        "JaFixture".into(),
        "--mode".into(),
        "normal".into(),
        "--pid-file".into(),
        dir.join("java.pid").into_os_string(),
    ];
    config.workspace_root = Some(dir.join("workspace"));
    fs::create_dir_all(dir.join("workspace")).expect("java workspace");
    config.ready_timeout = Duration::from_secs(10);
    let mut supervisor = SidecarSupervisor::new(config).expect("java config");
    supervisor.start().expect("java ready");
    assert_eq!(supervisor.state(), LifecycleState::Ready);
    let outcome = supervisor
        .shutdown(Duration::from_secs(2))
        .expect("java shutdown");
    assert!(matches!(
        outcome,
        ShutdownOutcome::Graceful { code: Some(0) }
    ));
    let _ = fs::remove_dir_all(dir);
}

/// 优先使用用户配置的 JDK，再从 PATH 做可移植发现，避免依赖开发机固定路径。
fn java_tools() -> Option<(PathBuf, PathBuf)> {
    let executable = if cfg!(windows) { "java.exe" } else { "java" };
    let compiler = if cfg!(windows) { "javac.exe" } else { "javac" };

    if let Some(java_home) = env::var_os("JAVA_HOME") {
        let home = PathBuf::from(java_home);
        if let Some(tools) = jdk_tools_from_home(&home, executable, compiler) {
            return Some(tools);
        }
        eprintln!("JAVA_HOME does not contain usable {executable} and {compiler}; checking PATH");
    }

    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        if let Some(tools) = jdk_tools_from_bin(&directory, executable, compiler) {
            return Some(tools);
        }
    }
    None
}

/// 从 JAVA_HOME 的 bin 目录解析两个绝对 executable，保证 SidecarConfig 不接收相对路径。
fn jdk_tools_from_home(
    home: &Path,
    executable: &str,
    compiler: &str,
) -> Option<(PathBuf, PathBuf)> {
    jdk_tools_from_bin(&home.join("bin"), executable, compiler)
}

/// 将 PATH 目录中的工具 canonicalize，避免相对 PATH 项绕过 executable 路径校验。
fn jdk_tools_from_bin(bin: &Path, executable: &str, compiler: &str) -> Option<(PathBuf, PathBuf)> {
    let java_candidate = bin.join(executable);
    let javac_candidate = bin.join(compiler);
    if !java_candidate.is_file() || !javac_candidate.is_file() {
        return None;
    }
    let java = fs::canonicalize(java_candidate).ok()?;
    let javac = fs::canonicalize(javac_candidate).ok()?;
    Some((java, javac))
}
