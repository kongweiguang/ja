// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! PTY child 的完整进程树收口。
//!
//! `portable-pty` 提供 child handle，但其跨平台 `kill` 不能保证 shell 后代一起
//! 退出；这里仅使用现有 Unix process-group/Windows Job Object 原语，不重新实现 PTY。

use super::error::{TerminalError, TerminalErrorCode};
use portable_pty::Child;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// 跨平台 process tree 控制边界。
pub(crate) trait ProcessTree: Send + Sync {
    /// 幂等终止所有仍属于本 session 的 child descendants。
    fn terminate(&self) -> io::Result<()>;
}

/// 创建并绑定当前 PTY child 的进程树 guard；失败时 caller 必须 kill child 并放弃 spawn。
pub(crate) fn attach(child: &dyn Child) -> Result<Box<dyn ProcessTree>, TerminalError> {
    #[cfg(unix)]
    {
        let pid = child
            .process_id()
            .ok_or(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed))?;
        return Ok(Box::new(UnixProcessTree::new(pid)));
    }
    #[cfg(windows)]
    {
        WindowsProcessTree::attach(child)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child;
        Err(TerminalError::new(TerminalErrorCode::UnsupportedPlatform))
    }
}

#[cfg(unix)]
struct UnixProcessTree {
    pid: i32,
    alive: AtomicBool,
}

#[cfg(unix)]
impl UnixProcessTree {
    /// portable-pty 的 Unix backend 使用 setsid，因此 child pid 也是独立 process group leader。
    fn new(pid: u32) -> Self {
        Self {
            pid: pid as i32,
            alive: AtomicBool::new(true),
        }
    }
}

#[cfg(unix)]
impl ProcessTree for UnixProcessTree {
    /// TERM 后立即 KILL，避免 close deadline 被不合作 shell 或 descendant 消耗。
    fn terminate(&self) -> io::Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut first_error = None;
        for signal in [SIGTERM, SIGKILL] {
            // Negative pid targets the process group created by portable-pty's setsid.
            if unsafe { kill(-self.pid, signal) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    first_error.get_or_insert(error);
                }
            }
        }
        if first_error.is_none() {
            self.alive.store(false, Ordering::Release);
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(unix)]
impl Drop for UnixProcessTree {
    /// 任何 early return 都尝试收口，防止 UI 关闭后留下孤儿 shell。
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(windows)]
struct WindowsProcessTree {
    job: AtomicPtr<std::ffi::c_void>,
    alive: AtomicBool,
}

#[cfg(windows)]
unsafe impl Send for WindowsProcessTree {}
#[cfg(windows)]
unsafe impl Sync for WindowsProcessTree {}

#[cfg(windows)]
impl WindowsProcessTree {
    /// 在 child 创建后立即绑定 Job Object；KILL_ON_JOB_CLOSE 覆盖已产生的后代。
    fn attach(child: &dyn Child) -> Result<Box<dyn ProcessTree>, TerminalError> {
        use std::os::windows::io::RawHandle;
        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job.is_null() {
            return Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed));
        }
        let mut limits = ExtendedLimitInformation::default();
        limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimitInformation).cast(),
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        } != 0;
        if !configured {
            unsafe { CloseHandle(job) };
            return Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed));
        }
        let raw: RawHandle = match child.as_raw_handle() {
            Some(raw) => raw,
            None => {
                unsafe {
                    CloseHandle(job);
                }
                return Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed));
            }
        };
        if unsafe { AssignProcessToJobObject(job, raw) } == 0 {
            unsafe {
                CloseHandle(job);
            }
            return Err(TerminalError::new(TerminalErrorCode::ProcessCleanupFailed));
        }
        Ok(Box::new(Self {
            job: AtomicPtr::new(job),
            alive: AtomicBool::new(true),
        }))
    }

    /// Claim the Job handle before entering Win32 so exactly one closer can
    /// call TerminateJobObject and CloseHandle for this process tree.
    fn claim_job(&self) -> Option<*mut std::ffi::c_void> {
        if !self.alive.load(Ordering::Acquire) {
            return None;
        }
        let job = claim_job_slot(&self.job);
        if job.is_none() {
            self.alive.store(false, Ordering::Release);
        }
        job
    }
}

#[cfg(windows)]
/// Atomically transfer the raw Job handle to one cleanup caller.
fn claim_job_slot(slot: &AtomicPtr<std::ffi::c_void>) -> Option<*mut std::ffi::c_void> {
    let job = slot.load(Ordering::Acquire);
    if job.is_null() {
        return None;
    }
    slot.compare_exchange(
        job,
        std::ptr::null_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    )
    .ok()
}

#[cfg(windows)]
impl ProcessTree for WindowsProcessTree {
    /// TerminateJobObject 是唯一能覆盖 ConPTY shell 后代的稳定 host 边界。
    fn terminate(&self) -> io::Result<()> {
        let Some(job) = self.claim_job() else {
            return Ok(());
        };
        let terminate_result = if unsafe { TerminateJobObject(job, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        let close_result = unsafe { CloseHandle(job) } != 0;
        self.alive.store(false, Ordering::Release);
        if terminate_result.is_ok() || close_result {
            // Closing the configured Job Object is a safe finalization path
            // when the child has already exited and TerminateJobObject races
            // that transition; the job flag still kills any descendants.
            Ok(())
        } else {
            terminate_result
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessTree {
    /// Job handle 关闭即触发 KILL_ON_JOB_CLOSE，故 Drop 也必须走同一幂等路径。
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time: i64,
    per_job_user_time: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operations: u64,
    write_operations: u64,
    other_operations: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic: BasicLimitInformation,
    io: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn AssignProcessToJobObject(job: *mut std::ffi::c_void, process: *mut std::ffi::c_void) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn CreateJobObjectW(
        attributes: *mut std::ffi::c_void,
        name: *const u16,
    ) -> *mut std::ffi::c_void;
    fn SetInformationJobObject(
        job: *mut std::ffi::c_void,
        info_class: u32,
        info: *mut std::ffi::c_void,
        length: u32,
    ) -> i32;
    fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// 并发 close 只能有一个 caller 获得 Job handle，避免 CAS 前重复触碰句柄。
    #[test]
    fn concurrent_job_claim_has_one_owner() {
        let slot = Arc::new(AtomicPtr::new(std::ptr::dangling_mut::<std::ffi::c_void>()));
        let barrier = Arc::new(Barrier::new(8));
        let mut callers = Vec::new();
        for _ in 0..8 {
            let slot = Arc::clone(&slot);
            let barrier = Arc::clone(&barrier);
            callers.push(thread::spawn(move || {
                barrier.wait();
                claim_job_slot(&slot).is_some()
            }));
        }
        let owners = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .filter(|claimed| *claimed)
            .count();
        assert_eq!(owners, 1);
        assert!(slot.load(Ordering::Acquire).is_null());
    }
}
