// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform process-tree and bounded-reap regression tests.

use super::*;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(unix, windows))]
struct TempFileGuard(std::path::PathBuf);

#[cfg(any(unix, windows))]
impl TempFileGuard {
    /// Own a fixture path so an assertion or process-spawn failure cannot leave
    /// a protocol artifact behind in the workspace or system temp directory.
    fn new(path: std::path::PathBuf) -> Self {
        Self(path)
    }
}

#[cfg(any(unix, windows))]
impl Drop for TempFileGuard {
    /// Remove only the exact fixture path; cleanup is idempotent after success.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct NeverReaps;

impl ProcessBackend for NeverReaps {
    /// 故意让 kill 失败，验证 helper 仍会在绝对 deadline 返回。
    fn kill(&self, _child: &mut Child) -> io::Result<()> {
        Err(io::Error::other("injected kill failure"))
    }

    /// 故意不报告退出，模拟 wait backend 卡住但不允许无界阻塞。
    fn try_wait(&self, _child: &mut Child) -> io::Result<Option<ExitStatus>> {
        Ok(None)
    }
}

/// 创建立即退出的跨平台 child，供 bounded reap fault hook 使用。
fn short_child() -> Child {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("short child spawned")
    }
    #[cfg(unix)]
    {
        Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("short child spawned")
    }
    #[cfg(not(any(unix, windows)))]
    {
        panic!("no process fixture for this platform")
    }
}

/// 注入 kill/wait 故障时必须在 deadline 内返回，并把 Child 所有权留给 caller。
#[test]
fn bounded_reap_returns_on_backend_fault() {
    let mut child = short_child();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(40))
        .expect("reap deadline fits");
    let error = bounded_reap_child_with(&mut child, deadline, &NeverReaps)
        .expect_err("faulting backend must be observable");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    let _ = bounded_reap_child(&mut child, Instant::now() + Duration::from_secs(1));
}

#[cfg(unix)]
/// 证明 Unix process group 收口会覆盖 shell 启动的 descendant，而不只结束 leader。
#[test]
fn process_group_termination_reaches_descendant() {
    let pid_path = std::env::temp_dir().join(format!(
        "ja-process-tree-{}-{}.pid",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos()
    ));
    let _pid_cleanup = TempFileGuard::new(pid_path.clone());
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "sleep 30 & printf '%s' \"$!\" > \"$1\"; wait",
        "ja-tree-fixture",
        pid_path.to_str().expect("temporary path is utf8"),
    ]);
    let guard = ProcessTreeGuard::prepare(&mut command).unwrap();
    let mut child = command.spawn().unwrap();
    guard.assign(&child).unwrap();
    guard.resume(&child).unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("test deadline fits in Instant");
    let descendant = loop {
        if let Ok(pid) = fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "descendant pid was not published"
        );
        thread::yield_now();
    };
    guard.terminate().unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("child reap deadline fits in Instant");
    bounded_reap_child(&mut child, deadline).expect("fixture child reaped");
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .expect("test deadline fits in Instant");
    while unsafe { kill(descendant, 0) } == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    let _ = fs::remove_file(pid_path);
    assert_ne!(unsafe { kill(descendant, 0) }, 0);
}

#[cfg(windows)]
mod job_backend_tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Copy, Default)]
    struct FaultingJob {
        job: *mut std::ffi::c_void,
        fail_assign: bool,
        fail_resume: bool,
        fail_terminate: bool,
        fail_close: bool,
        fail_terminate_process: bool,
    }

    impl FaultingJob {
        /// Build one production-backend fault case so every test enters through
        /// the ProcessTreeGuard assign/resume/terminate wrappers.
        fn for_guard(guard: &ProcessTreeGuard, operation: JobOperation) -> Self {
            let mut fault = Self {
                job: guard.job.raw(),
                ..Self::default()
            };
            match operation {
                JobOperation::Assign => fault.fail_assign = true,
                JobOperation::Resume => fault.fail_resume = true,
                JobOperation::Terminate => fault.fail_terminate = true,
                JobOperation::Close => fault.fail_close = true,
                JobOperation::TerminateProcess => fault.fail_terminate_process = true,
            }
            fault
        }

        /// Build a backend that delegates successful calls to the real Win32 APIs.
        fn success_for_guard(guard: &ProcessTreeGuard) -> Self {
            Self {
                job: guard.job.raw(),
                ..Self::default()
            }
        }

        /// Terminate and direct-process faults are combined to force the final
        /// exact-handle fallback while Job close still owns descendants.
        fn terminate_and_process_fault() -> Self {
            Self {
                fail_terminate: true,
                fail_terminate_process: true,
                ..Self::default()
            }
        }

        /// Bind a combined termination fault to the exact Job handle under test.
        fn for_guard_terminate_and_process(guard: &ProcessTreeGuard) -> Self {
            Self {
                job: guard.job.raw(),
                ..Self::terminate_and_process_fault()
            }
        }
    }

    unsafe impl Send for FaultingJob {}
    unsafe impl Sync for FaultingJob {}

    impl JobBackend for FaultingJob {
        /// Inject assign failure while retaining the duplicate exact child handle.
        fn assign(
            &self,
            _job: *mut std::ffi::c_void,
            _process: *mut std::ffi::c_void,
        ) -> io::Result<()> {
            if self.fail_assign {
                Err(io::Error::other("injected assign failure"))
            } else {
                let ok = unsafe { AssignProcessToJobObject(self.job, _process) };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }

        /// Inject resume failure after the production assign wrapper succeeds.
        fn resume(&self, _process_id: u32) -> io::Result<()> {
            if self.fail_resume {
                Err(io::Error::other("injected resume failure"))
            } else {
                resume_suspended_process(_process_id)
            }
        }

        /// Inject Job termination failure while preserving the close fallback.
        fn terminate(&self) -> io::Result<()> {
            if self.fail_terminate {
                Err(io::Error::other("injected job failure"))
            } else {
                let ok = unsafe { TerminateJobObject(self.job, 1) };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }

        /// Inject Job close failure so raw ownership remains observable.
        fn close_job(&self, _job: *mut std::ffi::c_void) -> io::Result<()> {
            if self.fail_close {
                Err(io::Error::other("injected close failure"))
            } else {
                let ok = unsafe { CloseHandle(_job) };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }

        /// Inject exact leader termination failure after group fallback attempts.
        fn terminate_process(&self, _process: *mut std::ffi::c_void) -> io::Result<()> {
            if self.fail_terminate_process {
                Err(io::Error::other("injected process failure"))
            } else {
                let ok = unsafe { TerminateProcess(_process, 1) };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Assert the structured operation record without exposing raw OS messages.
    fn assert_recorded(guard: &ProcessTreeGuard, operation: JobOperation) {
        assert!(
            guard
                .recorded_errors()
                .iter()
                .any(|entry| entry.operation == operation),
            "missing structured record for {operation:?}"
        );
    }

    /// Assign and resume injection must exercise the production adapter while
    /// retaining a real suspended child for the bounded cleanup path.
    #[test]
    fn real_child_assign_and_resume_faults_keep_ownership() {
        let mut command = Command::new("cmd");
        command.args(["/C", "ping 127.0.0.1 -n 30 >NUL"]);
        let mut guard = ProcessTreeGuard::prepare(&mut command).expect("job prepared");
        let mut child = command.spawn().expect("real child spawned");
        guard.set_backend_for_test(Arc::new(FaultingJob::for_guard(
            &guard,
            JobOperation::Assign,
        )));
        assert!(guard.assign(&child).is_err());
        assert_recorded(&guard, JobOperation::Assign);
        guard.set_backend_for_test(Arc::new(FaultingJob::for_guard(
            &guard,
            JobOperation::Resume,
        )));
        assert!(guard.resume(&child).is_err());
        assert_recorded(&guard, JobOperation::Resume);
        guard.set_backend_for_test(Arc::new(FaultingJob::success_for_guard(&guard)));
        assert!(guard.terminate().is_ok());
        bounded_reap_child(
            &mut child,
            Instant::now()
                .checked_add(Duration::from_secs(2))
                .expect("cleanup deadline fits"),
        )
        .expect("retained child must be reaped");
    }

    /// A real suspended child must be reaped before the guard drops after
    /// Job/Close/TerminateProcess faults; Drop is not the cleanup mechanism.
    #[test]
    fn real_child_fault_keeps_exact_handle_for_bounded_cleanup() {
        let mut command = Command::new("cmd");
        command.args(["/C", "ping 127.0.0.1 -n 30 >NUL"]);
        let mut guard = ProcessTreeGuard::prepare(&mut command).expect("job prepared");
        let mut child = command.spawn().expect("real child spawned");
        guard.assign(&child).expect("child assigned");
        guard.resume(&child).expect("child resumed");
        guard.set_backend_for_test(Arc::new(FaultingJob::for_guard_terminate_and_process(
            &guard,
        )));
        let error = guard
            .terminate()
            .expect_err("injected Job failure must be observable");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_recorded(&guard, JobOperation::Terminate);
        assert_recorded(&guard, JobOperation::TerminateProcess);
        bounded_reap_child(
            &mut child,
            Instant::now()
                .checked_add(Duration::from_secs(2))
                .expect("cleanup deadline fits"),
        )
        .expect("exact child handle must be reaped");
        drop(guard);
    }

    /// Spawn a real suspended PowerShell leader that creates a tracked descendant.
    fn spawn_real_descendant_fixture() -> Option<(ProcessTreeGuard, Child, u32, TempFileGuard)> {
        let pid_path = std::env::temp_dir().join(format!(
            "ja-process-tree-fault-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after epoch")
                .as_nanos()
        ));
        let pid_cleanup = TempFileGuard::new(pid_path.clone());
        let powershell =
            std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe");
        if !powershell.is_file() {
            return None;
        }
        let pid_literal = pid_path.to_string_lossy().replace('\'', "''");
        let script = format!(
            r#"$child = Start-Process -FilePath ($env:SystemRoot + '\System32\WindowsPowerShell\v1.0\powershell.exe') -ArgumentList @('-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30') -WindowStyle Hidden -PassThru; [IO.File]::WriteAllText('{pid_literal}', [string]$child.Id); Wait-Process -Id ($child.Id)"#
        );
        let mut command = Command::new(&powershell);
        command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        let guard = ProcessTreeGuard::prepare(&mut command).expect("job prepared");
        let child = command.spawn().expect("real leader spawned");
        guard.assign(&child).expect("leader assigned");
        guard.resume(&child).expect("leader resumed");
        let deadline = Instant::now() + Duration::from_secs(3);
        let descendant = loop {
            if let Ok(value) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = value.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "descendant pid was not published"
            );
            thread::yield_now();
        };
        Some((guard, child, descendant, pid_cleanup))
    }

    /// Job/Close/TerminateProcess faults on a real leader must still close the
    /// retained Job and reap its descendant before the absolute deadline.
    #[test]
    fn production_terminate_fault_reaps_real_descendant() {
        let Some((mut guard, mut child, descendant, _pid_cleanup)) =
            spawn_real_descendant_fixture()
        else {
            return;
        };
        guard.set_backend_for_test(Arc::new(FaultingJob::for_guard_terminate_and_process(
            &guard,
        )));
        assert!(guard.terminate().is_err());
        assert_recorded(&guard, JobOperation::Terminate);
        assert_recorded(&guard, JobOperation::TerminateProcess);
        bounded_reap_child(
            &mut child,
            Instant::now()
                .checked_add(Duration::from_secs(2))
                .expect("leader cleanup deadline fits"),
        )
        .expect("leader must be reaped");
        let deadline = Instant::now() + Duration::from_secs(2);
        while windows_process_exists(descendant) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(!windows_process_exists(descendant));
        drop(guard);
    }

    /// A real Job close fault must be recorded through terminate and descendants
    /// must exit before the guard is dropped.
    #[test]
    fn production_close_fault_reaps_real_descendant() {
        let Some((mut guard, mut child, descendant, _pid_cleanup)) =
            spawn_real_descendant_fixture()
        else {
            return;
        };
        guard.set_backend_for_test(Arc::new(FaultingJob::for_guard(
            &guard,
            JobOperation::Close,
        )));
        assert!(guard.terminate().is_err());
        assert_recorded(&guard, JobOperation::Close);
        bounded_reap_child(
            &mut child,
            Instant::now()
                .checked_add(Duration::from_secs(2))
                .expect("leader cleanup deadline fits"),
        )
        .expect("leader must be reaped before guard drop");
        let deadline = Instant::now() + Duration::from_secs(2);
        while windows_process_exists(descendant) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(!windows_process_exists(descendant));
        drop(guard);
    }

    /// Query tasklist without reopening a process handle, only for test
    /// observation of the descendant's eventual disappearance.
    fn windows_process_exists(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        Command::new("tasklist")
            .args(["/FI", &filter, "/NH"])
            .output()
            .ok()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split_whitespace()
                        .nth(1)
                        .is_some_and(|value| value == pid.to_string())
                })
            })
            .unwrap_or(false)
    }
}
