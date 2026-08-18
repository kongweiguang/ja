// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 终端 PTY worker 与关闭期间的资源回收。
//!
//! 将阻塞读写、resize、child wait 和 JoinHandle accounting 放到独立模块，
//! 是为了让 session facade 只负责生命周期状态机，而不是让一个文件同时
//! 承担公开 API、事件发布和平台 worker 的所有职责。

use super::{TerminalError, TerminalErrorCode, TerminalEventKind, TerminalRuntime, TerminalSize};
use portable_pty::{Child, PtySize};
use std::io::{Read, Write};
#[cfg(test)]
use std::sync::Barrier;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PTY_DRAIN_GRACE: Duration = Duration::from_millis(100);
const WORKER_FINISH_POLL: Duration = Duration::from_millis(1);
/// Keep the reservation and spawn-failure compensation counts in one place.
pub(super) const WORKER_COUNT: usize = 4;

/// portable-pty 的 resize DTO 转换集中在一个平台无关函数，避免 command 层重复映射。
pub(super) fn to_pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

/// 启动四个 worker；每个 worker 绑定自己的资源，失败时统一触发 bounded cleanup。
pub(super) fn spawn_workers(
    runtime: &Arc<TerminalRuntime>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
) -> Result<(), TerminalError> {
    let reader_runtime = runtime.clone();
    let writer_runtime = runtime.clone();
    let resize_runtime = runtime.clone();
    let child_runtime = runtime.clone();
    let reader_result = thread::Builder::new()
        .name("ja-terminal-reader".to_owned())
        .spawn(move || reader_worker(reader_runtime, reader));
    match reader_result {
        Ok(handle) => runtime.workers.register(handle),
        Err(_) => {
            // Four slots were reserved before spawning; complete the failed
            // slot and every later slot so close never waits for nonexistent
            // workers.
            runtime.workers.complete_slots(WORKER_COUNT);
            return Err(TerminalError::new(TerminalErrorCode::SpawnFailed));
        }
    }
    let writer_result = thread::Builder::new()
        .name("ja-terminal-writer".to_owned())
        .spawn(move || writer_worker(writer_runtime, writer));
    match writer_result {
        Ok(handle) => runtime.workers.register(handle),
        Err(_) => {
            runtime.workers.complete_slots(WORKER_COUNT - 1);
            return Err(TerminalError::new(TerminalErrorCode::SpawnFailed));
        }
    }
    let resize_result = thread::Builder::new()
        .name("ja-terminal-resize".to_owned())
        .spawn(move || resize_worker(resize_runtime));
    match resize_result {
        Ok(handle) => runtime.workers.register(handle),
        Err(_) => {
            runtime.workers.complete_slots(WORKER_COUNT - 2);
            return Err(TerminalError::new(TerminalErrorCode::SpawnFailed));
        }
    }
    let child_result = thread::Builder::new()
        .name("ja-terminal-child".to_owned())
        .spawn(move || child_worker(child_runtime, child));
    match child_result {
        Ok(handle) => runtime.workers.register(handle),
        Err(_) => {
            runtime.workers.complete_slots(WORKER_COUNT - 3);
            return Err(TerminalError::new(TerminalErrorCode::SpawnFailed));
        }
    }
    Ok(())
}

/// reader 以固定 buffer 读 raw bytes，跨 chunk 的 UTF-8/ANSI 由前端重组。
fn reader_worker(runtime: Arc<TerminalRuntime>, mut reader: Box<dyn Read + Send>) {
    let _guard = WorkerGuard::new(runtime.workers.clone());
    let mut buffer = vec![0_u8; runtime.limits.max_output_batch_bytes.min(64 * 1024)];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                if runtime.reader_can_finish() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Ok(count) => {
                if !runtime.publish_output(buffer[..count].to_vec()) {
                    break;
                }
            }
            Err(error) => {
                if runtime.stop.load(Ordering::Acquire) {
                    break;
                }
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                if !runtime.reader_can_finish() {
                    tracing::debug!(error_kind = ?error.kind(), "terminal PTY reader failed");
                    runtime.fail(TerminalErrorCode::PtyFailed);
                }
                break;
            }
        }
    }
    runtime.reader_finished();
}

/// writer 是唯一向 PTY 写入的 worker，保证 input chunks 不会交叉。
fn writer_worker(runtime: Arc<TerminalRuntime>, mut writer: Box<dyn Write + Send>) {
    let _guard = WorkerGuard::new(runtime.workers.clone());
    while let Some(data) = runtime.input.pop() {
        if runtime.stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = writer.write_all(&data).and_then(|_| writer.flush()) {
            if !runtime.stop.load(Ordering::Acquire) {
                tracing::debug!(error_kind = ?error.kind(), "terminal PTY writer failed");
                runtime.fail(TerminalErrorCode::PtyFailed);
            }
            break;
        }
    }
}

/// resize worker 只消费 coalesced latest value，避免 terminal resize storm。
fn resize_worker(runtime: Arc<TerminalRuntime>) {
    let _guard = WorkerGuard::new(runtime.workers.clone());
    while let Some(size) = runtime.resize_queue.pop() {
        if runtime.stop.load(Ordering::Acquire) {
            break;
        }
        let result = runtime
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|master| master.resize(to_pty_size(size)));
        match result {
            Some(Ok(())) => runtime.publish_control(TerminalEventKind::Resized { size }),
            Some(Err(error)) => {
                tracing::debug!(error = %error, "terminal resize failed");
                runtime.fail(TerminalErrorCode::PtyFailed);
                break;
            }
            None => break,
        }
    }
}

/// child worker 以 bounded poll 观察退出，正常/异常退出都经过同一终态路径。
fn child_worker(runtime: Arc<TerminalRuntime>, mut child: Box<dyn Child + Send + Sync>) {
    let _guard = WorkerGuard::new(runtime.workers.clone());
    let deadline = Instant::now()
        .checked_add(runtime.limits.operation_timeout)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                runtime.child_exited(status);
                // The child status is observable before the PTY reader sees
                // EOF. A short bounded grace preserves final output while the
                // explicit master close guarantees reader termination on
                // ConPTY, whose pipe may otherwise stay readable forever.
                thread::sleep(PTY_DRAIN_GRACE);
                runtime.drop_master();
                break;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(error_kind = ?error.kind(), "terminal child wait failed");
                runtime.fail(TerminalErrorCode::PtyFailed);
                break;
            }
        }
        if runtime.stop.load(Ordering::Acquire) && Instant::now() >= deadline {
            runtime.fail(TerminalErrorCode::WorkerShutdownTimeout);
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// 单槽 resize queue；设置新尺寸会覆盖尚未应用的旧尺寸。
pub(super) struct ResizeQueue {
    state: Mutex<ResizeState>,
    wake: Condvar,
}

struct ResizeState {
    pending: Option<TerminalSize>,
    closed: bool,
}

impl ResizeQueue {
    /// 创建 coalescing queue，而不是为每次窗口像素变化分配消息。
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(ResizeState {
                pending: None,
                closed: false,
            }),
            wake: Condvar::new(),
        }
    }

    /// 覆盖 pending size，后端只需应用最后一个有效尺寸。
    pub(super) fn set(&self, size: TerminalSize) -> Result<(), TerminalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(TerminalError::new(TerminalErrorCode::QueueClosed));
        }
        state.pending = Some(size);
        self.wake.notify_one();
        Ok(())
    }

    /// resize worker 阻塞取最新尺寸，close 后最终返回 None。
    pub(super) fn pop(&self) -> Option<TerminalSize> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(size) = state.pending.take() {
                return Some(size);
            }
            if state.closed {
                return None;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// 关闭并丢弃旧尺寸，防止 shutdown 之后 worker 重新触碰 master。
    pub(super) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.pending = None;
        self.wake.notify_all();
    }
}

/// worker 数量由 runtime 所有，便于 close 使用 deadline 等待而不 join 自己。
pub(super) struct WorkerTracker {
    wake: Condvar,
    state: Mutex<WorkerState>,
    handles: Mutex<Vec<JoinHandle<()>>>,
    #[cfg(test)]
    joined: AtomicUsize,
}

struct WorkerState {
    remaining: usize,
    #[cfg(test)]
    wait_before_block: Option<Arc<WaitGate>>,
}

#[cfg(test)]
struct WaitGate {
    entered: Barrier,
    release: Barrier,
}

pub(super) enum WorkerReap {
    Complete,
    Timeout,
    JoinFailed,
}

impl WorkerTracker {
    /// 预登记四类 worker，spawn 失败路径也可准确扣减。
    pub(super) fn new(count: usize) -> Self {
        Self {
            wake: Condvar::new(),
            state: Mutex::new(WorkerState {
                remaining: count,
                #[cfg(test)]
                wait_before_block: None,
            }),
            handles: Mutex::new(Vec::with_capacity(count)),
            #[cfg(test)]
            joined: AtomicUsize::new(0),
        }
    }

    /// Keep join handles with the runtime so timeout/retry can reap the same
    /// workers instead of detaching a live PTY owner.
    pub(super) fn register(&self, handle: JoinHandle<()>) {
        self.handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handle);
    }

    /// worker 完成时通知 close waiter。
    pub(super) fn done(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.remaining != 0 {
            state.remaining -= 1;
            // Updating the predicate and notifying while holding the same
            // mutex makes check-then-wait atomic from the waiter's perspective.
            self.wake.notify_all();
        }
    }

    /// 完成尚未启动的 worker 槽位，避免 thread spawn 失败留下虚假计数。
    pub(super) fn complete_slots(&self, count: usize) {
        for _ in 0..count {
            self.done();
        }
    }

    /// 在绝对 deadline 内等待所有 worker 退出。
    pub(super) fn wait_until(&self, deadline: Instant) -> WorkerReap {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while guard.remaining != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return WorkerReap::Timeout;
            }
            #[cfg(test)]
            if let Some(hook) = guard.wait_before_block.take() {
                // The hook exists only to make the check-then-wait race
                // deterministic in unit tests; production has no extra API.
                hook.entered.wait();
                hook.release.wait();
            }
            let (next, result) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
            if result.timed_out() {
                // Re-check the predicate after the deadline wakeup: a worker
                // may have completed concurrently with the timeout signal.
                continue;
            }
        }

        // `done` is emitted just before a worker closure returns, so the
        // handle may need a short bounded poll before `join`; never block past
        // the caller's absolute deadline waiting for that state transition.
        drop(guard);
        loop {
            let all_finished = {
                let handles = self
                    .handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                handles.iter().all(JoinHandle::is_finished)
            };
            if all_finished {
                break;
            }
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return WorkerReap::Timeout;
            }
            let poll = remaining.min(WORKER_FINISH_POLL);
            let (next, _) = self
                .wake
                .wait_timeout(state, poll)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            drop(state);
        }

        let handles = std::mem::take(
            &mut *self
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut join_failed = false;
        for handle in handles {
            if handle.join().is_err() {
                join_failed = true;
            }
            #[cfg(test)]
            self.joined.fetch_add(1, Ordering::Relaxed);
        }
        if join_failed {
            WorkerReap::JoinFailed
        } else {
            WorkerReap::Complete
        }
    }

    /// A closed generation is reclaimable only after all workers were joined.
    pub(super) fn is_reaped(&self) -> bool {
        let remaining = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remaining;
        remaining == 0
            && self
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }
}

/// RAII worker guard，确保 panic/early return 也会释放 tracker 计数。
struct WorkerGuard {
    tracker: Arc<WorkerTracker>,
}

impl WorkerGuard {
    /// 绑定当前 worker 的 completion accounting。
    fn new(tracker: Arc<WorkerTracker>) -> Self {
        Self { tracker }
    }
}

impl Drop for WorkerGuard {
    /// worker 退出时唤醒 bounded shutdown waiter。
    fn drop(&mut self) {
        self.tracker.done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟首个 worker 已启动、后续三个 worker 创建失败，关闭不得超时。
    #[test]
    fn unstarted_worker_slots_are_released() {
        let tracker = Arc::new(WorkerTracker::new(WORKER_COUNT));
        let worker_tracker = Arc::clone(&tracker);
        let started = Arc::new(Barrier::new(2));
        let worker_started = Arc::clone(&started);
        tracker.register(thread::spawn(move || {
            worker_started.wait();
            worker_tracker.done();
        }));
        started.wait();
        tracker.complete_slots(WORKER_COUNT - 1);
        assert!(matches!(
            tracker.wait_until(Instant::now() + Duration::from_secs(1)),
            WorkerReap::Complete
        ));
        assert!(tracker.is_reaped());
    }

    /// 同一 accounting mutex 必须覆盖 predicate 检查与进入 condvar，避免
    /// done 在 waiter 释放 mutex 前发通知而造成永久丢唤醒。
    #[test]
    fn wait_until_does_not_lose_done_notification() {
        let tracker = Arc::new(WorkerTracker::new(1));
        let wait_hook = Arc::new(WaitGate {
            entered: Barrier::new(2),
            release: Barrier::new(2),
        });
        tracker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wait_before_block = Some(Arc::clone(&wait_hook));

        let waiter_tracker = Arc::clone(&tracker);
        let waiter = thread::spawn(move || {
            waiter_tracker.wait_until(Instant::now() + Duration::from_secs(1))
        });
        wait_hook.entered.wait();

        let done_tracker = Arc::clone(&tracker);
        let done_started = Arc::new(Barrier::new(2));
        let done_start = Arc::clone(&done_started);
        let done = thread::spawn(move || {
            done_start.wait();
            done_tracker.done();
        });
        done_started.wait();
        wait_hook.release.wait();

        done.join().unwrap();
        assert!(matches!(waiter.join().unwrap(), WorkerReap::Complete));
    }

    /// join 不能因第一个 panic 短路，否则后续 worker 的句柄会被丢弃且
    /// session 可能错误地把未回收的线程标成可复用。
    #[test]
    fn wait_until_joins_all_handles_after_first_panic() {
        let tracker = Arc::new(WorkerTracker::new(2));
        let first_tracker = Arc::clone(&tracker);
        tracker.register(thread::spawn(move || {
            let _guard = WorkerGuard::new(first_tracker);
            panic!("intentional worker panic");
        }));
        let second_tracker = Arc::clone(&tracker);
        tracker.register(thread::spawn(move || {
            let _guard = WorkerGuard::new(second_tracker);
        }));

        assert!(matches!(
            tracker.wait_until(Instant::now() + Duration::from_secs(1)),
            WorkerReap::JoinFailed
        ));
        assert_eq!(tracker.joined.load(Ordering::Acquire), 2);
        assert!(tracker.is_reaped());
    }
}
